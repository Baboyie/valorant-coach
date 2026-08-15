// VOD review: browse matches, open one, watch a teammate's POV, comment.
//
// Two views on one page rather than two pages, because switching between "which
// scrim" and "whose POV" is the main thing a team does in a review and a
// navigation round trip each time would be felt.

const state = {
  matches: [],
  unfiled: [],
  maps: [],
  match: null,   // the open match
  vod: null,     // the POV being watched
  player: null,
  ready: false,
  auth: { enabled: false, user: null, clientId: null },
  demoUser: null, // stands in for a signed-in user until sign-in is configured
};

const $ = (id) => document.getElementById(id);

/* ---------------------------------------------------------------- utils */

function fmt(secs) {
  const s = Math.max(0, Math.floor(secs));
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), ss = s % 60;
  return h ? `${h}:${String(m).padStart(2, '0')}:${String(ss).padStart(2, '0')}`
           : `${m}:${String(ss).padStart(2, '0')}`;
}

/** "Sat 15 Aug" — enough to tell an evening's scrims apart at a glance. */
function prettyDate(iso) {
  if (!iso) return '';
  const d = new Date(iso + 'T00:00:00');
  if (isNaN(d)) return iso;
  return d.toLocaleDateString(undefined, { weekday: 'short', day: 'numeric', month: 'short' });
}

async function api(path, opts) {
  const res = await fetch(path, { credentials: 'same-origin', ...(opts || {}) });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data.error || `HTTP ${res.status}`);
  return data;
}

/** YouTube serves thumbnails without an API key, so POV tiles cost nothing. */
const thumb = (videoId) => `https://i.ytimg.com/vi/${videoId}/mqdefault.jpg`;

/* ----------------------------------------------------------------- auth */

async function loadAuth() {
  try { state.auth = await api('/api/auth/me'); } catch { /* stays disabled */ }
  renderAuth();
  if (state.auth.enabled && !state.auth.user) mountRealSignIn();
}

function currentUser() {
  return state.auth.user || state.demoUser;
}

function renderAuth() {
  const u = currentUser();
  const real = state.auth.enabled;

  // Real Google button only when the server has a client id; otherwise the
  // demo button, so the sign-in and profile flow can be looked at before any
  // Google Cloud project exists.
  $('signinSlot').classList.toggle('hidden', !real || !!u);
  $('demoSignIn').classList.toggle('hidden', real || !!u);
  $('profileBtn').classList.toggle('hidden', !u);

  if (u) {
    $('myname').textContent = u.name;
    $('avatar').src = u.picture || '';
    $('pAvatar').src = u.picture || '';
    $('pName').textContent = u.name;
    $('pEmail').textContent = u.email;
    $('pRole').textContent = u.role || 'player';
    $('pVods').textContent = state.matches.reduce((n, m) => n + m.vods.filter((v) => v.player === u.name).length, 0);
    $('pComments').textContent = u.demo ? '—' : '—';
    $('pScrims').textContent = state.matches.length;
    $('pDemoNote').classList.toggle('hidden', !u.demo);
    if (u.demo) {
      $('pDemoNote').textContent =
        'Demo profile. Set GOOGLE_CLIENT_ID and DEBRIEF_ALLOWED_EMAILS on the server to switch this to real Google sign-in — the code path is already built.';
    }
  }

  // With verified identity the author field is not editable: a free-text name
  // is worth nothing in an argument about who said what.
  const verified = !!state.auth.user;
  for (const [free, fixed] of [['author', 'authorFixed'], ['noteAuthor', 'noteAuthorFixed']]) {
    $(free).classList.toggle('hidden', verified);
    $(fixed).classList.toggle('hidden', !verified);
    if (verified) $(fixed).textContent = state.auth.user.name;
  }
}

function mountRealSignIn() {
  const s = document.createElement('script');
  s.src = 'https://accounts.google.com/gsi/client';
  s.async = true;
  s.onload = () => {
    google.accounts.id.initialize({
      client_id: state.auth.clientId,
      callback: async (resp) => {
        try {
          await api('/api/auth/google', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ credential: resp.credential }),
          });
          await loadAuth();
        } catch (e) { alert(e.message); }
      },
    });
    google.accounts.id.renderButton($('signinSlot'), { theme: 'filled_black', size: 'medium', text: 'signin' });
  };
  document.head.appendChild(s);
}

