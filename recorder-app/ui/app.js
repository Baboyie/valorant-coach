const { invoke } = window.__TAURI__.core;

const el = (id) => document.getElementById(id);
let cfg = null;
let lastClipPath = null;
let prevState = null;
let currentRecordingPath = null;
let mediaItems = [];
let mediaTab = "clip";

/* ---------------------------------------------------------------- config */

async function loadConfig() {
  cfg = await invoke("get_config");
  el("window").value = cfg.window_secs;
  el("fps").value = cfg.fps;
  el("bitrate").value = cfg.bitrate_mbps;
  el("hotkey").value = cfg.save_hotkey;
  el("outdir").value = cfg.output_dir;
  el("capaudio").checked = cfg.capture_audio;
  el("capmic").checked = cfg.capture_mic;
  el("mixaudio").checked = cfg.mix_audio;
  el("notifysound").checked = cfg.notify_sound;
  el("notifytoast").checked = cfg.notify_toast;
  setGainUI("Desktop", cfg.desktop_gain);
  setGainUI("Mic", cfg.mic_gain);
  setHotkeyHint();
  await refreshTargets();
  await refreshDevices();
  syncAudioEnabled();
}

function setHotkeyHint() {
  if (!cfg) return;
  el("hotkeyhint").innerHTML =
    `Press <kbd>${cfg.save_hotkey}</kbd> in game to keep the last ${cfg.window_secs} seconds.`;
}

/* ------------------------------------------------------------------ tabs */

const PANES = { status: "paneStatus", library: "paneLibrary", settings: "paneSettings" };
const NAVS = { status: "navStatus", library: "navLibrary", settings: "navSettings" };

function showTab(name) {
  for (const [key, pane] of Object.entries(PANES)) {
    el(pane).classList.toggle("hidden", key !== name);
    el(NAVS[key]).classList.toggle("active", key === name);
  }
  // The pane is one scroll container shared by all three; a tab switch that
  // kept the previous tab's offset would open Settings halfway down.
  document.querySelector(".pane").scrollTop = 0;
  if (name === "library") loadMedia();
}
for (const [key, nav] of Object.entries(NAVS)) {
  el(nav).addEventListener("click", () => showTab(key));
}

/* ---------------------------------------------------------------- status */

function fmtState(s) {
  // A queued action outranks the idle text: "window is minimised — recording
  // starts when it comes back" is the difference between waiting and broken.
  if (s.pending) return s.pending;
  const auto = !cfg || !cfg.target || cfg.target.kind === "valorant";
  if (s.state === "recording") return "recording";
  if (s.state === "buffering") return "buffering";
  if (s.game_running) return "starting…";
  return auto ? "waiting for Valorant" : "target not available";
}

function targetLabel(s) {
  if (s.target_title) return s.target_title;
  if (!cfg || !cfg.target) return "—";
  if (cfg.target.kind === "valorant") return "Valorant";
  if (cfg.target.kind === "monitor") return "Screen";
  return cfg.target.title || "Window";
}

