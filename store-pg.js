// Postgres storage backend (Neon), for serverless deployment.
//
// Mirrors store-fs.js function for function so `vods.js` can pick one at
// startup and nothing above it changes. The filesystem version stays the local
// default: developing against a cloud database for a JSON file's worth of data
// would be a poor trade.
//
// Uses Neon's HTTP driver rather than a pooled `pg` client. Serverless
// functions are created and destroyed constantly, and a connection pool in that
// environment exhausts the database's connection limit rather than protecting
// it.

const { neon } = require('@neondatabase/serverless');

let sqlClient = null;
function sql() {
  if (!sqlClient) {
    const url = process.env.POSTGRES_URL || process.env.DATABASE_URL;
    if (!url) throw new Error('POSTGRES_URL is not set');
    sqlClient = neon(url);
  }
  return sqlClient;
}

/**
 * Create tables if absent.
 *
 * Idempotent and cheap, so it runs on demand rather than as a separate
 * migration step a teammate would have to remember. The data is small enough
 * that a real migration tool would be more machinery than the problem.
 */
let ready = null;
async function ensureSchema() {
  if (ready) return ready;
  const q = sql();
  ready = (async () => {
    await q`CREATE TABLE IF NOT EXISTS matches (
      id          TEXT PRIMARY KEY,
      map         TEXT NOT NULL DEFAULT '',
      map_splash  TEXT NOT NULL DEFAULT '',
      played_on   TEXT NOT NULL DEFAULT '',
      kind        TEXT NOT NULL DEFAULT 'scrim',
      label       TEXT NOT NULL DEFAULT '',
      score       TEXT NOT NULL DEFAULT '',
      created_utc TIMESTAMPTZ NOT NULL DEFAULT now()
    )`;

    await q`CREATE TABLE IF NOT EXISTS vods (
      id        TEXT PRIMARY KEY,
      video_id  TEXT NOT NULL,
      title     TEXT NOT NULL DEFAULT '',
      player    TEXT NOT NULL DEFAULT '',
      -- A POV outlives the match it was filed against: deleting a match
      -- detaches its POVs rather than destroying someone's upload record.
      match_id  TEXT REFERENCES matches(id) ON DELETE SET NULL,
      label     TEXT NOT NULL DEFAULT '',
      added_utc TIMESTAMPTZ NOT NULL DEFAULT now(),
      source    TEXT NOT NULL DEFAULT 'youtube'
    )`;
    // One row per video, matching the filesystem backend's dedupe.
    await q`CREATE UNIQUE INDEX IF NOT EXISTS vods_video_id_idx ON vods (video_id)`;

    await q`CREATE TABLE IF NOT EXISTS notes (
      id          TEXT PRIMARY KEY,
      match_id    TEXT NOT NULL,
      author      TEXT NOT NULL DEFAULT 'anonymous',
      body        TEXT NOT NULL,
      created_utc TIMESTAMPTZ NOT NULL DEFAULT now()
    )`;
    await q`CREATE INDEX IF NOT EXISTS notes_match_idx ON notes (match_id, created_utc)`;

    await q`CREATE TABLE IF NOT EXISTS comments (
      id          TEXT PRIMARY KEY,
      thread_id   TEXT NOT NULL,
      at_ms       BIGINT NOT NULL DEFAULT 0,
      author      TEXT NOT NULL DEFAULT 'anonymous',
      body        TEXT NOT NULL,
      vod_id      TEXT,
      created_utc TIMESTAMPTZ NOT NULL DEFAULT now()
    )`;
    await q`CREATE INDEX IF NOT EXISTS comments_thread_idx ON comments (thread_id, at_ms)`;

    await q`CREATE TABLE IF NOT EXISTS shots (
      id           TEXT PRIMARY KEY,
      match_id     TEXT NOT NULL,
      url          TEXT NOT NULL,
      content_type TEXT NOT NULL,
      label        TEXT NOT NULL DEFAULT '',
      bytes        BIGINT NOT NULL DEFAULT 0,
      added_utc    TIMESTAMPTZ NOT NULL DEFAULT now()
    )`;
    await q`CREATE INDEX IF NOT EXISTS shots_match_idx ON shots (match_id, added_utc)`;
  })();

  // A failure must not be remembered. Neon's free compute suspends after five
  // minutes idle, so the first query from an instance that has been sitting
  // quiet is exactly the one that can fail while the database wakes — and a
  // rejected promise left in `ready` would answer every later request from
  // that instance with the same error for as long as it lives, while other
  // instances served fine. "Broken for some people, some of the time" is the
  // hardest kind of failure to chase, and one line avoids it.
  //
  // The derived promise is caught here so the reset is not itself an unhandled
  // rejection; callers still await `ready` and see the original error.
  ready.catch(() => { ready = null; });
  return ready;
}

