//! QuickByte's egui front-end — the port of the `QuickByte.UI` forms.
//!
//! The binary (`main.rs`, the composition root) builds the services and hands
//! them to [`app::UiState`]; everything visual lives in this library so the
//! windows of later milestones plug into the same framework (see
//! [`windows`] for how a window is registered and opened).
//!
//! | Module | Port of |
//! |---|---|
//! | [`theme`] | `Controls/Theme.cs` — both palettes, the 12 px type scale, compact spacing |
//! | [`icons`] | `Controls/IconFactory.cs`, `Controls/BrandIcon.cs` |
//! | [`widgets`] | `SmoothProgressBar`, `ListViewProgressPainter`, `ConnectionSegmentsBar`, `FlatTabView`, `ConnectionCountBox`, `ProgressAnimator` |
//! | [`app`], [`dispatcher`], [`view_model`] | `MainForm`'s window plumbing, `AppDispatcher`, the shared state |
//! | [`windows`] | `Forms/*.cs` |
//! | [`shell`] | the gate to `quickbyte_shell` (demo mode touches nothing) |
//! | [`demo`] | in-memory services for `--demo` |

pub mod actions;
pub mod app;
pub mod demo;
pub mod dispatcher;
pub mod icons;
pub mod services;
pub mod shell;
pub mod theme;
pub mod view_model;
pub mod widgets;
pub mod windows;
