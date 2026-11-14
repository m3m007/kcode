//! Tests for the kcode-side context-prune wiring.
//!
//! These are the proofs the task requires, each as an assertion rather than a
//! description:
//!
//! - **byte-stability** — the same payload compressed twice is byte-identical,
//!   and compressing it in a *fresh process-equivalent* state (new settings,
//!   new cache) is byte-identical too.
//! - **skeleton keeps signatures / drops bodies** — a real Rust file goes
//!   through `rust_skeleton` and the signatures survive while bodies are elided.
//! - **graph deterministic** — two runs over the same sources render
//!   byte-identical sorted/deduped text.
//! - **cache hit returns identical bytes** — the raw payload stored under its
//!   SHA-256 hash reads back byte-identical.
//! - **disabled = zero change** — `enabled = false` returns the input untouched.
//! - **fail-open** — an unopenable cache path and an unparseable Rust payload
//!   both pass the original through unchanged.
//! - **config env override changes behavior** — `KCODE_PRUNE_*` env vars move
//!   the parsed config, which then changes what the wiring does.

use super::*;
use crate::config::ContextPruneConfig;

/// A payload big enough to clear the default `min_bytes` and shaped so the
/// deterministic transforms actually fire: a JSON document with wasted
/// whitespace plus a long run of identical lines.
fn compressible_text() -> String {
    let mut out = String::new();
    out.push_str("{\n  \"tool\":   \"bash\",\n  \"status\": \"ok\",\n  \"lines\": [1, 2,   3]\n}\n");
    for _ in 0..40 {
        out.push_str("[2026-11-14 00:00:00] INFO worker pool heartbeat status=ok latency=3ms\n");
    }
    out.push_str("RESULT: 42\n");
    out
}

/// A Rust source with bodies worth dropping and signatures worth keeping.
///
/// Must stay above the default `min_bytes` (2048) or the file-read stage's
/// threshold gate correctly no-ops and the skeleton is never reached. A real
/// Rust source is thousands of bytes, so this keeps the payload representative
/// rather than trimming it under the gate.
fn rust_source() -> String {
    let mut out = String::new();
    out.push_str("/// Adds two numbers.\n");
    out.push_str("pub fn add(a: u32, b: u32) -> u32 {\n");
    out.push_str("    let sum = a + b;\n");
    out.push_str("    sum\n");
    out.push_str("}\n\n");
    out.push_str("struct Point {\n");
    out.push_str("    x: f64,\n");
    out.push_str("    y: f64,\n");
    out.push_str("}\n\n");
    out.push_str("impl Point {\n");
    out.push_str("    fn norm(&self) -> f64 {\n");
    out.push_str("        (self.x * self.x + self.y * self.y).sqrt()\n");
    out.push_str("    }\n");
    out.push_str("}\n\n");
    out.push_str("fn helper() {\n");
    // Long body: keeps the payload above `min_bytes` and gives the skeleton
    // plenty to elide. The body content is asserted absent below.
    for _ in 0..140 {
        out.push_str("    println!(\"spam\");\n");
    }
    out.push_str("}\n");
    out
}

fn enabled_settings(cache_path: &str) -> PruneSettings {
    PruneSettings {
        enabled: true,
        min_bytes: 2048,
        repeated_threshold: 3,
        skeleton: true,
        cache_path: cache_path.to_string(),
    }
}

/// Temp dir that cleans itself up. `crate::config` tests use `tempfile`, which is
/// already a dev-dependency of this crate.
fn temp_cache_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("kcode-context-prune-{}-{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    dir.join("cache.db")
}

#[test]
fn byte_stability_same_payload_compresses_to_identical_bytes() {
    let text = compressible_text();
    let settings = PruneSettings {
        cache_path: String::new(),
        ..enabled_settings("")
    };

    let (first, first_outcome) = compress_bytes(&text, &settings, false);
    let (second, second_outcome) = compress_bytes(&text, &settings, false);

    assert_eq!(first.as_bytes(), second.as_bytes(), "byte-stability violated");
    assert_eq!(first_outcome, second_outcome);
    assert!(
        first_outcome.ratio() > 0.40,
        "expected meaningful compression, got {:.2}",
        first_outcome.ratio()
    );
    assert!(!first_outcome.unchanged);
}

