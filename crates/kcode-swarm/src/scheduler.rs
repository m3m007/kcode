use anyhow::Result;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};

/// A workspace-member crate resolved by the orchestrator at startup.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CrateNode {
    pub name: String,
    pub manifest_dir: PathBuf,
}

/// Load workspace members + intra-workspace dependency edges via krates.
/// (Kept as a free function so tests can inject graphs without cargo.)
/// krates edge direction (verified in builder.rs): dependent -> dependency,
/// so `get_deps(id)` yields exactly the packages `id` depends on.
pub fn load_workspace_graph(
    manifest_path: &std::path::Path,
) -> Result<(Vec<CrateNode>, HashMap<String, HashSet<String>>)> {
    let mut cmd = krates::Cmd::new();
    cmd.manifest_path(manifest_path);
    let krates: krates::Krates = krates::Builder::new().build(cmd, |_filtered: krates::cm::Package| {})?;

    // Kid repr -> workspace member name.
    let mut member_kids: HashMap<String, String> = HashMap::new();
    for node in krates.workspace_members() {
        if let krates::Node::Krate { id, krate, .. } = node {
            member_kids.insert(id.repr.clone(), krate.name.clone());
        }
    }

    // NodeId -> member name (+ manifest dir) for Krate nodes that are members.
    let mut members: HashMap<krates::NodeId, (String, PathBuf)> = HashMap::new();
    for idx in krates.graph().node_indices() {
        if let krates::Node::Krate { id, krate, .. } = &krates.graph()[idx] {
            if let Some(name) = member_kids.get(&id.repr) {
                let dir = krate.manifest_path.parent().map(PathBuf::from).unwrap_or_default();
                members.insert(idx, (name.clone(), dir));
            }
        }
    }

    let mut deps: HashMap<String, HashSet<String>> = HashMap::new();
    for (idx, (name, _dir)) in &members {
        for (dep_node, _edge) in krates.get_deps(*idx) {
            if let krates::Node::Krate { id, .. } = dep_node {
                if let Some(dep_name) = member_kids.get(&id.repr) {
                    deps.entry(name.clone()).or_default().insert(dep_name.clone());
                }
            }
        }
    }

    let nodes = members
        .into_values()
        .map(|(name, manifest_dir)| CrateNode { name, manifest_dir })
        .collect();
    Ok((nodes, deps))
}

/// Kahn levels over workspace-internal edges: dependency crates first.
pub fn compute_waves(
    nodes: &[CrateNode],
    deps: &HashMap<String, HashSet<String>>,
) -> Result<Vec<Vec<CrateNode>>> {
    let names: HashSet<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
    let mut indegree: HashMap<&str, usize> = HashMap::new();
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();

    for n in nodes {
        indegree.entry(n.name.as_str()).or_insert(0);
    }
    for (crate_name, ds) in deps {
        for d in ds {
            if names.contains(d.as_str()) && names.contains(crate_name.as_str()) {
                *indegree.entry(crate_name.as_str()).or_insert(0) += 1;
                dependents.entry(d.as_str()).or_default().push(crate_name.as_str());
            }
        }
    }

    let mut queue: VecDeque<&str> =
        indegree.iter().filter(|&(_, d)| *d == 0).map(|(k, _)| *k).collect();
    let mut waves: Vec<Vec<CrateNode>> = Vec::new();
    let mut processed = 0usize;

    while !queue.is_empty() {
        let mut wave = Vec::new();
        for _ in 0..queue.len() {
            let name = queue.pop_front().unwrap();
            processed += 1;
            if let Some(node) = nodes.iter().find(|n| n.name == name) {
                wave.push(node.clone());
            }
            for dep in dependents.get(name).into_iter().flatten() {
                let e = indegree.get_mut(dep).unwrap();
                *e -= 1;
                if *e == 0 {
                    queue.push_back(dep);
                }
            }
        }
        waves.push(wave);
    }

    if processed != nodes.len() {
        anyhow::bail!("dependency cycle detected among workspace crates — refuse to schedule");
    }
    Ok(waves)
}

