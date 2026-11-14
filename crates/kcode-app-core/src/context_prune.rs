//! Deterministic, byte-stable context compression at kcode's ingest stages.
//!
//! kcode's context is **append-only** so a message already in a provider's
//! prompt cache stays byte-identical the next time it is sent. Anything that
//! compresses content on the way into context therefore has to be *byte-stable*:
//! the same input bytes must produce the same output bytes on every run, or the
//! cache misses and the entire prefix is re-billed. The forked
//! `context-prune` library (see `vendor/context-prune/README.md`) is exactly
//! that: pure functions of the input, no model inference, no clocks, no
//! randomness, no hash-map iteration order in the output.
//!
//! This module is the single seam both ingest stages go through:
//!
//! - **file-read stage** — `crate::tool::read::ReadTool::execute` calls
//!   [`prune_file_read`] just before building the `ToolOutput`.
//! - **tool-output stage** — `crate::agent::tools::tool_output_to_content_blocks`
//!   calls [`prune_tool_output`] just before building the `ContentBlock`s.
//!
//! Both are gated on `[context_prune] enabled` (default **false**), so with the
//! feature off every byte flowing through is untouched.
//!
//! # Fail-open
//!
//! A compression path must never lose a read or a tool result. Every entry point
//! here therefore returns the original payload unchanged on any failure:
//! `rust_skeleton` returns `None` on a parse error (upstream F5 safety),
//! `CacheStore::open`/`get`/`put` return `Err` on I/O failure (degrading to
//! plain recomputation), and any unexpected condition logs a warning through
//! `crate::logging::warn` and passes the input through.
//!
//! # Byte-stability
//!
//! [`compress_bytes`] is a pure function: `(input, config) -> output`. It is
//! called at most once per payload per process because the content-hash cache
//! stores the result keyed by SHA-256 of the input, and a cache hit returns
//! exactly the bytes that were stored.

use context_prune::cache::{CacheStore, hash_bytes};
use context_prune::compress::{CompressConfig, CompressOutcome, compress_text_with};

/// What a compression pass did to one payload.
///
/// Returned to callers that report savings (the tool-output stage logs it) and
/// used by the tests as the behavioral proof: `enabled` must produce
/// `ratio() > 0` on a compressible payload, and `disabled` must produce the
/// original bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct PruneOutcome {
    /// Bytes in.
    pub input_bytes: usize,
    /// Bytes out. Equal to `input_bytes` when nothing was applied.
    pub output_bytes: usize,
    /// Which transforms ran, in upstream's order.
    pub transforms_applied: Vec<&'static str>,
    /// True when the payload came back byte-identical (no-op).
    pub unchanged: bool,
}

impl PruneOutcome {
    /// 0.0 = no savings, 1.0 = everything removed. Mirrors
    /// `context_prune::compress::CompressOutcome::ratio`.
    pub fn ratio(&self) -> f64 {
        if self.input_bytes == 0 {
            return 0.0;
        }
        1.0 - (self.output_bytes as f64 / self.input_bytes as f64)
    }
}

impl From<CompressOutcome> for PruneOutcome {
    fn from(outcome: CompressOutcome) -> Self {
        Self {
            input_bytes: outcome.input_bytes,
            output_bytes: outcome.output_bytes,
            transforms_applied: outcome.transforms_applied,
            unchanged: outcome.input_bytes == outcome.output_bytes,
        }
    }
}

/// The compression knobs, taken from `[context_prune]`.
#[derive(Debug, Clone, PartialEq)]
pub struct PruneSettings {
    pub enabled: bool,
    pub min_bytes: usize,
    pub repeated_threshold: usize,
    pub skeleton: bool,
    pub cache_path: String,
}

impl From<&crate::config::ContextPruneConfig> for PruneSettings {
    fn from(config: &crate::config::ContextPruneConfig) -> Self {
        Self {
            enabled: config.enabled,
            min_bytes: config.min_bytes,
            repeated_threshold: config.repeated_threshold,
            skeleton: config.skeleton,
            cache_path: config.cache_path.clone(),
        }
    }
}

impl PruneSettings {
    /// Upstream's `CompressConfig`, driven by kcode's config. Only
    /// `min_size`/`dedupe_threshold` are mapped: the other two upstream knobs
    /// (`compact_json_lines`, `collapse_blanks`) are meaning-preserving and
    /// always on, matching upstream's default.
    fn compress_config(&self) -> CompressConfig {
        CompressConfig {
            min_size: self.min_bytes,
            dedupe_threshold: self.repeated_threshold,
            compact_json_lines: true,
            collapse_blanks: true,
        }
    }
}

