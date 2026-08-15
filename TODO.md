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

## 2. Team VOD reviewer — first version built

`public/review.html`. Paste a YouTube link, pick whose POV it is, watch it,
comment at timestamps. Comments jump the player when clicked.

Two decisions worth remembering, both of which removed most of the work:

- **No synchronised playback.** The team watches together over Discord
  screenshare, so a shared playhead would have bought nothing for the cost of a
  WebSocket server and session management.
- **No frame-accurate multi-POV alignment.** Comments are pinned to a position
  in one video rather than to a shared match timeline. The recorder still writes
  the UTC start time in its sidecar, and `vods.js` still groups recordings into
  matches by overlapping wall clock, so precise alignment remains available if
  it is ever wanted — it just is not required for this to be useful.

**YouTube hosting**, chosen over self-hosting: no storage cost, no transcoding,
no CDN, and seeking works. The `videos.insert` API quota turned out *not* to be
the blocker expected — since the June 2026 granular-quota change it has its own
100 uploads/day bucket rather than drawing 1600 units from the shared 10,000 —
but registering a pasted URL avoids OAuth, token refresh and Google verification
entirely, so automatic upload was not worth building yet.

Self-hosted upload (`POST /api/vod`, range-request streaming, match grouping)
stays in the server for VODs a team would rather not put on YouTube.

Still open:

- Automatic upload from the desktop app (needs OAuth per teammate).
- Comments are per-video; a comment does not appear on a teammate's POV of the
  same moment. Would need the match grouping to be wired into the review page.
- No auth on the server — a LAN or private VPS is the trust boundary.

## 2b. Team VOD reviewer — original scoping notes

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

- ~~ShadowPlay performance comparison (§18, §29).~~ **Deliberately skipped.** The
  only consequence: no comparative claim ships, per §29's own rule. The measured
  claim — no measurable FPS cost at a 240 cap — stands on its own and needs no
  competitor. Output quality *was* matched against real ShadowPlay clips (ADR
  §9c). If it is ever wanted, `Measure-Frames.ps1` already recognises
  `shadowplay-run*` filenames, so it is running the runs, not writing code.
- **In-app performance monitor (§17)** — mostly done. Recorder CPU, RAM, VRAM and
  disk write rate are live in the app alongside capture health. **Game FPS and
  frame time are still missing**: reading them honestly needs an ETW consumer of
  the kind PresentMon implements, which is a subsystem rather than a call.
  `bench/` already measures them properly out-of-process, so the gap is the
  live display, not the capability. §17 says never fake these — an ETW consumer
  or nothing.
- ~~Microphone (§23).~~ **Done** — captured as a separate AAC track alongside
  desktop audio, on both the file and replay paths. Off by default (`capture_mic`)
  since many machines have no usable microphone. Discord as its own third track
  is still not built; it currently arrives inside the desktop mix.

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
