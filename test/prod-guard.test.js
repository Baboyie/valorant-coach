// Production misconfiguration, met the way a serverless host meets it.
//
// server.js refuses to serve when NODE_ENV=production has no sign-in
// configured. Run directly it exits, which is right: someone is watching a
// terminal. Imported by Vercel it must not — exiting during module init is a
// platform-level crash with no response body, so every route answers an opaque
// 500 and the reason lives only in a runtime log nobody thinks to open on
// their first deploy. It refuses per request instead, naming the variable.
//
//   node --test test/prod-guard.test.js

const test = require('node:test');
const assert = require('node:assert');
const http = require('node:http');
const os = require('os');
const path = require('path');
const fsp = require('fs/promises');

process.env.NODE_ENV = 'production';
process.env.DEBRIEF_DATA_DIR = path.join(os.tmpdir(), `debrief-guard-${process.pid}`);
delete process.env.GOOGLE_CLIENT_ID;
delete process.env.DEBRIEF_ALLOW_OPEN;
delete process.env.POSTGRES_URL;
delete process.env.DATABASE_URL;

// The refusal prints its reason on the way up. This asserts on the response
// rather than the console, so silence it and keep the test output readable.
const printed = [];
const realError = console.error;
console.error = (...args) => printed.push(args.join(' '));
const app = require('../server.js');
console.error = realError;

let server, base;
test.before(async () => {
  server = http.createServer(app);
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  base = `http://127.0.0.1:${server.address().port}`;
});
test.after(async () => {
  await new Promise((r) => server.close(r));
  await fsp.rm(process.env.DEBRIEF_DATA_DIR, { recursive: true, force: true });
});

test('the reason is printed for the platform log as well', () => {
  assert.match(printed.join('\n'), /GOOGLE_CLIENT_ID/);
});

test('every route refuses with 503 and names the variable', async () => {
  for (const p of ['/api/health', '/api/scrims', '/api/auth/me']) {
    const res = await fetch(base + p);
    assert.strictEqual(res.status, 503, p);
    const body = await res.json();
    assert.strictEqual(body.ok, false);
    // Exactly one: the session secret is only required once sign-in is on, so
    // reporting it here would send whoever is reading on a second errand.
    assert.deepStrictEqual(body.problems.map((x) => x.variable), ['GOOGLE_CLIENT_ID']);
    assert.match(body.problems[0].fix, /DEBRIEF_ALLOW_OPEN/);
  }
});

test('a write is refused before it can reach the store', async () => {
  const res = await fetch(`${base}/api/scrims`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ map: 'Ascent', playedOn: '2026-09-03' }),
  });
  assert.strictEqual(res.status, 503);
});

test('pages are not served either', async () => {
  // The guard sits in front of the static handler on purpose. On Vercel the CDN
  // serves the pages whatever this process does, and they will show the 503's
  // message the moment they call the API — but nothing here hands out a UI that
  // looks like a working deployment.
  assert.strictEqual((await fetch(`${base}/review.html`)).status, 503);
});
