//! Harness-side parallel execution of independent read-only tool calls.
//!
//! When one model response carries several tool calls, kcode runs them one at a
//! time, in the model's order. That is the safe default: a later call can depend
//! on an earlier one's result, and two writes to the same file must not race.
//!
//! But a response very often carries several calls that are *independent*: two
//! reads, a grep and a glob, three lookups of unrelated symbols. Anthropic's
//! parallel-tool-use documentation states the rule directly — "independent,
//! read-only operations are usually safe to run in parallel for lower latency" —
//! and both Qwen Code (PR #2434) and MyCodeAgent ship exactly this change. So
//! does this module, gated behind `[parallel_tools]` and **disabled by
//! default**.
//!
//! Three properties make it safe, and each is enforced here rather than
//! assumed:
//!
//! 1. **Independence is proven by a static allow-list.** Only calls whose tool
//!    name is on `read_only_tools` may run concurrently. Everything else — every
//!    write, every `bash`, every MCP tool the operator has not vouched for —
//!    stays sequential. No effect system is attempted: none of the shipping
//!    harnesses has one, and a name-based allow-list is the whole proof that is
//!    actually deployed.
//! 2. **Order is preserved by construction.** Results are collected with
//!    `futures::future::join_all`, which returns them "in the same order as they
//!    were provided" — never from a `JoinSet` join, which returns completion
//!    order and panics on the first `JoinError`. The merge step then appends
//!    them in that order, so the conversation bytes are identical to a
//!    sequential run of the same round.
//! 3. **Failure is contained.** A call that errors, or whose spawned task
//!    panics, becomes that call's error result in its own slot. The round still
//!    produces one result per call, which is what the provider API requires and
//!    what lets the model reissue a failed call.
//!
//! The batch is partitioned into **maximal runs** of allow-listed calls, cut at
//! the first call that is not on the list (the MyCodeAgent rule). A run of one
//! call is never spawned: the sequential path already runs it without a task, so
//! spawning would only add overhead.
//!
//! ## Seam
//!
//! Both tool loops (the blocking one in `turn_loops.rs` and the streaming one in
//! `turn_streaming_mpsc.rs`) call [`Agent::execute_parallelizable_tools`] before
//! their per-call loop. It runs the parallelizable runs, appends their results to
//! the conversation in the model's original order, and returns the indices it
//! handled so the loop can skip them. Calls it did not handle are executed
//! sequentially, in order, by the unchanged loop body.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use futures::future::join_all;
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::tool::ToolOutput;
use super::*;

/// A maximal run of consecutive parallelizable tool calls.
///
/// Stored as `[start, start + len)` indices into the round's call vector rather
/// than as cloned calls, so the caller keeps iterating the original vector in
/// the original order and nothing is reordered by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ParallelRun {
    pub(super) start: usize,
    pub(super) len: usize,
}

impl ParallelRun {
    fn end(self) -> usize {
        self.start + self.len
    }

    /// True when the run is long enough to be worth a task per call. A single
    /// call is already sequential, so spawning it changes nothing but cost.
    fn is_worth_spawning(self) -> bool {
        self.len >= 2
    }

    /// Slice of `calls` this run covers.
    pub(super) fn calls<'a>(self, calls: &'a [ToolCall]) -> &'a [ToolCall] {
        &calls[self.start..self.end()]
    }
}

/// Partition `calls` into maximal runs of parallelizable calls.
///
/// The run is cut at the first call the predicate rejects, so a write in the
/// middle of a batch ends the run and everything after it stays sequential. This
/// is the conservative rule: a later call may depend on an earlier one's result,
/// and this way no call ever starts before the call the model listed before it
/// has finished, unless the model itself said they were independent by putting
/// them in one response and the allow-list vouches for every one of them.
///
/// Runs of length one are dropped: they gain nothing from a task.
pub(super) fn partition_parallel_runs(
    calls: &[ToolCall],
    is_parallelizable: impl Fn(&ToolCall) -> bool,
) -> Vec<ParallelRun> {
    let mut runs: Vec<ParallelRun> = Vec::new();
    let mut index = 0;
    while index < calls.len() {
        if !is_parallelizable(&calls[index]) {
            index += 1;
            continue;
        }
        let start = index;
        while index < calls.len() && is_parallelizable(&calls[index]) {
            index += 1;
        }
        let run = ParallelRun {
            start,
            len: index - start,
        };
        if run.is_worth_spawning() {
            runs.push(run);
        }
    }
    runs
}

/// What one parallel run produced.
struct ParallelRunOutcome {
    /// One entry per call in the run, **in the run's original order**. An `Err`
    /// is that call's failure, captured in its own slot exactly as the
    /// sequential path would have recorded it.
    results: Vec<Result<ToolOutput>>,
    /// Per-call wall clock in milliseconds, for the same duration metadata the
    /// sequential path records.
    elapsed_ms: Vec<u64>,
}

