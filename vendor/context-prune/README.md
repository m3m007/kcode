# context-prune (kcode fork)

In-tree fork of [`fuleinist/context-prune`](https://github.com/fuleinist/context-prune)
**v0.4.0** — MIT, Copyright (c) 2026 fuleinist. The unmodified upstream
`LICENSE` is at `./LICENSE`. This crate is not published; it exists so kcode
can consume the compression library as a path dependency.

## Why a fork

Upstream is a standalone **PTY proxy** binary (`context-prune` sits between a
coding agent and the LLM API). kcode owner decision (2026-01-14) requires the
compression to run **in-process**, at kcode's file-read and tool-output stages,
**config-driven and byte-stable**: identical content must produce byte-identical
output every time or a prompt cache built from that payload is invalidated on
every turn. Upstream's transforms are already deterministic (no model
inference, no clocks, no randomness), which is exactly what that needs; what
upstream lacks is a library target.

So this fork adds `src/lib.rs` + `[lib]` and drops the proxy.

## What changed vs upstream 0.4.0

| upstream | here | why |
|---|---|---|
| no `src/lib.rs`, `autolib=false`, one `[[bin]]` | `[lib]` + `src/lib.rs` exposing `pub mod compress; pub mod skeleton; pub mod graph; pub mod cache;` | kcode needs the library |
| `main.rs` (bin `context-prune`), `proxy.rs`, `profiles.rs`, `stats.rs` | not copied | the proxy is a separate process; kcode wires the library in-process |
| axum, bytes, clap, futures-util, reqwest, tokio, tokio-util deps | removed | only needed by the proxy/bin |
| `tree-sitter-rust` 0.24 | kept 0.24 | the workspace already resolves tree-sitter 0.25.10 + tree-sitter-rust 0.24.2 |

Nothing in the four library modules was edited. They are byte-identical to
upstream 0.4.0, so upstream's own tests still run here (`cargo test -p
context-prune`): `src/compress.rs` and `src/cache.rs` carry inline `#[cfg(test)]`
modules, `src/graph/tests.rs` and `src/skeleton/tests.rs` are the upstream test
files.

## Exposed API (upstream signatures, unchanged)

- `compress::compress_text(&str) -> (String, CompressOutcome)`
- `compress::compress_text_with(&str, &CompressConfig) -> (String, CompressOutcome)`
- `compress::{strip_ansi, collapse_repeated_lines, collapse_blank_runs, compact_json_lines, compress_json_value}`
- `compress::{CompressOutcome::ratio(), CompressConfig{min_size, dedupe_threshold, compact_json_lines, collapse_blanks}}`
- `skeleton::rust_skeleton(&str) -> Option<String>`
- `graph::CodeGraph::{add_source, finish}`, `graph::{render_text, render_json, collect_rust_files}`
- `cache::CacheStore::{open, get, put, entries}`, `cache::hash_bytes(&[u8]) -> String`

## Guarantees kcode relies on

- **Byte-stable.** Pure functions of the input; sorted/deduped graph output;
  SHA-256 content-hash cache keys. Same input, same output bytes, every run.
- **Fail-open.** `rust_skeleton` returns `None` on a parse error and
  `CacheStore` returns `Err` on I/O failure; callers pass the original payload
  through unchanged in both cases. `compress_text` can only ever shrink or
  keep a string, never grow it.
- **No panic-prone API.** kcode's guardrails (`scripts/check_panic_budget.py`)
  forbid `.unwrap()`/`.expect()`/`panic!` in new production files, so this fork
  keeps upstream's production code free of them. Upstream's *test* modules do
  use `unwrap()`/`expect()`; those live in `#[cfg(test)]` code and in files the
  scanner classifies as tests.
