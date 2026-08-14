require('dotenv').config();
const express = require('express');
const path = require('path');

const app = express();
app.use(express.json({ limit: '256kb' }));
app.use(express.static(path.join(__dirname, 'public')));

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

const PORT = process.env.PORT || 8787;
app.listen(PORT, () => {
  console.log(`DEBRIEF server running at http://localhost:${PORT}`);
  if (!process.env.ANTHROPIC_API_KEY) {
    console.warn('WARNING: ANTHROPIC_API_KEY not set — coach reports will fail. Copy .env.example to .env and fill it in.');
  }
});
