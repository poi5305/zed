#[cfg(test)]
mod blind_live_state_tests {
    use crate::live_state::*;
    use serde_json::json;

    // ---------- helpers ----------

    /// The payload Claude Code hands a hook: always `hook_event_name` + `session_id`, plus
    /// whatever the individual event carries.
    fn payload(name: &str, fields: serde_json::Value) -> serde_json::Value {
        let mut object = serde_json::Map::new();
        object.insert("hook_event_name".to_string(), json!(name));
        object.insert("session_id".to_string(), json!("s1"));
        object.insert("cwd".to_string(), json!("/work"));
        object.insert(
            "transcript_path".to_string(),
            json!("/work/transcript.jsonl"),
        );
        if let serde_json::Value::Object(fields) = fields {
            for (key, value) in fields {
                object.insert(key, value);
            }
        }
        serde_json::Value::Object(object)
    }

    fn parse_or_panic(line: &str) -> HookEvent {
        match parse_hook_event(line) {
            Some(event) => event,
            None => {
                panic!("expected the dispatcher line {line} to parse into a HookEvent, got None")
            }
        }
    }

    /// An event line exactly as the dispatcher writes it, with no `permission_mode` in the payload.
    fn hook_without_mode(name: &str, received_at_ms: i64, fields: serde_json::Value) -> HookEvent {
        let line = json!({
            "received_at_ms": received_at_ms,
            "event": payload(name, fields),
        })
        .to_string();
        parse_or_panic(&line)
    }

    /// The same, with the `permission_mode: "auto"` every real payload carries.
    fn hook(name: &str, received_at_ms: i64, fields: serde_json::Value) -> HookEvent {
        let mut object = serde_json::Map::new();
        object.insert("permission_mode".to_string(), json!("auto"));
        if let serde_json::Value::Object(fields) = fields {
            for (key, value) in fields {
                object.insert(key, value);
            }
        }
        hook_without_mode(name, received_at_ms, serde_json::Value::Object(object))
    }

    fn pre_tool_use(received_at_ms: i64, tool_use_id: &str, tool_name: &str) -> HookEvent {
        hook(
            "PreToolUse",
            received_at_ms,
            json!({
                "tool_use_id": tool_use_id,
                "tool_name": tool_name,
                "tool_input": { "command": "ls" },
            }),
        )
    }

    fn ask_user_question(received_at_ms: i64, tool_use_id: &str) -> HookEvent {
        hook(
            "PreToolUse",
            received_at_ms,
            json!({
                "tool_use_id": tool_use_id,
                "tool_name": "AskUserQuestion",
                "tool_input": { "questions": [{ "question": "ship it?", "options": ["yes", "no"] }] },
            }),
        )
    }

    fn permission_request(received_at_ms: i64, tool_use_id: &str, tool_name: &str) -> HookEvent {
        hook(
            "PermissionRequest",
            received_at_ms,
            json!({
                "tool_use_id": tool_use_id,
                "tool_name": tool_name,
                "tool_input": { "command": "rm -rf /tmp/x" },
            }),
        )
    }

    fn message_display(received_at_ms: i64, fields: serde_json::Value) -> HookEvent {
        hook("MessageDisplay", received_at_ms, fields)
    }

    // `Turn` may not derive Debug or PartialEq, so describe it by hand.
    fn describe_turn(turn: &Turn) -> String {
        match turn {
            Turn::Idle => "Idle".to_string(),
            Turn::Running { since_ms } => format!("Running {{ since_ms: {since_ms} }}"),
        }
    }

    fn describe_live_message(message: &Option<LiveMessage>) -> String {
        match message {
            None => "None".to_string(),
            Some(message) => format!(
                "Some {{ text: {:?}, message_id: {:?}, turn_id: {:?}, is_final: {}, updated_at_ms: {} }}",
                message.text,
                message.message_id,
                message.turn_id,
                message.is_final,
                message.updated_at_ms
            ),
        }
    }

