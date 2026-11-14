//! Tests for the `[progress]` flush-injection.
//!
//! These cover the trigger logic, the state file, the rendered nudge, and the
//! env overrides that feed them. The turn-loop wiring itself is covered by
//! `progress_flush_turn_loop_tests.rs`.

use super::*;
use crate::env::{remove_var, set_var};

/// A config with only the fields a test cares about left non-default.
fn cfg() -> crate::config::ProgressConfig {
    crate::config::ProgressConfig::default()
}

/// Remove every `[progress]` env var a test might have set.
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

/// A temp dir that cleans itself up, so the state file never lands in the repo.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let unique = format!(
            "kcode-progress-{label}-{}-{:?}",
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

// ---------------------------------------------------------------------------
// Trigger logic
// ---------------------------------------------------------------------------

#[test]
fn time_trigger_fires_once_the_interval_elapses() {
    let config = cfg();
    // Fresh state: nothing flushed yet, so the clock starts at the first
    // observation and no trigger fires immediately.
    let fresh = FlushState::default();
    assert!(
        triggers_crossed(&config, &fresh, 1_000, 0, 0).is_empty(),
        "a never-flushed state must not fire on the first observation"
    );

    // A flush recorded 599s ago is inside the 600s default interval.
    let recent = FlushState {
        last_flush_ts: Some(1_599),
        last_flush_turn: 0,
        last_flush_tokens: 0,
    };
    assert!(
        triggers_crossed(&config, &recent, 2_000, 0, 0).is_empty(),
        "599s elapsed is below the 600s interval"
    );

    // Exactly at the interval, and past it, the time trigger fires.
    let at_interval = FlushState {
        last_flush_ts: Some(1_400),
        last_flush_turn: 0,
        last_flush_tokens: 0,
    };
    assert_eq!(
        triggers_crossed(&config, &at_interval, 2_000, 0, 0),
        vec![FlushTrigger::Time]
    );
}

#[test]
fn turn_and_token_triggers_fire_at_their_thresholds() {
    let config = cfg();

    // Turn trigger: default flush_every_turns = 5.
    // `last_flush_ts` of 0 with `now_ts` 0 keeps the time trigger silent, so
    // these assertions isolate the turn trigger alone.
    let state = FlushState {
        last_flush_ts: Some(0),
        last_flush_turn: 3,
        last_flush_tokens: 0,
    };
    assert_eq!(
        triggers_crossed(&config, &state, 0, 7, 0),
        Vec::<FlushTrigger>::new(),
        "4 turns since the last flush must not fire a 5-turn trigger"
    );
    assert_eq!(
        triggers_crossed(&config, &state, 0, 8, 0),
        vec![FlushTrigger::Turns],
        "5 turns since the last flush fires the turn trigger"
    );

    // Token trigger: default flush_token_threshold = 40000.
    let token_state = FlushState {
        last_flush_ts: Some(0),
        last_flush_turn: 0,
        last_flush_tokens: 0,
    };
    assert_eq!(
        triggers_crossed(&config, &token_state, 0, 0, 39_999),
        Vec::<FlushTrigger>::new(),
        "39999 tokens since the last flush is below the 40000-token threshold"
    );
    assert_eq!(
        triggers_crossed(&config, &token_state, 0, 0, 40_000),
        vec![FlushTrigger::Tokens],
        "40000 tokens since the last flush fires the token trigger"
    );
}

#[test]
fn zero_valued_thresholds_disable_their_trigger() {
    let mut config = cfg();
    config.flush_interval_seconds = 0;
    config.flush_every_turns = 0;
    config.flush_token_threshold = 0;
    let state = FlushState {
        last_flush_ts: Some(1),
        last_flush_turn: 1,
        last_flush_tokens: 1,
    };
    assert!(
        triggers_crossed(&config, &state, 999_999, 999, 999_999).is_empty(),
        "all three thresholds disabled means nothing can fire"
    );
}

