// Filesystem storage backend — the local default.
//
// Same function surface as store-pg.js, so `vods.js` picks one at startup and
// nothing above notices. This stays the default for local runs: developing
// against a cloud database for a JSON file's worth of data would be a bad
// trade, and it keeps the app runnable with no accounts at all.

const fs = require('fs');
const fsp = require('fs/promises');
const path = require('path');

function root() {
  const base = process.env.DEBRIEF_DATA_DIR || path.join(__dirname, 'data');
  return path.join(base, 'vods');
}

async function readJson(file, fallback) {
  try {
    return JSON.parse(await fsp.readFile(file, 'utf8'));
  } catch {
    return fallback;
  }
}

async function writeJson(file, value) {
  await fsp.mkdir(path.dirname(file), { recursive: true });
  await fsp.writeFile(file, JSON.stringify(value, null, 2));
}

const matchesFile = () => path.join(root(), 'matches.json');
const vodsFile = () => path.join(root(), 'youtube.json');
const notesFile = (m) => path.join(root(), `notes-${m}.json`);
const commentsFile = (t) => path.join(root(), `comments-${t}.json`);
const shotsDir = (m) => path.join(root(), 'shots', m);
const shotsIndex = (m) => path.join(shotsDir(m), 'index.json');

/* --------------------------------------------------------------- matches */

async function readMatches() {
  const list = await readJson(matchesFile(), []);
  return list.sort(
    (a, b) =>
      (b.playedOn || '').localeCompare(a.playedOn || '') ||
      (b.createdUtc || '').localeCompare(a.createdUtc || '')
  );
}

async function saveMatch(rec) {
  const list = await readJson(matchesFile(), []);
  const next = list.some((m) => m.id === rec.id)
    ? list.map((m) => (m.id === rec.id ? rec : m))
    : [rec, ...list];
  await writeJson(matchesFile(), next);
  return rec;
}

async function deleteMatch(id) {
  const list = await readJson(matchesFile(), []);
  const next = list.filter((m) => m.id !== id);
  await writeJson(matchesFile(), next);

  // Detach POVs rather than deleting them: removing a match by accident should
  // not take everyone's uploads with it.
  const vods = await readJson(vodsFile(), []);
  await writeJson(vodsFile(), vods.map((v) => (v.matchId === id ? { ...v, matchId: null } : v)));

  await fsp.rm(notesFile(id), { force: true });
  await fsp.rm(shotsDir(id), { recursive: true, force: true });
  return list.length !== next.length;
}

/* ------------------------------------------------------------------ vods */

async function readVods() {
  return readJson(vodsFile(), []);
}

async function saveVod(rec) {
  const list = await readJson(vodsFile(), []);
  const existing = list.find((v) => v.videoId === rec.videoId);
  const merged = existing ? { ...rec, id: existing.id, addedUtc: existing.addedUtc } : rec;
  const next = existing
    ? list.map((v) => (v.videoId === rec.videoId ? merged : v))
    : [merged, ...list];
  await writeJson(vodsFile(), next);
  return merged;
}

async function deleteVod(id) {
  const list = await readJson(vodsFile(), []);
  const next = list.filter((v) => v.id !== id);
  await writeJson(vodsFile(), next);
  return list.length !== next.length;
}

/* ----------------------------------------------------------------- notes */

async function readNotes(matchId) {
  return readJson(notesFile(matchId), []);
}

async function addNote(matchId, entry) {
  const list = await readNotes(matchId);
  list.push(entry); // oldest first: a discussion reads top to bottom
  await writeJson(notesFile(matchId), list);
  return entry;
}

async function deleteNote(matchId, noteId) {
  const list = await readNotes(matchId);
  const next = list.filter((n) => n.id !== noteId);
  await writeJson(notesFile(matchId), next);
  return list.length !== next.length;
}

/* -------------------------------------------------------------- comments */

async function readComments(threadId) {
  return readJson(commentsFile(threadId), []);
}

async function addComment(threadId, entry) {
  const list = await readComments(threadId);
  list.push(entry);
  list.sort((a, b) => a.atMs - b.atMs);
  await writeJson(commentsFile(threadId), list);
  return entry;
}

async function deleteComment(threadId, commentId) {
  const list = await readComments(threadId);
  const next = list.filter((c) => c.id !== commentId);
  await writeJson(commentsFile(threadId), next);
  return list.length !== next.length;
}

/* ----------------------------------------------------------------- shots */

async function readShots(matchId) {
  const list = await readJson(shotsIndex(matchId), []);
  // The URL is what callers use; on disk that is our own serving route.
  return list.map((s) => ({ ...s, url: `/api/scrims/${matchId}/shots/${s.id}` }));
}

async function addShot(matchId, rec, bytes) {
  await fsp.mkdir(shotsDir(matchId), { recursive: true });
  await fsp.writeFile(path.join(shotsDir(matchId), `${rec.id}.${rec.ext}`), bytes);
  const list = await readJson(shotsIndex(matchId), []);
  list.push(rec);
  await writeJson(shotsIndex(matchId), list);
  return { ...rec, url: `/api/scrims/${matchId}/shots/${rec.id}` };
}

async function findShot(matchId, shotId) {
  const list = await readJson(shotsIndex(matchId), []);
  const rec = list.find((s) => s.id === shotId);
  if (!rec) return null;
  return {
    ...rec,
    // Local files are streamed by our own route rather than redirected to.
    file: path.join(shotsDir(matchId), `${shotId}.${rec.ext}`),
  };
}

async function deleteShot(matchId, shotId) {
  const found = await findShot(matchId, shotId);
  if (!found) return false;
  await fsp.rm(found.file, { force: true });
  const list = await readJson(shotsIndex(matchId), []);
  await writeJson(shotsIndex(matchId), list.filter((s) => s.id !== shotId));
  return true;
}

module.exports = {
  kind: 'filesystem',
  ensureSchema: async () => {},
  root,
  createReadStream: fs.createReadStream,
  readMatches, saveMatch, deleteMatch,
  readVods, saveVod, deleteVod,
  readNotes, addNote, deleteNote,
  readComments, addComment, deleteComment,
  readShots, addShot, findShot, deleteShot,
};
