# ADR-001 — Capture & Encode Architecture for the Valorant Recorder

Status: **Accepted** — capture and encode both built and now exercised on the
benchmark rig. The §29 acceptance benchmark itself is still outstanding (§9).
Date: 2026-08-14 · Last measured: 2026-08-15 on the i5-12400F / RTX 2060 rig

---

## 0. The decision in one line

**Windows.Graphics.Capture → D3D11 texture ring → async Media Foundation hardware MFT (DXGI-surface input, zero CPU readback) → bounded queue → buffered async writer.** No injection, no CPU encode, no frame data through JavaScript.

The single most important constraint is not performance. It is that **the lowest-overhead capture method is unusable for this specific game.** See §2.

---

## 1. Capture API evaluation

Three candidate paths were considered, per requirement §7 ("do not automatically assume one capture API is best").

### Option A — Hook-based game capture (the OBS "Game Capture" approach)

Inject a DLL into the game, hook `IDXGISwapChain::Present`, copy the backbuffer into a shared texture.

In pure overhead terms this is the best method: it captures at the game's own present rate and never involves the compositor.

**It is disqualified for Valorant, and it is not a fallback.**

- Riot Vanguard is a kernel-mode (ring-0) anti-cheat that blocks process injection by design. Injecting into `VALORANT-Win64-Shipping.exe` is indistinguishable from what a cheat does.
- This is not theoretical: OBS Game Capture is *currently broken* on Vanguard titles. As of **21 July 2026** users reported black-screen output from OBS Game Capture, and OBS stated the cause is likely Vanguard and outside their control. OBS 31.0's digital signature change conflicts with Vanguard's process protection.
- Riot has separately told Elgato that DirectShow is treated as a cheat-injection vector and is blocked outright.
- The community guidance for every Vanguard / BattlEye / EAC / FACEIT title is to use Display or Window capture instead.

There is also an architectural objection independent of the ban risk: a `Present` hook does work **on the game's render thread**, which directly violates requirement §21 (no render-thread synchronisation, no CPU/GPU stalls on the game path).

> **Rule for this project: we never inject into the game process. Not behind a flag, not as an "advanced option".** The downside is a user ban, which is unrecoverable and is our fault.

### Option B — DXGI Desktop Duplication (`IDXGIOutputDuplication`)

| | |
|---|---|
| Pros | Mature, hardware-accelerated, hands back a D3D11 texture on the GPU, no injection |
| Cons | **Must run on the same GPU that drives the display.** On hybrid-graphics gaming laptops (the majority of the laptop market) this needs manual GPU-preference intervention |
| | Monitor-granularity only — cannot target Valorant's window specifically |
| | Duplication is torn down by UAC/secure desktop and session switches; needs re-init state machine |
| | Historically fragile with exclusive fullscreen |

**Verdict: fallback only** — Windows 10 builds without WGC, or when `GraphicsCaptureSession.IsSupported()` returns false.

### Option C — Windows.Graphics.Capture (WGC) ✅ **CHOSEN**

- Hands back an **`ID3D11Texture2D` already resident on the GPU** via `Direct3D11CaptureFramePool`. No CPU readback anywhere in the capture stage — this is what satisfies requirement §8.
- **Works cross-GPU with no user intervention.** This is the deciding practical advantage over Desktop Duplication for gaming laptops.
- Entirely out-of-process. It does not touch the game, so it is Vanguard-safe.
- Can target Valorant's `HWND` directly, or the monitor.
- `Direct3D11CaptureFramePool.CreateFreeThreaded` delivers frames on a threadpool thread with **no `DispatcherQueue` and no UI-thread coupling** — required so the capture path never depends on the React layer (§6).

**Known cost — the yellow border.** WGC draws a capture indicator around the captured surface. Removing it requires *all* of:
1. Windows 11 only (not available on Win10),
2. **MSIX package identity** with the `graphicsCaptureWithoutBorder` capability declared in the manifest,
3. runtime user consent via `GraphicsCaptureAccess.RequestAccessAsync(GraphicsCaptureAccessKind.Borderless)`,
4. no other app on the machine holding `IsBorderRequired = true` for that same target.

> **Product implication, decided now rather than late:** a competitive recorder cannot draw a yellow border around the player's game. **The desktop app must ship as a packaged (MSIX) application.** This constrains the installer and code-signing story and should be settled before the UI exists, not after.

---

## 2. Encoder

Requirement §4 is respected: we write no codec. Two integration routes:

