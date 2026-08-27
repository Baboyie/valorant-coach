//! DEBRIEF recorder — Tauri shell around the measured capture pipeline.
//!
//! The window is a control surface, not the product: closing it hides to the
//! tray and buffering continues. Everything expensive lives on the engine
//! thread (see `engine.rs`), so nothing the webview does can stall capture.

mod config;
mod engine;
mod media;
mod sysmon;
mod vod;

use std::sync::Mutex;

use engine::{Cmd, Engine, Status};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

struct AppState {
    engine: Engine,
    config: Mutex<config::Config>,
}

/* ------------------------------------------------------------- commands */

#[tauri::command]
fn get_status(state: tauri::State<'_, AppState>) -> Status {
    state.engine.status()
}

#[tauri::command]
fn get_config(state: tauri::State<'_, AppState>) -> config::Config {
    state.config.lock().unwrap().clone()
}

#[tauri::command]
fn save_clip(state: tauri::State<'_, AppState>) {
    state.engine.send(Cmd::SaveClip);
}

#[tauri::command]
fn start_recording(state: tauri::State<'_, AppState>) {
    state.engine.send(Cmd::StartRecording);
}

#[tauri::command]
fn stop_recording(state: tauri::State<'_, AppState>) {
    state.engine.send(Cmd::StopRecording);
}

#[tauri::command]
fn set_config(
    state: tauri::State<'_, AppState>,
    new_config: config::Config,
) -> Result<(), String> {
    new_config.save().map_err(|e| e.to_string())?;
    *state.config.lock().unwrap() = new_config.clone();
    state.engine.send(Cmd::Reconfigure(Box::new(new_config)));
    Ok(())
}

/// Selectable audio endpoints, both roles. Enumerated on demand: a headset
/// plugged in after launch should appear when the user looks, not after a
/// restart.
#[tauri::command]
fn list_audio_devices() -> Vec<recorder_core::audio::AudioDevice> {
    recorder_core::audio::list_devices()
}

/// The gallery's rows: every clip and recording in the output directory,
/// sidecar metadata attached where a sidecar exists.
#[tauri::command]
fn list_media(state: tauri::State<'_, AppState>) -> Vec<media::MediaItem> {
    let dir = state.config.lock().unwrap().output_dir.clone();
    media::list(&dir)
}

/// Reveal a saved clip in Explorer. Selecting the file rather than opening the
/// folder saves the user hunting for it among a hundred timestamps.
#[tauri::command]
fn reveal_in_explorer(path: String) -> Result<(), String> {
    std::process::Command::new("explorer.exe")
        .arg(format!("/select,\"{path}\""))
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The overlay card asks to be dismissed once its slide-out has finished.
/// Hiding from the page rather than on a Rust timer keeps the timing next to
/// the animation that has to agree with it.
#[tauri::command]
fn hide_toast(app: AppHandle) {
    if let Some(w) = app.get_webview_window(TOAST_WINDOW) {
        let _ = w.hide();
    }
}

/// Show a sample card, so the popup can be seen and placed without waiting for
/// a real save — and so "is it working?" has an answer that does not require
/// being mid-match.
#[tauri::command]
fn preview_toast(app: AppHandle) {
    let Some(w) = app.get_webview_window(TOAST_WINDOW) else { return };
    place_toast(&w);
    let _ = w.emit_to(
        TOAST_WINDOW,
        "debrief://notice",
        serde_json::json!({
            "kind": "saved",
            "title": "Clip saved",
            "body": "clip-20260101-120000.mp4 · 52 ms",
            "ms": TOAST_MS,
        }),
    );
    show_toast_window(&w);
    let _ = w.set_always_on_top(true);
}

/// Everything a person could choose to record, for the picker. Enumerated on
/// demand rather than cached: windows come and go, and a stale list is how
/// someone records the wrong thing.
#[tauri::command]
fn list_targets() -> serde_json::Value {
    serde_json::json!({
        "monitors": recorder_core::capture::list_monitors(),
        "windows": recorder_core::capture::list_windows(),
    })
}

const TOAST_WINDOW: &str = "toast";
/// Card size in logical pixels, matched to `ui/toast.html`.
const TOAST_W: f64 = 340.0;
const TOAST_H: f64 = 76.0;
/// Distance from the working area's corner.
const TOAST_MARGIN: f64 = 18.0;
/// How long a card stays up. Long enough to read a filename mid-round,
/// short enough not to sit over the game.
const TOAST_MS: u32 = 3600;

/// Build the overlay window, hidden, ready to be shown on the first event.
///
/// **This is a window, not a hook.** ShadowPlay and Medal draw inside the
/// game's own swap chain, which means injecting into the game process — the
/// thing ADR §1 refuses, because it is what an anti-cheat cannot tell apart
/// from a cheat. An always-on-top, click-through, unfocusable window of our own
/// touches nothing of the game's, and the compositor puts it on top.
///
/// The price of not hooking: the compositor can only do that for a game running
/// **borderless**. A game in true exclusive fullscreen owns the display outright
/// and nothing short of a hook draws over it, so there the card simply does not
/// appear — which is why the chime exists and is not merely a duplicate.
fn build_toast_window(app: &AppHandle) -> tauri::Result<()> {
    if app.get_webview_window(TOAST_WINDOW).is_some() {
        return Ok(());
    }
    let w = WebviewWindowBuilder::new(app, TOAST_WINDOW, WebviewUrl::App("toast.html".into()))
        .title("DEBRIEF notice")
        .inner_size(TOAST_W, TOAST_H)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(false)
        // Never take focus. Stealing it from a game mid-round would be worse
        // than showing nothing at all.
        .focused(false)
        .visible(false)
        .build()?;
    // Clicks pass straight through to whatever is underneath, so the card can
    // never swallow a shot.
    let _ = w.set_ignore_cursor_events(true);

    // WS_EX_NOACTIVATE, which Tauri does not expose. Without it, showing the
    // card can take focus — and pulling focus out of a game mid-round would be
    // far worse than showing nothing at all. Building with .focused(false)
    // governs only the initial build; this governs every later show.
    #[cfg(windows)]
    if let Ok(h) = w.hwnd() {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE,
        };
        // Tauri carries its own copy of the windows crate, so its HWND is a
        // different type to ours despite being the same handle. Rebuild it from
        // the raw pointer rather than adding a second windows version.
        let hwnd = windows::Win32::Foundation::HWND(h.0 as *mut _);
        unsafe {
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE.0 as isize);
        }
    }
    Ok(())
}

