//! Blind tests: written from the written specification of the subagent
//! metadata parser, the transcript path builder, and the workflow run id
//! scraper, without reading their implementations.

#[cfg(test)]
mod blind_subagent_tests {
    use remote::claude_sessions::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    const REAL_SESSION_ID: &str = "9f1c2d3e-4a5b-6c7d-8e9f-0a1b2c3d4e5f";
    const REAL_AGENT_ID: &str = "af090e203ec41bc73";
    const REAL_RUN_ID: &str = "wf_b529a29d-562";

    /// Every id in the spec's rejection list, plus the reason the caller has
    /// for refusing it.
    const IDS_THAT_ARE_NOT_A_SINGLE_PATH_COMPONENT: &[(&str, &str)] = &[
        ("..", "the parent directory escapes the enclosing directory"),
        (
            "../../etc/passwd",
            "a traversal chain reaches outside the projects directory entirely",
        ),
        ("a/b", "a separator makes it more than one component"),
        ("", "an empty id names no component at all"),
        (".", "a dot-only id names the enclosing directory"),
    ];

    /// A throwaway `$HOME` populated with one plausible project slug so that a
    /// path builder which looks at the disk can find the session, and removed
    /// when the test ends.
    struct TemporaryHome {
        path: PathBuf,
    }

    impl TemporaryHome {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "zed-blind-subagent-{label}-{}-{nanos}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let home = TemporaryHome { path };
            home.populate();
            home
        }

        fn populate(&self) {
            let session_directory = self
                .path
                .join(".claude")
                .join("projects")
                .join("-Users-someone-src-project")
                .join(REAL_SESSION_ID);
            let subagents = session_directory.join("subagents");
            let workflow = subagents.join("workflows").join(REAL_RUN_ID);
            for directory in [&subagents, &workflow] {
                fs::create_dir_all(directory)
                    .unwrap_or_else(|error| panic!("could not create {directory:?}: {error}"));
            }
            for directory in [&subagents, &workflow] {
                for extension in ["jsonl", "meta.json"] {
                    let file = directory.join(format!("agent-{REAL_AGENT_ID}.{extension}"));
                    fs::write(&file, "")
                        .unwrap_or_else(|error| panic!("could not write {file:?}: {error}"));
                }
            }
            // A secret that lives outside the projects directory, so that a
            // rejected id is rejected on purpose rather than because there was
            // nothing there to reach.
            let sessions = self.path.join(".claude").join("sessions");
            fs::create_dir_all(&sessions)
                .unwrap_or_else(|error| panic!("could not create {sessions:?}: {error}"));
            let key = sessions.join("agent-key.jsonl");
            fs::write(&key, "secret")
                .unwrap_or_else(|error| panic!("could not write {key:?}: {error}"));
        }

