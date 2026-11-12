<div align="center">

<p>
<a href="https://trendshift.io/repositories/25042?utm_source=repository-badge&amp;utm_medium=badge&amp;utm_campaign=badge-repository-25042" target="_blank" rel="noopener noreferrer"><img src="https://trendshift.io/api/badge/repositories/25042" alt="1jehuang/kcode | Trendshift" width="250" height="55"></a>
</p>

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/brand/kcode-mark-inverse.svg">
  <img src="assets/brand/kcode-mark.svg" alt="kcode halftone donut logo" width="160">
</picture>

# kcode

<p>
  <strong>High-performance coding agents.</strong><br>
  Run dozens of agents in parallel without melting your machine. More agents, less RAM, no lag.
</p>

[![Last Commit](https://img.shields.io/github/last-commit/1jehuang/kcode/master?style=flat-square&logo=github)](https://github.com/1jehuang/kcode/commits/master)
[![GitHub Stars](https://img.shields.io/github/stars/1jehuang/kcode?style=flat-square&logo=github)](https://github.com/1jehuang/kcode/stargazers)
[![GitHub Downloads](https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2F1jehuang%2Fkcode%2Fmaster%2Fdocs%2Fbadges%2Fdownloads.json&style=flat-square&logo=github)](https://github.com/1jehuang/kcode/releases)

[![Discord](https://img.shields.io/badge/Discord-Join%20Community-5865F2?style=flat-square&logo=discord&logoColor=white)](https://discord.gg/nBe9vGyK9a)
[![Y Combinator](https://img.shields.io/badge/Y%20Combinator-S26-F26625?style=flat-square&logo=ycombinator&logoColor=white)](https://www.ycombinator.com/companies/kcode)
[![X (Twitter)](https://img.shields.io/badge/follow-%401jehuang-000000?style=flat-square&logo=x&logoColor=white)](https://x.com/1jehuang)

</div>

<div align="center">

  <a href="https://github.com/1jehuang/kcode/releases/download/readme-assets/kcode-performance-demo-2026-10.mp4">
    <img src="https://github.com/1jehuang/kcode/releases/download/readme-assets/kcode-performance-demo-2026-10-30fps.webp" alt="kcode performance demonstration" width="900">
  </a>

  <p><em>kcode performance demonstration (1x speed)</em></p>

<a href="https://github.com/1jehuang/kcode/releases/download/readme-assets/kcode-yc-launch.mp4">
  <img src="https://github.com/1jehuang/kcode/releases/download/readme-assets/kcode-yc-launch.webp" alt="kcode YC launch video" width="800">
</a>

</div>

<div align="center">

## Installation

</div>

```bash
# macOS & Linux
curl -fsSL https://kcode.sh/install | bash
```

<div align="center">

## Benches

</div>

### RAM per additional session (headless)

| Tool | First session | Each additional session | Comparison (additional) |
|---|---|---|---|
| **kcode** | **71.3 MB** | **~3.3 MB** | baseline |
| **OpenCode** | 637.1 MB | ~5.9 MB | **1.8× more RAM** |
| **pi** | 84.0 MB | ~79.6 MB | **24.1× more RAM** |
| **Claude Code** | 272.9 MB | ~169.3 MB | **51.3× more RAM** |
| **Codex CLI** | 398.6 MB | ~194.1 MB | **58.8× more RAM** |

[Reproduce](scripts/bench_headless_memory.py).

### Time to first input

Time until typed text appears on screen, across 10 interactive launches.

| Tool | Time to first input | Comparison |
|---|---|---|
| **kcode** | **19.1 ms** | baseline |
| **pi** | 596.4 ms | **31.2× slower** |
| **Codex CLI** | 905.8 ms | **47.4× slower** |
| **OpenCode** | 1047.9 ms | **54.9× slower** |
| **Claude Code** | 3512.8 ms | **183.9× slower** |

Full numbers on [kcode.sh](https://kcode.sh).

<div align="center">

## Screenshots

[Browse the interactive gallery on kcode.sh](https://kcode.sh/#screenshots).

<table>
  <tr>
    <td align="center"><a href="https://kcode.sh/screenshots/inline-image-rendering.webp"><img src="https://kcode.sh/screenshots/thumbs/inline-image-rendering.webp" alt="Kcode rendering screenshots inline after reading image files" width="440" /></a><br>Inline images</td>
    <td align="center"><a href="https://kcode.sh/screenshots/resume-menu.webp"><img src="https://kcode.sh/screenshots/thumbs/resume-menu.webp" alt="Kcode resume menu with a session list and preview" width="440" /></a><br>Resume menu</td>
  </tr>
  <tr>
    <td align="center"><a href="https://kcode.sh/screenshots/latex-math-rendering.webp"><img src="https://kcode.sh/screenshots/thumbs/latex-math-rendering.webp" alt="Kcode rendering LaTeX mathematics inline" width="440" /></a><br>Inline LaTeX</td>
    <td align="center"><a href="https://kcode.sh/screenshots/cache-cold-notification.webp"><img src="https://kcode.sh/screenshots/thumbs/cache-cold-notification.webp" alt="Kcode warning that the prompt cache went cold" width="440" /></a><br>Cache cold notification</td>
  </tr>
</table>

</div>

## Swarm

Spawn two or more agents in the same repo, and they will automatically be managed by the server to allow native collaboration. When agent A edits a file that agent B has read (code shifting under its feet), the server notifies agent B. Agent B can ignore it if it is not relevant, or it can check the diff to make sure that it doesn't conflict. Each agent has messaging abilities, capable of DMing just one agent, broadcasting to all other agents hosted by the server, or just agents working in that repo. This allows you to spawn multiple sessions in the same repo, and have all conflicts automatically resolved.

Agents are also able to spawn their own swarms autonomously. They have a swarm tool which allows them to spawn in their own teamates to accomplish tasks in parallel. Doing so turns the main agent into a coordinator and the spawned agents into workers. Groups of agents, their messaging channels, their completion statuses, etc are all automatically managed. This can be done headlessly or headed.


## OAuth and Providers

Supports a [wide range of providers](https://kcode.sh/docs#providers), including Claude OAuth and OpenAI Codex OAuth.

## Customizability / Self-Dev

Kcode is inventing a new form of customizability. One that doesn't limit you to what a plugin or extension can do. Tell your kcode agent to enter self dev mode, and it will start modifying its own source code. Kcode is optimized to iterate on itself. There is significant infrastructure around self development, which allows it to edit, build, and test its own source code, then reload its own binary and continue work in your (potentially many) sessions, fully automatically.


<!-- Add self-dev demo thumbnail/video and fuller writeup here. -->

## Other

- [Token optimization](docs/HARNESS.md#token-optimization)
- [KV cache](docs/HARNESS.md#kv-cache)
- [Hill-climbable measurements](docs/HARNESS.md#hill-climbable-measurements)

## kcode v2: Contract-First Coding Harness

This fork adds a disciplined execution layer on top of kcode's existing swarm and TUI capabilities.

**The problem we solved:** Unattended coding runs that loop for 12+ hours auditing their own audit trail, with no clear end condition and no enforced tool policy.

**What changed:**

- **Contract-first tickets.** Every task carries a written Goal, Hard Boundaries, Anti-Loop Rules, and a mechanical Done-Check (artifact existence + `cargo check --workspace` exit 0). The agent reads the contract before touching code.
- **Anti-loop enforcement.** MAX-RETRIES 2, MAX-TURNS 10. Failed compilation halts the run. No fake output, no "one more try" spirals.
- **Tool policy.** `ToolPolicy::None | Auto | Required | Named(name)` enforced at the harness level. Probes required-tool support at startup; degrades `Required → Auto` on HTTP 400. CLI flags `--tool-profile`, `--tools`, `--disabled-tools`.
- **Speed.** A Phase 1 integration task (vendoring 3 upstream crates, creating 2 new workspace crates, fixing 2 real bugs, secret scan clean) completed in under 1 hour. Old equivalent runs on upstream took 12+ hours.
- **Deterministic stop.** The run ends when the Done-Check passes or the retry limit is hit. No interpretation, no "is this good enough?" loops.

**When to use kcode v2 vs upstream:**

- Use upstream kcode for interactive TUI sessions, OAuth flows, and self-dev mode.
- Use kcode v2 for headless ticket execution where you need the agent to stop on its own.

[Full spec](docs/kcode-v2-contract-spec.md) · [Tool policy details](docs/TOOL-POLICY.md) · [Changelog](changelog)
