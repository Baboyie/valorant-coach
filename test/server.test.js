// Tests for the parts that fail quietly.
//
// Not a broad suite. These cover the three things that, when wrong, look like
// the app working: a forged session that verifies, an id that escapes its
// directory, and a YouTube link that silently registers the wrong video.
//
//   node --test test/

const test = require('node:test');
const assert = require('node:assert');
const os = require('os');
const path = require('path');
const fsp = require('fs/promises');

// Both must be set before the modules load: auth reads the secret lazily, but
// vods picks its storage backend at require time.
process.env.DEBRIEF_SESSION_SECRET = 'test-secret-not-a-real-one';
delete process.env.POSTGRES_URL;
delete process.env.DATABASE_URL;
process.env.DEBRIEF_DATA_DIR = path.join(os.tmpdir(), `debrief-test-${process.pid}`);

const auth = require('../auth');
const vods = require('../vods');

test.after(() => fsp.rm(process.env.DEBRIEF_DATA_DIR, { recursive: true, force: true }));

/* -------------------------------------------------------------- sessions */

const asReq = (cookie) => ({
  headers: { cookie: cookie == null ? undefined : `${auth.SESSION_COOKIE}=${encodeURIComponent(cookie)}` },
});
const USER = { sub: '1', email: 'babu.ochir@gmail.com', name: 'Babu', picture: null };

test('a session cookie round-trips', () => {
  const s = auth.getSession(asReq(auth.newSession(USER)));
  assert.strictEqual(s.user.email, USER.email);
  assert.ok(s.expiresAt > Date.now());
});

test('no cookie is no session', () => {
  assert.strictEqual(auth.getSession(asReq(null)), null);
  assert.strictEqual(auth.getSession({ headers: {} }), null);
});

test('an edited payload is rejected', () => {
  const c = auth.newSession(USER);
  const dot = c.lastIndexOf('.');
  const [payload, sig] = [c.slice(0, dot), c.slice(dot + 1)];
  const flipped = (payload[0] === 'a' ? 'b' : 'a') + payload.slice(1);
  assert.strictEqual(auth.getSession(asReq(`${flipped}.${sig}`)), null);
});

test('a forged identity is rejected', () => {
  // Well-formed claims, no valid signature — the shape an attacker can produce
  // without the secret.
  const payload = Buffer.from(
    JSON.stringify({ user: { email: 'attacker@example.com', name: 'Attacker' }, exp: Date.now() + 1e9 })
  ).toString('base64url');
  assert.strictEqual(auth.getSession(asReq(`${payload}.${'x'.repeat(43)}`)), null);
  // A wrong-length signature must be rejected too, not throw: timingSafeEqual
  // raises on mismatched lengths, which would be a 500 rather than a 401.
  assert.strictEqual(auth.getSession(asReq(`${payload}.short`)), null);
  assert.strictEqual(auth.getSession(asReq('nodotatall')), null);
});

test('an expired session is rejected even with a good signature', () => {
  const real = auth.newSession(USER);
  const dot = real.lastIndexOf('.');
  // Re-sign an expired payload the way the server would, proving expiry is
  // checked on its own rather than riding on the signature.
  const crypto = require('crypto');
  const payload = Buffer.from(
    JSON.stringify({ user: USER, exp: Date.now() - 1000 })
  ).toString('base64url');
  const sig = crypto
    .createHmac('sha256', process.env.DEBRIEF_SESSION_SECRET)
    .update(payload)
    .digest('base64url');
  assert.notStrictEqual(real.slice(dot + 1), sig); // sanity: different payloads
  assert.strictEqual(auth.getSession(asReq(`${payload}.${sig}`)), null);
});

/* ------------------------------------------------------------ id safety */

test('safeId rejects anything that could escape a path', () => {
  for (const ok of ['abc123', 'a-b_c', 'A'.repeat(64)]) {
    assert.ok(vods.safeId(ok), ok);
  }
  for (const bad of ['../etc', 'a/b', 'a\\b', '', 'A'.repeat(65), 'a.b', null, undefined, 42, {}]) {
    assert.ok(!vods.safeId(bad), String(bad));
  }
});

