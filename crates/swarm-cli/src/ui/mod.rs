//! The interactive terminal UI: one task owns the terminal, everything else
//! talks to it over channels.

pub mod app;
pub mod dialog;
pub mod diff;
pub mod editor;
pub mod files;
pub mod history;
pub mod markdown;
pub mod menu;
pub mod registry;
pub mod screen;
pub mod term;
pub mod text;
pub mod vim;

pub use app::{Action, Info, Mode, Status, echo_rows};
pub use dialog::{Accent, DialogSpec};
pub use term::{StartOptions, Ui, UiHandle};
