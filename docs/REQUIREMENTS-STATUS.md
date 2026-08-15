# Requirements status

Every `§N` in `ADR-001-capture-architecture.md` refers to a section of the
performance-first recording requirement. This maps each one to what actually
exists, with the evidence.

Status as of **2026-08-15**, all measurements from the i5-12400F / RTX 2060 rig.

Legend: **Done** · **Partial** · **Not started** · **N/A**

---

## The implementation requirement (do this before building a UI)

> 1. Research the capture APIs · 2. Compare overhead · 3. Determine architecture ·
> 4. Determine hardware encoding · 5. Minimal native prototype · 6. Benchmark it ·
> 7. Only then integrate

**Done, in that order.** ADR-001 §1 evaluates hook-based capture, Desktop
Duplication and WGC before any code; `recorder-proto` was headless and had no UI
until the numbers existed; the app came last. The founding question — *can we
capture Valorant with extremely low overhead?* — is answered with measurements,
not assertions.

---

## Performance requirements

| § | Requirement | Status | Evidence |
|---|---|---|---|
| 1 | Performance targets; don't market unproven numbers | **Done** | Every claim carries its measurement. The ADR still refuses to say "0% FPS loss" even now that the capped result rounds to it. |
| 2 | Recording must not compete for CPU | **Done** | 0.45–0.54% of 12 threads (~6% of one core), 86–87 MB. GPU encode, no readback, no polling, no per-frame JS, event-light detection. |
| 3 | Hardware encoding first, auto-detect | **Done** | `probe` enumerates hardware MFTs only and *refuses* to recommend a CPU fallback. NVENC H.264 + HEVC found on this rig. |
| 4 | Don't reinvent an encoder | **Done** | Media Foundation throughout. No codec written. |
| 5 | Desktop architecture, not browser capture | **Done** | WGC → D3D11 → NVENC → file/ring. No frame ever touches JavaScript. |
| 6 | Native performance-critical layer | **Done** | Rust owns capture, encode, replay, hotkeys, files. UI is plain HTML/JS and holds nothing capture needs. |
| 7 | Windows-first, benchmark alternatives | **Done** | WGC chosen over Desktop Duplication and hook capture with reasons recorded (ADR §1). |
| 8 | Avoid unnecessary frame copies | **Done** | One GPU→GPU `CopyResource`, then a DXGI surface handed straight to the encoder. Zero GPU→CPU transfers. |
| 9 | Async pipeline, never block the game | **Done** | Free-threaded capture callback → bounded ring → encoder thread. Callback p99 **13–52 µs**. |
| 10 | Frame-dropping strategy | **Done** | Every queue drops rather than waits. Drops are counted and reported by cause. |
| 20 | Frame-time consistency over average FPS | **Done** | The benchmark's headline figure. Capped result: **+0.032 ms** on a 0.588 ms baseline. |
| 21 | Low latency, no render-thread sync | **Done** | Nothing synchronises with the game's render thread; out-of-process by construction. |
| 28 | Priority order | **Done** | The startup-ordering and rebuild-storm fixes were both taken *against* convenience to protect priorities 1–3. |

## Recording features

| § | Requirement | Status | Notes |
|---|---|---|---|
| 11 | Competitive / Quality / Custom modes | **Not started** | Settings exist (resolution follows the window, fps, bitrate) but there are no named presets. Competitive Mode's *behaviour* is the default — no thumbnails, no AI, no analytics during capture. |
| 12 | Never process VOD during gameplay | **Done** | Nothing transcodes, indexes, or analyses. Compliance by absence, and worth keeping deliberate. |
| 13 | Cloud upload after gameplay | **Not started** | No upload path exists. |
| 14 | Disk performance, drive selection | **Partial** | Writes are buffered and sequential through the MF sink writer, and output is configurable. No drive picker, free-space, or write-speed display. |
| 15 | Storage estimation | **Not started** | Bitrate is known, so this is arithmetic and UI, not research. |
| 16 | Lightweight game detection | **Done** | Window-class lookup every 2 s, **only while idle**. Never enumerates the process table, never reads the game's memory. |
| 22 | H.264 / HEVC / AV1 | **Partial** | H.264 used throughout, per the requirement's own compatibility-first ordering. HEVC is detected but not selectable. AV1 is impossible on Turing — measured, not assumed. |
| 23 | Audio: game, mic, separate tracks | **Partial** | Desktop loopback (game + comms as heard) done, verified exact. **Microphone not captured** — the player's own voice is missing. Separate tracks not implemented. |
| 25 | Feel invisible | **Done** | Tray app, close-to-tray, auto-start on game detection, hotkey save. |
| 26 | Don't overload the desktop client | **Done** | No charts, no AI, no database, no animation. Plain HTML with a 2 Hz status poll. |
| 27 | Web/desktop split | **Partial** | Desktop side matches the split exactly. The web strand (`server.js`, `planner.html`) exists but has not been assessed this cycle. |

