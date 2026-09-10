#[cfg(test)]
mod blind_input_tests;
#[cfg(test)]
mod blind_registry_tests;
#[cfg(test)]
mod blind_subagent_tests;
#[cfg(test)]
mod blind_transcript_tests;
mod claude_sessions_button;
mod claude_sessions_panel;
mod session_source;
mod session_store;
mod transcript;

use gpui::{App, actions};
use serde::Deserialize;
use settings::{DockSide, RegisterSetting, Settings};
use workspace::Workspace;

pub use claude_sessions_button::ClaudeSessionsButton;
pub use claude_sessions_panel::ClaudeSessionsPanel;
// The registry parsing and liveness rules live in `remote`, so that the remote server can
// run them without depending on this crate's UI. The alias keeps the path this crate's
// own modules and tests use pointing at the one implementation.
pub use remote::claude_sessions as session_registry;
pub use session_registry::{RegisteredSession, SubagentMeta, SubagentSummary};
pub use session_source::{
    FileContents, LocalSource, RemoteSource, SessionInput, SessionListing, SessionSource,
};
pub use session_store::{ClaudeSessionStore, TranscriptTarget};
pub use transcript::{CompactMetadata, Transcript, TranscriptRecord};

/// The panel's own settings. Its dock side lives here rather than in the panel, so that
/// the side the user dragged it to is still there after a restart — the same place every
/// other panel keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, RegisterSetting)]
pub struct ClaudeSessionsSettings {
    pub dock: DockSide,
}

impl Settings for ClaudeSessionsSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            // The right-hand dock is the default because the left one already holds the
            // project panel and the project manager panel, and the point of this one is
            // to be readable beside them. A settings file too old to carry the key at
            // all gets that same answer rather than a panic.
            dock: content
                .claude_sessions
                .as_ref()
                .and_then(|claude_sessions| claude_sessions.dock)
                .unwrap_or(DockSide::Right),
        }
    }
}

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