#[test]
fn enabled_false_suppresses_every_trigger() {
    let mut config = cfg();
    config.enabled = false;
    let state = FlushState {
        last_flush_ts: Some(1),
        last_flush_turn: 1,
        last_flush_tokens: 1,
    };
    assert!(
        triggers_crossed(&config, &state, 1_000_000, 1_000, 1_000_000).is_empty(),
        "enabled = false must suppress the time, turn and token triggers"
    );
}

#[test]
fn env_override_changes_which_trigger_fires() {
    let _guard = PROGRESS_ENV_LOCK.lock().unwrap();
    clear_progress_env();

    // Baseline: 600s interval, a flush 500s ago does not fire.
    let mut config = crate::config::Config::default().progress;
    let state = FlushState {
        last_flush_ts: Some(500),
        last_flush_turn: 0,
        last_flush_tokens: 0,
    };
    assert!(
        triggers_crossed(&config, &state, 1_000, 0, 0).is_empty(),
        "baseline: 500s elapsed is below the default 600s interval"
    );

    // Now apply the env override the way the real config loader does and show
    // the same 500s elapsed now crosses the shortened interval.
    set_var("KCODE_FLUSH_INTERVAL_SECONDS", "300");
    let mut loaded = crate::config::Config::default();
    loaded.apply_env_overrides();
    config = loaded.progress.clone();
    assert_eq!(
        config.flush_interval_seconds, 300,
        "KCODE_FLUSH_INTERVAL_SECONDS must reach the config"
    );
    assert_eq!(
        triggers_crossed(&config, &state, 1_000, 0, 0),
        vec![FlushTrigger::Time],
        "500s elapsed now crosses the env-shortened 300s interval"
    );

    // A turn-threshold override moves the turn trigger the same way.
    set_var("KCODE_FLUSH_EVERY_TURNS", "2");
    let mut loaded = crate::config::Config::default();
    loaded.apply_env_overrides();
    assert_eq!(loaded.progress.flush_every_turns, 2);
    let turn_state = FlushState {
        last_flush_ts: Some(1_000),
        last_flush_turn: 4,
        last_flush_tokens: 0,
    };
    assert_eq!(
        triggers_crossed(&loaded.progress, &turn_state, 1_000, 6, 0),
        vec![FlushTrigger::Turns],
        "2 turns since the last flush fires the env-overridden 2-turn trigger"
    );

    // And the file override renames the document the nudge names.
    set_var("KCODE_PROGRESS_FILE", "docs/status.md");
    let mut loaded = crate::config::Config::default();
    loaded.apply_env_overrides();
    assert_eq!(loaded.progress.progress_file, "docs/status.md");
    let nudge = render_nudge(&loaded.progress, &[FlushTrigger::Time]);
    assert!(
        nudge.contains("docs/status.md"),
        "the nudge must name the env-overridden progress file, got: {nudge}"
    );

    clear_progress_env();
}

#[test]
fn env_override_disable_switch_suppresses_everything() {
    let _guard = PROGRESS_ENV_LOCK.lock().unwrap();
    clear_progress_env();
    set_var("KCODE_FLUSH_ENABLED", "0");
    crate::config::invalidate_config_cache();
    let mut loaded = crate::config::Config::default();
    loaded.apply_env_overrides();
    assert!(!loaded.progress.enabled);
    let state = FlushState {
        last_flush_ts: Some(1),
        last_flush_turn: 1,
        last_flush_tokens: 1,
    };
    assert!(triggers_crossed(&loaded.progress, &state, 1_000_000, 1_000, 1_000_000).is_empty());
    clear_progress_env();
}

#[test]
fn invalid_env_values_are_ignored_and_leave_the_default() {
    let _guard = PROGRESS_ENV_LOCK.lock().unwrap();
    clear_progress_env();
    set_var("KCODE_FLUSH_INTERVAL_SECONDS", "not-a-number");
    set_var("KCODE_FLUSH_EVERY_TURNS", "");
    crate::config::invalidate_config_cache();
    let mut loaded = crate::config::Config::default();
    loaded.apply_env_overrides();
    assert_eq!(
        loaded.progress.flush_interval_seconds,
        crate::config::ProgressConfig::default().flush_interval_seconds,
        "an unparseable env value must not clobber the default"
    );
    clear_progress_env();
}