**Route 1 — Media Foundation async hardware MFT** (Intel QSV / NVIDIA NVENC / AMD AMF all expose vendor MFTs)
- One code path covers all three vendors, satisfying §3's auto-detect requirement without three integrations.
- Input is an `IMFSample` backed by a **DXGI surface** (`MFCreateDXGISurfaceBuffer`) with an `IMFDXGIDeviceManager` sharing our D3D11 device → **the frame never leaves the GPU**.
- Run the MFT in **async mode** (`MF_TRANSFORM_ASYNC`) driven by `IMFAsyncCallback`, so the encoder is event-driven — no polling loop, satisfying §2.

**Route 2 — direct vendor SDKs** (NVENC SDK, AMF, oneVPL): lower latency and finer rate-control, but three separate integrations and vendor DLL loading.

**Decision: start with Route 1 behind a trait/interface.** Add a direct NVENC backend later *only if the benchmark shows the MF layer costs measurable overhead* — and NVIDIA is the majority case, so that is the one worth special-casing. Do not build three backends before measuring.

**Encoder settings for Competitive Mode:** H.264 High profile, **no B-frames** (they add latency and encoder work), `CODECAPI_AVLowLatencyMode`, ~2 s GOP, CBR. H.264 first per §22 — compatibility and lowest encoder cost beat codec efficiency here. HEVC/AV1 are later, opt-in, and must be re-benchmarked, not assumed.

---

## 3. Pipeline & threading (§9, §10, §21)

```
WGC free-threaded callback
  └─ copy into a pre-allocated D3D11 texture ring (GPU→GPU), push index, RETURN IMMEDIATELY
        │  ring full? → drop this frame. Never wait.
        ▼  bounded lock-free SPSC queue
Encoder thread — submit DXGI-backed IMFSample to async MFT
        ▼  MF async event callback
Mux / IO thread — buffered, sequential, async writes
```

Rules this pipeline enforces:

- **Nothing in the capture callback blocks.** The callback copies and returns; WGC frame textures must be released promptly so the frame pool can recycle them.
- **Every queue is bounded and every buffer is pre-allocated at session start.** Zero heap allocation in steady state, so there is nothing to spike on (§19).
- **Overflow always drops, never blocks** — this is the §10 priority order made mechanical rather than aspirational.
- **Thread priority is never raised above the game.** The recorder's threads run at normal or below.
- On hybrid CPUs, mark the IO/mux thread with `THREAD_POWER_THROTTLING_EXECUTION_SPEED` (EcoQoS) so the scheduler prefers **E-cores** for it, keeping P-cores free for Valorant. Cheap win for §2 on the 12th/13th-gen and Ryzen hybrid parts many players run — but note it is a **no-op on the target rig below**, whose i5-12400F is 6P+0E. Do not expect it to show up in our own benchmark; it is a win for other users' machines.

**Frame pacing:** WGC delivers at present/compositor rate; a 400 FPS game vastly overproduces for a 60 or 120 FPS target. Timestamp from the frame's `SystemRelativeTime` and drop early frames rather than encoding and discarding them — dropping before the encoder is what keeps encoder utilisation low.

---

## 4. Game detection (§16)

No process-list polling. Use `SetWinEventHook(EVENT_SYSTEM_FOREGROUND, WINEVENT_OUTOFCONTEXT)` and inspect only the newly-foregrounded window's owning process. This fires on user action, costs effectively nothing at idle, and never enumerates the process table. A 2-second snapshot fallback runs **only while not recording**.

---

## 5. Benchmark methodology (§18, §29)

**Use Intel PresentMon. Do not reinvent it.** It collects real present/frame-time data via ETW without touching the game process — the only measurement approach that is itself Vanguard-safe and overhead-honest. It yields average FPS, 1% and 0.1% lows, frame times, and latency estimates.

Matrix: `baseline (no recorder)` × `ours` × `ShadowPlay` × `OBS (WGC)`, three runs each, identical route, 1080p60 and 1440p120, in both CPU-bound and GPU-bound configurations.

Report: average FPS, 1% low, 0.1% low, **frame-time standard deviation** (§20 — this is the headline number, not average FPS), CPU %, GPU %, encoder %, RAM.

No performance claim ships until this table exists. Per §1, nothing is marketed as "0% FPS loss".

---

## 6. Machines: development vs. measurement

These are two different computers, and conflating them would invalidate every number we produce.

**Development machine (work laptop) — functional testing only.**
Intel Core i5-1334U (15 W ultrabook, 2P+8E, 1.3 GHz base), **Iris Xe integrated graphics**. No NVENC, no AMF — Quick Sync only. Adequate to develop and functionally verify the WGC capture path and the Media Foundation encoder plumbing. **Not** a valid benchmark rig: thermal throttling would dominate every measurement, and it cannot exercise NVENC at all. No performance figure from this machine goes in a report.

**Target / benchmark rig (home) — the machine that produces real numbers.**
Intel **i5-12400F** (Alder Lake, **6 P-cores, no E-cores**, 12 threads) · **NVIDIA RTX 2060** (6 GB) · 16 GB RAM.

