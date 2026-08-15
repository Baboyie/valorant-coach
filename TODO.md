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

## 3. Smaller things

- **Code signing.** Smart App Control blocks unsigned binaries outright, so
  nothing ships to another machine until this is settled (ADR §9a). Independent
  of §1's MSIX-for-borderless-capture reason.
- **Microphone as a third stream.** Loopback captures what the player *hears*,
  including their teammates, but not what the player *says*. A separate WASAPI
  capture endpoint mixed as its own track would let a reviewer mute one side.
- **Replay ring is `Mutex`-guarded** where ADR §3 specifies lock-free, and the
  save is synchronous. Fine at 60 locks/s; revisit if a hotkey save ever stutters.
