// The Postgres backend, run against real Postgres.
//
// PGlite is Postgres itself compiled to WebAssembly, so this executes the
// actual DDL and the actual queries — the DDL is checked by a planner that will
// reject it, `ON CONFLICT` needs an index that really exists, and `ON DELETE
// SET NULL` either fires or does not. A hand-written fake would have agreed
// with whatever the code did, which is the one thing a test must not do.
//
// The only thing not exercised is Neon's HTTP transport. Everything that could
// be wrong in code written here is.
//
//   node --test test/store-pg.test.js

const test = require('node:test');
const assert = require('node:assert');
const { PGlite } = require('@electric-sql/pglite');

const db = new PGlite();

/**
 * Neon's tagged-template interface, on top of PGlite.
 *
 * `sql\`... ${a} ...\`` becomes a parameterised query with `$1`, `$2`, … and
 * returns rows — the same contract store-pg.js is written against, so the
 * module under test is loaded unmodified.
 */
const neonShim = (strings, ...values) => {
  const text = strings.reduce((acc, s, i) => acc + s + (i < values.length ? `$${i + 1}` : ''), '');
  return db.query(text, values).then((r) => r.rows);
};

// Swap the driver before store-pg.js can require the real one. Injecting here
// rather than adding a seam to the production module keeps the thing being
// tested identical to the thing that ships.
require.cache[require.resolve('@neondatabase/serverless')] = {
  id: require.resolve('@neondatabase/serverless'),
  filename: require.resolve('@neondatabase/serverless'),
  loaded: true,
  exports: { neon: () => neonShim },
};
process.env.POSTGRES_URL = 'postgres://pglite/in-memory';

const store = require('../store-pg');

test.after(() => db.close());

test('the schema applies, and applies twice', async () => {
  await store.ensureSchema();
  // Idempotent on purpose: it runs on demand rather than as a migration step
  // someone has to remember, so a second call must be harmless.
  await store.ensureSchema();
  const rows = await neonShim`
    SELECT tablename FROM pg_tables WHERE schemaname = 'public' ORDER BY tablename`;
  assert.deepStrictEqual(rows.map((r) => r.tablename), ['comments', 'matches', 'notes', 'shots', 'vods']);
});

test('matches round-trip and sort newest first', async () => {
  await store.saveMatch({
    id: 'm-old', map: 'Split', mapSplash: 's1', playedOn: '2026-08-13',
    kind: 'tournament', label: 'Playoffs R1', score: '13-7',
  });
  await store.saveMatch({
    id: 'm-new', map: 'Ascent', mapSplash: 's2', playedOn: '2026-08-15',
    kind: 'scrim', label: 'vs Team Liquid', score: '13-9',
  });

  const list = await store.readMatches();
  assert.deepStrictEqual(list.map((m) => m.id), ['m-new', 'm-old']);

  // camelCase out, snake_case in the table — the translation is easy to get
  // half-right, which shows up as undefined in the UI rather than as an error.
  const [m] = list;
  assert.strictEqual(m.mapSplash, 's2');
  assert.strictEqual(m.playedOn, '2026-08-15');
  assert.ok(!Number.isNaN(Date.parse(m.createdUtc)));
});

test('saving an existing match updates it rather than inserting a second', async () => {
  await store.saveMatch({
    id: 'm-new', map: 'Ascent', mapSplash: 's2', playedOn: '2026-08-15',
    kind: 'scrim', label: 'vs Team Liquid', score: '13-11',
  });
  const list = await store.readMatches();
  assert.strictEqual(list.filter((m) => m.id === 'm-new').length, 1);
  assert.strictEqual(list.find((m) => m.id === 'm-new').score, '13-11');
});

test('re-registering a video updates in place', async () => {
  await store.saveVod({
    id: 'v1', videoId: 'dQw4w9WgXcQ', title: '', player: 'babu',
    matchId: 'm-new', label: 'babu POV', source: 'youtube',
  });
  // Same video, different generated id — the unique index on video_id has to
  // catch this, or a corrected player name leaves two POVs behind.
  const again = await store.saveVod({
    id: 'v2-different-id', videoId: 'dQw4w9WgXcQ', title: '', player: 'corrected',
    matchId: 'm-new', label: 'babu POV', source: 'youtube',
  });
  assert.strictEqual(again.id, 'v1');
  assert.strictEqual(again.player, 'corrected');
  assert.strictEqual((await store.readVods()).length, 1);
});