/// Resolve the concurrency bound for a run of `run_len` calls.
///
/// `max_concurrency` of 0 is treated as 1 rather than as "unlimited": a config
/// that cannot be parsed into a positive number must not become an unbounded
/// fan-out. Values above the run's length are clamped, so a semaphore is never
/// created with permits no call can use.
fn effective_permits(max_concurrency: usize, run_len: usize) -> usize {
    max_concurrency.max(1).min(run_len.max(1))
}

/// Execute one run's calls concurrently and collect the results positionally.
///
/// The semaphore bounds how many calls are in flight at once. `join_all` waits
/// for **every** future and returns their outputs in the order the futures were
/// provided, which is the run's original order, so nothing is lost and nothing
/// is reordered no matter which call finishes first.
///
/// Each call goes through [`super::turn_streaming_mpsc::execute_with_background`],
/// the same helper the sequential path uses, so background handoff, the
/// foreground budget, and the resulting message text are identical between the
/// two paths.
async fn execute_parallel_run(
    registry: Registry,
    run: ParallelRun,
    calls: &[ToolCall],
    max_concurrency: usize,
    make_ctx: impl Fn(&ToolCall) -> ToolContext,
) -> ParallelRunOutcome {
    let run_calls = run.calls(calls);
    let permits = effective_permits(max_concurrency, run_calls.len());
    let semaphore = Arc::new(Semaphore::new(permits));

    let futures = run_calls.iter().map(|tc| {
        let semaphore = Arc::clone(&semaphore);
        let registry = registry.clone();
        let name = tc.name.clone();
        let input: Value = tc.input.clone();
        let session_id = make_ctx(tc).session_id.clone();
        let tool_call_id = tc.id.clone();
        let ctx = make_ctx(tc);
        async move {
            // A closed semaphore can only happen if every permit was dropped,
            // which cannot occur while this future holds one; treat it as a
            // failure of this call rather than unwrapping.
            let permit = match semaphore.acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    return (
                        Err(anyhow::anyhow!(
                            "Parallel tool dispatch lost its concurrency permit"
                        )),
                        0,
                    );
                }
            };
            // Show the call as running for as long as it is in flight, matching
            // the sequential path's running-tool registration.
            let _running = crate::running_tool_registry::register(&session_id, &tool_call_id, &name);
            let started = Instant::now();
            let result = super::turn_streaming_mpsc::execute_with_background(registry, name, input, ctx)
                .await;
            let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            drop(permit);
            (result, elapsed_ms)
        }
    });

    let collected: Vec<(Result<ToolOutput>, u64)> = join_all(futures).await;
    let mut results = Vec::with_capacity(collected.len());
    let mut elapsed_ms = Vec::with_capacity(collected.len());
    for (result, elapsed) in collected {
        results.push(result);
        elapsed_ms.push(elapsed);
    }

    ParallelRunOutcome {
        results,
        elapsed_ms,
    }
}

/// The content blocks one call's result contributes to the conversation.
///
/// This is the *same* mapping the sequential loop applies, built from the same
/// helpers, so a parallel run and a sequential run of the same round append
/// byte-identical blocks. `Ok` goes through [`cap_tool_output_for_history`] and
/// [`tool_output_to_content_blocks`]; `Err` becomes an error tool result
/// carrying `format!("Error: {}", error)`, exactly as the sequential loop does.
fn tool_result_blocks(
    tool_use_id: String,
    tool_name: &str,
    result: Result<ToolOutput>,
) -> Vec<ContentBlock> {
    match result {
        Ok(output) => {
            let output = cap_tool_output_for_history(tool_name, output);
            tool_output_to_content_blocks(tool_use_id, output)
        }
        Err(error) => vec![ContentBlock::ToolResult {
            tool_use_id,
            content: format!("Error: {error}"),
            is_error: Some(true),
        }],
    }
}

/// A parallelizable call, and why it qualifies. Kept next to the partition so
/// the eligibility rule lives in one place.
pub(super) struct ParallelEligibility<'a> {
    pub(super) cfg: &'a crate::config::ParallelToolsConfig,
    /// Tool results the provider already produced. A call with a result here is
    /// executed by nobody locally, so it must not be dispatched.
    pub(super) sdk_tool_results: &'a HashMap<String, (String, bool)>,
}

impl ParallelEligibility<'_> {
    /// True when this call may run concurrently with its neighbours.
    ///
    /// A call that fails `validation_error()` is excluded: the sequential path
    /// answers it with a schema-correction message instead of running it, and
    /// dispatching it would send null arguments to a real tool.
    pub(super) fn allows(&self, tc: &ToolCall) -> bool {
        tc.validation_error().is_none()
            && !self.sdk_tool_results.contains_key(&tc.id)
            && self.cfg.allows_parallel(&tc.name)
    }
}

