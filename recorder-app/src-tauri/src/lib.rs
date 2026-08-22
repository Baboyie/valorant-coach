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
use tauri::{AppHandle, Manager, WindowEvent};
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