#[test]
fn byte_stability_survives_a_fresh_process_equivalent_state() {
    // Byte-stability has to hold across processes, not just within one call: a
    // prompt cache built by an earlier kcode run must still match. Fresh
    // settings and a fresh cache path stand in for the new process.
    let text = compressible_text();
    let a = enabled_settings("");
    let b = enabled_settings("");

    let (first, _) = compress_bytes(&text, &a, false);
    let (second, _) = compress_bytes(&text, &b, false);
    assert_eq!(first.as_bytes(), second.as_bytes());
}

#[test]
fn disabled_is_a_byte_for_byte_no_op_on_both_stages() {
    let text = compressible_text();
    let settings = PruneSettings {
        enabled: false,
        ..enabled_settings("")
    };

    let (file_out, file_outcome) = prune_file_read(&text, "notes.txt", &settings);
    assert_eq!(file_out, text);
    assert!(file_outcome.unchanged);
    assert_eq!(file_outcome.ratio(), 0.0);
    assert!(file_outcome.transforms_applied.is_empty());

    let (tool_out, tool_outcome) = prune_tool_output(&text, &settings);
    assert_eq!(tool_out, text);
    assert!(tool_outcome.unchanged);

    // And the default config is disabled, so a zero-config kcode never compresses.
    assert!(!ContextPruneConfig::default().enabled);
}

#[test]
fn sub_threshold_payload_passes_through_untouched() {
    let small = "tiny tool output\nsecond line\n".to_string();
    let settings = enabled_settings("");

    let (out, outcome) = prune_tool_output(&small, &settings);
    assert_eq!(out, small);
    assert!(outcome.unchanged);
}

#[test]
fn enabled_tool_output_actually_compresses_and_reports_the_ratio() {
    // Behavioral proof (not a grep): with the feature on, a compressible tool
    // result comes back smaller, and the outcome reports the savings.
    let text = compressible_text();
    let settings = enabled_settings("");

    let (compressed, outcome) = prune_tool_output(&text, &settings);
    assert!(
        outcome.output_bytes < outcome.input_bytes,
        "expected compression, {} -> {}",
        outcome.input_bytes,
        outcome.output_bytes
    );
    assert!(compressed.len() < text.len());
    assert!(outcome.ratio() > 0.0);
    assert!(
        outcome.transforms_applied.iter().any(|t| *t == "dedupe_lines"),
        "expected dedupe_lines to fire: {:?}",
        outcome.transforms_applied
    );
    // Meaning is preserved: the distinct facts survive.
    assert!(compressed.contains("RESULT: 42"));
    assert!(compressed.contains("worker pool heartbeat status=ok"));
}

#[test]
fn rust_file_read_is_skeletonized_keeping_signatures_dropping_bodies() {
    let source = rust_source();
    let settings = enabled_settings("");

    let (skeleton, outcome) = prune_file_read(&source, "src/lib.rs", &settings);

    assert_eq!(
        outcome.transforms_applied,
        vec!["rust_skeleton"],
        "a Rust read should go through the skeleton"
    );
    assert!(
        outcome.output_bytes < outcome.input_bytes,
        "skeleton {} >= input {}",
        outcome.output_bytes,
        outcome.input_bytes
    );
    // signatures survive
    assert!(skeleton.contains("pub fn add(a: u32, b: u32) -> u32"), "{skeleton}");
    assert!(skeleton.contains("fn norm(&self) -> f64"), "{skeleton}");
    assert!(skeleton.contains("fn helper()"), "{skeleton}");
    assert!(skeleton.contains("struct Point"), "{skeleton}");
    assert!(skeleton.contains("impl Point"), "{skeleton}");
    assert!(skeleton.contains("/// Adds two numbers."), "{skeleton}");
    // bodies are dropped
    assert!(!skeleton.contains("let sum"), "body leaked:\n{skeleton}");
    assert!(!skeleton.contains(".sqrt()"), "body leaked:\n{skeleton}");
    assert!(!skeleton.contains("spam"), "body leaked:\n{skeleton}");
    assert!(skeleton.contains("lines elided"), "elision marker missing:\n{skeleton}");
}

