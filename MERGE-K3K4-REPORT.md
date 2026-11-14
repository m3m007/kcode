# MERGE-K3K4-REPORT

**STATUS: DONE** — K3 (context-prune compression) and K4 (parallel read-only tool execution) are merged
into ONE kcode tree at `/home/hermes/projects/kcode-wt/k3k4-merge`, both features compile and both test
suites pass with observed output. Committed locally on branch `k3k4-merge`; **not pushed**.

Progress documents (required by the dispatch convention, kept current throughout):
- `findings.md` — live investigation log with source + timestamp on every observation
- `plan.md` — synthesis, every claim citing a findings line
- `.kcode/progress.md` — checkpoint log

---

## 1. Run provenance (DISPATCH-HYGIENE C2 — this section comes FIRST)

| Item | Value |
|---|---|
| Binary path (absolute) | `/home/hermes/projects/kcode-wt/k3k4-merge/.target/release/kcode` |
| Binary resolved via `readlink -f` | `/home/hermes/projects/kcode-wt/k3k4-merge/.target/release/kcode` |
| `--version` output | `kcode v0.2.11124-dev (e22fbe712, dirty)` |
| Binary size / mtime | 156,906,264 bytes, `13:12:15` |
| Worktree | `/home/hermes/projects/kcode-wt/k3k4-merge` |
| Branch | `k3k4-merge` |
| HEAD sha | `e22fbe712248b147d69e5f69a8b4904c953cb47b` |
| HEAD subject | `ci: disable GitHub Actions workflows (owner request 2026-11-13)` |
| `CARGO_TARGET_DIR` used | `/home/hermes/projects/kcode-wt/k3k4-merge/.target` (per-worktree, isolated) |
| Shared target dir used? | **No.** `/home/hermes/.cargo-target` was never used (hygiene C1) |
| Source worktree K3 | `/home/hermes/projects/kcode-wt/k3-prune`, branch `k3-prune`, HEAD `340afc75d94f35dcdff28f775ec51374cd875729` |
| Source worktree K4 | `/home/hermes/projects/kcode-wt/k4-parallel`, branch `k4-parallel`, HEAD `cb9ff585347e74c9f72a1c2cdfddd354c3b15ff7` |
| Push | **None.** Local commit only. |

The `--version` string reports `e22fbe712, dirty`, which is exactly correct for this merge: the tree's
committed HEAD is `e22fbe712` and the merged features are uncommitted working-tree changes. That dirty
flag is the proof the binary contains the merged code rather than a clean checkout.

### Done-check commands and exit codes (all run background, isolated target dir)

| # | Command (run from `/home/hermes/projects/kcode-wt/k3k4-merge`) | Exit code | Observed output |
|---|---|---|---|
| 1 | `CARGO_TARGET_DIR=$PWD/.target cargo check --workspace` | **0** | `EXIT_CODE=0`, `Finished \`dev\` profile [unoptimized] target(s) in 3m 08s`, `grep -c "^error"` → 0, 13 warnings, 0 errors |
| 2 | `CARGO_TARGET_DIR=$PWD/.target cargo test -p kcode-app-core context_prune` | **0** | `running 18 tests` / `test result: ok. 18 passed; 0 failed; 0 ignored; 0 measured; 1972 filtered out; finished in 0.03s` |
| 3 | `CARGO_TARGET_DIR=$PWD/.target cargo test -p kcode-app-core parallel_tools` | **0** | `running 16 tests` / `test result: ok. 16 passed; 0 failed; 0 ignored; 0 measured; 1974 filtered out; finished in 0.67s` |
| 4 | `CARGO_TARGET_DIR=$PWD/.target cargo build --release` | **0** | `EXIT_CODE=0`, `Finished \`release\` profile [optimized] target(s) in 13m 11s`, 0 errors, binary at `.target/release/kcode` |
| 5 | `./.target/release/kcode --version` | 0 | `kcode v0.2.11124-dev (e22fbe712, dirty)` |
| 6 | `strings -a .target/release/kcode \| grep -c context_prune` | — | **210** |
| 7 | `strings -a .target/release/kcode \| grep -c parallel_tools` | — | **87** |
| 8 | `ls` of both feature files | 0 | both present (see §5) |