    fn describe_tools(tools: &[RunningTool]) -> String {
        tools
            .iter()
            .map(|tool| {
                format!(
                    "{{ tool_use_id: {}, name: {}, input: {}, started_at_ms: {} }}",
                    tool.tool_use_id, tool.name, tool.input, tool.started_at_ms
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn describe_permission(request: &Option<PermissionRequest>) -> String {
        match request {
            None => "None".to_string(),
            Some(request) => format!(
                "Some {{ tool_use_id: {}, tool_name: {}, tool_input: {}, since_ms: {} }}",
                request.tool_use_id, request.tool_name, request.tool_input, request.since_ms
            ),
        }
    }

    fn describe_question(question: &Option<PendingQuestion>) -> String {
        match question {
            None => "None".to_string(),
            Some(question) => format!(
                "Some {{ tool_use_id: {}, questions: {}, since_ms: {} }}",
                question.tool_use_id, question.questions, question.since_ms
            ),
        }
    }

    /// Everything except `last_event_at_ms`, which rule 1 always moves.
    fn describe_state(state: &LiveState) -> String {
        format!(
            "live_message={} running_tools=[{}] pending_permission={} pending_question={} \
             permission_mode={:?} turn={} compacting={} last_notification={:?} session_ended={}",
            describe_live_message(&state.live_message),
            describe_tools(&state.running_tools),
            describe_permission(&state.pending_permission),
            describe_question(&state.pending_question),
            state.permission_mode,
            describe_turn(&state.turn),
            state.compacting,
            state.last_notification,
            state.session_ended,
        )
    }

    fn live_message_of(state: &LiveState) -> &LiveMessage {
        match state.live_message.as_ref() {
            Some(message) => message,
            None => panic!(
                "expected a live_message, got None; state was {}",
                describe_state(state)
            ),
        }
    }

    /// A state carrying one of everything, so that "clears X" rules have something to clear.
    fn busy_state() -> LiveState {
        let mut state = LiveState::default();
        state.apply(&hook("UserPromptSubmit", 100, json!({ "prompt": "hi" })));
        state.apply(&message_display(
            110,
            json!({ "index": 0, "delta": "wor", "message_id": "m1", "turn_id": "t1", "final": false }),
        ));
        state.apply(&pre_tool_use(120, "tu1", "Bash"));
        state.apply(&ask_user_question(130, "tu2"));
        state.apply(&permission_request(140, "tu1", "Bash"));
        state
    }

    fn parse_status_or_panic(json_text: &str) -> StatusSnapshot {
        match StatusSnapshot::parse(json_text) {
            Some(snapshot) => snapshot,
            None => panic!("expected {json_text} to parse into a StatusSnapshot, got None"),
        }
    }

    fn assert_close(actual: Option<f64>, expected: f64, what: &str) {
        match actual {
            Some(value) if (value - expected).abs() < 1e-9 => {}
            other => panic!("{what}: expected Some({expected}), got {other:?}"),
        }
    }

    // ---------- parse_hook_event ----------

    #[test]
    fn p1_a_blank_line_is_not_an_event() {
        for line in ["", "   ", "\t", "\n", " \r\n"] {
            let parsed = parse_hook_event(line);
            assert!(
                parsed.is_none(),
                "expected None for the blank line {:?}, got an event named {:?}",
                line,
                parsed.map(|event| event.name)
            );
        }
    }

    #[test]
    fn p2_a_non_object_line_is_not_an_event() {
        for line in ["[1]", "\"x\"", "12", "null", "true"] {
            let parsed = parse_hook_event(line);
            assert!(
                parsed.is_none(),
                "expected None for the non-object line {}, got an event named {:?}",
                line,
                parsed.map(|event| event.name)
            );
        }
    }

    #[test]
    fn p3_a_line_without_a_hook_event_name_is_not_an_event() {
        let without_name =
            json!({ "received_at_ms": 7, "event": { "session_id": "s1" } }).to_string();
        let parsed = parse_hook_event(&without_name);
        assert!(
            parsed.is_none(),
            "expected None when event.hook_event_name is missing, got an event named {:?}",
            parsed.map(|event| event.name)
        );

        let without_event = json!({ "received_at_ms": 7 }).to_string();
        let parsed = parse_hook_event(&without_event);
        assert!(
            parsed.is_none(),
            "expected None when there is no inner event object at all, got an event named {:?}",
            parsed.map(|event| event.name)
        );
    }

    #[test]
    fn p4_a_line_without_a_session_id_is_not_an_event() {
        let line =
            json!({ "received_at_ms": 7, "event": { "hook_event_name": "Stop" } }).to_string();
        let parsed = parse_hook_event(&line);
        assert!(
            parsed.is_none(),
            "expected None when event.session_id is missing, got an event with session_id {:?}",
            parsed.map(|event| event.session_id)
        );
    }

    #[test]
    fn p5_a_line_without_received_at_ms_parses_with_a_zero_clock() {
        let line =
            json!({ "event": { "hook_event_name": "Stop", "session_id": "s1" } }).to_string();
        let parsed = parse_or_panic(&line);
        assert_eq!(
            parsed.received_at_ms, 0,
            "a missing received_at_ms must become 0, got {}",
            parsed.received_at_ms
        );
        assert_eq!(parsed.name, "Stop", "the event name must still parse");
        assert_eq!(parsed.session_id, "s1", "the session id must still parse");
    }

    #[test]
    fn p6_a_full_pre_tool_use_line_fills_every_field_and_keeps_the_payload_verbatim() {
        let event_object = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s1",
            "permission_mode": "auto",
            "cwd": "/work",
            "transcript_path": "/work/transcript.jsonl",
            "tool_use_id": "toolu_01",
            "tool_name": "Bash",
            "tool_input": { "command": "ls -l", "description": "list" },
            "some_future_field": { "nested": [1, 2, 3] }
        });
        let line =
            json!({ "received_at_ms": 1_700_000_000_123i64, "event": event_object }).to_string();
        let parsed = parse_or_panic(&line);

        assert_eq!(
            parsed.received_at_ms, 1_700_000_000_123i64,
            "received_at_ms must come from the wrapper, got {}",
            parsed.received_at_ms
        );
        assert_eq!(
            parsed.name, "PreToolUse",
            "name must come from hook_event_name"
        );
        assert_eq!(parsed.session_id, "s1", "session_id must be copied");
        assert_eq!(
            parsed.permission_mode,
            Some("auto".to_string()),
            "permission_mode must be copied, got {:?}",
            parsed.permission_mode
        );
        assert_eq!(
            parsed.tool_use_id,
            Some("toolu_01".to_string()),
            "tool_use_id must be copied, got {:?}",
            parsed.tool_use_id
        );
        assert_eq!(
            parsed.tool_name,
            Some("Bash".to_string()),
            "tool_name must be copied, got {:?}",
            parsed.tool_name
        );
        assert_eq!(
            parsed.tool_input,
            Some(json!({ "command": "ls -l", "description": "list" })),
            "tool_input must be copied verbatim, got {:?}",
            parsed.tool_input
        );
        assert_eq!(
            parsed.raw, event_object,
            "raw must be the inner event object exactly (including unknown fields), got {}",
            parsed.raw
        );
    }

    // ---------- LiveState::apply ----------

    #[test]
    fn r1_every_event_moves_the_clock_and_only_a_present_permission_mode_replaces_the_mode() {
        let mut state = LiveState::default();
        assert_eq!(
            state.last_event_at_ms, 0,
            "a fresh LiveState starts at clock 0, got {}",
            state.last_event_at_ms
        );
        assert_eq!(
            state.permission_mode, None,
            "a fresh LiveState has no permission mode, got {:?}",
            state.permission_mode
        );

        state.apply(&hook("SubagentStop", 100, json!({})));
        assert_eq!(
            state.last_event_at_ms, 100,
            "last_event_at_ms must follow received_at_ms, got {}",
            state.last_event_at_ms
        );
        assert_eq!(
            state.permission_mode,
            Some("auto".to_string()),
            "a present permission_mode must be copied, got {:?}",
            state.permission_mode
        );

        state.apply(&hook(
            "Notification",
            200,
            json!({ "permission_mode": "plan", "notification_type": "idle_prompt" }),
        ));
        assert_eq!(
            state.permission_mode,
            Some("plan".to_string()),
            "a later permission_mode must replace the earlier one, got {:?}",
            state.permission_mode
        );

        state.apply(&hook_without_mode("SubagentStop", 300, json!({})));
        assert_eq!(
            state.last_event_at_ms, 300,
            "an event without permission_mode must still move the clock, got {}",
            state.last_event_at_ms
        );
        assert_eq!(
            state.permission_mode,
            Some("plan".to_string()),
            "an event without permission_mode must leave the previous mode, got {:?}",
            state.permission_mode
        );
    }

    #[test]
    fn r2_user_prompt_submit_starts_a_turn_and_wipes_the_previous_one() {
        let mut state = LiveState::default();
        state.apply(&hook("SessionEnd", 10, json!({ "reason": "exit" })));
        assert!(
            state.session_ended,
            "precondition: SessionEnd must mark the session ended before this rule is exercised"
        );
        state.apply(&message_display(
            20,
            json!({ "index": 0, "delta": "stale", "message_id": "m0", "turn_id": "t0", "final": false }),
        ));
        state.apply(&pre_tool_use(30, "tu1", "Bash"));
        state.apply(&ask_user_question(40, "tu2"));
        state.apply(&permission_request(50, "tu1", "Bash"));

        state.apply(&hook(
            "UserPromptSubmit",
            500,
            json!({ "prompt": "next thing" }),
        ));

        assert!(
            matches!(state.turn, Turn::Running { since_ms } if since_ms == 500),
            "UserPromptSubmit must start Running {{ since_ms: 500 }}, got {}",
            describe_turn(&state.turn)
        );
        assert!(
            state.live_message.is_none(),
            "the previous turn's live_message must be cleared, got {}",
            describe_live_message(&state.live_message)
        );
        assert!(
            state.running_tools.is_empty(),
            "running_tools must be cleared, got [{}]",
            describe_tools(&state.running_tools)
        );
        assert!(
            state.pending_permission.is_none(),
            "pending_permission must be cleared, got {}",
            describe_permission(&state.pending_permission)
        );
        assert!(
            state.pending_question.is_none(),
            "pending_question must be cleared, got {}",
            describe_question(&state.pending_question)
        );
        assert!(
            !state.session_ended,
            "a new prompt means the session is alive again, got session_ended = {}",
            state.session_ended
        );
    }

    #[test]
    fn r3a_message_display_index_zero_starts_a_message() {
        let mut state = LiveState::default();
        state.apply(&message_display(
            1_000,
            json!({ "index": 0, "delta": "Hel", "message_id": "m1", "turn_id": "t1", "final": false }),
        ));

        let message = live_message_of(&state);
        assert_eq!(
            message.text, "Hel",
            "text must be the first delta, got {:?}",
            message.text
        );
        assert_eq!(
            message.message_id,
            Some("m1".to_string()),
            "message_id must be copied, got {:?}",
            message.message_id
        );
        assert_eq!(
            message.turn_id,
            Some("t1".to_string()),
            "turn_id must be copied, got {:?}",
            message.turn_id
        );
        assert!(
            !message.is_final,
            "final: false must leave is_final false, got {}",
            message.is_final
        );
        assert_eq!(
            message.updated_at_ms, 1_000,
            "updated_at_ms must be the event's received_at_ms, got {}",
            message.updated_at_ms
        );
    }

    #[test]
    fn r3b_message_display_index_one_with_the_same_message_id_appends() {
        let mut state = LiveState::default();
        state.apply(&message_display(
            1_000,
            json!({ "index": 0, "delta": "Hel", "message_id": "m1", "turn_id": "t1", "final": false }),
        ));
        state.apply(&message_display(
            1_050,
            json!({ "index": 1, "delta": "lo", "message_id": "m1", "turn_id": "t1", "final": false }),
        ));

        let message = live_message_of(&state);
        assert_eq!(
            message.text, "Hello",
            "index 1 with the same message_id must append, got {:?}",
            message.text
        );
        assert_eq!(
            message.message_id,
            Some("m1".to_string()),
            "message_id must stay the same, got {:?}",
            message.message_id
        );
        assert_eq!(
            message.updated_at_ms, 1_050,
            "updated_at_ms must follow the newest piece, got {}",
            message.updated_at_ms
        );
    }

    #[test]
    fn r3c_message_display_index_zero_with_a_new_message_id_replaces() {
        let mut state = LiveState::default();
        state.apply(&message_display(
            1_000,
            json!({ "index": 0, "delta": "Hello", "message_id": "m1", "turn_id": "t1", "final": true }),
        ));
        state.apply(&message_display(
            2_000,
            json!({ "index": 0, "delta": "Second", "message_id": "m2", "turn_id": "t1", "final": false }),
        ));

        let message = live_message_of(&state);
        assert_eq!(
            message.text, "Second",
            "a new message must replace the old text, not append to it, got {:?}",
            message.text
        );
        assert_eq!(
            message.message_id,
            Some("m2".to_string()),
            "message_id must be the new one, got {:?}",
            message.message_id
        );
        assert!(
            !message.is_final,
            "is_final must come from the new piece, got {}",
            message.is_final
        );
        assert_eq!(
            message.updated_at_ms, 2_000,
            "updated_at_ms must be the new piece's clock, got {}",
            message.updated_at_ms
        );
    }

    #[test]
    fn r3d_message_display_without_an_index_appends_when_the_message_id_matches() {
        let mut state = LiveState::default();
        state.apply(&message_display(
            1_000,
            json!({ "index": 0, "delta": "Hel", "message_id": "m1", "turn_id": "t1", "final": false }),
        ));
        state.apply(&message_display(
            1_050,
            json!({ "delta": "lo", "message_id": "m1", "turn_id": "t1", "final": false }),
        ));

        let message = live_message_of(&state);
        assert_eq!(
            message.text, "Hello",
            "a missing index is unknown, not 0: with the same message_id it must append, got {:?}",
            message.text
        );
        assert_eq!(
            message.updated_at_ms, 1_050,
            "updated_at_ms must follow the newest piece, got {}",
            message.updated_at_ms
        );
    }

    #[test]
    fn r3e_message_display_without_an_index_starts_new_when_the_message_id_differs() {
        let mut state = LiveState::default();
        state.apply(&message_display(
            1_000,
            json!({ "index": 0, "delta": "Hel", "message_id": "m1", "turn_id": "t1", "final": false }),
        ));
        state.apply(&message_display(
            1_050,
            json!({ "delta": "Other", "message_id": "m2", "turn_id": "t1", "final": false }),
        ));

        let message = live_message_of(&state);
        assert_eq!(
            message.text, "Other",
            "a different message_id must start a new message even with no index, got {:?}",
            message.text
        );
        assert_eq!(
            message.message_id,
            Some("m2".to_string()),
            "message_id must be the new one, got {:?}",
            message.message_id
        );
    }

    #[test]
    fn r3f_message_display_final_true_marks_the_message_final() {
        let mut state = LiveState::default();
        state.apply(&message_display(
            1_000,
            json!({ "index": 0, "delta": "Hel", "message_id": "m1", "turn_id": "t1", "final": false }),
        ));
        assert!(
            !live_message_of(&state).is_final,
            "precondition: the first piece is not final"
        );

        state.apply(&message_display(
            1_100,
            json!({ "index": 1, "delta": "lo", "message_id": "m1", "turn_id": "t1", "final": true }),
        ));

        let message = live_message_of(&state);
        assert!(
            message.is_final,
            "final: true must set is_final, got {} (text {:?})",
            message.is_final, message.text
        );
        assert_eq!(
            message.text, "Hello",
            "the final piece still appends its delta, got {:?}",
            message.text
        );
    }

    #[test]
    fn r4a_pre_tool_use_starts_a_running_tool() {
        let mut state = LiveState::default();
        state.apply(&hook(
            "PreToolUse",
            1_234,
            json!({
                "tool_use_id": "tu1",
                "tool_name": "Bash",
                "tool_input": { "command": "cargo test" },
            }),
        ));

        assert_eq!(
            state.running_tools.len(),
            1,
            "one PreToolUse must produce one running tool, got [{}]",
            describe_tools(&state.running_tools)
        );
        let tool = match state.running_tools.first() {
            Some(tool) => tool,
            None => panic!("expected a running tool"),
        };
        assert_eq!(
            tool.tool_use_id, "tu1",
            "tool_use_id must be copied, got {}",
            tool.tool_use_id
        );
        assert_eq!(
            tool.name, "Bash",
            "name must come from tool_name, got {}",
            tool.name
        );
        assert_eq!(
            tool.input,
            json!({ "command": "cargo test" }),
            "input must be the tool_input verbatim, got {}",
            tool.input
        );
        assert_eq!(
            tool.started_at_ms, 1_234,
            "started_at_ms must be the event's received_at_ms, got {}",
            tool.started_at_ms
        );
    }

    #[test]
    fn r4b_a_repeated_pre_tool_use_replaces_instead_of_duplicating() {
        let mut state = LiveState::default();
        state.apply(&hook(
            "PreToolUse",
            10,
            json!({ "tool_use_id": "tu1", "tool_name": "Bash", "tool_input": { "command": "first" } }),
        ));
        state.apply(&hook(
            "PreToolUse",
            20,
            json!({ "tool_use_id": "tu1", "tool_name": "Read", "tool_input": { "file_path": "/x" } }),
        ));

        assert_eq!(
            state.running_tools.len(),
            1,
            "the same tool_use_id must not be pushed twice, got [{}]",
            describe_tools(&state.running_tools)
        );
        let tool = match state.running_tools.first() {
            Some(tool) => tool,
            None => panic!("expected a running tool"),
        };
        assert_eq!(
            tool.name, "Read",
            "the newer payload must win, got name {}",
            tool.name
        );
        assert_eq!(
            tool.input,
            json!({ "file_path": "/x" }),
            "the newer input must win, got {}",
            tool.input
        );
    }

    #[test]
    fn r4c_pre_tool_use_for_ask_user_question_also_records_a_pending_question() {
        let mut state = LiveState::default();
        let questions = json!([{ "question": "ship it?", "options": ["yes", "no"] }]);
        state.apply(&hook(
            "PreToolUse",
            777,
            json!({
                "tool_use_id": "tu9",
                "tool_name": "AskUserQuestion",
                "tool_input": { "questions": questions },
            }),
        ));

        let question = match state.pending_question.as_ref() {
            Some(question) => question,
            None => panic!(
                "AskUserQuestion must set pending_question, got None; running tools were [{}]",
                describe_tools(&state.running_tools)
            ),
        };
        assert_eq!(
            question.tool_use_id, "tu9",
            "pending_question must carry the tool_use_id, got {}",
            question.tool_use_id
        );
        assert_eq!(
            question.questions, questions,
            "questions must be tool_input.questions verbatim, got {}",
            question.questions
        );
        assert_eq!(
            question.since_ms, 777,
            "since_ms must be the event's received_at_ms, got {}",
            question.since_ms
        );
        assert_eq!(
            state.running_tools.len(),
            1,
            "AskUserQuestion is still a running tool, got [{}]",
            describe_tools(&state.running_tools)
        );
    }

    #[test]
    fn r4d_pre_tool_use_with_the_permission_request_id_means_it_was_allowed() {
        let mut state = LiveState::default();
        state.apply(&permission_request(100, "tu1", "Bash"));
        assert!(
            state.pending_permission.is_some(),
            "precondition: PermissionRequest must set pending_permission"
        );

        state.apply(&pre_tool_use(110, "tu1", "Bash"));

        assert!(
            state.pending_permission.is_none(),
            "a PreToolUse with the same tool_use_id means the request was allowed, got {}",
            describe_permission(&state.pending_permission)
        );
    }

    #[test]
    fn r4e_pre_tool_use_with_a_different_id_leaves_the_pending_permission() {
        let mut state = LiveState::default();
        state.apply(&permission_request(100, "tu1", "Bash"));
        state.apply(&pre_tool_use(110, "tu2", "Read"));

        let request = match state.pending_permission.as_ref() {
            Some(request) => request,
            None => panic!("an unrelated PreToolUse must not clear pending_permission, got None"),
        };
        assert_eq!(
            request.tool_use_id, "tu1",
            "the still-pending request must be the original one, got {}",
            request.tool_use_id
        );
    }

    #[test]
    fn r5_permission_request_records_the_request_and_a_second_one_replaces_it() {
        let mut state = LiveState::default();
        state.apply(&hook(
            "PermissionRequest",
            300,
            json!({
                "tool_use_id": "tu1",
                "tool_name": "Bash",
                "tool_input": { "command": "rm -rf /tmp/x" },
            }),
        ));

        let request = match state.pending_permission.as_ref() {
            Some(request) => request,
            None => panic!("PermissionRequest must set pending_permission, got None"),
        };
        assert_eq!(
            request.tool_use_id, "tu1",
            "tool_use_id must be copied, got {}",
            request.tool_use_id
        );
        assert_eq!(
            request.tool_name, "Bash",
            "tool_name must be copied, got {}",
            request.tool_name
        );
        assert_eq!(
            request.tool_input,
            json!({ "command": "rm -rf /tmp/x" }),
            "tool_input must be copied verbatim, got {}",
            request.tool_input
        );
        assert_eq!(
            request.since_ms, 300,
            "since_ms must be the event's received_at_ms, got {}",
            request.since_ms
        );

        state.apply(&hook(
            "PermissionRequest",
            400,
            json!({
                "tool_use_id": "tu2",
                "tool_name": "WebFetch",
                "tool_input": { "url": "https://example.com" },
            }),
        ));

        let request = match state.pending_permission.as_ref() {
            Some(request) => request,
            None => panic!("the second PermissionRequest must still leave a pending_permission"),
        };
        assert_eq!(
            request.tool_use_id, "tu2",
            "a second request replaces the first, got {}",
            request.tool_use_id
        );
        assert_eq!(
            request.tool_name, "WebFetch",
            "tool_name must be the new one, got {}",
            request.tool_name
        );
        assert_eq!(
            request.since_ms, 400,
            "since_ms must be the new clock, got {}",
            request.since_ms
        );
    }

    #[test]
    fn r6_post_tool_use_ends_only_the_matching_tool_and_only_matching_pendings() {
        let mut state = LiveState::default();
        state.apply(&pre_tool_use(100, "tu1", "Bash"));
        state.apply(&ask_user_question(110, "tu2"));
        state.apply(&permission_request(120, "tu1", "Bash"));

        state.apply(&hook(
            "PostToolUse",
            130,
            json!({ "tool_use_id": "tu1", "tool_name": "Bash", "tool_response": { "stdout": "ok" } }),
        ));

        assert_eq!(
            state.running_tools.len(),
            1,
            "only the matching tool may be removed, got [{}]",
            describe_tools(&state.running_tools)
        );
        let remaining = match state.running_tools.first() {
            Some(tool) => tool,
            None => panic!("expected the unrelated tool to survive"),
        };
        assert_eq!(
            remaining.tool_use_id, "tu2",
            "the surviving tool must be the unrelated one, got {}",
            remaining.tool_use_id
        );
        assert!(
            state.pending_permission.is_none(),
            "a PostToolUse for the permission's tool_use_id must clear it, got {}",
            describe_permission(&state.pending_permission)
        );
        assert!(
            state.pending_question.is_some(),
            "a PostToolUse for a different id must leave pending_question, got {}",
            describe_question(&state.pending_question)
        );

        state.apply(&hook(
            "PostToolUse",
            140,
            json!({ "tool_use_id": "tu2", "tool_name": "AskUserQuestion", "tool_response": { "answer": "yes" } }),
        ));

        assert!(
            state.running_tools.is_empty(),
            "the second PostToolUse must end the last tool, got [{}]",
            describe_tools(&state.running_tools)
        );
        assert!(
            state.pending_question.is_none(),
            "a PostToolUse for the question's tool_use_id must clear it, got {}",
            describe_question(&state.pending_question)
        );
    }

    #[test]
    fn r7_permission_denied_ends_that_tool_and_clears_a_matching_permission() {
        let mut state = LiveState::default();
        state.apply(&pre_tool_use(100, "tu1", "Bash"));
        state.apply(&pre_tool_use(110, "tu2", "Read"));
        state.apply(&permission_request(120, "tu1", "Bash"));

        state.apply(&hook(
            "PermissionDenied",
            130,
            json!({ "tool_use_id": "tu1", "tool_name": "Bash", "message": "denied" }),
        ));

        assert_eq!(
            state.running_tools.len(),
            1,
            "only the denied tool may be removed, got [{}]",
            describe_tools(&state.running_tools)
        );
        let remaining = match state.running_tools.first() {
            Some(tool) => tool,
            None => panic!("expected the unrelated tool to survive"),
        };
        assert_eq!(
            remaining.tool_use_id, "tu2",
            "the surviving tool must be the unrelated one, got {}",
            remaining.tool_use_id
        );
        assert!(
            state.pending_permission.is_none(),
            "the matching pending_permission must be cleared, got {}",
            describe_permission(&state.pending_permission)
        );
    }

    #[test]
    fn r8_notification_is_recorded_but_never_creates_a_pending_permission() {
        let mut state = LiveState::default();
        state.apply(&hook(
            "Notification",
            600,
            json!({ "notification_type": "permission_prompt", "title": "Claude", "message": "needs permission" }),
        ));

        assert_eq!(
            state.last_notification,
            Some(("permission_prompt".to_string(), 600)),
            "the notification type and its clock must be recorded, got {:?}",
            state.last_notification
        );
        assert!(
            state.pending_permission.is_none(),
            "only PermissionRequest creates a pending_permission, got {}",
            describe_permission(&state.pending_permission)
        );

        state.apply(&hook(
            "Notification",
            700,
            json!({ "notification_type": "idle_prompt", "message": "waiting" }),
        ));
        assert_eq!(
            state.last_notification,
            Some(("idle_prompt".to_string(), 700)),
            "a later notification must replace the earlier one, got {:?}",
            state.last_notification
        );
    }

    #[test]
    fn r9_stop_ends_the_turn_and_clears_even_an_unfinished_message() {
        let mut state = busy_state();
        assert!(
            matches!(state.turn, Turn::Running { .. }),
            "precondition: the turn must be running, got {}",
            describe_turn(&state.turn)
        );
        assert!(
            !live_message_of(&state).is_final,
            "precondition: the live message must still be unfinished"
        );

        state.apply(&hook(
            "Stop",
            900,
            json!({ "last_assistant_message": "working on it" }),
        ));

        assert!(
            matches!(state.turn, Turn::Idle),
            "Stop must end the turn, got {}",
            describe_turn(&state.turn)
        );
        assert!(
            state.live_message.is_none(),
            "Stop must clear the live_message even when it never went final, got {}",
            describe_live_message(&state.live_message)
        );
        assert!(
            state.running_tools.is_empty(),
            "Stop must clear running_tools, got [{}]",
            describe_tools(&state.running_tools)
        );
        assert!(
            state.pending_permission.is_none(),
            "Stop must clear pending_permission, got {}",
            describe_permission(&state.pending_permission)
        );
        assert!(
            state.pending_question.is_none(),
            "Stop must clear pending_question, got {}",
            describe_question(&state.pending_question)
        );
    }

    #[test]
    fn r10_subagent_stop_touches_nothing_but_the_clock_and_the_mode() {
        let mut state = busy_state();
        let before = describe_state(&state);

        state.apply(&hook(
            "SubagentStop",
            900,
            json!({ "last_assistant_message": "sub-agent done" }),
        ));

        assert_eq!(
            describe_state(&state),
            before,
            "SubagentStop must not change any live state (rule 1 fields excluded)"
        );
        assert_eq!(
            state.last_event_at_ms, 900,
            "SubagentStop must still move the clock, got {}",
            state.last_event_at_ms
        );
    }

    #[test]
    fn r11_pre_compact_and_post_compact_toggle_the_compacting_flag() {
        let mut state = LiveState::default();
        assert!(
            !state.compacting,
            "a fresh LiveState is not compacting, got {}",
            state.compacting
        );

        state.apply(&hook("PreCompact", 100, json!({ "trigger": "auto" })));
        assert!(
            state.compacting,
            "PreCompact must set compacting, got {}",
            state.compacting
        );

        state.apply(&hook("PostCompact", 200, json!({ "trigger": "auto" })));
        assert!(
            !state.compacting,
            "PostCompact must clear compacting, got {}",
            state.compacting
        );
    }

    #[test]
    fn r12_session_end_clears_everything_except_the_permission_mode() {
        let mut state = busy_state();
        state.apply(&hook("PreCompact", 150, json!({ "trigger": "manual" })));
        state.apply(&hook(
            "Notification",
            160,
            json!({ "notification_type": "idle_prompt" }),
        ));
        state.apply(&hook("UserPromptSubmit", 170, json!({ "prompt": "again" })));
        state.apply(&pre_tool_use(180, "tu3", "Bash"));

        state.apply(&hook(
            "SessionEnd",
            1_000,
            json!({ "reason": "exit", "permission_mode": "plan" }),
        ));

        assert!(
            state.session_ended,
            "SessionEnd must mark the session ended, got {}",
            state.session_ended
        );
        assert!(
            matches!(state.turn, Turn::Idle),
            "SessionEnd must end the turn, got {}",
            describe_turn(&state.turn)
        );
        assert!(
            state.live_message.is_none(),
            "SessionEnd must clear the live_message, got {}",
            describe_live_message(&state.live_message)
        );
        assert!(
            state.running_tools.is_empty(),
            "SessionEnd must clear running_tools, got [{}]",
            describe_tools(&state.running_tools)
        );
        assert!(
            state.pending_permission.is_none(),
            "SessionEnd must clear pending_permission, got {}",
            describe_permission(&state.pending_permission)
        );
        assert!(
            state.pending_question.is_none(),
            "SessionEnd must clear pending_question, got {}",
            describe_question(&state.pending_question)
        );
        assert!(
            !state.compacting,
            "SessionEnd must clear compacting, got {}",
            state.compacting
        );
        assert_eq!(
            state.last_notification, None,
            "SessionEnd must clear last_notification, got {:?}",
            state.last_notification
        );
        assert_eq!(
            state.permission_mode,
            Some("plan".to_string()),
            "permission_mode is the one field SessionEnd keeps, got {:?}",
            state.permission_mode
        );
    }

    #[test]
    fn r13_an_unknown_event_name_only_has_rule_one_effects() {
        let prior = [
            hook("UserPromptSubmit", 100, json!({ "prompt": "hi" })),
            hook(
                "PreToolUse",
                110,
                json!({ "tool_use_id": "tu1", "tool_name": "Bash", "tool_input": { "command": "ls" } }),
            ),
            message_display(
                120,
                json!({ "index": 0, "delta": "Hel", "message_id": "m1", "turn_id": "t1", "final": false }),
            ),
        ];

        let mut with_unknown = LiveState::default();
        for event in prior.iter() {
            with_unknown.apply(event);
        }
        with_unknown.apply(&hook(
            "PostModelSwitchFromTheFuture",
            999,
            json!({ "permission_mode": "plan", "anything": { "deep": [1, 2, 3] }, "tool_use_id": "tu1" }),
        ));

        let mut without_unknown = LiveState::default();
        for event in prior.iter() {
            without_unknown.apply(event);
        }
        without_unknown.permission_mode = Some("plan".to_string());

        assert_eq!(
            with_unknown.last_event_at_ms, 999,
            "an unknown event still moves the clock, got {}",
            with_unknown.last_event_at_ms
        );
        assert_eq!(
            with_unknown.permission_mode,
            Some("plan".to_string()),
            "an unknown event still copies permission_mode, got {:?}",
            with_unknown.permission_mode
        );
        assert_eq!(
            describe_state(&with_unknown),
            describe_state(&without_unknown),
            "an unknown event must otherwise leave the state exactly as it was"
        );
    }

    #[test]
    fn r14a_note_transcript_assistant_clears_a_final_message_the_transcript_caught_up_with() {
        let mut state = LiveState::default();
        state.apply(&message_display(
            10_000,
            json!({ "index": 0, "delta": "done", "message_id": "m1", "turn_id": "t1", "final": true }),
        ));

        state.note_transcript_assistant(8_000);

        assert!(
            state.live_message.is_none(),
            "a final message must be cleared once the transcript has it (ts 8000 >= 10000 - 2000), got {}",
            describe_live_message(&state.live_message)
        );
    }

    #[test]
    fn r14b_note_transcript_assistant_keeps_a_final_message_the_transcript_predates() {
        let mut state = LiveState::default();
        state.apply(&message_display(
            10_000,
            json!({ "index": 0, "delta": "done", "message_id": "m1", "turn_id": "t1", "final": true }),
        ));

        state.note_transcript_assistant(7_999);

        let message = match state.live_message.as_ref() {
            Some(message) => message,
            None => panic!(
                "an older assistant record (ts 7999 < 10000 - 2000) is a different message and must not clear the live one"
            ),
        };
        assert_eq!(
            message.text, "done",
            "the live message must survive untouched, got {:?}",
            message.text
        );
    }

    #[test]
    fn r14c_note_transcript_assistant_never_clears_an_unfinished_message() {
        let mut state = LiveState::default();
        state.apply(&message_display(
            10_000,
            json!({ "index": 0, "delta": "still typing", "message_id": "m1", "turn_id": "t1", "final": false }),
        ));

        state.note_transcript_assistant(10_000);
        assert!(
            state.live_message.is_some(),
            "an unfinished message must never be cleared by the transcript (same clock), got None"
        );

        state.note_transcript_assistant(99_000);
        let message = match state.live_message.as_ref() {
            Some(message) => message,
            None => panic!(
                "an unfinished message must never be cleared by the transcript (newer clock)"
            ),
        };
        assert_eq!(
            message.text, "still typing",
            "the unfinished message must survive untouched, got {:?}",
            message.text
        );
    }

    #[test]
    fn r15_note_tool_result_ends_that_tool_and_clears_pendings_with_the_same_id() {
        let mut state = LiveState::default();
        state.apply(&pre_tool_use(100, "tu1", "Bash"));
        state.apply(&ask_user_question(110, "tu2"));
        state.apply(&permission_request(120, "tu1", "Bash"));

        state.note_tool_result("tu1");

        assert_eq!(
            state.running_tools.len(),
            1,
            "only the tool with that id may be removed, got [{}]",
            describe_tools(&state.running_tools)
        );
        let remaining = match state.running_tools.first() {
            Some(tool) => tool,
            None => panic!("expected the unrelated tool to survive"),
        };
        assert_eq!(
            remaining.tool_use_id, "tu2",
            "the surviving tool must be the unrelated one, got {}",
            remaining.tool_use_id
        );
        assert!(
            state.pending_permission.is_none(),
            "a tool_result for the permission's id must clear it, got {}",
            describe_permission(&state.pending_permission)
        );
        assert!(
            state.pending_question.is_some(),
            "a tool_result for a different id must leave pending_question, got {}",
            describe_question(&state.pending_question)
        );

        state.note_tool_result("tu2");

        assert!(
            state.running_tools.is_empty(),
            "the second tool_result must end the last tool, got [{}]",
            describe_tools(&state.running_tools)
        );
        assert!(
            state.pending_question.is_none(),
            "a tool_result for the question's id must clear it, got {}",
            describe_question(&state.pending_question)
        );
    }

    #[test]
    fn r16_is_idle_needs_all_four_conditions() {
        let state = LiveState::default();
        assert!(
            state.is_idle(),
            "a fresh LiveState is idle, got false for {}",
            describe_state(&state)
        );

        let running_turn = LiveState {
            turn: Turn::Running { since_ms: 5 },
            ..Default::default()
        };
        assert!(
            !running_turn.is_idle(),
            "a running turn alone must make is_idle false, got true for {}",
            describe_state(&running_turn)
        );

        let running_tool = LiveState {
            running_tools: vec![RunningTool {
                tool_use_id: "tu1".to_string(),
                name: "Bash".to_string(),
                input: json!({ "command": "ls" }),
                started_at_ms: 5,
            }],
            ..Default::default()
        };
        assert!(
            !running_tool.is_idle(),
            "a running tool alone must make is_idle false, got true for {}",
            describe_state(&running_tool)
        );

        let waiting_permission = LiveState {
            pending_permission: Some(PermissionRequest {
                tool_use_id: "tu1".to_string(),
                tool_name: "Bash".to_string(),
                tool_input: json!({ "command": "ls" }),
                since_ms: 5,
            }),
            ..Default::default()
        };
        assert!(
            !waiting_permission.is_idle(),
            "a pending permission alone must make is_idle false, got true for {}",
            describe_state(&waiting_permission)
        );

        let waiting_question = LiveState {
            pending_question: Some(PendingQuestion {
                tool_use_id: "tu2".to_string(),
                questions: json!([{ "question": "ship it?" }]),
                since_ms: 5,
            }),
            ..Default::default()
        };
        assert!(
            !waiting_question.is_idle(),
            "a pending question alone must make is_idle false, got true for {}",
            describe_state(&waiting_question)
        );
    }

    // ---------- StatusSnapshot::parse ----------

    fn full_status_json() -> String {
        json!({
            "session_id": "s1",
            "transcript_path": "/work/transcript.jsonl",
            "model": { "id": "claude-opus-5", "display_name": "Opus 5" },
            "context_window": {
                "total_input_tokens": 108_000,
                "total_output_tokens": 4_200,
                "context_window_size": 200_000,
                "used_percentage": 54.5,
                "remaining_percentage": 45.5,
                "current_usage": 108_000
            },
            "cost": { "total_cost_usd": 3.25, "total_duration_ms": 1234 },
            "effort": { "level": "high" },
            "rate_limits": {
                "five_hour": { "used_percentage": 23.0, "resets_at": 1_789_717_752i64 },
                "seven_day": { "used_percentage": 7.5, "resets_at": 1_789_800_000i64 }
            },
            "exceeds_200k_tokens": false
        })
        .to_string()
    }

    #[test]
    fn s1_the_documented_status_line_json_maps_every_field() {
        let snapshot = parse_status_or_panic(&full_status_json());

        assert_eq!(
            snapshot.model_id,
            Some("claude-opus-5".to_string()),
            "model.id must map to model_id, got {:?}",
            snapshot.model_id
        );
        assert_eq!(
            snapshot.model_display_name,
            Some("Opus 5".to_string()),
            "model.display_name must map to model_display_name, got {:?}",
            snapshot.model_display_name
        );
        assert_eq!(
            snapshot.context_window_size,
            Some(200_000),
            "context_window.context_window_size must map, got {:?}",
            snapshot.context_window_size
        );
        assert_close(
            snapshot.context_used_percentage,
            54.5,
            "context_window.used_percentage must map to context_used_percentage",
        );
        assert_eq!(
            snapshot.total_input_tokens,
            Some(108_000),
            "context_window.total_input_tokens must map, got {:?}",
            snapshot.total_input_tokens
        );
        assert_eq!(
            snapshot.total_output_tokens,
            Some(4_200),
            "context_window.total_output_tokens must map, got {:?}",
            snapshot.total_output_tokens
        );
        assert_close(
            snapshot.total_cost_usd,
            3.25,
            "cost.total_cost_usd must map to total_cost_usd",
        );
        assert_eq!(
            snapshot.effort,
            Some("high".to_string()),
            "effort.level must map to effort, got {:?}",
            snapshot.effort
        );
        assert_close(
            snapshot.five_hour_used_percentage,
            23.0,
            "rate_limits.five_hour.used_percentage must map",
        );
        assert_eq!(
            snapshot.five_hour_resets_at,
            Some(1_789_717_752i64),
            "rate_limits.five_hour.resets_at must map, got {:?}",
            snapshot.five_hour_resets_at
        );
        assert_close(
            snapshot.seven_day_used_percentage,
            7.5,
            "rate_limits.seven_day.used_percentage must map",
        );
        assert_eq!(
            snapshot.seven_day_resets_at,
            Some(1_789_800_000i64),
            "rate_limits.seven_day.resets_at must map, got {:?}",
            snapshot.seven_day_resets_at
        );
        assert_eq!(
            snapshot.exceeds_200k_tokens,
            Some(false),
            "exceeds_200k_tokens must map, got {:?}",
            snapshot.exceeds_200k_tokens
        );
    }

    #[test]
    fn s2_a_null_used_percentage_is_none_while_its_neighbours_still_parse() {
        let text = json!({
            "model": { "id": "claude-opus-5", "display_name": "Opus 5" },
            "context_window": {
                "total_input_tokens": 108_000,
                "context_window_size": 200_000,
                "used_percentage": null
            }
        })
        .to_string();
        let snapshot = parse_status_or_panic(&text);

        assert_eq!(
            snapshot.context_used_percentage, None,
            "a null used_percentage must be None, got {:?}",
            snapshot.context_used_percentage
        );
        assert_eq!(
            snapshot.context_window_size,
            Some(200_000),
            "a null neighbour must not stop context_window_size parsing, got {:?}",
            snapshot.context_window_size
        );
        assert_eq!(
            snapshot.total_input_tokens,
            Some(108_000),
            "a null neighbour must not stop total_input_tokens parsing, got {:?}",
            snapshot.total_input_tokens
        );
        assert_eq!(
            snapshot.model_id,
            Some("claude-opus-5".to_string()),
            "a null neighbour must not stop model.id parsing, got {:?}",
            snapshot.model_id
        );
    }

    #[test]
    fn s3_an_empty_object_parses_with_every_field_none() {
        let snapshot = parse_status_or_panic("{}");

        assert_eq!(
            snapshot.model_id, None,
            "model_id must be None, got {:?}",
            snapshot.model_id
        );
        assert_eq!(
            snapshot.model_display_name, None,
            "model_display_name must be None, got {:?}",
            snapshot.model_display_name
        );
        assert_eq!(
            snapshot.context_window_size, None,
            "context_window_size must be None, got {:?}",
            snapshot.context_window_size
        );
        assert_eq!(
            snapshot.context_used_percentage, None,
            "context_used_percentage must be None, got {:?}",
            snapshot.context_used_percentage
        );
        assert_eq!(
            snapshot.total_input_tokens, None,
            "total_input_tokens must be None, got {:?}",
            snapshot.total_input_tokens
        );
        assert_eq!(
            snapshot.total_output_tokens, None,
            "total_output_tokens must be None, got {:?}",
            snapshot.total_output_tokens
        );
        assert_eq!(
            snapshot.total_cost_usd, None,
            "total_cost_usd must be None, got {:?}",
            snapshot.total_cost_usd
        );
        assert_eq!(
            snapshot.effort, None,
            "effort must be None, got {:?}",
            snapshot.effort
        );
        assert_eq!(
            snapshot.five_hour_used_percentage, None,
            "five_hour_used_percentage must be None, got {:?}",
            snapshot.five_hour_used_percentage
        );
        assert_eq!(
            snapshot.five_hour_resets_at, None,
            "five_hour_resets_at must be None, got {:?}",
            snapshot.five_hour_resets_at
        );
        assert_eq!(
            snapshot.seven_day_used_percentage, None,
            "seven_day_used_percentage must be None, got {:?}",
            snapshot.seven_day_used_percentage
        );
        assert_eq!(
            snapshot.seven_day_resets_at, None,
            "seven_day_resets_at must be None, got {:?}",
            snapshot.seven_day_resets_at
        );
        assert_eq!(
            snapshot.exceeds_200k_tokens, None,
            "exceeds_200k_tokens must be None, got {:?}",
            snapshot.exceeds_200k_tokens
        );
    }

    #[test]
    fn s4_anything_that_is_not_a_json_object_is_none() {
        for text in [
            "\"not an object\"",
            "[1, 2]",
            "null",
            "42",
            "",
            "{\"broken\":",
        ] {
            assert!(
                StatusSnapshot::parse(text).is_none(),
                "expected None for {text:?}, got a snapshot"
            );
        }
    }

    #[test]
    fn s5_missing_rate_limits_leaves_only_the_four_rate_fields_none() {
        let text = json!({
            "model": { "id": "claude-opus-5" },
            "context_window": { "used_percentage": 12.5, "context_window_size": 200_000 },
            "cost": { "total_cost_usd": 0.5 }
        })
        .to_string();
        let snapshot = parse_status_or_panic(&text);

        assert_eq!(
            snapshot.five_hour_used_percentage, None,
            "five_hour_used_percentage must be None without rate_limits, got {:?}",
            snapshot.five_hour_used_percentage
        );
        assert_eq!(
            snapshot.five_hour_resets_at, None,
            "five_hour_resets_at must be None without rate_limits, got {:?}",
            snapshot.five_hour_resets_at
        );
        assert_eq!(
            snapshot.seven_day_used_percentage, None,
            "seven_day_used_percentage must be None without rate_limits, got {:?}",
            snapshot.seven_day_used_percentage
        );
        assert_eq!(
            snapshot.seven_day_resets_at, None,
            "seven_day_resets_at must be None without rate_limits, got {:?}",
            snapshot.seven_day_resets_at
        );
        assert_close(
            snapshot.context_used_percentage,
            12.5,
            "the rest of the JSON must still parse",
        );
        assert_eq!(
            snapshot.model_id,
            Some("claude-opus-5".to_string()),
            "the rest of the JSON must still parse, got model_id {:?}",
            snapshot.model_id
        );
    }

    // ---------- timestamp_ms ----------

    #[test]
    fn t1_rfc3339_timestamps_become_milliseconds_since_the_epoch() {
        assert_eq!(
            timestamp_ms("1970-01-01T00:00:00.000Z"),
            Some(0),
            "the epoch itself must be 0, got {:?}",
            timestamp_ms("1970-01-01T00:00:00.000Z")
        );
        assert_eq!(
            timestamp_ms("1970-01-01T00:00:01.500Z"),
            Some(1_500),
            "one and a half seconds after the epoch must be 1500 ms, got {:?}",
            timestamp_ms("1970-01-01T00:00:01.500Z")
        );

        let earlier = match timestamp_ms("2026-09-18T07:49:12.345Z") {
            Some(value) => value,
            None => panic!("a well formed transcript timestamp must parse, got None"),
        };
        let later = match timestamp_ms("2026-09-18T07:49:13.345Z") {
            Some(value) => value,
            None => panic!("a well formed transcript timestamp must parse, got None"),
        };
        assert_eq!(
            later - earlier,
            1_000,
            "timestamps one second apart must be 1000 ms apart, got {} - {} = {}",
            later,
            earlier,
            later - earlier
        );
        assert!(
            earlier > 1_600_000_000_000,
            "a 2026 timestamp must be milliseconds, not seconds, got {earlier}"
        );
    }

    #[test]
    fn t2_a_timestamp_that_is_not_a_time_is_none() {
        for text in [
            "",
            "not a time",
            "2026-13-45T99:99:99.000Z",
            "1789717752345",
            "T07:49:12Z",
        ] {
            let parsed = timestamp_ms(text);
            assert!(
                parsed.is_none(),
                "expected None for {text:?}, got {parsed:?}"
            );
        }
    }
}
