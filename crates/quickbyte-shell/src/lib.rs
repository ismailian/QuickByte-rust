//! QuickByte's Windows integration — the non-visual half of the C# `QuickByte.UI`
//! project. Nothing here knows about egui: windows are referred to by raw `HWND`
//! values the UI hands over. All registry access in the product lives here
//! (Core and the agent have none — see MIGRATION.md §4.1).
//!
//! Everything is best-effort and silent on failure, exactly as the C# classes
//! documented: a refused registry write, a shell that will not answer, or a
//! missing file is never a reason to fail a save, a launch or a window.

#![cfg(windows)]

pub mod app_version;
pub mod clipboard_watcher;
pub mod dark_mode;
pub mod file_launcher;
pub mod keep_awake;
pub mod power_actions;
pub mod queue_agent_registration;
pub mod queue_wake_registration;
pub mod single_instance;
pub mod startup_registration;
pub mod system_theme;
pub mod taskbar_progress;
pub mod tray;
pub mod zone_information_policy;

mod win;
