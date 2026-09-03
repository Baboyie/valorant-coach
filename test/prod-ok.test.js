// The other half of the guard: a production deployment that *is* configured
// must serve normally. A refusal that fired on a correct deploy would be worse
// than the misconfiguration it was written to catch.
//
// This also covers what /api/health reports, which is the first thing anyone
// runs against a fresh deployment.
//
//   node --test test/prod-ok.test.js

const test = require('node:test');
const assert = require('node:assert');
const http = require('node:http');
const os = require('os');
const path = require('path');
const fsp = require('fs/promises');

process.env.NODE_ENV = 'production';
process.env.DEBRIEF_DATA_DIR = path.join(os.tmpdir(), `debrief-prod-ok-${process.pid}`);
process.env.GOOGLE_CLIENT_ID = '000000000000-test.apps.googleusercontent.com';
process.env.DEBRIEF_SESSION_SECRET = 'test-secret-not-a-real-one';
process.env.DEBRIEF_ALLOWED_EMAILS = 'player@example.com';
delete process.env.POSTGRES_URL;
delete process.env.DATABASE_URL;

const app = require('../server.js');

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

test('a configured production deployment serves', async () => {
  const res = await fetch(`${base}/api/health`);
  assert.strictEqual(res.status, 200);
  const body = await res.json();
  assert.strictEqual(body.ok, true);
  assert.strictEqual(body.auth, true);
  assert.strictEqual(body.allowed, 1);
  assert.ok(!('warnings' in body), `unexpected warnings: ${JSON.stringify(body.warnings)}`);
});

test('an empty allowlist is reported, because it locks everyone out silently', async () => {
  // Sign-in on with nobody allowed denies the person who just deployed it, and
  // says so by naming their own address — which reads as "wrong Google account"
  // rather than "unset variable". The config is read per request, so flipping
  // it here is what a deployment missing the variable looks like.
  const had = process.env.DEBRIEF_ALLOWED_EMAILS;
  process.env.DEBRIEF_ALLOWED_EMAILS = '';
  try {
    const body = await (await fetch(`${base}/api/health`)).json();
    // Still ok: reads work, and failing the check would take the site down over
    // something one environment variable fixes.
    assert.strictEqual(body.ok, true);
    assert.strictEqual(body.allowed, 0);
    assert.ok(body.warnings.some((w) => w.includes('DEBRIEF_ALLOWED_EMAILS')), body.warnings);
  } finally {
    process.env.DEBRIEF_ALLOWED_EMAILS = had;
  }
});

test('writes still require a signed-in teammate', async () => {
  const res = await fetch(`${base}/api/scrims`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ map: 'Ascent', playedOn: '2026-09-03' }),
  });
  assert.strictEqual(res.status, 401);
});
