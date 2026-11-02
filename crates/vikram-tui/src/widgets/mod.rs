//! Widgets. Each one is a pure function of [`App`] state, which is what lets the whole interface
//! be asserted against a `TestBackend` buffer without a terminal.

pub mod conversation;
pub mod markdown;
pub mod panels;
pub mod picker;

pub use conversation::{conversation, conversation_lines, extract_file_path};
pub use panels::{
    APPROVAL_ACTIONS, bots_panel, hive_panel, overlay, plasma, selection_overlay, session_tabs,
    shortcuts, status_bar, suggestions_popup,
};
pub use picker::{Picker, PickerItem, PickerKind, Verb, picker_lines, picker_title};
