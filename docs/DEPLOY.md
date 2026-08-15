# Deploying DEBRIEF

The review server keeps its state as **files on a disk**: matches, POV
registrations, team notes, timestamped comments, and scoreboard images all live
under `DEBRIEF_DATA_DIR`. So it needs a host that gives it a persistent disk.

## Why not Vercel

Vercel functions run on a read-only filesystem with an ephemeral `/tmp` that is
not shared between invocations. Deploying there does not fail loudly — the
pages load, you add a scrim, and it is gone minutes later. Scoreboard uploads
fail outright with `EROFS`, and the in-memory sessions sign people out at
random as requests land on different instances.

Everything the app writes:

| Data | Path under `DEBRIEF_DATA_DIR` |
|---|---|
| Matches | `vods/matches.json` |
| Registered POVs | `vods/youtube.json` |
| Team review notes | `vods/notes-<matchId>.json` |
| Timestamped comments | `vods/comments-<vodId>.json` |
| Scoreboards | `vods/shots/<matchId>/*.png` |
| Self-hosted VOD uploads | `vods/<id>/video.mp4` |

Making it Vercel-native means replacing all of that with Postgres/KV plus blob
storage, and swapping sessions for signed cookies. Worth doing if the app ever
needs to scale past one team; not worth it before.

## Before you deploy: turn sign-in on

On a LAN, unauthenticated is the right default. On a public URL it means anyone
who finds the address can post or delete your team's review.

The server **refuses to start** with `NODE_ENV=production` and no
`GOOGLE_CLIENT_ID`, rather than trusting whoever deploys it to remember.

1. Create an OAuth client at
   <https://console.cloud.google.com/apis/credentials> → *Web application*.
2. Add your deployed origin (e.g. `https://debrief.fly.dev`) to **Authorized
   JavaScript origins**.
3. Set `GOOGLE_CLIENT_ID` and `DEBRIEF_ALLOWED_EMAILS` (comma-separated).

Only `openid email profile` are requested — not sensitive scopes, so no Google
verification review and no 7-day token expiry. The server never receives a
Google access or refresh token.

If the deployment is genuinely on a private network and you want no sign-in,
set `DEBRIEF_ALLOW_OPEN=1` deliberately.

## Environment

| Variable | Required | Notes |
|---|---|---|
| `ANTHROPIC_API_KEY` | for coach reports | server-side only, never reaches the browser |
| `GOOGLE_CLIENT_ID` | in production | or `DEBRIEF_ALLOW_OPEN=1` |
| `DEBRIEF_ALLOWED_EMAILS` | with sign-in | empty list denies everyone |
| `DEBRIEF_DATA_DIR` | yes | must point at the mounted volume |
| `DEBRIEF_SECURE_COOKIES` | behind HTTPS | set to `1`; already set in the Dockerfile |
| `PORT` | no | defaults to 8787 |

## Fly.io

`fly.toml` is committed and already declares the volume.

```bash
fly launch --no-deploy          # accept the existing fly.toml
fly volumes create debrief_data --size 3 --region lhr
fly secrets set ANTHROPIC_API_KEY=sk-ant-... \
                GOOGLE_CLIENT_ID=xxxx.apps.googleusercontent.com \
                DEBRIEF_ALLOWED_EMAILS=you@gmail.com,teammate@gmail.com
fly deploy
```

`max_machines_running = 1` is deliberate: state is files on one volume, so a
second instance would serve a diverging copy of the team's review.

## Railway

Connect the repo; it detects the Dockerfile. Then add a **Volume** mounted at
`/data`, and set the variables above. Railway injects its own `PORT`, which the
server already honours.

## Render

New → Web Service → Docker. Add a **Disk** mounted at `/data`, set the
variables, and point the health check at `/api/health`.

## Checking it worked

```bash
curl https://your-app/api/health
```

```json
{ "ok": true, "dataDir": "/data", "auth": true }
```

`ok: false` means the data directory is not writable — almost always a volume
that was not mounted. The container will otherwise look perfectly healthy right
up until the first save fails, which is exactly why the health check writes a
probe file rather than just returning 200.

## Sizing the disk

Matches, notes and comments are JSON measured in kilobytes. Scoreboards are
capped at 12 MB each. The only large consumer is the **self-hosted VOD upload**
path, at roughly 4.8 GB per player per 40-minute match — if the team uses
YouTube links (the default in the UI), 3 GB of disk lasts a very long time.
