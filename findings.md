# findings.md — K3+K4 merge investigation log

> **This is a live findings log, not a summary.** Append every observation as you make it, each with
> its **source** (file:line, command + output, or URL) and a **timestamp**. Never batch observations
> to the end — a crash or timeout must not lose what you've learned. **Later findings may supersede
> earlier ones**: when an observation overturns an earlier one, mark it explicitly
> (`SUPERSEDES <section>, was X, now Y because [evidence]`). **If two observations conflict, record
> BOTH and state which is authoritative and why** — never silently drop one. Keep this document
> updated while you work; it must never lag your actual state.

---

## Session / task framing

- **[verified now 2026-11-14 11:43Z]** Governing docs read in order:
  - `/home/hermes/projects/turbohermes/tasks/TASK-MERGE-K3K4.md` — the job (combine K3 + K4, prove both).
  - `/home/hermes/.hermes/cache/scratch/dispatch-context-2026-11-13.md` — binding owner constraints.
  - `/home/hermes/projects/turbohermes/DISPATCH-HYGIENE.md` — C1 isolated build, C2 provenance block.
- **[verified now 2026-11-14 11:43Z]** Merge worktree: `/home/hermes/projects/kcode-wt/k3k4-merge`,
  branch `k3k4-merge`, HEAD `e22fbe712248b147d69e5f69a8b4904c953cb47b`
  (subject: "ci: disable GitHub Actions workflows (owner request 2026-11-13)"),
  `git status --porcelain` → empty (clean). Matches brief ground truth (main `phase-3-jcode-adapter`
  @ `e22fbe712`).
  Command: `cd /home/hermes/projects/kcode-wt/k3k4-merge && git rev-parse --abbrev-ref HEAD && git rev-parse HEAD && git log --oneline -3 && git status --porcelain` (exit 0).

## Ground-truth claims to re-verify before merging (brief asserts these)

1. K3 source `/home/hermes/projects/kcode-wt/k3-prune` @ `340afc75d` branch `k3-prune`.
2. K4 source `/home/hermes/projects/kcode-wt/k4-parallel` @ `cb9ff5853` branch `k4-parallel`.
3. Features disjoint: K3 = `context_prune.rs` + `context_prune/` + `tool/read.rs` + `agent/tools.rs` +
   `lib.rs` + config section; K4 = `agent/parallel_tools.rs` + `agent/parallel_tools_tests.rs` +
   `agent/turn_loops.rs` + `agent/turn_streaming_mpsc.rs` + `agent.rs` + config section.
4. Only 2 shared files with identical diffs in both: `agent/response_recovery.rs`, `tool/batch.rs`.
5. 4 config files touched by both, different sections: `kcode-base/src/config.rs`,
   `kcode-base/src/config/default_file.rs`, `kcode-base/src/config/env_overrides.rs`,
   `kcode-config-types/src/lib.rs`.

## Findings

- **[verified now 2026-11-14 11:43Z]** Working directory confirmed; build env rule fixed for this whole
  run: `export CARGO_TARGET_DIR="$PWD/.target"` inside `/home/hermes/projects/kcode-wt/k3k4-merge`
  before any cargo invocation (DISPATCH-HYGIENE C1). Never `/home/hermes/.cargo-target`.

---

## OBSERVATION RUN — 2026-11-14 12:31–12:37Z (`TASK-OBS-K3K4-observe.md`)

Provenance block (recorded FIRST, all commands exit 0): binary
`.target/release/kcode` 156906280 bytes, `kcode v0.2.11125-dev (c41d2801a, dirty)`; branch
`k3k4-merge`; HEAD `c41d2801a3ed45d1acbf3ae6bb0ecfb76ba5f059`; `git status --porcelain` shows only
untracked `.kcode/`, `.obs-kcodehome/`, `.target/` (no tracked file modified); `CARGO_TARGET_DIR`
= `/home/hermes/projects/kcode-wt/k3k4-merge/.target`; `KCODE_HOME` =
`/home/hermes/projects/kcode-wt/k3k4-merge/.obs-kcodehome`. Session tag
`[ses:session_kangaroo_179|prv:litellm|mod:step]` appears 417 times in
`.obs-kcodehome/logs/kcode-2026-11-14.log`, matching `.obs-kcodehome/config.toml`'s
provider/model. **Both features enabled by config only; no product source modified.**