test('deleting a match detaches its POVs but removes its notes and shots', async () => {
  await store.addNote('m-new', { id: 'n1', author: 'babu', body: 'lost mid every round' });
  await store.addShot('m-new', {
    id: 's1', url: 'https://blob/x.png', contentType: 'image/png', label: 'Half 1', bytes: 1234,
  });
  assert.strictEqual((await store.readNotes('m-new')).length, 1);
  assert.strictEqual((await store.readShots('m-new')).length, 1);

  assert.ok(await store.deleteMatch('m-new'));

  const [vod] = await store.readVods();
  assert.ok(vod, 'the POV should survive its match');
  assert.strictEqual(vod.matchId, null, 'and be detached, not orphaned by a dangling id');
  assert.strictEqual((await store.readNotes('m-new')).length, 0);
  assert.strictEqual((await store.readShots('m-new')).length, 0);
});

test('deleting something that is not there reports false rather than throwing', async () => {
  assert.strictEqual(await store.deleteMatch('nope'), false);
  assert.strictEqual(await store.deleteVod('nope'), false);
  assert.strictEqual(await store.deleteNote('m-old', 'nope'), false);
  assert.strictEqual(await store.deleteComment('thread', 'nope'), false);
  assert.strictEqual(await store.deleteShot('m-old', 'nope'), false);
  assert.strictEqual(await store.findShot('m-old', 'nope'), null);
});

test('notes read oldest first, comments read in timeline order', async () => {
  await store.addNote('m-old', { id: 'n-a', author: 'babu', body: 'first' });
  await store.addNote('m-old', { id: 'n-b', author: 'jett', body: 'second' });
  assert.deepStrictEqual((await store.readNotes('m-old')).map((n) => n.body), ['first', 'second']);

  // Inserted out of order deliberately: ordering has to come from the query.
  await store.addComment('t1', { id: 'c-late', atMs: 128000, author: 'sova', body: 'late', vodId: 'v1' });
  await store.addComment('t1', { id: 'c-early', atMs: 1000, author: 'babu', body: 'early', vodId: 'v1' });
  const got = await store.readComments('t1');
  assert.deepStrictEqual(got.map((c) => c.body), ['early', 'late']);

  // BIGINT arrives as a string from the driver unless it is converted, and a
  // string here would break every seek the review page does.
  assert.strictEqual(typeof got[0].atMs, 'number');
  assert.strictEqual(got[1].atMs, 128000);
});

test('comment threads do not leak into each other', async () => {
  await store.addComment('t2', { id: 'c-other', atMs: 5, author: 'x', body: 'other thread', vodId: null });
  assert.deepStrictEqual((await store.readComments('t1')).map((c) => c.id), ['c-early', 'c-late']);
  assert.strictEqual((await store.readComments('t2')).length, 1);
  assert.strictEqual((await store.readComments('never-used')).length, 0);
});

test('shots keep their blob URL and a numeric size', async () => {
  await store.addShot('m-old', {
    id: 's2', url: 'https://blob/y.png', contentType: 'image/png', label: 'Half 2', bytes: 98765,
  });
  const [shot] = await store.readShots('m-old');
  assert.strictEqual(shot.url, 'https://blob/y.png');
  assert.strictEqual(shot.contentType, 'image/png');
  assert.strictEqual(typeof shot.bytes, 'number');

  const found = await store.findShot('m-old', 's2');
  assert.strictEqual(found.url, 'https://blob/y.png');
  // No `file` key: that absence is what makes the server redirect to the CDN
  // instead of trying to stream a path that does not exist.
  assert.strictEqual(found.file, undefined);
});

test("a shot is not reachable through another match's id", async () => {
  assert.strictEqual(await store.findShot('m-someone-else', 's2'), null);
  assert.strictEqual(await store.deleteShot('m-someone-else', 's2'), false);
  assert.ok(await store.findShot('m-old', 's2'), 'and is still there afterwards');
});

test('a POV can exist with no match, and text survives a round trip', async () => {
  await store.saveVod({
    id: 'v-unfiled', videoId: 'kJQP7kiw5Fk', title: "O'Brien — 50% \"quoted\"",
    player: 'sova', matchId: null, label: '', source: 'youtube',
  });
  const v = (await store.readVods()).find((x) => x.id === 'v-unfiled');
  assert.strictEqual(v.matchId, null);
  // Quotes and dashes go through parameters, not string building. If they ever
  // stop doing so, this is the test that says so.
  assert.strictEqual(v.title, "O'Brien — 50% \"quoted\"");
});
