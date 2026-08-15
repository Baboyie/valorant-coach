// VOD review: pick a teammate's POV, watch it, comment at timestamps.
//
// Comments are anchored to a position in *this* video. An earlier design tried
// to anchor them to a shared match timeline so one comment landed on the same
// moment in all five POVs, but that needs every recording aligned to the
// millisecond — and the team watches together on Discord anyway, so the
// alignment bought nothing it was worth paying for.

const state = {
  videos: [],
  current: null,
  player: null,
  ready: false,
};

const $ = (id) => document.getElementById(id);

/* ------------------------------------------------------------ utilities */

/** 0:07, 4:31, 1:02:09 — the format people actually write timestamps in. */
function fmt(secs) {
  const s = Math.max(0, Math.floor(secs));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const ss = s % 60;
  return h > 0
    ? `${h}:${String(m).padStart(2, '0')}:${String(ss).padStart(2, '0')}`
    : `${m}:${String(ss).padStart(2, '0')}`;
}

async function api(path, opts) {
  const res = await fetch(path, opts);
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data.error || `HTTP ${res.status}`);
  return data;
}

/* -------------------------------------------------------- youtube player */

// The IFrame API loads asynchronously and calls a global hook, so the player
// cannot be created until it fires.
let apiReady = new Promise((resolve) => {
  window.onYouTubeIframeAPIReady = resolve;
});

(function loadYouTubeApi() {
  const s = document.createElement('script');
  s.src = 'https://www.youtube.com/iframe_api';
  document.head.appendChild(s);
})();

async function mountPlayer(videoId) {
  await apiReady;
  $('noPlayer').classList.add('hidden');

  if (state.player) {
    state.player.loadVideoById(videoId);
    return;
  }
  state.player = new YT.Player('player', {
    videoId,
    playerVars: { rel: 0, modestbranding: 1 },
    events: {
      onReady: () => {
        state.ready = true;
        tickTime();
      },
    },
  });
}

function currentTime() {
  return state.ready && state.player ? state.player.getCurrentTime() || 0 : 0;
}

// The comment bar shows the position a new comment would land at, so posting
// never involves guessing where "here" is.
function tickTime() {
  if (state.current) $('atTime').textContent = fmt(currentTime());
  requestAnimationFrame(tickTime);
}

function seekTo(secs) {
  if (state.player && state.ready) {
    state.player.seekTo(secs, true);
    state.player.playVideo();
  }
}

/* ----------------------------------------------------------------- VODs */

async function loadVideos() {
  const { videos } = await api('/api/youtube');
  state.videos = videos;
  renderList();
}

function renderList() {
  const ul = $('vodList');
  ul.innerHTML = '';
  $('empty').classList.toggle('hidden', state.videos.length > 0);

  for (const v of state.videos) {
    const li = document.createElement('li');
    li.className = state.current && state.current.id === v.id ? 'active' : '';
    const lab = document.createElement('span');
    lab.className = 'lab';
    lab.textContent = v.label || v.title || v.videoId;
    const who = document.createElement('span');
    who.className = 'who';
    who.textContent = v.player || 'unknown POV';
    li.append(lab, who);
    li.addEventListener('click', () => select(v));
    ul.appendChild(li);
  }
}

async function select(v) {
  state.current = v;
  renderList();
  $('nowPlaying').classList.remove('hidden');
  $('commentBar').classList.remove('hidden');
  $('npLabel').textContent = v.label || v.title || v.videoId;
  $('npWho').textContent = v.player ? `— ${v.player}` : '';
  $('hint').textContent = 'Comments are pinned to the timestamp showing on the left. Click any timestamp to jump there.';
  await mountPlayer(v.videoId);
  await loadComments();
}

/* ------------------------------------------------------------- comments */

// Comments live per video. The server keys them by an arbitrary id, so a VOD's
// own id serves as its comment thread.
async function loadComments() {
  if (!state.current) return;
  const { comments } = await api(`/api/match/${state.current.id}/comments`);
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
    ts.addEventListener('click', () => seekTo(c.atMs / 1000));

    const who = document.createElement('span');
    who.className = 'who';
    who.textContent = c.author;

    const rm = document.createElement('button');
    rm.className = 'rm';
    rm.textContent = 'remove';
    rm.addEventListener('click', async () => {
      await api(`/api/match/${state.current.id}/comments/${c.id}`, { method: 'DELETE' });
      loadComments();
    });

    head.append(ts, who, rm);

    const txt = document.createElement('div');
    txt.className = 'txt';
    txt.textContent = c.body;

    li.append(head, txt);
    ul.appendChild(li);
  }
}

async function postComment() {
  const body = $('body').value.trim();
  if (!body || !state.current) return;
  const author = $('author').value.trim() || 'anonymous';
  // Capture the time at the moment of posting, not when typing began — you
  // watch, you see it, you type. Using the earlier position would pin every
  // comment a few seconds before whatever it is about.
  const atMs = Math.round(currentTime() * 1000);
  await api(`/api/match/${state.current.id}/comments`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ atMs, author, body }),
  });
  $('body').value = '';
  localStorage.setItem('debrief.author', author);
  loadComments();
}

/* ------------------------------------------------------------------ wiring */

$('add').addEventListener('click', async () => {
  const url = $('url').value.trim();
  $('addErr').classList.add('hidden');
  if (!url) return;
  try {
    const added = await api('/api/youtube', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        url,
        player: $('who').value.trim(),
        label: $('label').value.trim(),
      }),
    });
    $('url').value = '';
    $('label').value = '';
    await loadVideos();
    select(added);
  } catch (e) {
    $('addErr').textContent = e.message;
    $('addErr').classList.remove('hidden');
  }
});

$('post').addEventListener('click', postComment);
$('body').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') postComment();
});

$('del').addEventListener('click', async () => {
  if (!state.current) return;
  await api(`/api/youtube/${state.current.id}`, { method: 'DELETE' });
  state.current = null;
  $('nowPlaying').classList.add('hidden');
  $('commentBar').classList.add('hidden');
  await loadVideos();
});

$('author').value = localStorage.getItem('debrief.author') || '';
loadVideos();
