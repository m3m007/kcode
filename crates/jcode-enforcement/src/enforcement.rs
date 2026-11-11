//! T0.1 — Provider tool-enforcement adapter (OpenAI-compatible only).
//!
//! Maps harness tool policy onto native engine parameters (`tool_choice`,
//! `parallel_tool_calls`), probes per-provider capability at session start,
//! and provides the harness-level interception loop that stays correct even
//! when a provider silently ignores `tool_choice`.
//!
//! Provider notes (verified 2026-11):
//! - OpenAI baseline: `tool_choice` = "none" | "auto" | "required" |
//!   {"type":"function","function":{"name":..}}; `parallel_tool_calls` default true.
//! - DeepSeek (`deepseek-flash`): supports `required`/named — but returns HTTP 400
//!   if thinking mode is enabled. Architect turns keep thinking on => policy Auto.
//! - StepFun (`step-3.7-flash`) / MiMo hosted: `required` support is not
//!   documented => always probe; fall back to interception.
//! - Self-hosted vLLM >= 0.8.3: `required` supported with `--enable-auto-tool-choice
//!   --tool-call-parser mimo` (for MiMo). llama.cpp: no engine-level required —
//!   use GBNF/`json_schema` response constraints instead.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Canonical model ids used by the swarm (provider-namespace dependent).
pub mod models {
    pub const ARCHITECT_DEEPSEEK: &str = "deepseek-flash"; // DeepSeek-V4.1-Flash
    pub const GRUNT_STEPFUN: &str = "step-3.7-flash";
    pub const GRUNT_MIMO: &str = "mimo-v2.6-flash"; // hosted; self-hosted: vLLM tool-call-parser mimo
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolPolicy {
    /// Model decides (summary phase, architect turns).
    Auto,
    /// Engine must emit a tool-call payload before returning.
    Required,
    /// Engine must call this exact tool (decode-time constraint where supported).
    Named(String),
    /// Tools advertised but never forced (debug).
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCaps {
    /// Engine accepts tool_choice = "required" / named without a 400.
    pub supports_required: bool,
    /// Engine accepts parallel_tool_calls = false.
    pub supports_parallel_flag: bool,
}

/// Harness phases gate how hard we enforce tool usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPhase {
    Exploration,
    Modification,
    Summary,
}

impl AgentPhase {
    pub fn requires_tool_call(self) -> bool {
        matches!(self, AgentPhase::Exploration | AgentPhase::Modification)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// HTTP 400 — typically an unsupported parameter (e.g. tool_choice in DeepSeek thinking mode).
    BadRequest,
    Other(String),
}

/// Minimal transport boundary so this module is unit-testable without HTTP.
pub trait LlmTransport {
    fn complete(
        &self,
        model: &str,
        body: Value,
    ) -> impl std::future::Future<Output = Result<Value, ProviderError>> + Send;
}

/// Build an OpenAI-compatible chat-completions body with engine-level tool forcing.
///
/// `parallel_tool_calls: false` keeps grunt turns deterministic: one tool call per
/// turn, executed and verified before the next. Providers that do not document the
/// flag simply ignore it; send it unconditionally when `caps.supports_parallel_flag`.
pub fn build_request(
    model: &str,
    messages: Vec<Value>,
    tools: &[Value],
    policy: ToolPolicy,
    caps: ProviderCaps,
    temperature: f32,
) -> Value {
    let tool_choice = match (&policy, caps.supports_required) {
        (ToolPolicy::None, _) => json!("none"),
        (ToolPolicy::Auto, _) => json!("auto"),
        (ToolPolicy::Required, true) => json!("required"),
        (ToolPolicy::Required, false) => json!("auto"), // degrade; interception loop covers us
        (ToolPolicy::Named(name), true) => json!({
            "type": "function",
            "function": { "name": name }
        }),
        (ToolPolicy::Named(_), false) => json!("auto"),
    };

    let mut body = json!({
        "model": model,
        "messages": messages,
        "tools": tools,
        "tool_choice": tool_choice,
        "temperature": temperature,
    });

    if caps.supports_parallel_flag {
        body["parallel_tool_calls"] = json!(false);
    }
    body
}

/// Startup probe with TWO retries on transient (non-400) errors (Risk R1):
/// a single network blip must not silently degrade the session to Auto.
/// A 400 is authoritative (DeepSeek thinking mode) and degrades immediately.
pub async fn probe_required_support(transport: &impl LlmTransport, model: &str) -> ProviderCaps {
    let tools = [json!({
        "type": "function",
        "function": {
            "name": "noop_probe",
            "description": "Capability probe. Do not use.",
            "parameters": { "type": "object", "properties": {} }
        }
    })];
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "ping" }],
        "tools": tools,
        "tool_choice": "required",
        "max_tokens": 16,
    });

    for _attempt in 0..=2 {
        match transport.complete(model, body.clone()).await {
            Ok(_) => {
                return ProviderCaps { supports_required: true, supports_parallel_flag: true };
            }
            Err(ProviderError::BadRequest) => {
                return ProviderCaps { supports_required: false, supports_parallel_flag: true };
            }
            Err(ProviderError::Other(_)) => continue,
        }
    }
    ProviderCaps { supports_required: false, supports_parallel_flag: false }
}

