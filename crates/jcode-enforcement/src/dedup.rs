//! T0.2 — Anti-loop middleware: SHA-256 fingerprint dedup + action quotas.
//!
//! Blocks the three Flash-model loop modes:
//!   1. immediate repetition (A -> A -> A),
//!   2. ping-pong oscillation inside a sliding window,
//!   3. endless audit streaks with no workspace mutation.
//!
//! The rejection string is returned to the model as the tool result — the
//! underlying tool never executes, so blocked turns cost zero I/O and zero
//! tokens of real output.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub name: String,
    pub payload: Value,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DedupAction {
    Execute,
    RejectLoop { reason: String },
}

pub struct ToolDedupEngine {
    /// Number of *prior* identical consecutive calls tolerated before blocking.
    /// 2 => the third identical consecutive call is rejected.
    max_consecutive_repeats: usize,
    /// Sliding window for oscillation detection (A -> B -> A -> B ...).
    history_window: usize,
    /// Max identical calls of one fingerprint inside the history window, AND
    /// max audit-tool calls inside the audit time window (v1.1 semantics).
    max_per_window: usize,
    /// Audit quota is TIME-based (v1.1 fix: the 4th constructor parameter was a
    /// streak counter that ignored the documented 6-second window).
    audit_window: Duration,
    /// Tools considered pure observation (counted against the audit quota).
    audit_tools: std::collections::HashSet<&'static str>,
    call_history: VecDeque<String>,
    call_counts: HashMap<String, usize>,
    /// Timestamps of recent audit-tool calls, pruned against `audit_window`.
    audit_calls: VecDeque<Instant>,
    last_mutation: Instant,
}

impl ToolDedupEngine {
    /// NOTE: serde_json::Value serializes map keys in sorted (BTreeMap) order
    /// unless the non-default `preserve_order` feature is enabled. Keep that
    /// feature OFF so fingerprints are canonical regardless of wire ordering.
    pub fn fingerprint(call: &ToolCall) -> String {
        let mut hasher = Sha256::new();
        hasher.update(call.name.as_bytes());
        hasher.update(b"\x00");
        hasher.update(serde_json::to_string(&call.payload).unwrap_or_default().as_bytes());
        format!("{:x}", hasher.finalize())
    }

    pub fn new(
        max_consecutive_repeats: usize,
        history_window: usize,
        max_per_window: usize,
        audit_window_secs: u64,
    ) -> Self {
        Self {
            max_consecutive_repeats,
            history_window,
            max_per_window,
            audit_window: Duration::from_secs(audit_window_secs),
            audit_tools: ["agentgrep_search", "graphicle_query_graph", "read_file", "graphicle_get_neighbors"]
                .into_iter()
                .collect(),
            call_history: VecDeque::with_capacity(history_window),
            call_counts: HashMap::new(),
            audit_calls: VecDeque::new(),
            last_mutation: Instant::now(),
        }
    }

    pub fn inspect(&mut self, call: &ToolCall) -> DedupAction {
        let fp = Self::fingerprint(call);

        // 1) Immediate repetition: count trailing identical fingerprints.
        let consecutive = self.call_history.iter().rev().take_while(|f| *f == &fp).count();
        if consecutive >= self.max_consecutive_repeats {
            return DedupAction::RejectLoop {
                reason: format!(
                    "Loop protection: tool `{}` already invoked {}x consecutively with no state \
                     mutation. Synthesize your findings or propose an edit now.",
                    call.name,
                    consecutive + 1
                ),
            };
        }

        // 2) Oscillation inside the sliding window.
        if self.call_counts.get(&fp).copied().unwrap_or(0) >= self.max_per_window {
            return DedupAction::RejectLoop {
                reason: format!(
                    "Circular pattern: `{}` repeated {} times in the recent window without \
                     progressing the task. Terminate this exploration branch.",
                    call.name,
                    self.max_per_window
                ),
            };
        }

        // 3) Audit quota, time-window based: force periodic mutation or synthesis.
        if self.audit_tools.contains(call.name.as_str()) {
            self.prune_audit_window();
            if self.audit_calls.len() >= self.max_per_window {
                return DedupAction::RejectLoop {
                    reason: format!(
                        "Audit quota exceeded: {} observation calls within {}s and no workspace \
                         mutation. Either apply an edit or emit a structured summary.",
                        self.audit_calls.len(),
                        self.audit_window.as_secs()
                    ),
                };
            }
            self.audit_calls.push_back(Instant::now());
        }

        if self.call_history.len() >= self.history_window {
            if let Some(evicted) = self.call_history.pop_front() {
                if let Some(cnt) = self.call_counts.get_mut(&evicted) {
                    *cnt = cnt.saturating_sub(1);
                    if *cnt == 0 {
                        self.call_counts.remove(&evicted);
                    }
                }
            }
        }
        self.call_history.push_back(fp.clone());
        *self.call_counts.entry(fp).or_insert(0) += 1;

        DedupAction::Execute
    }

