require('dotenv').config();
const express = require('express');
const path = require('path');
const fs = require('fs');
const fsp = require('fs/promises');
const vods = require('./vods');
const auth = require('./auth');

const app = express();
app.use(express.json({ limit: '256kb' }));
app.use(express.static(path.join(__dirname, 'public')));

// Sign-in routes. No-ops unless GOOGLE_CLIENT_ID is set, so an existing
// unauthenticated LAN setup keeps working exactly as it did.
auth.install(app);

// Refuse to serve a public deployment with writes wide open.
//
// Unauthenticated is the right default on a trusted LAN and a liability the
// moment the app has a public URL: anyone who finds it could post or delete a
// team's review. Rather than trust whoever deploys it to remember, production
// has to be configured or it does not boot.
//
// DEBRIEF_ALLOW_OPEN=1 is the escape hatch for a private network deployment
// that genuinely wants no sign-in.
if (process.env.NODE_ENV === 'production' && !auth.config().enabled
    && process.env.DEBRIEF_ALLOW_OPEN !== '1') {
  console.error(
    '\nRefusing to start: NODE_ENV=production with no GOOGLE_CLIENT_ID.\n' +
    'Anyone reaching this server could post or delete your team\'s review.\n\n' +
    'Set GOOGLE_CLIENT_ID and DEBRIEF_ALLOWED_EMAILS (see .env.example),\n' +
    'or set DEBRIEF_ALLOW_OPEN=1 if this really is a private network.\n'
  );
  process.exit(1);
}

// Same reasoning for the session secret. Without a fixed one each serverless
// instance signs with its own, so a cookie minted by one is rejected by the
// next and people are logged out at random — a bug that reads as flakiness
// rather than as misconfiguration, which is exactly why it is checked here.
if (process.env.NODE_ENV === 'production' && auth.config().enabled
    && !process.env.DEBRIEF_SESSION_SECRET) {
  console.error(
    '\nRefusing to start: sign-in is on but DEBRIEF_SESSION_SECRET is not set.\n' +
    'Sessions would break unpredictably across instances.\n\n' +
    'Generate one with:  node -e "console.log(require(\'crypto\').randomBytes(32).toString(\'base64url\'))"\n'
  );
  process.exit(1);
}

// Health check for the platform's restart logic. Proves the *storage* works,
// not merely that the process is up: a container without its volume, or a
// deployment with a bad connection string, looks perfectly healthy right up
// until the first save fails.
app.get('/api/health', async (_req, res) => {
  try {
    if (vods.storeKind === 'postgres') {
      await vods.ensureSchema();
      await vods.readMatches();
    } else {
      const dir = vods.store.root();
      await fsp.mkdir(dir, { recursive: true });
      const probe = path.join(dir, '.health');
      await fsp.writeFile(probe, String(Date.now()));
      await fsp.rm(probe, { force: true });
    }
    res.json({ ok: true, store: vods.storeKind, auth: auth.config().enabled });
  } catch (e) {
    res.status(503).json({ ok: false, store: vods.storeKind, error: e.message });
  }
});

// Only ever forward to Riot's own API hosts — never an arbitrary URL.
const RIOT_HOST_RE = /^[a-z0-9-]+\.api\.riotgames\.com$/i;

// GET /api/riot-proxy?url=<encoded Riot API url>
// Header: X-Riot-Key: <the user's personal Riot API key, entered client-side>
// Exists purely to dodge Riot's CORS block — the browser can't call
// api.riotgames.com directly, but this server can, so it forwards 1:1.
app.get('/api/riot-proxy', async (req, res) => {
  const target = req.query.url;
  const apiKey = req.get('x-riot-key');

  if (!target || !apiKey) {
    return res.status(400).json({ error: 'Missing url query param or X-Riot-Key header.' });
  }

  let parsed;
  try {
    parsed = new URL(target);
  } catch {
    return res.status(400).json({ error: 'Invalid url.' });
  }
  if (parsed.protocol !== 'https:' || !RIOT_HOST_RE.test(parsed.hostname)) {
    return res.status(400).json({ error: 'url must point at a *.api.riotgames.com host.' });
  }

  try {
    const riotRes = await fetch(parsed.toString(), {
      headers: { 'X-Riot-Token': apiKey }
    });
    const text = await riotRes.text();
    if (!riotRes.ok) {
      console.error(`[riot-proxy] ${riotRes.status} on ${parsed.pathname} — ${text.slice(0, 300)}`);
    }
    const retryAfter = riotRes.headers.get('retry-after');
    if (retryAfter) res.set('Retry-After', retryAfter);
    res.status(riotRes.status).type('application/json').send(text);
  } catch (e) {
    res.status(502).json({ error: 'Could not reach Riot API: ' + e.message });
  }
});

