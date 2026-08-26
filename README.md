# 📋 gitpixel

> **Fast, always-fresh code retrieval for LLM agents.**

A Rust sidecar that replaces grep-style scanning and stale code-graph tools with an indexed engine that stays correct mid-session. Built so an agent can search, trace, and reason about a codebase without re-reading it every turn.

![Status](https://img.shields.io/badge/status-active-success)
![Type](https://img.shields.io/badge/type-tool-blue)
![Language](https://img.shields.io/badge/Rust-2024%20edition-orange)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

## ✨ Features

🔍 **Indexed regex search** — Trigram shard (mmapped, delta-varint postings), regex→boolean query planning (Cox algebra over `regex-syntax` HIR), candidate verification with ripgrep's matcher crates. Sound by construction: the index can only over-approximate; verification is authoritative.

⏱️ **Git-anchored freshness** — Base shard pinned to a commit OID, a committed-delta layer on HEAD moves (`git diff --name-status`), and an in-memory dirty overlay for uncommitted/agent edits fed by an fs watcher. Query merge subtracts tombstones before unioning the overlay; content-hash tests cover equal-size edits with restored mtimes.

🕸️ **Code graph** — Tree-sitter extraction (TS/TSX/JS, Rust, Go, Java, Python) into SQLite; tiered call resolution (exact same-file → import-resolved → unique-name probable) that **never fans out** ambiguous names into edges. Unresolved calls surface through an epistemic envelope (`lower_bound: true` + count) so an agent can tell "0 callers" from "resolver gave up".

📊 **Analyses** — Blast-radius `impact` (d1 WILL BREAK / d2 LIKELY / d3 MAY NEED TESTING, LOW..CRITICAL risk), `uses` (callers/callees with confidence tiers), `trace` A→B, `processes` (entry-point BFS execution flows), `clusters` (label-propagation functional areas), `changes` (git diff hunks → symbols → affected flows → risk), token-budgeted `context`.

⚡ **Serving** — One core `Service`; CLI one-shot commands that transparently use a warm Unix-socket daemon (NDJSON protocol, fs watcher, idle timeout) when available. MCP is a planned thin adapter over the same `Service`.

## 🔧 Installation

### From source (release build)

```bash
git clone https://github.com/LivioGama/gitpixel.git
cd gitpixel
cargo build --release
# → target/release/gitpixel
```

### Workspace layout

| Crate | Role |
|-------|------|
| `gitpixel-core` | Trigram/sparse index, shard, plan, verify, freshness overlay |
| `gitpixel-graph` | Tree-sitter extraction, call resolution, analyses |
| `gitpixel-context` | Token-budgeted context assembly |
| `gitpixel-serve` | `Service`, daemon, NDJSON API |
| `gitpixel-cli` | `gitpixel` binary — every command surface |
| `gitpixel-bench` | Criterion benchmarks (see `docs/bench/`) |

## 🚀 Quick Start

```bash
# Build the text index for a repo
target/release/gitpixel index /path/to/repo

# Regex search with candidate/timing stats
target/release/gitpixel search 'handleClick' /path/to/repo --stats

# Build the code graph (SQLite, tree-sitter)
target/release/gitpixel graph /path/to/repo

# Start a warm daemon + fs watcher for the repo
target/release/gitpixel daemon start /path/to/repo

# Agent bootstrap in one command: text index + graph + warm daemon
target/release/gitpixel ready /path/to/repo
```

## 📖 Usage Examples

### Search

```bash
# Plain text matches (path:line:text)
target/release/gitpixel search 'fn\s+handle' /path/to/repo
# → src/main.rs:42:fn handle_request(req: Request) -> Response

# NDJSON matches for tooling
target/release/gitpixel search 'TODO' /path/to/repo --json
# → {"path":"src/api.rs","line":108,"text":"// TODO: rate limit"}

# Candidate/timing stats to stderr
target/release/gitpixel search 'handleClick' /path/to/repo --stats
# → candidates=12 matches=3 elapsed_us=14210

# Responses default to 100 matches; retrieve the next page explicitly
target/release/gitpixel search 'TODO' /path/to/repo --limit 100 --offset 100
```

### Code graph

```bash
# Look up symbols by name
target/release/gitpixel symbol handleClick /path/to/repo
# → function  handleClick  src/ui.rs:14-38  src/ui.rs#handleClick#function

# Token-budgeted context for a symbol uid
target/release/gitpixel context 'src/ui.rs#handleClick#function' /path/to/repo --budget 4000

# Blast radius — what breaks if I change this symbol?
target/release/gitpixel impact someFunction /path/to/repo --direction upstream
# → d1 WILL BREAK: 2   d2 LIKELY: 5   d3 MAY NEED TESTING: 11   risk: HIGH

# Direct callers / callees with confidence tiers
target/release/gitpixel uses someFunction /path/to/repo --role callers
# → callers: 4
#   [exact]   line 22  function  callerA  src/mod.rs:22:callerA
#   [import]  line 88  function  callerB  src/api.rs:88:callerB

# Call path between two symbols
target/release/gitpixel trace handlerA handlerB /path/to/repo

# Discovered execution flows (entry-point BFS)
target/release/gitpixel processes /path/to/repo

# Functional-area clusters (label propagation)
target/release/gitpixel clusters /path/to/repo

# What symbols/flows are affected by working-tree changes
target/release/gitpixel changes /path/to/repo

# Every capped list command supports continuation
target/release/gitpixel uses someFunction /path/to/repo --role callers --offset 20
target/release/gitpixel processes /path/to/repo --offset 5
target/release/gitpixel clusters /path/to/repo --offset 50
target/release/gitpixel changes /path/to/repo --offset 20
```

### Daemon

```bash
# Start (background, fs watcher, idle timeout)
target/release/gitpixel daemon start /path/to/repo
# → daemon started ($TMPDIR/gitpixel-<root-hash>.sock)

# Status
target/release/gitpixel daemon status /path/to/repo
# → daemon running (...)

# Stop
target/release/gitpixel daemon stop /path/to/repo
# → daemon stopped
```

Search and graph-analysis commands transparently use the warm daemon when one is up (NDJSON over a Unix socket, ~100ms ping gate) and fall back to an in-process `Service` otherwise. Pass `--no-daemon` on search to force in-process.

### Agent bootstrap

Use `ready` as the first GitPixel command for a repository. It discovers the
repository root, ensures the text index and code graph are usable, then starts
the warm daemon. Pass `--no-daemon` when only preparing index artifacts.

```bash
target/release/gitpixel ready /path/to/repo
target/release/gitpixel ready /path/to/repo --no-daemon --json
```

## 🎨 Design Notes

- **Trigram is the default extractor.** Cursor-style sparse n-grams remain available through `--extractor sparse`; the historical exploratory measurements that informed the default are archived in [docs/bench/phase1.md](docs/bench/phase1.md), but are not a current reproducible performance claim.
- **Performance versus ripgrep or GitNexus is not yet claimed.** A publishable comparison still needs a pinned paired harness with raw trial artifacts, environment and component versions, commit SHAs, median/p95/confidence intervals, correctness oracles, subprocess counts, agent-facing operation counts, and measured token data.
- **Freshness has explicit regression coverage.** Tests exercise commit anchoring, dirty overlays, symlink rejection, equal-size edits with restored mtimes, deletions, and oversized-file bounds.
- **The graph never lies about ambiguity.** Ambiguous call names are not fanned out into edges; the resolver surfaces an epistemic envelope instead, so "0 callers" and "resolver gave up" are distinguishable.
- Derived-code attribution in [NOTICE](NOTICE) (hypergrep, MIT).

## 🌍 Platform Support

| Platform | Status | Notes |
|----------|--------|-------|
| macOS | ✅ | Primary dev target (Apple Silicon benchmarks) |
| Linux | ✅ | Unix-socket daemon, mmap shard |
| Windows | ⚠️ | Not yet tested — Unix-socket daemon path is Unix-only |

## ⚠️ Caveats — What's Verified vs What's Missing

### ✅ Verified (this build)

| Layer | Evidence |
|-------|----------|
| Text index + regex search | Workspace tests cover shard round-trips, regex planning, result limiting, malformed shards, symlink rejection, and oversized-file rejection |
| Freshness engine | Tests cover commit anchoring, dirty overlays, deletion, equal-size content changes with restored mtimes, and non-Git directory reopening |
| Code graph extraction | Tests cover extraction, receiver preservation, named-import specificity, wildcard-import ambiguity, incremental definition ambiguity, and test-container exclusion |
| Daemon | Tests cover Unicode framing, oversized-frame rejection, and absolute read deadlines; public start/status/search/stop and stale-protocol fallback paths are exercised before release |
| Token-budgeted context | Tests require the complete serialized response to remain inside 50- and 500-token budgets |

### ❌ Missing / Deferred (treat outputs as best-effort)

| Gap | Impact | Status |
|-----|--------|--------|
| **Graph correctness harnesses** | Call resolution tiers, epistemic envelopes not property-tested | Next milestone — do not trust graph outputs at scale until landed |
| **Daemon long-run stability** | No multi-hour soak test; idle timeout + watcher untested under load | Next milestone |
| **`processes` / `clusters` output quality** | Ran but not quality-checked on large repos; cluster boundaries may be coarse | Needs review on real monorepos |
| **`changes` symbol overlap** | Found dirty files but returned no symbol overlaps on a diff where hunks were outside indexed symbols | Worth a look when hardening — may miss hunks in non-indexed file types |
| **MCP adapter** | Not yet wired; planned as thin layer over `Service` | Planned |
| **Windows** | Unix-socket daemon path is Unix-only | Not supported |
| **Scale benchmarks** | Freshness daemon unbenchmarked on large monorepos | Next milestone |
| **GitNexus comparison** | No paired retrieval-quality, latency, subprocess, or token artifact exists yet | Required before claiming replacement performance |

### How to read graph outputs until harnesses land

- **`lower_bound: true`** in a response envelope = resolver gave up on N same-name call sites; the returned edges are a **lower bound**, not the full set. Treat "0 callers" + `lower_bound: true` as "unknown", not "unused".
- **`impact` risk tiers** (d1 WILL BREAK / d2 LIKELY / d3 MAY NEED TESTING) are structurally sound but depend on edge completeness — if the resolver dropped ambiguous edges, d2/d3 may under-count.
- **`changes`** only maps hunks that land inside indexed symbols (TS/TSX/JS/Rust/Go/Java/Python). Hunks in other file types or outside symbol ranges are reported as dirty files but produce no symbol overlaps.

## 📝 Status

Working v1 with bounded search/context responses, freshness regression coverage, tiered graph resolution, and a persistent local daemon. Graph property harnesses, long-run daemon testing, MCP transport, and reproducible comparisons remain open. See **Caveats** before relying on broad graph conclusions or performance claims.

## 🤝 Contributing

Contributions welcome. Especially useful right now — these close the gaps in **Caveats**:

1. **Correctness harnesses** for the graph layer (call resolution tiers, epistemic envelopes, property tests against a brute-force oracle).
2. **Scale benchmarks** for the freshness daemon on large monorepos (multi-hour soak, watcher under load).
3. **`changes` hunk coverage** — map hunks outside indexed symbols, handle non-indexed file types.
4. **MCP adapter** — thin layer over the existing `Service`.
5. **Language extractors** — additional tree-sitter grammars beyond the current set.

## 📝 License

MIT License — see [NOTICE](NOTICE) for derived-code attribution (hypergrep, MIT; ClickHouse sparse-grams algorithm, Apache-2.0).

## 🙋 Support

- **Issues**: Report bugs via [GitHub Issues](https://github.com/LivioGama/gitpixel/issues)
- **Discussions**: Ask questions in [GitHub Discussions](https://github.com/LivioGama/gitpixel/discussions)

---

<div align="center">

**Made with ❤️ for agents that need to read code fast**

[⭐ Star this repo](https://github.com/LivioGama/gitpixel) if it helps you!

</div>
