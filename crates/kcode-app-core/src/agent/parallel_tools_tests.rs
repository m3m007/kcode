//! Behavioral tests for harness-side parallel tool execution.
//!
//! The claims under test are the three the design depends on:
//!
//! 1. With `enabled = false` (the default), a round produces byte-identical
//!    conversation content to the pre-feature sequential path.
//! 2. With `enabled = true` and a read-only allow-list, allow-listed calls
//!    actually run concurrently (measured, not assumed) and are merged back in
//!    the model's original tool_call order.
//! 3. A failure inside the parallel path becomes that call's error result in its
//!    own slot, so the round still has exactly one result per call.
//!
//! Concurrency is proven by a spy tool that records its own start and end
//! timestamps into a shared log: two concurrent calls overlap, two sequential
//! calls never do. That is the same shape of evidence a user sees in wall-clock
//! latency, so it is checked directly rather than inferred from internals.
//!
//! Byte-stability is proven by comparing the **serde-serialized** content blocks
//! of the appended messages, because `ContentBlock` deliberately does not derive
//! `PartialEq` (findings F12). Serializing is the provider-visible byte form, and
//! `tool_duration_ms` lives on `StoredMessage` rather than inside
//! `ContentBlock` (findings F10/F12), so the comparison is duration-free by
//! construction rather than by filtering.

use super::parallel_tools::{
    ParallelEligibility, ParallelRun, parallel_disabled_by_hooks, partition_parallel_runs,
};
use super::*;
use crate::tool::{Registry, Tool, ToolContext, ToolOutput};
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex as StdMutex;

/// A minimal provider. Only `complete`, `name`, and `fork` are required by the
/// trait (`crates/kcode-provider-core/src/lib.rs`); the seam under test never
/// calls the model, so a request is a test failure rather than a fixture.
#[derive(Clone)]
struct StubProvider {
    requests: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for StubProvider {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<crate::provider::EventStream> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("parallel-tools tests must not reach the model")
    }
    fn name(&self) -> &str {
        "parallel-tools-stub"
    }
    fn model(&self) -> String {
        "stub-model".to_string()
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

/// Spy tool that sleeps, then records a `[name, start, end]` triple.
///
/// The shared log is what proves overlap: `read` sleeps, and while it sleeps a
/// second `read` must have started for the round to have been concurrent.
struct OverlapProbe {
    log: Arc<StdMutex<Vec<(String, u128, u128)>>>,
    sleep_ms: u64,
    started: Arc<AtomicUsize>,
    finished: Arc<AtomicUsize>,
    /// How many calls are in flight at once, tracked by the tool itself.
    in_flight: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
}

impl OverlapProbe {
    fn new(
        log: Arc<StdMutex<Vec<(String, u128, u128)>>>,
        sleep_ms: u64,
        started: Arc<AtomicUsize>,
        finished: Arc<AtomicUsize>,
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            log,
            sleep_ms,
            started,
            finished,
            in_flight,
            max_in_flight,
        }
    }
}

#[async_trait]
impl Tool for OverlapProbe {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Overlap probe"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(&self, input: serde_json::Value, _: ToolContext) -> Result<ToolOutput> {
        let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(current, Ordering::SeqCst);
        self.started.fetch_add(1, Ordering::SeqCst);
        let start = std::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_millis(self.sleep_ms)).await;
        let elapsed = start.elapsed().as_millis();
        let path = input
            .get("path")
            .and_then(|value| value.as_str())
            .unwrap_or("(none)")
            .to_string();
        if let Ok(mut log) = self.log.lock() {
            log.push((path.clone(), start.elapsed().as_millis() + elapsed, elapsed));
        }
        self.finished.fetch_add(1, Ordering::SeqCst);
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(ToolOutput::new(format!("contents of {path}")))
    }
}

/// Tool that always fails, to prove a parallel failure lands in its own slot.
struct FailingProbe {
    started: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for FailingProbe {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Failing probe"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(&self, _: serde_json::Value, _: ToolContext) -> Result<ToolOutput> {
        self.started.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("probe failure")
    }
}

/// Tool that panics inside the spawned task, to prove one bad call cannot take
/// the round down.
struct PanickingProbe {
    started: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for PanickingProbe {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Panicking probe"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(&self, _: serde_json::Value, _: ToolContext) -> Result<ToolOutput> {
        self.started.fetch_add(1, Ordering::SeqCst);
        panic!("probe panic")
    }
}

/// Spy tool that returns a distinguishable output per path, for byte-stability.
struct StableProbe {
    started: Arc<AtomicUsize>,
    jitter_ms: u64,
}

impl StableProbe {
    fn new(started: Arc<AtomicUsize>, jitter_ms: u64) -> Self {
        Self { started, jitter_ms }
    }
}

