// The serverless entry point, driven the way Vercel drives it.
//
// The first deploy shipped a `[...path].js` catch-all that Vercel matched as a
// single segment. `/api/scrims` worked, `/api/scrims/:id/notes` and
// `/api/auth/me` 404'd at the router before any of our code ran, so nothing in
// the app's logs showed it. Every test passed, because every test called
// Express directly and skipped the layer that was broken.
//
// So these requests go in the shape Vercel produces *after* the rewrite —
// `/api/index?__debrief_path=scrims/abc/notes` — and assert the path is put
// back together before Express sees it.
//
//   node --test test/api-entry.test.js

const test = require('node:test');
const assert = require('node:assert');
const http = require('node:http');
const os = require('os');
const path = require('path');
const fsp = require('fs/promises');

process.env.DEBRIEF_DATA_DIR = path.join(os.tmpdir(), `debrief-entry-${process.pid}`);
delete process.env.POSTGRES_URL;
delete process.env.DATABASE_URL;
delete process.env.NODE_ENV;

const handler = require('../api/index.js');

let server, base;
test.before(async () => {
  server = http.createServer(handler);
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  base = `http://127.0.0.1:${server.address().port}`;
});
test.after(async () => {
  await new Promise((r) => server.close(r));
  await fsp.rm(process.env.DEBRIEF_DATA_DIR, { recursive: true, force: true });
});

/** Ask for a path the way Vercel's rewrite hands it to the function. */
const asVercel = (apiPath, query = '') =>
  fetch(`${base}/api/index?__debrief_path=${apiPath}${query ? `&${query}` : ''}`);

test('a one-segment route survives the rewrite', async () => {
  const res = await asVercel('health');
  assert.strictEqual(res.status, 200);
  assert.strictEqual((await res.json()).ok, true);
});

test('a two-segment route survives the rewrite', async () => {
  // This is the one that was dead in production.
  const res = await asVercel('auth/me');
  assert.strictEqual(res.status, 200);
  assert.ok('enabled' in (await res.json()));
});

test('a three-segment route survives the rewrite', async () => {
  for (const p of ['scrims/abc/notes', 'scrims/abc/shots', 'match/abc/comments']) {
    const res = await asVercel(p);
    assert.strictEqual(res.status, 200, p);
  }
});

test('percent-encoded separators work too', async () => {
  // Whether the platform hands the segments back raw or encoded is not
  // something to have an opinion about.
  const res = await asVercel('scrims%2Fabc%2Fnotes');
  assert.strictEqual(res.status, 200);
  assert.deepStrictEqual(await res.json(), { notes: [] });
});

test('the original query string is preserved', async () => {
  // The key header has to be present or the route rejects on that first and
  // the assertion proves nothing. With it, a 400 naming the host means `url`
  // arrived and was parsed; a dropped query would say "Missing url" instead.
  const res = await fetch(
    `${base}/api/index?__debrief_path=riot-proxy&url=${encodeURIComponent('https://evil.example.com/x')}`,
    { headers: { 'X-Riot-Key': 'not-a-real-key' } }
  );
  assert.strictEqual(res.status, 400);
  assert.match((await res.json()).error, /api\.riotgames\.com/);
});

test('the carrier parameter does not leak into the route', async () => {
  // It is our own plumbing; a route seeing it would be a surprise later.
  const res = await asVercel('scrims');
  const body = await res.json();
  assert.ok(!JSON.stringify(body).includes('__debrief_path'));
});

test('a POST keeps its method, body and headers through the rewrite', async () => {
  const res = await fetch(`${base}/api/index?__debrief_path=scrims`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ map: 'Ascent', playedOn: '2026-08-15', label: 'rewrite check' }),
  });
  assert.strictEqual(res.status, 200);
  const m = await res.json();
  assert.strictEqual(m.map, 'Ascent');
  assert.strictEqual(m.label, 'rewrite check');
});

test('an unknown route still 404s rather than being swallowed', async () => {
  assert.strictEqual((await asVercel('no/such/route')).status, 404);
});
