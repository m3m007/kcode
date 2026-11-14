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
