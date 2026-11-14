//! Turn-loop wiring tests for the `[progress]` flush-injection.
//!
//! These run the real `run_turn` loop against a scripted provider, so they
//! prove the nudge is injected between tool results and the next model call,
//! and that a natural end never nudges.

use super::*;
use async_trait::async_trait;
use crate::env::{remove_var, set_var};
use std::sync::Mutex;

type ExecLog = Arc<Mutex<Vec<serde_json::Value>>>;

fn clear_progress_env() {
    for key in [
        "KCODE_FLUSH_ENABLED",
        "KCODE_FLUSH_INTERVAL_SECONDS",
        "KCODE_FLUSH_EVERY_TURNS",
        "KCODE_FLUSH_TOKEN_THRESHOLD",
        "KCODE_PROGRESS_FILE",
    ] {
        remove_var(key);
    }
    crate::config::invalidate_config_cache();
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let unique = format!(
            "kcode-progress-loop-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Records every message list it is handed, so a test can assert on exactly
/// what the model was sent on each request.
#[derive(Clone)]
struct RecordingScriptProvider {
    rounds: Arc<Mutex<Vec<Vec<StreamEvent>>>>,
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
}

#[async_trait]
impl Provider for RecordingScriptProvider {
    async fn complete(
        &self,
        messages: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<crate::provider::EventStream> {
        self.requests.lock().unwrap().push(messages.to_vec());
        let mut queue = self.rounds.lock().unwrap();
        let events = queue
            .first()
            .expect("scripted rounds exhausted: turn made an unbounded request")
            .clone();
        queue.remove(0);
        Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
    }
    fn name(&self) -> &str {
        "progress-flush-script-test"
    }
    fn model(&self) -> String {
        "glm-test".into()
    }
    fn supports_compaction(&self) -> bool {
        false
    }
    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

struct CaptureTool(ExecLog);

#[async_trait]
impl crate::tool::Tool for CaptureTool {
    fn name(&self) -> &str {
        "capture"
    }
    fn description(&self) -> &str {
        "Capture test input"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"value":{"type":"boolean"}},"required":["value"]})
    }
    async fn execute(
        &self,
        input: serde_json::Value,
        _: crate::tool::ToolContext,
    ) -> Result<crate::tool::ToolOutput> {
        self.0.lock().unwrap().push(input);
        Ok(crate::tool::ToolOutput::new("ok"))
    }
}

fn tool_round(id: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolUseStart {
            id: id.into(),
            name: "capture".into(),
        },
        StreamEvent::ToolInputDeltaFor {
            id: id.into(),
            delta: "{\"value\":true}".into(),
        },
        StreamEvent::ToolUseEndFor { id: id.into() },
        StreamEvent::MessageEnd {
            stop_reason: Some("tool_use".into()),
        },
    ]
}

fn final_text_round() -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta("done".into()),
        StreamEvent::MessageEnd {
            stop_reason: Some("end_turn".into()),
        },
    ]
}

/// Build an agent whose flush state file lives in `dir`, with the config
/// pointed at that directory.
async fn flush_agent(
    dir: &TempDir,
    rounds: Vec<Vec<StreamEvent>>,
) -> (Agent, Arc<Mutex<Vec<Vec<Message>>>>, ExecLog) {
    let _sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
    let requests: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let exec_log: ExecLog = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn Provider> = Arc::new(RecordingScriptProvider {
        rounds: Arc::new(Mutex::new(rounds)),
        requests: Arc::clone(&requests),
    });
    let registry = Registry::empty();
    registry
        .register(
            "capture".into(),
            Arc::new(CaptureTool(Arc::clone(&exec_log))),
        )
        .await;
    let mut agent = Agent::new(provider, registry);
    // Point the session's working directory at the temp dir so the state file
    // and the progress document land there instead of the repo.
    agent.session.working_dir = Some(dir.path().to_string_lossy().to_string());
    agent.progress_flush_state = Some(progress_flush::FlushState::load(
        &progress_flush::state_file_path(
            &crate::config::config().progress,
            Some(dir.path().to_string_lossy().as_ref()),
        ),
    ));
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "test".into(),
            cache_control: None,
        }],
    );
    (agent, requests, exec_log)
}

/// Did any request carry the flush nudge?
fn requests_with_nudge(requests: &Arc<Mutex<Vec<Vec<Message>>>>) -> usize {
    requests
        .lock()
        .unwrap()
        .iter()
        .filter(|messages| {
            messages.iter().any(|message| {
                message.content.iter().any(|block| match block {
                    ContentBlock::Text { text, .. } => text.contains("Progress checkpoint"),
                    _ => false,
                })
            })
        })
        .count()
}

/// A natural-end run (text only, no tool calls) must never nudge, even with the
/// flush state file already past every threshold.
#[tokio::test]
async fn natural_end_run_does_not_nudge() {
    let _guard = PROGRESS_ENV_LOCK.lock().unwrap();
    clear_progress_env();
    let dir = TempDir::new("natural-end");
    let (mut agent, requests, _exec) = flush_agent(&dir, vec![final_text_round()]).await;

    // Force the state so every trigger would fire if the loop reached them.
    agent.progress_flush_state = Some(progress_flush::FlushState {
        last_flush_ts: Some(0),
        last_flush_turn: 0,
        last_flush_tokens: 0,
    });

    agent.run_turn(false).await.unwrap();

    assert_eq!(
        requests_with_nudge(&requests),
        0,
        "a text-only natural end must never be nudged"
    );
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "a natural end makes exactly one request"
    );
}

