# TODO

Next things to build, with enough context to pick up cold.

## 1. ~~Desktop audio capture~~ — done 2026-08-15

WASAPI loopback → AAC, on both the file and replay paths. See ADR §9b.

**One piece outstanding:** §9's "recording costs nothing measurable at a 240 fps
cap" was measured **video-only**. Adding an audio encode invalidates it until
re-measured, so the capped benchmark needs one more pass before that claim is
repeated anywhere. `recorder-proto record --no-audio` reproduces the original
video-only configuration for comparison. This needs a play session — see
`bench/README.md`.

## 2. Team VOD reviewer

Review clips and match VODs as a team, rather than alone.

Likely belongs with the existing web strand, not the desktop app:
`server.js` + `public/index.html` + `public/planner.html`. That server already
proxies the Riot API and generates AI coaching reports from match stats
(`/api/coach`), so per-match context already exists to hang a review on.

Not yet scoped. Open questions worth settling before building:

- What is being reviewed — clips DEBRIEF produces, full match VODs, or both?
- Is "team" synchronous (everyone watching together, shared playhead) or
  asynchronous (timestamped comments others read later)? These are very different
  builds.
- Where does video live? Local files are simplest, but "team" implies something
  hosted, which brings storage, auth, and accounts — none of which the project
  has today.
- Does it tie into the existing `/api/coach` report, so a review can jump to the
  rounds the coach flagged?

The desktop app has never been assessed against this: `public/planner.html` is
68 KB of tactical planner that may already do some of what a reviewer needs.
Read that strand before designing anything new.

## 3. Gaps against the original requirement

Full audit in [`docs/REQUIREMENTS-STATUS.md`](docs/REQUIREMENTS-STATUS.md). The
three that matter, in order:

- **ShadowPlay comparison (§18, §29).** The benchmark harness is rigorous but has
  only baseline × ours. §30's entire positioning — "comparable to or better than
  ShadowPlay" — is unproven without that column, and §1 forbids claiming it
  unmeasured. Needs NVIDIA app installed and a play session; `Measure-Frames.ps1`
  already recognises `shadowplay-run*` filenames, so it is running the runs, not
  changing the analysis.
- **In-app performance monitor (§17).** The app shows the recorder's health but
  not the player's: no game FPS, frame time, CPU %, GPU %, VRAM, encoder
  utilisation, or disk write speed. This is the screen that makes the product's
  claim visible to the user mid-match, and §17 specifies it concretely.
- **Microphone (§23).** Loopback captures what the player hears, not what they
  say. For a team-review product this is the more valuable half, and it is what
  makes §23's "separate audio tracks" meaningful.

Also outstanding but lower value for now: Competitive/Quality/Custom presets
(§11), storage estimation (§15), drive selection with free space and write speed
(§14), selectable HEVC (§22), cloud upload (§13).

## 4. Smaller things

- **Code signing.** Smart App Control blocks unsigned binaries outright, so
  nothing ships to another machine until this is settled (ADR §9a). Independent
  of §1's MSIX-for-borderless-capture reason.
- **Microphone as a third stream.** Loopback captures what the player *hears*,
  including their teammates, but not what the player *says*. A separate WASAPI
  capture endpoint mixed as its own track would let a reviewer mute one side.
- **Replay ring is `Mutex`-guarded** where ADR §3 specifies lock-free, and the
  save is synchronous. Fine at 60 locks/s; revisit if a hotkey save ever stutters.