/* ----------------------------------------------------------------- maps */

// Same source the planner uses, so the two pages agree on names and art.
async function loadMaps() {
  try {
    const r = await fetch('https://valorant-api.com/v1/maps');
    const j = await r.json();
    state.maps = (j.data || [])
      .filter((m) => m.displayName && m.splash && m.displayName !== 'The Range')
      .sort((a, b) => a.displayName.localeCompare(b.displayName));
  } catch {
    state.maps = [];
  }
  const sel = $('fMap');
  sel.innerHTML = '';
  for (const m of state.maps) {
    const o = document.createElement('option');
    o.value = m.displayName;
    o.textContent = m.displayName;
    sel.appendChild(o);
  }
}

const splashFor = (name) => (state.maps.find((m) => m.displayName === name) || {}).splash || '';

/* -------------------------------------------------------------- matches */

async function loadMatches() {
  const d = await api('/api/scrims');
  state.matches = d.matches;
  state.unfiled = d.unfiled;
  renderMatches();
}

function renderMatches() {
  const grid = $('matchGrid');
  grid.innerHTML = '';
  $('noMatches').classList.toggle('hidden', state.matches.length > 0);

  state.matches.forEach((m, idx) => {
    const card = document.createElement('button');
    card.className = 'card';

    const art = document.createElement('div');
    art.className = 'art';
    if (m.mapSplash) {
      const img = document.createElement('img');
      img.src = m.mapSplash;
      img.alt = m.map;
      // The first row is always on screen, so lazy-loading it only delays the
      // thing the page is for. Everything below stays lazy.
      img.loading = idx < 4 ? 'eager' : 'lazy';
      art.appendChild(img);
    }

    const kind = document.createElement('span');
    kind.className = 'kind';
    kind.textContent = m.kind;

    const over = document.createElement('div');
    over.className = 'over';
    const map = document.createElement('div');
    map.className = 'map';
    map.textContent = m.map || 'Unknown map';
    const lab = document.createElement('div');
    lab.className = 'lab';
    lab.textContent = m.label || '';
    over.append(map, lab);
    art.append(kind, over);

    const foot = document.createElement('div');
    foot.className = 'foot';
    const left = document.createElement('span');
    left.textContent = [prettyDate(m.playedOn), m.score].filter(Boolean).join(' · ');
    const right = document.createElement('span');
    right.className = 'count';
    right.textContent = `${m.vods.length} POV${m.vods.length === 1 ? '' : 's'}`;
    foot.append(left, right);

    card.append(art, foot);
    card.addEventListener('click', () => openMatch(m.id));
    grid.appendChild(card);
  });

  $('unfiledWrap').classList.toggle('hidden', state.unfiled.length === 0);
  const ul = $('unfiled');
  ul.innerHTML = '';
  for (const v of state.unfiled) {
    const li = document.createElement('li');
    li.textContent = `${v.player || 'unknown'} — ${v.label || v.videoId}`;
    ul.appendChild(li);
  }
}

function openMatch(id) {
  const m = state.matches.find((x) => x.id === id);
  if (!m) return;
  state.match = m;
  state.vod = null;

  $('viewMatches').classList.add('hidden');
  $('viewMatch').classList.remove('hidden');
  $('stage').classList.add('hidden');

  $('mHero').src = m.mapSplash || '';
  $('mKind').textContent = m.kind;
  $('mTitle').textContent = m.label || m.map || 'Match';
  $('mSub').textContent = [m.map, prettyDate(m.playedOn), m.score].filter(Boolean).join(' · ');

  renderPovs();
  loadNotes();
  loadShots();
}

function renderPovs() {
  const grid = $('povGrid');
  grid.innerHTML = '';
  const list = state.match.vods;
  $('noPovs').classList.toggle('hidden', list.length > 0);

  for (const v of list) {
    const b = document.createElement('button');
    b.className = 'pov' + (state.vod && state.vod.id === v.id ? ' active' : '');

    const th = document.createElement('div');
    th.className = 'thumb';
    const img = document.createElement('img');
    img.src = thumb(v.videoId);
    img.alt = '';
    img.loading = 'lazy';
    const play = document.createElement('span');
    play.className = 'play';
    play.textContent = '▶';
    th.append(img, play);

    const who = document.createElement('div');
    who.className = 'who';
    who.textContent = v.player || 'unknown POV';

    b.append(th, who);
    b.addEventListener('click', () => playVod(v));
    grid.appendChild(b);
  }
}

