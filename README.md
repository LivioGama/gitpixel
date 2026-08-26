# gitpixel

Fast, always-fresh code retrieval for LLM agents. A Rust sidecar that replaces
grep-style scanning and stale code-graph tools with an indexed engine that
stays correct mid-session.

## What it does

- **Indexed regex search** — trigram shard (mmapped, delta-varint postings),
  regex→boolean query planning (Cox algebra over `regex-syntax` HIR), candidate
  verification with ripgrep's matcher crates. Sound by construction: the index
  can only over-approximate; verification is authoritative.
- **Git-anchored freshness** — base shard pinned to a commit OID, a
  committed-delta layer on HEAD moves (`git diff --name-status`), and an
  in-memory dirty overlay for uncommitted/agent edits fed by an fs watcher.
  Query merge subtracts tombstones before unioning the overlay, so stale
  results are structurally impossible.
- **Code graph** — tree-sitter extraction (TS/TSX/JS, Rust, Go, Java, Python)
  into SQLite; tiered call resolution (exact same-file → import-resolved →
  unique-name probable) that **never fans out** ambiguous names into edges;
  unresolved calls surface through an epistemic envelope
  (`lower_bound: true` + count) so an agent can tell "0 callers" from
  "resolver gave up".
- **Analyses** — blast-radius `impact` (d1 WILL BREAK / d2 LIKELY / d3 MAY
  NEED TESTING, LOW..CRITICAL risk), `uses` (callers/callees with confidence
  tiers), `trace` A→B, `processes` (entry-point BFS execution flows),
  `clusters` (label-propagation functional areas), `changes` (git diff hunks →
  symbols → affected flows → risk), token-budgeted `context`.
- **Serving** — one core `Service`; CLI one-shot commands that transparently
  use a warm Unix-socket daemon (NDJSON protocol, fs watcher, idle timeout)
  when available. MCP is a planned thin adapter over the same `Service`.

## Quick start

```bash
cargo build --release
target/release/gitpixel index /path/to/repo          # text index (~300ms / 3K files)
target/release/gitpixel search 'handleClick' /path/to/repo --stats
target/release/gitpixel graph /path/to/repo          # build code graph
target/release/gitpixel impact someFunction /path/to/repo
target/release/gitpixel daemon start /path/to/repo   # warm daemon + watcher
```

## Design notes

- **Trigram is the default extractor, chosen by measurement.** Cursor-style
  sparse n-grams are fully implemented (`--extractor sparse`, property-tested
  against a brute-force oracle of the ClickHouse selection predicate) but lost
  the benchmark gate on real repos: 3–10× larger shards for identical
  identifier-query candidate sets. Numbers in [docs/bench/phase1.md](docs/bench/phase1.md).
- Measured on real repos (Apple Silicon): index build 299 ms for 3,319 files;
  cold-CLI search 9–21 ms vs ripgrep's 92–168 ms on the same queries with
  line-level output parity (zero missed, zero extra). See docs/bench/.
- Derived-code attribution in [NOTICE](NOTICE) (hypergrep, MIT).

## Status

Working v1 built from an approved phased plan (see docs/bench/ for the
verified parts). The graph layer and freshness daemon shipped fast-path
first: correctness harnesses and scale benchmarks for those layers are the
next milestone, so treat their outputs as best-effort until then.

MIT © Livio Gamassia
