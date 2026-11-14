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