async function tick() {
  let s;
  try {
    s = await invoke("get_status");
  } catch (e) {
    return;
  }

  // A finished recording or a fresh clip should appear without anyone hunting
  // for a refresh button.
  if (prevState === "recording" && s.state !== "recording") loadMedia();
  prevState = s.state;
  currentRecordingPath = s.recording_path || null;

  el("dot").className = "dot " + s.state;
  el("targetName").textContent = targetLabel(s);

  const badge = el("stateBadge");
  badge.textContent = s.pending ? "waiting" : s.state;
  badge.className = "state-badge " + (s.pending ? "" : s.state);

  // While recording there is no ring, so show elapsed recording time in the
  // same slot rather than a frozen 0.0s that looks like a stall.
  const fill = el("fill");
  if (s.state === "recording") {
    el("buffered").textContent = s.recording_secs.toFixed(1) + "s";
    el("bufCaption").textContent = "recorded to file";
    fill.style.width = "100%";
    fill.classList.add("recording");
  } else {
    el("buffered").textContent = s.buffered_secs.toFixed(1) + "s";
    const target = cfg ? cfg.window_secs : 30;
    el("bufCaption").textContent =
      s.ring_mb > 0 ? `of ${target}s buffered · ${s.ring_mb.toFixed(1)} MB` : fmtState(s);
    fill.style.width = Math.min(100, (s.buffered_secs / target) * 100).toFixed(1) + "%";
    fill.classList.remove("recording");
  }

  // §17: never fake these. The engine sends null until it has a real delta,
  // and an em dash is the honest rendering of "not measured yet".
  const p = s.perf;
  el("cpu").textContent = p ? p.cpu_pct.toFixed(2) + "%" : "—";
  el("ram").textContent = p ? p.ram_mb.toFixed(0) + "MB" : "—";
  el("vram").textContent = p && p.vram_mb != null ? p.vram_mb.toFixed(0) + "MB" : "—";
  el("disk").textContent = p ? p.disk_write_mbps.toFixed(1) + "MB/s" : "—";
  const vd = el("vramDetail");
  if (p && p.vram_mb != null) {
    vd.textContent = `VRAM budget ${p.vram_budget_mb.toFixed(0)} MB on the capture adapter.`;
    vd.classList.remove("hidden");
  } else {
    vd.classList.add("hidden");
  }

  el("kept").textContent = s.frames_kept.toLocaleString();
  el("p99").textContent = s.callback_p99_us ? s.callback_p99_us + " µs" : "—";
  el("dropfull").textContent = s.dropped_ring_full.toLocaleString();
  el("dropresize").textContent = s.dropped_resized.toLocaleString();
  // Green means measured-and-fine, red means a real drop. Grey stays grey
  // until there is something to report, for the same reason as the em dashes.
  el("dotKept").className = "hdot " + (s.frames_kept > 0 ? "ok" : "");
  el("dotP99").className = "hdot " + (s.callback_p99_us ? "ok" : "");
  el("dotFull").className = "hdot " + (s.dropped_ring_full > 0 ? "bad" : s.frames_kept > 0 ? "ok" : "");
  el("dotResize").className = "hdot " + (s.dropped_resized > 0 ? "bad" : s.frames_kept > 0 ? "ok" : "");

  el("clip").disabled = s.state !== "buffering";
  el("record").disabled = !s.game_running && !s.pending;
  const rec = s.state === "recording";
  el("recordLabel").textContent = rec ? "Stop" : "Record";
  el("record").className = rec ? "rec" : "";

  if (s.last_clip) {
    if (lastClipPath && s.last_clip !== lastClipPath) loadMedia();
    lastClipPath = s.last_clip;
    el("lastwrap").classList.remove("hidden");
    el("lastclip").textContent =
      s.last_clip + (s.last_save_ms != null ? `  (saved in ${Math.round(s.last_save_ms)} ms)` : "");
  }

  const err = el("error");
  if (s.last_error) {
    err.textContent = s.last_error;
    err.classList.remove("hidden");
  } else {
    err.classList.add("hidden");
  }
}

el("clip").addEventListener("click", () => invoke("save_clip"));

el("record").addEventListener("click", async () => {
  const s = await invoke("get_status");
  await invoke(s.state === "recording" ? "stop_recording" : "start_recording");
});

el("reveal").addEventListener("click", () => {
  if (lastClipPath) invoke("reveal_in_explorer", { path: lastClipPath });
});

/* --------------------------------------------------------------- targets */

// The picker is rebuilt from a live scan: windows come and go, and a stale
// list is how someone records the wrong thing. The current target stays
// selected even when it is not available right now, so a saved screen that is
// unplugged still shows as chosen rather than silently turning into Valorant.
const targetKey = (t) => JSON.stringify(t);

