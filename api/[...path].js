// Vercel's entry point for every /api/* request.
//
// A catch-all filename rather than a rewrite in vercel.json: the function then
// receives the original path in `req.url`, which is what Express needs to route
// on. With a rewrite, whether the original path survives is a platform detail
// this would quietly depend on.
//
// Everything else — the pages, the CSS, the map data — is served straight from
// `public/` by the CDN and never reaches Node at all.
module.exports = require('../server.js');
