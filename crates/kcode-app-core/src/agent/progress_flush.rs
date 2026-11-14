//! Periodic progress-flush nudges for long-running turns.
//!
//! The failure this fixes is time-shaped: a thinking model can spend tens of
//! minutes inside two or three tool rounds with no visible output, so a crash
//! (SIGTERM, quota exhaustion, restart) loses everything that only existed as
//! invisible reasoning. The turn loop injects a "write your progress now" nudge
//! between the tool results of one round and the next model call whenever a
//! configured threshold is crossed.
//!
//! Every threshold comes from `[progress]` config (env overrides
//! `KCODE_FLUSH_*`); nothing here hardcodes a policy value. The state lives in a
//! small JSON file (`state_file`) so it survives a crash exactly the way the
//! progress document it protects does.
//!
//! Design authority: `RESEARCH-progressive-flush.md` (t_5f0adb8a), which
//! modelled the nudge wording on Anthropic's documented progress-update line and
//! the injection contract on Claude Code's `additionalContext` hook pattern.

use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[cfg(test)]
use super::*;

/// Persisted flush bookkeeping. JSON, not Markdown: the model is less likely to
/// rewrite structured state it did not author, and this file is written by the
/// harness only.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct FlushState {
    /// Unix epoch seconds of the last flush. `None` means "never flushed".
    #[serde(default)]
    pub(super) last_flush_ts: Option<u64>,
    /// Turn-loop iteration index of the last flush.
    #[serde(default)]
    pub(super) last_flush_turn: u32,
    /// Tokens accumulated at the last flush.
    #[serde(default)]
    pub(super) last_flush_tokens: u64,
}

impl FlushState {
    /// Read the state file. A missing file is the "never flushed" state, and a
    /// corrupt file is treated the same way rather than failing the turn: the
    /// nudge is best-effort, so a bad state file must not kill a run.
    pub(super) fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Write the state file, creating parent directories. Failures are reported
    /// to the caller so it can log and continue.
    pub(super) fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        std::fs::write(path, text)
    }
}

/// Which threshold crossed. Reported so the nudge and the log can name the
/// reason, and so tests can assert on the trigger rather than on prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FlushTrigger {
    /// `flush_interval_seconds` elapsed since the last flush.
    Time,
    /// `flush_every_turns` iterations since the last flush.
    Turns,
    /// `flush_token_threshold` tokens since the last flush.
    Tokens,
}

impl FlushTrigger {
    /// Short label for logs.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            FlushTrigger::Time => "time",
            FlushTrigger::Turns => "turns",
            FlushTrigger::Tokens => "tokens",
        }
    }
}

/// Resolve the state-file path for a working directory.
///
/// Relative `state_file` values resolve against the session working directory
/// (falling back to the process cwd), so two concurrent sessions in different
/// directories do not share one flush clock.
pub(super) fn state_file_path(cfg: &crate::config::ProgressConfig, working_dir: Option<&str>) -> PathBuf {
    let raw = cfg.state_file.trim();
    let path = Path::new(raw);
    if path.is_absolute() || raw.is_empty() {
        return path.to_path_buf();
    }
    let base = working_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    base.join(path)
}

/// Serializes every `[progress]` env-mutating test in this crate's test binary.
///
/// `std::env` is process-global, and the two test modules below both mutate
/// `KCODE_FLUSH_*`, so they must share one lock: two separate `static`s in two
/// modules would not exclude each other and one test would read another's value.
#[cfg(test)]
pub(super) static PROGRESS_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Unix epoch seconds, or 0 if the clock is before the epoch.
pub(super) fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Which triggers have crossed, given the persisted state and the current
/// counters. Returns an empty vec when nothing has crossed, so the caller can
/// treat "no reasons" as "do not nudge".
///
/// A zero-valued threshold disables that trigger, matching the
/// `reactive_threshold_tokens = 0 disables` convention already documented in the
/// `[compaction]` section of the default config.
pub(super) fn triggers_crossed(
    cfg: &crate::config::ProgressConfig,
    state: &FlushState,
    now_ts: u64,
    turn_index: u32,
    tokens_since_flush: u64,
) -> Vec<FlushTrigger> {
    let mut reasons = Vec::new();
    if !cfg.enabled {
        return reasons;
    }

    // Time is primary. With no recorded flush yet, the clock starts at the
    // first observed timestamp so a resumed session does not nudge instantly.
    if cfg.flush_interval_seconds > 0 {
        let elapsed = match state.last_flush_ts {
            Some(last) => now_ts.saturating_sub(last),
            None => 0,
        };
        if elapsed >= cfg.flush_interval_seconds {
            reasons.push(FlushTrigger::Time);
        }
    }

    if cfg.flush_every_turns > 0 {
        let turns = turn_index.saturating_sub(state.last_flush_turn);
        if turns >= cfg.flush_every_turns {
            reasons.push(FlushTrigger::Turns);
        }
    }

    if cfg.flush_token_threshold > 0 {
        let tokens = tokens_since_flush.saturating_sub(state.last_flush_tokens);
        if tokens >= cfg.flush_token_threshold {
            reasons.push(FlushTrigger::Tokens);
        }
    }

    reasons
}

/// Render the flush nudge.
///
/// Wording follows the research (modelled on Anthropic's documented progress
/// line and its long-running-agent harness discipline): name the document, say
/// what to write, require one visible status line, and forbid starting new work
/// before the file exists.
pub(super) fn render_nudge(
    cfg: &crate::config::ProgressConfig,
    reasons: &[FlushTrigger],
) -> String {
    let reason_label = reasons
        .iter()
        .map(|reason| reason.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let mut nudge = format!(
        "Progress checkpoint ({reason_label}). Update `{progress_file}` now: what you established since the last flush, with sources for each claim, what remains, and any blocked item. One line of visible status in your reply. Do not start new work before the file is written.",
        progress_file = cfg.progress_file.trim()
    );
    if cfg.inject_doc_paths {
        nudge.push_str(&format!(
            " Current documents: `{}`, `{}`.",
            cfg.progress_file.trim(),
            cfg.plan_file.trim()
        ));
    }
    truncate_to_chars(&nudge, cfg.nudge_max_chars)
}

/// Cap a rendered nudge at `max` characters. `max` of 0 disables the cap.
///
/// Character-based (not byte-based) so a multi-byte path or CJK text never cuts
/// a character in half.
fn truncate_to_chars(text: &str, max: usize) -> String {
    if max == 0 || text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect()
}

#[cfg(test)]
#[path = "progress_flush_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "progress_flush_turn_loop_tests.rs"]
mod turn_loop_tests;
