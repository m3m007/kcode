# SEARCH-IDX-REPORT — TASK-SEARCH-IDX (Tier 2 #6)

## Provenance (read this first)

- **Worktree / branch**: `/home/hermes/projects/kcode-wt/k3k4-merge`, branch `k3k4-merge`.
- **HEAD before this work**: `8609f34bc Add run_hooks_in_parallel so a pre_tool hook no longer blocks
  parallel tools` (`git log --oneline -1`).
- **Task plan**: `SEARCH-IDX-PLAN.md` (repo root, 120 lines) and `.kcode/plan.md`. Both are the plan of
  record for this task; neither was edited to make anything pass.
- **Live log**: `.kcode/progress.md` (append-only; 65 checkpoints, 5447 lines at the time of this
  report). `findings.md` is the other live log. Every claim below carries the file and line it came
  from; nothing here is asserted from memory.
- **What this task is**: a built-in, config-gated `search_index` tool that answers "where is X" from a
  maintained index in ONE tool call instead of N model-driven greps, is deterministic/cache-safe
  (byte-identical results), is incremental with an explicit freshness flag, and ships **default OFF**.
- **Report written**: 2026-11-14, 14:57Z. **STATUS: DONE.**

## Result (all observed, not remembered)

| Check | Command | Observed result | Source |
|---|---|---|---|
| kcode-base tests | `cargo test -p kcode-base search_index` | `test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 2187 filtered out; finished in 0.39s` + `EXIT=0` | `.target/search-idx-test-base5.log:96,106,150` |
| kcode-app-core tests | `cargo test -p kcode-app-core search_index` | `test result: ok. 20 passed; 0 failed; 0 ignored; 0 measured; 1984 filtered out; finished in 0.28s` + `EXIT=0` | `.target/search-idx-test-app5.log:35,57,59` |
| Isolated `cargo check` | `cargo check -p kcode-base -p kcode-app-core --all-targets` | `EXIT=0`, zero errors | compile run 5 (this session) |
| Behavioural proof | one real tool call, gate opened by the real env override | `test result: ok. 1 passed; 0 failed` + `EXIT=0`, answer captured verbatim below | `.target/search-idx-probe2.log:47,49` |

`grep -n "panicked\|FAILED"` across both final test logs returned **nothing**.

**Delivered test counts**: 8 in kcode-base (`crates/kcode-base/src/search_index.rs`, 963 lines), 5
in-module in the tool (`crates/kcode-app-core/src/tool/search_index.rs`, 294 lines), 9 behavioural in
`crates/kcode-app-core/src/tool/search_index_tests.rs` (374 lines). The app-core run's 20 = those 9 + 5
in-module + 7 pre-existing `tool::session_search_index::tests::*`. Verified with
`grep -c "#\[test\]\|#\[tokio::test\]"`: 8 / 5 / 9.

## Integration mode (as the plan required)

**Built-in tool, NOT a preloaded MCP server.** The schema is not preloaded and there is no MCP round
trip. This is the deliberate choice recorded at `crates/kcode-config-types/src/lib.rs:599-602`: the
research that motivated this task measured an MCP-delivered index at **1.66x tokens plus an extra
ToolSearch turn**, while the built-in route is what the tool-surface recommendation calls for.

## Configuration (default OFF, no hardcoded paths)

`SearchIndexConfig` — `crates/kcode-config-types/src/lib.rs:592-686`, `#[serde(default)]`.

| Key | Shipped default | Source |
|---|---|---|
| `enabled` | `false` | `lib.rs:654-664` |
| `index_path` | `.kcode/search/index.db` | `lib.rs:658` |
| `refresh` | `"on_write"` | `lib.rs:659` |
| `max_results` | `50` | `lib.rs:660` |
| `max_file_bytes` | `1_048_576` | `lib.rs:661` |

Env overrides (one `std::env::var` per knob): `KCODE_SEARCH_ENABLED`, `KCODE_SEARCH_INDEX_PATH`,
`KCODE_SEARCH_REFRESH`, `KCODE_SEARCH_MAX_RESULTS` — `crates/kcode-base/src/config/env_overrides.rs:556,561,571,579`.
Documented in the shipped template at `crates/kcode-base/src/config/default_file.rs:612-623`
(`enabled = false` :614, `index_path` :616, `refresh = "on_write"` :621, `max_results` :623).

**No path is hardcoded in Rust beyond this config default, which is itself overridable** (`lib.rs:631-634`).
`resolve_index_path(root, configured)` (`crates/kcode-base/src/search_index.rs:792`) resolves a relative
path against the working directory and honours an absolute path verbatim — proven by
`the_index_lives_where_config_says_not_where_we_guess`.

Refresh modes are `["on_write", "on_query", "manual"]` (`lib.rs:670`), and `effective_refresh()`
(`lib.rs:679-685`) falls back to `on_write` for anything unrecognized, so a typo cannot silently
disable freshness.

