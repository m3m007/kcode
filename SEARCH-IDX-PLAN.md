# SEARCH-IDX plan.md — built-in `search_index` tool for kcode (TASK-SEARCH-IDX, Tier 2 #6)

Every claim below cites `findings.md` (S-nn sections) or a live file:line. Provenance block is in
`findings.md` and will be repeated FIRST in `SEARCH-IDX-REPORT.md`.

## Contract

- **Goal**: a built-in `search_index` tool that answers "where is X" (file and symbol location)
  from a maintained index in **one** tool call, instead of N model-driven greps.
- **Constraints**: config-gated `[search_index]` with `enabled = false` default plus
  `KCODE_SEARCH_*` env; no hardcoded paths; deterministic byte-stable sorted output; incremental
  refresh with a freshness flag; cache-safe (a repeated query must return identical bytes so the
  provider prompt cache is never perturbed by ordering jitter); tests added, never weakened;
  isolated `CARGO_TARGET_DIR=$PWD/.target`; background builds only; no `cargo fmt`, no
  `JCODE_GUARD_OFF`, no push; single tool calls.
- **Done-check** (exits 0 or non-zero, run by me in the worktree):
  1. `cargo check --workspace` → exit 0.
  2. New tests pass: correct location, byte-identical across 2 runs, disabled = no-op.
  3. Behavioral proof: enabled → a real query returns a compact ranked list in ONE call.

## Decision already made (findings.md S-06)

**Hand-rolled, kcode-owned SQLite FTS5 + tree-sitter index modeled on reflex-search's `meta.db`
design. reflex-search is NOT linked.** Rationale is in `findings.md` S-01/S-02/S-03/S-06 —
short form: reflex resolves (v2.3.1) but drags ~390 crates including a second `ratatui` (kcode
vendors a patched `ratatui-kcode`) and a second HTTP/TUI stack, so it does not "integrate
cleanly"; the research's measured win is reflex's CLI/plugin route (0.82x-0.96x) while its MCP
route is 1.66x *worse*, so the library route is an unmeasured third mode; and every primitive
needed (`rusqlite` bundled with FTS5, tree-sitter) is already in the workspace.

## Design (modeled on the proven contracts, not on new invention)

