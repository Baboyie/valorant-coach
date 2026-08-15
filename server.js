require('dotenv').config();
const express = require('express');
const path = require('path');
const fs = require('fs');
const fsp = require('fs/promises');
const vods = require('./vods');

const app = express();
app.use(express.json({ limit: '256kb' }));
app.use(express.static(path.join(__dirname, 'public')));

// Where uploaded POVs live. Configurable because the whole point of
// self-hosting is putting 24 GB per match on a drive that has room for it.
const DATA_DIR = process.env.DEBRIEF_DATA_DIR || path.join(__dirname, 'data');

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

/* ------------------------------------------------------------------ VODs */

// POST /api/vod  — upload one POV.
// Headers: X-Vod-Meta = URL-encoded JSON sidecar. Body = raw mp4 bytes.
//
// Streamed straight to disk rather than buffered: these are 40 MB clips and
// multi-gigabyte match recordings, and holding one in memory would put the
// server's footprint at the mercy of the largest thing anyone records.
app.post('/api/vod', async (req, res) => {
  let meta;
  try {
    meta = JSON.parse(decodeURIComponent(req.get('x-vod-meta') || ''));
  } catch {
    return res.status(400).json({ error: 'Missing or invalid X-Vod-Meta header.' });
  }
  if (!Number.isFinite(meta.started_epoch_ms) || !Number.isFinite(meta.duration_secs)) {
    // Without an absolute start time a POV cannot be aligned with anyone
    // else's, which makes it useless for the only thing this server is for.
    return res
      .status(400)
      .json({ error: 'Sidecar needs started_epoch_ms and duration_secs.' });
  }

  const id = vods.newId();
  const dir = path.join(vods.vodRoot(DATA_DIR), id);
  await fsp.mkdir(dir, { recursive: true });
  await fsp.writeFile(path.join(dir, 'meta.json'), JSON.stringify(meta, null, 2));

  const target = path.join(dir, 'video.mp4');
  const out = fs.createWriteStream(target);
  req.pipe(out);

  out.on('finish', () => res.json({ id, bytes: out.bytesWritten }));
  out.on('error', (e) => {
    // Leave meta.json behind: the listing marks the VOD incomplete, which is
    // how a teammate finds out their upload failed rather than silently
    // missing from the review.
    res.status(500).json({ error: 'Write failed: ' + e.message });
  });
});

// GET /api/matches — every POV, grouped into matches by overlapping time.
app.get('/api/matches', async (_req, res) => {
  try {
    const all = await vods.listVods(DATA_DIR);
    res.json({ matches: vods.groupIntoMatches(all).reverse() });
  } catch (e) {
    res.status(500).json({ error: e.message });
  }
});

// GET /api/vod/:id/video — stream a POV, with range support so the review
// page can seek without downloading gigabytes first.
app.get('/api/vod/:id/video', async (req, res) => {
  const { id } = req.params;
  if (!vods.safeId(id)) return res.status(400).end();
  const file = path.join(vods.vodRoot(DATA_DIR), id, 'video.mp4');

  let stat;
  try {
    stat = await fsp.stat(file);
  } catch {
    return res.status(404).json({ error: 'No such VOD.' });
  }

  const range = req.headers.range;
  if (!range) {
    res.writeHead(200, { 'Content-Length': stat.size, 'Content-Type': 'video/mp4', 'Accept-Ranges': 'bytes' });
    return fs.createReadStream(file).pipe(res);
  }
  // Range requests are what make scrubbing feel instant; without them a
  // browser refuses to seek past what it has already downloaded.
  const m = /bytes=(\d*)-(\d*)/.exec(range);
  const start = m && m[1] ? parseInt(m[1], 10) : 0;
  const end = m && m[2] ? parseInt(m[2], 10) : stat.size - 1;
  if (start >= stat.size || end >= stat.size || start > end) {
    return res.writeHead(416, { 'Content-Range': `bytes */${stat.size}` }).end();
  }
  res.writeHead(206, {
    'Content-Range': `bytes ${start}-${end}/${stat.size}`,
    'Accept-Ranges': 'bytes',
    'Content-Length': end - start + 1,
    'Content-Type': 'video/mp4',
  });
  fs.createReadStream(file, { start, end }).pipe(res);
});

/* --------------------------------------------------------- youtube VODs */

app.get('/api/youtube', async (_req, res) => {
  res.json({ videos: await vods.readYouTube(DATA_DIR) });
});

app.post('/api/youtube', async (req, res) => {
  try {
    res.json(await vods.addYouTube(DATA_DIR, req.body || {}));
  } catch (e) {
    res.status(400).json({ error: e.message });
  }
});

app.delete('/api/youtube/:id', async (req, res) => {
  res.json({ removed: await vods.removeYouTube(DATA_DIR, req.params.id) });
});

/* -------------------------------------------------------------- comments */

app.get('/api/match/:matchId/comments', async (req, res) => {
  res.json({ comments: await vods.readComments(DATA_DIR, req.params.matchId) });
});

app.post('/api/match/:matchId/comments', async (req, res) => {
  try {
    res.json(await vods.addComment(DATA_DIR, req.params.matchId, req.body || {}));
  } catch (e) {
    res.status(400).json({ error: e.message });
  }
});

app.delete('/api/match/:matchId/comments/:id', async (req, res) => {
  const removed = await vods.deleteComment(DATA_DIR, req.params.matchId, req.params.id);
  res.json({ removed });
});

const PORT = process.env.PORT || 8787;
vods.ensureDirs(DATA_DIR).catch((e) => console.error('could not create data dir:', e.message));

app.listen(PORT, () => {
  console.log(`DEBRIEF server running at http://localhost:${PORT}`);
  console.log(`VOD storage: ${vods.vodRoot(DATA_DIR)}`);
  if (!process.env.ANTHROPIC_API_KEY) {
    console.warn('WARNING: ANTHROPIC_API_KEY not set — coach reports will fail. Copy .env.example to .env and fill it in.');
  }
});