// ---------------------------------------------------------------------------
// Nudge rendering
// ---------------------------------------------------------------------------

#[test]
fn nudge_names_both_documents_and_states_the_discipline() {
    let config = cfg();
    let nudge = render_nudge(&config, &[FlushTrigger::Time]);
    assert!(
        nudge.contains(&config.progress_file),
        "nudge must name the progress file, got: {nudge}"
    );
    assert!(
        nudge.contains(&config.plan_file),
        "nudge must name the plan file, got: {nudge}"
    );
    assert!(nudge.contains("Progress checkpoint"));
    assert!(nudge.contains("One line of visible status"));
    assert!(nudge.contains("Do not start new work before the file is written"));
}

#[test]
fn nudge_respects_inject_doc_paths_false() {
    let mut config = cfg();
    config.inject_doc_paths = false;
    let nudge = render_nudge(&config, &[FlushTrigger::Time]);
    assert!(
        nudge.contains(&config.progress_file),
        "the primary document is named even without the path list"
    );
    assert!(
        !nudge.contains(&config.plan_file),
        "the plan path must not appear when inject_doc_paths is false, got: {nudge}"
    );
    assert!(!nudge.contains("Current documents"));
}

#[test]
fn nudge_is_capped_at_nudge_max_chars() {
    let mut config = cfg();
    // A tiny cap forces truncation of the default wording.
    config.nudge_max_chars = 40;
    let nudge = render_nudge(&config, &[FlushTrigger::Time, FlushTrigger::Turns]);
    assert_eq!(
        nudge.chars().count(),
        40,
        "the nudge must be capped at nudge_max_chars, got {nudge}"
    );
    assert!(nudge.starts_with("Progress checkpoint"));

    // A zero cap means "no cap".
    config.nudge_max_chars = 0;
    let uncapped = render_nudge(&config, &[FlushTrigger::Time]);
    assert!(uncapped.chars().count() > 40);
}

#[test]
fn nudge_names_the_trigger_reasons() {
    let config = cfg();
    let nudge = render_nudge(&config, &[FlushTrigger::Time, FlushTrigger::Turns, FlushTrigger::Tokens]);
    assert!(nudge.contains("time"), "got: {nudge}");
    assert!(nudge.contains("turns"), "got: {nudge}");
    assert!(nudge.contains("tokens"), "got: {nudge}");
}

#[test]
fn nudge_cap_counts_characters_not_bytes() {
    let mut config = cfg();
    // Multi-byte paths must not be cut mid-character.
    config.progress_file = "文档/进度.md".to_string();
    config.nudge_max_chars = 10;
    let nudge = render_nudge(&config, &[FlushTrigger::Time]);
    assert_eq!(nudge.chars().count(), 10);
    assert!(
        std::str::from_utf8(nudge.as_bytes()).is_ok(),
        "a char-based cap must never split a multi-byte character"
    );
}

// ---------------------------------------------------------------------------
// State file
// ---------------------------------------------------------------------------

#[test]
fn state_file_round_trips_and_creates_parent_directories() {
    let dir = TempDir::new("state-roundtrip");
    let path = dir.path().join(".kcode/progress/flush-state.json");

    // Missing file is the never-flushed state.
    assert_eq!(FlushState::load(&path), FlushState::default());

    let state = FlushState {
        last_flush_ts: Some(1_700_000_000),
        last_flush_turn: 7,
        last_flush_tokens: 123_456,
    };
    state.save(&path).expect("save must create parent dirs");
    assert!(path.exists(), "state file must be written");

    let reloaded = FlushState::load(&path);
    assert_eq!(reloaded, state, "the state file must round-trip exactly");

    // The persisted shape carries the documented keys.
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("last_flush_ts"), "got: {text}");
    assert!(text.contains("last_flush_turn"), "got: {text}");
    assert!(text.contains("last_flush_tokens"), "got: {text}");
}