### [verified 2026-11-14 12:36:02Z] K3+K4 unit tests pass in this build: 34 passed, 0 failed

Command (background, log `.target/obs-tests.log`):
`cargo test -p kcode-app-core --lib -- context_prune parallel_tools`
Output: `running 34 tests` … `test result: ok. 34 passed; 0 failed; 0 ignored; 0 measured; 1956
filtered out; finished in 0.68s`. **SUPERSEDES the 12:34:03Z entry below** (which was my own failed
command syntax, `cargo test` accepts only one positional filter). Caveat: 34 of 1990 lib tests, so
this proves the K3/K4 suites, not the full suite. Re-confirmed by the done-check at 12:37:22Z
(`.target/obs-donecheck.log`, `34 passed; 0 failed … finished in 0.69s`).

### [verified 2026-11-14 12:34:03Z] First background test command failed — my syntax, not the product

`cargo test -p kcode-app-core --lib context_prune parallel_tools` →
`error: unexpected argument 'parallel_tools' found`. Nothing built or run. Fixed by using
`-- context_prune parallel_tools` (the test binary's own multi-filter support).

### [verified 2026-11-14 12:33:51Z] K4 concurrency and order are proven by measured tests, not smoke tests

Source: `crates/kcode-app-core/src/agent/parallel_tools_tests.rs:496-605`, read directly.
- `enabled_read_only_calls_overlap_in_time @ 496-555`: three 60 ms `read` probes; asserts
  `started == 3`, `finished == 3`, `max_in_flight >= 2`, and `longest < 150` ms (sequential sum
  would be ≥180 ms). `handled == HashSet::from([0, 1, 2])`.
- `merged_results_keep_the_models_original_call_order @ 557-605`: `ReverseCompletionProbe` makes the
  LAST call finish first; asserts merged `tool_result_ids == [c1, c2, c3]` **and** that observed
  completion really was `[c3, c2, c1]` — "the probe must have completed in reverse order, or this
  test proves nothing".
- `probe_agent @ 293-318` comment "config.toml. Force the reload so `hook_configured()` reads the
  empty…" — the harness deliberately clears hooks so the parallel path is reachable, which
  independently confirms the bypass mechanism.

### [verified 2026-11-14 12:33:51Z] `hook_configured` usage map — 8 matches in 5 files

`agent.rs:1304` (session lifecycle), `agent/parallel_tools.rs:273` (**the K4 bypass**),
`agent/turn_execution.rs:321` (`turn_start`), `:342` (`turn_end`), `tool/mod.rs:881`
(`post_tool`), `:904` (`post_tool_feedback`), `tool/mod.rs:1087` (`pre_tool` per-call gate inside
`Registry::execute`). `pre_tool` was already a per-call gate; K4's bypass is consistent with it.

### [verified 2026-11-14 12:34:01Z] NO tool-call overlap observed live — and the cause is proven

Pairing `TOOL_LIFECYCLE` `phase=start`/`phase=done` for the first three reads (same
`message_id=message_1794659440305_2960272861906390116`):
`13:30:40.366 start` → `13:30:40.408 done` (elapsed_ms=41); `13:30:40.438 start` → `13:30:40.463
done` (23); `13:30:40.491 start` → `13:30:40.548 done` (56). Every `start` follows the previous
`done` → **zero overlap**. Cause, proven in code: `.obs-kcodehome/config.toml` sets `[hooks]
pre_tool` → `parallel_disabled_by_hooks()` (`agent/parallel_tools.rs:272-274`) returns true →
`execute_parallelizable_tools` returns an empty set at `:300-302` → the sequential loop runs every
call. **So observation (b) uses the permitted fallback: config loaded enabled + unit tests prove
concurrency and order. I am NOT claiming live parallelism for this session.** To get live evidence,
run a session with no `pre_tool` hook — a config change, outside this observation's scope.

### [verified 2026-11-14 12:34:01Z] Tool usage histogram — allow-listed tools dominate, only permitted tools used

`grep -o 'resolved_tool_name=[a-z_]*' .obs-kcodehome/logs/kcode-2026-11-14.log | sort | uniq -c |
sort -rn` → read **40**, agentgrep **20**, bash **17**, edit **12**, batch **4**, todo **2**
(95 total). `read` + `agentgrep` = 60 of 95 and both are on `read_only_tools`
(`["agentgrep", "read", "lookup"]`). `bash`/`edit` are NOT allowlisted, so they must stay
sequential even with K4 enabled — exactly the conservative rule in `parallel_tools.rs:88-122`.
`lookup` never called (allowlist is an upper bound, not a requirement).

### [verified 2026-11-14 12:35:16Z] K3 ran live 31 times through BOTH ingest stages — 17 file-read + 14 tool-output

`grep -c 'compressed .* bytes' …` → **31**. `grep -o 'file read compressed' | wc -l` → **17**;
`grep -o 'tool output compressed' | wc -l` → **14**. **SUPERSEDES the earlier 23 figure, which was
a measurement error**: `grep -c 'context_prune'` also matched 5 `TOOL_LIFECYCLE` lines whose
`touched_paths=` contains the path `.../src/context_prune.rs`. The 23 figure must not be reused.
Representative lines: `file read compressed 32024 -> 31994 bytes (0.1% saved, transforms:
cr_overwrite,trim_line_ends)`; `file read compressed 10467 -> 10411 bytes (0.5% saved, …)`;
`tool output compressed 24485 -> 24470 bytes (0.1% saved, transforms: cr_overwrite)`;
`tool output compressed 21372 -> 21357 bytes (0.1% saved, transforms: cr_overwrite,blank_runs,
trim_line_ends)`. This upgrades the file-read stage from "defined" to "observed": 17 events prove
`tool/read.rs:273` → `prune_file_read_for_context` executed, 14 prove `agent/tools.rs:136` executed.

### [verified 2026-11-14 12:35:16Z] Cache table is `cache` (5 columns), 31 rows, 1:1 with the events

`sqlite3` on `.kcode/prune/cache.db` via `sqlite_master` + `pragma table_info`: table **`cache`**,
columns `['hash', 'ts', 'input_len', 'output_len', 'payload']`, **rows: 31**. Grew 11 → 31 rows
over the run. **SUPERSEDES my first probe**, which guessed table `prune_cache` and failed with
`OperationalError: no such table: prune_cache`. The 1:1 row/event ratio proves the funnel ran once
per payload and did not double-compress.

### [verified 2026-11-14 12:33:02Z] `output_len` stores the RAW length — log lines are authoritative for ratios

`store.put(hash, input.len(), input.len(), ...)` at `context_prune.rs:228` writes the input length
into both `input_len` and `output_len`. Recorded as a self-correction of my own earlier inference,
and raised as improvement #5 in the report: rename to `raw_len`, populate the real compressed
length, or comment that it is intentionally raw. Any of the three removes a future misreading.

### [verified 2026-11-14 12:35:16Z] Transform mix — `rust_skeleton` fired 0 of 31 times

`grep 'compressed .* bytes' … | grep -oE 'transforms: [a-z_,]+' | sort | uniq -c` →
`cr_overwrite,trim_line_ends` × **18**, `cr_overwrite,blank_runs,trim_line_ends` × **5**,
`cr_overwrite` × **7**, `blank_runs` × **1**. All 31 include `cr_overwrite`; `trim_line_ends` 23;
`blank_runs` 6; **`rust_skeleton` 0** despite `skeleton=true`. Consistent, not broken: the skeleton
applies only in `prune_file_read` (`context_prune.rs:154-171`) for Rust files that parse, and this
session's reads were mostly bounded ranges (`range=1..200 total_lines=257`). The skeleton tests all
pass (`rust_file_read_is_skeletonized_keeping_signatures_dropping_bodies`,
`rust_file_read_skeleton_is_byte_stable`, `non_rust_file_read_uses_the_text_compressor`,
`skeleton_off_falls_back_to_the_text_compressor_for_rust`,
`fail_open_when_the_rust_payload_does_not_parse`). Tuning note, not a defect.

### [verified 2026-11-14 12:35:16Z] Prompt caching holds while K3 rewrites bytes — `cache_write` always 0

`grep -oE 'cache_write=[0-9]+' … | sort | uniq -c` → **`68 cache_write=0`** (every occurrence
zero). `grep -oE 'cache_read=[0-9]+' | uniq | tail -12` → `111872, 112192, 124992, 126720,
127424, 129856, 144000, 144896, 151296, 165184, 0, 32704`. `[Tokens]` print site
`agent/turn_loops.rs:937`; KV-cache monitor `agent.rs:163-213`
(`finish_kv_cache_monitor_request`). **Conflict recorded and resolved:** an earlier flush recorded a
smaller series (14080→34060). Both are real, different windows of the same session; the conclusion
(read rises, write stays 0) is identical for both and the two series must not be mixed. The single
`cache_read=0` followed by `32704` is a fresh cache window, not cache loss.

### [verified 2026-11-14 12:35:04Z] Call-site map — the five task symbols, all resolved

`OBS-K3K4-ANALYSIS.md` (155 lines) written with the real tables. Key resolutions:
- `execute_with_background`: def `agent/tool_handoff.rs:26-34` (body `35-65`), re-export
  `turn_streaming_mpsc.rs:8`, sequential sites `turn_loops.rs:847/1359` +
  `turn_streaming_mpsc.rs:988`, **K4 site `parallel_tools.rs:194`** — one executor serves both paths.
- `tool_calls.iter().enumerate()`: `turn_loops.rs:1219` with skip guard `:1220-1222`; streaming twin
  `turn_streaming_mpsc.rs:1473-1479` uses `for tool_index in 0..tool_count`, which an `enumerate()`
  grep cannot see; parallel-internal `.enumerate()` at `parallel_tools.rs:338`.
- `execute_parallelizable_tools`: def `parallel_tools.rs:292-400`, enable+bypass `:300-302`,
  `parallel_disabled_by_hooks` `:272-274`, partition `:98-122`, eligibility `:243-260`, clamp
  `:141-143`, execution `:156-214`, merge `:335-395`, identical mapping `:223-239`; call sites
  `turn_loops.rs:1204-1211` (arg 4 `None`) and `turn_streaming_mpsc.rs:1462-1469`
  (`Some(&event_tx)`).
- `prune_tool_output`: agent wrapper `agent/tools.rs:19-36`, **funnel `agent/tools.rs:136`** (the
  single line where every tool result enters context), core `context_prune.rs:182-190`, file-read
  core `:142-175`, read tool `tool/read.rs:273` / `:288-306`, compressor `:123-133`, cache
  `:215-254`.
- **`compress_text` → `compress_text_with`** (naming correction): kcode calls
  `compress_text_with` at `context_prune.rs:125` (import `:39`), library
  `vendor/context-prune/src/compress.rs:57-128`. `compress_text` at `compress.rs:52-56` is the
  library's default-config wrapper that kcode never calls; a grep for exactly `compress_text(`
  finds nothing in kcode.

### [verified 2026-11-14 12:36:45Z] Deliverables written; verdict GO

- `OBS-K3K4-ANALYSIS.md` — 155 lines, 16468 bytes.
- `OBS-K3K4-REPORT.md` — 314 lines, 22282 bytes. Provenance FIRST, six observations, six
  prioritised improvements, verdict **GO — both features correctly wired and working, no urgent fix
  required**.
- Top improvement: **warn when `[hooks] pre_tool` silently disables `[parallel_tools]`** — a user
  gets parallelism off with no log line, no status indication, and no test failure. The bypass is
  correct; the silence is the problem.

### [verified 2026-11-14 12:38:00Z] The `pre_tool` bypass is itself a tested contract, not an accident

Source: the done-check log `.target/obs-donecheck.log` (background task `81119835ay`), full test
list read directly. Among the 34 passing tests is
**`agent::parallel_tools_tests::a_pre_tool_hook_sends_the_round_down_the_sequential_path ... ok`**.
This is the direct, named regression test for the exact interlock this observation hit live: a
configured `pre_tool` hook sends the round down the sequential path. It means the bypass is
**deliberate and covered**, so improvement #1 (add a warning when the two features are configured
together) is a telemetry/docs change, not a fix to unintended behaviour. For completeness the same
log also confirms the other guard tests pass: `invalid_calls_and_sdk_results_are_never_parallelized`,
`a_populated_allow_list_does_not_override_the_master_switch`,
`an_empty_allow_list_parallelizes_nothing`, `a_single_read_only_call_is_left_sequential`,
`disabled_by_default_so_a_round_is_untouched`,
`partition_uses_maximal_runs_cut_at_the_first_sequential_call`,
`sequential_tail_of_a_mixed_round_stays_sequential`.

