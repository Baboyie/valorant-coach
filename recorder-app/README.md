# recorder-app — DEBRIEF

The desktop app: a tray clipper with manual recording, built on the capture
pipeline measured in [`../docs/ADR-001-capture-architecture.md`](../docs/ADR-001-capture-architecture.md).

Tauri v2 shell (Rust + WebView2). The UI is plain HTML/CSS/JS with **no build
step** — no npm install, no bundler. `cargo build` is the whole story.

## Running it

```bash
cargo run --release -p recorder-app
```

It appears in the tray and opens a window. Closing the window **hides it**;
buffering continues. Quit from the tray menu, which lets the engine finalise any
open file first.

With Valorant running it starts buffering automatically. Press **Alt+F10** in
game to save the last 30 seconds. Clips land in `Videos\DEBRIEF\clips`,
recordings in `Videos\DEBRIEF\recordings`, and both show in the in-app
gallery, which plays them over a self-served `media:` scheme with range
support, so seeking a multi-GB recording is instant. If a video will not play,
`%APPDATA%\DEBRIEF\media.log` says what the handler saw.

Clicking **Start recording** while the game is minimised — which is what
tabbing out of exclusive fullscreen does — queues the recording, restores the
game, and starts the moment the window has a real size again.

## Architecture

```
webview (UI)  ──invoke──▶  Tauri commands  ──channel──▶  engine thread
     ▲                                                        │
     └──────────── 2 Hz status snapshot ◀────────────────────┘
                                                              │
                                        recorder-core: WGC ─▶ NVENC ─▶ ring / file
```

**One thread owns the pipeline.** Every Media Foundation and Direct3D object
lives on the engine thread; the UI sends commands and reads a status snapshot,
and can never block capture no matter what the webview does. That is ADR §6's
"the capture path never depends on the React layer" made structural rather than
aspirational.

**Buffering and manual recording are mutually exclusive.** Running both means two
encoder instances fed the same textures — easy to build, but it doubles
encode-engine load, and the 15.3% figure §9 measured is what the overhead claim
rests on. Recording already produces the footage a clip would have contained, so
the exclusion costs nothing real.

**Game detection** is a window-class lookup on a 2-second tick that only runs
while idle — ADR §4's fallback path. The `SetWinEventHook` version §4 specifies
as primary needs a message pump on the engine thread, which would complicate the
frame loop; the fallback alone costs a `EnumWindows` call every 2 s and never
enumerates the process table.

## Settings

`%APPDATA%\DEBRIEF\config.json` — a plain file, deliberately readable and
editable. Changing settings in the UI restarts the capture session so window
length, frame rate and bitrate take effect immediately. The hotkey needs a
restart.

| | |
|---|---|
| `window_secs` | how much gameplay the ring holds (default 30) |
| `fps` | recording frame rate (default 60 — the clip is for review, not for playback at 240) |
| `bitrate_mbps` | 0 = derive from resolution and frame rate (~12 Mbps at 1080p60) |
| `save_hotkey` | Tauri accelerator syntax. Default `Alt+F10`, not a bare function key: Valorant binds those, and a global hotkey that shadows a game binding is experienced as the game breaking |
| `output_dir` | default `Videos\DEBRIEF` |
| `auto_buffer` | start buffering as soon as the target appears |
| `target` | what to record: `{"kind":"valorant"}` (default), a whole screen `{"kind":"monitor","device":"\\.\DISPLAY1"}`, or one window `{"kind":"window","title":"...","class":"..."}`. The UI fills this from a Discord-style picker; saved by identity, not handle, so it survives restarts. A window target follows the *window*, not the title — a browser retitling itself on a tab switch does not end the session. §29 performance numbers are only measured for Valorant; a 1440p/4K monitor is more pixels than a 1080p game window and encodes accordingly |
| `capture_audio` | record desktop audio via WASAPI loopback (default on) — what the player hears, game and comms together |
| `notify_toast` | the Medal/ShadowPlay-style card in the corner (default on). An always-on-top, click-through, WS_EX_NOACTIVATE window of our own — **not** a hook into the game, so it carries no anti-cheat risk. The price of not hooking: it can only appear over a game running *borderless*; in true exclusive fullscreen nothing but a hook draws over the picture. Preview it from Settings |
| `notify_sound` | chime on save, recording start/stop and failure (default on). The only confirmation that reaches you in game — Windows silences its own notifications while a game is fullscreen. Loopback records it, so it can be heard in the *next* thing recorded, never in the clip just saved |
| `capture_mic` | record the microphone as a **separate track** (default off), so a reviewer can isolate or mute the player's own voice. Off by default because many machines have no usable microphone, and a silent extra track is worse than none |

Audio tracks stay **separate rather than mixed** (§23). That is also the lighter
option: a microphone often runs at a different sample rate than the output
device, so mixing them would need resampling — exactly the audio processing §23
says to keep out of the way.

The config file must be plain UTF-8 **without a byte-order mark**. Notepad and
PowerShell's `-Encoding utf8` both write one; a BOM is stripped on load, but
other JSON tools may not be so forgiving.

## Self-test