        fn projects_directory(&self) -> PathBuf {
            self.path.join(".claude").join("projects")
        }
    }

    impl Drop for TemporaryHome {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.path).ok();
        }
    }

    fn components(path: &Path) -> Vec<String> {
        path.components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_real_general_purpose_meta_sample_keeps_every_field_it_carries() {
        let contents = r#"{"agentType":"general-purpose","description":"Phase B adversarial review round 2","toolUseId":"toolu_01BEgVRSnksoz6YAyUWEEdeQ","spawnDepth":1,"requestShape":"background","requestNonInteractive":true,"model":"opus"}"#;

        let meta = parse_subagent_meta(contents)
            .unwrap_or_else(|error| panic!("the real sample should parse, but failed: {error}"));

        assert_eq!(
            meta.agent_type, "general-purpose",
            "agentType should be read verbatim, got {:?}",
            meta.agent_type
        );
        assert_eq!(
            meta.description.as_deref(),
            Some("Phase B adversarial review round 2"),
            "description should be Some(\"Phase B adversarial review round 2\"), got {:?}",
            meta.description
        );
        assert_eq!(
            meta.tool_use_id.as_deref(),
            Some("toolu_01BEgVRSnksoz6YAyUWEEdeQ"),
            "toolUseId should be Some(\"toolu_01BEgVRSnksoz6YAyUWEEdeQ\"), got {:?}",
            meta.tool_use_id
        );
        assert_eq!(
            meta.spawn_depth, 1,
            "spawnDepth should be 1, got {}",
            meta.spawn_depth
        );
        assert_eq!(
            meta.model.as_deref(),
            Some("opus"),
            "model should be Some(\"opus\"), got {:?}",
            meta.model
        );
        assert_eq!(
            meta.workflow_phase, None,
            "this sample carries no workflowPhase, so it should be None, got {:?}",
            meta.workflow_phase
        );
    }

    #[test]
    fn a_real_workflow_meta_sample_keeps_its_workflow_phase_and_leaves_tool_use_id_empty() {
        let contents = r#"{"agentType":"workflow-subagent","description":"R2:send-atomicity","workflowPhase":"Wave 5","spawnDepth":1,"requestShape":"foreground","requestNonInteractive":false,"model":"opus"}"#;

        let meta = parse_subagent_meta(contents).unwrap_or_else(|error| {
            panic!("the real workflow sample should parse, but failed: {error}")
        });

        assert_eq!(
            meta.agent_type, "workflow-subagent",
            "agentType should be \"workflow-subagent\", got {:?}",
            meta.agent_type
        );
        assert_eq!(
            meta.workflow_phase.as_deref(),
            Some("Wave 5"),
            "workflowPhase should be Some(\"Wave 5\"), got {:?}",
            meta.workflow_phase
        );
        assert_eq!(
            meta.tool_use_id, None,
            "this sample carries no toolUseId, so it should be None, got {:?}",
            meta.tool_use_id
        );
        assert_eq!(
            meta.description.as_deref(),
            Some("R2:send-atomicity"),
            "description should be Some(\"R2:send-atomicity\"), got {:?}",
            meta.description
        );
    }

    #[test]
    fn meta_without_an_agent_type_is_rejected() {
        let contents = r#"{"description":"no type here","spawnDepth":2,"model":"opus"}"#;

        let result = parse_subagent_meta(contents);

        assert!(
            result.is_err(),
            "agentType is required, so this should be Err, but it parsed as {:?}",
            result.ok().map(|meta| meta.agent_type)
        );
    }

    #[test]
    fn fields_the_parser_has_never_heard_of_are_ignored_instead_of_failing() {
        let contents = r#"{"agentType":"fork","somethingAddedNextWeek":{"nested":[1,2,3]},"anotherNewField":"whatever"}"#;

        let meta = parse_subagent_meta(contents).unwrap_or_else(|error| {
            panic!("unknown fields must not make parsing fail, but it failed: {error}")
        });

        assert_eq!(
            meta.agent_type, "fork",
            "agentType should still be \"fork\", got {:?}",
            meta.agent_type
        );
    }

    #[test]
    fn an_absent_spawn_depth_is_treated_as_one() {
        let contents = r#"{"agentType":"Explore"}"#;

        let meta = parse_subagent_meta(contents)
            .unwrap_or_else(|error| panic!("this should parse, but failed: {error}"));

        assert_eq!(
            meta.spawn_depth, 1,
            "an absent spawnDepth should default to 1, got {}",
            meta.spawn_depth
        );
    }

    #[test]
    fn every_optional_field_absent_becomes_none() {
        let contents = r#"{"agentType":"claude"}"#;

        let meta = parse_subagent_meta(contents)
            .unwrap_or_else(|error| panic!("this should parse, but failed: {error}"));

        assert_eq!(
            (
                meta.description.as_deref(),
                meta.tool_use_id.as_deref(),
                meta.model.as_deref(),
                meta.workflow_phase.as_deref(),
            ),
            (None, None, None, None),
            "description/toolUseId/model/workflowPhase should all be None when absent, got \
             description={:?} toolUseId={:?} model={:?} workflowPhase={:?}",
            meta.description,
            meta.tool_use_id,
            meta.model,
            meta.workflow_phase
        );
    }

    #[test]
    fn an_agent_type_outside_the_names_seen_so_far_is_still_accepted() {
        let contents = r#"{"agentType":"my-own-reviewer-9000"}"#;

        let meta = parse_subagent_meta(contents).unwrap_or_else(|error| {
            panic!("agentType is a free string, so this should parse, but failed: {error}")
        });

        assert_eq!(
            meta.agent_type, "my-own-reviewer-9000",
            "a user-defined agentType should be kept verbatim, got {:?}",
            meta.agent_type
        );
    }

    #[test]
    fn a_transcript_path_without_a_workflow_run_id_uses_the_flat_subagents_directory() {
        let home = TemporaryHome::new("layout-a");

        let path = subagent_transcript_path(&home.path, REAL_SESSION_ID, REAL_AGENT_ID, None)
            .unwrap_or_else(|| {
                panic!(
                    "real ids must be accepted, but got None for session {REAL_SESSION_ID} \
                     agent {REAL_AGENT_ID} under {:?}",
                    home.path
                )
            });

        let names = components(&path);
        assert!(
            path.starts_with(home.projects_directory()),
            "the path should live under {:?}, got {path:?}",
            home.projects_directory()
        );
        assert_eq!(
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            Some(format!("agent-{REAL_AGENT_ID}.jsonl")),
            "the file name should be agent-{REAL_AGENT_ID}.jsonl, got {path:?}"
        );
        assert_eq!(
            names.iter().rev().nth(1).map(String::as_str),
            Some("subagents"),
            "without a run id the transcript sits directly in `subagents`, got {path:?}"
        );
        assert!(
            names.iter().any(|name| name == REAL_SESSION_ID),
            "the session id {REAL_SESSION_ID} should appear in the path, got {path:?}"
        );
        assert!(
            !names.iter().any(|name| name == "workflows"),
            "no run id was given, so there should be no `workflows` segment, got {path:?}"
        );
    }

    #[test]
    fn a_transcript_path_with_a_workflow_run_id_uses_the_workflows_run_directory() {
        let home = TemporaryHome::new("layout-b");

        let path = subagent_transcript_path(
            &home.path,
            REAL_SESSION_ID,
            REAL_AGENT_ID,
            Some(REAL_RUN_ID),
        )
        .unwrap_or_else(|| {
            panic!(
                "real ids plus the real run id {REAL_RUN_ID} must be accepted, but got None \
                 under {:?}",
                home.path
            )
        });

        let names = components(&path);
        assert!(
            path.starts_with(home.projects_directory()),
            "the path should live under {:?}, got {path:?}",
            home.projects_directory()
        );
        assert_eq!(
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            Some(format!("agent-{REAL_AGENT_ID}.jsonl")),
            "the file name should be agent-{REAL_AGENT_ID}.jsonl, got {path:?}"
        );
        assert_eq!(
            names
                .iter()
                .rev()
                .skip(1)
                .take(3)
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec![REAL_RUN_ID, "workflows", "subagents"],
            "the three directories above the file should be subagents/workflows/{REAL_RUN_ID}, \
             got {path:?}"
        );
        assert!(
            names.iter().any(|name| name == REAL_SESSION_ID),
            "the session id {REAL_SESSION_ID} should appear in the path, got {path:?}"
        );
    }

    #[test]
    fn a_session_id_that_is_not_a_single_path_component_is_rejected() {
        let home = TemporaryHome::new("bad-session");

        for (session_id, reason) in IDS_THAT_ARE_NOT_A_SINGLE_PATH_COMPONENT {
            let path = subagent_transcript_path(&home.path, session_id, REAL_AGENT_ID, None);
            assert!(
                path.is_none(),
                "session id {session_id:?} should be rejected ({reason}), but it produced {:?}",
                path
            );
        }
    }

    #[test]
    fn an_agent_id_that_is_not_a_single_path_component_is_rejected() {
        let home = TemporaryHome::new("bad-agent");

        for (agent_id, reason) in IDS_THAT_ARE_NOT_A_SINGLE_PATH_COMPONENT {
            let path = subagent_transcript_path(&home.path, REAL_SESSION_ID, agent_id, None);
            assert!(
                path.is_none(),
                "agent id {agent_id:?} should be rejected ({reason}), but it produced {:?}",
                path
            );
        }
    }

    #[test]
    fn a_workflow_run_id_that_is_not_a_single_path_component_is_rejected() {
        let home = TemporaryHome::new("bad-run");

        for (run_id, reason) in IDS_THAT_ARE_NOT_A_SINGLE_PATH_COMPONENT {
            let path =
                subagent_transcript_path(&home.path, REAL_SESSION_ID, REAL_AGENT_ID, Some(run_id));
            assert!(
                path.is_none(),
                "workflow run id {run_id:?} should be rejected ({reason}), but it produced {:?}",
                path
            );
        }
    }

    #[test]
    fn an_agent_id_that_climbs_out_of_the_projects_directory_cannot_reach_the_sessions_keys() {
        let home = TemporaryHome::new("key-escape");
        let key = home
            .path
            .join(".claude")
            .join("sessions")
            .join("agent-key.jsonl");
        assert!(
            key.exists(),
            "the fixture key file {key:?} should exist, so that rejection is what stops the \
             traversal rather than the file being absent"
        );

        for agent_id in [
            "../../../sessions/key",
            "../../sessions/key",
            "..%2F..%2Fsessions/key",
            "../key",
        ] {
            let path = subagent_transcript_path(&home.path, REAL_SESSION_ID, agent_id, None);
            assert!(
                path.is_none(),
                "agent id {agent_id:?} must not be turned into a path, but it produced {:?}",
                path
            );
        }
    }

    #[test]
    fn a_session_id_that_climbs_out_of_the_projects_directory_cannot_reach_the_sessions_keys() {
        let home = TemporaryHome::new("session-escape");

        for session_id in ["../sessions", "../../.claude/sessions", "..", "./."] {
            let path = subagent_transcript_path(&home.path, session_id, REAL_AGENT_ID, None);
            assert!(
                path.is_none(),
                "session id {session_id:?} must not be turned into a path, but it produced {:?}",
                path
            );
        }
    }

    #[test]
    fn the_run_id_line_of_a_real_workflow_tool_result_is_extracted() {
        let text = concat!(
            "Workflow started.\n",
            "Run ID: wf_b529a29d-562\n",
            "Watch it with /tasks.\n",
        );

        let run_id = workflow_run_id_in_tool_result(text);

        assert_eq!(
            run_id.as_deref(),
            Some("wf_b529a29d-562"),
            "the run id should be extracted as \"wf_b529a29d-562\", got {run_id:?}"
        );
    }

    #[test]
    fn text_carrying_no_run_id_yields_nothing() {
        let text = "Workflow finished with 3 agents. No identifiers here, just prose.";

        let run_id = workflow_run_id_in_tool_result(text);

        assert_eq!(
            run_id, None,
            "there is no run id in this text, so the result should be None, got {run_id:?}"
        );
    }

    #[test]
    fn the_first_run_id_wins_when_the_text_carries_several() {
        let text = concat!(
            "Run ID: wf_b529a29d-562\n",
            "Resumed from Run ID: wf_0000aaaa-111\n",
        );

        let run_id = workflow_run_id_in_tool_result(text);

        assert_eq!(
            run_id.as_deref(),
            Some("wf_b529a29d-562"),
            "the first run id in the text should win, expected \"wf_b529a29d-562\", got {run_id:?}"
        );
    }

    #[test]
    fn an_extracted_run_id_is_never_something_that_could_escape_a_directory() {
        let texts = [
            "Run ID: ../../etc/passwd\n",
            "Run ID: ..\n",
            "Run ID: wf_../../etc/passwd\n",
            "Run ID: wf_a/b\n",
            "Run ID:  \n",
            "Run ID: \n",
            "Run ID: wf_ok-1 and then more words\n",
        ];

        for text in texts {
            if let Some(run_id) = workflow_run_id_in_tool_result(text) {
                assert!(
                    !run_id.contains('/')
                        && !run_id.contains('\\')
                        && !run_id.contains("..")
                        && !run_id.is_empty()
                        && !run_id.chars().any(char::is_whitespace),
                    "an extracted run id is used as a path segment, so it must contain no \
                     separator, no `..`, no whitespace and not be empty, but {text:?} yielded \
                     {run_id:?}"
                );
            }
        }
    }

    #[test]
    fn a_run_id_extracted_from_a_real_tool_result_is_accepted_as_a_path_segment() {
        let home = TemporaryHome::new("round-trip");
        let text = "Workflow launched.\nRun ID: wf_b529a29d-562\n";

        let run_id = workflow_run_id_in_tool_result(text)
            .unwrap_or_else(|| panic!("the real sample {text:?} should yield a run id"));
        let path = subagent_transcript_path(
            &home.path,
            REAL_SESSION_ID,
            REAL_AGENT_ID,
            Some(run_id.as_str()),
        );

        assert!(
            path.is_some(),
            "a run id scraped from a real tool result must survive the path sanitizer, but \
             {run_id:?} produced None"
        );
    }
}