/// Whether a `pre_tool` gate or input transformer is configured.
///
/// Both run inside `Registry::execute`, between dispatch and the tool body, and
/// both can rewrite a call's input. Running them once per call *before*
/// dispatching (the Qwen Code PR #2434 approach) would let the parallel path
/// start with inputs the transformer has not seen yet, so a transformer that
/// adds a path could hand one call a path another call already used. Rather
/// than accept that, the parallel path steps aside entirely and the round runs
/// on the unchanged sequential path, which calls them per call in order.
pub(super) fn parallel_disabled_by_hooks() -> bool {
    crate::hooks::hook_configured("pre_tool")
}

impl Agent {
    /// Execute the parallelizable runs of `tool_calls`, appending each result to
    /// the conversation in the model's original tool_call order.
    ///
    /// `cfg` is passed in rather than read from the global config so the
    /// enabled/disabled and allow-list decisions are made once, by the caller,
    /// and are directly testable. The two loop sites pass
    /// `&crate::config::config().parallel_tools`.
    ///
    /// Returns the indices this seam handled; the caller skips those in its own
    /// per-call loop. When the feature is off, or nothing qualifies, this does
    /// nothing and returns an empty set, which leaves the caller's sequential
    /// loop executing every call exactly as it did before.
    ///
    /// The parallel path is skipped when a `pre_tool` hook is configured, see
    /// [`parallel_disabled_by_hooks`].
    pub(super) async fn execute_parallelizable_tools(
        &mut self,
        tool_calls: &[ToolCall],
        sdk_tool_results: &HashMap<String, (String, bool)>,
        cfg: &crate::config::ParallelToolsConfig,
        event_tx: Option<&mpsc::UnboundedSender<ServerEvent>>,
    ) -> Result<HashSet<usize>> {
        let handled: HashSet<usize> = HashSet::new();
        if !cfg.enabled || parallel_disabled_by_hooks() {
            return Ok(handled);
        }

        let eligibility = ParallelEligibility {
            cfg,
            sdk_tool_results,
        };
        let runs = partition_parallel_runs(tool_calls, |tc| eligibility.allows(tc));
        if runs.is_empty() {
            return Ok(handled);
        }

        let message_id = self.session.id.clone();
        let session_id = self.session.id.clone();
        let working_dir = self.working_dir().map(PathBuf::from);
        let stdin_request_tx = self.stdin_request_tx.clone();
        let graceful_shutdown = self.graceful_shutdown.clone();
        let registry = self.registry.clone();
        let max_concurrency = cfg.max_concurrency;

        let mut handled = handled;
        for run in runs {
            let make_ctx = |tc: &ToolCall| ToolContext {
                session_id: session_id.clone(),
                message_id: message_id.clone(),
                tool_call_id: tc.id.clone(),
                working_dir: working_dir.clone(),
                stdin_request_tx: stdin_request_tx.clone(),
                graceful_shutdown_signal: Some(graceful_shutdown.clone()),
                execution_mode: ToolExecutionMode::AgentTurn,
            };
            let outcome =
                execute_parallel_run(registry.clone(), run, tool_calls, max_concurrency, make_ctx)
                    .await;
            for (offset, (tc, result)) in tool_calls[run.start..run.end()]
                .iter()
                .zip(outcome.results)
                .enumerate()
            {
                let index = run.start + offset;
                let elapsed = outcome.elapsed_ms[offset];
                handled.insert(index);

                // Telemetry matches the sequential loop: one success or
                // one failure per call, never per batch.
                match &result {
                    Ok(_) => crate::telemetry::record_tool_call(),
                    Err(_) => crate::telemetry::record_tool_failure(),
                }
                self.unlock_tools_if_needed(&tc.name);
                let blocks = tool_result_blocks(tc.id.clone(), &tc.name, result);
                // One message per call, exactly as the sequential path emits
                // one user message per tool call.
                self.add_message_with_duration(Role::User, blocks, Some(elapsed));

                let status = if self.session.messages.last().is_some_and(|message| {
                    matches!(
                        message.content.last(),
                        Some(ContentBlock::ToolResult { is_error: Some(true), .. })
                    )
                }) {
                    ToolStatus::Error
                } else {
                    ToolStatus::Completed
                };
                Bus::global().publish(BusEvent::ToolUpdated(ToolEvent {
                    session_id: session_id.clone(),
                    message_id: message_id.clone(),
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.name.clone(),
                    status,
                    intent: tc.intent.clone(),
                    title: None,
                }));
                if let Some(event_tx) = event_tx {
                    let (output, error) = self.session.messages.last().map_or_else(
                        || (String::new(), None),
                        |message| match message.content.last() {
                            Some(ContentBlock::ToolResult {
                                content, is_error, ..
                            }) => (
                                content.clone(),
                                is_error.filter(|flag| *flag).map(|_| content.clone()),
                            ),
                            _ => (String::new(), None),
                        },
                    );
                    let _ = event_tx.send(ServerEvent::ToolDone {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        output,
                        error,
                    });
                }
            }
        }

        Ok(handled)
    }
}