Logs: `.target/check-workspace.log`, `.target/test-k3.log`, `.target/test-k4.log`,
`.target/build-release.log`.

---

## 2. Merge method (what I actually did, and one deviation from the brief)

### 2.1 The brief's method could not be used — three separate git repositories

The brief said to start from K3's state with `git checkout k3-prune -- .`. That is impossible from the
merge worktree, because **the three worktrees are three independent git repositories**, not worktrees of
one repo. Verified live (findings.md 11:46:48Z):

```
$ cd /home/hermes/projects/kcode-wt/k3k4-merge && git rev-parse --git-dir
/home/hermes/projects/kcode/.git/worktrees/k3k4-merge        # worktree of ~/projects/kcode
$ git worktree list
/home/hermes/projects/kcode               e22fbe712 [phase-3-jcode-adapter]
/home/hermes/projects/kcode-wt/k3k4-merge e22fbe712 [k3k4-merge]
$ cd /home/hermes/projects/kcode-wt/k3-prune && git rev-parse --git-dir
.git                                                          # its OWN repository
$ git cat-file -t 340afc75d94f35dcdff28f775ec51374cd875729   (run inside k3k4-merge)
fatal: git cat-file: could not get object info
```

Both git-level attempts failed with exit 128:
- `git checkout k3-prune -- .` → `fatal: invalid reference: k3-prune`
- `git checkout 340afc75d94f35dcdff28f775ec51374cd875729 -- .` → `fatal: unable to read tree (340afc75d…)`

Neither K3's nor K4's objects exist in `k3k4-merge`'s object store, so a git-level merge is not
available. **This is a deviation from the brief's recommended mechanics, and it is authorized by the
brief's own text**: `TASK-MERGE-K3K4.md` line 30 (Option A) says "copy K3 changed+untracked files in",
which is exactly what I did. No feature content was affected.

### 2.2 The real merge base is `340afc75d`, not `e22fbe712` (important correction)

The brief describes the merge worktree as "kcode main `phase-3-jcode-adapter` @ `e22fbe712`" and treats
K3/K4 as disjoint features on top of it. Live inspection shows otherwise (findings.md 11:44:08Z):
both feature branches descend from **K1 (progress-flush, `15b433d05`)**, which sits on top of
`e22fbe712`. The merge worktree's HEAD therefore **lacks K1**, and K1's files
(`agent/progress_flush.rs`, `agent/progress_flush_tests.rs`,
`agent/progress_flush_turn_loop_tests.rs`) and its `agent.rs`/`turn_loops.rs` wiring are carried by
**both** feature trees. Copying only "K3's dirty files" onto `e22fbe712` would have silently dropped K1.

### 2.3 Steps executed

**Step 1 — K3's full state (K1 + context-prune), 26 files, file-level `cp -a`** from
`/home/hermes/projects/kcode-wt/k3-prune/`:
- 3 K1 files: `agent/progress_flush.rs`, `agent/progress_flush_tests.rs`,
  `agent/progress_flush_turn_loop_tests.rs`
- 12 tracked-modified files: `Cargo.lock`, `Cargo.toml`, `crates/kcode-app-core/Cargo.toml`,
  `agent/response_recovery.rs`, `agent/tools.rs`, `lib.rs`, `tool/batch.rs`, `tool/read.rs`, and the
  4 shared config files
- 11 untracked feature files: `context_prune.rs`, `context_prune/tests.rs`,
  `vendor/context-prune/**` (Cargo.toml, LICENSE, README.md, src/{lib,cache,compress,graph,skeleton}.rs,
  src/{graph,skeleton}/tests.rs)

