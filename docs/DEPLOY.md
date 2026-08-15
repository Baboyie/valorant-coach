# Deploying DEBRIEF

The review site runs on Vercel's free tier, with a free Neon database and free
Vercel Blob storage. Nothing here costs money at a team's scale, and the reason
is worth stating plainly: **DEBRIEF never stores video.**

Full recordings go to YouTube and the database keeps the link. Clips stay on the
machine that made them. What's left — matches, POVs, team notes, timestamped
comments — is text, and a season of it for a five-stack is measured in
**kilobytes**. Scoreboard screenshots are the only binary, at roughly 10 KB
each after the browser re-encodes them.

| | Free tier | What a team actually uses |
|---|---|---|
| Neon Postgres | 0.5 GB storage, 100 CU-hours/mo per project | well under 1 MB |
| Vercel Blob | free on Hobby, within its published limits | a few MB of scoreboards |
| Vercel Functions | free on Hobby | five people, a few evenings a week |

Neon's free compute suspends after five minutes idle, so the first request after
a quiet afternoon takes about half a second longer while it wakes. At 0.25 CU
that allowance is several hundred hours of *active* database time a month, which
a review session does not come close to touching.

## Two storage backends

The server picks one at startup, with no flag to set:

- **No `POSTGRES_URL`** → files under `DEBRIEF_DATA_DIR`. The default, and the
  right one for running it on your own PC. No accounts, no network.
- **`POSTGRES_URL` set** → Postgres, with scoreboards in Vercel Blob. The only
  thing that works on a serverless host, where the filesystem is read-only and
  `/tmp` is not shared between requests.

Both are in `store-fs.js` and `store-pg.js` behind the same functions, so
nothing above them knows which is running. `/api/health` reports it.

---

## Deploying to Vercel

### 1. Push to GitHub

```bash
git push -u origin main
```

### 2. Create the database

In the Vercel dashboard: **Storage → Create Database → Neon Postgres**. Connect
it to the project. Vercel injects `POSTGRES_URL` itself — that variable is the
switch that moves the app onto the database, so nothing else needs setting.

Tables are created on first use. There is no migration step to remember.

### 3. Create the blob store

**Storage → Create → Blob**, connected to the same project. That sets
`BLOB_READ_WRITE_TOKEN`.

Without it every page still works and only scoreboard uploads fail — which is a
worse failure than an obvious one, so add it now rather than discovering it
mid-review.

### 4. Turn sign-in on

On a LAN, unauthenticated is the right default. On a public URL it means anyone
who finds the address can post or delete your team's review, so **the server
refuses to start** in production without it.

1. Create an OAuth client at
   <https://console.cloud.google.com/apis/credentials> → *Web application*.
2. Add your deployed origin (`https://debrief-yourteam.vercel.app`) to
   **Authorized JavaScript origins**.
3. Set the variables below.

Only `openid email profile` are requested. Google does not treat those as
sensitive, so there is no verification review and no 7-day token expiry, and
this server never receives a Google access or refresh token — a breach here
leaks a list of email addresses, not the ability to post to anyone's channel.

### 5. Set the environment variables

Project → **Settings → Environment Variables**:

| Variable | Required | Notes |
|---|---|---|
| `GOOGLE_CLIENT_ID` | yes | or `DEBRIEF_ALLOW_OPEN=1` to deliberately run open |
| `DEBRIEF_ALLOWED_EMAILS` | yes | comma-separated; an empty list denies everyone |
| `DEBRIEF_SESSION_SECRET` | yes | see below |
| `DEBRIEF_SECURE_COOKIES` | yes | `1` — Vercel is HTTPS |
| `ANTHROPIC_API_KEY` | for coach reports | server-side only, never reaches the browser |
| `POSTGRES_URL` | set for you | by the Neon integration |
| `BLOB_READ_WRITE_TOKEN` | set for you | by the Blob integration |

```bash
node -e "console.log(require('crypto').randomBytes(32).toString('base64url'))"
```

`DEBRIEF_SESSION_SECRET` signs the session cookie. It has to be fixed and shared
across instances: serverless requests land wherever, and if each instance
invented its own secret it would reject cookies minted by the others, signing
people out at unpredictable moments. The server refuses to start without it
rather than let that look like flakiness.

### 6. Deploy and check

```bash
curl https://your-app.vercel.app/api/health
```

```json
{ "ok": true, "store": "postgres", "auth": true }
```

The check runs a real query rather than returning 200, because a bad connection
string produces a site that looks perfect until the first save.

## How it is wired

A rewrite in `vercel.json` sends `/api/:path*` to `api/index.js`, which hands
the request to the same Express app that runs locally. Everything else — pages,
CSS, map data — is served straight from `public/` by the CDN and never reaches
Node.

`server.js` only calls `listen()` when it is the entry point, so the same file
is a normal server locally and a module in production.

**Do not replace the rewrite with an `api/[...path].js` catch-all.** That was
the first attempt and it shipped broken: Vercel matched it as a *single*
segment, so `/api/scrims` worked while `/api/auth/me`, `/api/scrims/:id/notes`,
`/api/scrims/:id/shots` and `/api/match/:id/comments` all 404'd at the router,
before any application code ran — so nothing appeared in the logs. Sign-in,
team notes, scoreboards and comments were dead on a deployment that looked
healthy. `test/api-entry.test.js` drives the entry point in the shape Vercel
produces after the rewrite, which is the only way this gets caught before a
deploy.

## What does not run on Vercel

The self-hosted VOD upload — `POST /api/vod`, range-request streaming, and
grouping POVs by overlapping timestamps — has been **removed**. It needed
multi-gigabyte writes to local disk, which serverless cannot do at all, and the
team uses YouTube links instead. Leaving it in would have meant shipping a
route that fails in a way nobody could diagnose.

Request bodies also cap out around 4.5 MB, below the 12 MB scoreboard limit. The
review page re-encodes anything larger to WebP before uploading, so a 4K
screenshot still works — it just arrives smaller.

## Self-hosting instead

If you would rather keep everything on a machine you own, the filesystem backend
is unchanged and needs no database at all:

```bash
npm start
```

`Dockerfile` and `fly.toml` are still here for a volume-backed host. Mount a
volume, point `DEBRIEF_DATA_DIR` at it, and set the same sign-in variables. One
instance only — the filesystem backend keeps state on one disk, so a second
instance would serve a diverging copy of the team's review.

## Tests

```bash
npm test
```

Covers the things that fail quietly: forged and expired session cookies, ids
that try to escape their directory, YouTube links in every form people paste,
and a full round-trip through the storage layer.