/* --------------------------------------------------------- youtube links */

test('parseYouTubeId accepts the forms people actually paste', () => {
  const id = 'dQw4w9WgXcQ';
  for (const form of [
    id,
    `https://www.youtube.com/watch?v=${id}`,
    `https://www.youtube.com/watch?v=${id}&t=91s`,
    `https://m.youtube.com/watch?app=desktop&v=${id}`,
    `https://youtu.be/${id}`,
    `https://youtu.be/${id}?t=91`,
    `https://www.youtube.com/embed/${id}`,
    `https://www.youtube.com/shorts/${id}`,
    `https://www.youtube.com/live/${id}`,
    `  https://youtu.be/${id}  `,
  ]) {
    assert.strictEqual(vods.parseYouTubeId(form), id, form);
  }
});

test('parseYouTubeId refuses what it cannot read', () => {
  for (const bad of ['', 'not a link', 'https://youtube.com/', 'https://vimeo.com/12345', null, 42]) {
    assert.strictEqual(vods.parseYouTubeId(bad), null, String(bad));
  }
});

/* -------------------------------------------------------------- storage */

test('the whole review round-trips through the store', async (t) => {
  const match = await vods.saveMatch({
    map: 'Ascent', playedOn: '2026-08-15', kind: 'scrim', label: 'Scrim 1',
  });
  assert.ok(vods.safeId(match.id));

  await t.test('an unknown kind falls back rather than being stored', async () => {
    const m = await vods.saveMatch({ map: 'Bind', kind: 'nonsense' });
    assert.strictEqual(m.kind, 'scrim');
  });

  await t.test('re-adding the same video updates rather than duplicates', async () => {
    const a = await vods.addYouTube({ url: 'https://youtu.be/dQw4w9WgXcQ', player: 'babu', matchId: match.id });
    const b = await vods.addYouTube({ url: 'https://www.youtube.com/watch?v=dQw4w9WgXcQ', player: 'corrected' });
    assert.strictEqual(a.videoId, b.videoId);
    assert.strictEqual(a.id, b.id);
    assert.strictEqual(b.player, 'corrected');
    assert.strictEqual((await vods.readYouTube()).filter((v) => v.videoId === 'dQw4w9WgXcQ').length, 1);
  });

  await t.test('a POV filed against a deleted match lands in unfiled', async () => {
    // Otherwise it belongs to no match and is not unfiled either, so it shows
    // up nowhere at all — and on Postgres it is a foreign-key 500.
    const v = await vods.addYouTube({ url: 'https://youtu.be/9bZkp7q19f0', matchId: 'deadbeef' });
    assert.strictEqual(v.matchId, null);
  });

  await t.test('comments come back in timeline order, not insertion order', async () => {
    await vods.addComment(match.id, { atMs: 91000, body: 'late' });
    await vods.addComment(match.id, { atMs: 1000, body: 'early' });
    const got = await vods.readComments(match.id);
    assert.deepStrictEqual(got.map((c) => c.body), ['early', 'late']);
  });

  await t.test('an empty note is refused', async () => {
    await assert.rejects(() => vods.addNote(match.id, { body: '   ' }));
  });

  await t.test('a non-image is refused', async () => {
    await assert.rejects(
      () => vods.addShot(match.id, { contentType: 'application/pdf', bytes: Buffer.from('x') }),
      /Unsupported image type/
    );
  });

  await t.test('deleting a match detaches its POVs instead of destroying them', async () => {
    assert.ok(await vods.deleteMatch(match.id));
    const left = (await vods.readYouTube()).find((v) => v.videoId === 'dQw4w9WgXcQ');
    assert.ok(left, 'the POV should survive its match');
    assert.strictEqual(left.matchId, null);
  });
});
