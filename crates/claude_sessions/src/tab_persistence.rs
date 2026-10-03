use std::path::PathBuf;

use db::{
    query,
    sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
    sqlez_macros::sql,
};
use serde::{Deserialize, Serialize};
use workspace::{ItemId, WorkspaceDb, WorkspaceId};

use crate::{RegisteredSession, TranscriptTarget};

/// What a session tab needs to be opened again after the window that held it is gone.
///
/// Only the session's identity is kept, never what a scan last said about it: the process
/// id is what lets the first scan after a restore recognise a session that `/clear` gave
/// a new id while nothing was watching, and the rest is what the tab draws before that
/// scan lands. Status and heartbeat would be stale by the time anything read them back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SerializedSessionTab {
    pub session_id: String,
    #[serde(default)]
    pub agent: Option<SerializedAgent>,
    #[serde(default)]
    pub seed: Option<SerializedSeed>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SerializedAgent {
    pub agent_id: String,
    #[serde(default)]
    pub workflow_run_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SerializedSeed {
    pub process_id: u32,
    pub working_directory: PathBuf,
    #[serde(default)]
    pub process_start: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tmux_target: Option<String>,
    #[serde(default)]
    pub bridge_session_id: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<PathBuf>,
}

impl SerializedSessionTab {
    pub fn new(
        session_id: &str,
        seed: Option<&(RegisteredSession, Option<PathBuf>)>,
        target: &TranscriptTarget,
    ) -> Self {
        let agent = match target {
            TranscriptTarget::Main => None,
            TranscriptTarget::Subagent {
                agent_id,
                workflow_run_id,
            } => Some(SerializedAgent {
                agent_id: agent_id.clone(),
                workflow_run_id: workflow_run_id.clone(),
            }),
        };
        // A seed describing another session would point the restored tab's first frames
        // at the wrong conversation, so it is dropped rather than kept.
        let seed = seed
            .filter(|(session, _)| session.session_id == session_id)
            .map(|(session, transcript_path)| SerializedSeed {
                process_id: session.process_id,
                working_directory: session.working_directory.clone(),
                process_start: session.process_start.clone(),
                version: session.version.clone(),
                kind: session.kind.clone(),
                name: session.name.clone(),
                tmux_target: session.tmux_target.clone(),
                bridge_session_id: session.bridge_session_id.clone(),
                transcript_path: transcript_path.clone(),
            });
        Self {
            session_id: session_id.to_string(),
            agent,
            seed,
        }
    }

    pub fn target(&self) -> TranscriptTarget {
        match &self.agent {
            None => TranscriptTarget::Main,
            Some(agent) => TranscriptTarget::Subagent {
                agent_id: agent.agent_id.clone(),
                workflow_run_id: agent.workflow_run_id.clone(),
            },
        }
    }

    pub fn seed(&self) -> Option<(RegisteredSession, Option<PathBuf>)> {
        let seed = self.seed.as_ref()?;
        Some((
            RegisteredSession {
                process_id: seed.process_id,
                session_id: self.session_id.clone(),
                working_directory: seed.working_directory.clone(),
                process_start: seed.process_start.clone(),
                version: seed.version.clone(),
                kind: seed.kind.clone(),
                name: seed.name.clone(),
                status: None,
                updated_at: None,
                tmux_target: seed.tmux_target.clone(),
                bridge_session_id: seed.bridge_session_id.clone(),
            },
            seed.transcript_path.clone(),
        ))
    }
}

pub(crate) struct ClaudeSessionTabDb(ThreadSafeConnection);

impl Domain for ClaudeSessionTabDb {
    const NAME: &str = stringify!(ClaudeSessionTabDb);

    const MIGRATIONS: &[&str] = &[sql!(
        CREATE TABLE claude_session_tabs (
            workspace_id INTEGER,
            item_id INTEGER,
            state TEXT NOT NULL,
            PRIMARY KEY(workspace_id, item_id),
            FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
            ON DELETE CASCADE
        ) STRICT;
    )];
}

db::static_connection!(ClaudeSessionTabDb, [WorkspaceDb]);

impl ClaudeSessionTabDb {
    query! {
        pub(crate) async fn save_tab(item_id: ItemId, workspace_id: WorkspaceId, state: String) -> Result<()> {
            INSERT OR REPLACE INTO claude_session_tabs(item_id, workspace_id, state)
            VALUES (?, ?, ?)
        }
    }

    query! {
        pub(crate) async fn get_tab(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<String>> {
            SELECT state
            FROM claude_session_tabs
            WHERE item_id = ? AND workspace_id = ?
        }
    }
}

/// An empty alive set is a workspace that restored no session tabs, not `NOT IN ()`.
#[cfg_attr(not(any(test, target_family = "wasm")), allow(dead_code))]
pub(crate) fn delete_unloaded_tabs_sql(alive_count: usize) -> String {
    if alive_count == 0 {
        "DELETE FROM claude_session_tabs WHERE workspace_id = ?".to_string()
    } else {
        let placeholders = vec!["?"; alive_count].join(", ");
        format!(
            "DELETE FROM claude_session_tabs WHERE workspace_id = ? AND item_id NOT IN ({placeholders})"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered(session_id: &str) -> RegisteredSession {
        RegisteredSession {
            process_id: 4242,
            session_id: session_id.to_string(),
            working_directory: PathBuf::from("/work/project"),
            process_start: "Thu Oct  2 10:00:00 2026".to_string(),
            version: "2.1.0".to_string(),
            kind: "interactive".to_string(),
            name: Some("poi".to_string()),
            status: Some("busy".to_string()),
            updated_at: Some(1_700_000_000_000),
            tmux_target: Some("poi:@3.%7".to_string()),
            bridge_session_id: Some("bridge-1".to_string()),
        }
    }

    fn round_trip(tab: &SerializedSessionTab) -> SerializedSessionTab {
        let json = serde_json::to_string(tab).expect("serializing a tab");
        serde_json::from_str(&json).expect("deserializing a tab")
    }

    #[test]
    fn a_main_conversation_tab_round_trips_with_its_identity() {
        let seed = (
            registered("session-1"),
            Some(PathBuf::from("/home/me/.claude/projects/p/session-1.jsonl")),
        );
        let tab = SerializedSessionTab::new("session-1", Some(&seed), &TranscriptTarget::Main);
        let restored = round_trip(&tab);
        assert_eq!(restored, tab);
        assert_eq!(restored.target(), TranscriptTarget::Main);

        let (session, transcript_path) = restored.seed().expect("the seed survives");
        let mut expected = seed.0.clone();
        expected.status = None;
        expected.updated_at = None;
        assert_eq!(
            session, expected,
            "status and heartbeat are not restored, everything else is"
        );
        assert_eq!(transcript_path, seed.1);
    }

    #[test]
    fn an_agent_tab_round_trips_its_target() {
        let target = TranscriptTarget::Subagent {
            agent_id: "agent-a1".to_string(),
            workflow_run_id: Some("run-9".to_string()),
        };
        let tab = SerializedSessionTab::new("session-1", None, &target);
        let restored = round_trip(&tab);
        assert_eq!(restored.target(), target);
        assert_eq!(restored.seed(), None);

        let plain_agent = TranscriptTarget::Subagent {
            agent_id: "agent-b2".to_string(),
            workflow_run_id: None,
        };
        let restored = round_trip(&SerializedSessionTab::new("session-1", None, &plain_agent));
        assert_eq!(restored.target(), plain_agent);
    }

    #[test]
    fn a_seed_for_another_session_is_not_kept() {
        let seed = (registered("session-other"), None);
        let tab = SerializedSessionTab::new("session-1", Some(&seed), &TranscriptTarget::Main);
        assert_eq!(tab.seed, None, "got {:?}", tab.seed);
        assert_eq!(tab.seed(), None);
    }

    #[test]
    fn a_tab_saved_with_only_a_session_id_still_reads() {
        let restored: SerializedSessionTab =
            serde_json::from_str(r#"{"session_id":"session-1"}"#).expect("minimal state");
        assert_eq!(restored.session_id, "session-1");
        assert_eq!(restored.target(), TranscriptTarget::Main);
        assert_eq!(restored.seed(), None);
    }

    #[test]
    fn delete_unloaded_tabs_never_emits_an_empty_in_list() {
        assert_eq!(
            delete_unloaded_tabs_sql(0),
            "DELETE FROM claude_session_tabs WHERE workspace_id = ?"
        );
        assert_eq!(
            delete_unloaded_tabs_sql(2),
            "DELETE FROM claude_session_tabs WHERE workspace_id = ? AND item_id NOT IN (?, ?)"
        );
    }
}
