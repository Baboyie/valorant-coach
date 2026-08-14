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

Expected: Quick Sync on the work laptop, NVENC on the RTX 2060. **Confirmed on both**
(2026-08-15). The 2060 reports NVENC H.264 and HEVC and no AV1 at all, which is
correct for Turing — see ADR §8.

### `capture` — what does the capture stage alone cost?

```bash
cargo run --release -- capture 30 60
```

`capture [seconds] [fps]`. Captures Valorant if running, otherwise the foreground
window. Reports frames arrived, frames kept, drops (split by cause), and the mean
and worst-case duration of the capture callback.

Encoding is deliberately *not* in this path. Isolating capture from encode means
that when a bad number shows up later, it can be attributed to the right stage
instead of guessed at.

### `record` — capture plus hardware encode, to a real file

```bash
cargo run --release -- record 30 60 out.mp4
```

`record [seconds] [fps] [output]`. Adds the encoder stage: DXGI-surface samples
into a Media Foundation sink writer, H.264, no readback. Reports everything
`capture` does plus encode-submit cost and how many frames the encoder accepted.

Check the reported frame rate of the resulting file against "frames kept". They
should agree — the muxer writes real timestamps, so a file that claims the
*configured* fps rather than the achieved one would mean the VFR path has
regressed (ADR §7).

### `--foreground` — target any window instead of the game

```bash
cargo run --release -- record 20 60 out.mp4 --foreground
```

Works with `capture` and `record`. Forces the foreground window as the target even
when Valorant is running.

This exists because a **minimised** Valorant is detected correctly and then yields
zero frames — WGC composites nothing for an iconic window. That makes a backgrounded
game useless as a smoke test for the encoder, and this flag lets the encode path be
exercised without needing anyone sitting at the game.

## What to read in the output

**Callback worst-case is the number that matters, not the mean.** Per §20, a
recorder with a good average and occasional spikes is worse competitively than a
slightly slower one that is consistent. The callback runs on a threadpool thread
fed by the compositor; if it ever takes long enough to matter, that is the most
likely mechanism by which this recorder could disturb the game.

**Frames arrived vs. frames kept** shows the pacing working. When the source is
redrawing faster than the target — a 60 fps target against a game running at the
panel's rate — you should see substantially more arrivals than keeps, with the
difference landing in `dropped by pacing`. Frames are dropped *before* the GPU copy,
so a paced-out frame costs almost nothing.

Read that ratio against the **actual** refresh rate, not an assumed one: the
benchmark rig is 1080p **240 Hz**. And do not read a low arrival rate as a fault
before checking what was on screen — WGC is change-driven, so a mostly-static
desktop legitimately delivers ~25/s against a 60 fps target with zero pacing drops.

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
