// Google Sign-In — identity only.
//
// Scopes are `openid email profile`, which Google does not classify as
// sensitive: no verification review, and no 7-day refresh-token expiry. That is
// the entire reason this is separate from YouTube upload, which needs the
// sensitive `youtube.upload` scope and everything that comes with it.
//
// **We never hold an access or refresh token.** The browser gets an ID token
// from Google, we verify it once to learn who the person is, and then we issue
// our own session. Nothing here can act on anyone's Google account, so a breach
// of this server leaks a list of email addresses rather than the ability to
// upload to someone's channel.
//
// Auth is optional. With no client id configured the server behaves exactly as
// it did before — an unauthenticated LAN tool — because breaking a working
// setup by adding a feature to it would be the wrong trade.

const crypto = require('crypto');
const { OAuth2Client } = require('google-auth-library');

const SESSION_COOKIE = 'debrief_sid';
const SESSION_TTL_MS = 30 * 24 * 60 * 60 * 1000; // 30 days

// In memory on purpose: a restart logs everyone out, which for a self-hosted
// team tool is a fair price for not writing a session file on every request.
const sessions = new Map();

function config() {
  const clientId = process.env.GOOGLE_CLIENT_ID || '';
  // Comma-separated emails allowed to sign in. Without it *any* Google account
  // on earth could sign in, which is not what "team" means — so an empty
  // allowlist denies everyone rather than admitting everyone.
  const allow = (process.env.DEBRIEF_ALLOWED_EMAILS || '')
    .split(',')
    .map((s) => s.trim().toLowerCase())
    .filter(Boolean);
  return {
    clientId,
    allow,
    enabled: !!clientId,
    // Set when served over HTTPS. Left off by default because a LAN box on
    // plain HTTP would otherwise drop the cookie and nobody could log in.
    secureCookies: process.env.DEBRIEF_SECURE_COOKIES === '1',
  };
}

/** Parse one cookie without pulling in a parser dependency. */
function readCookie(req, name) {
  const raw = req.headers.cookie;
  if (!raw) return null;
  for (const part of raw.split(';')) {
    const i = part.indexOf('=');
    if (i < 0) continue;
    if (part.slice(0, i).trim() === name) {
      return decodeURIComponent(part.slice(i + 1).trim());
    }
  }
  return null;
}

function newSession(user) {
  // 32 random bytes is not guessable, so the cookie needs no signing — its
  // value carries no claims, only a lookup key.
  const sid = crypto.randomBytes(32).toString('base64url');
  sessions.set(sid, { user, expiresAt: Date.now() + SESSION_TTL_MS });
  return sid;
}

function getSession(req) {
  const sid = readCookie(req, SESSION_COOKIE);
  if (!sid) return null;
  const s = sessions.get(sid);
  if (!s) return null;
  if (s.expiresAt < Date.now()) {
    sessions.delete(sid);
    return null;
  }
  return { sid, ...s };
}

function setSessionCookie(res, sid, cfg) {
  const bits = [
    `${SESSION_COOKIE}=${encodeURIComponent(sid)}`,
    'Path=/',
    'HttpOnly',            // JavaScript cannot read it, so XSS cannot steal it
    'SameSite=Lax',        // a cross-site POST cannot ride the session
    `Max-Age=${Math.floor(SESSION_TTL_MS / 1000)}`,
  ];
  if (cfg.secureCookies) bits.push('Secure');
  res.setHeader('Set-Cookie', bits.join('; '));
}

function clearSessionCookie(res, cfg) {
  const bits = [`${SESSION_COOKIE}=`, 'Path=/', 'HttpOnly', 'SameSite=Lax', 'Max-Age=0'];
  if (cfg.secureCookies) bits.push('Secure');
  res.setHeader('Set-Cookie', bits.join('; '));
}

/**
 * Verify a Google ID token and return the identity inside it.
 *
 * `verifyIdToken` checks the RSA signature against Google's published keys, the
 * issuer, the audience, and expiry. Decoding the JWT payload without this would
 * accept anything anyone typed — the token is attacker-supplied input, and its
 * contents mean nothing until the signature says they do.
 */
async function verifyGoogleCredential(credential, cfg) {
  const client = new OAuth2Client(cfg.clientId);
  const ticket = await client.verifyIdToken({
    idToken: credential,
    audience: cfg.clientId,
  });
  const p = ticket.getPayload();
  if (!p || !p.email) throw new Error('Token carried no email.');
  // Google says whether it has verified the address itself. An unverified one
  // proves nothing about who is holding it.
  if (!p.email_verified) throw new Error('That Google account has an unverified email.');
  return {
    sub: p.sub,
    email: p.email.toLowerCase(),
    name: p.name || p.email,
    picture: p.picture || null,
  };
}

function isAllowed(email, cfg) {
  return cfg.allow.includes(email.toLowerCase());
}

/**
 * Gate for anything that writes.
 *
 * When auth is not configured this is a no-op, so an existing LAN setup keeps
 * working untouched. Reads stay open either way: the point of the allowlist is
 * to control who can *change* things, and a team that can reach the server can
 * already watch the VODs.
 */
function requireAuth(req, res, next) {
  const cfg = config();
  if (!cfg.enabled) {
    req.user = null;
    return next();
  }
  const s = getSession(req);
  if (!s) return res.status(401).json({ error: 'Sign in to do that.' });
  req.user = s.user;
  next();
}

function install(app) {
  // POST /api/auth/google  { credential }
  app.post('/api/auth/google', async (req, res) => {
    const cfg = config();
    if (!cfg.enabled) {
      return res.status(400).json({ error: 'Sign-in is not configured on this server.' });
    }
    try {
      const user = await verifyGoogleCredential((req.body || {}).credential, cfg);
      if (!isAllowed(user.email, cfg)) {
        // Deliberately says which address was rejected: the usual reason is
        // signing in with the wrong Google account, and hiding that would just
        // make it unfixable.
        return res
          .status(403)
          .json({ error: `${user.email} is not on this team's allowlist.` });
      }
      setSessionCookie(res, newSession(user), cfg);
      res.json({ user });
    } catch (e) {
      res.status(401).json({ error: 'Could not verify that sign-in: ' + e.message });
    }
  });

  app.post('/api/auth/logout', (req, res) => {
    const s = getSession(req);
    if (s) sessions.delete(s.sid);
    clearSessionCookie(res, config());
    res.json({ ok: true });
  });

  // Lets the page know whether to show a sign-in button, and as whom.
  app.get('/api/auth/me', (req, res) => {
    const cfg = config();
    const s = getSession(req);
    res.json({
      enabled: cfg.enabled,
      clientId: cfg.clientId || null,
      user: s ? s.user : null,
    });
  });
}

module.exports = { install, requireAuth, config, verifyGoogleCredential, isAllowed };
