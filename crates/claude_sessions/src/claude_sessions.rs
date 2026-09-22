#[cfg(test)]
mod blind_anchor_tests;
#[cfg(test)]
mod blind_live_state_tests;
#[cfg(test)]
mod blind_registry_tests;
#[cfg(test)]
mod blind_subagent_tests;
#[cfg(test)]
mod blind_transcript_tests;
mod claude_sessions_panel;
mod live_state;
mod session_source;
mod session_store;
mod terminal_anchors;
mod transcript;
mod usage;

use gpui::{App, actions};
use serde::Deserialize;
use settings::{DockSide, RegisterSetting, Settings};
use workspace::Workspace;

pub use claude_sessions_panel::ClaudeSessionsPanel;
// The registry parsing and liveness rules live in `remote`, so that the remote server can
// run them without depending on this crate's UI. The alias keeps the path this crate's
// own modules and tests use pointing at the one implementation.
pub use live_state::{
    ChannelInboxEvent, HookEvent, LiveMessage, LiveState, PendingQuestion, PermissionRequest,
    RunningTool, StatusSnapshot, Turn, parse_channel_inbox_line, parse_hook_event, timestamp_ms,
};
pub use remote::claude_sessions as session_registry;
pub use session_registry::HookInstallOutcome;
pub use session_registry::{AgentListing, RegisteredSession, SubagentMeta, SubagentSummary};
pub use session_source::{FileContents, LocalSource, RemoteSource, SessionListing, SessionSource};
pub use session_store::{
    ClaudeSessionStore, EndedReason, EndedSession, LiveSession, SessionRow, StoreClock,
    TranscriptTarget,
};
pub use terminal_anchors::{
    AnchorGlyph, AnchorRow, Anchoring, Glyphs, MAX_TRANSCRIPT_ANCHORS, MIN_SKELETON, ScreenRow,
    TranscriptAnchor, align, anchor_rows, rows_match, screen_rows, skeleton,
};
pub use transcript::{AutoModeFlags, CompactMetadata, Transcript, TranscriptRecord};
pub use usage::{ModelRates, Usage, rates_for_model};

/// The panel's own settings. Its dock side lives here rather than in the panel, so that
/// the side the user dragged it to is still there after a restart — the same place every
/// other panel keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, RegisterSetting)]
pub struct ClaudeSessionsSettings {
    pub dock: DockSide,
    #[serde(default = "default_user_prompt_glyph")]
    pub user_prompt_glyph: char,
    #[serde(default = "default_assistant_glyph")]
    pub assistant_glyph: char,
}

fn default_user_prompt_glyph() -> char {
    '>'
}

fn default_assistant_glyph() -> char {
    '⏺'
}

/// A settings value is one terminal cell. Empty and multi-character strings are not
/// glyphs, so they keep the default; a single character, including a space or a
/// non-ASCII letter, is used as written.
fn configured_glyph(value: Option<&str>, default: char) -> char {
    let Some(value) = value else {
        return default;
    };
    let mut characters = value.chars();
    match (characters.next(), characters.next()) {
        (Some(glyph), None) => glyph,
        _ => default,
    }
}

impl Settings for ClaudeSessionsSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let claude_sessions = content.claude_sessions.as_ref();
        Self {
            // The left-hand dock is the default because the project panel defaults to the
            // right one, and a panel sharing a dock with it would take turns being the
            // visible one rather than being readable beside it. A settings file too old
            // to carry the key at all gets that same answer rather than a panic.
            dock: claude_sessions
                .and_then(|claude_sessions| claude_sessions.dock)
                .unwrap_or(DockSide::Left),
            user_prompt_glyph: configured_glyph(
                claude_sessions
                    .and_then(|claude_sessions| claude_sessions.user_prompt_glyph.as_deref()),
                default_user_prompt_glyph(),
            ),
            assistant_glyph: configured_glyph(
                claude_sessions
                    .and_then(|claude_sessions| claude_sessions.assistant_glyph.as_deref()),
                default_assistant_glyph(),
            ),
        }
    }
}

actions!(
    claude_sessions,
    [
        /// Toggles focus on the Claude sessions panel.
        ToggleFocus,
        /// Opens the Claude sessions conversation as a tab in the editor area.
        OpenInEditor,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<ClaudeSessionsPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &OpenInEditor, window, cx| {
            ClaudeSessionsPanel::open_in_pane(workspace, window, cx);
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use settings::{ClaudeSessionsSettingsContent, Settings, SettingsContent};

    use super::*;

    #[test]
    fn glyph_settings_keep_one_character_and_fall_back_otherwise() {
        let defaults = ClaudeSessionsSettings::from_settings(&SettingsContent::default());
        assert_eq!(defaults.dock, DockSide::Left);
        assert_eq!(defaults.user_prompt_glyph, '>');
        assert_eq!(defaults.assistant_glyph, '⏺');

        let mut content = SettingsContent::default();
        content.claude_sessions = Some(ClaudeSessionsSettingsContent {
            user_prompt_glyph: Some("中".to_string()),
            assistant_glyph: Some("●".to_string()),
            ..Default::default()
        });
        let configured = ClaudeSessionsSettings::from_settings(&content);
        assert_eq!(configured.user_prompt_glyph, '中');
        assert_eq!(configured.assistant_glyph, '●');

        content.claude_sessions = Some(ClaudeSessionsSettingsContent {
            user_prompt_glyph: Some(" ".to_string()),
            assistant_glyph: Some(">>".to_string()),
            ..Default::default()
        });
        let mixed = ClaudeSessionsSettings::from_settings(&content);
        assert_eq!(mixed.user_prompt_glyph, ' ');
        assert_eq!(mixed.assistant_glyph, '⏺');

        content.claude_sessions = Some(ClaudeSessionsSettingsContent {
            user_prompt_glyph: Some(String::new()),
            assistant_glyph: None,
            ..Default::default()
        });
        let empty = ClaudeSessionsSettings::from_settings(&content);
        assert_eq!(empty.user_prompt_glyph, '>');
        assert_eq!(empty.assistant_glyph, '⏺');
    }
}
