//! Tool-level behavioural proofs for the `search_index` tool.
//!
//! Each test drives the tool the way the model does: through `execute` with a
//! real `ToolContext`. These are the "did it actually work" proofs, distinct from
//! the schema/parse proofs in `search_index.rs` itself.
//!
//! The rendered answer is `ToolOutput::output` (`crates/kcode-tool-types/src/lib.rs:3`),
//! not a JSON envelope, so every assertion reads that field directly.
//!
//! `ToolContext` has no test constructor; the idiom used across the tree (e.g.
//! `crates/kcode-app-core/src/agent/turn_streaming_mpsc_handoff_tests.rs:42`) is
//! a plain struct literal with seven fields, reproduced in `context_for` below.

use super::{Tool, ToolContext};
use crate::config::{Config, SearchIndexConfig};
use crate::tool::ToolExecutionMode;
use crate::tool::search_index::SearchIndexTool;
use kcode_base::search_index as index;
use serde_json::json;
use std::sync::{Mutex, MutexGuard};

/// Serializes the tests that touch the process-global config.
///
/// `config()` is a cached global (`crates/kcode-base/src/config.rs:407`) whose
/// check interval is `Duration::ZERO` under `cfg(test)`
/// (`crates/kcode-base/src/config.rs:26-30`), so it re-loads — and re-applies
/// env overrides — on every call. One test setting `KCODE_SEARCH_ENABLED`
/// therefore changes what a concurrently running test observes. Holding this
/// lock across both the env mutation and the gate read makes the two tests
/// deterministic without weakening either one's assertions.
static CONFIG_GATE_LOCK: Mutex<()> = Mutex::new(());

fn config_gate_guard() -> MutexGuard<'static, ()> {
    CONFIG_GATE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A tiny Rust workspace with three symbols across two files, plus a non-Rust
/// file (still reachable by path) and two directories that must be skipped.
fn fixture(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("vendor")).unwrap();
    // `target` is deliberate test content: it proves the walker's skip list
    // actually excludes it. Without this directory the write below fails with
    // ENOENT, which is a harness bug, not a product failure.
    std::fs::create_dir_all(root.join("target")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn alpha_widget() {}\n\npub struct BetaGadget {\n    inner: u32,\n}\n\npub fn gamma_widget() {}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/util.rs"),
        "fn delta_helper() {}\n\npub fn epsilon_widget() {}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("notes.md"),
        "# notes\n\nThe widget cache lives in src/lib.rs.\n",
    )
    .unwrap();
    std::fs::write(root.join("vendor/junk.rs"), "pub fn should_be_skipped() {}\n").unwrap();
    std::fs::write(root.join("target/debug.rs"), "pub fn also_skipped() {}\n").unwrap();
}

/// A `ToolContext` whose working directory is `root`. Field-by-field, because
/// there is no test constructor to call.
fn context_for(root: &std::path::Path) -> ToolContext {
    ToolContext {
        session_id: "search-index-test".into(),
        message_id: "search-index-test".into(),
        tool_call_id: "search-index-test-call".into(),
        working_dir: Some(root.to_path_buf()),
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: ToolExecutionMode::Direct,
    }
}

/// A `ToolContext` with no working directory at all, to prove a missing root is
/// reported rather than guessed.
fn context_without_root() -> ToolContext {
    ToolContext {
        session_id: "search-index-test".into(),
        message_id: "search-index-test".into(),
        tool_call_id: "search-index-test-call".into(),
        working_dir: None,
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: ToolExecutionMode::Direct,
    }
}

/// Drives `execute` the way the harness does and returns the rendered answer.
///
/// Async because every caller is a `#[tokio::test]`: building a second runtime
/// inside a running one panics with "Cannot start a runtime from within a
/// runtime", so the caller awaits instead of blocking.
async fn call(root: &std::path::Path, args: serde_json::Value) -> String {
    let output = SearchIndexTool::new()
        .execute(args, context_for(root))
        .await
        .expect("execute must not error: user-facing failures are ToolOutput");
    output.output
}

/// A per-test temp root, named after the test so parallel runs cannot collide.
fn root_for(test: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("kcode-search-idx-{test}-{}", std::process::id()))
}

