use anyhow::Result;
use db::{
    kvp::KeyValueStore,
    query,
    sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
    sqlez_macros::sql,
};
use util::ResultExt as _;
use workspace::{ItemId, WorkspaceDb, WorkspaceId};

use crate::session_store::TranscriptTarget;

const DOCK_SELECTION_NAMESPACE: &str = "claude_sessions_dock_selection";

pub struct ClaudeSessionsDb(ThreadSafeConnection);

impl Domain for ClaudeSessionsDb {
    const NAME: &str = stringify!(ClaudeSessionsDb);

    const MIGRATIONS: &[&str] = &[sql!(
        CREATE TABLE claude_session_tabs (
            workspace_id INTEGER,
            item_id INTEGER,
            session_id TEXT,
            agent_id TEXT,
            workflow_run_id TEXT,
            PRIMARY KEY(workspace_id, item_id),
            FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
            ON DELETE CASCADE
        ) STRICT;
    )];
}

db::static_connection!(ClaudeSessionsDb, [WorkspaceDb]);

impl ClaudeSessionsDb {
    query! {
        async fn save_tab_row(
            item_id: ItemId,
            workspace_id: WorkspaceId,
            session_id: Option<String>,
            agent_id: Option<String>,
            workflow_run_id: Option<String>
        ) -> Result<()> {
            INSERT OR REPLACE INTO claude_session_tabs
                (item_id, workspace_id, session_id, agent_id, workflow_run_id)
            VALUES (?, ?, ?, ?, ?)
        }
    }

    query! {
        fn get_tab_row(
            item_id: ItemId,
            workspace_id: WorkspaceId
        ) -> Result<Option<(Option<String>, Option<String>, Option<String>)>> {
            SELECT session_id, agent_id, workflow_run_id
            FROM claude_session_tabs
            WHERE item_id = ? AND workspace_id = ?
        }
    }

    pub async fn save_tab(
        &self,
        item_id: ItemId,
        workspace_id: WorkspaceId,
        tab: SavedTab,
    ) -> Result<()> {
        let (agent_id, workflow_run_id) = match tab.target {
            TranscriptTarget::Main => (None, None),
            TranscriptTarget::Subagent {
                agent_id,
                workflow_run_id,
            } => (Some(agent_id), workflow_run_id),
        };
        self.save_tab_row(
            item_id,
            workspace_id,
            tab.session_id,
            agent_id,
            workflow_run_id,
        )
        .await
    }

    pub fn get_tab(&self, item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<SavedTab>> {
        Ok(self.get_tab_row(item_id, workspace_id)?.map(
            |(session_id, agent_id, workflow_run_id)| SavedTab {
                session_id,
                // A row without an agent id is the session's own conversation, whatever
                // the run id says: the run id only names where an agent's file lives.
                target: match agent_id {
                    Some(agent_id) => TranscriptTarget::Subagent {
                        agent_id,
                        workflow_run_id,
                    },
                    None => TranscriptTarget::Main,
                },
            },
        ))
    }
}

/// What a Claude session tab is reading, which is all it needs to be opened again after a
/// restart: the session itself runs in tmux and outlives Zed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedTab {
    pub session_id: Option<String>,
    pub target: TranscriptTarget,
}

pub fn read_dock_selection(workspace_id: WorkspaceId, kvp: &KeyValueStore) -> Option<String> {
    kvp.scoped(DOCK_SELECTION_NAMESPACE)
        .read(&i64::from(workspace_id).to_string())
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str::<Option<String>>(&json).log_err())
        .flatten()
}

pub async fn save_dock_selection(
    workspace_id: WorkspaceId,
    session_id: Option<String>,
    kvp: KeyValueStore,
) -> Result<()> {
    kvp.scoped(DOCK_SELECTION_NAMESPACE)
        .write(
            i64::from(workspace_id).to_string(),
            serde_json::to_string(&session_id)?,
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db(name: &'static str) -> (ClaudeSessionsDb, WorkspaceId) {
        let db = ClaudeSessionsDb(db::open_test_db::<(WorkspaceDb, ClaudeSessionsDb)>(name).await);
        let workspace_id = db
            .write(|connection| {
                connection
                    .select_row::<WorkspaceId>(
                        "INSERT INTO workspaces DEFAULT VALUES RETURNING workspace_id",
                    )
                    .unwrap()()
                .unwrap()
            })
            .await
            .unwrap();
        (db, workspace_id)
    }

    #[gpui::test]
    async fn test_tab_round_trips_session_and_target() {
        let (db, workspace_id) = test_db("test_tab_round_trips_session_and_target").await;
        let tabs = [
            SavedTab {
                session_id: Some("session-main".to_string()),
                target: TranscriptTarget::Main,
            },
            SavedTab {
                session_id: Some("session-workflow".to_string()),
                target: TranscriptTarget::Subagent {
                    agent_id: "agent-1".to_string(),
                    workflow_run_id: Some("run-7".to_string()),
                },
            },
            SavedTab {
                session_id: Some("session-plain-agent".to_string()),
                target: TranscriptTarget::Subagent {
                    agent_id: "agent-2".to_string(),
                    workflow_run_id: None,
                },
            },
        ];
        for (item_id, tab) in (1..).zip(tabs.iter()) {
            db.save_tab(item_id, workspace_id, tab.clone())
                .await
                .unwrap();
        }
        for (item_id, tab) in (1..).zip(tabs.iter()) {
            assert_eq!(
                db.get_tab(item_id, workspace_id).unwrap().as_ref(),
                Some(tab)
            );
        }
        assert_eq!(db.get_tab(99, workspace_id).unwrap(), None);
    }

    #[gpui::test]
    async fn test_saving_a_tab_again_replaces_what_it_read() {
        let (db, workspace_id) = test_db("test_saving_a_tab_again_replaces_what_it_read").await;
        let first = SavedTab {
            session_id: Some("session-a".to_string()),
            target: TranscriptTarget::Subagent {
                agent_id: "agent-1".to_string(),
                workflow_run_id: Some("run-1".to_string()),
            },
        };
        let second = SavedTab {
            session_id: Some("session-b".to_string()),
            target: TranscriptTarget::Main,
        };
        db.save_tab(5, workspace_id, first).await.unwrap();
        db.save_tab(5, workspace_id, second.clone()).await.unwrap();
        assert_eq!(db.get_tab(5, workspace_id).unwrap(), Some(second));
    }
}
