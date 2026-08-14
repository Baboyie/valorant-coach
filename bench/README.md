# bench — the §29 acceptance benchmark

Harness for the measurement ADR-001 §5 and §29 require. Two scripts:

| | |
|---|---|
| `Invoke-Benchmark.ps1` | runs **one** measured pass and writes CSVs |
| `Measure-Frames.ps1` | reads every pass and prints the §29 table |

`Invoke-Benchmark.ps1` deliberately does one run per invocation. Each run needs a
human flying an identical route, so a loop over the whole matrix would only create
the illusion that it was automated.

## Before the first run

- **Elevated PowerShell.** PresentMon opens an ETW session, which needs admin. The
  script checks and refuses early rather than failing 60 seconds into a route you
  have already played.
- **Valorant running and in the Range.** The Range is repeatable in a way live
  matches are not, which is the whole reason §29 specifies it.
- `cargo build --release` in `../recorder-proto` for the `ours` condition.

## Running the matrix

Three runs per condition, identical route each time:

```powershell
.\Invoke-Benchmark.ps1 -Condition baseline -Run 1
.\Invoke-Benchmark.ps1 -Condition baseline -Run 2
.\Invoke-Benchmark.ps1 -Condition baseline -Run 3
.\Invoke-Benchmark.ps1 -Condition ours -Run 1
.\Invoke-Benchmark.ps1 -Condition ours -Run 2
.\Invoke-Benchmark.ps1 -Condition ours -Run 3
```

Then:

```powershell
.\Measure-Frames.ps1 -Markdown
```

Do all six in one sitting. Frame timings move with driver state, background
processes and GPU temperature, so a baseline taken yesterday is not a control for a
recorder run taken today.

The recorder is started ~3 s before measurement begins and is allowed to finish on
its own afterwards. It is never killed: `finish()` writes the mp4 moov atom, and a
killed recorder leaves an unplayable file that reads like an encoder bug.

## Reading the table

**Frame-time standard deviation is the headline, not average FPS.** §20 is explicit
that a recorder with a good average and periodic hitches is worse competitively than
a slightly slower but consistent one. `Measure-Frames.ps1` prints the stddev delta
last and labels it, so it is the number the eye lands on.

`1% low` here means *the FPS implied by the 99th-percentile frame time* — the
FrameView/CapFrameX convention, not "mean of the slowest 1% of frames". Both are in
circulation and they do not produce the same number, so the definition ships with
the table.

`Encode %` comes from PresentMon's `--track_gpu_video` plus the
`engtype_VideoEncode` GPU counters. On the RTX 2060 this is NVENC, and it should be
close to zero in the `baseline` condition — if it is not, something else on the
machine is encoding and the run is contaminated.

## What this harness will not do

It does not touch the Valorant process. No injection, no hooks, no handle to the
game — see ADR §1, which rules that out permanently rather than as a default.

It uses the **PresentMon console build**, not the 2.x MSI. The MSI ships an in-game
overlay that hooks the target process to render itself, which is exactly the pattern
§1 forbids. Console build only, ETW only.

## Extending the matrix

`ShadowPlay` and `OBS` columns are the remaining two conditions in §5's matrix.
`Measure-Frames.ps1` already recognises `shadowplay-run*` and `obs-run*` filename
prefixes, so adding those conditions is a matter of running them, not of changing
the analysis.