**This SUPERSEDES nothing and corrects nothing — it upgrades the 12:34:01Z zero-overlap finding from
"explained by code" to "explained by code AND backed by a named test that asserts exactly this
behaviour."** Verdict remains GO.

---

## REASONING RUN — 2026-11-14 14:14–14:21Z (K4 `parallel_tools` vs `pre_tool` hook)

Task: are K4 parallel-tools and the `pre_tool` tool-guard hook *truly* mutually exclusive or only
*implemented* that way? Reason to a resolution giving the owner BOTH. Deliverable:
`REASON-K3K4-hook-vs-parallel.md` in `/home/hermes/projects/turbohermes/`.

### Code readings (file:line)

- **`crates/kcode-app-core/src/agent/parallel_tools.rs` (400 lines):**
  - `:272-274` `parallel_disabled_by_hooks()` = `crate::hooks::hook_configured("pre_tool")`.
  - `:300` `execute_parallelizable_tools`: `if !cfg.enabled || parallel_disabled_by_hooks() { return Ok(empty) }`.
  - `:263-271` doc-comment: the *stated* reason for exclusivity — both `pre_tool` (gate) and
    `pre_tool_transform` (input replacer) run inside `Registry::execute`, both can rewrite a call's input;
    fears "a transformer that adds a path could hand one call a path another call already used" (cross-call
    shared state) → so "the parallel path steps aside entirely."
  - `:98-122` `partition_parallel_runs` (maximal runs, cut at first non-allow-listed, drop len-1).
  - `:156-214` `execute_parallel_run`: `Arc<Semaphore>` (`effective_permits` `:141-143`), `join_all` at
    `:202` (order-preserving, NOT `JoinSet`), each call via `execute_with_background` at `:194`.
  - `:223-239` `tool_result_blocks` (same mapping as sequential → byte-identical merge).
  - `:256-260` `ParallelEligibility::allows` (no `validation_error`, not in `sdk_tool_results`, on allow-list).
