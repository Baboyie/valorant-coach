// Release builds get the windows subsystem so launching the app does not flash
// a console. Debug keeps the console, because the engine reports failures
// there and losing them during development would be worse than a stray window.
// The autotest path needs a console to report into, so the subsystem is only
// switched when we are actually going to be a GUI.
#![cfg_attr(
    all(not(debug_assertions), not(feature = "console")),
    windows_subsystem = "windows"
)]

fn main() {
    recorder_app_lib::run();
}