/// Show the card without activating it.
///
/// Tauri's show() goes through ShowWindow(SW_SHOW), which activates.
/// SW_SHOWNOACTIVATE says what is actually meant, and does not depend on the
/// WS_EX_NOACTIVATE style having stuck.
fn show_toast_window(w: &tauri::WebviewWindow) {
    #[cfg(windows)]
    {
        if let Ok(h) = w.hwnd() {
            use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_SHOWNOACTIVATE};
            let hwnd = windows::Win32::Foundation::HWND(h.0 as *mut _);
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
            return;
        }
    }
    let _ = w.show();
}

/// Put the card in the bottom-right of the work area of the monitor it is on.
///
/// The *work area* rather than the full monitor, so it sits above the taskbar
/// on a desktop; over a fullscreen game the two are the same rectangle.
fn place_toast(w: &tauri::WebviewWindow) {
    let Ok(Some(mon)) = w.current_monitor() else { return };
    let scale = mon.scale_factor();
    // The work area, not the whole monitor: on a desktop that keeps the card
    // clear of the taskbar instead of half behind it. Over a fullscreen game
    // the taskbar is gone and the two rectangles are the same.
    let area = mon.work_area();
    let origin = area.position.to_logical::<f64>(scale);
    let size = area.size.to_logical::<f64>(scale);
    let _ = w.set_position(tauri::LogicalPosition::new(
        origin.x + size.width - TOAST_W - TOAST_MARGIN,
        origin.y + size.height - TOAST_H - TOAST_MARGIN,
    ));
}