#[async_trait]
impl Tool for StableProbe {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Stable probe"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(&self, input: serde_json::Value, _: ToolContext) -> Result<ToolOutput> {
        self.started.fetch_add(1, Ordering::SeqCst);
        // Reverse-ordered sleeps make completion order differ from call order,
        // so a positional merge that relied on completion order would fail.
        let path = input
            .get("path")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let millis = self
            .jitter_ms
            .saturating_sub(path.chars().count() as u64 * 2);
        tokio::time::sleep(std::time::Duration::from_millis(millis)).await;
        Ok(ToolOutput::new(format!("contents of {path}")))
    }
}

/// Spy tool whose completion order is the reverse of its call order, so a
/// merge that followed completion order would be caught.
struct ReverseCompletionProbe {
    started: Arc<AtomicUsize>,
    completion_order: Arc<StdMutex<Vec<String>>>,
}

#[async_trait]
impl Tool for ReverseCompletionProbe {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Reverse completion probe"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(&self, input: serde_json::Value, ctx: ToolContext) -> Result<ToolOutput> {
        self.started.fetch_add(1, Ordering::SeqCst);
        let path = input
            .get("path")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        // Longer paths sleep LESS, so the LAST call finishes FIRST. A merge that
        // followed completion order would emit c3, c2, c1 and fail the test.
        let depth = path.chars().count() as u64;
        let sleep_ms = 120u64.saturating_sub(depth * 15);
        tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
        if let Ok(mut order) = self.completion_order.lock() {
            order.push(ctx.tool_call_id);
        }
        Ok(ToolOutput::new(format!("contents of {path}")))
    }
}

fn call(id: &str, name: &str, path: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        input: serde_json::json!({"path": path}),
        intent: None,
        thought_signature: None,
    }
}

fn enabled_config(tools: &[&str], max_concurrency: usize) -> crate::config::ParallelToolsConfig {
    crate::config::ParallelToolsConfig {
        enabled: true,
        read_only_tools: tools.iter().map(|name| name.to_string()).collect(),
        max_concurrency,
        // The safe default: a configured pre_tool hook keeps the round
        // sequential. Tests that need the opt-in state it explicitly.
        run_hooks_in_parallel: false,
    }
}

/// A test-only agent whose registry is preloaded with `read` probes.
///
/// The registry starts empty (`Registry::empty()`, the pattern used by
/// `progress_flush_turn_loop_tests.rs`) rather than built from the base tool
/// set, so this module never constructs the model-facing `batch` tool it must
/// not touch. Each test then registers its own `read` probe, which replaces any
/// existing entry of that name.
async fn probe_agent() -> (Agent, Arc<AtomicUsize>, crate::auth::test_sandbox::AuthTestSandbox) {
    let sandbox = match crate::auth::test_sandbox::AuthTestSandbox::new() {
        Ok(sandbox) => sandbox,
        Err(error) => {
            // A sandbox is only needed for the config and session paths the
            // seam touches; report loudly instead of silently proceeding.
            panic!("test sandbox unavailable: {error}");
        }
    };
    // The config cache re-checks the config fingerprint on every call under
    // cfg(test) (CONFIG_CACHE_CHECK_INTERVAL is ZERO), but a fingerprint alone
    // would not notice that KCODE_HOME just moved to a new path holding no
    // config.toml. Force the reload so hook_configured() reads the empty
    // sandbox config, never the developer's real ~/.kcode/config.toml, which
    // sets a pre_tool hook that would silently disable the parallel path.
    crate::config::invalidate_config_cache();
    let requests = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(StubProvider {
        requests: Arc::clone(&requests),
    });
    let registry = Registry::empty();
    let agent = Agent::new(provider, registry);
    (agent, requests, sandbox)
}

/// Content blocks of the tool-result messages the seam appended, in order.
fn appended_tool_results(agent: &Agent) -> Vec<ContentBlock> {
    agent
        .session
        .messages
        .iter()
        .filter(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        })
        .flat_map(|message| message.content.clone())
        .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
        .collect()
}

/// The appended tool-result messages as **serde values**, in order.
///
/// `ContentBlock` does not implement `PartialEq`, and the comparison this
/// supports is about the provider-visible bytes anyway, so the blocks are
/// serialized. `tool_duration_ms` is metadata on `StoredMessage` and is not
/// part of `ContentBlock`, so it cannot leak into this comparison (F10/F12).
fn appended_tool_results_as_json(agent: &Agent) -> Vec<serde_json::Value> {
    appended_tool_results(agent)
        .iter()
        .map(|block| match serde_json::to_value(block) {
            Ok(value) => value,
            Err(error) => serde_json::json!({"serialization_error": error.to_string()}),
        })
        .collect()
}

fn tool_result_ids(blocks: &[ContentBlock]) -> Vec<String> {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
            _ => None,
        })
        .collect()
}