/* Row shapes are translated at the boundary so the API and the UI keep the
   camelCase/snake_case they already use, rather than every caller learning the
   column names. */

const toMatch = (r) => ({
  id: r.id,
  map: r.map,
  mapSplash: r.map_splash,
  playedOn: r.played_on,
  kind: r.kind,
  label: r.label,
  score: r.score,
  createdUtc: new Date(r.created_utc).toISOString(),
});

const toVod = (r) => ({
  id: r.id,
  videoId: r.video_id,
  title: r.title,
  player: r.player,
  matchId: r.match_id,
  label: r.label,
  addedUtc: new Date(r.added_utc).toISOString(),
  source: r.source,
});

/* --------------------------------------------------------------- matches */

async function readMatches() {
  await ensureSchema();
  const rows = await sql()`
    SELECT * FROM matches
    ORDER BY played_on DESC NULLS LAST, created_utc DESC`;
  return rows.map(toMatch);
}

async function saveMatch(rec) {
  await ensureSchema();
  await sql()`
    INSERT INTO matches (id, map, map_splash, played_on, kind, label, score)
    VALUES (${rec.id}, ${rec.map}, ${rec.mapSplash}, ${rec.playedOn},
            ${rec.kind}, ${rec.label}, ${rec.score})
    ON CONFLICT (id) DO UPDATE SET
      map = EXCLUDED.map, map_splash = EXCLUDED.map_splash,
      played_on = EXCLUDED.played_on, kind = EXCLUDED.kind,
      label = EXCLUDED.label, score = EXCLUDED.score`;
  const [row] = await sql()`SELECT * FROM matches WHERE id = ${rec.id}`;
  return toMatch(row);
}

async function deleteMatch(id) {
  await ensureSchema();
  // ON DELETE SET NULL on vods.match_id detaches POVs rather than removing
  // them; notes and shots for a dead match are of no use to anyone.
  const rows = await sql()`DELETE FROM matches WHERE id = ${id} RETURNING id`;
  await sql()`DELETE FROM notes WHERE match_id = ${id}`;
  await sql()`DELETE FROM shots WHERE match_id = ${id}`;
  return rows.length > 0;
}

/* ------------------------------------------------------------------ vods */

async function readVods() {
  await ensureSchema();
  const rows = await sql()`SELECT * FROM vods ORDER BY added_utc DESC`;
  return rows.map(toVod);
}

async function saveVod(rec) {
  await ensureSchema();
  // Re-registering a video updates it in place, so a corrected player name
  // does not leave two entries behind.
  await sql()`
    INSERT INTO vods (id, video_id, title, player, match_id, label, source)
    VALUES (${rec.id}, ${rec.videoId}, ${rec.title}, ${rec.player},
            ${rec.matchId}, ${rec.label}, ${rec.source})
    ON CONFLICT (video_id) DO UPDATE SET
      title = EXCLUDED.title, player = EXCLUDED.player,
      match_id = EXCLUDED.match_id, label = EXCLUDED.label`;
  const [row] = await sql()`SELECT * FROM vods WHERE video_id = ${rec.videoId}`;
  return toVod(row);
}

