// A failed schema init must not be remembered.
//
// `ensureSchema` caches its promise so the DDL runs once per instance. Caching
// a *rejected* one is the failure this guards: Neon's free compute suspends
// after five minutes idle, so the first query from an instance that has been
// sitting quiet is exactly the one that can fail while the database wakes. If
// that rejection stayed in the cache, every later request through that instance
// would get the same error for as long as it lived, while other instances
// served fine — "broken for some people, some of the time".
//
//   node --test test/store-pg-retry.test.js

const test = require('node:test');
const assert = require('node:assert');
const { PGlite } = require('@electric-sql/pglite');

const db = new PGlite();

// Real Postgres underneath, with one scripted failure in front of it — the
// shape a database that is still waking up produces.
let failNext = true;
let calls = 0;
const neonShim = (strings, ...values) => {
  calls++;
  if (failNext) {
    failNext = false;
    return Promise.reject(new Error('Error connecting to database: fetch failed'));
  }
  const text = strings.reduce((acc, s, i) => acc + s + (i < values.length ? `$${i + 1}` : ''), '');
  return db.query(text, values).then((r) => r.rows);
};

// Swap the driver before store-pg.js can require the real one, exactly as
// test/store-pg.test.js does, so the module under test ships unmodified.
require.cache[require.resolve('@neondatabase/serverless')] = {
  id: require.resolve('@neondatabase/serverless'),
  filename: require.resolve('@neondatabase/serverless'),
  loaded: true,
  exports: { neon: () => neonShim },
};
process.env.POSTGRES_URL = 'postgres://pglite/in-memory';

const store = require('../store-pg');

test.after(() => db.close());

test('a schema init that fails is retried, not cached forever', async () => {
  await assert.rejects(store.ensureSchema(), /fetch failed/);

  // The instance is not poisoned: the next request through it works, which is
  // the whole point — the database woke up, so the app should too.
  await store.ensureSchema();
  const m = await store.saveMatch({
    id: 'm1', map: 'Lotus', mapSplash: '', playedOn: '2026-09-03',
    kind: 'scrim', label: 'after the wake-up', score: '',
  });
  assert.strictEqual(m.label, 'after the wake-up');
});

test('a schema init that succeeded is still cached', async () => {
  // Retrying a failure is only correct if success is still remembered —
  // otherwise every request would re-run the DDL.
  const before = calls;
  await store.ensureSchema();
  assert.strictEqual(calls, before, 'ensureSchema re-ran its DDL after it had already succeeded');
});