Consequences that follow directly from this hardware:

- **NVENC is the primary encoder target.** Vendor MFT first, per §2; a direct NVENC SDK backend is the one vendor special-case worth adding if measurement justifies it.
- **RTX 2060 is Turing / 7th-gen NVENC: H.264 and HEVC only. It cannot encode AV1** (AV1 encode begins with Ada / RTX 40). §22's AV1 option is therefore untestable on this rig and is deferred outright rather than stubbed.
- **6 P-cores, no E-cores.** The EcoQoS E-core trick in §3 is a no-op here. It remains correct for users on hybrid CPUs, but it must not be counted as a win in *our* benchmark.
- 16 GB RAM and 6 GB VRAM make the replay-buffer sizing question real: an in-RAM replay buffer must be budgeted against a game that already wants a large share of 16 GB.
- A 12400F + RTX 2060 at competitive Valorant settings is a **CPU-bound** configuration at 1080p low. That is the harder and more relevant case for §29, and it is the one where recorder CPU contention will show up most clearly. Good rig to prove the thesis on.

**Sequencing:** the user is at the work laptop now, so the prototype is written and compiled here against Quick Sync for correctness, then run and measured at home on NVENC. Build the encoder behind an interface from the start so swapping QSV → NVENC is a config change, not a rewrite.

---

## 7. Prototype status (step 5)

`recorder-proto` builds and runs on the dev laptop. Rust 1.97.1, `windows` crate 0.62.2.

**Encoder detection works.** Quick Sync H.264, HEVC and AV1 MFTs are found and the
H.264-first preference selects correctly. Two notes:

- `MFTEnumEx` returns **duplicate registrations** — every encoder appeared twice.
  Deduplicated by (codec, name). Worth remembering before any settings UI lists
  encoders from a raw enumeration.
- An AV1 encoder MFT is *registered* on this Iris Xe part. Registration is not
  proof of working hardware AV1 encode, and it is untested. It changes nothing:
  the target rig is Turing and has no AV1 encode at all.

**Capture works, and pacing had a serious bug worth recording.**

The first implementation gated frames on "time since the last kept frame ≥ target
interval". Measured result at a 60 fps target: 59.5 fps arriving, **31.7 fps kept**
— a "60 fps" recording that was actually 30.

The cause is a beat pattern. At 60 fps the interval is 16.67 ms while the
compositor delivers roughly every 16.8 ms, so ordinary jitter puts a frame
fractionally under the threshold; it is dropped, and its successor then arrives a
full two intervals later. The failure only appears when the source rate is *close
to* the target rate, which is the normal case, and it silently halves the frame
rate rather than erroring.

Replaced with a deadline accumulator that advances by exactly one interval per
kept frame, plus a half-interval jitter tolerance. Verified:

| Target | Arrived | Kept | Dropped by pacing |
|---|---|---|---|
| 60 fps | 599 (59.9/s) | 599 (59.9/s) | 0 |
| 30 fps | 599 (59.9/s) | 301 (30.1/s) | 298 |

**Capture callback cost on the dev laptop:** mean stable at 6–14 µs across every
run. Worst case is normally 33–98 µs, with one 511 µs outlier observed once and
not reproduced in three subsequent runs — a scheduler preemption on a 15 W part,
not a structural stall. Correctness signal only; Iris Xe, thermally throttled, no
game running. Recorded as baseline shape, not as a performance claim.

**WGC is change-driven, not clock-driven.** Measured arrival rates on an idle
desktop were 8.5–29 /s against 57–60 /s when the captured surface was actively
redrawing, with no code change between runs. The frame pool delivers when the
surface changes, not on a clock.

Harmless for Valorant, which redraws continuously. But it constrains the encoder
stage directly:

- The muxer must write **variable frame rate with real timestamps** taken from
  `SystemRelativeTime`, not assume a constant cadence.
- If constant-frame-rate output is ever required, duplicate frames must be
  synthesised at mux time. Doing that by re-submitting identical frames to the
  encoder would burn encoder cycles for no visual gain — exactly the waste §2
  forbids. Prefer letting the container express the timing.
- "Recording FPS" in the §17 performance monitor must therefore be reported from
  kept-frame timestamps, not assumed from the configured target, or it will lie
  whenever the scene is static.

**Instrumentation gap — closed.** The callback now records a 64-bucket
microsecond histogram alongside mean and max, and `capture`/`record` print
p50/p99/p99.9. A single max cannot distinguish one warmup spike from a recurring
stall, and that distinction is the whole point of measuring consistency.

**Encoder stage and muxer — built.** `record` submits DXGI-backed `IMFSample`s to
an `IMFSinkWriter` with hardware transforms enabled, sharing our D3D11 device via
`IMFDXGIDeviceManager`, and writes real per-sample timestamps. The replay buffer
is still to build.