#[test]
fn corrupt_state_file_falls_back_to_the_default_state() {
    let dir = TempDir::new("corrupt-state");
    let path = dir.path().join("flush-state.json");
    std::fs::write(&path, "{not json at all").unwrap();
    assert_eq!(
        FlushState::load(&path),
        FlushState::default(),
        "a corrupt state file must not fail the turn"
    );
}

#[test]
fn flushing_updates_the_persisted_state() {
    let dir = TempDir::new("flush-update");
    let path = dir.path().join("nested/flush-state.json");

    // First observation: no flush recorded yet.
    let mut state = FlushState::load(&path);
    let reasons = triggers_crossed(&cfg(), &state, 1_000, 0, 0);
    assert!(reasons.is_empty());

    // The loop records a flush: state is rewritten with the current counters.
    state = FlushState {
        last_flush_ts: Some(1_000),
        last_flush_turn: 0,
        last_flush_tokens: 0,
    };
    state.save(&path).unwrap();

    // Immediately afterwards, no trigger fires again (the clock restarted).
    let after = FlushState::load(&path);
    assert!(
        triggers_crossed(&cfg(), &after, 1_000, 0, 0).is_empty(),
        "a just-recorded flush must not re-trigger at the same instant"
    );

    // Once the interval passes, it fires again.
    assert_eq!(
        triggers_crossed(&cfg(), &after, 1_600, 0, 0),
        vec![FlushTrigger::Time]
    );
}

#[test]
fn state_file_path_resolves_relative_to_the_working_dir() {
    let config = cfg();
    let resolved = state_file_path(&config, Some("/tmp/some-session"));
    assert_eq!(resolved, PathBuf::from("/tmp/some-session/.kcode/progress/flush-state.json"));

    // Absolute paths are honoured verbatim.
    let mut absolute = cfg();
    absolute.state_file = "/var/lib/kcode/flush.json".to_string();
    assert_eq!(
        state_file_path(&absolute, Some("/tmp/some-session")),
        PathBuf::from("/var/lib/kcode/flush.json")
    );
}

// ---------------------------------------------------------------------------
// Config plumbing
// ---------------------------------------------------------------------------

#[test]
fn progress_defaults_match_the_documented_values() {
    let config = crate::config::ProgressConfig::default();
    assert!(config.enabled);
    assert_eq!(config.flush_interval_seconds, 600);
    assert_eq!(config.flush_every_turns, 5);
    assert_eq!(config.flush_token_threshold, 40_000);
    assert_eq!(config.progress_file, ".kcode/progress.md");
    assert_eq!(config.plan_file, ".kcode/plan.md");
    assert_eq!(config.state_file, ".kcode/progress/flush-state.json");
    assert!(config.inject_doc_paths);
    assert_eq!(config.nudge_max_chars, 8_000);
}

#[test]
fn progress_section_parses_from_toml() {
    let config: crate::config::ProgressConfig = toml::from_str(
        r#"
enabled = false
flush_interval_seconds = 300
flush_every_turns = 2
flush_token_threshold = 1000
progress_file = "notes/progress.md"
plan_file = "notes/plan.md"
state_file = "notes/state.json"
inject_doc_paths = false
nudge_max_chars = 500
"#,
    )
    .unwrap();
    assert!(!config.enabled);
    assert_eq!(config.flush_interval_seconds, 300);
    assert_eq!(config.flush_every_turns, 2);
    assert_eq!(config.flush_token_threshold, 1_000);
    assert_eq!(config.progress_file, "notes/progress.md");
    assert_eq!(config.plan_file, "notes/plan.md");
    assert_eq!(config.state_file, "notes/state.json");
    assert!(!config.inject_doc_paths);
    assert_eq!(config.nudge_max_chars, 500);
}

#[test]
fn empty_toml_yields_the_progress_defaults() {
    let config: crate::config::Config = toml::from_str("").unwrap();
    assert!(config.progress.enabled);
    assert_eq!(config.progress.flush_interval_seconds, 600);
}