/// A continuation run must nudge once the interval has elapsed, and must record
/// the flush in the state file.
#[tokio::test]
async fn continuation_run_nudges_and_updates_the_state_file() {
    let _guard = PROGRESS_ENV_LOCK.lock().unwrap();
    clear_progress_env();
    // Make the time trigger fire immediately so the test does not sleep.
    // A 1-second interval is the smallest *enabled* value: `0` disables the
    // time trigger entirely, so the nudge could never fire.
    set_var("KCODE_FLUSH_INTERVAL_SECONDS", "1");
    crate::config::invalidate_config_cache();
    let dir = TempDir::new("continuation");

    // The config is cached process-wide, so write the state file the agent will
    // read: an already-elapsed flush plus the config defaults read at runtime.
    let cfg = {
        let mut loaded = crate::config::Config::default();
        loaded.apply_env_overrides();
        loaded.progress.clone()
    };
    let state_path =
        progress_flush::state_file_path(&cfg, Some(dir.path().to_string_lossy().as_ref()));
    // A flush long ago: any positive interval has elapsed.
    progress_flush::FlushState {
        last_flush_ts: Some(1),
        last_flush_turn: 0,
        last_flush_tokens: 0,
    }
    .save(&state_path)
    .unwrap();

    let (mut agent, requests, exec) =
        flush_agent(&dir, vec![tool_round("t1"), tool_round("t2"), final_text_round()]).await;
    // Reload the state the agent will use (it was written after construction).
    agent.progress_flush_state = Some(progress_flush::FlushState::load(&state_path));

    agent.run_turn(false).await.unwrap();

    assert_eq!(
        exec.lock().unwrap().len(),
        2,
        "both scripted tool rounds must execute"
    );
    let nudged = requests_with_nudge(&requests);
    assert!(
        nudged >= 1,
        "a continuation past the interval must inject the nudge (requests: {})",
        requests.lock().unwrap().len()
    );

    // The nudge names both configured documents.
    let nudge_text = requests
        .lock()
        .unwrap()
        .iter()
        .flat_map(|messages| messages.iter())
        .filter_map(|message| {
            message.content.iter().find_map(|block| match block {
                ContentBlock::Text { text, .. } if text.contains("Progress checkpoint") => {
                    Some(text.clone())
                }
                _ => None,
            })
        })
        .next()
        .expect("a nudge must have been sent");
    assert!(
        nudge_text.contains(&cfg.progress_file),
        "the nudge must name the progress file, got: {nudge_text}"
    );
    assert!(
        nudge_text.contains(&cfg.plan_file),
        "the nudge must name the plan file, got: {nudge_text}"
    );

    // The state file records the flush, so the same threshold cannot re-fire.
    let persisted = progress_flush::FlushState::load(&state_path);
    assert!(
        persisted.last_flush_ts.is_some(),
        "the flush must be recorded in the state file"
    );
    assert!(
        persisted.last_flush_turn >= 1,
        "the recorded flush must carry the turn index"
    );

    clear_progress_env();
}

/// With `enabled = false` the agent never loads flush state, so nothing nudges
/// no matter how far past the thresholds the run goes.
#[tokio::test]
async fn disabled_progress_suppresses_the_nudge() {
    let _guard = PROGRESS_ENV_LOCK.lock().unwrap();
    clear_progress_env();
    set_var("KCODE_FLUSH_ENABLED", "0");
    // Leave the time trigger enabled and elapsed: the only reason nothing
    // nudges is `enabled = false`.
    set_var("KCODE_FLUSH_INTERVAL_SECONDS", "1");
    crate::config::invalidate_config_cache();
    let dir = TempDir::new("disabled");

    let (mut agent, requests, exec) =
        flush_agent(&dir, vec![tool_round("t1"), tool_round("t2"), final_text_round()]).await;
    // Even a fully-elapsed state must not nudge when the feature is off.
    agent.progress_flush_state = Some(progress_flush::FlushState {
        last_flush_ts: Some(0),
        last_flush_turn: 0,
        last_flush_tokens: 0,
    });
    // Mirror the loader: disabled means no state at all.
    agent.progress_flush_state = None;

    agent.run_turn(false).await.unwrap();

    assert_eq!(exec.lock().unwrap().len(), 2, "tools still run when disabled");
    assert_eq!(
        requests_with_nudge(&requests),
        0,
        "enabled = false must suppress every nudge"
    );

    clear_progress_env();
}

/// The turn index grows across loop passes, which is what the turn trigger
/// measures.
#[tokio::test]
async fn turn_index_advances_once_per_loop_pass() {
    let _guard = PROGRESS_ENV_LOCK.lock().unwrap();
    clear_progress_env();
    let dir = TempDir::new("turn-index");

    let (mut agent, _requests, exec) =
        flush_agent(&dir, vec![tool_round("t1"), tool_round("t2"), final_text_round()]).await;
    agent.progress_flush_state = None;

    agent.run_turn(false).await.unwrap();

    assert_eq!(exec.lock().unwrap().len(), 2);
    assert_eq!(
        agent.progress_turn_index, 3,
        "three loop passes (two tool rounds plus the final text round)"
    );
}
