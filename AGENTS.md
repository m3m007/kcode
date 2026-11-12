# Repository Guidelines

## Repository Scope

- Kcode Desktop is in a separate repository.

## TUI style

- UI chrome must not wrap: startup headers, account rows, status labels, and
  other compact UI rows should fit their available terminal-cell width or
  truncate with an ellipsis. Count Unicode display cells, not characters.
  Preserve one logical row per visual row, including at narrow widths.
  Chat prose and other intentionally multiline content may wrap normally.
- Reuse the info widget's limit-bar renderer for usage indicators instead of
  introducing a different bar glyph, color, or used/remaining convention.

## Development Workflow

- **Commit and push as you go** - Make small, scoped commits and push them to
  the current branch's remote as meaningful work is completed, rather than
  waiting until the end of the task. Preserve unrelated changes and never
  force-push unless the user explicitly authorizes it.
- **Use the user's Git identity** - Create commits with the configured
  `user.name` and `user.email`. Do not override them with `Kcode`, `Kcode agent`,
  or a fabricated agent email. Preserve existing contributor attribution when
  integrating work. If no identity is configured, ask rather than inventing one.
- **Welcome pull requests from everyone** - Review contributions on their merits,
  regardless of whether the author is a maintainer, an existing contributor, a
  first-time contributor, or an agent. Good PRs can be merged directly after review
  and validation. Do not require a maintainer-authored rewrite merely because of
  who submitted the change. See `CONTRIBUTING.md` for the contribution policy.
- **Prefer master/main** - Work directly on the repository's primary branch
  (`master` or `main`) by default. Do not create a branch or worktree merely to
  start a task. Only do so when the user explicitly requests it or isolation is
  genuinely necessary, and explain the concrete reason first.
- **Keep work scoped** - Preserve unrelated work. If already on a branch with
  ongoing changes, finish and integrate safely rather than switching blindly.
  When the user asks you to review or integrate a PR or branch, you may inspect,
  test, and integrate that contribution regardless of author status. Do not pull
  in unrelated branches or merge a PR without user authorization.

## Provider model discovery

- Model arrays are offline fallbacks, not release allowlists. Providers with a
  catalog must discover new model IDs without a Kcode release and expose them
  through switching lists and structured model routes.
- Implement both cached prefetch and explicit refresh. Explicit refresh must
  bypass fresh caches. Preserve the last good catalog on network, authentication,
  or malformed-response failures, and scope persistent caches to their endpoint.
- Wire new providers into the shared refresh fanout and model-update publication,
  including long-lived sessions, not only startup or login.
- Regression tests must discover an arbitrary future model absent from all
  fallback arrays, using the provider's actual response schema. Check picker
  routes, forced refresh with a fresh cache, and last-good behavior on failure.
- If a backend has no discovery API, document that limitation instead of
  presenting a hardcoded list or a no-op refresh as live discovery.

## Install Notes
- `~/.local/bin/kcode` is the launcher symlink used from `PATH`.
- `~/.kcode/builds/current/kcode` is the active local/source-build channel; self-dev builds and `scripts/install_release.sh` point the launcher here.
- `~/.kcode/builds/stable/kcode` is the stable release channel; `scripts/install.sh` installs this and points the launcher here.
- `~/.kcode/builds/versions/<version>/kcode` stores immutable binaries.
- `~/.kcode/builds/canary/kcode` still exists for canary/testing flows, but it is not the primary self-dev install path.
- On Windows, the equivalents are `%LOCALAPPDATA%\\kcode\\bin\\kcode.exe` for the launcher, `%LOCALAPPDATA%\\kcode\\builds\\stable\\kcode.exe` for stable, and `%LOCALAPPDATA%\\kcode\\builds\\versions\\<version>\\kcode.exe` for immutable installs; `scripts/install.ps1` currently installs the stable channel.
- Ensure `~/.local/bin` is **before** `~/.cargo/bin` in `PATH`.

## Verifying a change at runtime

`cargo build` alone proves nothing about behavior. `kcode run` and interactive
sessions are served by the long-lived daemon at
`~/.kcode/builds/shared-server/kcode`, which is a symlink into
`~/.kcode/builds/versions/<version>/`. Until that symlink is repointed and the
daemon restarted (`kcode self-dev --build`), a freshly built binary is inert and
every runtime check silently measures the old code.

To test a change without disturbing the shared daemon or the caller's session,
run your build against its own socket:

```bash
cargo build --profile selfdev
./target/selfdev/kcode run --no-update --socket /run/user/1000/kcode-mytest.sock '<prompt>'
```

Two things that waste time otherwise:

- `crate::logging::info` writes to a log file, not stderr, so instrumenting a
  code path with it produces no visible output under `--trace`. Use `eprintln!`
  for throwaway diagnostics and delete it before committing.
- Confirm which binary you are actually inspecting. `strings` on
  `builds/shared-server/kcode` reads a 70-byte symlink, not a program; resolve it
  with `readlink -f` first.
