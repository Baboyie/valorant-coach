//! The recorder pipeline from ADR-001, as a library.
//!
//! Everything here was proven and measured through `recorder-proto` on the
//! benchmark rig before it became a library: WGC capture of a live
//! Vanguard-protected Valorant, NVENC encode with no CPU readback, the replay
//! ring with pooled buffers, and resize survival. See
//! `docs/ADR-001-capture-architecture.md` §7–§9 for the numbers and the bugs
//! that measurement shook out.
//!
//! Consumers: `recorder-proto` (the headless CLI the benchmarks run) and
//! `recorder-app` (the Tauri desktop app). The design rule carried over from
//! the prototype: nothing in this crate knows about a UI, and the capture
//! callback never blocks, allocates, or waits on anything downstream.

pub mod audio;
pub mod capture;
pub mod cue;
pub mod d3d;
pub mod encoder;
pub mod encoders;
pub mod export;
pub mod mix;
pub mod mp4;
pub mod replay;
