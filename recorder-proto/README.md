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

### `replay` — the last N seconds, on demand

```bash
cargo run --release -- replay 30 60 clip.mp4
```

`replay [window] [fps] [output]`. Encodes continuously into a memory ring of
*compressed* samples and, at the end of the run, muxes the last `window` seconds
into a file. The run lasts `window + 15` seconds on purpose, so the ring wraps and
eviction is actually exercised.

Compressed, not raw, because the arithmetic is not close: ten seconds of 1080p60
BGRA is ~5 GB of VRAM on a 6 GB card that is also running the game; the same ten
seconds encoded is ~15 MB of RAM. Evicted frames donate their buffers to a pool,
so after the ring first fills, steady state allocates nothing — the printed
`allocated` counter must stop growing after warmup, and the output shows it.

The save is container work only — the frames are already encoded. Measured on the
2060 rig: **~50 ms** for an 11-second clip. That number is the product promise:
the moment a replay protects has already happened, so saving must feel instant.

A clip must start on a keyframe. The ring retains two GOPs beyond the window and
the save picks the latest keyframe at or before the window start, so a saved clip
is slightly *longer* than requested, never truncated or broken at the front.

If the output ever reports `non-monotonic timestamps`, the encoder emitted
B-frames (reordered timestamps) and the clip's timing is suspect. NVENC's MFT
defaults to none; the explicit B=0 request via ICodecAPI is rejected on this
driver, so the counter is the check that the default holds.

### `audio` — desktop audio capture on its own

```bash
cargo run --release -- audio 10
```

Captures WASAPI loopback for N seconds and reports packets, timestamp span, and
levels. **Peak level is the number that matters**: packet counts prove the
plumbing runs, but only a non-zero peak proves we are capturing what the speakers
are playing rather than a well-formed stream of silence. Play something first —
loopback records the default output device, so a quiet machine yields a correct,
empty result.

`record` and `replay` capture audio by default; `--no-audio` turns it off, which
is what the benchmark path uses, since ADR §9's overhead figures were measured
video-only and only stay comparable if new runs match.

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

### `--hwnd <handle>` — target one specific window

```bash
cargo run --release -- capture 20 60 --hwnd 0x690C0E
```

Accepts hex (`0x…`) or decimal. A **test affordance, not a product feature**: it
exists because Windows refuses `SetForegroundWindow` to a background process, so a
test script cannot put its chosen window in front for `--foreground` to pick up.
Without it, the resize path below cannot be exercised from a script at all.

## Behaviour when the target changes shape

Players alt-tab, minimise, and change resolution mid-match, so this is ordinary
operation rather than an error case.

- **Starting** against a target smaller than 64 px on either axis fails immediately
  with a message naming the size. A minimised window reports an iconic placeholder
  (160×28 for Valorant), which would otherwise produce a recorder that captures
  nothing at all, or an encoder configuration Media Foundation rejects with
  `MF_E_INVALIDMEDIATYPE`.
- **Resizing mid-session** is detected, the frame pool is rebuilt once, and frames
  that no longer match are dropped and reported under `dropped, target resized`.
  They are dropped rather than scaled because the encoder cannot change resolution
  mid-stream. Capture resumes on its own when the window returns to its original
  size — no restart needed.

`dropped, target resized` being non-zero is therefore information, not a fault: it
says the window changed shape, which is a different statement from the recorder
falling behind (`dropped, no free ring slot`).

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
