# Tool Policy

Enforced at the harness level, not by prompt.

## Policy Levels

| Level | Behavior |
|-------|----------|
| `None` | No enforcement; agent chooses freely. |
| `Auto` | Tools available; agent may use or skip. |
| `Required` | Agent MUST use the listed tools. Harness blocks runs if support is missing. |
| `Named(name)` | Only the named tool is allowed for this operation. |

## Startup Probe

On launch, the harness probes required-tool support with 2 retries.
If a `Required` tool returns HTTP 400 (e.g., thinking-mode mismatch),
the policy degrades to `Auto` and the run continues.

## CLI Flags

- `--tool-profile <name>` — load a named tool set.
- `--tools <csv>` — override allowed tools.
- `--disabled-tools <csv>` — block specific tools.

## Key Set

The enforced key set (measured across 1,538 build commands):

| Tool | Purpose |
|------|---------|
| `rtk` | Smart grep/read replacement |
| `rg` | Fast content search |
| `gitleaks` | Secret scanning |
| `cargo-deny` | Dependency license/audit |
| `cargo fmt` + `clippy` | Formatting + lint |
| `graphicle` | Codebase graph queries |

Demoted with reasons: `ripsecrets`/`kingfisher` (overlap gitleaks),
`cargo-public-api` (overlaps cargo-modules), `cargo-machete`, `tokei`,
`difftastic`/`git-delta`, `bat`, `fd`, `rust-analyzer`, `sqlite3`, `jq`, `gh`.