/* ---------------------------------------------------------- team notes */

/** "just now", "14m ago", "3d ago" — a discussion reads better in relative time. */
function ago(iso) {
  const s = Math.max(0, (Date.now() - new Date(iso).getTime()) / 1000);
  if (s < 60) return 'just now';
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  if (s < 604800) return `${Math.floor(s / 86400)}d ago`;
  return new Date(iso).toLocaleDateString();
}

/** Distinct colour per author, so a thread is scannable without reading names. */
function hueFor(name) {
  let h = 0;
  for (let i = 0; i < name.length; i++) h = (h * 31 + name.charCodeAt(i)) % 360;
  return h;
}

async function loadNotes() {
  if (!state.match) return;
  const { notes } = await api(`/api/scrims/${state.match.id}/notes`);
  const ul = $('noteList');
  ul.innerHTML = '';
  $('noNotes').classList.toggle('hidden', notes.length > 0);
  $('noteCount').textContent = notes.length
    ? `${notes.length} take${notes.length === 1 ? '' : 's'} from ${new Set(notes.map((n) => n.author)).size} player${new Set(notes.map((n) => n.author)).size === 1 ? '' : 's'}`
    : '';

  for (const n of notes) {
    const li = document.createElement('li');

    const av = document.createElement('span');
    av.className = 'noteav';
    av.style.background = `hsl(${hueFor(n.author)} 45% 26%)`;
    av.style.color = `hsl(${hueFor(n.author)} 70% 72%)`;
    av.textContent = (n.author || '?').trim().charAt(0).toUpperCase();

    const main = document.createElement('div');
    main.className = 'notemain';

    const head = document.createElement('div');
    head.className = 'head';
    const who = document.createElement('strong');
    who.textContent = n.author;
    const when = document.createElement('span');
    when.className = 'muted small';
    when.textContent = ago(n.createdUtc);
    when.title = new Date(n.createdUtc).toLocaleString();
    const rm = document.createElement('button');
    rm.className = 'rm';
    rm.textContent = 'remove';
    rm.addEventListener('click', async () => {
      await api(`/api/scrims/${state.match.id}/notes/${n.id}`, { method: 'DELETE' });
      loadNotes();
    });
    head.append(who, when, rm);

    const body = document.createElement('div');
    body.className = 'notebody';
    body.textContent = n.body;

    main.append(head, body);
    li.append(av, main);
    ul.appendChild(li);
  }
}

async function postNote() {
  const body = $('noteBody').value.trim();
  if (!body || !state.match) return;
  const u = currentUser();
  const author = state.auth.user
    ? state.auth.user.name
    : ($('noteAuthor').value.trim() || (u && u.name) || 'anonymous');
  $('noteErr').classList.add('hidden');
  try {
    await api(`/api/scrims/${state.match.id}/notes`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ author, body }),
    });
    $('noteBody').value = '';
    localStorage.setItem('debrief.author', author);
    loadNotes();
  } catch (e) {
    $('noteErr').textContent = e.message;
    $('noteErr').classList.remove('hidden');
  }
}

/* ---------------------------------------------------------- scoreboards */

async function loadShots() {
  if (!state.match) return;
  const { shots } = await api(`/api/scrims/${state.match.id}/shots`);
  const grid = $('shotGrid');
  grid.innerHTML = '';
  $('noShots').classList.toggle('hidden', shots.length > 0);

  for (const s of shots) {
    const src = `/api/scrims/${state.match.id}/shots/${s.id}`;
    const fig = document.createElement('figure');
    fig.className = 'shot';

    const img = document.createElement('img');
    img.src = src;
    img.alt = s.label || 'scoreboard';
    img.addEventListener('click', () => openLightbox(src));

    const rm = document.createElement('button');
    rm.className = 'rm';
    rm.textContent = '×';
    rm.title = 'Remove';
    rm.addEventListener('click', async (e) => {
      e.stopPropagation();
      await api(`/api/scrims/${state.match.id}/shots/${s.id}`, { method: 'DELETE' });
      loadShots();
    });

    fig.append(img, rm);
    grid.appendChild(fig);
  }
}

