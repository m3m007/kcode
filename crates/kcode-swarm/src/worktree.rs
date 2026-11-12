use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use tokio::process::Command as AsyncCommand;

pub struct EphemeralWorktree {
    pub task_id: String,
    pub path: PathBuf,
    pub branch_name: String,
    pub target_dir: PathBuf,
    repo_root: PathBuf,
    keep_on_drop: bool,
}

impl EphemeralWorktree {
    /// `git worktree add -b <branch> <dir> <base_ref>`.
    /// Worktrees live under `<repo>/target/agent_worktrees/<task_id>` — inside
    /// the universally ignored target/ dir, so git never sees them.
    pub async fn create(repo_root: &Path, task_id: &str, base_ref: &str) -> Result<Self> {
        let branch_name = format!("agent/task-{task_id}");
        let worktree_dir = repo_root.join("target/agent_worktrees").join(task_id);
        let target_dir = repo_root.join("target/agent_swarm").join(task_id);

        tokio::fs::create_dir_all(&target_dir).await?;
        let out = AsyncCommand::new("git")
            .current_dir(repo_root)
            .args(["worktree", "add", "-b", &branch_name])
            .arg(&worktree_dir)
            .arg(base_ref)
            .output()
            .await
            .context("spawning git worktree add")?;
        if !out.status.success() {
            anyhow::bail!("git worktree add failed: {}", String::from_utf8_lossy(&out.stderr));
        }

        Ok(Self {
            task_id: task_id.to_string(),
            path: worktree_dir,
            branch_name,
            target_dir,
            repo_root: repo_root.to_path_buf(),
            keep_on_drop: false,
        })
    }

    /// Environment for every worker/checker subprocess in this worktree.
    pub fn cargo_env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("CARGO_TARGET_DIR", self.target_dir.display().to_string()),
            ("CARGO_INCREMENTAL", "0".into()),
            ("RUSTC_WRAPPER", "sccache".into()),
            ("SCCACHE_DIR", self.repo_root.join(".sccache").display().to_string()),
            ("SCCACHE_CACHE_SIZE", "20G".into()),
            ("SCCACHE_BASEDIRS", self.repo_root.display().to_string()),
            ("INSTA_UPDATE", "no".into()),
        ]
    }

    pub fn apply_env(&self, cmd: &mut AsyncCommand) {
        for (k, v) in self.cargo_env() {
            cmd.env(k, v);
        }
    }

    /// Commit everything (incl. untracked) inside the worktree; returns SHA.
    /// v1.1 fix: fully async — the old version called blocking
    /// `std::process::Command` inside an `async fn`, stalling the Tokio worker.
    pub async fn commit_changes(&self, message: &str) -> Result<String> {
        let add = AsyncCommand::new("git")
            .current_dir(&self.path)
            .args(["add", "-A"])
            .output()
            .await
            .context("git add -A in worktree")?;
        if !add.status.success() {
            anyhow::bail!("git add -A failed in worktree {}", self.task_id);
        }
        let commit = AsyncCommand::new("git")
            .current_dir(&self.path)
            .args(["commit", "-m", message])
            .output()
            .await
            .context("git commit in worktree")?;
        if !commit.status.success() {
            anyhow::bail!("git commit failed in worktree {}", self.task_id);
        }
        let head = AsyncCommand::new("git")
            .current_dir(&self.path)
            .args(["rev-parse", "HEAD"])
            .output()
            .await
            .context("git rev-parse in worktree")?;
        Ok(String::from_utf8_lossy(&head.stdout).trim().to_string())
    }

    /// Merge this branch into `target_branch`. Precondition: the main checkout
    /// (repo_root) is clean and checked out on the integration branch; the
    /// orchestrator pins it at startup. `checkout` is a no-op if already there.
    pub async fn merge_into(&self, target_branch: &str) -> Result<()> {
        let current = AsyncCommand::new("git")
            .current_dir(&self.repo_root)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .await?;
        let on_branch = String::from_utf8_lossy(&current.stdout).trim() == target_branch;
        if !on_branch {
            let co = AsyncCommand::new("git")
                .current_dir(&self.repo_root)
                .args(["checkout", target_branch])
                .output()
                .await?;
            if !co.status.success() {
                anyhow::bail!(
                    "cannot switch integration branch to {target_branch}: {}",
                    String::from_utf8_lossy(&co.stderr)
                );
            }
        }
        let merge_msg = format!("Merge agent task {}", self.task_id);
        let out = AsyncCommand::new("git")
            .current_dir(&self.repo_root)
            .args(["merge", "--no-ff", self.branch_name.as_str(), "-m", merge_msg.as_str()])
            .output()
            .await?;
        if !out.status.success() {
            anyhow::bail!(
                "merge of {} failed (conflict or dirty main checkout): {}",
                self.branch_name,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(())
    }

    pub async fn clean_up(mut self) -> Result<()> {
        self.teardown().await
    }

    async fn teardown(&mut self) -> Result<()> {
        let _ = AsyncCommand::new("git")
            .current_dir(&self.repo_root)
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .output()
            .await;
        let _ = AsyncCommand::new("git")
            .current_dir(&self.repo_root)
            .args(["branch", "-D", &self.branch_name])
            .output()
            .await;
        let _ = AsyncCommand::new("git")
            .current_dir(&self.repo_root)
            .args(["worktree", "prune"])
            .output()
            .await;
        self.keep_on_drop = true;
        Ok(())
    }
}

impl Drop for EphemeralWorktree {
    /// RAII: worktrees must not leak when a task panics or is cancelled.
    /// Uses the blocking std::process API because Drop is synchronous.
    fn drop(&mut self) {
        if self.keep_on_drop {
            return;
        }
        let repo = self.repo_root.clone();
        let path = self.path.clone();
        let branch = self.branch_name.clone();
        std::thread::spawn(move || {
            let _ = Command::new("git").current_dir(&repo)
                .args(["worktree", "remove", "--force"])
                .arg(&path)
                .output();
            let _ = Command::new("git").current_dir(&repo).args(["branch", "-D", &branch]).output();
            let _ = Command::new("git").current_dir(&repo).args(["worktree", "prune"]).output();
        });
    }
}