#[derive(Debug, Clone)]
pub struct ToolInvocation {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone)]
pub enum ModelTurn {
    ToolCalls(Vec<ToolInvocation>),
    Text(String),
}

/// Extract the first assistant turn from an OpenAI-compatible response.
pub fn parse_turn(response: &Value) -> ModelTurn {
    let msg = &response["choices"][0]["message"];
    if let Some(calls) = msg["tool_calls"].as_array() {
        let parsed: Vec<ToolInvocation> = calls
            .iter()
            .filter_map(|c| {
                Some(ToolInvocation {
                    id: c["id"].as_str().unwrap_or_default().to_string(),
                    name: c["function"]["name"].as_str()?.to_string(),
                    arguments: match c["function"]["arguments"] {
                        Value::String(ref s) => serde_json::from_str(s)
                            .unwrap_or_else(|_| json!({ "_raw": s })),
                        ref v => v.clone(),
                    },
                })
            })
            .collect();
        if !parsed.is_empty() {
            return ModelTurn::ToolCalls(parsed);
        }
    }
    ModelTurn::Text(
        msg["content"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    )
}

const NO_TOOL_CORRECTION: &str = "Protocol Error: no tool call detected. You are in execution \
mode: invoke a search or inspection tool (agentgrep_search, graphicle_query_graph, \
read_file) before proposing any change. Do not output plans or commentary.";

/// Harness-level interception loop. Runs AFTER engine-level forcing as the
/// safety net for providers that ignore or reject `tool_choice`. v1.1: the
/// failed text-only turns and synthetic `NO_TOOL_CORRECTION` messages are
/// TRUNCATED from `messages` once a valid tool call is obtained (or on any
/// exit), so correction churn never pollutes the model's context window.
pub async fn run_turn_with_enforcement(
    transport: &impl LlmTransport,
    model: &str,
    messages: &mut Vec<Value>,
    tools: &[Value],
    caps: ProviderCaps,
    phase: AgentPhase,
    max_retries: usize,
) -> Result<ModelTurn, anyhow::Error> {
    let mut engine_policy = if phase.requires_tool_call() { ToolPolicy::Required } else { ToolPolicy::Auto };

    // DeepSeek thinking mode rejects required/named with 400 — degrade up front.
    if model == models::ARCHITECT_DEEPSEEK && engine_policy == ToolPolicy::Required {
        engine_policy = ToolPolicy::Auto;
    }

    let checkpoint_len = messages.len();

    for attempt in 0..=max_retries {
        let body = build_request(model, messages.clone(), tools, engine_policy.clone(), caps, 0.1);
        let response = match transport.complete(model, body).await {
            Ok(r) => r,
            Err(ProviderError::BadRequest) if attempt == 0 && engine_policy == ToolPolicy::Required => {
                // Provider lied in probe or config drift: degrade once, continue.
                engine_policy = ToolPolicy::Auto;
                continue;
            }
            Err(e) => {
                messages.truncate(checkpoint_len);
                return Err(anyhow::anyhow!("provider error: {e:?}"));
            }
        };

        match parse_turn(&response) {
            ModelTurn::ToolCalls(calls) if !calls.is_empty() => {
                // Pop any intermediate failed text turns + NO_TOOL_CORRECTION prompts.
                messages.truncate(checkpoint_len);
                messages.push(json!({
                    "role": "assistant",
                    "tool_calls": response["choices"][0]["message"]["tool_calls"]
                }));
                return Ok(ModelTurn::ToolCalls(calls));
            }
            // Summary phase (or architect free-form turn): text is legitimate.
            turn if !phase.requires_tool_call() => {
                messages.truncate(checkpoint_len);
                messages.push(response["choices"][0]["message"].clone());
                return Ok(turn);
            }
            // Forced phase: text or an empty tool_calls array both count as a
            // protocol violation -> intercept and correct (never user-visible).
            turn => {
                if attempt == max_retries {
                    messages.truncate(checkpoint_len);
                    anyhow::bail!("model refused to emit a tool call after {max_retries} corrections");
                }
                if let ModelTurn::Text(text) = turn {
                    messages.push(json!({ "role": "assistant", "content": text }));
                }
                messages.push(json!({ "role": "user", "content": NO_TOOL_CORRECTION }));
            }
        }
    }
    unreachable!("retry loop returns inside the body")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockTransport;

    impl LlmTransport for MockTransport {
        async fn complete(&self, _model: &str, body: Value) -> Result<Value, ProviderError> {
            if body["tool_choice"] == json!("required") {
                // Simulate a provider that accepts required and returns a tool call.
                return Ok(json!({
                    "choices": [{ "message": { "tool_calls": [{
                        "id": "1", "type": "function",
                        "function": { "name": "agentgrep_search",
                                      "arguments": "{\"query\":\"SessionTokenTracker\"}" } }]
                    }}]
                }));
            }
            Ok(json!({ "choices": [{ "message": { "content": "I will plan everything now..." } }] }))
        }
    }

    #[test]
    fn required_is_degraded_when_caps_say_no() {
        let body = build_request(
            models::GRUNT_STEPFUN,
            vec![json!({"role":"user","content":"x"})],
            &[],
            ToolPolicy::Required,
            ProviderCaps { supports_required: false, supports_parallel_flag: true },
            0.1,
        );
        assert_eq!(body["tool_choice"], json!("auto"));
        assert_eq!(body["parallel_tool_calls"], json!(false));
    }

    #[tokio::test]
    async fn interception_loop_corrects_text_only_turns() {
        let mut messages = vec![json!({ "role": "user", "content": "find SessionTokenTracker" })];
        let caps = ProviderCaps { supports_required: true, supports_parallel_flag: true };
        let turn = run_turn_with_enforcement(
            &MockTransport, models::GRUNT_STEPFUN, &mut messages, &[], caps,
            AgentPhase::Exploration, 3,
        )
        .await
        .expect("first forced request returns a tool call");
        match turn {
            ModelTurn::ToolCalls(calls) => assert_eq!(calls[0].name, "agentgrep_search"),
            other => panic!("expected tool calls, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn architect_thinking_mode_never_sends_required() {
        struct AssertTransport;
        impl LlmTransport for AssertTransport {
            async fn complete(&self, _m: &str, body: Value) -> Result<Value, ProviderError> {
                assert_ne!(body["tool_choice"], json!("required"), "DeepSeek thinking mode 400s on required");
                Ok(json!({ "choices": [{ "message": { "content": "plan" } }] }))
            }
        }
        let mut messages = vec![json!({ "role": "user", "content": "decompose task" })];
        let caps = ProviderCaps { supports_required: true, supports_parallel_flag: true };
        let _ = run_turn_with_enforcement(
            &AssertTransport, models::ARCHITECT_DEEPSEEK, &mut messages, &[],
            caps, AgentPhase::Exploration, 1,
        )
        .await;
    }

    #[tokio::test]
    async fn probe_retries_transient_errors_before_degrading() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Flaky(AtomicUsize);
        impl LlmTransport for Flaky {
            async fn complete(&self, _m: &str, _body: Value) -> Result<Value, ProviderError> {
                if self.0.fetch_sub(1, Ordering::SeqCst) > 1 {
                    return Err(ProviderError::Other("network blip".into()));
                }
                Ok(json!({ "choices": [{ "message": { "tool_calls": [] } }] }))
            }
        }
        let caps = probe_required_support(&Flaky(AtomicUsize::new(2)), models::GRUNT_MIMO).await;
        assert!(caps.supports_required, "two transient failures must not degrade the session");

        // A 400 is authoritative: degrade on the first response.
        struct Always400;
        impl LlmTransport for Always400 {
            async fn complete(&self, _m: &str, _body: Value) -> Result<Value, ProviderError> {
                Err(ProviderError::BadRequest)
            }
        }
        let caps = probe_required_support(&Always400, models::GRUNT_STEPFUN).await;
        assert!(!caps.supports_required);
        assert!(caps.supports_parallel_flag);
    }

    #[tokio::test]
    async fn corrections_leave_no_residue_in_context() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct TextOnceThenTool(AtomicUsize);
        impl LlmTransport for TextOnceThenTool {
            async fn complete(&self, _m: &str, body: Value) -> Result<Value, ProviderError> {
                if body["tool_choice"] == json!("required")
                    && self.0.fetch_sub(1, Ordering::SeqCst) == 1
                {
                    // First forced call: protocol violation (plain text).
                    return Ok(json!({
                        "choices": [{ "message": { "content": "I will search now..." } }]
                    }));
                }
                Ok(json!({
                    "choices": [{ "message": { "tool_calls": [{
                        "id": "9", "type": "function",
                        "function": { "name": "read_file", "arguments": "{\"path\":\"src/lib.rs\"}" } }]
                    }}]
                }))
            }
        }
        let mut messages = vec![json!({ "role": "user", "content": "do it" })];
        let caps = ProviderCaps { supports_required: true, supports_parallel_flag: true };
        let turn = run_turn_with_enforcement(
            &TextOnceThenTool(AtomicUsize::new(1)), models::GRUNT_MIMO,
            &mut messages, &[], caps, AgentPhase::Exploration, 3,
        )
        .await
        .expect("corrected on the second attempt");
        assert!(matches!(turn, ModelTurn::ToolCalls(_)));
        // Only the original user turn + the final assistant tool-call turn remain.
        assert_eq!(messages.len(), 2, "no correction residue: {messages:?}");
        assert_eq!(messages[1]["role"], "assistant");
        assert!(messages[1].get("tool_calls").is_some());
    }
}