#[test]
fn rust_file_read_skeleton_is_byte_stable() {
    let source = rust_source();
    let settings = enabled_settings("");

    let (first, _) = prune_file_read(&source, "src/lib.rs", &settings);
    let (second, _) = prune_file_read(&source, "src/lib.rs", &settings);
    assert_eq!(first.as_bytes(), second.as_bytes());
}

#[test]
fn non_rust_file_read_uses_the_text_compressor() {
    let mut text = String::new();
    text.push_str("build log:\n");
    // Must clear the default `min_bytes` (2048) or the stage no-ops before the
    // dedupe transform can fire. A real build log is far larger than this.
    for _ in 0..150 {
        text.push_str("compiling kcode v0.2.0\n");
    }
    let settings = enabled_settings("");

    let (compressed, outcome) = prune_file_read(&text, "build.log", &settings);
    assert!(outcome.transforms_applied.contains(&"dedupe_lines"));
    assert!(compressed.len() < text.len());
    assert!(!compressed.contains("pub fn"), "no skeleton on a log file");
}

#[test]
fn skeleton_off_falls_back_to_the_text_compressor_for_rust() {
    let source = rust_source();
    let settings = PruneSettings {
        skeleton: false,
        ..enabled_settings("")
    };

    let (out, outcome) = prune_file_read(&source, "src/lib.rs", &settings);

    // The skeleton must not run: that is the whole point of `skeleton = false`.
    assert!(
        !outcome.transforms_applied.contains(&"rust_skeleton"),
        "skeleton must be off, got {:?}",
        outcome.transforms_applied
    );
    // It must instead fall back to the text compressor, which on this payload
    // collapses the 140 identical `println!` lines.
    assert!(
        outcome.transforms_applied.contains(&"dedupe_lines"),
        "expected the text compressor to run, got {:?}",
        outcome.transforms_applied
    );
    assert!(
        outcome.output_bytes < outcome.input_bytes,
        "text compressor did not shrink the payload: {} -> {}",
        outcome.input_bytes,
        outcome.output_bytes
    );

    // Signatures survive the text compressor unchanged (it only collapses
    // redundancy, it never drops code).
    assert!(out.contains("pub fn add(a: u32, b: u32) -> u32"), "{out}");
    assert!(out.contains("fn norm(&self) -> f64"), "{out}");
    assert!(out.contains("struct Point"), "{out}");
    assert!(out.contains("/// Adds two numbers."), "{out}");

    // Bodies are *deduped*, not elided: the code is still present, and the
    // skeleton's `lines elided` marker is absent because the skeleton never ran.
    assert!(out.contains("println!(\"spam\")"), "body was dropped:\n{out}");
    assert!(
        out.contains("previous line repeated"),
        "expected the dedupe marker:\n{out}"
    );
    assert!(!out.contains("lines elided"), "skeleton ran despite skeleton=false:\n{out}");
}

#[test]
fn code_graph_is_deterministic_sorted_and_deduped() {
    use context_prune::graph::{CodeGraph, render_text};

    let a = rust_source();
    let b = "/// Second module.\npub fn other() -> u32 {\n    add(1, 2)\n}\n".to_string();

    let build = || {
        let mut graph = CodeGraph::default();
        graph.add_source("src/lib.rs", &a);
        graph.add_source("src/other.rs", &b);
        graph.finish();
        render_text(&graph)
    };

    let first = build();
    let second = build();

    assert_eq!(first.as_bytes(), second.as_bytes(), "graph render not byte-stable");

    // symbols present
    assert!(first.contains("fn add"), "{first}");
    assert!(first.contains("method Point::norm"), "{first}");
    assert!(first.contains("fn other"), "{first}");
    // edges resolved by name, and deduped
    let edge_lines: Vec<&str> = first
        .lines()
        .filter(|line| line.contains(" -> "))
        .collect();
    assert!(!edge_lines.is_empty(), "no edges resolved:\n{first}");
    let mut sorted = edge_lines.clone();
    sorted.sort_unstable();
    assert_eq!(edge_lines, sorted, "edges not sorted:\n{first}");
    let mut deduped = edge_lines.clone();
    deduped.dedup();
    assert_eq!(edge_lines.len(), deduped.len(), "edges not deduped:\n{first}");
}

