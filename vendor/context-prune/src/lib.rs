//! In-tree kcode fork of [`context-prune`](https://github.com/fuleinist/context-prune) 0.4.0
//! (MIT, Copyright (c) 2026 fuleinist; see `LICENSE`).
//!
//! Upstream ships a **proxy binary** (`src/main.rs`, `src/proxy.rs`) and no library
//! target: `Cargo.toml` sets `autolib = false` and declares only `[[bin]]`. kcode
//! needs the deterministic compression library in-process — byte-stable, so a
//! prompt cache built from a compressed payload is never invalidated — so this
//! fork adds a library target that re-exports the four upstream modules
//! unchanged:
//!
//! | module | what it does |
//! |--------|--------------|
//! | [`compress`] | deterministic text compression: JSON compact reserialize, ANSI strip, CR-overwrite collapse, repeated-line dedupe, blank-run collapse, per-line JSON compaction |
//! | [`skeleton`] | tree-sitter Rust skeleton: keeps signatures, drops function/field/enum bodies (None on parse error) |
//! | [`graph`] | code graph: sorted, deduped symbol + call map over Rust sources |
//! | [`cache`] | content-hash (SHA-256) cache of compressed blobs in SQLite |
//!
//! Every transform is a pure function of its input: no clocks, no randomness, no
//! hash-map iteration order leaking into output. That is what makes the output
//! byte-stable, which is the property the caller relies on.
//!
//! ## Divergences from upstream 0.4.0
//!
//! 1. Added this file (`src/lib.rs`) plus a `[lib]` section in `Cargo.toml`.
//!    No upstream module was edited.
//! 2. Dropped `main.rs` (bin `context-prune`), `proxy.rs`, `profiles.rs`,
//!    `stats.rs` and their dependencies (axum, bytes, clap, futures-util,
//!    reqwest, tokio, tokio-util). kcode consumes the library, not the proxy.
//! 3. `tree-sitter-rust` 0.24 replaces upstream's 0.23-era resolution where the
//!    workspace lock already pins a 0.24 line (see findings: the workspace
//!    `Cargo.lock` has tree-sitter 0.25.10 and tree-sitter-rust 0.24.2 in the
//!    registry cache, and tree-sitter-rust 0.23.3 is the older pin used by
//!    `kcode-embedding`/`kcode-enforcement`).
//!
//! The upstream crate's own tests (`src/graph/tests.rs`,
//! `src/skeleton/tests.rs`, and the `#[cfg(test)]` modules inside `compress.rs`
//! and `cache.rs`) travel with the modules and still run under `cargo test -p
//! context-prune`.
//!
//! ## Fail-open contract
//!
//! Nothing in this crate can fail a caller's read or tool result: every entry
//! point either returns a value or signals "pass the original through"
//! (`rust_skeleton` returns `None`, `CacheStore` returns `Err`, which callers
//! treat as "recompute"). See `kcode_app_core::context_prune` for the wiring.

#![forbid(unsafe_code)]

pub mod cache;
pub mod compress;
pub mod graph;
pub mod skeleton;