const COACH_SYSTEM_PROMPT = `You are a sharp, encouraging Valorant coach reviewing a player's recent match stats, including round-level economy, opening-duel, and trade data. Respond ONLY with raw JSON (no markdown fences, no preamble) matching exactly this shape:
{
  "overview": "2-3 sentence summary of current form",
  "strengths": ["...", "...", "..."],
  "weaknesses": ["...", "...", "..."],
  "economyNotes": "2-3 sentences on their buy-round decision making and win rates by buy type (eco/semi-buy/full-buy) — call out any leak (e.g. losing full buys, weak eco round conversion)",
  "positioningNotes": "2-3 sentences inferred from their opening-duel (entry) rate and trade rate — are they overextending and dying alone, entrying too passively, or getting traded well by teammates",
  "playOpportunities": ["3-4 concrete, situational suggestions about buy decisions, when to entry vs hold, and how to use their team's economy/positioning to create round-winning opportunities"],
  "tips": [{"title":"short title","detail":"1-2 sentence actionable tip"},{"title":"...","detail":"..."},{"title":"...","detail":"..."},{"title":"...","detail":"..."}],
  "focusArea": "one sentence naming the single highest-leverage thing to work on next"
}
Keep it concrete and grounded in the numbers given, not generic Valorant advice. If round-level economy/duel/trade data is missing or has zero rounds recorded, say so plainly in economyNotes/positioningNotes instead of inventing numbers.`;

// POST /api/coach  { summary: {...} }
// Anthropic key lives only in this server's .env — never sent to or from the browser.
app.post('/api/coach', async (req, res) => {
  const apiKey = process.env.ANTHROPIC_API_KEY;
  if (!apiKey) {
    return res.status(500).json({ error: 'Server is missing ANTHROPIC_API_KEY. Add it to .env and restart the server.' });
  }

  const summary = req.body && req.body.summary;
  if (!summary || typeof summary !== 'object') {
    return res.status(400).json({ error: 'Missing summary in request body.' });
  }

  try {
    const anthropicRes = await fetch('https://api.anthropic.com/v1/messages', {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        'x-api-key': apiKey,
        'anthropic-version': '2023-06-01'
      },
      body: JSON.stringify({
        model: 'claude-sonnet-5',
        max_tokens: 1500,
        system: COACH_SYSTEM_PROMPT,
        messages: [{ role: 'user', content: JSON.stringify(summary) }]
      })
    });

    const data = await anthropicRes.json();

    if (!anthropicRes.ok) {
      const msg = (data && data.error && data.error.message) || `Anthropic API error (${anthropicRes.status})`;
      return res.status(anthropicRes.status).json({ error: msg });
    }

    let text = (data.content || []).map(b => b.text || '').join('');
    text = text.replace(/```json|```/g, '').trim();

    let report;
    try {
      report = JSON.parse(text);
    } catch {
      return res.status(502).json({ error: 'Coach model returned non-JSON output.' });
    }

    res.json(report);
  } catch (e) {
    res.status(502).json({ error: 'Could not reach Anthropic API: ' + e.message });
  }
});

/* -------------------------------------------------------------- matches */

// Matches with their POVs attached, which is how the browser wants them: one
// request per page rather than a match list plus a VOD list to join by hand.
app.get('/api/scrims', async (_req, res) => {
  const [matches, videos] = await Promise.all([
    vods.readMatches(),
    vods.readYouTube(),
  ]);
  res.json({
    matches: matches.map((m) => ({
      ...m,
      vods: videos.filter((v) => v.matchId === m.id),
    })),
    // POVs nobody has filed against a match yet.
    unfiled: videos.filter((v) => !v.matchId),
  });
});

app.post('/api/scrims', auth.requireAuth, async (req, res) => {
  try {
    res.json(await vods.saveMatch(req.body || {}));
  } catch (e) {
    res.status(400).json({ error: e.message });
  }
});

app.delete('/api/scrims/:id', auth.requireAuth, async (req, res) => {
  res.json({ removed: await vods.deleteMatch(req.params.id) });
});

/* --------------------------------------------------------- match notes */

app.get('/api/scrims/:id/notes', async (req, res) => {
  res.json({ notes: await vods.readNotes(req.params.id) });
});