/**
 * Upload a Blob or File as-is.
 *
 * Posted as raw bytes with the image's own content type rather than multipart,
 * so a clipboard Blob goes straight up with nothing in between — which is the
 * point, because a scoreboard arrives via Win+Shift+S and Ctrl+V, not via a
 * file that was ever saved to disk.
 */
async function uploadShot(blob, label) {
  if (!state.match) return;
  $('shotErr').classList.add('hidden');
  try {
    await api(`/api/scrims/${state.match.id}/shots`, {
      method: 'POST',
      headers: {
        'Content-Type': blob.type || 'image/png',
        'X-Shot-Label': encodeURIComponent(label || ''),
      },
      body: blob,
    });
    loadShots();
  } catch (e) {
    $('shotErr').textContent = e.message;
    $('shotErr').classList.remove('hidden');
  }
}

function openLightbox(src) {
  $('lbImg').src = src;
  $('lightbox').classList.remove('hidden');
}

// Paste anywhere while a match is open. Scoped to the match view so a paste
// into the comment box is not hijacked into an upload.
document.addEventListener('paste', (e) => {
  if (!state.match || $('viewMatch').classList.contains('hidden')) return;
  const items = [...(e.clipboardData?.items || [])];
  const img = items.find((i) => i.type.startsWith('image/'));
  if (!img) return;
  e.preventDefault();
  uploadShot(img.getAsFile(), 'pasted');
});

/* --------------------------------------------------------------- player */

let apiReady = new Promise((r) => { window.onYouTubeIframeAPIReady = r; });
(function () {
  const s = document.createElement('script');
  s.src = 'https://www.youtube.com/iframe_api';
  document.head.appendChild(s);
})();

async function playVod(v) {
  state.vod = v;
  $('stage').classList.remove('hidden');
  $('npWho').textContent = v.player || 'unknown POV';
  $('npLabel').textContent = state.match.label || '';
  renderPovs();

  await apiReady;
  if (state.player) state.player.loadVideoById(v.videoId);
  else {
    state.player = new YT.Player('player', {
      videoId: v.videoId,
      playerVars: { rel: 0, modestbranding: 1 },
      events: { onReady: () => { state.ready = true; tick(); } },
    });
  }
  loadComments();
}

const now = () => (state.ready && state.player ? state.player.getCurrentTime() || 0 : 0);

function tick() {
  if (state.vod) $('atTime').textContent = fmt(now());
  requestAnimationFrame(tick);
}

function seek(s) {
  if (state.player && state.ready) { state.player.seekTo(s, true); state.player.playVideo(); }
}

/* ------------------------------------------------------------- comments */

async function loadComments() {
  if (!state.vod) return;
  const { comments } = await api(`/api/match/${state.vod.id}/comments`);
  const ul = $('comments');
  ul.innerHTML = '';
  $('noComments').classList.toggle('hidden', comments.length > 0);

  for (const c of comments) {
    const li = document.createElement('li');
    const head = document.createElement('div');
    head.className = 'head';

    const ts = document.createElement('button');
    ts.className = 'ts';
    ts.textContent = fmt(c.atMs / 1000);
    ts.title = 'Jump here';
    ts.addEventListener('click', () => seek(c.atMs / 1000));

    const who = document.createElement('span');
    who.className = 'muted small';
    who.textContent = c.author;

    const rm = document.createElement('button');
    rm.className = 'rm';
    rm.textContent = 'remove';
    rm.addEventListener('click', async () => {
      await api(`/api/match/${state.vod.id}/comments/${c.id}`, { method: 'DELETE' });
      loadComments();
    });

    head.append(ts, who, rm);
    const txt = document.createElement('div');
    txt.textContent = c.body;
    li.append(head, txt);
    ul.appendChild(li);
  }
}

async function postComment() {
  const body = $('body').value.trim();
  if (!body || !state.vod) return;
  const u = currentUser();
  const author = state.auth.user ? state.auth.user.name : ($('author').value.trim() || (u && u.name) || 'anonymous');
  // Timed at the moment of posting: you watch, you see it, you type. Using the
  // position from when typing started would pin every note early.
  await api(`/api/match/${state.vod.id}/comments`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ atMs: Math.round(now() * 1000), author, body }),
  });
  $('body').value = '';
  localStorage.setItem('debrief.author', author);
  loadComments();
}

