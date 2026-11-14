# plan.md — K3+K4 merge execution plan

> **This plan grows from `findings.md` only.** Every claim cites a findings line or a source you read.
> Do not start planning before the findings justify it. If a finding later supersedes a plan
> assumption, update the plan in the same edit and note why.

## 1. Objective (from `tasks/TASK-MERGE-K3K4.md`)

Produce one kcode tree at `/home/hermes/projects/kcode-wt/k3k4-merge` containing **both** K3
(context-prune compression) and K4 (parallel read-only tool execution), each independently
config-gated and default-off, with both test suites passing unchanged (18 + 16). Build and verify in an
isolated target dir (`$PWD/.target`, DISPATCH-HYGIENE C1).

## 2. Why the merge base is `340afc75d`, not `e22fbe712`

Findings 11:44:08Z: `git log --oneline -6` in both worktrees shows `e22fbe712` (main) → `15b433d05`
(K1 progress-flush) → `340afc75d` (k3-prune tip) and → `4397a44c6` → `c41475362` (K4) → `cb9ff5853`.
The merge worktree starts at `e22fbe712`, which **lacks K1**. Both features sit on top of K1. Therefore
step 1 must bring in K1 too, i.e. K3's *whole* tree at `340afc75d` plus K3's uncommitted context-prune
work — not merely "K3's dirty files" on top of `e22fbe712`.

## 3. Merge sequence (4 steps)

### Step 1 — establish K3's state (K1 + context-prune) in `k3k4-merge`
1. **File-level copy, not `git checkout`.** *(SUPERSEDES the earlier `git checkout k3-prune -- .`
   plan: findings 11:46:48Z proved the three worktrees are **three independent git repositories** —
   `git checkout k3-prune -- .` → `fatal: invalid reference: k3-prune`, and
   `git checkout 340afc75d… -- .` → `fatal: unable to read tree`. Neither K3's nor K4's objects exist
   in `k3k4-merge`'s object store, so a git-level merge is impossible. The brief's own Option A
   wording — "copy K3 changed+untracked files in" (`tasks/TASK-MERGE-K3K4.md` line 30) — is therefore
   the method of record.)*
   `cp -a` the following from `/home/hermes/projects/kcode-wt/k3-prune/` into `k3k4-merge/`:
   - K1's committed files (needed because the merge tree starts at `e22fbe712`, which lacks K1):
     `crates/kcode-app-core/src/agent/progress_flush.rs`,
     `.../agent/progress_flush_tests.rs`, `.../agent/progress_flush_turn_loop_tests.rs`.
   - K3's tracked-modified files: `Cargo.lock`, `Cargo.toml`, `crates/kcode-app-core/Cargo.toml`,
     `crates/kcode-app-core/src/agent/response_recovery.rs`, `.../agent/tools.rs`, `.../lib.rs`,
     `.../tool/batch.rs`, `.../tool/read.rs`, `crates/kcode-base/src/config.rs`,
     `crates/kcode-base/src/config/default_file.rs`, `crates/kcode-base/src/config/env_overrides.rs`,
     `crates/kcode-config-types/src/lib.rs`.
     *(These carry K1's edits too, so copying them covers K1's changes to the 4 config files,
     `agent.rs` and `turn_loops.rs` in the same pass.)*
   - K3's untracked feature files: `crates/kcode-app-core/src/context_prune.rs`,
     `crates/kcode-app-core/src/context_prune/tests.rs`, `vendor/context-prune/**`.
   - K1's `agent.rs` / `turn_loops.rs` are overwritten in Step 2 by K4's supersets, so they need no
     separate copy here.
2. Verify after the copy: `cmp` each copied file against its k3-prune source (exit 0), and confirm
   `git status --porcelain` in `k3k4-merge` lists exactly the expected paths.

*Rationale for including `response_recovery.rs` and `batch.rs`:* findings 11:44:48Z proved them
byte-identical in both worktrees, so taking K3's copy cannot lose anything and satisfies the brief's
"keep shared-base files as-is".

*Not copied:* k3-prune's docs (`findings.md`, `plan.md`, `K3-REPORT.md`, `K3FIX-REPORT.md`,
`V-REPORT.md`, `.kcode/`) — they are that job's narrative, not merge content, and the merge tree keeps
its own progress docs.

