// VOD storage, match grouping, and review comments.
//
// Self-hosted on purpose. A 5-stack recording a 40-minute match at 16 Mbps is
// roughly 4.8 GB per player — about 24 GB for one match — so hosted object
// storage would cost real money for a team that already owns a spare drive.
// Run this on any machine the team can reach and point every DEBRIEF client at
// it. There is deliberately no account system: a LAN or a private VPS is the
// trust boundary, and inventing auth here would be security theatre.
//
// The interesting logic is `groupIntoMatches`. Everything else is file I/O.

const fs = require('fs');
const fsp = require('fs/promises');
const path = require('path');
const crypto = require('crypto');

// Two recordings belong to the same match if their time ranges overlap at all.
// Players start recording at different moments — someone hits record in agent
// select, someone else mid-first-round — so the only thing they reliably share
// is that they were recording *at the same time*.
const GAP_TOLERANCE_MS = 90 * 1000;

function vodRoot(baseDir) {
  return path.join(baseDir, 'vods');
}

async function ensureDirs(baseDir) {
  await fsp.mkdir(vodRoot(baseDir), { recursive: true });
}

/** Reject anything that could escape the storage directory. */
function safeId(id) {
  return typeof id === 'string' && /^[A-Za-z0-9_-]{1,64}$/.test(id);
}

function newId() {
  return crypto.randomBytes(12).toString('hex');
}

/** Read every sidecar we have stored. */
async function listVods(baseDir) {
  const root = vodRoot(baseDir);
  let entries;
  try {
    entries = await fsp.readdir(root);
  } catch {
    return [];
  }
  const out = [];
  for (const id of entries) {
    if (!safeId(id)) continue;
    try {
      const meta = JSON.parse(await fsp.readFile(path.join(root, id, 'meta.json'), 'utf8'));
      const stat = await fsp.stat(path.join(root, id, 'video.mp4')).catch(() => null);
      out.push({
        ...meta,
        id,
        bytes: stat ? stat.size : 0,
        // An upload that died partway leaves metadata with no playable file.
        // Surfaced rather than hidden, so a teammate can see their POV failed
        // instead of wondering why nobody can see them.
        complete: !!stat && stat.size > 0,
      });
    } catch {
      // A half-written VOD directory is skipped, not fatal.
    }
  }
  return out;
}

/**
 * Group recordings into matches by overlapping wall-clock time.
 *
 * This is the whole trick behind multi-POV: five people record the same match
 * on five machines with no shared identifier, but their recordings necessarily
 * overlap in absolute time. Sort by start, then extend a group while the next
 * recording begins before the group ends (plus tolerance for someone who
 * started a beat late).
 *
 * Tolerance exists because a player who alt-tabs, restarts the recorder, or
 * joins a match late would otherwise be split into their own "match" and
 * disappear from the review.
 */
function groupIntoMatches(vods) {
  const usable = vods
    .filter((v) => Number.isFinite(v.started_epoch_ms) && Number.isFinite(v.duration_secs))
    .sort((a, b) => a.started_epoch_ms - b.started_epoch_ms);

  const groups = [];
  for (const v of usable) {
    const start = v.started_epoch_ms;
    const end = start + v.duration_secs * 1000;
    const g = groups[groups.length - 1];
    if (g && start <= g.endsMs + GAP_TOLERANCE_MS) {
      g.vods.push(v);
      g.startsMs = Math.min(g.startsMs, start);
      g.endsMs = Math.max(g.endsMs, end);
    } else {
      groups.push({ startsMs: start, endsMs: end, vods: [v] });
    }
  }

  return groups.map((g) => ({
    // Deterministic: the same recordings always produce the same match id, so
    // comments survive a server restart or a re-scan.
    id: crypto.createHash('sha1').update(String(g.startsMs)).digest('hex').slice(0, 12),
    startsMs: g.startsMs,
    endsMs: g.endsMs,
    startedUtc: new Date(g.startsMs).toISOString(),
    durationSecs: Math.round((g.endsMs - g.startsMs) / 100) / 10,
    players: [...new Set(g.vods.map((v) => v.player || 'unknown'))],
    vods: g.vods.map((v) => ({
      id: v.id,
      player: v.player || 'unknown',
      startedEpochMs: v.started_epoch_ms,
      // Offset from the match start — what a player needs to seek this POV to
      // a given match time. Doing the arithmetic here means every client
      // agrees rather than each re-deriving it.
      offsetMs: v.started_epoch_ms - g.startsMs,
      durationSecs: v.duration_secs,
      width: v.width,
      height: v.height,
      fps: v.fps,
      audioTracks: v.audio_tracks || [],
      kind: v.kind,
      bytes: v.bytes,
      complete: v.complete,
    })),
  }));
}