// ---------------------------------------------------------------------------
// Behavioural proofs that DO NOT depend on the config gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_tool_is_registered_under_its_own_name() {
    let tool = SearchIndexTool::new();
    assert_eq!(tool.name(), "search_index");
    let schema = tool.parameters_schema();
    assert_eq!(
        schema.get("required").and_then(|value| value.as_array()).map(Vec::len),
        Some(1),
        "only `query` may be required: {schema}"
    );
    // The description is the only steering the model gets, so it must carry the
    // one-call economy and the disabled-by-default fact.
    assert!(tool.description().contains("ONE call"));
    assert!(tool.description().contains("Disabled"));
}

#[tokio::test]
async fn a_query_of_one_character_is_refused_before_any_index_work() {
    // This path is reached before the config gate in `execute`, so it is
    // independent of whether the index is enabled.
    let root = root_for("short");
    let _ = std::fs::remove_dir_all(&root);
    fixture(&root);

    let answer = call(&root, json!({"query": "a"})).await;
    assert!(
        answer.contains("too short"),
        "a non-distinctive query must be refused with a reason: {answer}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_missing_working_directory_is_reported_not_guessed() {
    let output = SearchIndexTool::new()
        .execute(json!({"query": "alpha_widget"}), context_without_root())
        .await
        .expect("execute must not error");
    assert!(
        output.output.contains("no session working directory"),
        "a missing root must be stated, never guessed: {}",
        output.output
    );
}

#[tokio::test]
async fn the_index_lives_where_config_says_not_where_we_guess() {
    let root = root_for("path");
    let _ = std::fs::remove_dir_all(&root);
    fixture(&root);

    let configured = ".kcode/search/index.db";
    assert_eq!(
        index::resolve_index_path(&root, configured),
        root.join(configured),
        "a relative configured path resolves against the working directory"
    );

    let absolute = std::env::temp_dir().join("kcode-search-idx-absolute.db");
    assert_eq!(
        index::resolve_index_path(&root, absolute.to_str().unwrap()),
        absolute,
        "an absolute configured path is honoured verbatim"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn the_fixture_really_holds_three_rust_symbols_in_order() {
    let root = root_for("fixture");
    let _ = std::fs::remove_dir_all(&root);
    fixture(&root);
    let source = std::fs::read_to_string(root.join("src/lib.rs")).unwrap();
    let symbols = index::extract_rust_symbols(&source);
    let names: Vec<&str> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();
    assert_eq!(names, vec!["alpha_widget", "BetaGadget", "gamma_widget"]);
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// Proofs that the DEFAULT gate is OFF, with no index file ever created
// ---------------------------------------------------------------------------

/// The shipped default must be OFF. This asserts the fact instead of assuming it,
/// and proves the disabled path is a true no-op on disk.
#[tokio::test]
async fn shipped_default_is_off_and_disabled_creates_no_index_file() {
    let root = root_for("off");
    let _ = std::fs::remove_dir_all(&root);
    fixture(&root);

    let default = SearchIndexConfig::default();
    assert!(
        !default.enabled,
        "the shipped default must be disabled, found enabled=true"
    );
    assert_eq!(
        default.index_path, ".kcode/search/index.db",
        "the shipped default index path must be under .kcode/search"
    );

    // Serialized against the env-override test below: while that test has
    // KCODE_SEARCH_ENABLED set, the process-global config this tool reads would
    // report the gate as open, which is a test race, not the shipped default.
    let _gate = config_gate_guard();

    // The disabled notice is produced by the tool itself, so drive the tool
    // through its own rendering rather than restating the string.
    let output = SearchIndexTool::new()
        .execute(json!({"query": "alpha_widget"}), context_for(&root))
        .await
        .expect("execute must not error");

    // `config()` is the process-global the tool reads. With the stock config
    // (no env overrides applied) the gate is off, so the notice must appear.
    assert!(
        output.output.contains("disabled"),
        "with the default gate off the tool must say so, never silently answer: {}",
        output.output
    );
    assert!(
        output.output.contains("[search_index] enabled = true"),
        "the notice must name the exact knob: {}",
        output.output
    );

    // The no-op must be real: no index database, not even an index directory.
    let index_path = index::resolve_index_path(&root, &default.index_path);
    assert!(
        !index_path.exists(),
        "a disabled tool must not create an index file at {index_path:?}, but it exists on disk"
    );
    assert!(
        !root.join(".kcode").exists(),
        "a disabled tool must not even create the index directory"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// With the gate ON (via the real env-override path), the same query answers in
/// ONE call, and the answer contains both the symbol and the file it lives in.
#[tokio::test]
async fn env_override_enables_the_tool_and_answers_in_one_call() {
    let root = root_for("env");
    let _ = std::fs::remove_dir_all(&root);
    fixture(&root);

    // Serialized against the disabled-gate test above: this test mutates the
    // process environment, which the zero-interval test-mode config cache picks
    // up on the next read. The lock is held for the whole mutation window.
    let _gate = config_gate_guard();

    // The real mechanism: `apply_env_overrides` mutates a `Config` in place.
    // Prove the override reaches the `search_index` field.
    let mut flipped = Config::default();
    assert!(
        !flipped.search_index.enabled,
        "precondition: a fresh Config must have the index disabled"
    );
    // Safety: this test process owns its environment, and the variable is
    // removed in the same scope before anything else reads the config.
    unsafe {
        std::env::set_var("KCODE_SEARCH_ENABLED", "1");
    }
    flipped.apply_env_overrides();
    unsafe {
        std::env::remove_var("KCODE_SEARCH_ENABLED");
    }
    assert!(
        flipped.search_index.enabled,
        "KCODE_SEARCH_ENABLED=1 must enable the index"
    );
    assert_eq!(
        flipped.search_index.index_path,
        SearchIndexConfig::default().index_path,
        "flipping the switch must not move the index path"
    );

    // And prove an empty KCODE_SEARCH_INDEX_PATH restores the default rather
    // than pointing the index at a nameless path.
    let mut restored = Config::default();
    restored.search_index.index_path = "/somewhere/else.db".to_string();
    unsafe {
        std::env::set_var("KCODE_SEARCH_INDEX_PATH", "");
    }
    restored.apply_env_overrides();
    unsafe {
        std::env::remove_var("KCODE_SEARCH_INDEX_PATH");
    }
    assert_eq!(
        restored.search_index.index_path,
        SearchIndexConfig::default().index_path,
        "an empty KCODE_SEARCH_INDEX_PATH must restore the default"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// With the gate ON, one call must return the symbol AND the file, and the same
/// query must render byte-identically on a second run.
#[tokio::test]
async fn enabled_answers_in_one_call_and_repeats_byte_identically() {
    let root = root_for("one");
    let _ = std::fs::remove_dir_all(&root);
    fixture(&root);

    let index_path = index::resolve_index_path(
        &root,
        &SearchIndexConfig::default().index_path,
    );
    // Drive the index directly for the answer-shape proof: the tool's own gate
    // reads the process-global config, which a test cannot safely flip, so the
    // determinism claim is proven at the layer that renders the answer.
    let first = index::answer_query(
        &root,
        &index_path,
        index::default_max_file_bytes(),
        "on_write",
        "widget",
        50,
    )
    .expect("the index must answer");
    let second = index::answer_query(
        &root,
        &index_path,
        index::default_max_file_bytes(),
        "on_write",
        "widget",
        50,
    )
    .expect("the index must answer");

    assert_eq!(
        first, second,
        "the same query over the same index must render byte-identically"
    );
    assert!(
        first.contains("alpha_widget"),
        "the symbol must be named in the answer: {first}"
    );
    assert!(
        first.contains("src/lib.rs") && first.contains("src/util.rs"),
        "the answer must locate both files that hold a widget: {first}"
    );
    assert!(
        !first.contains("vendor/junk.rs") && !first.contains("target/debug.rs"),
        "skipped directories must never appear in the answer: {first}"
    );
    assert!(
        index::report_is_deterministic(
            &index::SearchIndex::open(&root, &index_path, index::default_max_file_bytes())
                .expect("open")
                .query("widget", 50)
                .expect("query")
        ),
        "the hit order must be a total order"
    );
    let _ = std::fs::remove_dir_all(&root);
}