async function refreshTargets() {
  const sel = el("target");
  const { monitors, windows } = await invoke("list_targets");
  const opts = [
    { t: { kind: "valorant" }, label: "Valorant (auto-detect)" },
    ...monitors.map((m) => ({
      t: { kind: "monitor", device: m.device },
      label: `Screen ${m.index} — ${m.width}×${m.height}${m.primary ? " (primary)" : ""}`,
    })),
    ...windows.map((w) => ({
      t: { kind: "window", title: w.title, class: w.class },
      label: `Window — ${w.title}`,
    })),
  ];
  const current = cfg && cfg.target ? cfg.target : { kind: "valorant" };
  if (!opts.some((o) => targetKey(o.t) === targetKey(current))) {
    const what = current.kind === "monitor" ? current.device : current.title;
    opts.splice(1, 0, { t: current, label: `(not available now) ${what}` });
  }
  sel.innerHTML = "";
  for (const o of opts) {
    const opt = document.createElement("option");
    opt.value = targetKey(o.t);
    opt.textContent = o.label;
    sel.appendChild(opt);
  }
  sel.value = targetKey(current);
}

// Picking a target applies immediately — it is a choice, not a setting, and
// Discord trained everyone to expect the share to start on the click.
el("target").addEventListener("change", async () => {
  const next = { ...cfg, target: JSON.parse(el("target").value) };
  try {
    await invoke("set_config", { newConfig: next });
    cfg = next;
    flash("target applied");
  } catch (e) {
    flash("could not apply: " + e);
  }
});

el("refreshTargets").addEventListener("click", refreshTargets);

/* ----------------------------------------------------------------- audio */

// Sliders are integer percent; the config is a linear multiplier. 100% is
// unity, 200% is +6 dB — enough to rescue a quiet headset mic without being
// enough to turn any source into noise.
const pctToGain = (p) => Number(p) / 100;
const gainToPct = (g) => Math.round((g == null ? 1 : g) * 100);

function setGainUI(which, gain) {
  const pct = gainToPct(gain);
  el("gain" + which).value = pct;
  el("gain" + which + "Val").textContent = pct + "%";
}

async function refreshDevices() {
  let devices = [];
  try {
    devices = await invoke("list_audio_devices");
  } catch {
    devices = [];
  }
  fillDevices("devDesktop", devices.filter((d) => d.kind === "desktop"), cfg && cfg.desktop_device);
  fillDevices("devMic", devices.filter((d) => d.kind === "microphone"), cfg && cfg.mic_device);
}

function fillDevices(id, list, chosen) {
  const sel = el(id);
  sel.innerHTML = "";
  const add = (value, label) => {
    const o = document.createElement("option");
    o.value = value;
    o.textContent = label;
    sel.appendChild(o);
  };
  // Empty means "whatever Windows is using", which is what most people want
  // and what keeps working when they change it in Windows.
  const def = list.find((d) => d.default);
  add("", def ? `Default — ${def.name}` : "Default");
  for (const d of list) add(d.id, d.name);
  // A saved device that is not plugged in right now stays selected and says
  // so, rather than silently reverting to the default and recording the wrong
  // thing the next time it is plugged back in.
  if (chosen && !list.some((d) => d.id === chosen)) {
    add(chosen, "(not connected) saved device");
  }
  sel.value = chosen || "";
}

// Nothing below a track's checkbox means anything if the source is off.
function syncAudioEnabled() {
  const d = el("capaudio").checked;
  const m = el("capmic").checked;
  el("devDesktop").disabled = !d;
  el("gainDesktop").disabled = !d;
  el("devMic").disabled = !m;
  el("gainMic").disabled = !m;
  // Mixing is only meaningful with two things to mix.
  const canMix = d && m;
  el("mixaudio").disabled = !canMix;
  el("mixaudio").parentElement.classList.toggle("muted", !canMix);
}

