const { invoke } = window.__TAURI__.core;

const el = (id) => document.getElementById(id);
let cfg = null;
let lastClipPath = null;

async function loadConfig() {
  cfg = await invoke("get_config");
  el("window").value = cfg.window_secs;
  el("fps").value = cfg.fps;
  el("bitrate").value = cfg.bitrate_mbps;
  el("hotkey").value = cfg.save_hotkey;
  el("outdir").value = cfg.output_dir;
  el("capaudio").checked = cfg.capture_audio;
  el("capmic").checked = cfg.capture_mic;
  await refreshTargets();
  el("hotkeyhint").textContent = `Press ${cfg.save_hotkey} in game to save the last ${cfg.window_secs}s.`;
}

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

function fmtState(s) {
  // Name the target unless it is Valorant, whose name is the product's —
  // “buffering: Notepad” rather than leaving the user to find out from the
  // clip which window they actually recorded.
  const auto = !cfg || !cfg.target || cfg.target.kind === "valorant";
  const named = (verb) => (!auto && s.target_title ? `${verb}: ${s.target_title}` : verb);
  if (s.state === "recording") return named("recording");
  if (s.state === "buffering") return named("buffering");
  if (s.game_running) return named("starting…");
  return auto ? "waiting for Valorant" : "chosen window or screen is not available";
}

async function tick() {
  let s;
  try {
    s = await invoke("get_status");
  } catch (e) {
    return;
  }

  el("dot").className = "dot " + s.state;
  el("state").textContent = fmtState(s);

  // While recording there is no ring, so show elapsed recording time in the
  // same slot rather than a frozen 0.0s that looks like a stall.
  if (s.state === "recording") {
    el("buffered").textContent = s.recording_secs.toFixed(1) + "s";
    el("ringsize").textContent = "recorded";
    el("fill").style.width = "100%";
  } else {
    el("buffered").textContent = s.buffered_secs.toFixed(1) + "s";
    el("ringsize").textContent =
      s.ring_mb > 0 ? `buffered · ${s.ring_mb.toFixed(1)} MB` : "buffered";
    const target = cfg ? cfg.window_secs : 30;
    el("fill").style.width =
      Math.min(100, (s.buffered_secs / target) * 100).toFixed(1) + "%";
  }

  // §17: never fake these. The engine sends null until it has a real delta,
  // and an em dash is the honest rendering of "not measured yet".
  const p = s.perf;
  el("cpu").textContent = p ? p.cpu_pct.toFixed(2) + " %" : "—";
  el("ram").textContent = p ? p.ram_mb.toFixed(0) + " MB" : "—";
  el("vram").textContent =
    p && p.vram_mb != null
      ? `${p.vram_mb.toFixed(0)} MB of ${p.vram_budget_mb.toFixed(0)}`
      : "—";
  el("disk").textContent = p ? p.disk_write_mbps.toFixed(1) + " MB/s" : "—";

  el("kept").textContent = s.frames_kept.toLocaleString();
  el("p99").textContent = s.callback_p99_us ? s.callback_p99_us + " µs" : "—";
  el("dropfull").textContent = s.dropped_ring_full.toLocaleString();
  el("dropresize").textContent = s.dropped_resized.toLocaleString();

  el("clip").disabled = s.state !== "buffering";
  el("record").disabled = !s.game_running;
  const rec = s.state === "recording";
  el("record").textContent = rec ? "Stop recording" : "Start recording";
  el("record").className = rec ? "rec" : "";

  if (s.last_clip) {
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

// Picking a target applies immediately — it is a choice, not a setting, and
// Discord trained everyone to expect the share to start on the click.
el("target").addEventListener("change", async () => {
  const next = { ...cfg, target: JSON.parse(el("target").value) };
  try {
    await invoke("set_config", { newConfig: next });
    cfg = next;
    el("saved").textContent = "target applied";
    setTimeout(() => (el("saved").textContent = ""), 2500);
  } catch (e) {
    el("saved").textContent = "could not apply: " + e;
  }
});

el("refreshTargets").addEventListener("click", refreshTargets);

el("reveal").addEventListener("click", () => {
  if (lastClipPath) invoke("reveal_in_explorer", { path: lastClipPath });
});

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
    target: JSON.parse(el("target").value),
  };
  try {
    await invoke("set_config", { newConfig: next });
    cfg = next;
    el("hotkeyhint").textContent = `Press ${cfg.save_hotkey} in game to save the last ${cfg.window_secs}s.`;
    el("saved").textContent = "saved — session restarting";
    setTimeout(() => (el("saved").textContent = ""), 2500);
  } catch (e) {
    el("saved").textContent = "could not save: " + e;
  }
});

loadConfig().then(tick);
setInterval(tick, 500);