    /// Call on every successful mutating tool (write/apply_patch/transaction commit).
    pub fn notify_workspace_mutation(&mut self) {
        self.call_history.clear();
        self.call_counts.clear();
        self.audit_calls.clear();
        self.last_mutation = Instant::now();
    }

    fn prune_audit_window(&mut self) {
        while let Some(front) = self.audit_calls.front() {
            if front.elapsed() >= self.audit_window {
                self.audit_calls.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn seconds_since_mutation(&self) -> u64 {
        self.last_mutation.elapsed().as_secs()
    }
}

/// Closing tool contract: every read/search tool result MUST terminate in an
/// explicit status so the model never sees an ambiguous empty response.
pub fn close_tool_result(mut result: Value, complete: bool, instruction: &str) -> Value {
    if let Some(obj) = result.as_object_mut() {
        // v1.1: PRESERVE an explicit status ("PASSED"/"REJECTED" from the fast
        // gate) so callers can branch on it; only fill in the closing contract
        // status when the result does not carry one.
        obj.entry("status".to_string())
            .or_insert_with(|| if complete { "COMPLETE".into() } else { "PARTIAL".into() });
        obj.insert("instruction".into(), instruction.into());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn search(q: &str) -> ToolCall {
        ToolCall { name: "agentgrep_search".into(), payload: json!({ "query": q }) }
    }

    #[test]
    fn blocks_third_identical_consecutive_call() {
        let mut eng = ToolDedupEngine::new(2, 16, 4, 6);
        assert_eq!(eng.inspect(&search("foo")), DedupAction::Execute);
        assert_eq!(eng.inspect(&search("foo")), DedupAction::Execute);
        match eng.inspect(&search("foo")) {
            DedupAction::RejectLoop { .. } => {}
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[test]
    fn mutation_resets_quota() {
        let mut eng = ToolDedupEngine::new(2, 16, 4, 2);
        for _ in 0..2 {
            let _ = eng.inspect(&search("foo"));
        }
        eng.notify_workspace_mutation();
        assert_eq!(eng.inspect(&search("foo")), DedupAction::Execute);
    }

    #[test]
    fn fingerprints_are_key_order_independent() {
        let a = ToolCall { name: "t".into(), payload: json!({ "query": "x", "depth": 1 }) };
        let b = ToolCall { name: "t".into(), payload: json!({ "depth": 1, "query": "x" }) };
        assert_eq!(ToolDedupEngine::fingerprint(&a), ToolDedupEngine::fingerprint(&b));
    }

    #[test]
    fn closing_contract_marks_empty_results_complete() {
        let out = close_tool_result(json!({ "matches": [] }), true, "No hits. Conclude the step.");
        assert_eq!(out["status"], "COMPLETE");
    }

    #[test]
    fn closing_contract_preserves_explicit_fast_gate_status() {
        // v1.1: the grunt loop branches on outcome["status"] == "PASSED";
        // close_tool_result must not overwrite it.
        let out = close_tool_result(json!({ "status": "PASSED" }), true, "ok");
        assert_eq!(out["status"], "PASSED");
        let out = close_tool_result(json!({ "status": "REJECTED", "error": "x" }), false, "fix");
        assert_eq!(out["status"], "REJECTED");
    }

    #[test]
    fn audit_quota_is_time_windowed_and_reset_by_mutation() {
        // 4 distinct audit calls within the 6s window are allowed...
        let mut eng = ToolDedupEngine::new(2, 16, 4, 6);
        for i in 0..4 {
            assert_eq!(eng.inspect(&search(&format!("q{i}"))), DedupAction::Execute);
        }
        // ...the 5th distinct one hits the quota (layers 1/2 see fresh fingerprints).
        match eng.inspect(&search("q4")) {
            DedupAction::RejectLoop { reason } => assert!(reason.contains("Audit quota")),
            other => panic!("expected audit quota rejection, got {other:?}"),
        }
        // A successful mutation resets the quota.
        eng.notify_workspace_mutation();
        assert_eq!(eng.inspect(&search("q4")), DedupAction::Execute);
    }
}