| Contract | Source | How it is implemented here |
|---|---|---|
| `meta.db` = SQLite holding file metadata + symbol cache | research "Determinism and cache implications" | one SQLite DB at the configured `index_path` |
| Freshness by size + mtime + content hash, not by commit; `can_trust_results` flag | same | `files` table with `size`, `mtime_ns`, `content_hash`; a `stale` flag surfaced in the tool output |
| Incremental refresh on write, not on a timer | research Q1.4 item 5 | `refresh()` re-walks and upserts only changed/new/removed paths |
| Byte-identical results for the same query; stable ordering by path and line | research + codegraph-rust contract | ORDER BY is `(path, line, kind, name)` with `COLLATE BINARY`, and the rendered text is produced by a single `format!`-based function with no clocks, no hash-map iteration, no floats |
| No model calls, no embeddings, canonical output guarded by byte-stable fixtures | codegraph-rust | the index and the renderer are pure functions of tree state; the determinism test asserts byte equality across runs |
| Tool surface of 1-2 calls returning a compact ranked list | research Q1.4 item 3 (reflex's `list_locations` / `find_references` split) | `search_index` takes `query` + optional `kind` (`file`/`symbol`) + `limit`, and returns one compact block |

**Why FTS5 rather than trigram**: `rusqlite` is already a dependency with `bundled`
(`crates/kcode-base/Cargo.toml:47`), and SQLite's bundled source includes FTS5. Reflex's trigram
index is a *storage format* choice, not the contract; the contract is "one call answers where X
is, deterministically". FTS5 gives substring/prefix matching plus SQL-level deterministic ordering
with zero new crates.

**Why the index backend lives in `kcode-base`, not `kcode-app-core`**: `rusqlite` is already a
`kcode-base` dependency (`crates/kcode-base/Cargo.toml:47`) while `kcode-app-core` has no SQLite
dependency of its own, and `kcode-app-core` already depends on `kcode-base`. Tree-sitter is already
in the graph at `tree-sitter = "0.25"` / `tree-sitter-rust = "0.23"`
(`crates/kcode-enforcement/Cargo.toml:17-18`, `crates/kcode-swarm/Cargo.toml:18-19`), and the
parsing idiom is proven at `crates/kcode-enforcement/src/ast_guard.rs:107-120`. So the index module
goes in `kcode-base` (which owns storage elsewhere) and the thin tool wrapper stays in
`kcode-app-core/src/tool/search_index.rs` where every other tool lives.

**Symbol extraction scope**: Rust first, via `tree-sitter-rust` queries modeled on
`ast_guard.rs` (`function_item`, `struct_item`, `enum_item`, `trait_item`, `impl` blocks with their
type, `const`/`static`/`type` items). Non-Rust files are indexed for **path and content** (FTS5
over the file body) so "where is this string" still answers in one call, but symbol extraction is
Rust-only in this pass and that limitation is stated in the tool description and the report rather
than faked.

## Files to change (all additive)

1. `crates/kcode-config-types/src/lib.rs` — add `SearchIndexConfig` next to `ParallelToolsConfig`
   (~line 595), `#[serde(default)]`, `Default { enabled: false, index_path: ".kcode/search/index.db",
   refresh: "on_write" }`, env names in the doc comment.
2. `crates/kcode-base/src/config.rs` — `pub use kcode_config_types::{… SearchIndexConfig …}` (line 6
   block) and `pub search_index: SearchIndexConfig` next to `pub parallel_tools` (line 745).
3. `crates/kcode-base/src/config/env_overrides.rs` — `KCODE_SEARCH_ENABLED` (via `parse_env_bool`),
   `KCODE_SEARCH_INDEX_PATH` (trimmed, empty restores default), `KCODE_SEARCH_REFRESH` (validated
   against `on_write`/`on_query`/`manual`, invalid ignored rather than panicking).
4. `crates/kcode-base/src/config/default_file.rs` — `[search_index]` template block after
   `[parallel_tools]` (line ~591) with the env-override comment, plus **additive** assertions in
   the existing default-config test after line 1134 (no existing assertion touched).
5. `crates/kcode-base/src/search_index.rs` (new) — the index: schema, `open_or_create`,
   `refresh` (incremental, respects ignore rules), `query` (deterministic), `stats`/freshness.
6. `crates/kcode-base/src/lib.rs` — `pub mod search_index;`
7. `crates/kcode-app-core/src/tool/search_index.rs` (new) — `SearchIndexTool` implementing `Tool`
   per `session_search.rs:291`; config gate read as `crate::config::config().search_index`; when
   disabled returns the disabled notice **without** creating or touching an index.
8. `crates/kcode-app-core/src/tool/mod.rs` — `mod search_index;` (near line 48) and
   `Self::insert_tool_timed(&mut m, &mut timings, "search_index", search_index::SearchIndexTool::new);`
   (near line 487).
9. `crates/kcode-app-core/src/tool/search_index_tests.rs` (new) + `mod search_index_tests;` — the
   three required proofs plus a freshness/refresh test.
10. `crates/kcode-app-core/Cargo.toml` — no change expected (kcode-base is already a dependency).

## Test plan (additive; nothing existing is modified)

| Test | Proves |
|---|---|
| `index_answers_symbol_location` | build an index over a temp fixture tree, query a known symbol, assert the exact `path:line` and that the path is the fixture file |
| `same_query_returns_byte_identical_results_across_runs` | run the query twice, assert `a == b` byte-for-byte; then drop and rebuild the index and assert the rendered bytes are identical again |
| `disabled_by_default_is_a_no_op` | default config → `search_index.enabled == false` (mirrors `default_file.rs:1125-1134`); executing the tool with a disabled config returns the disabled notice and creates no index file on disk |
| `refresh_is_incremental_and_marks_staleness` | index a tree, modify one file, refresh, assert only that file's row changed and the fresh flag is true; then mutate behind the index's back and assert the freshness flag reports stale |
| `env_overrides_enable_the_tool` | `KCODE_SEARCH_ENABLED=1` flips the gate (same idiom as `env_overrides.rs:533`) |

## Risks and contingencies

- `blake3` availability in the kcode graph is inferred from the reflex resolution list (findings.md
  open item). Contingency: content hashing uses a kcode-internal hash; no crate is added either way.
- tree-sitter-rust query text is the only nontrivial new logic. If a query fails to compile at
  runtime the index falls back to path+content indexing for that file and records the failure in
  the freshness/stats surface, never a hard error (fail-open, matching `[context_prune]`'s
  documented behavior).
- Ignore rules: use the `ignore` crate if it is already reachable from `kcode-base`; otherwise walk
  with an explicit skip list for `.git`, `target`, `node_modules` and honor `.gitignore` minimally.
  Confirmed at implementation time; if `ignore` is not already a `kcode-base` dependency, no new
  crate is added and the skip list is used instead.

## Out of scope (stated so the report is honest)

- Live model-observed turn counts. The behavioral proof is the tool implementation returning a
  compact answer from one call against a real tree; the research's turn/token ratios are cited as
  prior measurement, not re-measured here.
- Non-Rust symbol extraction (path+content indexing still covers those files).
- A watcher daemon: refresh is on write (tool-call time) and on query when stale, per the research
  requirement, not a filesystem watcher.
