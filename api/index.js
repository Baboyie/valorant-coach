// Vercel's entry point for every /api/* request.
//
// This was a `[...path].js` catch-all first, which looked right and was not:
// Vercel matched it as a *single* segment, so `/api/scrims` reached Express and
// `/api/scrims/:id/notes` never did. Sign-in, notes, comments and scoreboards
// all 404'd at the router, before any of our code ran — which is why nothing in
// the app's own logs showed a problem.
//
// So the path comes from an explicit rewrite in vercel.json instead. It is
// carried in a query parameter rather than trusting `req.url` to survive the
// rewrite: rewrites are supposed to preserve the original URL, but "supposed
// to" is a platform detail to depend on only when the failure would be loud,
// and this one would be silent.

const app = require('../server.js');

const CARRIER = '__debrief_path';

module.exports = (req, res) => {
  const url = new URL(req.url, 'http://placeholder');
  const carried = url.searchParams.get(CARRIER);

  if (carried !== null) {
    // Put the real path back before Express routes on it, keeping any query
    // the caller sent — /api/riot-proxy?url=... depends on it.
    url.searchParams.delete(CARRIER);
    const qs = url.searchParams.toString();
    req.url = `/api/${carried}${qs ? `?${qs}` : ''}`;
  }

  return app(req, res);
};
