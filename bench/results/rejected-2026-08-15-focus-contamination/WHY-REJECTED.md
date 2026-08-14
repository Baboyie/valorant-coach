# Rejected: first §29 attempt, 2026-08-15

Six runs (3 baseline, 3 recording) on the i5-12400F / RTX 2060 rig. Kept as
evidence, **not** valid as a benchmark. Two independent contaminations, either of
which is disqualifying on its own.

## 1. The harness measured the operator alt-tabbing into the game

Valorant caps itself to ~30 fps when it is not the foreground window. The harness
started PresentMon immediately, but the operator launches it from a console — so
every run began with the game unfocused and its first second or so of frames
landing at ~33 ms.

| run | wall time below 40 fps |
|---|---|
| baseline 1 | 1.67% |
| baseline 2 | 2.00% |
| baseline 3 | 3.89% |
| ours 1 | **9.16%** |
| ours 2 | 3.39% |
| ours 3 | 4.66% |

The asymmetry is what makes this fatal rather than merely noisy. The `ours`
condition starts the recorder process first, which steals focus, so the extra
contamination landed on the recording arm and was indistinguishable from recorder
overhead. `ours run 1` also caught a genuine mid-run alt-tab, which alone dragged
the condition's 1% low from ~116 fps to 87.8.

Those frames land precisely on the percentile metrics §20 designates as the
headline. A harness that damages one arm of its own comparison is worse than no
harness.

## 2. Discord was recording the whole time

`discord_clips` (Discord's clip capture) held the GPU video-encode engine at a
constant **9.4%** across all three baseline runs — steady, not bursty. So:

- the "baseline" was never recorder-free; it was baseline plus another recorder, and
- our NVENC path was competing with Discord's for the same encode engine.

## What the rejected numbers said

Reported -13.96% average FPS, -35.68% 1% low, +0.964 ms frame-time stddev. Those
figures are **not** the recorder's cost and must not be quoted. The true cost is
unknown until a clean run exists.

## Fixes applied before re-running

- `Invoke-Benchmark.ps1` now blocks until Valorant is genuinely the foreground
  window, then settles 2 s, before any measurement starts.
- It polls focus at 10 Hz for the whole run and reports any loss, storing
  `focusLostPct` in the run's `meta.json`.
- It refuses to start when another process is on the video-encode engine, unless
  `-AllowEncoderContention` is passed explicitly.
- `Measure-Frames.ps1` independently recomputes throttled-frame share from the
  frame data and refuses to summarise quietly over a contaminated run, so a stale
  CSV cannot smuggle one into a table.