The Tauri window cannot be clicked from a script, so there is a headless mode
that drives the engine directly and reports what happened. It exercises
everything below the webview: config, engine thread, detection, buffering, save.

```bash
cargo build --release -p recorder-app --features console
set DEBRIEF_AUTOTEST=1
target\release\recorder-app.exe
```

`DEBRIEF_AUTOTEST_SECS` sets how long to buffer before saving (default 20).
The `console` feature keeps a console attached in release builds, which the
autotest needs to report into.

`DEBRIEF_TEST_FOREGROUND=1` records the foreground window instead of Valorant.
A test affordance, not a feature: the engine is otherwise only ever willing to
record the game, which makes the clip and audio paths unverifiable whenever it
is not running.

## Building on a OneDrive-synced checkout

**Build outside the synced folder.** Smart App Control (enforcing on the
benchmark rig) refuses to execute freshly built unsigned binaries that carry
Mark-of-the-Web, and OneDrive tags synced files with MOTW — so cargo's own
build-script executables get blocked:

```
error: failed to run custom build command for `recorder-app`
  An Application Control policy has blocked this file. (os error 4551)
```

Fix with a `.cargo/config.toml` in the repo root (git-ignored, since the path is
machine-specific):

```toml
[build]
target-dir = "D:/dev/vc-target"
```

This also stops OneDrive syncing gigabytes of build artefacts, whose file locks
can fail a link step mid-build.

**This is a preview of a shipping problem, not just a dev annoyance.** Smart App
Control will block the distributed app for the same reason unless it is
code-signed, which reinforces ADR §1's conclusion that this must ship as a signed
MSIX package — that requirement now has a second, independent justification
beyond removing the WGC capture border.

### Smart App Control now blocks the built app itself (2026-08-15)

Moving the target directory off OneDrive fixed the *build*. It does not fix the
*output*. On this rig, with SAC enforcing
(`HKLM:\SYSTEM\CurrentControlSet\Control\CI\Policy` →
`VerifiedAndReputablePolicyState = 1`), a freshly linked `recorder-app.exe` is
refused:

```
Start-Process : This command cannot be run due to the error:
An Application Control policy has blocked this file.
```

What is confusing, and worth writing down so the next person does not chase it:

- The file has **no** Mark-of-the-Web, so this is not the OneDrive problem.
- Build-script executables linked minutes earlier ran fine, so it is not a
  blanket refusal of everything newly compiled.
- `recorder-proto.exe` from earlier the same day still runs. SAC's verdicts are
  reputation-based and per-file, and it had already blessed that one.
- A **later** rebuild, with SAC still enforcing, was allowed. A block is not a
  new steady state and rebuilds are not doomed — retry before concluding
  anything.

So it is not predictable from the build, and **a working unsigned binary is a
scarce resource**: rebuilding overwrites it via cargo's hardlink and there is no
way back, since the previous artifact in `deps/` is replaced too. Copy a known
good `recorder-app.exe` aside before rebuilding — `D:\dev\vc-known-good` on this
rig — so a bad verdict costs a retry rather than the evening.

**It also blocks build scripts and proc-macro DLLs, and there retrying does
not help.** `cargo check` (the debug profile) currently cannot run at all here:
freshly linked proc-macro DLLs are refused with `os error 4551`, so validate
with `cargo build --release` instead. Worse, a *new* dependency can be
unbuildable outright — `tauri-plugin-notification` pulls `rand` → `zerocopy`,
whose build script was refused twelve times across twelve fresh links. That is
why notifications are a chime and a tray tooltip rather than Windows toasts.
A half-built tree also leaves fingerprints claiming success, so cargo will
happily hand rustc `--extern` paths to artifacts that no longer exist; the
symptom is `can't find crate for tauri` with no attempt to rebuild it, and the
fix is `cargo clean --release` and a full rebuild.

**The working workaround for the app binary: relink and retry.** Verdicts attach to the file, so a
fresh link is a fresh verdict — `touch src/main.rs && cargo build --release`
and launch again. Twice now a blocked binary was followed by an allowed one
with SAC unchanged in between. Combined with keeping a known-good copy in
`D:\dev\vc-known-good`, a block costs minutes, not the evening.

**Do not turn SAC off over a single block.** Turning it off is irreversible —
re-enabling requires reinstalling Windows — and the evidence above says the next
build may well be fine. It is the nuclear option, not the fix.

The one permanent fix is to **sign the binary** with a certificate from a CA that
SAC trusts. Self-signed certificates do not work however they are installed
locally: SAC ignores additions to the local trust store by design. For an
open-source project with a public repo, the SignPath Foundation
(<https://signpath.org/>) issues certificates for free, which would also remove
the WGC capture border that ADR §1 wants MSIX for.

Meanwhile `recorder-proto` (`probe|capture|record|replay|audio`) still runs and
records full matches, which is what gets uploaded anyway.

## Known gaps

- Not code-signed, so Smart App Control blocks it on machines that enforce it.
- Not packaged as MSIX, so WGC draws its yellow capture border (ADR §1).
- Gallery has no thumbnails; rows are text. A thumbnail means decoding one
  frame per file through a Media Foundation source reader.
- The engine's `SetWinEventHook` detection is the fallback, not the primary path.
