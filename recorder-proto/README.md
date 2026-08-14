# recorder-proto

Headless capture/encode prototype. **Not** the product, and deliberately not a UI —
per the implementation requirement, this exists only to answer one question before
anything else gets built:

> Can we capture Valorant with extremely low overhead?

Design rationale lives in [`../docs/ADR-001-capture-architecture.md`](../docs/ADR-001-capture-architecture.md).
Read that first; the short version is that Vanguard rules out injection-based game
capture, so this is built on Windows.Graphics.Capture.

## Build

Requires Rust (MSVC toolchain) and MSVC Build Tools with the C++ workload.

```bash
cargo build --release
```

## Commands

### `probe` — what can this machine encode with?

```bash
cargo run --release -- probe
```

Enumerates **hardware** encoder MFTs only. If a machine has no hardware encoder,
this reports that rather than falling back to a CPU encoder — §2 says recording
must not compete with Valorant for CPU, so a silent x264 fallback would be the
wrong answer, not a graceful one.

Expected: Quick Sync on the work laptop, NVENC on the RTX 2060.

### `capture` — what does the capture stage alone cost?

```bash
cargo run --release -- capture 30 60
```

`capture [seconds] [fps]`. Captures Valorant if running, otherwise the foreground
window. Reports frames arrived, frames kept, drops (split by cause), and the mean
and worst-case duration of the capture callback.

Encoding is deliberately *not* in this path yet. Isolating capture from encode
means that when a bad number shows up later, it can be attributed to the right
stage instead of guessed at.

## What to read in the output

**Callback worst-case is the number that matters, not the mean.** Per §20, a
recorder with a good average and occasional spikes is worse competitively than a
slightly slower one that is consistent. The callback runs on a threadpool thread
fed by the compositor; if it ever takes long enough to matter, that is the most
likely mechanism by which this recorder could disturb the game.

**Frames arrived vs. frames kept** shows the pacing working. At a 60 fps target on
a 144 Hz display you should see substantially more arrivals than keeps, and the
difference should land in `dropped by pacing`. Frames are dropped *before* the GPU
copy, so a paced-out frame costs almost nothing.

## Measuring properly (§18, §29)

Numbers from the work laptop are **correctness signals only** — an i5-1334U with
Iris Xe will be thermally throttled and has no NVENC. Do not put them in a report.

Real measurement, on the i5-12400F / RTX 2060 rig:

1. Use **Intel PresentMon** for frame data. It reads ETW and never touches the game
   process, which is both the honest way to measure and the only Vanguard-safe way.
2. Matrix: baseline (no recorder) × this prototype × ShadowPlay × OBS (WGC).
3. Three runs each, identical route — the Range is repeatable in a way live matches
   are not.
4. Report average FPS, 1% low, 0.1% low, **frame-time standard deviation**, CPU %,
   GPU %, encoder %, RAM.

A 12400F/2060 at 1080p low is a CPU-bound configuration, which is the harder case
and the one where recorder CPU contention shows up most clearly. That makes it a
good rig to prove the thesis on, not a limitation.

No performance claim ships before this table exists.