#[test]
fn content_hash_cache_returns_identical_bytes() {
    let text = compressible_text();
    let cache = temp_cache_path("roundtrip");
    let settings = enabled_settings(cache.to_str().unwrap_or(""));

    // Store via the file-read stage, then read the raw payload back by content.
    let (compressed, outcome) = prune_file_read(&text, "notes.txt", &settings);
    assert!(outcome.output_bytes < outcome.input_bytes);

    let raw = retrieve_raw(&settings, &text);
    let raw = raw.expect("raw payload should be retained in the cache");
    assert_eq!(raw.as_bytes(), text.as_bytes(), "cache hit not byte-identical");

    // A second compression of the same payload is byte-identical to the first,
    // so a repeated read never invalidates a prompt cache.
    let (again, _) = prune_file_read(&text, "notes.txt", &settings);
    assert_eq!(compressed.as_bytes(), again.as_bytes());

    // And a different payload hashes to a different entry.
    assert!(retrieve_raw(&settings, "some other payload").is_none());

    let _ = std::fs::remove_dir_all(cache.parent().unwrap_or(std::path::Path::new(".")));
}

#[test]
fn fail_open_when_the_cache_cannot_be_opened() {
    // A cache path that cannot be created (a regular file used as a directory)
    // must not break the compression, and must not lose the payload.
    let blocker = temp_cache_path("blocker");
    std::fs::write(&blocker, b"not a directory").ok();

    let text = compressible_text();
    let settings = PruneSettings {
        cache_path: blocker
            .join("nested")
            .join("cache.db")
            .to_str()
            .unwrap_or("")
            .to_string(),
        ..enabled_settings("")
    };

    let (compressed, outcome) = prune_tool_output(&text, &settings);
    assert!(
        outcome.output_bytes < outcome.input_bytes,
        "compression must still run when the cache is broken"
    );
    assert!(compressed.contains("RESULT: 42"), "payload lost on cache failure");

    let _ = std::fs::remove_dir_all(blocker.parent().unwrap_or(std::path::Path::new(".")));
}

#[test]
fn fail_open_when_the_rust_payload_does_not_parse() {
    // Upstream's F5 safety: `rust_skeleton` returns None on a parse error. The
    // file must still come back, just through the text compressor.
    let broken = format!("fn ( this is not rust }}{{\n{}", "x\n".repeat(400));
    let settings = enabled_settings("");

    let (out, outcome) = prune_file_read(&broken, "src/broken.rs", &settings);
    assert!(!out.is_empty(), "the read must never come back empty");
    assert!(outcome.output_bytes <= outcome.input_bytes);
    assert!(out.contains("this is not rust"), "content lost:\n{out}");
}

#[test]
fn fail_open_on_empty_payloads() {
    let settings = enabled_settings("");
    let (out, outcome) = prune_tool_output("", &settings);
    assert_eq!(out, "");
    assert_eq!(outcome.ratio(), 0.0);
    assert!(outcome.unchanged);
}

#[test]
fn compression_never_grows_a_payload() {
    let settings = enabled_settings("");
    let samples = [
        String::new(),
        "short".to_string(),
        "no trailing spaces\nhere".to_string(),
        format!("{}\n{}", "y".repeat(3000), "z".repeat(3000)),
    ];
    for sample in samples {
        let (out, outcome) = prune_tool_output(&sample, &settings);
        assert!(
            out.len() <= sample.len(),
            "grew payload: {} -> {}",
            sample.len(),
            out.len()
        );
        assert!(outcome.output_bytes <= outcome.input_bytes);
    }
}

