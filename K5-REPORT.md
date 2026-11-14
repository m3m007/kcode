# K5 REPORT: `run_hooks_in_parallel` — make the `pre_tool` hook and parallel read-only tools composable

Status: **COMPLETE**. All four done-checks exit 0 (observed from real output, quoted verbatim below). Local commit on `k3k4-merge` only — **no push**.

---

## 1. Provenance (recorded first, before any change)

| Fact | Value | Source |
|---|---|---|
| Worktree | `/home/hermes/projects/kcode-wt/k3k4-merge` | live `pwd` / git |
| Branch | `k3k4-merge` | `git rev-parse --abbrev-ref HEAD` |
| HEAD before my changes | `c41d2801a3ed45d1acbf3ae6bb0ecfb76ba5f059` | `git rev-parse HEAD` |
| Git identity (used as-is, not overridden) | `user.name=reidarmb`, `user.email=reidarmb@gmail.com` | `git config user.name` / `user.email` |
| Binary under test | `/home/hermes/projects/kcode-wt/k3k4-merge/.target/release/kcode` | `ls -la` |
| Version reported by that binary | `kcode v0.2.11125-dev (c41d2801a, dirty)` | `kcode --version` |
| Build env | `CARGO_TARGET_DIR=$PWD/.target` (isolated worktree target dir) | `export` in every cargo invocation |
| `~/.local/bin/kcode` symlink | **dangling** — not usable as a binary source | `ls -la` / `readlink` |
| Design doc approved | `REASON-K3K4-hook-vs-parallel.md` | repo doc |
| Owner authorization | §c/§d implementation greenlit verbatim `'1'` | owner message |

Note on the binary: the `--version` above was captured *before* my four source edits; it is the pre-change provenance of the codebase I worked on. It is not evidence about my change. The evidence about my change is the four done-checks in §4, all run after the edits.

---

## 2. What the task was, and what was actually wrong

The stated assumption was that K4's parallel read-only tool execution and the `pre_tool` tool-guard hook were mutually exclusive. Reading the code showed they are not. The hook runs **per call, inside `Registry::execute`** (`crates/kcode-app-core/src/tool/mod.rs:1087-1112`, reached from `tool_handoff.rs:42`), and the parallel path calls that same `Registry::execute` from each worker. So parallelism never bypasses the hook — it only changes *when* the hook runs relative to the other calls.

The only thing that made them exclusive was a single blanket short-circuit at `parallel_tools.rs:300` (old line numbering): `if !cfg.enabled || parallel_disabled_by_hooks() { return }`. That condition forced the whole round down the sequential path whenever any `pre_tool` hook was configured, regardless of whether the hook was a pure per-call function.

Two problems with that status quo, both fixed here:

1. **The features were needlessly exclusive.** An operator with a stateless allow-list gate got no parallelism at all.
2. **The interlock was silent.** The seam simply returned an empty set, so a user who configured a hook and expected parallelism saw a feature that looked broken rather than deliberately disabled.

### The contract change (explicit)

> **Before:** "a configured `pre_tool` hook forces the round sequential."
>
> **After:** "a configured `pre_tool` hook forces the round sequential **unless** the operator sets `[parallel_tools] run_hooks_in_parallel = true`, in which case the hook runs once per call inside each parallel worker."

Default is `false`, which is byte-for-byte today's behavior. `#[serde(default)]` on `ParallelToolsConfig` means an existing config that does not mention the key loads as `false`.

---

## 3. The four file changes

### 3.1 `crates/kcode-config-types/src/lib.rs`

- `:554-569` — new `pub run_hooks_in_parallel: bool` field with doc comment explaining what the operator is asserting (hook is a pure per-call function of its input; no cross-call state, counters, ordering, or rate limits) and what the residual hazard is.
- `:578` — `run_hooks_in_parallel: false` in `impl Default for ParallelToolsConfig`.

