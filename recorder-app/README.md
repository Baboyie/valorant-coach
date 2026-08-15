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
game to save the last 30 seconds. Clips land in `Videos\DEBRIEF`.

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
| `auto_buffer` | start buffering as soon as Valorant appears |
| `capture_audio` | record desktop audio via WASAPI loopback (default on) — what the player hears, game and comms together |
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

So it is not predictable from the build, and **a working unsigned binary is a
scarce resource**: rebuilding overwrites it via cargo's hardlink and there is no
way back, since the previous artifact in `deps/` is replaced too. Copy a known
good `recorder-app.exe` somewhere else before rebuilding if you need to keep the
ability to record that day.

There are only two real fixes, and neither is free:

1. **Turn Smart App Control off.** Windows Security → App & browser control →
   Smart App Control → Off. This is **irreversible** — re-enabling it requires
   reinstalling Windows — and it lowers the machine's protection generally.
2. **Sign the binary** with a certificate from a CA that SAC trusts. Self-signed
   certificates do not work, however they are installed locally: SAC ignores
   additions to the local trust store by design.

Meanwhile `recorder-proto` (`probe|capture|record|replay|audio`) still runs and
records full matches, which is what gets uploaded anyway.

## Known gaps

- Not code-signed, so Smart App Control blocks it on machines that enforce it.
- Not packaged as MSIX, so WGC draws its yellow capture border (ADR §1).
- Clips list is not in the UI; "Show in Explorer" reveals the last one.
- The engine's `SetWinEventHook` detection is the fallback, not the primary path.
