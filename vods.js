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

module.exports = {
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