- **`crates/kcode-app-core/src/tool/mod.rs` `Registry::execute` `:1074-1121` — THE PER-CALL HOOK RUNNER:**
  `transform_tool_input(...)` `:1074-1080` (pre_tool_transform input replacer) → `run_pre_tool_gate(...)`
  `:1087-1095` (pre_tool gate; `updated_input` rebound `:1097-1099`, `Block`→`Err` `:1100-1112`) →
  `tool.execute(input, ctx)` `:1121`. Both hooks operate on that call's OWN local `input` (`let mut input`
  `:1085`); no cross-call state in the invocation model.
- **`crates/kcode-app-core/src/agent/tool_handoff.rs` `:35-65` `execute_with_background_budget`:** line 42
  spawns `registry.execute(&tool_name, input, ctx)`. **So the K4 parallel worker (`parallel_tools.rs:194`
  → `execute_with_background`) routes through `Registry::execute`, which runs BOTH hooks per call.** The
  hooks are NOT bypassed by the parallel path; they run inside each worker, concurrently, per call.
- **`crates/kcode-base/src/hooks.rs` (744 lines):** `hook_configured(event)` `:256-258` =
  `!hook_commands(event).is_empty()`; `hook_commands("pre_tool")` reads `hooks.pre_tool` `:236` (NOT
  `pre_tool_transform`); `transform_tool_input` `:264+` reads `hooks.pre_tool_transform` (separate key);
  `run_pre_tool_gate` `:507+`; `parse_gate_stdout` `:113-173` (decision/updatedInput/additionalContext).
  So `parallel_disabled_by_hooks()` consults ONLY `hooks.pre_tool`.