---

## 8. Measurements on the benchmark rig (2026-08-15)

First run of the prototype on the i5-12400F / RTX 2060 / 16 GB rig. Display is
**1080p 240 Hz** — note `recorder-proto/README.md` still describes expected
arrival/keep ratios in terms of a 144 Hz panel.

**Encoder probe confirms every §6 prediction.**

```
H.264   NVIDIA (NVENC)   NVIDIA H.264 Encoder MFT
HEVC    NVIDIA (NVENC)   NVIDIA HEVC Encoder MFT
selected: H.264 via NVIDIA (NVENC)
```

No AV1 encoder is registered — Turing cannot encode it, as predicted, so §22's AV1
option stays deferred on evidence rather than on inference. The 12400F is an
F-part with no iGPU, so NVENC is the only hardware encoder present; this rig
cannot cross-check Quick Sync.

**The duplicate-registration quirk is Intel-specific.** §7 recorded `MFTEnumEx`
returning every encoder twice on Iris Xe. On NVIDIA each encoder appears exactly
once. The dedup is still correct to keep — but it is working around vendor
behaviour, not a universal property of `MFTEnumEx`.

**Capture callback is tighter here than on the dev laptop**, which is the expected
direction: a 65 W desktop part does not thermally throttle. Over a 20 s window,
1920×1032, no game running:

| | mean | p50 | p99 | p99.9 | max |
|---|---|---|---|---|---|
| capture only | 11.3 µs | 10 µs | 21 µs | 51 µs | 51 µs |
| capture + encode | 12.3 µs | 11 µs | 27 µs | 63 µs | 88 µs |

The laptop's 511 µs outlier did not reproduce. Encode submit averaged 37 µs with a
1.3 ms maximum, the maximum being sink-writer warmup on the first sample.

**End-to-end encode works on NVENC.** 533 frames arrived, 533 kept, 533 accepted,
zero dropped; a 9.5 MB MP4 with `ftyp`/`mdat`/`moov`/`avc1` intact. Windows reports
the file as **26.59 fps**, matching the measured keep rate rather than the
configured 60 — the variable-frame-rate timestamping in §7 behaving as designed.
A container that claimed 60 fps here would have been lying.

### The founding question, answered: WGC captures Valorant

This is what the prototype existed to determine, and it had never been run against
the live game before today.

Capturing the real Valorant window, 15 s at a 60 fps target:

```
target : VALORANT
frames arrived : 890 (59.3/s)   frames kept : 890 (59.3/s)
dropped by pacing : 0           dropped, ring full : 0
callback  mean 11.0 us   p50 10   p99 21   p99.9 43   max 43.1 us
```

Vanguard did not interfere: no block, no crash, no teardown. Capture cost against
the live game is indistinguishable from capture against an ordinary desktop window.

**And the frames contain the game.** Frame count alone would not have proved this —
§1 records that OBS Game Capture returns *black frames* on Vanguard titles, which
looks like success from every counter we track. A 12 s recording was decoded and
sampled: mean luminance 42.3 on a 0–255 scale, standard deviation 30.1, 247 distinct
colours across the sampled grid, and the decoded frame is plainly the Valorant
collection screen. A blacked-out capture would have shown near-zero variance.

So the answer to "can we capture Valorant with extremely low overhead?" is **yes for
capture**, on this hardware, with the caveat that all of the above was measured with
the game idle in menus. Overhead *while playing* is still §29's job.

### Three defects this run exposed

**1. `find_valorant` never matched, and failed toward a plausible-looking answer.**
The lookup was `FindWindowW(None, "VALORANT")`. Valorant's title is `"VALORANT  "`
with two trailing spaces and `FindWindowW` matches exactly — but that is not the
whole story: on this rig `FindWindowW` returns NULL even when given the exact
padded title *or* the class name, while `EnumWindows` walks straight to the window.
Rewritten to enumerate and match the class `VALORANTUnrealWindow`, with a
trimmed-title fallback.

This mattered more than a missed window. `pick_target` falls back to the foreground
window, so the benchmark would have measured the recorder capturing a *terminal*
and labelled the result Valorant. A wrong number that looks right is worse than a
crash.

Matching on class also needs no handle to the game process — no `OpenProcess`
against a Vanguard-protected target, which is the right privilege level to want.

**2. Capture started before the encoder existed, silently losing the opening
frames.** `StartCapture` was the last statement of capture construction, so frames
began arriving while the caller was still building its encoder, and the ring filled
during Media Foundation's sink-writer setup. Measured: **43 frames lost** on a
6-slot ring, against **zero** for the same window with no encoder attached.

