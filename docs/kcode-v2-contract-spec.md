# kcode v2 Contract Specification

A contract-first execution layer for headless coding tasks. Every ticket must carry
the following sections before dispatch.

## Ticket Contract

```markdown
## Goal
One sentence. What does done look like?

## Hard Boundaries
- Do not edit files outside this scope.
- Do not run network calls to non-allowlisted hosts.
- Do not commit without owner review.

## Anti-Loop Rules
- MAX-RETRIES: 2
- MAX-TURNS: 10
- Halt on: compilation failure, test failure, secret leak
- No fake output: if you cannot verify, say so and stop.

## Done-Check
Mechanical commands that exit 0 when the task is complete.
Example: `cargo check --workspace` + artifact existence check.

## Context
Background the agent needs: ticket IDs, commit hashes, relevant docs.
```

## Dispatch Rules

1. Read the ticket contract before any tool call.
2. Run the Done-Check after every meaningful change.
3. Stop when Done-Check passes or Anti-Loop Rules trigger.
4. Report deviations transparently; do not hide them.
5. Secrets found during the run are reported and redacted; never committed.

## Speed

Old upstream runs: 12+ hours of ralph-looping on audit trails.
kcode v2: under 1 hour for Phase 1 integration tasks.
The difference is the contract. The agent no longer guesses what "done" means.