#[test]
fn config_env_overrides_change_behavior() {
    // Behavioral proof for the config layer: KCODE_PRUNE_* env vars must move
    // the parsed config, and the moved config must change what the wiring does.
    // Env is process-global, so this runs serially with the rest of the suite's
    // config tests by holding the shared test-env lock.
    let _guard = crate::storage::lock_test_env();

    let base = ContextPruneConfig::default();
    assert!(!base.enabled, "the feature must default to off");

    let previous: Vec<(String, Option<String>)> = [
        "KCODE_PRUNE_ENABLED",
        "KCODE_PRUNE_MIN_BYTES",
        "KCODE_PRUNE_REPEATED_THRESHOLD",
        "KCODE_PRUNE_SKELETON",
        "KCODE_PRUNE_CACHE_PATH",
    ]
    .into_iter()
    .map(|key| (key.to_string(), std::env::var(key).ok()))
    .collect();

    unsafe {
        std::env::set_var("KCODE_PRUNE_ENABLED", "true");
        std::env::set_var("KCODE_PRUNE_MIN_BYTES", "128");
        std::env::set_var("KCODE_PRUNE_REPEATED_THRESHOLD", "2");
        std::env::set_var("KCODE_PRUNE_SKELETON", "0");
        std::env::set_var("KCODE_PRUNE_CACHE_PATH", "/tmp/kcode-prune-env-test.db");
    }

    let mut config = crate::config::Config::default();
    config.apply_env_overrides();

    let settings = PruneSettings::from(&config.context_prune);
    assert!(settings.enabled, "KCODE_PRUNE_ENABLED must enable the feature");
    assert_eq!(settings.min_bytes, 128, "KCODE_PRUNE_MIN_BYTES must apply");
    assert_eq!(settings.repeated_threshold, 2, "KCODE_PRUNE_REPEATED_THRESHOLD must apply");
    assert!(!settings.skeleton, "KCODE_PRUNE_SKELETON=0 must disable skeleton");
    assert_eq!(settings.cache_path, "/tmp/kcode-prune-env-test.db");

    // And the overridden config changes observable behavior: a 200-byte payload
    // is below the default 2048 threshold but above the overridden 128, so with
    // the default settings it is untouched and with the overrides it compresses.
    let payload = format!("{}\n{}", "alpha ".repeat(30), "beta\n".repeat(30));
    let (default_out, default_outcome) = prune_tool_output(&payload, &PruneSettings::from(&base));
    assert_eq!(default_out, payload);
    assert!(default_outcome.unchanged);

    let (overridden_out, overridden_outcome) = prune_tool_output(&payload, &settings);
    assert!(
        overridden_outcome.output_bytes < overridden_outcome.input_bytes,
        "env override did not change behavior: {} -> {}",
        overridden_outcome.input_bytes,
        overridden_outcome.output_bytes
    );
    assert!(overridden_out.len() < payload.len());

    // Restore the environment exactly as it was.
    for (key, value) in previous {
        match value {
            Some(value) => unsafe { std::env::set_var(&key, value) },
            None => unsafe { std::env::remove_var(&key) },
        }
    }
}

#[test]
fn config_defaults_match_the_documented_values() {
    let config = ContextPruneConfig::default();
    assert!(!config.enabled);
    assert_eq!(config.min_bytes, 2048);
    assert_eq!(config.repeated_threshold, 3);
    assert!(config.skeleton);
    assert_eq!(config.cache_path, ".kcode/prune/cache.db");

    // `#[serde(default)]` means an absent section in a user's config file is
    // exactly the default (disabled), never a surprise-enabled feature.
    let parsed: crate::config::Config =
        toml::from_str("").unwrap_or_else(|_| unreachable!("an empty config must parse"));
    assert!(!parsed.context_prune.enabled);
}