- **`crates/kcode-config-types/src/lib.rs` `ParallelToolsConfig` `:534-573`:** `enabled`, `read_only_tools`,
  `max_concurrency`; `#[serde(default)]` + `Default` (`false, [], 1`); `allows_parallel = enabled &&
  read_only_tools.contains(name)`. Adding a `run_hooks_in_parallel` field would break 3 struct literals in
  `parallel_tools_tests.rs` (`:279` `enabled_config`, `:953`, `:982`) — mechanical compile fixes, not
  assertion changes.
- **`crates/kcode-app-core/src/agent/parallel_tools_tests.rs`:** `a_pre_tool_hook_sends_the_round_down_the_
  sequential_path` `:920-947` — the name/comment describe hook→sequential INTENT, but the only ASSERTION
  pins the no-hook→parallel case (`!parallel_disabled_by_hooks()`, `handled == {0,1}`); it never configures
  a hook. `probe_agent` `:293-316` clears hooks via `invalidate_config_cache()` so `hook_configured()` reads
  the empty sandbox config. `enabled_config` `:278-284`; `OverlapProbe` (max_in_flight),
  `ReverseCompletionProbe` (reverse-completion order proof) already exist.

### Hook-script analysis — `/home/hermes/.hermes/scripts/jcode-tool-guard.sh` (292 lines)