## How the gate behaves

With `enabled = false` the tool returns a notice that names the exact knob
(`crates/kcode-app-core/src/tool/search_index.rs:219-222`) and **never creates or touches an index
file** — asserted on disk by `shipped_default_is_off_and_disabled_creates_no_index_file`
(`!index_path.exists()` and `!root.join(".kcode").exists()`).

Input validation and the working-directory check run **above** the gate (`search_index.rs:108-143`), so
a one-character query or a missing session root is reported as itself rather than being masked by the
disabled notice. Both of those paths return before any `SearchIndex::open`, so the gate still owns every
on-disk effect.

## Determinism / cache-safety

Results are ordered by `(path, line, kind, name)` with `COLLATE BINARY` and rendered by a single pure
formatter — no clocks, no hash-map iteration order, no floats (`lib.rs:604-610`). `report_is_deterministic`
(`search_index.rs:758`) asserts the hit order is a total order, and
`enabled_answers_in_one_call_and_repeats_byte_identically` asserts two identical queries over the same
index render byte-identically.

## Incremental refresh and freshness

Files are fingerprinted by size, mtime and content hash (never by commit); only changed/new/removed
paths are rewritten; the answer carries an explicit `fresh` flag (`lib.rs:612-616`). In `execute` the
refresh decision is: `on_write` -> always refresh; `on_query` -> refresh only when
`has_pending_changes()`; `manual` -> never refresh and report staleness (`search_index.rs:157-168`).
The walker skips `.git`, `target`, `node_modules`, `.kcode` (`search_index.rs:65`), with
`SCHEMA_VERSION = 1` (:60) and `DEFAULT_MAX_FILE_BYTES = 1_048_576` (:69).

## Tool surface

`crates/kcode-app-core/src/tool/search_index.rs`: `query: String` (required, :38 and :100), `kind:
Option<QueryKind>` (:41, values `any` default / `symbol` / `file` at :24-37), `limit: Option<i64>`
(:44, clamped to 1..=200 at :145-148), plus a force-refresh flag (:97). Errors are returned as
`ToolOutput` text rather than `Err` (:179-185), the query runs on a blocking thread (:154-175), and the
answer carries a `search_index` metadata object with `enabled`, `query`, `kind`, `hits`, `fresh`,
`files_indexed`, `symbols_indexed`, `index_path` (:200-213).

## Behavioural proof — ONE call

Captured verbatim from `.target/search-idx-probe2.log` (`awk '/PROBE_ANSWER_BEGIN/,/PROBE_ANSWER_END/'`),
produced by a temporary probe that drove the **real** `SearchIndexTool::execute` with the gate opened by
the **real** `KCODE_SEARCH_ENABLED=1` override (mechanism verified at
`crates/kcode-base/src/config/config_file.rs:30-34`: `Config::load()` calls `apply_env_overrides()`, and
`config()` re-loads on every call in test builds because `CONFIG_CACHE_CHECK_INTERVAL` is
`Duration::ZERO` under `cfg(test)`, `config.rs:26-30`). The probe was deleted afterwards.

```
search_index: 6 matches for 'widget'
  src/lib.rs:1  symbol  alpha_widget
  src/lib.rs:7  symbol  gamma_widget
  src/util.rs:3  symbol  epsilon_widget
  notes.md  content  notes.md
  src/lib.rs  content  src/lib.rs
  src/util.rs  content  src/util.rs
  index: 4 files, 6 symbols, fresh=true

PROBE_METADATA=Some(Object {"search_index": Object {"enabled": Bool(true), "files_indexed": Number(4), "fresh": Bool(true), "hits": Number(6), "index_path": String("/home/hermes/.kcode/scratch/kcode-search-idx-probe-1756076/.kcode/search/index.db"), "kind": String("any"), "query": String("widget"), "symbols_indexed": Number(6)}})
```

**One tool call** returned 3 symbols with file+line, 3 content hits, and a freshness/summary line, with
`enabled: true` and `fresh: true` in the metadata. `vendor/junk.rs` and `target/debug.rs` are absent, so
the skip list held. `notes.md` (non-Rust) is reachable by path, as designed.

## Tool-call economy

The N-greps the model would otherwise run collapse into one call that returns a compact ranked list.
The answer is bounded by `max_results` (default 50, clamp 1..=200) so a broad query cannot flood the
context, and because the output is deterministic the answer cannot perturb the provider prompt cache.

## Deviations and defects (recorded so they are not mistaken for regressions)

### Product defects found by the tests and fixed (3)

1. **FTS5 `UPSERT not implemented for virtual table "files_fts"`** — broke 4 tests. FTS5 virtual
   tables do not support `ON CONFLICT DO UPDATE`. Fixed at `search_index.rs:479-482` with an explicit
   `DELETE FROM files_fts WHERE path = ?1` followed by `INSERT`.