// Volume is applied live rather than on Save: the engine pushes gain at the
// running captures without rebuilding, so dragging a slider costs nothing and
// waiting for a Save click to hear the result would make it unusable.
let gainTimer = null;
function onGain(which, field) {
  const input = el("gain" + which);
  input.addEventListener("input", () => {
    el("gain" + which + "Val").textContent = input.value + "%";
    if (!cfg) return;
    cfg[field] = pctToGain(input.value);
    clearTimeout(gainTimer);
    // Coalesce a drag into one write; the config file is on disk and the
    // engine restarts nothing, but there is no reason to do it 40 times.
    gainTimer = setTimeout(() => invoke("set_config", { newConfig: cfg }).catch(() => {}), 150);
  });
}
onGain("Desktop", "desktop_gain");
onGain("Mic", "mic_gain");

for (const id of ["capaudio", "capmic"]) {
  el(id).addEventListener("change", syncAudioEnabled);
}
el("refreshDevices").addEventListener("click", refreshDevices);
el("previewToast").addEventListener("click", () => invoke("preview_toast"));

/* --------------------------------------------------------------- gallery */

const fmtDur = (secs) => {
  if (secs == null) return "—";
  const s = Math.round(secs);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
};
const fmtBytes = (b) => (b >= 1e9 ? (b / 1e9).toFixed(2) + " GB" : (b / 1e6).toFixed(1) + " MB");
const fmtWhen = (m) => {
  const ms = m.started_epoch_ms ?? m.modified_epoch_ms;
  const d = new Date(ms);
  const today = new Date();
  const sameDay = d.toDateString() === today.toDateString();
  const time = d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
  // "Today 06:51" beats "Sat, Aug 22, 06:51 AM" in a 210px card, and the recent
  // ones are the ones anyone is looking for.
  if (sameDay) return `Today ${time}`;
  const days = Math.floor((today - d) / 86400000);
  if (days < 7) return `${d.toLocaleDateString(undefined, { weekday: "short" })} ${time}`;
  return d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
};

async function loadMedia() {
  try {
    mediaItems = await invoke("list_media");
  } catch {
    mediaItems = [];
  }
  renderMedia();
}

// Decoded frames, keyed by path. Kept for the session rather than written to
// disk: one frame costs a couple of range requests through the media: scheme,
// and a cache on disk would be another thing to invalidate when a file changes.
const thumbs = new Map();
// One at a time. Decoding several videos at once competes with the encoder for
// the same GPU, and §2 is explicit that nothing here may do that.
let thumbQueue = Promise.resolve();

/**
 * Grab a frame from a video by playing it, invisibly, to one position.
 *
 * Done in the webview rather than in Rust because the decoder is already here
 * and already reaches these files through the media: scheme. A Media Foundation
 * source reader plus an image encoder would be a few hundred lines to arrive at
 * the same JPEG.
 */
function thumbFor(m) {
  if (thumbs.has(m.path)) return Promise.resolve(thumbs.get(m.path));
  const job = () =>
    new Promise((resolve) => {
      const v = document.createElement("video");
      v.muted = true;
      v.preload = "metadata";
      // Without this the canvas is tainted and toDataURL throws: the page runs
      // on tauri.localhost and the video is served from media.localhost, which
      // is a different origin however local both are. The media handler already
      // answers with Access-Control-Allow-Origin, so asking for CORS is all
      // that was missing — and it must be set before src, or the request goes
      // out without it.
      v.crossOrigin = "anonymous";
      let done = false;
      const finish = (val) => {
        if (done) return;
        done = true;
        thumbs.set(m.path, val);
        v.removeAttribute("src");
        v.load();
        resolve(val);
      };
      // A file that will not decode must not hang the queue behind it.
      const bail = setTimeout(() => finish(null), 6000);
      v.addEventListener("loadeddata", () => {
        // A tenth of the way in: frame zero of a clip is often the tail of a
        // loading screen or a fade, which says nothing about the round.
        const at = Math.min(Math.max((v.duration || 0) * 0.1, 0.1), 8);
        v.currentTime = isFinite(at) ? at : 0.1;
      });
      v.addEventListener("seeked", () => {
        try {
          const c = document.createElement("canvas");
          // Sized for the grid card it will fill (~253 CSS px), doubled for
          // display scaling. 136 was right for the old 62px list thumbnail and
          // is why the grid launched looking like a mosaic — a capture size
          // must track the box it is shown in.
          const w = 512;
          c.width = w;
          c.height = Math.max(1, Math.round((v.videoHeight / v.videoWidth) * w)) || 288;
          const ctx = c.getContext("2d");
          ctx.imageSmoothingQuality = "high";
          ctx.drawImage(v, 0, 0, c.width, c.height);
          clearTimeout(bail);
          finish(c.toDataURL("image/jpeg", 0.8));
        } catch (e) {
          // Was silent, and silence is why an empty grid looked like nothing
          // was happening rather than like one step failing.
          console.warn("thumbnail failed for", m.name, e);
          clearTimeout(bail);
          finish(null);
        }
      });
      v.addEventListener("error", () => {
        console.warn("thumbnail source failed for", m.name, v.error && v.error.message);
        clearTimeout(bail);
        finish(null);
      });
      v.src = mediaSrc(m.path);
    });
  thumbQueue = thumbQueue.then(job, job);
  return thumbQueue;
}