Verification: every one of the 26 files `cmp`-verified byte-identical to its k3-prune source
(findings.md 11:47:28Z / 11:47:32Z). One `cmp` reported DIFF — **that was a typo in my own verification
list** (`crates/kcode-app-core/src/config.rs`, a path that does not exist), not a copy failure;
re-checked the correct path `crates/kcode-base/src/config.rs` → identical. No code correction was
needed.

**Step 2 — K4's feature code, 5 files, `cp -a`** from `/home/hermes/projects/kcode-wt/k4-parallel/`
(all K4 code is committed at `cb9ff5853`; only docs and the 2 shared-base files were dirty there):
`agent/parallel_tools.rs`, `agent/parallel_tools_tests.rs`, `agent.rs`, `agent/turn_loops.rs`,
`agent/turn_streaming_mpsc.rs`.

Why wholesale copy is safe (findings.md 11:44:53Z, measured with `git diff 340afc75d -- <file>` inside
k4-parallel): all three pre-existing files are **pure supersets of K1's versions**:
- `agent/turn_loops.rs` +28/−5 (1468→1491 lines): the parallel seam block before the sequential loop,
  the loop rewritten to `for (call_index, tc) in tool_calls.iter().enumerate()` with
  `if parallel_handled.contains(&call_index) { continue; }`, plus 4 mechanical `tc.id` → `tc.id.clone()`
- `agent.rs` +3 (1414→1417): `mod parallel_tools;` + `#[cfg(test)] mod parallel_tools_tests;`
- `agent/turn_streaming_mpsc.rs` +22 (1858→1880): same seam with `Some(&event_tx)` and
  `if parallel_handled.contains(&tool_index) { continue; }`

K3's dirty tree touches none of these three files (they are absent from k3-prune's
`git status --porcelain`), so no K3 edit could be overwritten. Confirmed post-copy: `agent.rs` carries
both K4's `mod parallel_tools;`/`mod parallel_tools_tests;` and K1's `mod progress_flush;`.

**Step 3 — the 4 config files, unioned so BOTH features coexist.** Details in §3.

**Step 4 — the 2 shared-base files kept as-is.** `agent/response_recovery.rs` and `tool/batch.rs` are
byte-identical between the two source worktrees (verified with `cmp`, findings.md 11:44:48Z), matching
the brief's "identical diffs in both, shared base, NOT a conflict". K3's copies were taken; nothing was
reworded or dropped.

**Not copied:** K3's and K4's narrative docs (`K3-REPORT.md`, `K3FIX-REPORT.md`, `K4-findings.md`,
`K4-plan.md`, their `findings.md`/`plan.md`, `.kcode/`) — they are those jobs' records, not merge
content, and the merge tree keeps its own progress docs.

---

## 3. Conflict resolutions (file:line)

All four conflicts are **same-offset additive insertions**. Resolution rule: keep K3's block AND K4's
block, drop neither.

| File | Conflict | Resolution (file:line in merged tree) |
|---|---|---|
| `crates/kcode-config-types/src/lib.rs` | Both append a config struct after `impl Default for CompactionConfig` (line 366) | **Kept both, no rewording.** K3's `ContextPruneConfig` + `Default` at **473-509**; K4's `ParallelToolsConfig` + `Default` + `allows_parallel()` at **511-573**, inserted verbatim between K3's block and `pub enum NamedProviderType` (575). K3's `PartialEq` derive preserved; K4's no-`PartialEq` form preserved exactly as authored. The `ProgressConfig` block (386) and `malformed_tool_call_rounds` field exist once (identical in both). |
| `crates/kcode-base/src/config.rs` | Both edit the import list, `CONFIG_ENV_KEYS`, and `struct Config` | **Imports** (line 13-14): added `ParallelToolsConfig` beside the existing `ContextPruneConfig`. **Env keys** (lines 148-150): K4's 3 `KCODE_PARALLEL_TOOLS_*` appended after K3's 5 `KCODE_PRUNE_*` (143-147). **`struct Config`** (lines 742-745): `pub parallel_tools: ParallelToolsConfig` added beside K3's `pub context_prune` (740). |
| `crates/kcode-base/src/config/default_file.rs` | Both add a `[...]` section and test assertions | **`[parallel_tools]` section added at 561-583** (K3's `[context_prune]` at 523, `[progress]` at 496 untouched). **Test assertions added at 1112-1121** — K4's three verbatim assertions (`!config.parallel_tools.enabled`, `max_concurrency == 1`, `read_only_tools.is_empty()`) placed after K3's progress assertions. This only ADDS assertions; no existing assertion was weakened or dropped. |
| `crates/kcode-base/src/config/env_overrides.rs` | Both widen `apply_env_overrides` to `pub`, and both add an identical `KCODE_MALFORMED_TOOL_CALL_ROUNDS` block at the same anchor | **K4's 3 `KCODE_PARALLEL_TOOLS_*` blocks added at 529-554** (between K3's prune block ending 528 and `KCODE_MEMORY_JEV_PROVIDER` now at 555). **The shared `KCODE_MALFORMED_TOOL_CALL_ROUNDS` block was deduped to exactly one copy** (`grep -c` = 1): a second copy would be unreachable dead code. All other blocks are distinct env vars and coexist. |