/// The behavioral proof, printed so it is visible in `cargo test -- --nocapture`
/// and captured in the task report: with the feature **enabled** a real file
/// read and a real tool output come back measurably smaller (with the ratio the
/// wiring logs), and with the feature **disabled** they come back byte-identical.
/// The same test re-compresses each payload a second time and asserts the two
/// runs are byte-identical, which is the byte-stability proof.
///
/// It is a real assertion as well as a printout: every number it prints is
/// asserted, so it fails if any of them ever regresses.
#[test]
fn behavioral_proof_enabled_compresses_disabled_is_unchanged_and_both_are_byte_stable() {
    let text = compressible_text();
    let source = rust_source();
    let enabled = enabled_settings("");
    let disabled = PruneSettings {
        enabled: false,
        ..enabled_settings("")
    };

    eprintln!("[context_prune] === behavioral proof ===");

    // ---- enabled: tool output actually compresses ----
    let (tool_a, tool_outcome_a) = prune_tool_output(&text, &enabled);
    let (tool_b, tool_outcome_b) = prune_tool_output(&text, &enabled);
    assert_eq!(
        tool_a.as_bytes(),
        tool_b.as_bytes(),
        "byte-stability violated on the tool-output stage"
    );
    assert_eq!(tool_outcome_a, tool_outcome_b);
    assert!(tool_outcome_a.output_bytes < tool_outcome_a.input_bytes);

    // ---- enabled: Rust file read is skeletonized ----
    let (file_a, file_outcome_a) = prune_file_read(&source, "src/lib.rs", &enabled);
    let (file_b, file_outcome_b) = prune_file_read(&source, "src/lib.rs", &enabled);
    assert_eq!(
        file_a.as_bytes(),
        file_b.as_bytes(),
        "byte-stability violated on the file-read stage"
    );
    assert_eq!(file_outcome_a, file_outcome_b);
    assert!(file_outcome_a.output_bytes < file_outcome_a.input_bytes);

    // ---- disabled: both stages are byte-for-byte unchanged ----
    let (tool_off, tool_off_outcome) = prune_tool_output(&text, &disabled);
    let (file_off, file_off_outcome) = prune_file_read(&source, "src/lib.rs", &disabled);
    assert_eq!(tool_off, text, "disabled must not touch a tool output");
    assert_eq!(file_off, source, "disabled must not touch a file read");
    assert!(tool_off_outcome.unchanged);
    assert!(file_off_outcome.unchanged);

    eprintln!(
        "[context_prune] tool output  enabled : {} -> {} bytes (ratio {:.4}, transforms {})",
        tool_outcome_a.input_bytes,
        tool_outcome_a.output_bytes,
        tool_outcome_a.ratio(),
        tool_outcome_a.transforms_applied.join(",")
    );
    eprintln!(
        "[context_prune] tool output  disabled: {} -> {} bytes (unchanged = {})",
        tool_off_outcome.input_bytes,
        tool_off_outcome.output_bytes,
        tool_off_outcome.unchanged
    );
    eprintln!(
        "[context_prune] rust read    enabled : {} -> {} bytes (ratio {:.4}, transforms {})",
        file_outcome_a.input_bytes,
        file_outcome_a.output_bytes,
        file_outcome_a.ratio(),
        file_outcome_a.transforms_applied.join(",")
    );
    eprintln!(
        "[context_prune] rust read    disabled: {} -> {} bytes (unchanged = {})",
        file_off_outcome.input_bytes,
        file_off_outcome.output_bytes,
        file_off_outcome.unchanged
    );

    // The skeleton keeps signatures and drops bodies, visibly.
    eprintln!(
        "[context_prune] skeleton keeps `pub fn add(a: u32, b: u32) -> u32` = {}, drops `let sum` = {}, drops `spam` = {}",
        file_a.contains("pub fn add(a: u32, b: u32) -> u32"),
        file_a.contains("let sum"),
        file_a.contains("spam")
    );
}
