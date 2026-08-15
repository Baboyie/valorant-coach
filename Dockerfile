# DEBRIEF review server.
#
# Deliberately plain: this app's state is files on a disk, so it wants a host
# that gives it a disk. That rules out serverless platforms whose filesystem is
# read-only and whose /tmp does not survive between requests — every match,
# note, comment and scoreboard would silently vanish. See docs/DEPLOY.md.

FROM node:22-alpine

# dumb-init so SIGTERM reaches node rather than being swallowed by PID 1;
# without it a redeploy kills the process mid-write to a JSON file.
RUN apk add --no-cache dumb-init

WORKDIR /app

# Dependencies first so a code change does not re-install them.
COPY package*.json ./
RUN npm ci --omit=dev

COPY . .

# Where the volume gets mounted. Overridden by DEBRIEF_DATA_DIR if the host
# prefers a different path.
ENV DEBRIEF_DATA_DIR=/data
ENV NODE_ENV=production
ENV PORT=8787
EXPOSE 8787

# Behind a proxy that terminates TLS, so the session cookie must carry Secure.
ENV DEBRIEF_SECURE_COOKIES=1

RUN addgroup -S debrief && adduser -S debrief -G debrief \
 && mkdir -p /data && chown -R debrief:debrief /data /app
USER debrief

HEALTHCHECK --interval=30s --timeout=4s --start-period=5s --retries=3 \
  CMD node -e "fetch('http://127.0.0.1:'+(process.env.PORT||8787)+'/api/health').then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))"

ENTRYPOINT ["dumb-init", "--"]
CMD ["node", "server.js"]
