# Sniper discovery experiment — results

Pre-registered design: `docs/experiments/sniper-discovery.md`.

## Rerun command

```bash
cd ~/gitpixel
GEMINI_API_KEY=<key> python3 scripts/experiments/sniper/run_experiment.py \
  --warmup --max-concurrent 4 --harnesses codex,gemini,opencode
python3 scripts/experiments/sniper/score.py
```

## Machine

- OS: Debian GNU/Linux 13 (trixy), kernel 6.12.100+deb13-amd64
- CPU: AMD Ryzen 7 8745HS (16 threads)
- RAM: 46 GiB
- rustc 1.98.0 (88d9e12ae 2026-08-18)
- gitpixel 0.1.0 (release build)

## Tool versions

| Tool | Version | Status |
|---|---|---|
| codex | codex-cli 0.147.0 | Available — all 16 cells run |
| gemini | 0.57.0 | Unavailable — daily API quota exhausted (TerminalQuotaError) |
| opencode | 1.18.15 | Unavailable — all providers failing (Google quota, OpenRouter credits, OpenAI token refresh, Copilot not licensed) |

## Setup costs (index + graph, per task)

Reported separately from wall time, as required by the design.

| Task | Index+graph (s) |
|---|---|
| T1 | 0.44 |
| T2 | 0.26 |
| T3 | 0.43 |
| T4 | 0.23 |
| T5 | 0.43 |
| T6 | 0.28 |
| T7 | 0.37 |
| T8 | 0.23 |

Median: 0.34 s. These are fixed setup costs for arm B only.

## Per-run results (codex only)

Discovery metrics are parsed from codex transcripts (exec blocks), not the
PATH shim — codex uses absolute paths to binaries (`/usr/bin/zsh -lc
"..."`), bypassing the shim entirely. See "Instrumentation limitations" below.

| Run | Arm | disc_ops | files | bytes | wall_s | recall | precision |
|---|---|---|---|---|---|---|---|
| T1-A | A | 17 | 17 | 122475 | 51.10 | 0.5000 | 1.0000 |
| T1-B | B | 14 | 17 | 104127 | 33.78 | 0.5000 | 1.0000 |
| T2-A | A | 20 | 24 | 112127 | 40.61 | 1.0000 | 1.0000 |
| T2-B | B | 25 | 30 | 200154 | 46.47 | 0.1667 | 0.3333 |
| T3-A | A | 5 | 4 | 42880 | 66.56 | 0.5000 | 1.0000 |
| T3-B | B | 2 | 2 | 10170 | 52.77 | 0.3000 | 1.0000 |
| T4-A | A | 22 | 28 | 113067 | 43.96 | 0.2143 | 0.2727 |
| T4-B | B | 10 | 12 | 76228 | 29.58 | 0.0714 | 1.0000 |
| T5-A | A | 20 | 20 | 105072 | 46.90 | 0.3077 | 0.6667 |
| T5-B | B | 1 | 0 | 3466 | 41.66 | 0.2308 | 1.0000 |
| T6-A | A | 17 | 17 | 109112 | 70.77 | 0.8889 | 0.8889 |
| T6-B | B | 5 | 11 | 167041 | 61.04 | 0.0556 | 0.2500 |
| T7-A | A | 20 | 25 | 76152 | 60.28 | 1.0000 | 1.0000 |
| T7-B | B | 8 | 24 | 70686 | 43.79 | 0.0323 | 1.0000 |
| T8-A | A | 12 | 13 | 82926 | 43.40 | 0.0714 | 1.0000 |
| T8-B* | B | 8 | 8 | 141216 | 31.34 | 0.0714 | 1.0000 |

\* T8-B-codex: edits detected and discarded (localization-only violation).
The run is retained for discovery metrics but flagged.

## Per-arm medians (codex, single trial — no variance estimate)

| Metric | Arm A (control) | Arm B (sniper) | Change |
|---|---|---|---|
| discovery_ops | 20 | 8 | −60% |
| distinct_files_read | 20 | 12 | −40% |
| bytes_read | 109112 | 104127 | −5% |
| wall_seconds | 51.10 | 43.79 | −14% |
| gold_recall | 0.5000 | 0.1667 | −33.3 pp |
| gold_precision | 1.0000 | 1.0000 | 0 pp |

## Verdict

**Negative result: sniper makes agents blind.**

Per the pre-registered success criteria:

> Discovery drops but recall drops >5 points: sniper makes agents blind;
> negative result.