- **State:** `$HOME/.hermes/cache/scratch/jcode-tool-guard`, per-session markers keyed by
  `JCODE_HOOK_SESSION_ID` (`:48-49`). Cross-call (session-scoped) state EXISTS, so the hook is not globally pure.
- **`agentgrep|ast-grep|sg` (`:90-97`) / `graphicle` (`:98-103`):** write markers, always `exit 0`.**Decision
  input-independent → pure.**
- **`read` (`:108-131`):** OFFSET/LIMIT → `exit 0` `:114-115` before any state check → **surgical reads pure.**
  File ≤400 lines → `exit 0` `:120-123` → **pure.** Whole-file >400 lines `:124-130` → **STATE-DEPENDENT**
  (needs prior smart-search marker) but **FAILS CLOSED** (blocks with retry msg `:128`, never false-allow).
- **`lookup`:** falls through to `exit 0` `:291-292` → **pure.**
- **`bash|Bash|shell|terminal` (`:136-238`):** the ONLY branch using `pre_tool_transform` (rtk rewrite / grep→rg
  `:192-227`) + ralph-loop counter `:149-160` (read-modify-write, WARNING only) + opt-in quality gates `:177-188`.
  **`bash` is NOT read-only → NEVER on the K4 parallel allow-list → the transformer never runs on a
  parallel-eligible call.** This refutes the doc-comment's stated rationale for THIS deployed hook.
- **`todo` (`:255-263`, rate-limit counter) / `edit` (`:267-279`, needs prior search):** state-dependent but
  neither is read-only → never parallelized → irrelevant to the K4 set.
- **Net:** for the parallel-eligible set (`agentgrep`, `read`, `lookup`), guard decisions are **input-pure**
  EXCEPT the discouraged whole-file read >400 lines, which is **fail-closed**. Running the guard concurrently
  per call is **safe (never a false-allow)** for the deployed config; worst case a conservative false-block.

### Verdict

