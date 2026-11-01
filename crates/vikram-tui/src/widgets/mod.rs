//! Widgets. Each one is a pure function of [`App`] state, which is what lets the whole interface
//! be asserted against a `TestBackend` buffer without a terminal.

pub mod conversation;
pub mod markdown;
pub mod panels;

pub use conversation::{conversation, conversation_lines, extract_file_path};
pub use panels::{
    bots_panel, hive_panel, overlay, plasma, session_tabs, shortcuts, status_bar, suggestions_popup,
};