async function deleteMedia(m, li) {
  try {
    await invoke("delete_media", { path: m.path });
    thumbs.delete(m.path);
    el("mediaErr").classList.add("hidden");
    await loadMedia();
  } catch (e) {
    const err = el("mediaErr");
    err.textContent = String(e);
    err.classList.remove("hidden");
    const c = li.querySelector(".mconfirm");
    if (c) c.remove();
  }
}

/** Cover the card's own frame with the question, so which one is unambiguous. */
function askToDelete(m, li, thumb) {
  if (thumb.querySelector(".mconfirm")) return;
  const box = document.createElement("div");
  box.className = "mconfirm";

  const q = document.createElement("p");
  q.textContent = "Delete this clip?";

  const row = document.createElement("div");
  const yes = document.createElement("button");
  yes.className = "danger";
  yes.textContent = "Delete";
  yes.addEventListener("click", (e) => {
    e.stopPropagation();
    deleteMedia(m, li);
  });
  const no = document.createElement("button");
  no.textContent = "Cancel";
  no.addEventListener("click", (e) => {
    e.stopPropagation();
    box.remove();
  });

  row.append(yes, no);
  box.append(q, row);
  // Swallow the card's own play-on-click while the question is up.
  box.addEventListener("click", (e) => e.stopPropagation());
  thumb.appendChild(box);
}

const ICON_TRASH =
  '<svg viewBox="0 0 24 24"><path d="M3 6h18"></path><path d="M8 6V4h8v2"></path><path d="M19 6l-1 14H6L5 6"></path><path d="M10 11v6"></path><path d="M14 11v6"></path></svg>';
const ICON_FOLDER =
  '<svg viewBox="0 0 24 24"><path d="M4 20h16a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2z"></path></svg>';
const ICON_PLAY = '<svg viewBox="0 0 24 24"><path d="M8 5v14l11-7z"></path></svg>';

