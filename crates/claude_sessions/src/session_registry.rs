use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RegisteredSession {
    #[serde(rename = "pid")]
    pub process_id: u32,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "cwd")]
    pub working_directory: PathBuf,
    #[serde(rename = "procStart")]
    pub process_start: String,
    pub version: String,
    pub kind: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(rename = "updatedAt", default)]
    pub updated_at: Option<i64>,
    #[serde(rename = "tmux", default)]
    pub tmux_target: Option<String>,
    #[serde(rename = "bridgeSessionId", default)]
    pub bridge_session_id: Option<String>,
}

/// 解析單一註冊檔。未知欄位忽略，缺少必要欄位回 Err。
pub fn parse_registered_session(contents: &str) -> anyhow::Result<RegisteredSession> {
    let session = serde_json::from_str(contents)?;
    Ok(session)
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Liveness {
    Live,
    StaleHeartbeat,
    ProcessGone,
    PidReused,
}

pub fn liveness(
    session: &RegisteredSession,
    now_millis: i64,
    stale_after_millis: i64,
    process_start_of_pid: &dyn Fn(u32) -> Option<String>,
) -> Liveness {
    let Some(actual_process_start) = process_start_of_pid(session.process_id) else {
        return Liveness::ProcessGone;
    };

    if actual_process_start != session.process_start {
        return Liveness::PidReused;
    }

    if let Some(updated_at) = session.updated_at {
        if now_millis.saturating_sub(updated_at) > stale_after_millis {
            return Liveness::StaleHeartbeat;
        }
    }

    Liveness::Live
}

pub fn visible_sessions(
    sessions: Vec<RegisteredSession>,
    project_root: Option<&Path>,
    now_millis: i64,
    stale_after_millis: i64,
    process_start_of_pid: &dyn Fn(u32) -> Option<String>,
) -> Vec<RegisteredSession> {
    let mut visible_sessions: Vec<RegisteredSession> = sessions
        .into_iter()
        .filter(|session| {
            session.kind == "interactive"
                && liveness(
                    session,
                    now_millis,
                    stale_after_millis,
                    process_start_of_pid,
                ) == Liveness::Live
        })
        .collect();

    visible_sessions.sort_by(|first_session, second_session| {
        let first_is_in_project = project_root.is_some_and(|root| {
            first_session.working_directory == root
                || first_session.working_directory.starts_with(root)
        });
        let second_is_in_project = project_root.is_some_and(|root| {
            second_session.working_directory == root
                || second_session.working_directory.starts_with(root)
        });

        // The process id breaks ties so that the order does not fall through to the
        // arbitrary order the directory listing produced: it is unique among live
        // sessions and fixed for a session's lifetime, so a session cannot swap places
        // with its namesake between two polls and make the list flicker.
        let first_sort_key = (
            !first_is_in_project,
            first_session.name.as_deref().unwrap_or(""),
            first_session.process_id,
        );
        let second_sort_key = (
            !second_is_in_project,
            second_session.name.as_deref().unwrap_or(""),
            second_session.process_id,
        );

        first_sort_key.cmp(&second_sort_key)
    });

    visible_sessions
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_ONE_JSON: &str = r#"{"pid":10064,"sessionId":"4e2e3600-89c0-4cd5-9994-525c708559ab",
 "cwd":"/Users/andy/go/src/github.com/poi5305/zed","startedAt":1789007244364,
 "procStart":"Thu Sep 10 02:27:23 2026","version":"2.1.267","peerProtocol":1,
 "peerFeatures":["notify_idle","reply_across_default_dirs","artifact_yield"],
 "kind":"interactive","entrypoint":"cli","pidDomain":"darwin",
 "tmux":"zed:@6.%8","messagingSocketPath":"/tmp/cc-socks/10064.sock",
 "name":"zed-e4","nameSource":"derived","nameSince":1789007244364,
 "status":"busy","updatedAt":1789009278063,"statusUpdatedAt":1789009278063,
 "bridgeSessionId":"session_01Tf2BzmxZDH3YKYpdxtSrpD"}"#;

    const SAMPLE_TWO_JSON: &str = r#"{"pid":17694,"sessionId":"095bcff6-b9a8-4584-a3c6-861f16c9a807",
 "cwd":"/Users/andy/go/src/github.com/poi5305/zed","startedAt":1789008783382,
 "procStart":"Thu Sep 10 02:53:02 2026","version":"2.1.267","peerProtocol":1,
 "peerFeatures":["notify_idle"],"kind":"interactive","entrypoint":"cli",
 "pidDomain":"darwin","messagingSocketPath":"/tmp/cc-socks/17694.sock",
 "name":"zed-28","nameSource":"derived","nameSince":1789008783382,
 "status":"busy","updatedAt":1789009345355,"statusUpdatedAt":1789009345355,
 "bridgeSessionId":"session_01DzrBq3KnHvrXgSL43UVZgB"}"#;

    #[test]
    fn test_parse_sample_one_all_fields() -> anyhow::Result<()> {
        let session = parse_registered_session(SAMPLE_ONE_JSON)?;
        assert_eq!(session.process_id, 10064);
        assert_eq!(session.session_id, "4e2e3600-89c0-4cd5-9994-525c708559ab");
        assert_eq!(
            session.working_directory,
            PathBuf::from("/Users/andy/go/src/github.com/poi5305/zed")
        );
        assert_eq!(session.process_start, "Thu Sep 10 02:27:23 2026");
        assert_eq!(session.version, "2.1.267");
        assert_eq!(session.kind, "interactive");
        assert_eq!(session.name.as_deref(), Some("zed-e4"));
        assert_eq!(session.status.as_deref(), Some("busy"));
        assert_eq!(session.updated_at, Some(1789009278063));
        assert_eq!(session.tmux_target.as_deref(), Some("zed:@6.%8"));
        assert_eq!(
            session.bridge_session_id.as_deref(),
            Some("session_01Tf2BzmxZDH3YKYpdxtSrpD")
        );
        Ok(())
    }

    #[test]
    fn test_parse_sample_two_without_tmux() -> anyhow::Result<()> {
        let session = parse_registered_session(SAMPLE_TWO_JSON)?;
        assert_eq!(session.process_id, 17694);
        assert_eq!(session.tmux_target, None);
        Ok(())
    }

    #[test]
    fn test_parse_with_unknown_fields() -> anyhow::Result<()> {
        let json_with_future_field = r#"{"pid":10064,"sessionId":"4e2e3600-89c0-4cd5-9994-525c708559ab",
 "cwd":"/Users/andy/go/src/github.com/poi5305/zed","startedAt":1789007244364,
 "procStart":"Thu Sep 10 02:27:23 2026","version":"2.1.267","peerProtocol":1,
 "peerFeatures":["notify_idle","reply_across_default_dirs","artifact_yield"],
 "kind":"interactive","entrypoint":"cli","pidDomain":"darwin",
 "tmux":"zed:@6.%8","messagingSocketPath":"/tmp/cc-socks/10064.sock",
 "name":"zed-e4","nameSource":"derived","nameSince":1789007244364,
 "status":"busy","updatedAt":1789009278063,"statusUpdatedAt":1789009278063,
 "bridgeSessionId":"session_01Tf2BzmxZDH3YKYpdxtSrpD",
 "futureField": 123}"#;

        let session = parse_registered_session(json_with_future_field)?;
        assert_eq!(session.process_id, 10064);
        Ok(())
    }

    #[test]
    fn test_parse_missing_session_id_returns_error() {
        let json_missing_session_id = r#"{"pid":10064,
 "cwd":"/Users/andy/go/src/github.com/poi5305/zed",
 "procStart":"Thu Sep 10 02:27:23 2026","version":"2.1.267",
 "kind":"interactive"}"#;

        let result = parse_registered_session(json_missing_session_id);
        assert!(result.is_err());
    }

    #[test]
    fn test_liveness_process_gone() -> anyhow::Result<()> {
        let session = parse_registered_session(SAMPLE_ONE_JSON)?;
        let process_start_of_pid = |_process_id: u32| None;
        let liveness_result = liveness(&session, 1789009278063, 60000, &process_start_of_pid);
        assert_eq!(liveness_result, Liveness::ProcessGone);
        Ok(())
    }

    #[test]
    fn test_liveness_pid_reused() -> anyhow::Result<()> {
        let session = parse_registered_session(SAMPLE_ONE_JSON)?;
        let process_start_of_pid = |_process_id: u32| Some("Fri Sep 11 00:00:00 2026".to_string());
        let liveness_result = liveness(&session, 1789009278063, 60000, &process_start_of_pid);
        assert_eq!(liveness_result, Liveness::PidReused);
        Ok(())
    }

    #[test]
    fn test_liveness_stale_heartbeat() -> anyhow::Result<()> {
        let session = parse_registered_session(SAMPLE_ONE_JSON)?;
        let process_start_of_pid = |_process_id: u32| Some("Thu Sep 10 02:27:23 2026".to_string());
        let now_millis = 1789009278063 + 60001;
        let stale_after_millis = 60000;
        let liveness_result = liveness(
            &session,
            now_millis,
            stale_after_millis,
            &process_start_of_pid,
        );
        assert_eq!(liveness_result, Liveness::StaleHeartbeat);
        Ok(())
    }

    #[test]
    fn test_liveness_live() -> anyhow::Result<()> {
        let session = parse_registered_session(SAMPLE_ONE_JSON)?;
        let process_start_of_pid = |_process_id: u32| Some("Thu Sep 10 02:27:23 2026".to_string());
        let now_millis = 1789009278063 + 30000;
        let stale_after_millis = 60000;
        let liveness_result = liveness(
            &session,
            now_millis,
            stale_after_millis,
            &process_start_of_pid,
        );
        assert_eq!(liveness_result, Liveness::Live);
        Ok(())
    }

    #[test]
    fn test_liveness_live_when_updated_at_none() -> anyhow::Result<()> {
        let mut session = parse_registered_session(SAMPLE_ONE_JSON)?;
        session.updated_at = None;
        let process_start_of_pid = |_process_id: u32| Some("Thu Sep 10 02:27:23 2026".to_string());
        let now_millis = 9999999999999;
        let stale_after_millis = 1;
        let liveness_result = liveness(
            &session,
            now_millis,
            stale_after_millis,
            &process_start_of_pid,
        );
        assert_eq!(liveness_result, Liveness::Live);
        Ok(())
    }

    #[test]
    fn test_visible_sessions_filters_and_sorts() -> anyhow::Result<()> {
        let project_root = Path::new("/Users/andy/go/src/github.com/poi5305/zed");

        let mut session_non_interactive = parse_registered_session(SAMPLE_ONE_JSON)?;
        session_non_interactive.process_id = 1;
        session_non_interactive.kind = "non_interactive".to_string();

        let mut session_gone = parse_registered_session(SAMPLE_ONE_JSON)?;
        session_gone.process_id = 2;

        let mut session_project_beta = parse_registered_session(SAMPLE_ONE_JSON)?;
        session_project_beta.process_id = 3;
        session_project_beta.name = Some("beta".to_string());
        session_project_beta.working_directory = project_root.join("crates/claude_sessions");

        let mut session_project_alpha = parse_registered_session(SAMPLE_ONE_JSON)?;
        session_project_alpha.process_id = 4;
        session_project_alpha.name = Some("alpha".to_string());
        session_project_alpha.working_directory = project_root.to_path_buf();

        let mut session_other_project_no_name = parse_registered_session(SAMPLE_ONE_JSON)?;
        session_other_project_no_name.process_id = 5;
        session_other_project_no_name.name = None;
        session_other_project_no_name.working_directory = PathBuf::from("/Users/andy/other");

        let mut session_other_project_zebra = parse_registered_session(SAMPLE_ONE_JSON)?;
        session_other_project_zebra.process_id = 6;
        session_other_project_zebra.name = Some("zebra".to_string());
        session_other_project_zebra.working_directory = PathBuf::from("/Users/andy/other");

        let sessions = vec![
            session_other_project_zebra,
            session_project_beta,
            session_non_interactive,
            session_other_project_no_name,
            session_gone,
            session_project_alpha,
        ];

        let process_start_of_pid = |process_id: u32| {
            if process_id == 2 {
                None
            } else {
                Some("Thu Sep 10 02:27:23 2026".to_string())
            }
        };

        let result = visible_sessions(
            sessions,
            Some(project_root),
            1789009278063,
            60000,
            &process_start_of_pid,
        );

        let process_ids: Vec<u32> = result
            .into_iter()
            .map(|session| session.process_id)
            .collect();
        assert_eq!(process_ids, vec![4, 3, 5, 6]);

        Ok(())
    }

    #[test]
    fn test_visible_sessions_project_root_none() -> anyhow::Result<()> {
        let mut session_beta = parse_registered_session(SAMPLE_ONE_JSON)?;
        session_beta.process_id = 1;
        session_beta.name = Some("beta".to_string());

        let mut session_alpha = parse_registered_session(SAMPLE_ONE_JSON)?;
        session_alpha.process_id = 2;
        session_alpha.name = Some("alpha".to_string());

        let sessions = vec![session_beta, session_alpha];
        let process_start_of_pid = |_process_id: u32| Some("Thu Sep 10 02:27:23 2026".to_string());

        let result = visible_sessions(sessions, None, 1789009278063, 60000, &process_start_of_pid);

        let process_ids: Vec<u32> = result
            .into_iter()
            .map(|session| session.process_id)
            .collect();
        assert_eq!(process_ids, vec![2, 1]);

        Ok(())
    }

    #[test]
    fn sessions_that_tie_on_name_are_ordered_by_process_id() -> anyhow::Result<()> {
        let project_root = Path::new("/Users/andy/go/src/github.com/poi5305/zed");

        let mut same_name_higher_pid = parse_registered_session(SAMPLE_ONE_JSON)?;
        same_name_higher_pid.process_id = 900;
        same_name_higher_pid.name = Some("zed-e4".to_string());
        same_name_higher_pid.working_directory = project_root.to_path_buf();

        let mut same_name_lower_pid = parse_registered_session(SAMPLE_ONE_JSON)?;
        same_name_lower_pid.process_id = 500;
        same_name_lower_pid.name = Some("zed-e4".to_string());
        same_name_lower_pid.working_directory = project_root.join("crates");

        let mut unnamed_higher_pid = parse_registered_session(SAMPLE_ONE_JSON)?;
        unnamed_higher_pid.process_id = 400;
        unnamed_higher_pid.name = None;
        unnamed_higher_pid.working_directory = PathBuf::from("/Users/andy/other");

        let mut unnamed_lower_pid = parse_registered_session(SAMPLE_ONE_JSON)?;
        unnamed_lower_pid.process_id = 300;
        unnamed_lower_pid.name = None;
        unnamed_lower_pid.working_directory = PathBuf::from("/Users/andy/other");

        // The order the directory listing happened to hand them over, which is what the
        // ordering must not depend on.
        let sessions = vec![
            same_name_higher_pid,
            unnamed_higher_pid,
            same_name_lower_pid,
            unnamed_lower_pid,
        ];
        let process_start_of_pid = |_process_id: u32| Some("Thu Sep 10 02:27:23 2026".to_string());

        let result = visible_sessions(
            sessions,
            Some(project_root),
            1789009278063,
            60000,
            &process_start_of_pid,
        );

        let process_ids: Vec<u32> = result
            .into_iter()
            .map(|session| session.process_id)
            .collect();
        assert_eq!(
            process_ids,
            vec![500, 900, 300, 400],
            "sessions tying on project membership and name must be ordered by process id, got {process_ids:?}"
        );

        Ok(())
    }
}