/// Announce engine events three ways: an on-screen card, a chime, and the tray
/// tooltip. Each covers a case the others cannot.
///
/// The **card** is the Medal/ShadowPlay-style popup, and works whenever the game
/// is borderless. The **chime** is what gets through in true exclusive
/// fullscreen, where nothing but a hook can draw over the picture. The
/// **tooltip** is for tabbing out later and asking what happened.
///
/// Windows toasts are deliberately absent: `tauri-plugin-notification` depends
/// on `rand`, whose `zerocopy` build script Smart App Control refuses to
/// execute on this machine — twelve fresh links, twelve refusals. They would
/// also have been the least useful of the four, since Windows silences its own
/// notifications while a game is fullscreen.
fn present_notices(app: AppHandle, rx: std::sync::mpsc::Receiver<engine::Notice>) {
    use engine::Notice;

    let name = |p: &std::path::Path| {
        p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()
    };

    for notice in rx {
        // Read fresh each time, so toggling a setting takes effect on the next
        // event rather than at the next restart.
        let (sound, toast) = match app.try_state::<AppState>() {
            Some(state) => {
                let c = state.config.lock().unwrap();
                (c.notify_sound, c.notify_toast)
            }
            None => (true, true),
        };

        let (kind, title, body, cue): (&str, &str, String, fn()) = match &notice {
            Notice::ClipSaved { path, ms } => (
                "saved",
                "Clip saved",
                format!("{} · {} ms", name(path), ms.round()),
                recorder_core::cue::saved as fn(),
            ),
            Notice::RecordingStarted => (
                "recording",
                "Recording",
                "Started — press stop in DEBRIEF when done.".to_string(),
                recorder_core::cue::marker as fn(),
            ),
            Notice::RecordingSaved { path, secs } => (
                "saved",
                "Recording saved",
                format!("{} · {:.0}s", name(path), secs),
                recorder_core::cue::marker as fn(),
            ),
            Notice::Failed { what } => (
                "failed",
                "DEBRIEF",
                what.clone(),
                recorder_core::cue::failed as fn(),
            ),
        };

        // The chime first: it is the half that reaches someone mid-round, and
        // building a window should not delay it.
        if sound {
            cue();
        }

        if toast {
            if let Some(w) = app.get_webview_window(TOAST_WINDOW) {
                // Re-placed on every notice: the card should follow the monitor
                // the user is actually on, and that can change between saves.
                place_toast(&w);
                let _ = w.emit_to(
                    TOAST_WINDOW,
                    "debrief://notice",
                    serde_json::json!({
                        "kind": kind,
                        "title": title,
                        "body": body,
                        "ms": TOAST_MS,
                    }),
                );
                // Shown after the payload, so the card never appears blank for a
                // frame before its text arrives.
                show_toast_window(&w);
                // Re-assert topmost: a game that has taken the top spot since
                // the last notice would otherwise cover the card.
                let _ = w.set_always_on_top(true);
            }
        }

        if let Some(tray) = app.tray_by_id("main") {
            let _ = tray.set_tooltip(Some(&format!("DEBRIEF — {title}: {body}")));
        }
    }
}

/* ---------------------------------------------------------------- setup */

fn show_main_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// Drive the engine headlessly and report what happened.
///
/// The Tauri window cannot be clicked from a script, so without this the only
/// way to verify the app's own wiring — config, engine thread, detection,
/// buffering, save — is by hand. Everything below the webview is exercised
/// here; only the UI layer is not.
///
/// Enabled with `DEBRIEF_AUTOTEST=1`. Never reachable in normal use.
pub fn run_autotest() {
    let secs: u64 = std::env::var("DEBRIEF_AUTOTEST_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    let mut cfg = config::Config::load();
    // Short window so the test does not have to run for a full buffer.
    cfg.window_secs = 10;
    println!("autotest: window {}s, buffering for {secs}s", cfg.window_secs);
    println!("output   : {}", cfg.output_dir.display());

    let engine = Engine::spawn(cfg);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut last = String::new();
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let s = engine.status();
        let line = format!(
            "{:?} game={} buffered={:.1}s ring={:.1}MB kept={} p99={}us",
            s.state, s.game_running, s.buffered_secs, s.ring_mb, s.frames_kept, s.callback_p99_us
        );
        if line != last {
            println!("  {line}");
            last = line;
        }
        if let Some(e) = &s.last_error {
            println!("  ERROR: {e}");
        }
    }

    println!("autotest: requesting clip");
    engine.send(Cmd::SaveClip);
    std::thread::sleep(std::time::Duration::from_secs(3));

    let s = engine.status();
    match (&s.last_clip, &s.last_error) {
        (Some(p), _) => {
            println!("autotest: SAVED {p}");
            if let Some(ms) = s.last_save_ms {
                println!("autotest: save cost {ms:.0} ms");
            }
            match std::fs::metadata(p) {
                Ok(m) => println!("autotest: file is {:.2} MB", m.len() as f64 / 1e6),
                Err(e) => println!("autotest: could not stat clip: {e}"),
            }
        }
        (None, Some(e)) => println!("autotest: FAILED — {e}"),
        (None, None) => println!("autotest: FAILED — no clip and no error reported"),
    }

    engine.send(Cmd::Shutdown);
    std::thread::sleep(std::time::Duration::from_millis(600));
}