### Step 1b — verify the K3 layer
- `cmp` every copied file against its k3-prune source → all exit 0.
- `git status --porcelain` in `k3k4-merge` shows the expected modified + untracked set only.
- Spot-check that K1 arrived: `agent/progress_flush.rs` exists and `lib.rs` contains
  `pub mod context_prune;`.

### Step 2 — layer K4's feature code
Copy from `/home/hermes/projects/kcode-wt/k4-parallel/` (all K4 code is committed at `cb9ff5853`,
findings 11:43:50Z):
- `crates/kcode-app-core/src/agent/parallel_tools.rs` (new, 400 lines)
- `crates/kcode-app-core/src/agent/parallel_tools_tests.rs` (new, 1080 lines)
- `crates/kcode-app-core/src/agent.rs` — K1 + 3 lines (`mod parallel_tools;`, `#[cfg(test)] mod parallel_tools_tests;`)
- `crates/kcode-app-core/src/agent/turn_loops.rs` — K1 + 28/-5
- `crates/kcode-app-core/src/agent/turn_streaming_mpsc.rs` — K1 + 22

*Why wholesale copy is safe:* findings 11:44:53Z measured `git diff 340afc75d -- <file>` for all three
in k4-parallel and found only the K4 additions listed there. K3's dirty tree does not touch any of the
three (they are absent from k3-prune's `git status --porcelain`, findings 11:43:50Z), so after step 1
they are at K1's version and K4's copy is a strict superset.

### Step 3 — union the 4 config files (the only real conflicts)
Both features are additive; the conflicts are same-offset insertions (findings 11:45:24Z–11:45:28Z).
Resolution rule for every one of the four: **keep K3's block AND K4's block, drop neither.**

| File | K3 adds | K4 adds | Resolution |
|---|---|---|---|
| `crates/kcode-config-types/src/lib.rs` | `ContextPruneConfig` + `Default` | `ParallelToolsConfig` + `Default` + `allows_parallel()` | Append K4's struct after K3's (same insertion point after `CompactionConfig`'s impl). `ProgressConfig` and `malformed_tool_call_rounds` are identical in both — one copy. |
| `crates/kcode-base/src/config.rs` | import `ContextPruneConfig`; 5 `KCODE_PRUNE_*` in `CONFIG_ENV_KEYS`; `pub progress` + `pub context_prune` in `Config` | import `ParallelToolsConfig`; 3 `KCODE_PARALLEL_TOOLS_*`; `pub progress` + `pub parallel_tools` | Imports: add `ParallelToolsConfig`. Env keys: add K4's 3 after K3's 5. `Config`: add `pub parallel_tools` beside `pub context_prune` (one `pub progress`). |
| `crates/kcode-base/src/config/default_file.rs` | `[progress]` + `[context_prune]` sections; `malformed_tool_call_rounds` comment; progress assertions | `[parallel_tools]` section; same comment block; progress + parallel_tools assertions | Add K4's `[parallel_tools]` section; add K4's `parallel_tools` assertions to the existing test (K4's test block is the union of progress + parallel_tools, so it replaces K3's progress-only block — this *adds* assertions, never weakens any). |
| `crates/kcode-base/src/config/env_overrides.rs` | `pub` widening; `malformed_tool_call_rounds`; 5 `KCODE_PRUNE_*`; 5 `KCODE_FLUSH_*`/`KCODE_PROGRESS_FILE` | same `pub` widening; identical `malformed_tool_call_rounds`; 3 `KCODE_PARALLEL_TOOLS_*` | Append K4's 3 `KCODE_PARALLEL_TOOLS_*` blocks after K3's prune blocks. Keep ONE `KCODE_MALFORMED_TOOL_CALL_ROUNDS` block (dedupe — a second copy would be unreachable dead code, and `cargo clippy`'s dead-code/unused lints would flag it). |

### Step 4 — build/verify in the isolated target dir
```sh
cd /home/hermes/projects/kcode-wt/k3k4-merge
export CARGO_TARGET_DIR="$PWD/.target"     # never /home/hermes/.cargo-target
```
1. `cargo check --workspace` (background) → exit 0.
2. `cargo test -p kcode-app-core context_prune` → 18 passed, 0 failed.
3. `cargo test -p kcode-app-core parallel_tools` → 16 passed, 0 failed.
4. `grep` both config sections in `config.rs`; confirm each defaults off.
5. `cargo build --release` (background) → `$PWD/.target/release/kcode --version`; `strings` finds both
   `context_prune` and `parallel_tools`.
6. Confirm both `context_prune.rs` and `parallel_tools.rs` exist in the merged tree.

## 5. Explicitly out of scope / non-goals

- No push (local commit only, `tasks/TASK-MERGE-K3K4.md` line 68).
- No `JCODE_GUARD_OFF`, no `cargo fmt`, no installs (dispatch-context constraint 6).
- No weakening of either feature's tests (constraint 1). Both suites run byte-identical to their
  source trees' assertions.
- K1's progress-flush feature is carried along because it is the shared base of both branches; it is
  not a target of this merge but must not be dropped.
- No changes to `vendor/context-prune` internals, no changes to the shared-base files.

## 6. Done-check (verbatim from the brief)

`CARGO_TARGET_DIR=$PWD/.target cargo check --workspace` → exit 0; `cargo test -p kcode-app-core
context_prune` → 18 passed 0 failed; `cargo test -p kcode-app-core parallel_tools` → 16 passed 0
failed; both config sections present and independently defaulted-off; `$PWD/.target/release/kcode
--version` exists and `strings` finds `context_prune` AND `parallel_tools`; `context_prune.rs` +
`parallel_tools.rs` both present. Then `MERGE-K3K4-REPORT.md` + local commit.

## 7. Risk notes

- **Cargo.lock:** K3's `Cargo.lock` (dirty) already carries the `vendor/context-prune` entry; K4 did
  not touch `Cargo.lock`. Taking K3's is correct; if cargo wants to rewrite it, that is a normal
  lock refresh, not a feature change.
- **Feature collision on `agent.rs`:** both K1 and K4 add `mod` lines at the same spot; K4's file is
  the superset (findings 11:44:53Z), so no hand-edit is needed there.
- **Grunt rules:** no `unwrap`/`expect`/`panic!`/`#allow` in any new or edited line — all copied code
  already satisfies this in its source tree; my config edits are declarations and env-var parsing
  blocks that follow the same `if let Ok(...)` pattern.

---

## Observation run progress — 2026-11-14 12:31–12:37Z (`TASK-OBS-K3K4-observe.md`)

**Outcome: GO.** Both K3 (context compression) and K4 (parallel tools) are correctly wired and
working in the merged tree at `c41d2801a`. No urgent fix required. Observation only — no product
source modified (`git status --porcelain` shows only untracked `.kcode/`, `.obs-kcodehome/`,
`.target/`).

### Provenance (recorded FIRST, all commands exit 0)

Binary `.target/release/kcode` 156906280 bytes, `kcode v0.2.11125-dev (c41d2801a, dirty)`; branch
`k3k4-merge`; HEAD `c41d2801a3ed45d1acbf3ae6bb0ecfb76ba5f059`; `CARGO_TARGET_DIR` =
`/home/hermes/projects/kcode-wt/k3k4-merge/.target`; `KCODE_HOME` =
`/home/hermes/projects/kcode-wt/k3k4-merge/.obs-kcodehome`; session tag
`[ses:session_kangaroo_179|prv:litellm|mod:step]` 417 occurrences, matching the isolated config's
provider/model. Both features enabled by config only.

### What is now proven (and how)

1. **[12:37:22Z, done-check] K3+K4 unit tests green — `34 passed; 0 failed`**, run twice in this
   build (12:36:02Z and 12:37:22Z). Command:
   `cargo test -p kcode-app-core --lib -- context_prune parallel_tools`
   (`test result: ok. 34 passed; 0 failed; 0 ignored; 0 measured; 1956 filtered out`). Includes the
   two K4 load-bearing tests (`enabled_read_only_calls_overlap_in_time` asserts `max_in_flight >= 2`
   and a longest call under 150 ms against a ≥180 ms sequential sum;
   `merged_results_keep_the_models_original_call_order` uses a reverse-completion probe and asserts
   model order is preserved) and all 18 K3 tests (byte-stability, fail-open, threshold, skeleton,
   env overrides). Caveat: 34 of 1990 lib tests, so this proves the K3/K4 suites, not the full suite.
2. **[12:35:16Z] K3 ran live 31 times through BOTH ingest stages** — `grep -c 'compressed .* bytes'`
   → 31, split 17 `file read` + 14 `tool output`. SUPERSEDES an earlier 23, which was my measurement
   error (`grep -c 'context_prune'` also matched 5 `TOOL_LIFECYCLE` path lines).
3. **[12:35:16Z] Cache table is `cache`, 31 rows, 1:1 with the events** — proves the funnel ran once
   per payload with no double-compression. SUPERSEDES my wrong table name `prune_cache`.
   `output_len` stores the RAW length (`store.put(hash, input.len(), input.len(), ...)` at
   `context_prune.rs:228`), so log lines are authoritative for ratios.
4. **[12:35:16Z] Prompt caching holds** — `cache_write=0` in all 68 occurrences; `cache_read` climbs
   (111872→165184). Two real series recorded (an earlier 14080→34060 window and this full-session
   one) with an explicit warning not to mix them.
5. **[12:35:16Z] `rust_skeleton` fired 0 of 31** — `cr_overwrite` 31, `trim_line_ends` 23,
   `blank_runs` 6. Consistent, not broken: the skeleton needs a full-file Rust parse and this
   session's reads were bounded ranges. All five skeleton tests pass. Tuning note, not a defect.
6. **[12:34:01Z] Zero tool-call overlap observed live, with a proven cause** — the next `start` always
   follows the previous `done`. Cause: `.obs-kcodehome/config.toml` sets `[hooks] pre_tool` →
   `parallel_disabled_by_hooks()` (`parallel_tools.rs:272-274`) → empty handled set at `:300-302` →
   sequential loop runs everything. **K4 is therefore proven by config-loaded + unit tests (the
   task's permitted fallback); live parallelism is explicitly NOT claimed.** To observe it live,
   run a session with no `pre_tool` hook — a config change outside this observation's scope.
7. **[12:34:01Z] Tool histogram** — read 40, agentgrep 20, bash 17, edit 12, batch 4, todo 2 (95).
   `read`+`agentgrep` (60) are allowlisted; `bash`/`edit` are not, so they stay sequential by design.
8. **[12:35:04Z] All five task symbols resolved** — `execute_with_background` (def
   `tool_handoff.rs:26-34`, K4 site `parallel_tools.rs:194`, so one executor serves both paths),
   `tool_calls.iter().enumerate()` (`turn_loops.rs:1219` + skip guard `:1220-1222`; streaming twin is
   `0..tool_count` at `turn_streaming_mpsc.rs:1473-1479`, invisible to an `enumerate()` grep),
   `execute_parallelizable_tools` (def `parallel_tools.rs:292-400`, call sites `turn_loops.rs:1204-1211`
   and `turn_streaming_mpsc.rs:1462-1469`), `prune_tool_output` (wrapper `agent/tools.rs:19-36`, funnel
   `agent/tools.rs:136`, core `context_prune.rs:182-190`, file-read twin `tool/read.rs:273/288-306`),
   and **`compress_text` → `compress_text_with`** (`context_prune.rs:125`, library
   `vendor/context-prune/src/compress.rs:57-128`; `compress_text` at `:52-56` is the library's
   default-config wrapper that kcode never calls).

### Deliverables

- `OBS-K3K4-ANALYSIS.md` — 155 lines, 16468 bytes: five symbol tables (file:line + role), the full
  ASCII path from model tool_calls to `ContentBlock::ToolResult`, and the config surface table
  showing both features default OFF.
- `OBS-K3K4-REPORT.md` — 314 lines, 22282 bytes: provenance FIRST, six observations, six prioritised
  improvements, verdict GO, and a corrections table.

### Top improvement (the only real gap found)

**Warn when `[hooks] pre_tool` silently disables `[parallel_tools]`.** A user who enables
`parallel_tools` and also has a `pre_tool` hook gets parallelism off with no log line, no status
indication, and no test failure — the feature looks enabled and is not. The bypass is correct (an
ordering transformer must run per call); the silence is the problem. Fix is a `logging::warn` at
`parallel_tools.rs:300` plus a config-reference line, not a correctness change. Remaining
improvements: per-round parallel telemetry; a distinct `rust_skeleton` log marker; document
`min_bytes=2048` as a deliberate choice; document that `output_len` is the raw length; note in the
config reference that allowlisting `bash`/`edit` trades ordering guarantees for speed.

### Corrections recorded during this observation

K3 count 23→31 (12:35:16Z, my measurement); cache table `prune_cache`→`cache` (12:35:16Z, my wrong
name); `output_len` is the raw length (12:33:02Z, my inference); `cache_read` series mismatch
(12:35:16Z, both real, different windows); `compress_text`→`compress_text_with` naming (12:32:05Z);
`cargo test` two-filter syntax (12:34:03Z, my command); progress docs are at the worktree root, not
under `.kcode/` (12:36:57Z, my path). **Job corrections used: 1 of 3.** No stop condition triggered.

### [added 2026-11-14 12:38:12Z] The `pre_tool` bypass is a tested contract, not an accident

The done-check log (`.target/obs-donecheck.log`, read in full) shows
**`agent::parallel_tools_tests::a_pre_tool_hook_sends_the_round_down_the_sequential_path ... ok`** —
a named regression test for the exact interlock that produced the zero-overlap result above. This
upgrades that finding from "explained by code" to "explained by code and pinned by a named test",
and reclassifies the top improvement (warn when `[hooks] pre_tool` and `[parallel_tools]` are both
configured) from a possible correctness fix into a telemetry/docs change. The same log confirms the
other K4 guards pass: `invalid_calls_and_sdk_results_are_never_parallelized`,
`a_populated_allow_list_does_not_override_the_master_switch`, `an_empty_allow_list_parallelizes_nothing`,
`a_single_read_only_call_is_left_sequential`, `disabled_by_default_so_a_round_is_untouched`,
`partition_uses_maximal_runs_cut_at_the_first_sequential_call`,
`sequential_tail_of_a_mixed_round_stays_sequential`. Verdict unchanged: GO.

---

## REASONING RUN — 2026-11-14 (K4 parallel vs pre_tool hook): resolution design

**Question:** can K4 `parallel_tools` and the `pre_tool` tool-guard BOTH be on? **Answer: yes — they are only
*implemented* as mutually exclusive, not truly so.** The per-call hook (`pre_tool` gate + `pre_tool_transform`)
already runs *inside each parallel worker* via `Registry::execute` (`tool/mod.rs:1074-1121`, reached from
`tool_handoff.rs:42` ← `parallel_tools.rs:194`). The blanket disable (`parallel_tools.rs:300`) is a
conservative "any hook might be order-sensitive" approximation.

**Recommended resolution (config-driven, default-preserving):**
1. Add `[parallel_tools] run_hooks_in_parallel` (bool, **default false** = today's sequential behavior).
2. When true, the parallel path proceeds; each worker runs the hook per-call automatically (no `Registry::execute`
   change needed).
3. One-time WARN + telemetry when a `pre_tool` hook is configured AND `run_hooks_in_parallel` is false →
   "parallel tool execution disabled for this round" — never silent again.
4. Contract tests: `a_pre_tool_hook_sends_the_round_down_the_sequential_path` PRESERVED (its no-hook assertion
   is unchanged; default keeps it green). ADD a new test proving hook-active AND ≥2 overlap AND model order.

**Trade-offs / risk:** narrow fail-closed hazard (co-batched large whole-file `read` whose allow depends on an
`agentgrep` marker not yet written → conservative false-block, retryable). Byte-stability/prompt-cache preserved
(`join_all` merges in model order regardless of hooks). Hook-class distinction IS warranted (per-call-pure vs
order-sensitive/stateful); the harness cannot classify a script, so it is an operator-declared opt-in.

**Status:** design delivered, NOT implemented — the proof test (sandbox hook-script + merged
OverlapProbe/ReverseCompletionProbe + 3 struct-literal fixes in the protected test file) is substantial, not
"small," so per the task gate "implement only if clearly safe and small, else deliver the design and stop
(owner will decide)." Ready-to-implement blueprint in `REASON-K3K4-hook-vs-parallel.md`.

---

# K5 — implementation plan → executed (2026-11-14)

The plan above was the design. The owner greenlit the §c/§d implementation (verbatim: "1"), so this
section records the plan as executed. Every change is cited `file:line`; the live log is
`.kcode/progress.md` and the full code-reading record is the K5 section of `findings.md`.

## The 4 file changes (as executed)

1. **`crates/kcode-config-types/src/lib.rs`** — `pub run_hooks_in_parallel: bool` at `:569` (doc comment
   `:555-568`) + `run_hooks_in_parallel: false` in `Default` at `:578`. `#[serde(default)]` at `:533` makes
   it backward-compatible: an older `config.toml` loads as `false` = today's behavior.
2. **`crates/kcode-base/src/config/default_file.rs`** — documented at `:584-593` (10 comment lines +
   `run_hooks_in_parallel = false`); ADDITIVE `assert!(!config.parallel_tools.run_hooks_in_parallel, …)`
   at `:1131-1136`. No existing assertion modified.
3. **`crates/kcode-app-core/src/agent/parallel_tools.rs`** — gate at `:335-346` (was `:300`) split into
   `!cfg.enabled` then `parallel_disabled_by_hooks() && !cfg.run_hooks_in_parallel` →
   `warn_parallel_disabled_by_hooks_once()` + return; the once-per-process warn (`static AtomicBool` +
   `crate::logging::warn`) at `:291-311`; the stale "transformer adds a path" doc-comment at `:264-284`
   rewritten. No `Registry::execute` change — the hook already runs per call inside each worker.
4. **`crates/kcode-app-core/src/agent/parallel_tools_tests.rs`** — the new field added to the three
   `ParallelToolsConfig` literals (`:285`, `:960`, `:990`), a mechanical compile fix with no assertion
   touched; the new proof test `a_pre_tool_hook_runs_inside_parallel_workers_when_opted_in` at `:1101-1223`
   with its merged probe `HookedOverlapProbe` at `:1225-1269`.

## The proof test (what green must show)

`a_pre_tool_hook_runs_inside_parallel_workers_when_opted_in` builds a sandbox `config.toml`
(`[hooks] pre_tool = <sandbox>/allow.sh`, `[parallel_tools] enabled=true run_hooks_in_parallel=true
read_only_tools=["read"] max_concurrency=4`) + an executable `allow.sh` writing one marker line per
invocation, invalidates the config cache, and submits `c1("a")`, `c2("ab")`, `c3("abc")`. Green requires
**all three** simultaneously: (i) the marker file has exactly **3** lines (hook ran once per call inside the
workers), (ii) `max_in_flight >= 2` (calls overlapped), (iii) `tool_result_ids == [c1,c2,c3]` while
`completion_order == [c3,c2,c1]` (merge followed model order, not completion order).

## Contract change (explicit)

"a `pre_tool` hook always forces the round sequential" becomes "a `pre_tool` hook forces the round
sequential **unless** the operator opts in to running it inside each parallel worker." Off by default;
the safe default is untouched; the residual hazard is fail-closed.

## Deviations

- **Telemetry counter omitted.** No existing counter has "round serialized by hook" semantics, and reusing
  `record_tool_call()` would inflate a usage metric with a non-call event. The task allows this
  ("a telemetry counter if trivially available (else omit and note)"); the once-per-process
  `logging::warn` is the never-silent guarantee.

## Verification

Done-check run in the isolated target dir (`CARGO_TARGET_DIR=/home/hermes/projects/kcode-wt/k3k4-merge/.target`,
never the shared `~/.cargo-target`), background only:
1. `cargo check --workspace`
2. `cargo test -p kcode-app-core --lib -- parallel_tools` (all existing tests incl.
   `a_pre_tool_hook_sends_the_round_down_the_sequential_path` unchanged + the new proof test)
3. the default-config test (`default_config_template_parses`)
Verbatim outputs and exit codes are in `K5-REPORT.md`.
