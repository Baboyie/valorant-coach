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

## Known gaps

- Not code-signed, so Smart App Control blocks it on machines that enforce it.
- Not packaged as MSIX, so WGC draws its yellow capture border (ADR §1).
- Clips list is not in the UI; "Show in Explorer" reveals the last one.
- The engine's `SetWinEventHook` detection is the fallback, not the primary path.