function renderMedia() {
  const list = el("mediaList");
  list.innerHTML = "";
  // The file being written right now is not watchable — no moov atom until
  // finalise — so it stays out of the grid rather than playing as broken.
  const rows = mediaItems.filter(
    (m) => m.kind === mediaTab && m.path !== currentRecordingPath
  );
  el("noMedia").classList.toggle("hidden", rows.length > 0);

  for (const m of rows) {
    const li = document.createElement("li");
    li.title = m.path;
    li.addEventListener("click", () => openPlayer(m));

    const thumb = document.createElement("div");
    thumb.className = "mthumb";
    thumb.innerHTML = ICON_PLAY;

    const img = document.createElement("img");
    img.alt = "";
    thumb.appendChild(img);

    if (m.duration_secs != null) {
      const d = document.createElement("span");
      d.className = "dur";
      d.textContent = fmtDur(m.duration_secs);
      thumb.appendChild(d);
    }

    const play = document.createElement("div");
    play.className = "mplay";
    play.innerHTML = ICON_PLAY;
    thumb.appendChild(play);

    const actions = document.createElement("div");
    actions.className = "mactions";

    const del = document.createElement("button");
    del.className = "danger";
    del.title = "Delete";
    del.innerHTML = ICON_TRASH;
    del.addEventListener("click", (e) => {
      e.stopPropagation();
      askToDelete(m, li, thumb);
    });

    const folder = document.createElement("button");
    folder.title = "Show in folder";
    folder.innerHTML = ICON_FOLDER;
    folder.addEventListener("click", (e) => {
      e.stopPropagation();
      invoke("reveal_in_explorer", { path: m.path });
    });

    actions.append(del, folder);
    thumb.appendChild(actions);

    // Decoded lazily and in order, so opening the tab does not start a dozen
    // decoders at once.
    thumbFor(m).then((src) => {
      if (!src) return;
      img.src = src;
      img.classList.add("ready");
    });

    const name = document.createElement("div");
    name.className = "m-name";
    name.textContent = m.player ? `${m.player} — ${m.name}` : m.name;

    const meta = document.createElement("div");
    meta.className = "m-meta";
    // Narrow cards, so only what distinguishes one clip from another: when it
    // happened and how big it is. Resolution and tracks live in the tooltip.
    meta.textContent = [fmtWhen(m), fmtBytes(m.bytes)].filter(Boolean).join(" · ");

    const info = document.createElement("div");
    info.className = "minfo";
    info.append(name, meta);

    li.append(thumb, info);
    list.appendChild(li);
  }
}

const setLibTab = (tab) => {
  mediaTab = tab;
  el("tabClips").classList.toggle("active", tab === "clip");
  el("tabRecs").classList.toggle("active", tab === "recording");
  renderMedia();
};
el("tabClips").addEventListener("click", () => setLibTab("clip"));
el("tabRecs").addEventListener("click", () => setLibTab("recording"));

el("openFolder").addEventListener("click", () => {
  const any = mediaItems.find((m) => m.kind === mediaTab) || mediaItems[0];
  if (any) invoke("reveal_in_explorer", { path: any.path });
});

/* ---------------------------------------------------------------- player */

// The media: scheme is served by the app itself with range support, so seeking
// works and nothing ever loads a whole recording into memory.
const mediaSrc = (p) => "http://media.localhost/" + encodeURIComponent(p);

function openPlayer(m) {
  playerItem = m;
  cut = { start: 0, end: null };
  renderCut();
  cutStatus(null);
  el("playerTitle").textContent = m.player ? `${m.player} — ${m.name}` : m.name;
  const v = el("player");
  v.src = mediaSrc(m.path);
  el("playerWrap").classList.remove("hidden");
  v.play().catch(() => {});
}

function closePlayer() {
  // A running export keeps going — it captured its paths — and announces
  // itself with the same toast a clip save gets.
  playerItem = null;
  const v = el("player");
  v.pause();
  v.removeAttribute("src");
  v.load(); // actually releases the file handle
  el("playerWrap").classList.add("hidden");
}

/* ------------------------------------------------------------------- cuts */

// Cut points for the open clip, in seconds; a null end means "to the end"
// (the duration is not known until the metadata loads).
let playerItem = null;
let cut = { start: 0, end: null };

const fmtCut = (s) => {
  const m = Math.floor(s / 60);
  return `${m}:${(s - m * 60).toFixed(1).padStart(4, "0")}`;
};

// The shortest cut worth exporting; also keeps the two handles from
// crossing or sitting on top of each other.
const MIN_CUT = 0.2;

function renderCut() {
  const dur = el("player").duration;
  const whole =
    cut.start <= 0.05 && (cut.end == null || (isFinite(dur) && cut.end >= dur - 0.05));
  el("cutRange").textContent = whole
    ? "whole clip"
    : `${fmtCut(cut.start)} → ${cut.end != null ? fmtCut(cut.end) : "end"}`;
}