/// Compress one payload deterministically. Pure function of `(input, settings)`.
///
/// Fail-open: on any error the original payload is returned unchanged. Callers
/// that want the raw payload kept pass `retain_raw`, which stores the input in
/// the content-hash cache keyed by SHA-256 so it can be retrieved later.
pub fn compress_bytes(input: &str, settings: &PruneSettings, retain_raw: bool) -> (String, PruneOutcome) {
    let cfg = settings.compress_config();
    let (compressed, upstream_outcome) = compress_text_with(input, &cfg);
    let outcome = PruneOutcome::from(upstream_outcome);

    if retain_raw && outcome.output_bytes < outcome.input_bytes {
        store_raw(settings, input);
    }

    (compressed, outcome)
}

/// File-read stage: compress a file's rendered text before it enters context.
///
/// Rust sources go through the tree-sitter skeleton (signatures kept, bodies
/// elided) when `settings.skeleton` is on and the file parses cleanly; anything
/// else — and any Rust file the skeleton refuses — falls back to the general
/// text compressor. Fail-open: a `None` skeleton or a compression error passes
/// the original text through.
pub fn prune_file_read(
    text: &str,
    file_path: &str,
    settings: &PruneSettings,
) -> (String, PruneOutcome) {
    if !settings.enabled {
        return no_op(text);
    }
    if text.len() < settings.min_bytes {
        return no_op(text);
    }

    if settings.skeleton && is_rust_source(file_path) {
        if let Some(skeleton) = context_prune::skeleton::rust_skeleton(text) {
            if skeleton.len() < text.len() {
                store_raw(settings, text);
                let outcome = PruneOutcome {
                    input_bytes: text.len(),
                    output_bytes: skeleton.len(),
                    transforms_applied: vec!["rust_skeleton"],
                    unchanged: false,
                };
                return (skeleton, outcome);
            }
            // Skeleton did not help: fall through to the text compressor rather
            // than emitting a same-size payload.
        }
        // `None` = parse error (upstream F5 safety): fail open via the text
        // compressor, never by losing the file.
    }

    let (compressed, outcome) = compress_bytes(text, settings, true);
    (compressed, outcome)
}

/// Tool-output stage: compress a tool result before it enters context.
///
/// Same fail-open contract as [`prune_file_read`]. The raw payload is retained
/// in the content-hash cache when compression actually saved bytes, so the model
/// can still retrieve the full output.
pub fn prune_tool_output(text: &str, settings: &PruneSettings) -> (String, PruneOutcome) {
    if !settings.enabled {
        return no_op(text);
    }
    if text.len() < settings.min_bytes {
        return no_op(text);
    }
    compress_bytes(text, settings, true)
}

/// A disabled or sub-threshold pass: byte-identical output, no transforms.
fn no_op(text: &str) -> (String, PruneOutcome) {
    (
        text.to_string(),
        PruneOutcome {
            input_bytes: text.len(),
            output_bytes: text.len(),
            transforms_applied: Vec::new(),
            unchanged: true,
        },
    )
}

fn is_rust_source(file_path: &str) -> bool {
    std::path::Path::new(file_path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("rs"))
}

/// Store the raw payload in the content-hash cache so the compressed form is
/// never a dead end. Fail-open: an unwritable or unopenable cache logs a
/// warning and compression proceeds regardless.
fn store_raw(settings: &PruneSettings, input: &str) {
    if settings.cache_path.trim().is_empty() {
        return;
    }
    let Some(parent) = std::path::Path::new(&settings.cache_path).parent() else {
        return;
    };
    if !parent.as_os_str().is_empty() {
        let _ = std::fs::create_dir_all(parent);
    }
    let hash = hash_bytes(input.as_bytes());
    match CacheStore::open(&settings.cache_path) {
        Ok(store) => {
            if let Err(err) = store.put(&hash, input.len(), input.len(), input.as_bytes()) {
                crate::logging::warn(&format!(
                    "[context_prune] raw payload cache write failed (path={}): {err}",
                    settings.cache_path
                ));
            }
        }
        Err(err) => crate::logging::warn(&format!(
            "[context_prune] raw payload cache open failed (path={}): {err}",
            settings.cache_path
        )),
    }
}

/// Read a retained raw payload back out of the content-hash cache.
///
/// Returns `None` when the cache is disabled, unreadable, or has no entry for
/// this payload. The caller then keeps whatever compressed text it has.
pub fn retrieve_raw(settings: &PruneSettings, input: &str) -> Option<String> {
    if settings.cache_path.trim().is_empty() {
        return None;
    }
    let hash = hash_bytes(input.as_bytes());
    let store = CacheStore::open(&settings.cache_path).ok()?;
    let bytes = store.get(&hash).ok()??;
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests;
