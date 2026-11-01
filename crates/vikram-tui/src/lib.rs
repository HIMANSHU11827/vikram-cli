//! The VIKRAM terminal interface.
//!
//! Layering: `app` is a view model, `widgets` render it, `input` translates keys into commands, and
//! `ui` composes the layout. None of these hold runtime ownership, so a redraw, a panel toggle or a
//! session switch can never cancel work the runtime is doing.

pub mod app;
pub mod editor;
pub mod input;
pub mod layout;
pub mod theme;
pub mod ui;
pub mod widgets;

pub use app::{App, Overlay, SessionView, UiCommand};
pub use theme::Theme;