function cutStatus(msg) {
  const p = el("cutStatus");
  p.textContent = msg || "";
  p.classList.toggle("hidden", !msg);
}

/* The trim slider. Dragging a handle scrubs the video to it, so the frame
   under the cut point is always the one on screen. */

function trimLayout() {
  const v = el("player");
  const dur = v.duration;
  const ok = isFinite(dur) && dur > 0;
  el("trimBar").classList.toggle("dim", !ok);
  if (!ok) return;
  const end = cut.end != null ? cut.end : dur;
  const pct = (t) => Math.min(100, Math.max(0, (t / dur) * 100)) + "%";
  el("trimStart").style.left = pct(cut.start);
  el("trimEnd").style.left = pct(end);
  const sel = el("trimSel");
  sel.style.left = pct(cut.start);
  sel.style.width = Math.max(0, ((end - cut.start) / dur) * 100) + "%";
  el("trimPlay").style.left = pct(v.currentTime);
  renderCut();
}

// Seeks are coalesced: scrubbing fires far faster than the decoder lands on
// frames, and queueing every position would leave the preview replaying a
// history of stale seeks after the pointer stopped.
let pendingSeek = null;
function previewSeek(t) {
  const v = el("player");
  if (!v.paused) v.pause();
  if (v.seeking) {
    pendingSeek = t;
    return;
  }
  v.currentTime = t;
}

function timeAtX(clientX) {
  const r = el("trimBar").getBoundingClientRect();
  const f = Math.min(1, Math.max(0, (clientX - r.left) / r.width));
  return f * (el("player").duration || 0);
}

/** Move one cut point to `t`, clamped so the cut never collapses. */
function setCutPoint(which, t) {
  const dur = el("player").duration;
  if (!isFinite(dur) || dur <= 0) return;
  if (which === "start") {
    cut.start = Math.max(0, Math.min(t, (cut.end != null ? cut.end : dur) - MIN_CUT));
    previewSeek(cut.start);
  } else {
    cut.end = Math.min(dur, Math.max(t, cut.start + MIN_CUT));
    previewSeek(cut.end);
  }
  trimLayout();
}

// A plain flag rather than hasPointerCapture: capture is requested as an
// optimisation (moves keep coming when the pointer leaves the handle) but the
// drag must not depend on it being granted.
let trimDrag = null;

function wireHandle(which) {
  const h = el(which === "start" ? "trimStart" : "trimEnd");
  h.addEventListener("pointerdown", (e) => {
    e.preventDefault();
    e.stopPropagation();
    trimDrag = which;
    try {
      h.setPointerCapture(e.pointerId);
    } catch {}
    h.focus();
  });
  h.addEventListener("pointermove", (e) => {
    if (trimDrag !== which) return;
    setCutPoint(which, timeAtX(e.clientX));
  });
  for (const ev of ["pointerup", "pointercancel"]) {
    h.addEventListener(ev, () => (trimDrag = null));
  }
  h.addEventListener("keydown", (e) => {
    const step = e.shiftKey ? 1 : 0.1;
    const delta =
      e.key === "ArrowLeft" ? -step : e.key === "ArrowRight" ? step : null;
    if (delta == null) return;
    e.preventDefault();
    const dur = el("player").duration;
    const at = which === "start" ? cut.start : cut.end != null ? cut.end : dur;
    setCutPoint(which, at + delta);
  });
}
wireHandle("start");
wireHandle("end");

// Pressing on the bare track grabs the nearer handle and starts dragging it —
// one gesture instead of an aim-for-the-handle test of dexterity.
el("trimBar").addEventListener("pointerdown", (e) => {
  const v = el("player");
  const dur = v.duration;
  if (!isFinite(dur) || dur <= 0) return;
  const t = timeAtX(e.clientX);
  const end = cut.end != null ? cut.end : dur;
  const which = Math.abs(t - cut.start) <= Math.abs(t - end) ? "start" : "end";
  trimDrag = which;
  const h = el(which === "start" ? "trimStart" : "trimEnd");
  try {
    h.setPointerCapture(e.pointerId);
  } catch {}
  setCutPoint(which, t);
});
window.addEventListener("pointerup", () => (trimDrag = null));