**Why here:** this is the single source of truth for the config struct. Putting the field here, with `#[serde(default)]` already on the struct (`crates/kcode-config-types/src/lib.rs:533`, struct at `:534`, verified by reading), is what makes older configs load as `false` rather than failing to deserialize.

### 3.2 `crates/kcode-base/src/config/default_file.rs`

- `:584-593` — documented the new key in the shipped `config.toml` template, directly after `max_concurrency`, with the operator-facing rationale and the one-time-warning behavior.
- `:1133-1136` — one **additive** assertion in `default_config_template_parses`:
  ```rust
  assert!(
      !config.parallel_tools.run_hooks_in_parallel,
      "the shipped user config must keep a configured pre_tool hook serializing the round"
  );
  ```

**Why here:** the template is what users actually copy, so the key must be discoverable. **No pre-existing assertion was modified or removed** — this is a new `assert!` appended after the existing `read_only_tools.is_empty()` check, which is what makes the shipped default a tested invariant rather than a comment.

### 3.3 `crates/kcode-app-core/src/agent/parallel_tools.rs`

- `:51` — added `use std::sync::atomic::{AtomicBool, Ordering};`.
- `:264-287` — rewrote the doc comment on `parallel_disabled_by_hooks()`. The old comment claimed the two features were incompatible ("Rather than accept that, the parallel path steps aside entirely"). It now states the accurate reasoning: this is the *conservative default's* trigger, not an incompatibility claim, and it names the exact residual hazard (cross-call state) plus the fail-closed direction (a state-dependent hook can only block a call it would have allowed, never pass one it would have blocked).
- `:293-311` — new `static WARNED_PARALLEL_DISABLED_BY_HOOKS: AtomicBool` and `pub(super) fn warn_parallel_disabled_by_hooks_once()`. Uses `swap(true, Ordering::Relaxed)` so a long-lived session logs once, not once per round.
- `:335-347` — the gate in `execute_parallelizable_tools`, split from one condition into two:
  ```rust
  if !cfg.enabled {
      return Ok(handled);
  }
  if parallel_disabled_by_hooks() && !cfg.run_hooks_in_parallel {
      // The operator has not asserted their hook is per-call-pure, so the
      // round stays sequential. Say so once: a silent interlock reads as a
      // broken feature, not a deliberate one.
      warn_parallel_disabled_by_hooks_once();
      return Ok(handled);
  }
  ```
- `:324-326` — updated the `execute_parallelizable_tools` doc comment to mention the opt-in.

**Why here:** this is the single place the exclusion decision is made. **No change was made to `Registry::execute`, the dispatch path, or any worker.** The hook already ran per call; only the decision to run the round concurrently changed.

### 3.4 `crates/kcode-app-core/src/agent/parallel_tools_tests.rs`

- `:283-285`, `:960`, `:990` — added `run_hooks_in_parallel: false` to the three existing config literals. **Mechanical compile fix only.** No existing assertion was touched.
- `:1084-1269` — new test `a_pre_tool_hook_runs_inside_parallel_workers_when_opted_in` plus its probe `HookedOverlapProbe`.

**The proof test, and why it cannot pass vacuously:**

The hook is a real config-file hook invoked as a subprocess, written into the sandbox's `KCODE_HOME/config.toml` exactly as an operator's script would be — not a test seam. Before measuring anything, the test asserts the setup is real:

```rust
assert!(
    parallel_disabled_by_hooks(),
    "the sandbox config must actually configure a pre_tool hook"
);
```

so if the hook silently failed to load, the test fails at setup rather than passing on a no-hook run.

Then it asserts all three properties of the composability claim:

1. **The hook ran once per call inside the workers** — marker line count must be exactly 3 for 3 calls.
2. **The calls overlapped** — `max_in_flight >= 2`.
3. **The merge ignores completion order** — results must be in the model's call order `[c1, c2, c3]` while the probe is deliberately built to *finish* in reverse `[c3, c2, c1]`. If the merge had inherited completion order, the test would catch it.