## Measurement and diagnostics

| § | Requirement | Status | Notes |
|---|---|---|---|
| 17 | In-app performance monitor | **Partial** | Recorder CPU %, RAM, VRAM (used/budget), disk write rate, recording FPS, capture callback p99, drops by cause, ring occupancy — all read from the OS, never estimated, and rendered as "—" until a real delta exists. **Missing: game FPS and frame time**, which need an ETW consumer; `bench/` measures them properly out-of-process instead. |
| 18 | Benchmark mode | **Partial** | `bench/` implements the methodology rigorously — but as PowerShell + PresentMon, not in-app, and **without the ShadowPlay and OBS columns** the requirement names. |
| 19 | Internal profiler | **Partial** | Capture-callback histogram (p50/p99/p99.9), encode submit cost, queue drops by cause, ring allocation counters. Missing disk-queue depth and per-stage latency tracing. |
| 29 | Acceptance test matrix | **Partial** | Done: 1080p, 60 fps recording, CPU-bound, capped and uncapped, three-plus runs, real report. **Not done: 1440p, 120 fps recording, an explicit GPU-bound configuration, and the ShadowPlay comparison.** |
| 30 | Positioning | **N/A** | Product framing, not an engineering task. |

---

## The honest summary

**The foundation the requirement demanded is built and measured.** Capture,
encode, replay and the app all exist, and the differentiator — recording that
does not cost the player performance — is measured rather than asserted:
frame-time consistency moves **0.032 ms**, the recorder costs **~0.5% of one
CPU's twelve threads** and **87 MB**, and at a 240 fps cap no FPS delta is
distinguishable from the control's own run-to-run spread.

**Three gaps are worth naming plainly:**

1. **The §29 matrix is a third complete.** 1080p60 CPU-bound is done thoroughly.
   1440p, 120 fps recording and a GPU-bound configuration are not.

   The **ShadowPlay performance column is deliberately skipped** (owner's
   decision, 2026-08-15). The consequence is narrow but real, and it is §29's
   own rule rather than an opinion: *"Do not claim 'better than ShadowPlay'
   until measured."* No comparative claim can ship — not in the app, the README,
   or any marketing copy.

   What can be said truthfully, because it is measured: **at a 240 fps cap,
   recording costs no measurable FPS** — every delta inside the control's own
   run-to-run spread — at ~0.5% of twelve CPU threads and 87 MB. That is a
   stronger claim than most recorders can evidence, and it needs no competitor.

   Their *output* was compared and matched (§9c): same codec, profile and level,
   comparable bitrate. Quality parity is evidenced; performance parity is not.
2. **§17's performance monitor is mostly absent.** The app reports the
   recorder's own health but not the player's — no game FPS, frame time, CPU,
   GPU, or encoder utilisation. This is the requirement's own example UI, and
   it is the screen that would make the product's claim visible to the user
   while they play.
3. **The audio requirement is half met.** Desktop loopback captures what the
   player hears; nothing captures what they *say*. For a coaching product where
   a team reviews comms, that is the more important half.

**One caveat carried forward:** the capped no-measurable-cost result was
measured **video-only**. Desktop audio adds an encode it did not include, so
that result does not describe the current build until re-run.
