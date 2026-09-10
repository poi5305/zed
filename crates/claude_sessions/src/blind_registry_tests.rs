#[cfg(test)]
mod blind_registry_tests {
    use crate::session_registry::*;
    use std::path::PathBuf;

    const SAMPLE: &str = r#"{"pid":10064,"sessionId":"4e2e3600-89c0-4cd5-9994-525c708559ab",
     "cwd":"/Users/andy/go/src/github.com/poi5305/zed","startedAt":1789007244364,
     "procStart":"Thu Sep 10 02:27:23 2026","version":"2.1.267","peerProtocol":1,
     "peerFeatures":["notify_idle"],"kind":"interactive","entrypoint":"cli",
     "pidDomain":"darwin","tmux":"zed:@6.%8",
     "messagingSocketPath":"/tmp/cc-socks/10064.sock","name":"zed-e4",
     "nameSource":"derived","nameSince":1789007244364,"status":"busy",
     "updatedAt":1789009278063,"statusUpdatedAt":1789009278063,
     "bridgeSessionId":"session_01Tf2BzmxZDH3YKYpdxtSrpD"}"#;

    fn sample_without(field: &str) -> String {
        let mut value: serde_json::Value =
            serde_json::from_str(SAMPLE).expect("SAMPLE must be valid JSON");
        let object = value
            .as_object_mut()
            .expect("SAMPLE must be a JSON object at the top level");
        assert!(
            object.remove(field).is_some(),
            "test bug: field {:?} was not present in SAMPLE, so removing it proves nothing",
            field
        );
        value.to_string()
    }

    fn describe(result: &anyhow::Result<RegisteredSession>) -> String {
        match result {
            Ok(session) => format!(
                "Ok(pid={}, sessionId={:?}, kind={:?})",
                session.process_id, session.session_id, session.kind
            ),
            Err(error) => format!("Err({error})"),
        }
    }

    fn session(
        process_id: u32,
        session_id: &str,
        working_directory: &str,
        name: Option<&str>,
        kind: &str,
    ) -> RegisteredSession {
        RegisteredSession {
            process_id,
            session_id: session_id.to_string(),
            working_directory: PathBuf::from(working_directory),
            process_start: "start-of-pid".to_string(),
            version: "2.1.267".to_string(),
            kind: kind.to_string(),
            name: name.map(|name| name.to_string()),
            status: None,
            updated_at: None,
            tmux_target: None,
            bridge_session_id: None,
        }
    }

    /// Every pid is alive with the same process start string as `session()` produces.
    fn all_alive(_process_id: u32) -> Option<String> {
        Some("start-of-pid".to_string())
    }

    fn identifiers(sessions: &[RegisteredSession]) -> Vec<String> {
        sessions
            .iter()
            .map(|session| session.session_id.clone())
            .collect()
    }

    // ---------- parse_registered_session ----------

    #[test]
    fn parses_the_real_world_sample_including_optional_fields() {
        let parsed = parse_registered_session(SAMPLE);
        let session = match parsed {
            Ok(session) => session,
            Err(error) => panic!("expected Ok for the real-world sample, got Err({error})"),
        };
        assert_eq!(
            session.process_id, 10064,
            "process_id from pid; session_id was {:?}",
            session.session_id
        );
        assert_eq!(
            session.session_id, "4e2e3600-89c0-4cd5-9994-525c708559ab",
            "session_id from sessionId"
        );
        assert_eq!(
            session.working_directory,
            PathBuf::from("/Users/andy/go/src/github.com/poi5305/zed"),
            "working_directory from cwd"
        );
        assert_eq!(
            session.process_start, "Thu Sep 10 02:27:23 2026",
            "process_start from procStart"
        );
        assert_eq!(session.version, "2.1.267", "version from version");
        assert_eq!(session.kind, "interactive", "kind from kind");
        assert_eq!(
            session.name,
            Some("zed-e4".to_string()),
            "name from name (Option)"
        );
        assert_eq!(
            session.status,
            Some("busy".to_string()),
            "status from status (Option)"
        );
        assert_eq!(
            session.updated_at,
            Some(1789009278063),
            "updated_at from updatedAt (Option<i64>)"
        );
        assert_eq!(
            session.tmux_target,
            Some("zed:@6.%8".to_string()),
            "tmux_target from tmux"
        );
        assert_eq!(
            session.bridge_session_id,
            Some("session_01Tf2BzmxZDH3YKYpdxtSrpD".to_string()),
            "bridge_session_id from bridgeSessionId"
        );
    }

    #[test]
    fn missing_pid_is_an_error() {
        let contents = sample_without("pid");
        let result = parse_registered_session(&contents);
        assert!(
            result.is_err(),
            "expected Err when pid is missing, got {}",
            describe(&result)
        );
    }

    #[test]
    fn missing_session_id_is_an_error() {
        let contents = sample_without("sessionId");
        let result = parse_registered_session(&contents);
        assert!(
            result.is_err(),
            "expected Err when sessionId is missing, got {}",
            describe(&result)
        );
    }

    #[test]
    fn missing_cwd_is_an_error() {
        let contents = sample_without("cwd");
        let result = parse_registered_session(&contents);
        assert!(
            result.is_err(),
            "expected Err when cwd is missing, got {}",
            describe(&result)
        );
    }

    #[test]
    fn missing_proc_start_is_an_error() {
        let contents = sample_without("procStart");
        let result = parse_registered_session(&contents);
        assert!(
            result.is_err(),
            "expected Err when procStart is missing, got {}",
            describe(&result)
        );
    }

    #[test]
    fn missing_version_is_an_error() {
        let contents = sample_without("version");
        let result = parse_registered_session(&contents);
        assert!(
            result.is_err(),
            "expected Err when version is missing, got {}",
            describe(&result)
        );
    }

    #[test]
    fn missing_kind_is_an_error() {
        let contents = sample_without("kind");
        let result = parse_registered_session(&contents);
        assert!(
            result.is_err(),
            "expected Err when kind is missing, got {}",
            describe(&result)
        );
    }

    #[test]
    fn only_required_fields_parses_and_optionals_are_none() {
        let contents = r#"{"pid":7,"sessionId":"s","cwd":"/tmp","procStart":"p",
            "version":"v","kind":"interactive"}"#;
        let session = match parse_registered_session(contents) {
            Ok(session) => session,
            Err(error) => panic!("expected Ok with only required fields, got Err({error})"),
        };
        assert_eq!(session.name, None, "name must default to None");
        assert_eq!(session.status, None, "status must default to None");
        assert_eq!(session.updated_at, None, "updated_at must default to None");
        assert_eq!(
            session.tmux_target, None,
            "tmux_target must default to None"
        );
        assert_eq!(
            session.bridge_session_id, None,
            "bridge_session_id must default to None"
        );
    }

    #[test]
    fn unknown_fields_are_ignored_rather_than_rejected() {
        let contents = r#"{"pid":7,"sessionId":"s","cwd":"/tmp","procStart":"p",
            "version":"v","kind":"interactive",
            "someFutureField":{"nested":[1,2,{"deeper":true}]},
            "anotherOne":null,"yetAnother":"text"}"#;
        let result = parse_registered_session(contents);
        assert!(
            result.is_ok(),
            "unknown fields must be ignored (no deny_unknown_fields), got {}",
            describe(&result)
        );
    }

    #[test]
    fn malformed_json_is_an_error() {
        let result = parse_registered_session("{\"pid\":7,");
        assert!(
            result.is_err(),
            "expected Err for truncated JSON, got {}",
            describe(&result)
        );
    }

    // ---------- liveness ----------

    #[test]
    fn liveness_is_process_gone_when_pid_lookup_returns_none() {
        let session = session(10, "s", "/tmp", None, "interactive");
        let lookup = |_process_id: u32| None;
        let actual = liveness(&session, 1_000, 100, &lookup);
        assert_eq!(
            actual,
            Liveness::ProcessGone,
            "pid lookup returned None; session {:?}",
            session.session_id
        );
    }

    #[test]
    fn process_gone_outranks_a_stale_heartbeat() {
        let mut session = session(10, "s", "/tmp", None, "interactive");
        session.updated_at = Some(0);
        let lookup = |_process_id: u32| None;
        let actual = liveness(&session, 1_000_000, 100, &lookup);
        assert_eq!(
            actual,
            Liveness::ProcessGone,
            "rule 1 must be checked before rule 3; updated_at={:?}",
            session.updated_at
        );
    }

    #[test]
    fn liveness_is_pid_reused_when_process_start_differs() {
        let session = session(10, "s", "/tmp", None, "interactive");
        let lookup = |_process_id: u32| Some("a-different-start".to_string());
        let actual = liveness(&session, 1_000, 100, &lookup);
        assert_eq!(
            actual,
            Liveness::PidReused,
            "expected PidReused because {:?} != {:?}",
            "a-different-start",
            session.process_start
        );
    }

    #[test]
    fn pid_reused_outranks_a_stale_heartbeat() {
        let mut session = session(10, "s", "/tmp", None, "interactive");
        session.updated_at = Some(0);
        let lookup = |_process_id: u32| Some("a-different-start".to_string());
        let actual = liveness(&session, 1_000_000, 100, &lookup);
        assert_eq!(
            actual,
            Liveness::PidReused,
            "rule 2 must be checked before rule 3; updated_at={:?}",
            session.updated_at
        );
    }

    #[test]
    fn process_start_comparison_is_exact_string_equality() {
        let session = session(10, "s", "/tmp", None, "interactive");
        let lookup = |_process_id: u32| Some(" start-of-pid".to_string());
        let actual = liveness(&session, 1_000, 100, &lookup);
        assert_eq!(
            actual,
            Liveness::PidReused,
            "a leading space must not be trimmed away; stored {:?}",
            session.process_start
        );
    }

    #[test]
    fn heartbeat_exactly_at_the_threshold_is_still_live() {
        let mut session = session(10, "s", "/tmp", None, "interactive");
        session.updated_at = Some(500);
        // Spec says StaleHeartbeat requires `now - updated_at > stale_after`, so equality is Live.
        let actual = liveness(&session, 600, 100, &all_alive);
        assert_eq!(
            actual,
            Liveness::Live,
            "now - updated_at = {} and stale_after = {}; equality must stay Live",
            600 - 500,
            100
        );
    }

    #[test]
    fn heartbeat_one_millisecond_past_the_threshold_is_stale() {
        let mut session = session(10, "s", "/tmp", None, "interactive");
        session.updated_at = Some(500);
        let actual = liveness(&session, 601, 100, &all_alive);
        assert_eq!(
            actual,
            Liveness::StaleHeartbeat,
            "now - updated_at = {} and stale_after = {}; strictly greater must be stale",
            601 - 500,
            100
        );
    }

    #[test]
    fn missing_heartbeat_never_counts_as_stale() {
        let session = session(10, "s", "/tmp", None, "interactive");
        let actual = liveness(&session, i64::MAX / 2, 1, &all_alive);
        assert_eq!(
            actual,
            Liveness::Live,
            "updated_at is {:?}, so rule 3 must be skipped entirely",
            session.updated_at
        );
    }

    #[test]
    fn heartbeat_from_the_future_is_live_not_stale() {
        let mut session = session(10, "s", "/tmp", None, "interactive");
        session.updated_at = Some(10_000);
        let actual = liveness(&session, 1_000, 100, &all_alive);
        assert_eq!(
            actual,
            Liveness::Live,
            "clock skew gives now - updated_at = {}, which is not > {}",
            1_000 - 10_000,
            100
        );
    }

    #[test]
    fn zero_stale_after_still_treats_a_same_millisecond_heartbeat_as_live() {
        let mut session = session(10, "s", "/tmp", None, "interactive");
        session.updated_at = Some(1_000);
        let actual = liveness(&session, 1_000, 0, &all_alive);
        assert_eq!(
            actual,
            Liveness::Live,
            "now - updated_at = 0 which is not > 0; session {:?}",
            session.session_id
        );
    }

    #[test]
    fn liveness_passes_the_sessions_own_pid_to_the_lookup() {
        let session = session(4242, "s", "/tmp", None, "interactive");
        let observed = std::cell::RefCell::new(Vec::new());
        let lookup = |process_id: u32| {
            observed.borrow_mut().push(process_id);
            Some("start-of-pid".to_string())
        };
        let actual = liveness(&session, 1_000, 100, &lookup);
        assert_eq!(actual, Liveness::Live, "sanity: session should be live");
        assert_eq!(
            observed.into_inner(),
            vec![4242],
            "lookup must be called with the session's own process_id"
        );
    }

    // ---------- visible_sessions ----------

    #[test]
    fn visible_sessions_keeps_only_interactive_kinds() {
        let sessions = vec![
            session(1, "interactive-one", "/elsewhere", Some("a"), "interactive"),
            session(2, "print-one", "/elsewhere", Some("b"), "print"),
            session(3, "sdk-one", "/elsewhere", Some("c"), "sdk"),
            session(4, "empty-kind", "/elsewhere", Some("d"), ""),
            session(5, "cased", "/elsewhere", Some("e"), "Interactive"),
        ];
        let visible = visible_sessions(sessions, None, 1_000, 100, &all_alive);
        assert_eq!(
            identifiers(&visible),
            vec!["interactive-one".to_string()],
            "kind must match \"interactive\" exactly"
        );
    }

    #[test]
    fn visible_sessions_drops_everything_that_is_not_live() {
        let mut stale = session(2, "stale", "/elsewhere", Some("b"), "interactive");
        stale.updated_at = Some(0);
        let sessions = vec![
            session(1, "live", "/elsewhere", Some("a"), "interactive"),
            stale,
            session(3, "gone", "/elsewhere", Some("c"), "interactive"),
            session(4, "reused", "/elsewhere", Some("d"), "interactive"),
        ];
        let lookup = |process_id: u32| match process_id {
            3 => None,
            4 => Some("some-other-start".to_string()),
            _ => Some("start-of-pid".to_string()),
        };
        let visible = visible_sessions(sessions, None, 1_000_000, 100, &lookup);
        assert_eq!(
            identifiers(&visible),
            vec!["live".to_string()],
            "stale, gone and pid-reused sessions must all be filtered out"
        );
    }

    #[test]
    fn own_project_sessions_sort_before_foreign_ones_then_by_name() {
        let sessions = vec![
            session(1, "foreign-a", "/other", Some("a"), "interactive"),
            session(2, "own-b", "/root/sub/deeper", Some("b"), "interactive"),
            session(3, "foreign-unnamed", "/other/two", None, "interactive"),
            session(4, "own-a", "/root", Some("a"), "interactive"),
        ];
        let root = PathBuf::from("/root");
        let visible = visible_sessions(sessions, Some(root.as_path()), 1_000, 100, &all_alive);
        assert_eq!(
            identifiers(&visible),
            vec![
                "own-a".to_string(),
                "own-b".to_string(),
                "foreign-unnamed".to_string(),
                "foreign-a".to_string(),
            ],
            "own project first, then name ascending with None treated as the empty string"
        );
    }

    #[test]
    fn a_none_project_root_makes_every_session_foreign_and_sorts_by_name() {
        let sessions = vec![
            session(1, "second", "/root", Some("b"), "interactive"),
            session(2, "first", "/root", Some("a"), "interactive"),
            session(3, "zeroth", "/root", None, "interactive"),
        ];
        let visible = visible_sessions(sessions, None, 1_000, 100, &all_alive);
        assert_eq!(
            identifiers(&visible),
            vec![
                "zeroth".to_string(),
                "first".to_string(),
                "second".to_string(),
            ],
            "with no project root every session is foreign, so only the name orders them"
        );
    }

    #[test]
    fn a_project_root_below_the_working_directory_is_not_the_same_project() {
        // Reverse prefix: the session's cwd is the *parent* of the project root.
        let sessions = vec![
            session(1, "parent-cwd", "/root", Some("b"), "interactive"),
            session(2, "inside-cwd", "/root/sub/x", Some("c"), "interactive"),
        ];
        let root = PathBuf::from("/root/sub");
        let visible = visible_sessions(sessions, Some(root.as_path()), 1_000, 100, &all_alive);
        assert_eq!(
            identifiers(&visible),
            vec!["inside-cwd".to_string(), "parent-cwd".to_string()],
            "only the session inside the root belongs to the project, so it sorts first \
             even though its name {:?} is after {:?}",
            "c",
            "b"
        );
    }

    #[test]
    fn prefix_matching_is_by_path_component_not_by_string() {
        let sessions = vec![
            session(1, "sibling", "/root-other/x", Some("a"), "interactive"),
            session(2, "genuine", "/root/x", Some("z"), "interactive"),
        ];
        let root = PathBuf::from("/root");
        let visible = visible_sessions(sessions, Some(root.as_path()), 1_000, 100, &all_alive);
        assert_eq!(
            identifiers(&visible),
            vec!["genuine".to_string(), "sibling".to_string()],
            "/root-other/x must not count as being under /root even though the string \
             \"/root\" is a prefix of it"
        );
    }

    #[test]
    fn working_directory_equal_to_the_root_belongs_to_the_project() {
        let sessions = vec![
            session(1, "foreign", "/other", Some("a"), "interactive"),
            session(2, "exact", "/root", Some("z"), "interactive"),
        ];
        let root = PathBuf::from("/root");
        let visible = visible_sessions(sessions, Some(root.as_path()), 1_000, 100, &all_alive);
        assert_eq!(
            identifiers(&visible),
            vec!["exact".to_string(), "foreign".to_string()],
            "an exact match counts as belonging to the project"
        );
    }

    #[test]
    fn identical_names_in_the_same_project_do_not_panic_and_keep_both_sessions() {
        let sessions = vec![
            session(1, "first", "/root", Some("same"), "interactive"),
            session(2, "second", "/root/sub", Some("same"), "interactive"),
        ];
        let root = PathBuf::from("/root");
        let visible = visible_sessions(sessions, Some(root.as_path()), 1_000, 100, &all_alive);
        // The spec does not fix the tie order, so only the multiset is asserted here.
        let mut actual = identifiers(&visible);
        actual.sort();
        assert_eq!(
            actual,
            vec!["first".to_string(), "second".to_string()],
            "both equally named sessions must survive sorting"
        );
    }

    #[test]
    fn duplicate_none_names_do_not_panic() {
        let sessions = vec![
            session(1, "first", "/other", None, "interactive"),
            session(2, "second", "/other", None, "interactive"),
        ];
        let visible = visible_sessions(sessions, None, 1_000, 100, &all_alive);
        assert_eq!(
            visible.len(),
            2,
            "two unnamed sessions must both survive; got {:?}",
            identifiers(&visible)
        );
    }

    #[test]
    fn an_empty_input_yields_an_empty_result() {
        let visible = visible_sessions(Vec::new(), None, 1_000, 100, &all_alive);
        assert_eq!(
            identifiers(&visible),
            Vec::<String>::new(),
            "no sessions in, no sessions out"
        );
    }

    #[test]
    fn name_ordering_is_plain_string_ordering() {
        let sessions = vec![
            session(1, "upper", "/other", Some("Zed"), "interactive"),
            session(2, "lower", "/other", Some("alpha"), "interactive"),
        ];
        let visible = visible_sessions(sessions, None, 1_000, 100, &all_alive);
        assert_eq!(
            identifiers(&visible),
            vec!["upper".to_string(), "lower".to_string()],
            "byte-wise string ordering puts {:?} before {:?}",
            "Zed",
            "alpha"
        );
    }
}