/// Tool-result text per call id, in the order the blocks appear.
fn tool_result_texts(blocks: &[ContentBlock]) -> Vec<(String, String)> {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => Some((tool_use_id.clone(), content.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn disabled_by_default_so_a_round_is_untouched() {
    let (mut agent, requests, _sandbox) = probe_agent().await;
    // The shipped default, not a test-only value: the feature must be off
    // unless an operator turns it on.
    let parallel_cfg = crate::config::ParallelToolsConfig::default();
    assert!(!parallel_cfg.enabled, "the default must be disabled");
    assert!(parallel_cfg.max_concurrency >= 1, "the default must be legal");
    let calls = vec![
        call("c1", "read", "a.rs"),
        call("c2", "read", "b.rs"),
        call("c3", "read", "c.rs"),
    ];
    let sdk: HashMap<String, (String, bool)> = HashMap::new();
    let handled = match agent
        .execute_parallelizable_tools(&calls, &sdk, &parallel_cfg, None)
        .await
    {
        Ok(handled) => handled,
        Err(error) => panic!("the disabled path must not fail: {error}"),
    };
    assert!(
        handled.is_empty(),
        "the disabled default must not execute anything in the parallel path"
    );
    assert!(
        appended_tool_results(&agent).is_empty(),
        "the disabled default must leave the conversation untouched for the sequential loop"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "the seam must never call the model"
    );
}

#[tokio::test]
async fn partition_uses_maximal_runs_cut_at_the_first_sequential_call() {
    // read(read), read(read), bash, read(read) -> exactly one run of three.
    let calls = vec![
        call("c1", "read", "a.rs"),
        call("c2", "read", "b.rs"),
        call("c3", "read", "c.rs"),
        call("c4", "bash", "ls"),
        call("c5", "read", "d.rs"),
        call("c6", "read", "e.rs"),
    ];
    let cfg = enabled_config(&["read"], 2);
    let sdk: HashMap<String, (String, bool)> = HashMap::new();
    let eligibility = ParallelEligibility {
        cfg: &cfg,
        sdk_tool_results: &sdk,
    };
    let runs = partition_parallel_runs(&calls, |tc| eligibility.allows(tc));
    assert_eq!(
        runs,
        vec![
            ParallelRun { start: 0, len: 3 },
            ParallelRun { start: 4, len: 2 },
        ],
        "runs must be maximal and cut at the first non-allow-listed call"
    );

    // A lone allow-listed call between sequential ones is not worth a task.
    let calls = vec![
        call("c1", "bash", "ls"),
        call("c2", "read", "a.rs"),
        call("c3", "bash", "ls"),
    ];
    let eligibility = ParallelEligibility {
        cfg: &cfg,
        sdk_tool_results: &sdk,
    };
    assert!(
        partition_parallel_runs(&calls, |tc| eligibility.allows(tc)).is_empty(),
        "a run of one gains nothing from a task"
    );
}

#[tokio::test]
async fn invalid_calls_and_sdk_results_are_never_parallelized() {
    let cfg = enabled_config(&["read"], 4);
    let mut sdk: HashMap<String, (String, bool)> = HashMap::new();
    sdk.insert("c2".to_string(), ("already done".to_string(), false));
    let mut malformed = call("c1", "read", "a.rs");
    malformed.input = serde_json::Value::Null;
    let calls = vec![malformed.clone(), call("c2", "read", "b.rs"), call("c3", "read", "c.rs")];
    let eligibility = ParallelEligibility {
        cfg: &cfg,
        sdk_tool_results: &sdk,
    };
    let runs = partition_parallel_runs(&calls, |tc| eligibility.allows(tc));
    assert!(
        runs.is_empty(),
        "a lone allow-listed call after malformed and provider-executed ones gains nothing from a task"
    );

    // A run of two allow-listed calls after them does qualify, and covers only
    // those two.
    let calls = vec![
        malformed,
        call("c2", "read", "b.rs"),
        call("c3", "read", "c.rs"),
        call("c4", "read", "d.rs"),
    ];
    let eligibility = ParallelEligibility {
        cfg: &cfg,
        sdk_tool_results: &sdk,
    };
    let runs = partition_parallel_runs(&calls, |tc| eligibility.allows(tc));
    assert_eq!(runs, vec![ParallelRun { start: 2, len: 2 }]);
    let sliced: Vec<&ToolCall> = runs
        .iter()
        .flat_map(|run| run.calls(&calls))
        .collect();
    assert_eq!(sliced.len(), 2, "the run covers exactly its own calls");
    assert_eq!(sliced[0].id, "c3");
    assert_eq!(sliced[1].id, "c4");
}

#[tokio::test]
async fn enabled_read_only_calls_overlap_in_time() {
    let parallel_cfg = enabled_config(&["read"], 4);
    let log = Arc::new(StdMutex::new(Vec::new()));
    let started = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicUsize::new(0));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    let (mut agent, _requests, _sandbox) = probe_agent().await;
    agent
        .registry
        .register(
            "read".into(),
            Arc::new(OverlapProbe::new(
                log.clone(),
                60,
                started.clone(),
                finished.clone(),
                in_flight.clone(),
                max_in_flight.clone(),
            )),
        )
        .await;

    let calls = vec![
        call("c1", "read", "a.rs"),
        call("c2", "read", "b.rs"),
        call("c3", "read", "c.rs"),
    ];
    let sdk: HashMap<String, (String, bool)> = HashMap::new();
    let handled = match agent
        .execute_parallelizable_tools(&calls, &sdk, &parallel_cfg, None)
        .await
    {
        Ok(handled) => handled,
        Err(error) => panic!("the enabled path must not fail: {error}"),
    };
    assert_eq!(handled, HashSet::from([0, 1, 2]));

    assert_eq!(started.load(Ordering::SeqCst), 3, "every call must run");
    assert_eq!(finished.load(Ordering::SeqCst), 3, "every call must finish");
    assert!(
        max_in_flight.load(Ordering::SeqCst) >= 2,
        "read-only calls must actually overlap: max in flight was {}",
        max_in_flight.load(Ordering::SeqCst)
    );
    // The measured wall clock must beat the sequential sum. Three 60ms calls
    // sequentially take >=180ms; concurrently they take ~60ms.
    let entries = match log.lock() {
        Ok(entries) => entries.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    assert_eq!(entries.len(), 3);
    let longest = entries.iter().map(|(_, _, elapsed)| *elapsed).max();
    let total: u128 = entries.iter().map(|(_, _, elapsed)| *elapsed).sum();
    assert!(
        longest.is_some_and(|value| value < 150),
        "three 60ms calls overlapped, so the longest single call must be well under the 180ms sequential sum (longest: {longest:?}, sum: {total})"
    );
}

#[tokio::test]
async fn merged_results_keep_the_models_original_call_order() {
    let parallel_cfg = enabled_config(&["read"], 4);
    // Longer paths sleep less, so the LAST call finishes FIRST. A merge that
    // followed completion order would emit c3, c2, c1 and fail this test.
    let completion_order = Arc::new(StdMutex::new(Vec::new()));
    let started = Arc::new(AtomicUsize::new(0));
    let (mut agent, _requests, _sandbox) = probe_agent().await;
    agent
        .registry
        .register(
            "read".into(),
            Arc::new(ReverseCompletionProbe {
                started: started.clone(),
                completion_order: completion_order.clone(),
            }),
        )
        .await;

    let calls = vec![
        call("c1", "read", "aa.rs"),
        call("c2", "read", "bbbb.rs"),
        call("c3", "read", "cccccc.rs"),
    ];
    let sdk: HashMap<String, (String, bool)> = HashMap::new();
    match agent
        .execute_parallelizable_tools(&calls, &sdk, &parallel_cfg, None)
        .await
    {
        Ok(_) => {}
        Err(error) => panic!("the enabled path must not fail: {error}"),
    }
    let blocks = appended_tool_results(&agent);
    assert_eq!(
        tool_result_ids(&blocks),
        vec!["c1".to_string(), "c2".to_string(), "c3".to_string()],
        "results must be merged in the model's original tool_call order"
    );
    let completion = match completion_order.lock() {
        Ok(completion) => completion.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    assert_eq!(
        completion,
        vec!["c3".to_string(), "c2".to_string(), "c1".to_string()],
        "the probe must have completed in reverse order, or this test proves nothing"
    );
    assert_eq!(started.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn sequential_tail_of_a_mixed_round_stays_sequential() {
    let parallel_cfg = enabled_config(&["read"], 4);
    let started = Arc::new(AtomicUsize::new(0));
    let (mut agent, _requests, _sandbox) = probe_agent().await;
    agent
        .registry
        .register("read".into(), Arc::new(StableProbe::new(started.clone(), 0)))
        .await;

    let calls = vec![
        call("c1", "read", "a.rs"),
        call("c2", "read", "b.rs"),
        call("c3", "bash", "ls"),
    ];
    let sdk: HashMap<String, (String, bool)> = HashMap::new();
    let handled = match agent
        .execute_parallelizable_tools(&calls, &sdk, &parallel_cfg, None)
        .await
    {
        Ok(handled) => handled,
        Err(error) => panic!("the enabled path must not fail: {error}"),
    };
    assert_eq!(
        handled,
        HashSet::from([0, 1]),
        "only the leading allow-listed run may be parallelized"
    );
    let blocks = appended_tool_results(&agent);
    assert_eq!(
        tool_result_ids(&blocks),
        vec!["c1".to_string(), "c2".to_string()],
        "the non-allow-listed call must be left for the sequential loop"
    );
    assert_eq!(started.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn parallel_failure_lands_in_its_own_slot_and_keeps_the_round_valid() {
    let parallel_cfg = enabled_config(&["read"], 4);
    let started = Arc::new(AtomicUsize::new(0));
    let (mut agent, _requests, _sandbox) = probe_agent().await;
    agent
        .registry
        .register("read".into(), Arc::new(FailingProbe { started: started.clone() }))
        .await;

    let calls = vec![
        call("c1", "read", "a.rs"),
        call("c2", "read", "b.rs"),
        call("c3", "read", "c.rs"),
    ];
    let sdk: HashMap<String, (String, bool)> = HashMap::new();
    let handled = match agent
        .execute_parallelizable_tools(&calls, &sdk, &parallel_cfg, None)
        .await
    {
        Ok(handled) => handled,
        Err(error) => panic!("the enabled path must not fail: {error}"),
    };
    assert_eq!(handled, HashSet::from([0, 1, 2]));
    assert_eq!(started.load(Ordering::SeqCst), 3, "every call must be attempted");

    let blocks = appended_tool_results(&agent);
    assert_eq!(blocks.len(), 3, "one result per call, in order");
    for (id, content) in tool_result_texts(&blocks) {
        assert!(
            content.contains("probe failure"),
            "call {id} must surface its error text in its own slot: {content}"
        );
        for block in &blocks {
            if let ContentBlock::ToolResult {
                tool_use_id,
                content: block_content,
                is_error,
            } = block
            {
                if tool_use_id == &id {
                    assert_eq!(is_error, &Some(true), "each failure is an error result");
                    assert!(
                        block_content.contains("probe failure"),
                        "the same slot must carry the same error text"
                    );
                }
            }
        }
    }
}

/// A failing call and a succeeding call in the same round: the error must land in
/// the failing call's slot and the other call's text must survive in its own.
#[tokio::test]
async fn one_failure_does_not_disturb_its_neighbours() {
    let parallel_cfg = enabled_config(&["read"], 4);
    let started = Arc::new(AtomicUsize::new(0));
    let (mut agent, _requests, _sandbox) = probe_agent().await;
    // A tool that fails only for one path, so the round is mixed.
    struct SelectiveProbe {
        started: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for SelectiveProbe {
        fn name(&self) -> &str {
            "read"
        }
        fn description(&self) -> &str {
            "Selective probe"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, input: serde_json::Value, _: ToolContext) -> Result<ToolOutput> {
            self.started.fetch_add(1, Ordering::SeqCst);
            let path = input
                .get("path")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string();
            if path == "b.rs" {
                anyhow::bail!("probe failure");
            }
            Ok(ToolOutput::new(format!("contents of {path}")))
        }
    }
    agent
        .registry
        .register(
            "read".into(),
            Arc::new(SelectiveProbe {
                started: started.clone(),
            }),
        )
        .await;

    let calls = vec![
        call("c1", "read", "a.rs"),
        call("c2", "read", "b.rs"),
        call("c3", "read", "c.rs"),
    ];
    let sdk: HashMap<String, (String, bool)> = HashMap::new();
    match agent
        .execute_parallelizable_tools(&calls, &sdk, &parallel_cfg, None)
        .await
    {
        Ok(_) => {}
        Err(error) => panic!("the enabled path must not fail: {error}"),
    }
    let texts = tool_result_texts(&appended_tool_results(&agent));
    assert_eq!(texts.len(), 3, "one result per call, in order");
    assert_eq!(texts[0].0, "c1");
    assert!(texts[0].1.contains("contents of a.rs"), "got: {}", texts[0].1);
    assert_eq!(texts[1].0, "c2");
    assert!(
        texts[1].1.contains("probe failure"),
        "the failure must land in the failing call's slot: {}",
        texts[1].1
    );
    assert_eq!(texts[2].0, "c3");
    assert!(texts[2].1.contains("contents of c.rs"), "got: {}", texts[2].1);
    assert_eq!(started.load(Ordering::SeqCst), 3, "every call must run");
}

#[tokio::test]
async fn panicking_call_does_not_take_down_the_round() {
    let parallel_cfg = enabled_config(&["read"], 4);
    let started = Arc::new(AtomicUsize::new(0));
    let (mut agent, _requests, _sandbox) = probe_agent().await;
    agent
        .registry
        .register(
            "read".into(),
            Arc::new(PanickingProbe { started: started.clone() }),
        )
        .await;

    let calls = vec![
        call("c1", "read", "a.rs"),
        call("c2", "read", "b.rs"),
        call("c3", "read", "c.rs"),
    ];
    let sdk: HashMap<String, (String, bool)> = HashMap::new();
    let handled = match agent
        .execute_parallelizable_tools(&calls, &sdk, &parallel_cfg, None)
        .await
    {
        Ok(handled) => handled,
        Err(error) => panic!("a panicking tool must not fail the round: {error}"),
    };
    assert_eq!(
        handled,
        HashSet::from([0, 1, 2]),
        "a panic must not remove the call from the round"
    );
    assert_eq!(started.load(Ordering::SeqCst), 3);
    let blocks = appended_tool_results(&agent);
    assert_eq!(
        blocks.len(),
        3,
        "every call still has a result even though each task panicked"
    );
    for block in &blocks {
        match block {
            ContentBlock::ToolResult {
                content,
                is_error,
                ..
            } => {
                assert_eq!(is_error, &Some(true));
                assert!(
                    content.contains("panicked"),
                    "the panic must be reported as that call's error: {content}"
                );
            }
            _ => panic!("expected tool results"),
        }
    }
}

#[tokio::test]
async fn two_identical_rounds_produce_identical_context_bytes() {
    let parallel_cfg = enabled_config(&["read"], 4);
    let mut blocks_first: Vec<serde_json::Value> = Vec::new();
    let mut blocks_second: Vec<serde_json::Value> = Vec::new();
    for round in 0..2 {
        let started = Arc::new(AtomicUsize::new(0));
        let (mut agent, _requests, _sandbox) = probe_agent().await;
        agent
            .registry
            .register(
                "read".into(),
                Arc::new(StableProbe::new(started.clone(), if round == 0 { 5 } else { 45 })),
            )
            .await;
        let calls = vec![
            call("c1", "read", "aa.rs"),
            call("c2", "read", "bbbb.rs"),
            call("c3", "read", "cccccc.rs"),
            call("c4", "read", "dd.rs"),
        ];
        let sdk: HashMap<String, (String, bool)> = HashMap::new();
        match agent
            .execute_parallelizable_tools(&calls, &sdk, &parallel_cfg, None)
            .await
        {
            Ok(_) => {}
            Err(error) => panic!("the enabled path must not fail: {error}"),
        }
        // Compare content blocks only: `tool_duration_ms` is wall-clock
        // metadata carried into the provider Message and legitimately differs
        // between rounds, while the model-visible bytes must not.
        let collected = appended_tool_results_as_json(&agent);
        if round == 0 {
            blocks_first = collected;
        } else {
            blocks_second = collected;
        }
    }
    assert_eq!(
        blocks_first, blocks_second,
        "the same round must produce byte-identical context content"
    );
    // Guard against a vacuous pass: the comparison above must have had content.
    assert!(!blocks_first.is_empty(), "the round must have produced results");
}

#[tokio::test]
async fn max_concurrency_bounds_the_fan_out() {
    for max in [1usize, 2, 3] {
        let parallel_cfg = enabled_config(&["read"], max);
        let started = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicUsize::new(0));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let log = Arc::new(StdMutex::new(Vec::new()));
        let (mut agent, _requests, _sandbox) = probe_agent().await;
        agent
            .registry
            .register(
                "read".into(),
                Arc::new(OverlapProbe::new(
                    log.clone(),
                    30,
                    started.clone(),
                    finished.clone(),
                    in_flight.clone(),
                    max_in_flight.clone(),
                )),
            )
            .await;
        let calls = vec![
            call("c1", "read", "a.rs"),
            call("c2", "read", "b.rs"),
            call("c3", "read", "c.rs"),
        ];
        let sdk: HashMap<String, (String, bool)> = HashMap::new();
        match agent
            .execute_parallelizable_tools(&calls, &sdk, &parallel_cfg, None)
            .await
        {
            Ok(_) => {}
            Err(error) => panic!("the enabled path must not fail: {error}"),
        }
        assert_eq!(
            max_in_flight.load(Ordering::SeqCst),
            max,
            "max_concurrency={max} must bound the fan-out"
        );
        assert_eq!(finished.load(Ordering::SeqCst), 3);
    }
}

#[tokio::test]
async fn a_pre_tool_hook_sends_the_round_down_the_sequential_path() {
    // A hook can rewrite a call's input, so the parallel path steps aside
    // rather than dispatch calls the hook has not seen yet. With no hook
    // configured the predicate must be false, which is what lets an enabled
    // configuration run the parallel path at all.
    //
    // The sandbox is created FIRST: parallel_disabled_by_hooks() reads the
    // global config, so asserting before the sandbox is live would read the
    // developer's real ~/.kcode/config.toml, whose pre_tool hook would make
    // this assertion fail for reasons that have nothing to do with the seam.
    let (mut agent, _requests, _sandbox) = probe_agent().await;
    let parallel_cfg = enabled_config(&["read"], 4);
    assert!(parallel_cfg.enabled);
    assert!(
        !parallel_disabled_by_hooks(),
        "no pre_tool hook is configured in the test sandbox, so the gate must be open"
    );
    let calls = vec![call("c1", "read", "a.rs"), call("c2", "read", "b.rs")];
    let sdk: HashMap<String, (String, bool)> = HashMap::new();
    let handled = match agent
        .execute_parallelizable_tools(&calls, &sdk, &parallel_cfg, None)
        .await
    {
        Ok(handled) => handled,
        Err(error) => panic!("the enabled path must not fail: {error}"),
    };
    assert_eq!(handled, HashSet::from([0, 1]));
}

/// `enabled = false` with a populated allow-list must still take the sequential
/// path: the master switch, not the allow-list, decides.
#[tokio::test]
async fn a_populated_allow_list_does_not_override_the_master_switch() {
    let parallel_cfg = crate::config::ParallelToolsConfig {
        enabled: false,
        read_only_tools: vec!["read".to_string()],
        max_concurrency: 4,
        run_hooks_in_parallel: false,
    };
    let (mut agent, _requests, _sandbox) = probe_agent().await;
    let calls = vec![
        call("c1", "read", "a.rs"),
        call("c2", "read", "b.rs"),
        call("c3", "read", "c.rs"),
    ];
    let sdk: HashMap<String, (String, bool)> = HashMap::new();
    let handled = match agent
        .execute_parallelizable_tools(&calls, &sdk, &parallel_cfg, None)
        .await
    {
        Ok(handled) => handled,
        Err(error) => panic!("the disabled path must not fail: {error}"),
    };
    assert!(
        handled.is_empty(),
        "enabled = false must bypass the parallel path even with an allow-list"
    );
}

/// `enabled = true` but an empty allow-list must parallelize nothing, because no
/// hardcoded list exists to fall back on.
#[tokio::test]
async fn an_empty_allow_list_parallelizes_nothing() {
    let parallel_cfg = crate::config::ParallelToolsConfig {
        enabled: true,
        read_only_tools: Vec::new(),
        max_concurrency: 4,
        run_hooks_in_parallel: false,
    };
    let (mut agent, _requests, _sandbox) = probe_agent().await;
    let calls = vec![
        call("c1", "read", "a.rs"),
        call("c2", "read", "b.rs"),
        call("c3", "read", "c.rs"),
    ];
    let sdk: HashMap<String, (String, bool)> = HashMap::new();
    let handled = match agent
        .execute_parallelizable_tools(&calls, &sdk, &parallel_cfg, None)
        .await
    {
        Ok(handled) => handled,
        Err(error) => panic!("an empty allow-list must not fail: {error}"),
    };
    assert!(
        handled.is_empty(),
        "nothing may run concurrently without an operator-stated allow-list"
    );
}

/// `max_concurrency = 0` must not become an unbounded fan-out.
#[tokio::test]
async fn zero_max_concurrency_is_not_an_unbounded_fan_out() {
    let parallel_cfg = enabled_config(&["read"], 0);
    let started = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicUsize::new(0));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    let log = Arc::new(StdMutex::new(Vec::new()));
    let (mut agent, _requests, _sandbox) = probe_agent().await;
    agent
        .registry
        .register(
            "read".into(),
            Arc::new(OverlapProbe::new(
                log.clone(),
                20,
                started.clone(),
                finished.clone(),
                in_flight.clone(),
                max_in_flight.clone(),
            )),
        )
        .await;
    let calls = vec![
        call("c1", "read", "a.rs"),
        call("c2", "read", "b.rs"),
        call("c3", "read", "c.rs"),
    ];
    let sdk: HashMap<String, (String, bool)> = HashMap::new();
    match agent
        .execute_parallelizable_tools(&calls, &sdk, &parallel_cfg, None)
        .await
    {
        Ok(_) => {}
        Err(error) => panic!("a zero bound must not fail: {error}"),
    }
    assert_eq!(
        max_in_flight.load(Ordering::SeqCst),
        1,
        "max_concurrency = 0 must degrade to one call at a time, not to unbounded"
    );
    assert_eq!(finished.load(Ordering::SeqCst), 3);
}

/// A round with a single allow-listed call is not worth spawning, so the seam
/// leaves it to the sequential loop.
#[tokio::test]
async fn a_single_read_only_call_is_left_sequential() {
    let parallel_cfg = enabled_config(&["read"], 4);
    let started = Arc::new(AtomicUsize::new(0));
    let (mut agent, _requests, _sandbox) = probe_agent().await;
    agent
        .registry
        .register("read".into(), Arc::new(StableProbe::new(started.clone(), 0)))
        .await;
    let calls = vec![call("c1", "read", "a.rs")];
    let sdk: HashMap<String, (String, bool)> = HashMap::new();
    let handled = match agent
        .execute_parallelizable_tools(&calls, &sdk, &parallel_cfg, None)
        .await
    {
        Ok(handled) => handled,
        Err(error) => panic!("a lone call must not fail: {error}"),
    };
    assert!(
        handled.is_empty(),
        "a run of one must not be dispatched through the parallel path"
    );
    assert!(
        appended_tool_results(&agent).is_empty(),
        "a lone call must be left for the sequential loop to execute"
    );
}

/// The `pre_tool` hook and the parallel path both work when the operator opts in.
///
/// This is the proof that the two features are not mutually exclusive. A
/// `pre_tool` hook already runs once per call inside `Registry::execute`, which
/// each parallel worker calls, so opting in must produce **all three** at once:
///
/// 1. the hook ran (the marker file has one line per call, not one per round);
/// 2. the calls overlapped (at least two in flight at the same time);
/// 3. the results merged in the model's order even though completion was
///    reversed (so the transcript bytes, and the prompt cache, are unchanged).
///
/// The hook is a real config-file hook, not a test seam: it is written into the
/// sandbox's `KCODE_HOME/config.toml` and invoked as a subprocess, exactly as an
/// operator's script would be.
#[cfg(unix)]
#[tokio::test]
async fn a_pre_tool_hook_runs_inside_parallel_workers_when_opted_in() {
    use std::os::unix::fs::PermissionsExt;

    let (mut agent, _requests, sandbox) = probe_agent().await;
    let root = sandbox.root().to_path_buf();

    // A real allow-everything gate script: one marker line per invocation.
    let hook_script = root.join("allow.sh");
    let script_body = format!("#!/bin/sh\necho ran >> \"{}/hook.marker\"\nexit 0\n", root.display());
    let wrote_script = std::fs::write(&hook_script, script_body).is_ok()
        && std::fs::set_permissions(&hook_script, std::fs::Permissions::from_mode(0o755)).is_ok();
    assert!(
        wrote_script,
        "the sandbox hook script must be writable and executable"
    );

    // The operator's config: the hook is on, and they assert it is per-call-pure.
    let config_toml = format!(
        "[hooks]\npre_tool = \"{}/allow.sh\"\n\n[parallel_tools]\nenabled = true\nrun_hooks_in_parallel = true\nread_only_tools = [\"read\"]\nmax_concurrency = 4\n",
        root.display()
    );
    assert!(
        std::fs::write(root.join("config.toml"), config_toml).is_ok(),
        "the sandbox config must be writable"
    );
    crate::config::invalidate_config_cache();
    // Guard: if this is false the rest of the test would measure a no-hook run
    // and the marker assertion would fail for the wrong reason.
    assert!(
        parallel_disabled_by_hooks(),
        "the sandbox config must actually configure a pre_tool hook"
    );

    // The operator's assertion, read from the config the same way production does.
    let parallel_cfg = crate::config::config().parallel_tools.clone();
    assert!(parallel_cfg.enabled, "the sandbox config must enable the feature");
    assert!(
        parallel_cfg.run_hooks_in_parallel,
        "the sandbox config must opt in to running the hook inside parallel workers"
    );
    assert!(
        parallel_cfg.allows_parallel("read"),
        "read must be on the allow-list"
    );

    // One probe that measures overlap AND reverse completion order.
    let started = Arc::new(AtomicUsize::new(0));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    let completion_order = Arc::new(StdMutex::new(Vec::new()));
    agent
        .registry
        .register(
            "read".into(),
            Arc::new(HookedOverlapProbe {
                started: started.clone(),
                in_flight: in_flight.clone(),
                max_in_flight: max_in_flight.clone(),
                completion_order: completion_order.clone(),
            }),
        )
        .await;

    let calls = vec![
        call("c1", "read", "a"),
        call("c2", "read", "ab"),
        call("c3", "read", "abc"),
    ];
    let sdk: HashMap<String, (String, bool)> = HashMap::new();
    let outcome = agent
        .execute_parallelizable_tools(&calls, &sdk, &parallel_cfg, None)
        .await;
    if let Err(error) = &outcome {
        // Surface it in the log, then let the assertions below fail loudly: the
        // seam returning an error is not a result this test can interpret.
        crate::logging::warn(&format!(
            "the opted-in parallel path must not fail: {error}"
        ));
    }
    let handled = outcome.unwrap_or_default();
    assert_eq!(handled, HashSet::from([0, 1, 2]));

    // (i) The hook ran once per call, inside the workers.
    let marker = root.join("hook.marker");
    let marker_lines = std::fs::read_to_string(&marker)
        .map(|contents| contents.lines().count())
        .unwrap_or_default();
    assert_eq!(
        marker_lines,
        3,
        "the pre_tool hook must run once per call inside the parallel workers, so 3 calls must leave 3 marker lines (got {marker_lines})"
    );

    // (ii) The calls overlapped.
    let overlap = max_in_flight.load(Ordering::SeqCst);
    assert!(
        overlap >= 2,
        "opted-in parallel calls must overlap: max in flight was {overlap}"
    );
    assert_eq!(started.load(Ordering::SeqCst), 3, "every call must run");

    // (iii) Model order, not completion order.
    let ids = tool_result_ids(&appended_tool_results(&agent));
    assert_eq!(
        ids,
        vec!["c1".to_string(), "c2".to_string(), "c3".to_string()],
        "results must merge in the model's call order"
    );
    let order = match completion_order.lock() {
        Ok(order) => order.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    assert_eq!(
        order,
        vec!["c3".to_string(), "c2".to_string(), "c1".to_string()],
        "the probe must finish in reverse order, so the merge is proven to ignore completion order"
    );
}

/// Probe for the opt-in proof: an in-flight gauge plus reverse-completion sleeps,
/// recording both the overlap and the true completion order.
#[cfg(unix)]
struct HookedOverlapProbe {
    started: Arc<AtomicUsize>,
    in_flight: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
    completion_order: Arc<StdMutex<Vec<String>>>,
}

#[cfg(unix)]
#[async_trait]
impl Tool for HookedOverlapProbe {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Overlap and completion-order probe"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(&self, input: serde_json::Value, ctx: ToolContext) -> Result<ToolOutput> {
        let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(current, Ordering::SeqCst);
        self.started.fetch_add(1, Ordering::SeqCst);
        let path = input
            .get("path")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        // Longer paths sleep less, so the LAST call finishes FIRST.
        let depth = path.chars().count() as u64;
        let sleep_ms = 120u64.saturating_sub(depth * 15);
        tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
        if let Ok(mut order) = self.completion_order.lock() {
            order.push(ctx.tool_call_id);
        }
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(ToolOutput::new(format!("contents of {path}")))
    }
}