pub fn run() {
    if std::env::var("DEBRIEF_AUTOTEST").is_ok() {
        run_autotest();
        return;
    }

    let cfg = config::Config::load();
    // Persist immediately so a first run leaves an editable file on disk
    // rather than a config that exists only in memory.
    let _ = cfg.save();
    // One-time: move loose clip-*/recording-* files into clips/ and
    // recordings/. Safe here because nothing is being written yet.
    media::migrate_layout(&cfg.output_dir);
    let hotkey = cfg.save_hotkey.clone();
    let engine = Engine::spawn(cfg.clone());

    tauri::Builder::default()
        // Must be first, per the plugin's own requirement.
        //
        // A second instance is not a harmless duplicate. It opens its own
        // capture session and its own NVENC encoder against the same game, so
        // encode load doubles — and the measured 15.3% encoder cost, which the
        // "no measurable FPS impact" claim rests on, is a figure for *one*
        // recorder. The visible symptom is milder and misleading: the second
        // process cannot claim the global hotkey, so Alt+F10 silently does
        // nothing while both copies quietly compete for the encoder.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // Launching again is a request to see the app, not to start
            // another one — most often clicking the shortcut having forgotten
            // it is already in the tray.
            show_main_window(app);
        }))
        // Gallery playback. Serves .mp4 files from the output directory with
        // HTTP range semantics, so the webview's <video> can seek a
        // multi-gigabyte recording without anything loading it whole. The
        // path is re-validated on every request; this scheme is the only
        // bridge between the webview and the filesystem.
        .register_uri_scheme_protocol("media", |ctx, request| {
            let root = ctx
                .app_handle()
                .try_state::<AppState>()
                .map(|s| s.config.lock().unwrap().output_dir.clone());
            media::serve(root, request)
        })
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    // Fire on press only; without this the clip saves twice,
                    // once on the way down and once on the way up.
                    if event.state == ShortcutState::Pressed {
                        if let Some(state) = app.try_state::<AppState>() {
                            state.engine.send(Cmd::SaveClip);
                        }
                    }
                })
                .build(),
        )
        .manage(AppState {
            engine,
            config: Mutex::new(cfg),
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_config,
            set_config,
            save_clip,
            start_recording,
            stop_recording,
            reveal_in_explorer,
            list_targets,
            list_media,
            list_audio_devices,
            hide_toast,
            preview_toast,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();

            // ---- tray ----
            let open = MenuItem::with_id(app, "open", "Open DEBRIEF", true, None::<&str>)?;
            let clip = MenuItem::with_id(app, "clip", "Save clip", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &clip, &quit])?;

            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("DEBRIEF — Valorant recorder")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "open" => show_main_window(app),
                    "clip" => {
                        if let Some(state) = app.try_state::<AppState>() {
                            state.engine.send(Cmd::SaveClip);
                        }
                    }
                    "quit" => {
                        if let Some(state) = app.try_state::<AppState>() {
                            // Let the engine finalise any open file before the
                            // process goes away.
                            state.engine.send(Cmd::Shutdown);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(400));
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main_window(tray.app_handle());
                    }
                })
                .build(app)?;

            // ---- overlay card ----
            // Built up front and kept hidden: creating a webview window takes
            // long enough that doing it on the first save would delay the
            // confirmation it exists to deliver.
            if let Err(e) = build_toast_window(&handle) {
                eprintln!("could not create the notice overlay: {e}");
            }

            // ---- notifications ----
            // Driven from the engine, not from the UI polling status: this
            // window spends its life hidden in the tray, and Chromium throttles
            // timers in a hidden webview to as little as once a minute. A
            // confirmation that lands a minute after the hotkey is not one.
            if let Some(rx) = handle.state::<AppState>().engine.take_notices() {
                let notify_handle = handle.clone();
                std::thread::Builder::new()
                    .name("debrief-notify".into())
                    .spawn(move || present_notices(notify_handle, rx))
                    .expect("failed to spawn notification thread");
            }

            // ---- global hotkey ----
            // A hotkey that fails to register is worth surfacing: the user
            // would otherwise press it mid-match and silently get nothing.
            if let Err(e) = handle.global_shortcut().register(hotkey.as_str()) {
                eprintln!("could not register hotkey {hotkey}: {e}");
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing or minimizing hides the window into the tray. A recorder
            // that stops buffering because someone tidied their taskbar is not
            // a recorder — and once closing hides, a minimized-but-present
            // taskbar entry is just a second, inconsistent way to do the same
            // thing. The tray icon is the way back in either case.
            match event {
                WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    let _ = window.hide();
                }
                WindowEvent::Resized(_) => {
                    // Minimize arrives as a Resized event; ask the window.
                    // Hide without un-minimizing first — that would replay the
                    // restore animation on its way out. show_main_window()
                    // already un-minimizes on the way back in.
                    if window.is_minimized().unwrap_or(false) {
                        let _ = window.hide();
                    }
                }
                _ => {}
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running DEBRIEF");
}