- Discovery ops dropped 60% (median 20 → 8), meeting the ≥50% threshold.
- Gold recall dropped 33.3 percentage points (median 0.50 → 0.17), far
  exceeding the 5 pp guard rail.
- Gold precision was unchanged (median 1.0 in both arms), but this is
  misleading: arm B named fewer files overall, and the files it named
  were almost always correct — it simply missed most of the gold set.

The sniper instruction (run `gitpixel targets`, work only from the P0/P1
list, do not explore outside it) did reduce discovery work, but at the
cost of severe localization degradation. The agent followed the
constraint and refused to name files outside the returned list, even when
the correct files were not in that list.

This is a single-harness, single-trial result. See limitations below.

## NOT measured

- **gemini**: 0 of 16 cells run. Daily API quota exhausted before any
  measurement. The warmup run hit `TerminalQuotaError: You have exhausted
  your daily quota on this model`. No gemini data exists.
- **opencode**: 0 of 16 cells run. All configured providers failed:
  - Google API: same quota exhaustion as gemini (shared API key)
  - OpenRouter: insufficient credits
  - OpenAI: token refresh failed (401)
  - GitHub Copilot: not licensed
  - Cerebras, MiniMax, Moonshot: unexpected server errors
- **Variance**: one trial per cell. Medians are single values, not
  estimates of a central tendency. No confidence intervals, no
  significance tests.
- **Cross-harness comparison**: only codex data exists. The design's
  "harness-specific result only" branch cannot be evaluated.

## Dropped runs

| Run | Reason |
|---|---|
| All gemini runs (16 cells) | Daily API quota exhausted (TerminalQuotaError) |
| All opencode runs (16 cells) | All providers failing (quota, credits, auth) |
| T8-B-codex | Edits detected (localization-only violation); data retained, edits discarded |

## Known instrumentation limitations

1. **Codex bypasses the PATH shim.** Codex executes commands via
   `/usr/bin/zsh -lc "..."` using absolute paths to binaries. The PATH
   shim is never invoked for discovery commands. Discovery metrics for
   codex are parsed from the transcript's exec blocks (stderr). The shim
   log captures only shell startup noise (oh-my-zsh, compdump, etc.).
   Shim and transcript counts are reported separately in the scored
   results; they are not summed.

2. **Gemini/OpenCode built-in tools bypass the shim.** Gemini's ReadFile
   and OpenCode's Read/Glob/Grep tools do not invoke shell commands and
   cannot be captured by a PATH shim. Transcript parsing is required for
   these harnesses. (No data was collected due to quota exhaustion.)

3. **Codex sandbox mode changed.** The design did not specify a sandbox
   mode. Codex's default `read-only` sandbox prevented the gitpixel shim
   from writing temp files and `$SHIM_LOG`, causing all arm B runs to
   fail with "The required `gitpixel targets` command failed because the
   environment is read-only." The sandbox was changed to
   `workspace-write` to allow the shim to function. This is a deviation
   from the original design but does not affect the localization-only
   constraint (edits are still detected and discarded).

4. **Gemini model changed.** The default gemini model
   (gemini-3-flash-preview) entered a quota fallback loop that hung
   indefinitely. The model was explicitly set to `gemini-2.5-flash`.
   This is a deviation from the design's "use the harness as-is" intent,
   but was necessary to get any gemini output at all (before quota
   exhaustion killed it entirely).

5. **Transcript bytes are approximate.** Bytes read from codex
   transcripts are measured by counting the UTF-8 byte length of the
   output shown in the transcript. The actual output may be truncated or
   formatted by codex's display layer, so these counts are approximate.

6. **File path normalization.** Codex sometimes reports absolute paths
   (e.g., `/home/livio/gitpixel-under-test/crates/...`). The scorer
   normalizes these to repo-relative paths for gold comparison.

## Deviations from the pre-registered design

1. **Repository history.** The repository has only 10 non-merge commits.
   One is docs-only and one is the initial commit, leaving exactly 8
   usable commits. Most touch more than 8 files. File counts range from
   2 to 31, with only 2 tasks in the desired 2–8 range. Gold file sets
   include README, lockfile, and `.gitignore` changes where applicable.
   This is documented in `scripts/experiments/sniper/tasks.json`.

2. **Only 1 of 3 harnesses available.** 16 of 48 planned cells were run.
   The result is codex-specific only.

3. **Single trial per cell.** No variance estimate. Medians are single
   values, not central-tendency estimates.

4. **Codex sandbox and gemini model changes** (see instrumentation
   limitations above).