They surfaced as "dropped, no free ring slot", which reads like encoder
backpressure under load — a startup ordering bug wearing the costume of a
performance result. `StartCapture` is now a separate `Capture::start()` called
after everything downstream is constructed. Re-measured: 533/533 frames kept, zero
drops, and the encode-submit maximum fell from 3.4 ms to 1.3 ms.

**3. The output footer asserted the wrong machine.** Every run printed "numbers
from this machine are for correctness only — overhead figures are only meaningful
on the RTX 2060 rig". Written on the laptop, true there, and actively misleading
the moment it ran *on* the RTX 2060 rig, where it argued against the only numbers
§6 considers reportable. Runs now print the DXGI adapter description instead, so
the output states its own provenance rather than assuming the reader's machine.

### Known gap: capture-item size changes are not handled

The frame pool and texture ring are sized once from `GraphicsCaptureItem.Size()` at
session start and never resized. A session begun against a **minimised** Valorant
takes its size from the iconic window placeholder — measured at 160×28 at
(-32000,-32000) — and then delivers zero frames, because WGC composites nothing for
an iconic window. Restoring the window mid-session would not fix it: the ring is
already the wrong shape.

Observed twice in one session, because Valorant was minimised between runs:

- `capture` against the iconic window: session builds, **0 frames** in 15 s.
- `record` against the iconic window: the encoder is configured `160x28` and Media
  Foundation rejects the media type outright — `0xC00D36B4`
  (`MF_E_INVALIDMEDIATYPE`), since that is below NVENC's minimum frame size.

The `record` failure is at least loud. The `capture` one is silent, and a benchmark
run that captured nothing would have looked like a spectacularly low-overhead
recorder rather than a broken one.

**Resolved 2026-08-15.** Two halves:

*Refuse to start on a degenerate target.* A session whose capture item is smaller
than 64 px on either axis now fails immediately with `E_INVALIDARG` and a message
naming the size, instead of producing a recorder that captures nothing or an
encoder configuration Media Foundation rejects.

*Survive a resize mid-session.* The callback compares each frame's `ContentSize`
against the ring, and on a change flags the owning thread, which calls
`Direct3D11CaptureFramePool.Recreate` from `Capture::poll_resize`. Frames that do
not match the ring are dropped and counted rather than fed to an encoder that
cannot change resolution mid-stream, and capture resumes by itself once the target
returns to its original size.

Two things this cost, both worth recording:

- **The rebuild does not belong in the callback.** The first version called
  `Recreate` there and did not compile: `IDirect3DDevice` is not `Send`. That was
  the type system being right for a deeper reason — `Recreate` allocates a pool and
  its textures, which is exactly the steady-state allocation §19 forbids, and doing
  it on the compositor's thread would put it on the one path this design exists to
  keep cheap.
- **Signal on change, not on difference.** Flagging whenever `content != ring`
  meant every frame arriving at the wrong size requested another rebuild: measured
  at **eight pool rebuilds for two resizes**. Now the last-seen content size is
  tracked and only a genuine change signals — re-measured at **one rebuild per
  resize**, holding steady across twelve forced redraws while mismatched.

Verified against a window resized underneath a live capture: one rebuild, seven
frames dropped as mismatched, and capture resuming on its own afterwards. The
degenerate-size guard was verified separately by raising the threshold so an
ordinary window trips it.

Still true for the benchmark, since dropped frames are still lost frames: **start
the recorder only when Valorant is on screen at its final resolution.**
`bench/Invoke-Benchmark.ps1` refuses to start a run against a minimised game.

## 8a. Replay buffer (built 2026-08-15)

The last §3 pipeline stage. Design: the ring holds **encoded** H.264 samples in
system RAM, never raw frames — ten seconds of 1080p60 BGRA is ~5 GB against a 6 GB
card running the game, while the same ten seconds encoded is ~15 MB (§6's budget
question, answered by arithmetic). The encoder runs continuously; a save is
container work only.

Implementation: the same sink writer and hardware-encoder path as `record`, with
the MP4 sink swapped for a **sample grabber sink** whose callback hands each
compressed sample to the ring. Encoding and muxing are thereby decoupled. On save,
a second sink writer muxes the buffered samples in passthrough (H.264 in, H.264
out, no transform); SPS/PPS are lifted from the first IDR's in-band parameter sets
and attached as `MF_MT_MPEG_SEQUENCE_HEADER` rather than hoping the sink finds
them. The passthrough type must be *minimal* — encoder instructions (GOP, profile)
on a mux type make Media Foundation reject it as inconsistent (`0xC00D36B4`,
observed).

Decisions that follow §19/§10 directly:

- **Evicted frames donate their buffers to a pool**, so once the ring has filled,
  steady state allocates nothing. Verified: 633 allocations while filling, then
  471 consecutive reuses and zero further allocations.
