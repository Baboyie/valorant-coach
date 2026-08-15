// Review data: matches, POVs, notes, comments, scoreboards.
//
// This file holds the rules — validation, id generation, dedupe, what a "match"
// means. Where the bytes land is store-fs.js or store-pg.js, chosen once at
// startup. Callers never learn which.
//
// The split exists because the app has two homes: a PC on someone's desk, where
// files on disk are the right answer, and a serverless deployment, where they
// are impossible. Neither should force compromises on the other.

const crypto = require('crypto');

// Postgres when a connection string is present, disk otherwise. No flag to set
// — a deployment that has a database is a deployment that wants it.
const usePg = !!(process.env.POSTGRES_URL || process.env.DATABASE_URL);
const store = usePg ? require('./store-pg') : require('./store-fs');

/** Reject anything that could escape a storage path or a SQL identifier. */
function safeId(id) {
  return typeof id === 'string' && /^[A-Za-z0-9_-]{1,64}$/.test(id);
}

function newId() {
  return crypto.randomBytes(12).toString('hex');
}

/* ---------------------------------------------------- youtube link parsing */

/**
 * Pull the 11-character video id out of whatever form of link someone pastes.
 *
 * People paste watch URLs, share links, embeds, links with a timestamp, or just
 * the bare id. Accepting only one shape would feel broken for no reason.
 */
function parseYouTubeId(input) {
  if (typeof input !== 'string') return null;
  const s = input.trim();
  if (/^[A-Za-z0-9_-]{11}$/.test(s)) return s;
  const patterns = [
    /[?&]v=([A-Za-z0-9_-]{11})/,
    /youtu\.be\/([A-Za-z0-9_-]{11})/,
    /\/embed\/([A-Za-z0-9_-]{11})/,
    /\/shorts\/([A-Za-z0-9_-]{11})/,
    /\/live\/([A-Za-z0-9_-]{11})/,
  ];
  for (const re of patterns) {
    const m = re.exec(s);
    if (m) return m[1];
  }
  return null;
}

/* -------------------------------------------------------------- matches */

// A match is what a team reviews: one map, one date, one scrim block, with the
// POVs hanging off it. Created explicitly rather than inferred from timestamps,
// because a team running three scrims in an evening wants to *name* them.
async function readMatches() {
  return store.readMatches();
}

async function saveMatch(input) {
  const existing = input.id && safeId(input.id)
    ? (await store.readMatches()).find((m) => m.id === input.id)
    : null;

  return store.saveMatch({
    id: existing ? existing.id : newId(),
    map: String(input.map || '').slice(0, 40),
    // Splash art URL comes from the client, which already fetched the map list
    // for its picker. Storing it means the grid renders without every page load
    // re-fetching the whole catalogue.
    mapSplash: String(input.mapSplash || '').slice(0, 400),
    // ISO date. Several scrims share one, which is why `label` exists too.
    playedOn: String(input.playedOn || '').slice(0, 10),
    kind: ['scrim', 'ranked', 'tournament', 'vod'].includes(input.kind) ? input.kind : 'scrim',
    label: String(input.label || '').slice(0, 120),
    score: String(input.score || '').slice(0, 20),
    createdUtc: existing ? existing.createdUtc : new Date().toISOString(),
  });
}

async function deleteMatch(id) {
  if (!safeId(id)) return false;
  return store.deleteMatch(id);
}

/* ------------------------------------------------------------------ vods */

// The database stores a *link*, never a video. Full recordings live on YouTube;
// this is the index that says whose POV it is and which match it belongs to.
async function readYouTube() {
  return store.readVods();
}

async function addYouTube(entry) {
  const videoId = parseYouTubeId(entry.url || entry.videoId);
  if (!videoId) throw new Error('Could not find a YouTube video id in that link.');

  // Null is allowed: a POV nobody has filed yet is still worth keeping, and
  // demanding a match up front would make adding a link a two-step chore.
  let matchId = safeId(entry.matchId) ? entry.matchId : null;

  // A well-formed id for a match that no longer exists gets treated as unfiled.
  // Two people with the page open is enough to produce this — one deletes a
  // match while the other files a POV against it. Storing the dead id instead
  // would hide the POV completely: it belongs to no match, and it is not
  // "unfiled" either, so it appears nowhere on the page.
  if (matchId && !(await store.readMatches()).some((m) => m.id === matchId)) {
    matchId = null;
  }

  return store.saveVod({
    id: newId(),
    videoId,
    title: String(entry.title || '').slice(0, 200),
    player: String(entry.player || '').slice(0, 64),
    matchId,
    label: String(entry.label || '').slice(0, 200),
    addedUtc: new Date().toISOString(),
    source: 'youtube',
  });
}

