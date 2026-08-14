# TODO

Next things to build, with enough context to pick up cold.

## 1. Desktop audio capture

Clips are currently silent. Add desktop audio so a saved clip has comms and game
sound.

- **WASAPI loopback** (`IAudioClient` with `AUDCLNT_STREAMFLAGS_LOOPBACK`) is the
  route — out-of-process, no injection, so it stays inside the ADR §1 rule the
  same way WGC does.
- Feed it into the existing sink writer as a **second stream** (AAC), rather than
  muxing separately. The encoder path already exists; this is a new stream on it.
- **The replay ring needs an audio ring too.** Today it holds encoded video
  samples only, and the save picks a start point from a video keyframe. Audio has
  no keyframes, so the clip's audio must be trimmed to the chosen video start
  rather than the other way round.
- **A/V sync is the risk.** Video timestamps come from WGC's
  `SystemRelativeTime`; audio timestamps come from WASAPI's own clock. They need
  rebasing to a common origin, and the §7 lesson applies — do not assume a
  constant cadence for either.
- Watch the overhead claim: §9 measured recording at no measurable cost with
  video only. Adding an audio encode changes that, so the capped-240 benchmark
  should be re-run before the claim is repeated.

## 2. Team VOD reviewer

Review clips and match VODs as a team, rather than alone.

Likely belongs with the existing web strand, not the desktop app:
`server.js` + `public/index.html` + `public/planner.html`. That server already
proxies the Riot API and generates AI coaching reports from match stats
(`/api/coach`), so per-match context already exists to hang a review on.

Not yet scoped. Open questions worth settling before building:

- What is being reviewed — clips DEBRIEF produced, full match VODs, or both?
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