{
  const v = el("player");
  // The duration exists only once metadata loads; that is when a fresh clip's
  // cut becomes the whole clip and the slider first has a scale to draw on.
  v.addEventListener("loadedmetadata", () => {
    cut = { start: 0, end: v.duration };
    trimLayout();
  });
  v.addEventListener("seeked", () => {
    if (pendingSeek != null) {
      const t = pendingSeek;
      pendingSeek = null;
      v.currentTime = t;
    }
    trimLayout();
  });
  v.addEventListener("timeupdate", () => {
    const dur = v.duration;
    if (!isFinite(dur) || dur <= 0) return;
    el("trimPlay").style.left =
      Math.min(100, Math.max(0, (v.currentTime / dur) * 100)) + "%";
  });
}

el("cutExport").addEventListener("click", async () => {
  const m = playerItem;
  if (!m) return;
  const startS = cut.start;
  const endS = cut.end != null ? cut.end : el("player").duration;
  if (!isFinite(endS) || endS <= startS) {
    cutStatus("nothing to export — the cut is empty");
    return;
  }
  const mode = el("cutMode").value;
  const btn = el("cutExport");
  btn.disabled = true;
  cutStatus(
    mode === "trim"
      ? "Exporting…"
      : "Re-encoding — takes about a second per second of clip…"
  );
  try {
    const r = await invoke("export_clip", { path: m.path, startS, endS, mode });
    let msg = `Saved ${r.path.split(/[\\/]/).pop()} · ${fmtBytes(r.bytes)}`;
    // A lossless trim can only start on a keyframe; say so when it moved.
    if (mode === "trim" && startS - r.actual_start_s > 0.05) {
      msg += ` · starts ${(startS - r.actual_start_s).toFixed(1)}s early (keyframe)`;
    }
    cutStatus(msg);
    loadMedia();
  } catch (e) {
    cutStatus("Export failed: " + e);
  } finally {
    btn.disabled = false;
  }
});

// A <video> that cannot load shows 0:00 and nothing else. Put the reason
// where the title was, so "stuck" becomes a code someone can act on:
// 1 aborted, 2 network, 3 decode, 4 source not supported / blocked.
el("player").addEventListener("error", () => {
  const err = el("player").error;
  const code = err ? err.code : "?";
  const msg = err && err.message ? ` — ${err.message}` : "";
  el("playerTitle").textContent = `could not play (media error ${code}${msg})`;
});

el("playerClose").addEventListener("click", closePlayer);
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !el("playerWrap").classList.contains("hidden")) closePlayer();
});

/* -------------------------------------------------------------- settings */

function flash(msg) {
  el("saved").textContent = msg;
  setTimeout(() => (el("saved").textContent = ""), 2500);
}

el("save").addEventListener("click", async () => {
  const next = {
    ...cfg,
    window_secs: parseInt(el("window").value, 10),
    fps: parseInt(el("fps").value, 10),
    bitrate_mbps: parseInt(el("bitrate").value, 10),
    save_hotkey: el("hotkey").value,
    output_dir: el("outdir").value,
    capture_audio: el("capaudio").checked,
    capture_mic: el("capmic").checked,
    mix_audio: el("mixaudio").checked,
    notify_sound: el("notifysound").checked,
    notify_toast: el("notifytoast").checked,
    desktop_gain: pctToGain(el("gainDesktop").value),
    mic_gain: pctToGain(el("gainMic").value),
    desktop_device: el("devDesktop").value,
    mic_device: el("devMic").value,
    target: JSON.parse(el("target").value),
  };
  try {
    await invoke("set_config", { newConfig: next });
    cfg = next;
    setHotkeyHint();
    flash("saved — session restarting");
  } catch (e) {
    flash("could not save: " + e);
  }
});

loadConfig().then(tick);
loadMedia();
setInterval(tick, 500);