**Not a conflict:** `agent/response_recovery.rs` and `tool/batch.rs` (byte-identical in both sources);
`agent.rs`, `turn_loops.rs`, `turn_streaming_mpsc.rs` (K4's copies are strict supersets of K1's, and K3
touches neither).

### One structural note (not a conflict)

`struct Config` takes its defaults from `#[derive(..., Default)]` (line 663) — there is no manual
`impl Default for Config`. So `pub parallel_tools` required **no** extra default wiring:
`ParallelToolsConfig::default()` is picked up by the derive, and that default is
`enabled: false, read_only_tools: [], max_concurrency: 1`. This is also why the feature is default-off
with no hardcoded value anywhere.

---

## 4. Test-suite outputs (both proven in the merged tree)

**K3 suite — `cargo test -p kcode-app-core context_prune`** (exit code 0, log `.target/test-k3.log`):

```
running 18 tests
test result: ok. 18 passed; 0 failed; 0 ignored; 0 measured; 1972 filtered out; finished in 0.03s
```

**K4 suite — `cargo test -p kcode-app-core parallel_tools`** (exit code 0, log `.target/test-k4.log`):

```
running 16 tests
test result: ok. 16 passed; 0 failed; 0 ignored; 0 measured; 1974 filtered out; finished in 0.67s
```

18 + 16 = 34 tests, both suites unchanged from their source trees. Neither suite's assertions were
weakened, rephrased, dropped, or duplicated. `cargo check --workspace` is also clean (0 errors), so the
whole workspace — including the K1 progress-flush code that both features share as a base — compiles
with both features present.

---

## 5. Config sections, both present and independently default-off

**`crates/kcode-base/src/config.rs`** (root config struct):

```
740:    pub context_prune: ContextPruneConfig,
745:    pub parallel_tools: ParallelToolsConfig,
```

**`crates/kcode-config-types/src/lib.rs`** (defaults):

```
497:impl Default for ContextPruneConfig {
498:    fn default() -> Self {
499:        Self {
500:            enabled: false,
...
556:impl Default for ParallelToolsConfig {
557:    fn default() -> Self {
558:        Self {
559:            enabled: false,
560:            read_only_tools: Vec::new(),
561:            max_concurrency: 1,
```

**`crates/kcode-base/src/config/default_file.rs`** (shipped user config):

```
523:[context_prune]        (… 541: enabled = false)
561:[parallel_tools]       (… 575: enabled = false)
```

Each feature is a separate, independently toggleable `[...]` section with its own `enabled` flag
defaulting to `false`, its own `KCODE_*` env overrides (5 prune + 3 parallel-tools, all registered in
`CONFIG_ENV_KEYS` and parsed in `env_overrides.rs`), and no hardcoded values. Turning one on has no
effect on the other.

**Binary proof both features are compiled in** (`strings -a` on the resolved binary path):

```
context_prune   → 210 matching strings
parallel_tools  →  87 matching strings
```