---

## 4. Done-checks — verbatim output and exit codes

Every exit code below is quoted from the captured log. **None of these results is claimed without having seen it.**

### Done-check 1 — `cargo check --workspace` (background task `2962039fyk`)

```
    Checking kcode-app-core v0.1.0 (/home/hermes/projects/kcode-wt/k3k4-merge/crates/kcode-app-core)
    Checking kcode v0.2.0 (/home/hermes/projects/kcode-wt/k3k4-merge)
    Finished `dev` profile [unoptimized] target(s) in 44.88s
EXIT_CODE=0
```
Exit code `0`, duration 45.44s. The 13 `warning` lines in that log are pre-existing workspace warnings; `grep -i "parallel_tools\|run_hooks"` over the log returns **0 hits**, so none come from my four files.

*Proves compilation. Says nothing about behavior.*

### Done-check 2 — `cargo test -p kcode-app-core --lib -- parallel_tools` (background task `352429ojy5`)

```
running 17 tests
test agent::parallel_tools_tests::invalid_calls_and_sdk_results_are_never_parallelized ... ok
test agent::parallel_tools_tests::a_populated_allow_list_does_not_override_the_master_switch ... ok
test agent::parallel_tools_tests::an_empty_allow_list_parallelizes_nothing ... ok
test agent::parallel_tools_tests::a_pre_tool_hook_sends_the_round_down_the_sequential_path ... ok
test agent::parallel_tools_tests::disabled_by_default_so_a_round_is_untouched ... ok
test agent::parallel_tools_tests::a_single_read_only_call_is_left_sequential ... ok
test agent::parallel_tools_tests::partition_uses_maximal_runs_cut_at_the_first_sequential_call ... ok
test agent::parallel_tools_tests::a_pre_tool_hook_runs_inside_parallel_workers_when_opted_in ... ok
test agent::parallel_tools_tests::enabled_read_only_calls_overlap_in_time ... ok
test agent::parallel_tools_tests::merged_results_keep_the_models_original_call_order ... ok
test agent::parallel_tools_tests::one_failure_does_not_disturb_its_neighbours ... ok
test agent::parallel_tools_tests::panicking_call_does_not_take_down_the_round ... ok
test agent::parallel_tools_tests::parallel_failure_lands_in_its_own_slot_and_keeps_the_round_valid ... ok
test agent::parallel_tools_tests::sequential_tail_of_a_mixed_round_stays_sequential ... ok
test agent::parallel_tools_tests::zero_max_concurrency_is_not_an_unbounded_fan_out ... ok
test agent::parallel_tools_tests::two_identical_rounds_produce_identical_context_bytes ... ok
test agent::parallel_tools_tests::max_concurrency_bounds_the_fan_out ... ok
test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 1974 filtered out; finished in 0.75s
EXIT_CODE=0
```
Exit code `0`, duration 104.08s. 16 pre-existing tests + the new proof test, **0 failed**. Note `a_pre_tool_hook_sends_the_round_down_the_sequential_path ... ok` — the test that pins the *default* behavior still passes unmodified, which is the regression guard for "default `false` == today's behavior."

### Done-check 3 — `cargo test -p kcode-base --lib -- default_config_template_parses` (background task `467930nrmj`)

```
running 1 test
test config::default_file::tests::default_config_template_parses ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 2186 filtered out; finished in 0.01s
EXIT_CODE=0
```
Exit code `0`, duration 211.43s. This exercises the additive assertion at `default_file.rs:1133-1136`, proving the shipped default is still `false`.

### Done-check 4 — same suite as #2, on the tree actually committed (background task `7643866x4g`)