async function deleteVod(id) {
  await ensureSchema();
  const rows = await sql()`DELETE FROM vods WHERE id = ${id} RETURNING id`;
  return rows.length > 0;
}

/* ----------------------------------------------------------------- notes */

async function readNotes(matchId) {
  await ensureSchema();
  const rows = await sql()`
    SELECT * FROM notes WHERE match_id = ${matchId} ORDER BY created_utc ASC`;
  return rows.map((r) => ({
    id: r.id, author: r.author, body: r.body,
    createdUtc: new Date(r.created_utc).toISOString(),
  }));
}

async function addNote(matchId, entry) {
  await ensureSchema();
  await sql()`
    INSERT INTO notes (id, match_id, author, body)
    VALUES (${entry.id}, ${matchId}, ${entry.author}, ${entry.body})`;
  return entry;
}

async function deleteNote(matchId, noteId) {
  await ensureSchema();
  const rows = await sql()`
    DELETE FROM notes WHERE id = ${noteId} AND match_id = ${matchId} RETURNING id`;
  return rows.length > 0;
}

/* -------------------------------------------------------------- comments */

async function readComments(threadId) {
  await ensureSchema();
  const rows = await sql()`
    SELECT * FROM comments WHERE thread_id = ${threadId} ORDER BY at_ms ASC`;
  return rows.map((r) => ({
    id: r.id, atMs: Number(r.at_ms), author: r.author, body: r.body,
    vodId: r.vod_id, createdUtc: new Date(r.created_utc).toISOString(),
  }));
}

async function addComment(threadId, entry) {
  await ensureSchema();
  await sql()`
    INSERT INTO comments (id, thread_id, at_ms, author, body, vod_id)
    VALUES (${entry.id}, ${threadId}, ${entry.atMs}, ${entry.author},
            ${entry.body}, ${entry.vodId})`;
  return entry;
}

async function deleteComment(threadId, commentId) {
  await ensureSchema();
  const rows = await sql()`
    DELETE FROM comments WHERE id = ${commentId} AND thread_id = ${threadId} RETURNING id`;
  return rows.length > 0;
}

/* ----------------------------------------------------------------- shots */

async function readShots(matchId) {
  await ensureSchema();
  const rows = await sql()`
    SELECT * FROM shots WHERE match_id = ${matchId} ORDER BY added_utc ASC`;
  return rows.map((r) => ({
    id: r.id, url: r.url, contentType: r.content_type, label: r.label,
    bytes: Number(r.bytes), addedUtc: new Date(r.added_utc).toISOString(),
  }));
}

async function addShot(matchId, rec) {
  await ensureSchema();
  await sql()`
    INSERT INTO shots (id, match_id, url, content_type, label, bytes)
    VALUES (${rec.id}, ${matchId}, ${rec.url}, ${rec.contentType},
            ${rec.label}, ${rec.bytes})`;
  return rec;
}

async function findShot(matchId, shotId) {
  await ensureSchema();
  const [row] = await sql()`
    SELECT * FROM shots WHERE id = ${shotId} AND match_id = ${matchId}`;
  if (!row) return null;
  return { id: row.id, url: row.url, contentType: row.content_type, bytes: Number(row.bytes) };
}

async function deleteShot(matchId, shotId) {
  await ensureSchema();
  const rows = await sql()`
    DELETE FROM shots WHERE id = ${shotId} AND match_id = ${matchId} RETURNING id`;
  return rows.length > 0;
}

module.exports = {
  kind: 'postgres',
  ensureSchema,
  readMatches, saveMatch, deleteMatch,
  readVods, saveVod, deleteVod,
  readNotes, addNote, deleteNote,
  readComments, addComment, deleteComment,
  readShots, addShot, findShot, deleteShot,
};