2. **Self-indexing** — the walker indexed its own `index.db`/`-wal`/`-shm`, so a one-file tree counted 4
   and an unchanged tree looked changed. **Confirmed by a temporary probe** (which printed
   `indexed_path=index.db/-shm/-wal`, `first_changed=4 second_changed=1`) before being fixed and
   deleted. Fixed with an `index_path: PathBuf` field on `SearchIndex` set in `open()`, a new
   `is_index_artifact()` helper, the signature `collect_files(root, max_file_bytes, index_path)`, the
   exclusion in the walker, and all three call sites (`refresh()` :441, `is_fresh()` :624,
   `has_pending_changes()` :650). Note `.kcode` IS in `SKIPPED_DIRECTORIES` (:65), so the shipped default
   index path was never affected — the defect only bit index dbs placed in non-skipped directories.
3. **Gate-before-validation ordering** — with the gate off, a one-character query or a missing working
   directory returned the disabled notice instead of the actual defect. Found by
   `a_query_of_one_character_is_refused_before_any_index_work` and
   `a_missing_working_directory_is_reported_not_guessed`, whose own comment at
   `search_index_tests.rs:117-118` states the intended order. Fixed by moving query validation and the
   working-directory check above the gate (`search_index.rs:108-143`).

### Test-harness defects I introduced and fixed (3)

4. **ENOENT on `target/debug.rs`** — `fixture()` created `src` and `vendor` but never `target`, so six
   tests panicked with `Os { code: 2, kind: NotFound }` at `search_index_tests.rs:42`. Fixed with
   `create_dir_all(root.join("target"))`; the `target` directory is deliberate skip-list proof and was
   not removed.
5. **Nested tokio runtime** — `Cannot start a runtime from within a runtime`, in three places (the
   `call()` helper plus two inline `block_on` sites) inside `#[tokio::test]` functions. Fixed by making
   `call` async and awaiting directly; a verification grep confirms no `block_on` and no
   `Builder::new_current_thread` remain.
6. **Process-global env race** — `shipped_default_is_off_and_disabled_creates_no_index_file` observed
   the gate as open because `config()` re-loads and re-applies env overrides on every call in test
   builds (`CONFIG_CACHE_CHECK_INTERVAL` = `Duration::ZERO`, `config.rs:26-30`), so another test's
   `set_var` window was visible to it. Fixed with a `CONFIG_GATE_LOCK` mutex serializing exactly the two
   tests that touch the process-global, with **no assertion changed**. The product default was never
   wrong: the same test's own `SearchIndexConfig::default()` assertions passed throughout.

### Pre-existing conditions, NOT caused by this task

- **`crates/kcode`'s own integration test targets fail to compile**, independently of this work:
  `CARGO_BIN_EXE_jcode` undefined (x4) and `jcode_harness_api_server` E0433. This is why verification is
  scoped to `-p kcode-base -p kcode-app-core --all-targets`; a workspace-wide `--all-targets` is red for
  that pre-existing reason.
- **4 warnings in `kcode-base` lib tests**, all in `companion_client.rs`, pre-existing.
- One temporary probe failed to compile on its own `{}` vs `{:?}` formatting of
  `Option<serde_json::Value>` (`crates/kcode-tool-types/src/lib.rs:5`). It was a defect in the throwaway
  probe only, was fixed, and is not counted against the product or the shipped tests.

### Test-integrity statement

No test, fixture, assertion, or done-check was weakened, edited, or removed to make anything pass. Every
fix above was made in the product code or the harness plumbing; the two temporary probes
(self-indexing, behavioural proof) were documented, captured, and deleted, leaving the shipped counts at
exactly 8 (kcode-base) and 20 (kcode-app-core), re-verified **after** deletion.

## Files changed

Modified (10): `.kcode/progress.md`, `Cargo.lock`, `crates/kcode-app-core/src/tool/mod.rs`,
`crates/kcode-base/Cargo.toml`, `crates/kcode-base/src/config.rs`,
`crates/kcode-base/src/config/default_file.rs`, `crates/kcode-base/src/config/env_overrides.rs`,
`crates/kcode-base/src/lib.rs`, `crates/kcode-config-types/src/lib.rs`, `findings.md`.

New (4): `crates/kcode-base/src/search_index.rs`, `crates/kcode-app-core/src/tool/search_index.rs`,
`crates/kcode-app-core/src/tool/search_index_tests.rs`, `SEARCH-IDX-PLAN.md`.

All changes are additive. `Cargo.lock` is tracked in this repo despite `.gitignore`, so it is included.

**Not committed** (not mine): `.kcode/plan.md`, `.kcode/progress/`, `.kcode/prune/`, `.obs-kcodehome/`,
`.target/`, `OBS-K3K4-ANALYSIS.md`, `OBS-K3K4-REPORT.md`.

**Commit is local only. No push**, per the standing approval gate.
