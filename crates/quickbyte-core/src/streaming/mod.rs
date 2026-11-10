//! Streaming video (HLS and DASH) — the C# `Streaming/` folder.

pub mod dash_manifest;
pub mod hls_playlist;
pub mod stream_detector;
pub mod stream_fetch;
pub mod stream_plan;
pub mod stream_resolver;
pub mod stream_state;

pub use stream_resolver::{StreamChoice, StreamDescription};
