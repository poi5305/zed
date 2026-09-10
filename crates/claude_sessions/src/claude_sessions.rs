#[cfg(test)]
mod blind_registry_tests;
#[cfg(test)]
mod blind_transcript_tests;
mod claude_sessions_button;
mod claude_sessions_panel;
mod session_registry;
mod session_store;
mod transcript;

use gpui::{App, actions};
use workspace::Workspace;

pub use claude_sessions_button::ClaudeSessionsButton;
pub use claude_sessions_panel::ClaudeSessionsPanel;
pub use session_registry::RegisteredSession;
pub use session_store::ClaudeSessionStore;
pub use transcript::{CompactMetadata, Transcript, TranscriptRecord};

actions!(
    claude_sessions,
    [
        /// Toggles focus on the Claude sessions panel.
        ToggleFocus
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