/* ------------------------------------------------------------- comments */

function commentsPath(baseDir, matchId) {
  return path.join(vodRoot(baseDir), `comments-${matchId}.json`);
}

async function readComments(baseDir, matchId) {
  try {
    return JSON.parse(await fsp.readFile(commentsPath(baseDir, matchId), 'utf8'));
  } catch {
    return [];
  }
}

async function addComment(baseDir, matchId, comment) {
  const list = await readComments(baseDir, matchId);
  const entry = {
    id: newId(),
    // Anchored to **match time**, not to a position in one person's video.
    // A comment at 12:04 of the match means the same moment in every POV;
    // a comment at "4:31 into Alice's clip" means nothing to anyone else.
    atMs: Math.max(0, Math.round(Number(comment.atMs) || 0)),
    author: String(comment.author || 'anonymous').slice(0, 64),
    body: String(comment.body || '').slice(0, 4000),
    // Optionally pinned to whose POV prompted it, without restricting playback
    // to that POV.
    vodId: safeId(comment.vodId) ? comment.vodId : null,
    createdUtc: new Date().toISOString(),
  };
  if (!entry.body.trim()) throw new Error('empty comment');
  list.push(entry);
  list.sort((a, b) => a.atMs - b.atMs);
  await fsp.writeFile(commentsPath(baseDir, matchId), JSON.stringify(list, null, 2));
  return entry;
}

async function deleteComment(baseDir, matchId, commentId) {
  const list = await readComments(baseDir, matchId);
  const next = list.filter((c) => c.id !== commentId);
  await fsp.writeFile(commentsPath(baseDir, matchId), JSON.stringify(next, null, 2));
  return list.length !== next.length;
}

/* -------------------------------------------------------------- matches */

// A match is what a team actually reviews: one map, one date, one scrim block.
// Recordings hang off it, one per player.
//
// Created explicitly rather than inferred. `groupIntoMatches` below can derive
// matches from overlapping wall-clock time and does it well, but a team running
// three scrims in an evening wants to *name* them — "scrim vs Nova, map 2" —
// and no amount of timestamp arithmetic produces that.

function matchesPath(baseDir) {
  return path.join(vodRoot(baseDir), 'matches.json');
}

async function readMatches(baseDir) {
  try {
    return JSON.parse(await fsp.readFile(matchesPath(baseDir), 'utf8'));
  } catch {
    return [];
  }
}

async function writeMatches(baseDir, list) {
  await fsp.mkdir(vodRoot(baseDir), { recursive: true });
  await fsp.writeFile(matchesPath(baseDir), JSON.stringify(list, null, 2));
}

async function saveMatch(baseDir, input) {
  const list = await readMatches(baseDir);
  const existing = input.id ? list.find((m) => m.id === input.id) : null;

  const record = {
    id: existing ? existing.id : newId(),
    map: String(input.map || '').slice(0, 40),
    // Splash art is passed in by the client, which already fetched the map list
    // from valorant-api for the picker. Storing the URL means the card renders
    // without every page load re-fetching the whole map catalogue.
    mapSplash: String(input.mapSplash || '').slice(0, 400),
    // ISO date (YYYY-MM-DD). Several scrims a day share one, which is exactly
    // why `label` exists alongside it.
    playedOn: String(input.playedOn || '').slice(0, 10),
    kind: ['scrim', 'ranked', 'tournament', 'vod'].includes(input.kind) ? input.kind : 'scrim',
    label: String(input.label || '').slice(0, 120),
    score: String(input.score || '').slice(0, 20),
    createdUtc: existing ? existing.createdUtc : new Date().toISOString(),
  };

  const next = existing ? list.map((m) => (m.id === record.id ? record : m)) : [record, ...list];
  // Newest first, and within a day the most recently added first — which is the
  // order a team reviews an evening's scrims in.
  next.sort((a, b) => (b.playedOn || '').localeCompare(a.playedOn || '') || b.createdUtc.localeCompare(a.createdUtc));
  await writeMatches(baseDir, next);
  return record;
}