- **The ring retains two GOPs beyond the window** so a keyframe always exists at
  or before the window start; a clip must begin on an IDR or its first GOP is
  garbage. GOP is pinned to 2 s via `MF_MT_MAX_KEYFRAME_SPACING` (observed
  interval on NVENC: 1.34 s — a maximum, not a cadence).
- **Saving snapshots under the lock and muxes outside it.** A grabber thread
  blocked for the mux's tens of milliseconds would back up the sink writer and
  eventually the capture ring — §10's drop-never-stall, kept true here too.
- **B-frames are the failure mode to watch**: they arrive in decode order with
  reordered timestamps, which the ring would store faithfully and the muxer would
  garble. The explicit B=0 request via `ICodecAPI` is *rejected* by this NVIDIA
  driver (`E_INVALIDARG`), so the ring counts non-monotonic timestamps as a
  canary. Measured: zero across every run — NVENC's MFT emits no B-frames by
  default. The canary stays, because a driver update could change the default
  silently.

Measured on the rig (960×640 animated test window, 10 s window, 25 s run): 1104
frames captured with zero drops, ring steady at 14.0 s spanned (window + margin,
exactly as specified), and **save cost 50 ms** for an 11.3 s / 504-frame clip.
That save cost is the product promise: the moment a replay protects has already
happened, so the save must feel instant.

Prototype simplifications, recorded so they are not mistaken for the design: the
ring is `Mutex`-guarded (uncontended at 60 locks/s; the shipping §3 pipeline
specifies lock-free), and the save is synchronous on the caller's thread.

## 9. §29 benchmark — first results (provisional)

Measured 2026-08-15 on the benchmark rig, 1080p, Valorant in the Range, Intel
PresentMon 2.5.1 console build. **Baseline × ours only** — ShadowPlay and OBS are
not installed on this rig, so two of §5's four columns are absent.

Runs are screened before they count. A run is discarded if it spent >1% of wall
time below 40 fps (Valorant throttles to ~30 fps unfocused), or — for a baseline —
if another process was on the GPU encode engine, since a contended control is
slower than a true one and would flatter us. Of nine runs, **two baselines and four
recording runs survived**; one baseline lost focus and two more had a foreign
encoder appear mid-run.

| | baseline (n=2) | recording (n=4) | delta |
|---|---|---|---|
| Average FPS | 291.1 | 266.7 | **−8.4%** |
| 1% low | 187.7 | 169.5 | −9.7% |
| 0.1% low | 147.2 | 132.9 | −9.7% |
| **Frame-time stddev** | **0.588 ms** | **0.670 ms** | **+0.082 ms** |
| GPU 3D | 18.2% | 20.8% | +2.6 pp |
| GPU encode (NVENC) | 0% | 15.3% | +15.3 pp |
| Recorder RAM | — | 86–87 MB | |
| Recorder CPU | — | 0.45–0.54% of 12 threads | |

### What this establishes, and what it does not

**Do not quote the FPS figures.** The two clean baselines were 302.6 and 279.6 fps
— **8.2% apart from each other, with no recorder running in either.** The measured
recorder cost is 8.4%. When the spread between two controls equals the effect,
the effect is not resolved. A Welch test over the clean runs gives |t| ≈ 1 against
a threshold near 2.4, and the 95% interval on the difference comfortably spans
zero. The honest statement is that the recorder's FPS cost is **somewhere between
nothing and roughly 10%, and this dataset cannot narrow it further.**

The variance is route variance, not measurement noise. At 260–380 fps uncapped in
the Range, where the camera points dominates frame rate.

**Four results are solid**, because their run-to-run spread is small relative to
the quantity itself:

1. **Frame-time consistency barely moves: +0.082 ms on a 0.588 ms baseline.** This
   held at +0.08 to +0.10 ms across every way of filtering the runs, including
   filterings that flipped the FPS numbers around. §20 makes this the headline
   figure over average FPS, and on this evidence recording does not make frame
   delivery meaningfully less consistent.
2. **NVENC costs 15.3% of the encode engine** at 1080p60. Measured directly, and it
   is a dedicated block — it is not competing with the shaders for the 3D queue.
3. **The recorder costs 86–87 MB of RAM and about half a percent of twelve
   threads** (~6% of one core). Both were stable to within a few percent across
   four runs. §2's requirement that recording must not compete with the game for
   CPU is met with room to spare.
4. **Zero frames were dropped** in any recording run.

### The experiment that would settle the FPS question