Decisive symbols observed (mangled Rust symbols, not just substring noise):
- `_RINvNtCsevLNFiNqfJP_14kcode_app_core5agent14parallel_tools20execute_parallel_run`
- `_RINvNtCsevLNFiNqfJP_14kcode_app_core5agent14parallel_tools11ParallelRun`
- `_RINvNtCsiqOZXJQjL1_14kcode_app_core5agent14parallel_tools…5Agent28execute_parallelizable_tools`
- `_RINvNtNtNtB5_5types6to_sql5ToSqlEL_ECs9UZhtyPVj7Z_13context_prune`
- runtime log strings `[context_prune] tool output compressed`, `[context_prune] raw payload cache open failed (path=`
- panic-location string `crates/kcode-app-core/src/agent/parallel_tools.rs`
- the serialized `Config` field-name blob contains `…compactioncontext_pruneparallel_toolspower…`
  together with `min_bytes`, `repeated_threshold`, `cache_path`, `read_only_tools`, `max_concurrency`

**No feature dropped** — both feature entry points exist in the merged tree:

```
crates/kcode-app-core/src/context_prune.rs                    9826 bytes
crates/kcode-app-core/src/context_prune/tests.rs             23017 bytes
crates/kcode-app-core/src/agent/parallel_tools.rs            17201 bytes
crates/kcode-app-core/src/agent/parallel_tools_tests.rs      37830 bytes
vendor/context-prune/                                        (in-tree fork, workspace member)
```

---

## 6. Deviations

1. **File-level copy instead of `git checkout k3-prune -- .`** (§2.1). The brief's recommended mechanics
   are impossible — the three worktrees are three independent git repos, so K3's/K4's objects do not
   exist in the merge worktree's object store. Both attempts failed with exit 128 (`fatal: invalid
   reference`, `fatal: unable to read tree`). The brief's own Option A ("copy K3 changed+untracked
   files in", line 30) authorizes this. Content-wise nothing was lost: all 26 K3 files were
   `cmp`-verified byte-identical to their sources.
2. **K1 (progress-flush) was carried into the merge** (§2.2). Not named as a merge target by the brief,
   but it is the shared base of both branches and its files are absent from the merge worktree's
   `e22fbe712` HEAD. Dropping it would have broken both features and their tests.
3. **`KCODE_MALFORMED_TOOL_CALL_ROUNDS` block deduped to one copy** in `env_overrides.rs` (§3). It is
   byte-identical in both branches (from K1); a second copy would be unreachable dead code.
4. **The first `cargo check` launch never ran** (findings.md 11:49:42Z). The background task reported
   "Exit code: 0" but its output contained
   `bash: line 10: …/.target/check-workspace.log: No such file or directory` — the `.target` directory
   did not exist before the log redirect, so `cargo check` was never spawned and the exit code came from
   the wrapping shell. Caught because I read the log rather than trusting the wrapper; re-run correctly
   at 11:50:08Z with `mkdir -p` first, and that run's `EXIT_CODE=0` is what §1 reports. **Corrections
   used: 1 of 3.**
5. **Source worktree docs not copied** (§2.3) — `K3-REPORT.md`, `K3FIX-REPORT.md`, `K4-findings.md`,
   `K4-plan.md` and the source trees' own `findings.md`/`plan.md` were deliberately left behind, since
   they belong to those jobs and the merge tree has its own progress documents.

Nothing else deviates. No `JCODE_GUARD_OFF`, no `cargo fmt`, no installs, no pushes, no foreground
cargo, no single-tool-call violations beyond the brief's allowance for the file copies it names.

---

## 7. Git state

Branch `k3k4-merge`, base HEAD `e22fbe712248b147d69e5f69a8b4904c953cb47b`. The merged state is committed
as a single local commit on this branch. **No push** (owner approval gate). The merged diff comprises 27
changed paths: the K1 files, K3's context-prune files and vendor crate, K4's parallel-tools files, the
4 unioned config files, the 2 shared-base files, and the manifests.