async function deleteMatch(baseDir, id) {
  const list = await readMatches(baseDir);
  const next = list.filter((m) => m.id !== id);
  await writeMatches(baseDir, next);

  // Detach its VODs rather than deleting them. Someone who removes a match by
  // accident should not lose everyone's uploads with it.
  const vids = await readYouTube(baseDir);
  const relinked = vids.map((v) => (v.matchId === id ? { ...v, matchId: null } : v));
  await fsp.writeFile(youtubeIndexPath(baseDir), JSON.stringify(relinked, null, 2));

  return list.length !== next.length;
}

/* --------------------------------------------------------- youtube VODs */

// A registered YouTube video: the team uploads wherever they like and hands us
// a link. No OAuth, no API quota, no upload code, and YouTube does the
// transcoding, seeking and CDN work that would otherwise be ours.
//
// The app stores only the video id and what the team says about it. Nothing
// here fetches from YouTube — the embed player does that in the browser.

function youtubeIndexPath(baseDir) {
  return path.join(vodRoot(baseDir), 'youtube.json');
}

/**
 * Pull the 11-character video id out of whatever form of link someone pastes.
 *
 * People paste watch URLs, share links, embeds, links with a timestamp, or
 * just the bare id. Accepting only one shape would make this feel broken for
 * no reason.
 */
function parseYouTubeId(input) {
  if (typeof input !== 'string') return null;
  const s = input.trim();
  if (/^[A-Za-z0-9_-]{11}$/.test(s)) return s;
  const patterns = [
    /[?&]v=([A-Za-z0-9_-]{11})/,      // watch?v=ID
    /youtu\.be\/([A-Za-z0-9_-]{11})/,  // youtu.be/ID
    /\/embed\/([A-Za-z0-9_-]{11})/,    // /embed/ID
    /\/shorts\/([A-Za-z0-9_-]{11})/,   // /shorts/ID
    /\/live\/([A-Za-z0-9_-]{11})/,     // /live/ID
  ];
  for (const re of patterns) {
    const m = re.exec(s);
    if (m) return m[1];
  }
  return null;
}

async function readYouTube(baseDir) {
  try {
    return JSON.parse(await fsp.readFile(youtubeIndexPath(baseDir), 'utf8'));
  } catch {
    return [];
  }
}

async function addYouTube(baseDir, entry) {
  const videoId = parseYouTubeId(entry.url || entry.videoId);
  if (!videoId) throw new Error('Could not find a YouTube video id in that link.');

  const list = await readYouTube(baseDir);
  // Re-registering the same video updates it rather than creating a duplicate,
  // so a fixed typo in a player name does not leave two entries behind.
  const existing = list.find((v) => v.videoId === videoId);
  const record = {
    id: existing ? existing.id : newId(),
    videoId,
    title: String(entry.title || '').slice(0, 200),
    player: String(entry.player || '').slice(0, 64),
    // Which match this POV belongs to. Null is allowed — a VOD that has not
    // been filed yet is still worth keeping, and forcing a match on upload
    // would make adding a link a two-step chore.
    matchId: safeId(entry.matchId) || entry.matchId ? String(entry.matchId).slice(0, 64) : null,
    // Free text, kept for VODs with no match: "Ascent, 13-11" is more use than
    // an id someone would have to look up.
    label: String(entry.label || '').slice(0, 200),
    addedUtc: existing ? existing.addedUtc : new Date().toISOString(),
    source: 'youtube',
  };
  const next = existing
    ? list.map((v) => (v.videoId === videoId ? record : v))
    : [record, ...list];
  await fsp.mkdir(vodRoot(baseDir), { recursive: true });
  await fsp.writeFile(youtubeIndexPath(baseDir), JSON.stringify(next, null, 2));
  return record;
}

async function removeYouTube(baseDir, id) {
  const list = await readYouTube(baseDir);
  const next = list.filter((v) => v.id !== id);
  await fsp.writeFile(youtubeIndexPath(baseDir), JSON.stringify(next, null, 2));
  return list.length !== next.length;
}

module.exports = {
  readMatches,
  saveMatch,
  deleteMatch,
  parseYouTubeId,
  readYouTube,
  addYouTube,
  removeYouTube,
  vodRoot,
  ensureDirs,
  safeId,
  newId,
  listVods,
  groupIntoMatches,
  readComments,
  addComment,
  deleteComment,
  GAP_TOLERANCE_MS,
};