pub struct GruntSwarmOrchestrator {
    concurrency_limit: Arc<Semaphore>,
    active_crates: Arc<Mutex<HashSet<PathBuf>>>,
    workspace_root: PathBuf,
}

impl GruntSwarmOrchestrator {
    pub fn new(workspace_root: PathBuf, max_concurrent_workers: usize) -> Self {
        Self {
            concurrency_limit: Arc::new(Semaphore::new(max_concurrent_workers)),
            active_crates: Arc::new(Mutex::new(HashSet::new())),
            workspace_root,
        }
    }

    pub fn workspace_root(&self) -> &std::path::Path {
        &self.workspace_root
    }

    /// Resolve the nearest crate root (dir containing Cargo.toml) for a file.
    pub fn resolve_crate_root(&self, file: &std::path::Path) -> Option<PathBuf> {
        let mut cur = file.parent()?;
        loop {
            if cur.join("Cargo.toml").is_file() {
                return Some(cur.to_path_buf());
            }
            cur = cur.parent()?;
        }
    }

    /// Dispatch one wave's tasks; same-crate tasks are serialized via the
    /// lock table, cross-crate tasks run in parallel under the semaphore.
    pub async fn run_wave<F, Fut>(
        &self,
        tasks: Vec<(CrateNode, PathBuf)>,
        worker: F,
    ) -> Vec<Result<String>>
    where
        F: Fn(PathBuf, PathBuf) -> Fut + Clone + Send + 'static,
        Fut: std::future::Future<Output = Result<String>> + Send + 'static,
    {
        let mut join_set = tokio::task::JoinSet::new();
        let mut deferred: Vec<(CrateNode, PathBuf)> = Vec::new();
        let mut outcomes = Vec::new();

        for (crate_node, target_file) in tasks {
            let crate_dir = crate_node.manifest_dir.clone();
            let mut locks = self.active_crates.lock().await;
            if locks.contains(&crate_dir) {
                deferred.push((crate_node, target_file));
                continue;
            }
            locks.insert(crate_dir.clone());
            drop(locks);

            let sem = Arc::clone(&self.concurrency_limit);
            let locks_ref = Arc::clone(&self.active_crates);
            let worker = worker.clone();
            join_set.spawn(async move {
                let _permit = sem.acquire().await.expect("semaphore open");
                let result = worker(crate_dir.clone(), target_file).await;
                locks_ref.lock().await.remove(&crate_dir);
                result
            });
        }

        while let Some(res) = join_set.join_next().await {
            match res {
                Ok(outcome) => outcomes.push(outcome),
                Err(e) => outcomes.push(Err(anyhow::anyhow!("worker panicked: {e}"))),
            }
        }

        // Same-wave same-crate tasks deferred by the lock run sequentially now.
        if !deferred.is_empty() {
            let mut more = self.run_wave(deferred, worker).await;
            outcomes.append(&mut more);
        }
        outcomes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str) -> CrateNode {
        CrateNode { name: name.into(), manifest_dir: PathBuf::from(format!("crates/{name}")) }
    }

    #[test]
    fn waves_respect_dependency_order() {
        let nodes = vec![node("core"), node("daemon"), node("tools")];
        let mut deps = HashMap::new();
        deps.insert("daemon".to_string(), HashSet::from(["core".to_string()]));
        deps.insert("tools".to_string(), HashSet::from(["core".to_string()]));

        let waves = compute_waves(&nodes, &deps).unwrap();
        assert_eq!(waves.len(), 2);
        assert_eq!(waves[0][0].name, "core");
        let wave1: Vec<&str> = waves[1].iter().map(|n| n.name.as_str()).collect();
        assert!(wave1.contains(&"daemon") && wave1.contains(&"tools"));
    }

    #[test]
    fn detects_cycles() {
        let nodes = vec![node("a"), node("b")];
        let mut deps = HashMap::new();
        deps.insert("a".to_string(), HashSet::from(["b".to_string()]));
        deps.insert("b".to_string(), HashSet::from(["a".to_string()]));
        assert!(compute_waves(&nodes, &deps).is_err());
    }
}
