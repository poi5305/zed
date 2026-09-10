#[cfg(test)]
mod blind_input_tests;
#[cfg(test)]
mod blind_registry_tests;
#[cfg(test)]
mod blind_transcript_tests;
mod claude_sessions_button;
mod claude_sessions_panel;
mod session_source;
mod session_store;
mod transcript;

use gpui::{App, actions};
use workspace::Workspace;

pub use claude_sessions_button::ClaudeSessionsButton;
pub use claude_sessions_panel::ClaudeSessionsPanel;
// The registry parsing and liveness rules live in `remote`, so that the remote server can
// run them without depending on this crate's UI. The alias keeps the path this crate's
// own modules and tests use pointing at the one implementation.
pub use remote::claude_sessions as session_registry;
pub use session_registry::RegisteredSession;
pub use session_source::{
    FileContents, LocalSource, RemoteSource, SessionInput, SessionListing, SessionSource,
};
pub use session_store::ClaudeSessionStore;
pub use transcript::{CompactMetadata, Transcript, TranscriptRecord};

actions!(
    claude_sessions,
    [
        /// Toggles focus on the Claude sessions panel.
        ToggleFocus,
        /// Sends the message to the selected Claude Code session.
        SendMessage,
        /// Interrupts the selected Claude Code session, as Escape does in the terminal.
        Interrupt
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<ClaudeSessionsPanel>(window, cx);
        });
    })
    .detach();
}