```
running 17 tests
test agent::parallel_tools_tests::a_pre_tool_hook_runs_inside_parallel_workers_when_opted_in ... ok
test agent::parallel_tools_tests::a_pre_tool_hook_sends_the_round_down_the_sequential_path ... ok
test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 1974 filtered out; finished in 0.76s
EXIT_CODE=0
```
Exit code `0`, duration 55.99s. **This is the run that validates the commit.** It was executed after I removed a temporary diagnostic (see §5), so it measures exactly the tree that was committed. `grep -c "K5PROOF" /tmp/k5-final-tests.log` → `0`, confirming the temporary diagnostic left no trace.

---

## 5. Measured proof values

A passing `assert_eq!` prints nothing, so the measured values were surfaced separately with `--nocapture` and a temporary `eprintln!` (background task `696333pt7d`, exit 0):

```
K5PROOF marker_lines=3 overlap=3 ids=["c1", "c2", "c3"]
test agent::parallel_tools_tests::a_pre_tool_hook_runs_inside_parallel_workers_when_opted_in ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 1990 filtered out; finished in 0.20s
EXIT_CODE=0
```

| Measured value | Required by | Meaning |
|---|---|---|
| `marker_lines=3` | `assert_eq!(marker_lines, 3)` | The `pre_tool` hook ran **once per call inside the workers**, not once per round. This is the core of the task. |
| `overlap=3` | `assert!(overlap >= 2)` | All three calls were genuinely in flight simultaneously. The round really was parallel, not accidentally serialized. |
| `ids=["c1", "c2", "c3"]` | `assert_eq!(ids, vec!["c1", "c2", "c3"])` | Results merged in the model's call order. |
| completion order `["c3", "c2", "c1"]` | `assert_eq!(order, vec!["c3", "c2", "c1"])` | The probe finishes in **reverse**, so a pass proves the merge ignores completion order rather than inheriting it. Transcript bytes and prompt cache are unchanged. |

**The temporary `eprintln!` was removed before committing**, and done-check 4 re-ran the full suite on the cleaned tree to prove the committed source still passes. The removal is a planned cleanup, not a correction.

---

## 6. Deviations

**One deviation, as anticipated by the task.**

1. **Telemetry counter omitted.** The task allowed "a telemetry counter if trivially available (else omit and note)". It is not trivially available here: no existing counter has "round serialized by hook" semantics, and reusing `record_tool_call()` would inflate a real usage metric with events that are not tool calls. Inventing a new counter would mean touching the telemetry surface for a diagnostic nicety, which is not proportionate to the change.
   **The never-silent guarantee is instead the once-per-process `logging::warn`** in `warn_parallel_disabled_by_hooks_once()` (`parallel_tools.rs:293-311`). One `WARN` per process tells the operator exactly which knob changes the behavior, without repeating for every round in a long-lived session.

No other deviations. In particular: no existing assertion was modified, no test was weakened, no `unwrap`/`expect`/`panic!`/`#[allow]` was introduced, and `Registry::execute` is untouched.

---

## 7. Final diff

```
 crates/kcode-app-core/src/agent/parallel_tools.rs  |  65 +++++--
 .../src/agent/parallel_tools_tests.rs              | 186 +++++++++++++++++++++
 crates/kcode-base/src/config/default_file.rs       |  14 ++
 crates/kcode-config-types/src/lib.rs               |  17 ++
 4 files changed, 272 insertions(+), 10 deletions(-)
```

Plus progress docs: `findings.md` (K5 section), `plan.md` (K5 section), `.kcode/progress.md`.

---

## 8. Summary

The `pre_tool` hook and K4's parallel read-only tool execution were never truly incompatible — the hook already ran per call inside each parallel worker. The only thing excluding them was one blanket short-circuit and a silent interlock. This change makes the exclusion an explicit, operator-controlled, default-off decision:

- `run_hooks_in_parallel = false` (default): unchanged behavior, plus a one-time warning so the disable is never silent.
- `run_hooks_in_parallel = true`: the round runs concurrently and the hook runs once per call inside each worker.

Four done-checks, all exit 0, all observed from real output. Committed locally on `k3k4-merge`. **Not pushed.**