/* -------------------------------------------------------------- wiring */

$('newMatch').addEventListener('click', () => {
  $('fDate').value = new Date().toISOString().slice(0, 10);
  $('fLabel').value = '';
  $('fScore').value = '';
  $('matchErr').classList.add('hidden');
  $('matchDlg').showModal();
});

$('saveMatch').addEventListener('click', async (e) => {
  e.preventDefault();
  const map = $('fMap').value;
  try {
    await api('/api/scrims', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        map,
        mapSplash: splashFor(map),
        playedOn: $('fDate').value,
        kind: $('fKind').value,
        label: $('fLabel').value,
        score: $('fScore').value,
      }),
    });
    $('matchDlg').close();
    await loadMatches();
  } catch (err) {
    $('matchErr').textContent = err.message;
    $('matchErr').classList.remove('hidden');
  }
});

$('addPov').addEventListener('click', () => {
  $('fUrl').value = '';
  $('fWho').value = (currentUser() || {}).name || localStorage.getItem('debrief.author') || '';
  $('povErr').classList.add('hidden');
  $('povDlg').showModal();
});

$('savePov').addEventListener('click', async (e) => {
  e.preventDefault();
  try {
    await api('/api/youtube', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        url: $('fUrl').value,
        player: $('fWho').value,
        matchId: state.match.id,
        label: state.match.label,
      }),
    });
    $('povDlg').close();
    await loadMatches();
    openMatch(state.match.id);
  } catch (err) {
    $('povErr').textContent = err.message;
    $('povErr').classList.remove('hidden');
  }
});

$('backToMatches').addEventListener('click', () => {
  $('viewMatch').classList.add('hidden');
  $('viewMatches').classList.remove('hidden');
  state.match = null;
  state.vod = null;
  loadMatches();
});

$('delMatch').addEventListener('click', async () => {
  if (!state.match) return;
  await api(`/api/scrims/${state.match.id}`, { method: 'DELETE' });
  $('backToMatches').click();
});

$('delVod').addEventListener('click', async () => {
  if (!state.vod) return;
  await api(`/api/youtube/${state.vod.id}`, { method: 'DELETE' });
  const id = state.match.id;
  await loadMatches();
  openMatch(id);
});

$('post').addEventListener('click', postComment);
$('body').addEventListener('keydown', (e) => { if (e.key === 'Enter') postComment(); });

// Demo sign-in: shows the profile UI without a Google Cloud project. Replaced
// by the real button the moment GOOGLE_CLIENT_ID is set.
$('demoSignIn').addEventListener('click', () => {
  state.demoUser = {
    name: localStorage.getItem('debrief.author') || 'Baboyie',
    email: 'you@example.com',
    role: 'IGL',
    picture: 'https://api.dicebear.com/7.x/thumbs/svg?seed=debrief',
    demo: true,
  };
  renderAuth();
});

$('postNote').addEventListener('click', postNote);
// Enter posts, Shift+Enter makes a new line — a match take is often a
// paragraph, so the textarea has to allow one.
$('noteBody').addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); postNote(); }
});

$('shotFile').addEventListener('change', (e) => {
  const f = e.target.files && e.target.files[0];
  if (f) uploadShot(f, f.name);
  e.target.value = '';
});

$('lbClose').addEventListener('click', () => $('lightbox').classList.add('hidden'));
$('lightbox').addEventListener('click', (e) => {
  if (e.target.id === 'lightbox') $('lightbox').classList.add('hidden');
});
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') $('lightbox').classList.add('hidden');
});

$('profileBtn').addEventListener('click', () => $('profile').classList.remove('hidden'));
$('profileClose').addEventListener('click', () => $('profile').classList.add('hidden'));
$('profile').addEventListener('click', (e) => { if (e.target.id === 'profile') $('profile').classList.add('hidden'); });

$('signout').addEventListener('click', async () => {
  if (state.auth.user) { await api('/api/auth/logout', { method: 'POST' }); state.auth.user = null; mountRealSignIn(); }
  state.demoUser = null;
  $('profile').classList.add('hidden');
  renderAuth();
});

$('author').value = localStorage.getItem('debrief.author') || '';
loadAuth();
loadMaps().then(loadMatches);