app.post('/api/scrims/:id/notes', auth.requireAuth, async (req, res) => {
  try {
    const body = { ...(req.body || {}) };
    if (req.user) body.author = req.user.name;
    res.json(await vods.addNote(req.params.id, body));
  } catch (e) {
    res.status(400).json({ error: e.message });
  }
});

app.delete('/api/scrims/:id/notes/:noteId', auth.requireAuth, async (req, res) => {
  res.json({ removed: await vods.deleteNote(req.params.id, req.params.noteId) });
});

/* --------------------------------------------------- match screenshots */

app.get('/api/scrims/:id/shots', async (req, res) => {
  res.json({ shots: await vods.readShots(req.params.id) });
});

// Raw image bytes in the body, type from Content-Type, optional label in a
// header. Same shape as the VOD upload, and it means a browser can post a
// pasted clipboard Blob directly with no multipart encoding in between.
app.post(
  '/api/scrims/:id/shots',
  auth.requireAuth,
  express.raw({ type: Object.keys(vods.SHOT_TYPES), limit: vods.SHOT_MAX_BYTES }),
  async (req, res) => {
    try {
      const rec = await vods.addShot(req.params.id, {
        contentType: req.get('content-type'),
        bytes: req.body,
        label: decodeURIComponent(req.get('x-shot-label') || ''),
      });
      res.json(rec);
    } catch (e) {
      res.status(400).json({ error: e.message });
    }
  }
);

app.get('/api/scrims/:id/shots/:shotId', async (req, res) => {
  const found = await vods.shotFile(req.params.id, req.params.shotId);
  if (!found) return res.status(404).json({ error: 'No such image.' });
  // On blob storage the image is already served from a CDN, so redirect rather
  // than pull the bytes through a function invocation to hand them on unchanged.
  if (!found.file) return res.redirect(302, found.url);
  res.type(found.contentType);
  // These never change once written, so let the browser keep them.
  res.set('Cache-Control', 'private, max-age=31536000, immutable');
  fs.createReadStream(found.file).pipe(res);
});

app.delete('/api/scrims/:id/shots/:shotId', auth.requireAuth, async (req, res) => {
  res.json({ removed: await vods.deleteShot(req.params.id, req.params.shotId) });
});

/* --------------------------------------------------------- youtube VODs */

app.get('/api/youtube', async (_req, res) => {
  res.json({ videos: await vods.readYouTube() });
});

app.post('/api/youtube', auth.requireAuth, async (req, res) => {
  try {
    res.json(await vods.addYouTube(req.body || {}));
  } catch (e) {
    res.status(400).json({ error: e.message });
  }
});

app.delete('/api/youtube/:id', auth.requireAuth, async (req, res) => {
  res.json({ removed: await vods.removeYouTube(req.params.id) });
});

/* -------------------------------------------------------------- comments */

app.get('/api/match/:matchId/comments', async (req, res) => {
  res.json({ comments: await vods.readComments(req.params.matchId) });
});

app.post('/api/match/:matchId/comments', auth.requireAuth, async (req, res) => {
  try {
    const body = { ...(req.body || {}) };
    // When signed in, authorship comes from the verified identity rather than
    // whatever the client typed. Otherwise "author" is a text field anyone can
    // put a teammate's name in, which makes review notes worthless in an
    // argument about who said what.
    if (req.user) body.author = req.user.name;
    res.json(await vods.addComment(req.params.matchId, body));
  } catch (e) {
    res.status(400).json({ error: e.message });
  }
});

app.delete('/api/match/:matchId/comments/:id', auth.requireAuth, async (req, res) => {
  const removed = await vods.deleteComment(req.params.matchId, req.params.id);
  res.json({ removed });
});

// On Vercel the platform imports this module and does its own routing, so
// binding a port there would be wrong. Locally, nothing imports it and it is
// the server — hence the check rather than two entry points to keep in step.
if (require.main === module) {
  const PORT = process.env.PORT || 8787;
  vods.ensureSchema().catch((e) => console.error('storage not ready:', e.message));

  app.listen(PORT, () => {
    console.log(`DEBRIEF server running at http://localhost:${PORT}`);
    console.log(`Storage: ${vods.storeKind}`);
    if (!process.env.ANTHROPIC_API_KEY) {
      console.warn('WARNING: ANTHROPIC_API_KEY not set — coach reports will fail. Copy .env.example to .env and fill it in.');
    }
  });
}

module.exports = app;
