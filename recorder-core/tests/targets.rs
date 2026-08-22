//! The picker's enumerators, against the real desktop.
//!
//! Nothing here can be mocked usefully: the point is that EnumDisplayMonitors
//! and EnumWindows are called with the right shapes and the results are
//! filtered the way alt-tab filters them. Runs on any Windows desktop session.

use recorder_core::capture;

#[test]
fn there_is_at_least_one_monitor_and_exactly_one_is_primary() {
    let m = capture::list_monitors();
    assert!(!m.is_empty(), "no monitors enumerated");
    assert_eq!(m.iter().filter(|x| x.primary).count(), 1, "{m:?}");
    for x in &m {
        assert!(x.width > 0 && x.height > 0, "{x:?}");
        assert!(x.device.starts_with(r"\\.\DISPLAY"), "{x:?}");
        assert!(x.index >= 1);
    }
}

#[test]
fn a_saved_device_name_resolves_and_a_stale_one_falls_back_to_primary() {
    let m = capture::list_monitors();
    let primary = m.iter().find(|x| x.primary).unwrap();
    let by_name = capture::find_monitor(&primary.device).expect("primary by name");
    assert_eq!(by_name.0 as isize, primary.hmonitor);
    let fallback = capture::find_monitor(r"\\.\DISPLAY99").expect("fallback");
    assert_eq!(
        fallback.0 as isize, primary.hmonitor,
        "a stale device name should fall back to the primary monitor"
    );
}

#[test]
fn windows_are_titled_visible_and_never_ours() {
    let w = capture::list_windows();
    for x in &w {
        assert!(!x.title.is_empty(), "{x:?}");
        assert_ne!(x.hwnd, 0);
    }
    // This process is a console test runner with no windows of its own, so the
    // self-exclusion is exercised only by absence — but a listed window must
    // still resolve back through the identity lookup.
    if let Some(first) = w.first() {
        let found = capture::find_window_by_identity(&first.title, &first.class);
        assert!(found.is_some(), "a listed window must be findable by its identity");
    }
}

#[test]
fn an_unknown_window_identity_is_none_not_a_guess() {
    assert!(capture::find_window_by_identity("no such title 8f3a", "NoSuchClass8f3a").is_none());
}