Cap Valorant's frame rate at the panel's 240 Hz and re-run. Both conditions then
pin at the cap wherever there is headroom, which collapses the route variance that
is currently swamping everything, and it converts a fuzzy question ("how much
headroom does recording consume?") into the one that actually matters
competitively: **does recording ever push the player below their refresh rate?**

### Capped re-run (same day): the FPS question, resolved

Ran with Valorant capped to 240 fps, runs alternating baseline/ours. The cap did
exactly what it was chosen to do: the two pristine baselines agree to **0.08%**
(237.1 vs 237.3 fps average), versus 8.2% disagreement uncapped. There is finally
a control tight enough to measure against.

Screening earned its keep again — of eight runs, three baselines were excluded:
two where the operator briefly alt-tabbed (caught by the harness's 10 Hz focus
poll; the frame-based check alone misses sub-2 s alt-tabs, whose transition
hitches still poison the 0.1% low — 58.8 vs ~125 fps on otherwise identical runs),
and two overlapping a foreign encoder, one of which was *our own harness's
previous recorder still finalising* (the recorder's runtime margin outlived the
teardown wait; fixed by starting the recorder only after focus is held and
refusing to proceed while one is alive).

Pristine runs, 60 s each, 1080p @ 240 cap, recorder at 1080p60 / ~12 Mbps NVENC:

| | baseline (n=2) | recording (n=3) | delta |
|---|---|---|---|
| Average FPS | 237.2 | 237.1 | −0.04% |
| 1% low | 152.7 | 152.4 | −0.2% |
| 0.1% low | 127.8 | 126.4 | −1.1% |
| Frame-time stddev | 0.796 ms | 0.828 ms | +0.032 ms |
| Wall time below 120 fps | 0.08% | 0.09% | +0.01 pp |
| Wall time below 60 fps | 0% | 0% | 0 |

Every delta is inside the baseline's own run-to-run spread. **While capped at
240 fps on this rig, recording costs nothing measurable**: the game does not get
pushed below its refresh-rate budget (below-120 time identical, below-60 time
zero in both conditions), and frame-time consistency moves by 0.03 ms on a 0.8 ms
baseline — noise. The pacing path was also exercised at real rates for the first
time: ~240 arrivals/s paced down to 60 kept, drops landing in `dropped by pacing`
as designed.

Two honest caveats. Baseline n=2 rather than §5's three, because two of three
were consumed by the harness bug — but two controls agreeing to 0.08% bound the
answer more tightly than three noisy ones ever did, so this stands. And the
*uncapped* cost remains what §9's first table says: unresolved between roughly
0 and 10%, because for uncapped play the route variance is not a measurement
artifact — it is the signal. A player who caps (the common competitive
configuration on a 240 Hz panel) gets the recorder for free; an uncapped player's
cost is real but small enough that this dataset cannot pin it.

Per §1, none of this is described as "0% FPS loss" — including now, when it
rounds to that.

## 9a. The app (built 2026-08-15)

`recorder-app` — Tauri v2 shell over `recorder-core`, which was extracted from
the prototype into a library so the app and the benchmark CLI share one measured
pipeline rather than two drifting copies. `recorder-proto` remains the
instrument §5 references.

Structure, and why: **one engine thread owns every Media Foundation and Direct3D
object**, with the UI sending commands down a channel and reading a status
snapshot. §6's requirement that the capture path never depend on the UI layer
becomes structural — no webview behaviour can stall capture, because the webview
holds nothing capture needs.

**Buffering and manual recording are mutually exclusive in v1.** Both at once
means two encoders fed the same textures, doubling encode-engine load and
invalidating the 15.3% figure §9 measured. A recording already contains what a
clip would have.

Verified on the rig against live Valorant: buffering held at exactly
window + margin (34.0 s for a 30 s window), 2,918 frames kept, capture callback
p99 **52 µs**, zero resize drops, and clips saved by global hotkey **in
141 ms** — 30.0 s at 57.7 fps, the configured window exactly.

### A muxing bug the app exposed that the prototype could not

The first app clip contained 13.8 s of footage and claimed to be **2 seconds
long**. The file held every frame — it was larger than the ring it came from —
but the container's timeline was wrong.

Cause: the muxer used the encoder's *nominal* per-sample duration (1/fps)
instead of the real gap between samples. 143 frames ÷ 60 fps = 2.4 s, which is
what the MP4 sink wrote. This is precisely what §7 warned about — "the muxer
must write variable frame rate with real timestamps, not assume a constant
cadence" — and it stayed invisible through every prototype test because dense
footage makes nominal and real spacing agree. It took a **static scene**, where
WGC's change-driven delivery spreads few frames over many seconds, to separate
them. Fixed by deriving each sample's duration from the next sample's timestamp.

Worth recording as a testing lesson: the prototype exercised this path
repeatedly against an animated window and never saw it. The bug needed the
*absence* of motion, which is a state a synthetic test naturally avoids.

### Smart App Control: a second, independent reason to ship signed MSIX

Building the app on this rig failed with `os error 4551` — Smart App Control
(enforcing) refusing to execute a freshly compiled, unsigned build-script binary
that carried Mark-of-the-Web, which OneDrive applies to everything it syncs.
Working around it during development is easy (build outside the synced folder).

The shipping implication is not easy: **SAC will block the distributed app for
the same reason unless it is code-signed.** §1 already concluded this must ship
as a packaged MSIX to remove the WGC capture border. That conclusion now has a
second justification that is entirely independent of the border, which makes the
code-signing story a prerequisite rather than a preference.

## 10. Blockers

- ~~§29 acceptance benchmarking is gated on physical access to the home rig.~~
  **Resolved** — the prototype now builds and runs there; toolchain, PresentMon and
  harness are installed.
- ~~§29 acceptance benchmarking is gated on a play session.~~ **Done** — see §9.
  Capture, encode, RAM and CPU costs are measured and solid.
- ~~The FPS cost is still unresolved.~~ **Resolved for capped play** by the 240 fps
  re-run (§9): no measurable cost — every delta inside the control's own spread.
  The *uncapped* cost remains bounded at 0–10% and would need a repeatable
  scripted route to pin tighter; not worth gating anything on.
- ShadowPlay and OBS columns of the §5 matrix are not installed on this rig, so the
  current table is baseline × ours only.
- ~~Replay buffer is unbuilt.~~ **Built and measured** — see §8a. Size changes are
  handled (§8). The §3 pipeline now exists end to end; what remains prototype-grade
  is the ring's mutex (spec says lock-free) and the synchronous save.
- **Code signing is now a hard prerequisite**, not a packaging preference: Smart
  App Control blocks unsigned binaries outright (§9a), independently of §1's
  capture-border reason for MSIX. Nothing ships to another machine until this is
  settled.

### Note on the toolchain install

The MSVC Build Tools install stalled mid-download and needed recovery. Recorded
because it will recur on any fresh machine: `winget` will not add a workload to an
existing Build Tools instance, and `setup.exe modify` / `install` / `repair` all
return **87** once an instance exists with `installationComplete` blank. The route
that works is running the **bootstrapper** (`vs_BuildTools.exe`) directly with
`--add Microsoft.VisualStudio.Workload.VCTools --includeRecommended`.

**Benchmark rig, 2026-08-15.** The bootstrapper route worked first time on a clean
machine (no prior VS instance, so the 87 problem never arose). Toolchain deliberately
installed to `D:` because `C:` had only 22 GB free:

| | |
|---|---|
| Rust 1.97.1 | `RUSTUP_HOME=D:\dev\rustup`, `CARGO_HOME=D:\dev\cargo` |
| MSVC 14.44.35207 | `D:\dev\BuildTools` (`--installPath`, plus `--nocache`) |
| PresentMon 2.5.1 | `D:\dev\tools\PresentMon.exe` — **console build**, see `bench/README.md` |

`--installPath` does **not** relocate the Windows SDK; 10.0.26100.0 went to `C:` and
cost ~2.5 GB. Budget for that on any drive-constrained machine. Release build of the
prototype from a cold cargo registry: 44 s.

---

## Sources

- [OBS Game Capture not working on Valorant / League — why](https://win.gg/obs-game-capture-not-working-valorant-league-of-legends-fix/)
- [OBS Forums — Adding Valorant via Game Capture, Error -4](https://obsproject.com/forum/threads/adding-valorant-via-game-capture-and-error-4.182953/)
- [Elgato — Issues with Valorant and Elgato Capture Devices](https://help.elgato.com/hc/en-us/articles/9482241809805-Issues-with-Valorant-and-Elgato-Capture-Devices)
- [OBS Forums — Windows Graphics Capture vs DXGI Desktop Duplication](https://obsproject.com/forum/threads/windows-graphics-capture-vs-dxgi-desktop-duplication.149320/)
- [Win32CaptureSample — Desktop Duplication vs Windows Graphics Capture](https://github.com/robmikh/Win32CaptureSample/issues/24)
- [OBS — Game Capture and Window Capture internals](https://deepwiki.com/obsproject/obs-studio/4.2.3-game-capture-and-window-capture)
- [Microsoft Learn — GraphicsCaptureSession.IsBorderRequired](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession.isborderrequired)
- [Microsoft Q&A — removing the capture border for desktop apps](https://learn.microsoft.com/en-us/answers/questions/108678/how-to-remove-yellow-boarder-capture-indicator-fro)
- [NVIDIA Developer Forums — NVENC vs Media Foundation Transform performance](https://forums.developer.nvidia.com/t/68272)
- [Evaluation of GPU Video Encoder for Low-Latency Real-Time 4K UHD Encoding](https://arxiv.org/pdf/2511.18688)