async function removeYouTube(id) {
  if (!safeId(id)) return false;
  return store.deleteVod(id);
}

/* ------------------------------------------------------------ match notes */

// Everyone's take on the match as a whole, as opposed to comments, which pin to
// one moment in one POV.
async function readNotes(matchId) {
  if (!safeId(matchId)) return [];
  return store.readNotes(matchId);
}

async function addNote(matchId, note) {
  if (!safeId(matchId)) throw new Error('Bad match id.');
  const body = String(note.body || '').slice(0, 4000);
  if (!body.trim()) throw new Error('empty note');
  return store.addNote(matchId, {
    id: newId(),
    author: String(note.author || 'anonymous').slice(0, 64),
    body,
    createdUtc: new Date().toISOString(),
  });
}

async function deleteNote(matchId, noteId) {
  if (!safeId(matchId) || !safeId(noteId)) return false;
  return store.deleteNote(matchId, noteId);
}

/* --------------------------------------------------------------- comments */

async function readComments(threadId) {
  if (!safeId(threadId)) return [];
  return store.readComments(threadId);
}

async function addComment(threadId, comment) {
  if (!safeId(threadId)) throw new Error('Bad thread id.');
  const body = String(comment.body || '').slice(0, 4000);
  if (!body.trim()) throw new Error('empty comment');
  return store.addComment(threadId, {
    id: newId(),
    // Anchored to a position in this video.
    atMs: Math.max(0, Math.round(Number(comment.atMs) || 0)),
    author: String(comment.author || 'anonymous').slice(0, 64),
    body,
    vodId: safeId(comment.vodId) ? comment.vodId : null,
    createdUtc: new Date().toISOString(),
  });
}

async function deleteComment(threadId, commentId) {
  if (!safeId(threadId) || !safeId(commentId)) return false;
  return store.deleteComment(threadId, commentId);
}

/* ---------------------------------------------------- match screenshots */

// Only these types, and the extension comes from the content-type rather than
// any filename the client supplies: a filename is attacker-controlled input,
// and honouring it is how "scoreboard.png" turns into something executable.
const SHOT_TYPES = {
  'image/png': 'png',
  'image/jpeg': 'jpg',
  'image/webp': 'webp',
  'image/gif': 'gif',
};
// Generous for a 4K screenshot, small enough that a wrong paste cannot fill a
// disk or a free storage tier.
const SHOT_MAX_BYTES = 12 * 1024 * 1024;

async function readShots(matchId) {
  if (!safeId(matchId)) return [];
  return store.readShots(matchId);
}

async function addShot(matchId, { contentType, bytes, label }) {
  if (!safeId(matchId)) throw new Error('Bad match id.');
  const ext = SHOT_TYPES[contentType];
  if (!ext) throw new Error(`Unsupported image type: ${contentType || 'none given'}.`);
  if (!bytes || !bytes.length) throw new Error('Empty upload.');
  if (bytes.length > SHOT_MAX_BYTES) throw new Error('Image is larger than 12 MB.');

  const rec = {
    id: newId(),
    ext,
    contentType,
    label: String(label || '').slice(0, 120),
    bytes: bytes.length,
    addedUtc: new Date().toISOString(),
  };

  if (store.kind === 'postgres') {
    // Blob storage, because a serverless filesystem cannot hold it.
    const { put } = require('@vercel/blob');
    const blob = await put(`shots/${matchId}/${rec.id}.${ext}`, bytes, {
      access: 'public',
      contentType,
      addRandomSuffix: false,
    });
    return store.addShot(matchId, { ...rec, url: blob.url });
  }
  return store.addShot(matchId, rec, bytes);
}

async function shotFile(matchId, shotId) {
  if (!safeId(matchId) || !safeId(shotId)) return null;
  return store.findShot(matchId, shotId);
}

async function deleteShot(matchId, shotId) {
  if (!safeId(matchId) || !safeId(shotId)) return false;
  const found = await store.findShot(matchId, shotId);
  if (found && store.kind === 'postgres' && found.url) {
    const { del } = require('@vercel/blob');
    // A failed blob delete must not block the row delete: an orphaned image
    // costs a few KB, a row pointing at nothing breaks the page.
    try { await del(found.url); } catch { /* orphan is the lesser evil */ }
  }
  return store.deleteShot(matchId, shotId);
}

module.exports = {
  store,
  storeKind: store.kind,
  ensureSchema: store.ensureSchema,
  safeId,
  newId,
  parseYouTubeId,
  readMatches, saveMatch, deleteMatch,
  readYouTube, addYouTube, removeYouTube,
  readNotes, addNote, deleteNote,
  readComments, addComment, deleteComment,
  readShots, addShot, shotFile, deleteShot,
  SHOT_TYPES, SHOT_MAX_BYTES,
};