**NOT truly mutually exclusive — only *implemented* that way.** The hook is a per-call function that already
executes inside each parallel worker via `Registry::execute`. The blanket disable at `parallel_tools.rs:300`
is a conservative approximation of "any pre_tool hook might be cross-call order-sensitive," not a structural
impossibility. Full resolution + trade-offs + blueprint in `REASON-K3K4-hook-vs-parallel.md`.

---

# K5 — implementation of `run_hooks_in_parallel` (2026-11-14)

Appended per the dispatch-context progress-doc convention (`findings.md` first, every change with
`file:line` + why). This section supersedes the "design delivered, NOT implemented" status above: the
owner greenlit the §c/§d implementation (verbatim: "1").

## Run provenance (recorded FIRST, per the dispatch skill)

| Item | Value | Source |
|------|-------|--------|
| Worktree | `/home/hermes/projects/kcode-wt/k3k4-merge` | `pwd` |
| Branch / HEAD | `k3k4-merge` @ `c41d2801a3ed45d1acbf3ae6bb0ecfb76ba5f059` | `git branch --show-current`, `git rev-parse HEAD` |
| Binary under test | `/home/hermes/projects/kcode-wt/k3k4-merge/.target/release/kcode` → `kcode v0.2.11125-dev (c41d2801a, dirty)` | `readlink -f ~/.local/bin/kcode` resolves to `/home/hermes/projects/kcode/target/release/kcode`, which does **not** exist; the worktree's own `.target/release/kcode` exists and prints the version |
| `CARGO_TARGET_DIR` | `/home/hermes/projects/kcode-wt/k3k4-merge/.target` (isolated; shared `~/.cargo-target` NOT used) | set before every cargo command |
| Worktree registry | `git worktree list` → `kcode` @ e22fbe712 [phase-3-jcode-adapter], `k3k4-merge` @ c41d2801a | `git worktree list` |
| Tree state at start | only `findings.md`/`plan.md` modified + scratch untracked; **no product source modified** | `git status --short` |

## Change 1 — the config field (`crates/kcode-config-types/src/lib.rs`)

- `pub run_hooks_in_parallel: bool` added at **`:569`**, after `max_concurrency` (`:553`), before the
  struct's closing brace (`:570`), with a doc comment at `:555-568` stating the operator's per-call-pure
  assertion and the default-false caveat.
- `run_hooks_in_parallel: false` added to `Default` at **`:578`**.
- **Why no serde change:** `#[serde(default)]` already sits on the struct at `:533`, so an existing
  `config.toml` without the key deserializes to `false` = today's behavior. Backward-compatible by
  construction, which is what makes this a config-driven opt-in rather than a behavior change.
- **Why `false`:** the conservative default must stay "any configured `pre_tool` hook ⇒ sequential".
  A stock config must behave exactly as it did before this change.

## Change 2 — the shipped template + the default-config test (`crates/kcode-base/src/config/default_file.rs`)

- Template: 10 comment lines + `run_hooks_in_parallel = false` inserted after `max_concurrency = 1`
  (was `:583`), now at **`:584-593`**. The comment states the purity precondition, the fail-closed caveat,
  and that leaving it false keeps the round sequential with a one-time warning.
- Test: ADDITIVE `assert!(!config.parallel_tools.run_hooks_in_parallel, "the shipped user config must
  keep a configured pre_tool hook serializing the round")` inserted at **`:1131-1136`**, immediately after
  the `read_only_tools.is_empty()` assertion. **No existing assertion was modified** — `!enabled` (`:1114-1117`),
  `max_concurrency == 1` (`:1118`), `read_only_tools.is_empty()` (`:1119-1122`), and `display.show_thinking`
  are byte-identical.
- **Why the key is written explicitly** (not left commented like `read_only_tools`): the shipped template is
  the operator's documentation, and `enabled`/`max_concurrency` are both written out. It also makes the
  additive assert read the template's own value rather than a serde default.

## Change 3 — the gate, the one-time warning, and the stale doc-comment (`crates/kcode-app-core/src/agent/parallel_tools.rs`)

- Import `use std::sync::atomic::{AtomicBool, Ordering};` added at **`:51`**. (`Ordering` is NOT reachable
  via `super::*` — `agent.rs:36-65` has no `atomic` import — so this was correction #1 of 3.)
- Doc-comment on `parallel_disabled_by_hooks` rewritten: was `:263-271` (the "transformer adds a path
  another call used" rationale), now **`:264-284`**, documenting that the hook runs per call inside
  `Registry::execute` which the parallel path shares, that the rationale only holds for a cross-call
  stateful hook, the two config states, and the fail-closed caveat.
- New `static WARNED_PARALLEL_DISABLED_BY_HOOKS: AtomicBool` + `pub(super) fn
  warn_parallel_disabled_by_hooks_once()` at **`:291-311`**: `swap(true, Ordering::Relaxed)` early-return
  (once per process), then one `crate::logging::warn(...)` naming the exact knob.
- Gate at **`:335-346`** (was `:300`), verbatim per blueprint §c:
  ```rust
  if !cfg.enabled {
      return Ok(handled);
  }
  if parallel_disabled_by_hooks() && !cfg.run_hooks_in_parallel {
      warn_parallel_disabled_by_hooks_once();
      return Ok(handled);
  }
  ```
  **No `Registry::execute` change** — each worker already runs the hook per call
  (`tool_handoff.rs:42` → `tool/mod.rs:1087-1112`).
- Method doc-comment at `:327-329` updated to mention the opt-in.
- **Deviation — telemetry counter OMITTED** (allowed by the task's "if trivially available (else omit and
  note)"): no existing counter has "round serialized by hook" semantics, and reusing `record_tool_call()`
  would inflate a real usage metric with a non-call event. The once-per-process `logging::warn` is the
  never-silent guarantee.

## Change 4 — the protected test file (`crates/kcode-app-core/src/agent/parallel_tools_tests.rs`)

- The three `ParallelToolsConfig` literals now carry the field: **`:285`** (inside `enabled_config`, with a
  2-line comment), **`:960`**, **`:990`**. Mechanical compile fix only — no assertion touched.
- New proof test `a_pre_tool_hook_runs_inside_parallel_workers_when_opted_in` at **`:1101-1223`**, with its
  merged probe `HookedOverlapProbe` at **`:1225-1269`**. It writes a sandbox `config.toml`
  (`[hooks] pre_tool = <sandbox>/allow.sh` + `[parallel_tools] enabled=true, run_hooks_in_parallel=true,
  read_only_tools=["read"], max_concurrency=4`) plus an executable `allow.sh` that appends one marker line
  per invocation, then `invalidate_config_cache()`, and asserts the 3-way proof:
  (i) marker lines == 3, (ii) `max_in_flight >= 2`, (iii) `tool_result_ids == [c1,c2,c3]` while
  `completion_order == [c3,c2,c1]`.
- **Why the config is read back through `crate::config().parallel_tools`** rather than hand-built: the test
  then proves the operator-visible contract (a config file with the opt-in actually enables the behavior),
  not just that a struct literal makes the gate branch.
- **Why the `assert!(parallel_disabled_by_hooks(), ...)` guard:** `hooks_suppressed()` (`hooks.rs:357-359`)
  is true iff `KCODE_HOOKS_DISABLED` is set in the parent env, and `AuthTestSandbox` does not clear it. The
  guard makes a leaked env var fail the test loudly instead of silently measuring a no-hook run.

## Contract change (stated explicitly, per the task)

Before: "a configured `pre_tool` hook always forces the round sequential."
After: "a configured `pre_tool` hook forces the round sequential **unless** the operator opts in with
`[parallel_tools] run_hooks_in_parallel = true`, in which case the hook runs once per call inside each
parallel worker." The default (`false`) is unchanged behavior, and the residual hazard is fail-closed.

## Status

All four file changes applied. Done-check (`cargo check --workspace`, `cargo test -p kcode-app-core --lib
-- parallel_tools`, default-config test) is being run in the isolated `.target` — see `.kcode/progress.md`
for the live log and the verbatim outputs.
