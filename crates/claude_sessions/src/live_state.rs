//! Session activity derived from hook events, the statusLine snapshot, and the
//! transcript catching up with both.
//!
//! No I/O and no GPUI: the store feeds this values, and the panel reads them.

use serde_json::Value;

const ASK_USER_QUESTION_TOOL: &str = "AskUserQuestion";

pub struct HookEvent {
    pub received_at_ms: i64,
    pub name: String,
    pub session_id: String,
    pub permission_mode: Option<String>,
    pub tool_use_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_input: Option<Value>,
    pub raw: Value,
}

/// One JSONL line of `~/.claude/zed-events/<id>.jsonl`. Returns None for blank lines,
/// non-objects, lines without `event.hook_event_name` or `event.session_id`. A missing
/// `received_at_ms` becomes 0.
pub fn parse_hook_event(line: &str) -> Option<HookEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let wrapper: Value = serde_json::from_str(trimmed).ok()?;
    let wrapper_object = wrapper.as_object()?;
    let event = wrapper_object.get("event")?;
    let event_object = event.as_object()?;
    let name = string_field(event_object.get("hook_event_name"))?;
    let session_id = string_field(event_object.get("session_id"))?;
    if name.is_empty() || session_id.is_empty() {
        return None;
    }
    let received_at_ms = integer_field(wrapper_object.get("received_at_ms")).unwrap_or(0);

    Some(HookEvent {
        received_at_ms,
        name,
        session_id,
        permission_mode: string_field(event_object.get("permission_mode")),
        tool_use_id: string_field(event_object.get("tool_use_id")),
        tool_name: string_field(event_object.get("tool_name")),
        tool_input: event_object
            .get("tool_input")
            .cloned()
            .filter(|value| !value.is_null()),
        raw: event.clone(),
    })
}

pub struct LiveMessage {
    pub turn_id: Option<String>,
    pub message_id: Option<String>,
    pub text: String,
    pub is_final: bool,
    pub updated_at_ms: i64,
}

pub struct RunningTool {
    pub tool_use_id: String,
    pub name: String,
    pub input: Value,
    pub started_at_ms: i64,
}

pub struct PermissionRequest {
    pub tool_use_id: String,
    pub tool_name: String,
    pub tool_input: Value,
    pub since_ms: i64,
}

pub struct PendingQuestion {
    pub tool_use_id: String,
    pub questions: Value,
    pub since_ms: i64,
}

#[derive(Default)]
pub enum Turn {
    #[default]
    Idle,
    Running {
        since_ms: i64,
    },
}

#[derive(Default)]
pub struct LiveState {
    pub live_message: Option<LiveMessage>,
    pub running_tools: Vec<RunningTool>,
    pub pending_permission: Option<PermissionRequest>,
    pub pending_question: Option<PendingQuestion>,
    pub permission_mode: Option<String>,
    pub turn: Turn,
    pub compacting: bool,
    pub last_notification: Option<(String, i64)>,
    pub last_event_at_ms: i64,
    pub session_ended: bool,
}

impl LiveState {
    pub fn apply(&mut self, event: &HookEvent) {
        self.last_event_at_ms = event.received_at_ms;
        if let Some(permission_mode) = event.permission_mode.as_ref() {
            self.permission_mode = Some(permission_mode.clone());
        }

        match event.name.as_str() {
            "UserPromptSubmit" => self.apply_user_prompt_submit(event),
            "MessageDisplay" => self.apply_message_display(event),
            "PreToolUse" => self.apply_pre_tool_use(event),
            "PermissionRequest" => self.apply_permission_request(event),
            "PostToolUse" => self.apply_post_tool_use(event),
            "PermissionDenied" => self.apply_permission_denied(event),
            "Notification" => self.apply_notification(event),
            "Stop" => self.apply_stop(),
            "SubagentStop" => {}
            "PreCompact" => self.compacting = true,
            "PostCompact" => self.compacting = false,
            "SessionEnd" => self.apply_session_end(),
            _ => {}
        }
    }

    /// The transcript absorbed an assistant record with this timestamp (ms since epoch,
    /// from the record's RFC3339 `timestamp`).
    pub fn note_transcript_assistant(&mut self, timestamp_ms: i64) {
        let Some(live_message) = self.live_message.as_ref() else {
            return;
        };
        if live_message.is_final && timestamp_ms >= live_message.updated_at_ms.saturating_sub(2000)
        {
            self.live_message = None;
        }
    }

    /// The transcript absorbed a tool_result for this tool_use_id.
    pub fn note_tool_result(&mut self, tool_use_id: &str) {
        self.remove_running_tool(tool_use_id);
        self.clear_pending_for(tool_use_id);
    }

    /// The transcript recorded that the user interrupted the turn. Stop does not run on
    /// an interrupt, so the record is the only evidence the turn is over.
    pub fn note_interrupted(&mut self) {
        self.idle_turn();
    }

    pub fn is_idle(&self) -> bool {
        matches!(self.turn, Turn::Idle)
            && self.running_tools.is_empty()
            && self.pending_permission.is_none()
            && self.pending_question.is_none()
    }

    fn apply_user_prompt_submit(&mut self, event: &HookEvent) {
        self.turn = Turn::Running {
            since_ms: event.received_at_ms,
        };
        self.live_message = None;
        self.running_tools.clear();
        self.pending_permission = None;
        self.pending_question = None;
        self.session_ended = false;
    }

    fn apply_message_display(&mut self, event: &HookEvent) {
        let message_id = string_field(event.raw.get("message_id"));
        let turn_id = string_field(event.raw.get("turn_id"));
        let delta = string_field(event.raw.get("delta")).unwrap_or_default();
        let is_final = event
            .raw
            .get("final")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let index = event.raw.get("index").and_then(Value::as_u64);

        let start_new = match index {
            Some(0) => true,
            Some(_) => {
                self.live_message.is_none()
                    || self
                        .live_message
                        .as_ref()
                        .map(|message| &message.message_id)
                        != Some(&message_id)
            }
            None => self
                .live_message
                .as_ref()
                .is_none_or(|message| message.message_id != message_id),
        };

        if start_new {
            self.live_message = Some(LiveMessage {
                turn_id,
                message_id,
                text: delta,
                is_final,
                updated_at_ms: event.received_at_ms,
            });
        } else if let Some(live_message) = self.live_message.as_mut() {
            live_message.text.push_str(&delta);
            live_message.is_final = is_final;
            live_message.updated_at_ms = event.received_at_ms;
            if turn_id.is_some() {
                live_message.turn_id = turn_id;
            }
        }
    }

    fn apply_pre_tool_use(&mut self, event: &HookEvent) {
        // A call the payload does not name cannot be tracked. Recording it under the
        // empty id is worse than ignoring it: the PostToolUse that would end it carries
        // no id either and is itself ignored, so nothing would ever take it off again.
        let Some(tool_use_id) = event
            .tool_use_id
            .clone()
            .filter(|tool_use_id| !tool_use_id.is_empty())
        else {
            return;
        };
        let name = event.tool_name.clone().unwrap_or_default();
        let input = event.tool_input.clone().unwrap_or(Value::Null);

        if self
            .pending_permission
            .as_ref()
            .is_some_and(|permission| permission.tool_use_id == tool_use_id)
        {
            self.pending_permission = None;
        }

        let running = RunningTool {
            tool_use_id: tool_use_id.clone(),
            name: name.clone(),
            input: input.clone(),
            started_at_ms: event.received_at_ms,
        };
        if let Some(existing) = self
            .running_tools
            .iter_mut()
            .find(|tool| tool.tool_use_id == tool_use_id)
        {
            *existing = running;
        } else {
            self.running_tools.push(running);
        }

        if name == ASK_USER_QUESTION_TOOL {
            self.pending_question = Some(PendingQuestion {
                tool_use_id,
                questions: input.get("questions").cloned().unwrap_or(Value::Null),
                since_ms: event.received_at_ms,
            });
        }
    }

    fn apply_permission_request(&mut self, event: &HookEvent) {
        // Same reason as [`Self::apply_pre_tool_use`]: only the id answers this request,
        // so a request that carries none can only be drawn and never cleared.
        let Some(tool_use_id) = event
            .tool_use_id
            .clone()
            .filter(|tool_use_id| !tool_use_id.is_empty())
        else {
            return;
        };
        self.pending_permission = Some(PermissionRequest {
            tool_use_id,
            tool_name: event.tool_name.clone().unwrap_or_default(),
            tool_input: event.tool_input.clone().unwrap_or(Value::Null),
            since_ms: event.received_at_ms,
        });
    }

    fn apply_post_tool_use(&mut self, event: &HookEvent) {
        let Some(tool_use_id) = event.tool_use_id.as_deref() else {
            return;
        };
        self.remove_running_tool(tool_use_id);
        self.clear_pending_for(tool_use_id);
    }

    fn apply_permission_denied(&mut self, event: &HookEvent) {
        let Some(tool_use_id) = event.tool_use_id.as_deref() else {
            return;
        };
        self.remove_running_tool(tool_use_id);
        if self
            .pending_permission
            .as_ref()
            .is_some_and(|permission| permission.tool_use_id == tool_use_id)
        {
            self.pending_permission = None;
        }
    }

    fn apply_notification(&mut self, event: &HookEvent) {
        let notification_type =
            string_field(event.raw.get("notification_type")).unwrap_or_default();
        self.last_notification = Some((notification_type, event.received_at_ms));
    }

    fn apply_stop(&mut self) {
        self.idle_turn();
    }

    fn idle_turn(&mut self) {
        self.turn = Turn::Idle;
        self.live_message = None;
        self.running_tools.clear();
        self.pending_permission = None;
        self.pending_question = None;
    }

    fn apply_session_end(&mut self) {
        self.session_ended = true;
        self.turn = Turn::Idle;
        self.live_message = None;
        self.running_tools.clear();
        self.pending_permission = None;
        self.pending_question = None;
        self.compacting = false;
        self.last_notification = None;
    }

    fn remove_running_tool(&mut self, tool_use_id: &str) {
        self.running_tools
            .retain(|tool| tool.tool_use_id != tool_use_id);
    }

    fn clear_pending_for(&mut self, tool_use_id: &str) {
        if self
            .pending_permission
            .as_ref()
            .is_some_and(|permission| permission.tool_use_id == tool_use_id)
        {
            self.pending_permission = None;
        }
        if self
            .pending_question
            .as_ref()
            .is_some_and(|question| question.tool_use_id == tool_use_id)
        {
            self.pending_question = None;
        }
    }
}

pub struct StatusSnapshot {
    pub model_id: Option<String>,
    pub model_display_name: Option<String>,
    pub context_window_size: Option<u64>,
    pub context_used_percentage: Option<f64>,
    pub total_input_tokens: Option<u64>,
    pub total_output_tokens: Option<u64>,
    pub total_cost_usd: Option<f64>,
    pub effort: Option<String>,
    pub five_hour_used_percentage: Option<f64>,
    pub five_hour_resets_at: Option<i64>,
    pub seven_day_used_percentage: Option<f64>,
    pub seven_day_resets_at: Option<i64>,
    pub exceeds_200k_tokens: Option<bool>,
}

impl StatusSnapshot {
    /// None only if `json` is not a JSON object; every field individually optional;
    /// null → None.
    pub fn parse(json: &str) -> Option<StatusSnapshot> {
        let value: Value = serde_json::from_str(json).ok()?;
        let object = value.as_object()?;
        let model = object.get("model");
        let context_window = object.get("context_window");
        let cost = object.get("cost");
        let effort = object.get("effort");
        let five_hour = object
            .get("rate_limits")
            .and_then(|limits| limits.get("five_hour"));
        let seven_day = object
            .get("rate_limits")
            .and_then(|limits| limits.get("seven_day"));

        Some(StatusSnapshot {
            model_id: string_field(model.and_then(|model| model.get("id"))),
            model_display_name: string_field(model.and_then(|model| model.get("display_name"))),
            context_window_size: unsigned_field(
                context_window.and_then(|window| window.get("context_window_size")),
            ),
            context_used_percentage: float_field(
                context_window.and_then(|window| window.get("used_percentage")),
            ),
            total_input_tokens: unsigned_field(
                context_window.and_then(|window| window.get("total_input_tokens")),
            ),
            total_output_tokens: unsigned_field(
                context_window.and_then(|window| window.get("total_output_tokens")),
            ),
            total_cost_usd: float_field(cost.and_then(|cost| cost.get("total_cost_usd"))),
            effort: string_field(effort.and_then(|effort| effort.get("level"))),
            five_hour_used_percentage: float_field(
                five_hour.and_then(|window| window.get("used_percentage")),
            ),
            five_hour_resets_at: integer_field(
                five_hour.and_then(|window| window.get("resets_at")),
            ),
            seven_day_used_percentage: float_field(
                seven_day.and_then(|window| window.get("used_percentage")),
            ),
            seven_day_resets_at: integer_field(
                seven_day.and_then(|window| window.get("resets_at")),
            ),
            exceeds_200k_tokens: object.get("exceeds_200k_tokens").and_then(Value::as_bool),
        })
    }
}

/// Milliseconds since epoch from a transcript record's RFC3339 `timestamp`.
pub fn timestamp_ms(record_timestamp: &str) -> Option<i64> {
    Some(
        chrono::DateTime::parse_from_rfc3339(record_timestamp)
            .ok()?
            .timestamp_millis(),
    )
}

fn string_field(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_string)
}

fn integer_field(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|number| i64::try_from(number).ok()))
        .or_else(|| value.as_f64().map(|number| number as i64))
        .or_else(|| parse_numeric_string(value.as_str()).map(|number| number as i64))
}

fn unsigned_field(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|number| u64::try_from(number).ok()))
        .or_else(|| {
            value
                .as_f64()
                .and_then(|number| (number >= 0.0 && number.is_finite()).then_some(number as u64))
        })
        .or_else(|| {
            parse_numeric_string(value.as_str())
                .and_then(|number| (number >= 0.0 && number.is_finite()).then_some(number as u64))
        })
}

fn parse_numeric_string(text: Option<&str>) -> Option<f64> {
    // Some writers encode millisecond times as JSON strings. `as_i64`/`as_f64` only
    // see Number, and treating a numeric string as "no time" (0) skips the permission
    // pairing window.
    let text = text?.trim();
    text.parse::<i64>()
        .ok()
        .map(|number| number as f64)
        .or_else(|| text.parse::<f64>().ok().filter(|number| number.is_finite()))
}

fn float_field(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    value
        .as_f64()
        .or_else(|| value.as_i64().map(|number| number as f64))
        .or_else(|| value.as_u64().map(|number| number as f64))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelInboxEvent {
    Ready {
        at_ms: i64,
    },
    PermissionRequest {
        at_ms: i64,
        request_id: String,
        tool_name: String,
        description: String,
        input_preview: String,
    },
    PermissionAnswered {
        at_ms: i64,
        request_id: String,
        behavior: String,
    },
    MessageSent {
        at_ms: i64,
        outbox_file: String,
        content_chars: u64,
    },
    Error {
        at_ms: i64,
        outbox_file: Option<String>,
        reason: String,
    },
    Closed {
        at_ms: i64,
        reason: String,
    },
    Interrupted {
        at_ms: i64,
        claude_pid: u32,
        reason: String,
    },
    Unknown {
        raw: String,
    },
}

/// One JSONL line of `~/.claude/zed-channel/<pid>/inbox.jsonl`.
///
/// Blank lines are skipped. Every other line is kept: unknown kinds and unparseable
/// payloads become [`ChannelInboxEvent::Unknown`] rather than being dropped, because the
/// inbox is the only record of what the channel server did.
pub fn parse_channel_inbox_line(line: &str) -> Option<ChannelInboxEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
        return Some(ChannelInboxEvent::Unknown {
            raw: trimmed.to_string(),
        });
    };
    let Some(object) = value.as_object() else {
        return Some(ChannelInboxEvent::Unknown {
            raw: trimmed.to_string(),
        });
    };
    let Some(kind) = string_field(object.get("kind")) else {
        return Some(ChannelInboxEvent::Unknown {
            raw: trimmed.to_string(),
        });
    };
    let at_ms = integer_field(object.get("at_ms")).unwrap_or(0);
    Some(match kind.as_str() {
        "ready" => ChannelInboxEvent::Ready { at_ms },
        "permission_request" => ChannelInboxEvent::PermissionRequest {
            at_ms,
            request_id: string_field(object.get("request_id")).unwrap_or_default(),
            tool_name: string_field(object.get("tool_name")).unwrap_or_default(),
            description: string_field(object.get("description")).unwrap_or_default(),
            input_preview: string_field(object.get("input_preview")).unwrap_or_default(),
        },
        "permission_answered" => ChannelInboxEvent::PermissionAnswered {
            at_ms,
            request_id: string_field(object.get("request_id")).unwrap_or_default(),
            behavior: string_field(object.get("behavior")).unwrap_or_default(),
        },
        "message_sent" => ChannelInboxEvent::MessageSent {
            at_ms,
            outbox_file: string_field(object.get("outbox_file")).unwrap_or_default(),
            content_chars: unsigned_field(object.get("content_chars")).unwrap_or(0),
        },
        "error" => ChannelInboxEvent::Error {
            at_ms,
            outbox_file: string_field(object.get("outbox_file")).filter(|name| !name.is_empty()),
            reason: string_field(object.get("reason")).unwrap_or_default(),
        },
        "closed" => ChannelInboxEvent::Closed {
            at_ms,
            reason: string_field(object.get("reason")).unwrap_or_default(),
        },
        "interrupted" => ChannelInboxEvent::Interrupted {
            at_ms,
            claude_pid: unsigned_field(object.get("claude_pid"))
                .and_then(|pid| u32::try_from(pid).ok())
                .unwrap_or(0),
            reason: string_field(object.get("reason")).unwrap_or_default(),
        },
        _ => ChannelInboxEvent::Unknown {
            raw: trimmed.to_string(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(name: &str, received_at_ms: i64, extra: Value) -> HookEvent {
        let mut raw = extra;
        let object = raw.as_object_mut().expect("event extras are an object");
        object.insert("hook_event_name".into(), json!(name));
        object
            .entry("session_id")
            .or_insert_with(|| json!("session-1"));
        HookEvent {
            received_at_ms,
            name: name.to_string(),
            session_id: object
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or("session-1")
                .to_string(),
            permission_mode: object
                .get("permission_mode")
                .and_then(Value::as_str)
                .map(str::to_string),
            tool_use_id: object
                .get("tool_use_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            tool_name: object
                .get("tool_name")
                .and_then(Value::as_str)
                .map(str::to_string),
            tool_input: object
                .get("tool_input")
                .cloned()
                .filter(|value| !value.is_null()),
            raw,
        }
    }

    fn apply_named(state: &mut LiveState, name: &str, received_at_ms: i64, extra: Value) {
        state.apply(&event(name, received_at_ms, extra));
    }

    #[test]
    fn parse_hook_event_skips_blank_and_unshaped_lines() {
        assert!(parse_hook_event("").is_none());
        assert!(parse_hook_event("   \n").is_none());
        assert!(parse_hook_event("[]").is_none());
        assert!(parse_hook_event(r#"{"received_at_ms":1}"#).is_none());
        assert!(
            parse_hook_event(r#"{"received_at_ms":1,"event":{"hook_event_name":"Stop"}}"#)
                .is_none()
        );
        assert!(parse_hook_event(r#"{"received_at_ms":1,"event":{"session_id":"s"}}"#).is_none());
    }

    #[test]
    fn parse_hook_event_reads_the_wrapper_and_defaults_received_at() {
        let parsed = parse_hook_event(
            r#"{"event":{"hook_event_name":"Stop","session_id":"s1","permission_mode":"auto","tool_use_id":"t","tool_name":"Read","tool_input":{"file_path":"a.rs"}}}"#,
        )
        .expect("a wrapped event parses");
        assert_eq!(parsed.received_at_ms, 0);
        assert_eq!(parsed.name, "Stop");
        assert_eq!(parsed.session_id, "s1");
        assert_eq!(parsed.permission_mode.as_deref(), Some("auto"));
        assert_eq!(parsed.tool_use_id.as_deref(), Some("t"));
        assert_eq!(parsed.tool_name.as_deref(), Some("Read"));
        assert_eq!(parsed.tool_input, Some(json!({"file_path":"a.rs"})));
        assert_eq!(parsed.raw["hook_event_name"], json!("Stop"));
    }

    #[test]
    fn every_event_records_time_and_permission_mode() {
        let mut state = LiveState::default();
        apply_named(
            &mut state,
            "PostModelSwitch",
            42,
            json!({"permission_mode": "plan"}),
        );
        assert_eq!(state.last_event_at_ms, 42);
        assert_eq!(state.permission_mode.as_deref(), Some("plan"));
        assert!(state.is_idle());
    }

    #[test]
    fn user_prompt_submit_starts_a_turn_and_clears_live_work() {
        let mut state = LiveState::default();
        apply_named(
            &mut state,
            "PreToolUse",
            1,
            json!({
                "tool_use_id": "old",
                "tool_name": "AskUserQuestion",
                "tool_input": {"questions": [{"question": "q"}]}
            }),
        );
        apply_named(
            &mut state,
            "PermissionRequest",
            2,
            json!({"tool_use_id": "perm", "tool_name": "Bash"}),
        );
        apply_named(
            &mut state,
            "MessageDisplay",
            3,
            json!({"index": 0, "delta": "hi", "final": false}),
        );
        state.session_ended = true;

        apply_named(&mut state, "UserPromptSubmit", 10, json!({}));

        assert!(matches!(state.turn, Turn::Running { since_ms: 10 }));
        assert!(state.live_message.is_none());
        assert!(state.running_tools.is_empty());
        assert!(state.pending_permission.is_none());
        assert!(state.pending_question.is_none());
        assert!(!state.session_ended);
        assert!(!state.is_idle());
    }

    #[test]
    fn message_display_starts_appends_and_treats_missing_index_as_unknown() {
        let mut state = LiveState::default();
        apply_named(
            &mut state,
            "MessageDisplay",
            1,
            json!({"message_id": "m1", "index": 0, "delta": "Hel", "final": false}),
        );
        apply_named(
            &mut state,
            "MessageDisplay",
            2,
            json!({"message_id": "m1", "index": 1, "delta": "lo", "final": false}),
        );
        let live = state.live_message.as_ref().expect("a message is open");
        assert_eq!(live.text, "Hello");
        assert!(!live.is_final);

        apply_named(
            &mut state,
            "MessageDisplay",
            3,
            json!({"message_id": "m1", "delta": "!", "final": true}),
        );
        let live = state.live_message.as_ref().expect("unknown index appends");
        assert_eq!(live.text, "Hello!");
        assert!(live.is_final);
        assert_eq!(live.updated_at_ms, 3);

        apply_named(
            &mut state,
            "MessageDisplay",
            4,
            json!({"message_id": "m2", "delta": "new", "final": false}),
        );
        let live = state.live_message.as_ref().expect("a new id starts over");
        assert_eq!(live.text, "new");
        assert_eq!(live.message_id.as_deref(), Some("m2"));

        apply_named(
            &mut state,
            "MessageDisplay",
            5,
            json!({"message_id": "m2", "index": 0, "delta": "reset", "final": false}),
        );
        assert_eq!(
            state
                .live_message
                .as_ref()
                .map(|message| message.text.as_str()),
            Some("reset"),
            "index 0 starts a new message even when the id matches"
        );
    }

    #[test]
    fn pre_tool_use_tracks_tools_questions_and_allowed_permissions() {
        let mut state = LiveState::default();
        apply_named(
            &mut state,
            "PermissionRequest",
            1,
            json!({"tool_use_id": "t1", "tool_name": "Bash", "tool_input": {"command": "ls"}}),
        );
        apply_named(
            &mut state,
            "PreToolUse",
            2,
            json!({"tool_use_id": "t1", "tool_name": "Bash", "tool_input": {"command": "ls"}}),
        );
        assert!(state.pending_permission.is_none());
        assert_eq!(state.running_tools.len(), 1);

        apply_named(
            &mut state,
            "PreToolUse",
            3,
            json!({
                "tool_use_id": "q1",
                "tool_name": "AskUserQuestion",
                "tool_input": {"questions": [{"question": "Which?"}]}
            }),
        );
        assert_eq!(state.running_tools.len(), 2);
        assert_eq!(
            state
                .pending_question
                .as_ref()
                .map(|question| question.tool_use_id.as_str()),
            Some("q1")
        );
        assert_eq!(
            state
                .pending_question
                .as_ref()
                .map(|question| &question.questions),
            Some(&json!([{"question": "Which?"}]))
        );

        apply_named(
            &mut state,
            "PreToolUse",
            4,
            json!({"tool_use_id": "t1", "tool_name": "Bash", "tool_input": {"command": "pwd"}}),
        );
        assert_eq!(state.running_tools.len(), 2);
        assert_eq!(state.running_tools[0].input, json!({"command": "pwd"}));
        assert_eq!(state.running_tools[0].started_at_ms, 4);
    }

    #[test]
    fn post_tool_use_and_permission_denied_drop_matching_work() {
        let mut state = LiveState::default();
        apply_named(
            &mut state,
            "PreToolUse",
            1,
            json!({
                "tool_use_id": "q1",
                "tool_name": "AskUserQuestion",
                "tool_input": {"questions": []}
            }),
        );
        apply_named(
            &mut state,
            "PermissionRequest",
            2,
            json!({"tool_use_id": "p1", "tool_name": "Bash"}),
        );
        apply_named(
            &mut state,
            "PreToolUse",
            3,
            json!({"tool_use_id": "p1", "tool_name": "Bash"}),
        );
        apply_named(&mut state, "PostToolUse", 4, json!({"tool_use_id": "q1"}));
        assert!(state.pending_question.is_none());
        assert_eq!(
            state
                .running_tools
                .iter()
                .map(|tool| tool.tool_use_id.as_str())
                .collect::<Vec<_>>(),
            vec!["p1"]
        );

        apply_named(
            &mut state,
            "PermissionRequest",
            5,
            json!({"tool_use_id": "p1", "tool_name": "Bash"}),
        );
        apply_named(
            &mut state,
            "PermissionDenied",
            6,
            json!({"tool_use_id": "p1"}),
        );
        assert!(state.running_tools.is_empty());
        assert!(state.pending_permission.is_none());
    }

    #[test]
    fn notification_does_not_create_a_permission_request() {
        let mut state = LiveState::default();
        apply_named(
            &mut state,
            "Notification",
            9,
            json!({"notification_type": "permission_prompt"}),
        );
        assert_eq!(
            state.last_notification,
            Some(("permission_prompt".to_string(), 9))
        );
        assert!(state.pending_permission.is_none());
    }

    #[test]
    fn stop_idles_and_clears_even_a_non_final_live_message() {
        let mut state = LiveState::default();
        apply_named(&mut state, "UserPromptSubmit", 1, json!({}));
        apply_named(
            &mut state,
            "MessageDisplay",
            2,
            json!({"index": 0, "delta": "partial", "final": false}),
        );
        apply_named(
            &mut state,
            "PreToolUse",
            3,
            json!({"tool_use_id": "t", "tool_name": "Read"}),
        );
        apply_named(
            &mut state,
            "Stop",
            4,
            json!({"last_assistant_message": "partial"}),
        );
        assert!(matches!(state.turn, Turn::Idle));
        assert!(state.live_message.is_none());
        assert!(state.running_tools.is_empty());
        assert!(state.is_idle());
    }

    #[test]
    fn compact_session_end_and_transcript_notes() {
        let mut state = LiveState::default();
        apply_named(&mut state, "PreCompact", 1, json!({}));
        assert!(state.compacting);
        apply_named(&mut state, "PostCompact", 2, json!({}));
        assert!(!state.compacting);

        apply_named(
            &mut state,
            "MessageDisplay",
            10_000,
            json!({"index": 0, "delta": "done", "final": true, "message_id": "m"}),
        );
        apply_named(
            &mut state,
            "PreToolUse",
            11,
            json!({
                "tool_use_id": "t",
                "tool_name": "AskUserQuestion",
                "tool_input": {"questions": [1]}
            }),
        );
        apply_named(
            &mut state,
            "PermissionRequest",
            12,
            json!({"tool_use_id": "p", "tool_name": "Bash"}),
        );
        state.note_transcript_assistant(7_999);
        assert!(
            state.live_message.is_some(),
            "a timestamp more than 2s before the message must not clear it"
        );
        apply_named(
            &mut state,
            "MessageDisplay",
            13,
            json!({"message_id": "m", "delta": "x", "final": false}),
        );
        state.note_transcript_assistant(20);
        assert!(
            state.live_message.is_some(),
            "a non-final message is never cleared by the transcript"
        );

        apply_named(
            &mut state,
            "MessageDisplay",
            14,
            json!({"message_id": "m2", "index": 0, "delta": "fin", "final": true}),
        );
        state.note_transcript_assistant(13);
        assert!(state.live_message.is_none());

        state.note_tool_result("t");
        assert!(
            state
                .running_tools
                .iter()
                .all(|tool| tool.tool_use_id != "t")
        );
        assert!(state.pending_question.is_none());

        apply_named(
            &mut state,
            "SessionEnd",
            30,
            json!({"permission_mode": "auto"}),
        );
        assert!(state.session_ended);
        assert!(matches!(state.turn, Turn::Idle));
        assert!(state.live_message.is_none());
        assert!(state.running_tools.is_empty());
        assert!(state.pending_permission.is_none());
        assert!(state.pending_question.is_none());
        assert!(!state.compacting);
        assert!(state.last_notification.is_none());
        assert_eq!(state.permission_mode.as_deref(), Some("auto"));
    }

    #[test]
    fn status_snapshot_parses_optional_and_null_fields() {
        assert!(StatusSnapshot::parse("[]").is_none());
        assert!(StatusSnapshot::parse("not json").is_none());

        let snapshot = StatusSnapshot::parse(
            r#"{
                "model": {"id": "claude-opus", "display_name": "Opus"},
                "context_window": {
                    "context_window_size": 200000,
                    "used_percentage": null,
                    "total_input_tokens": 10,
                    "total_output_tokens": 2
                },
                "cost": {"total_cost_usd": 1.5},
                "effort": {"level": "high"},
                "rate_limits": {
                    "five_hour": {"used_percentage": 12.5, "resets_at": 111},
                    "seven_day": {"used_percentage": 3, "resets_at": 222}
                },
                "exceeds_200k_tokens": false
            }"#,
        )
        .expect("an object parses");
        assert_eq!(snapshot.model_id.as_deref(), Some("claude-opus"));
        assert_eq!(snapshot.model_display_name.as_deref(), Some("Opus"));
        assert_eq!(snapshot.context_window_size, Some(200000));
        assert_eq!(snapshot.context_used_percentage, None);
        assert_eq!(snapshot.total_input_tokens, Some(10));
        assert_eq!(snapshot.total_output_tokens, Some(2));
        assert_eq!(snapshot.total_cost_usd, Some(1.5));
        assert_eq!(snapshot.effort.as_deref(), Some("high"));
        assert_eq!(snapshot.five_hour_used_percentage, Some(12.5));
        assert_eq!(snapshot.five_hour_resets_at, Some(111));
        assert_eq!(snapshot.seven_day_used_percentage, Some(3.0));
        assert_eq!(snapshot.seven_day_resets_at, Some(222));
        assert_eq!(snapshot.exceeds_200k_tokens, Some(false));
    }

    #[test]
    fn timestamp_ms_reads_rfc3339() {
        let parsed = timestamp_ms("2026-09-13T15:11:12.113Z").expect("a RFC3339 timestamp");
        let expected = chrono::DateTime::parse_from_rfc3339("2026-09-13T15:11:12.113Z")
            .expect("fixture")
            .timestamp_millis();
        assert_eq!(parsed, expected);
        assert_eq!(timestamp_ms("not a time"), None);
    }

    /// A call the payload does not name cannot be tracked, and tracking it under the
    /// empty id is worse than not tracking it: the PostToolUse that would end it carries
    /// no id either, so nothing ever takes it off the activity line.
    #[test]
    fn a_tool_call_the_payload_does_not_name_is_not_tracked() {
        let mut state = LiveState::default();
        apply_named(
            &mut state,
            "PreToolUse",
            10,
            json!({"tool_name": "Bash", "tool_input": {"command": "ls"}}),
        );
        apply_named(
            &mut state,
            "PreToolUse",
            11,
            json!({"tool_name": "AskUserQuestion", "tool_input": {"questions": [{"question": "which?"}]}}),
        );
        apply_named(
            &mut state,
            "PermissionRequest",
            12,
            json!({"tool_name": "Bash", "tool_input": {"command": "rm -rf /"}}),
        );
        apply_named(
            &mut state,
            "PreToolUse",
            13,
            json!({"tool_use_id": 7, "tool_name": "Read", "tool_input": {"file_path": "/a"}}),
        );

        let running: Vec<&str> = state
            .running_tools
            .iter()
            .map(|tool| tool.tool_use_id.as_str())
            .collect();
        assert!(
            running.is_empty(),
            "a PreToolUse without a string tool_use_id must record nothing; expected no running tools, got ids {running:?}"
        );
        assert!(
            state.pending_question.is_none(),
            "an AskUserQuestion without a tool_use_id must not park a question; expected None, got id {:?}",
            state
                .pending_question
                .as_ref()
                .map(|question| question.tool_use_id.clone())
        );
        assert!(
            state.pending_permission.is_none(),
            "a PermissionRequest without a tool_use_id must not park a permission card; expected None, got id {:?}",
            state
                .pending_permission
                .as_ref()
                .map(|permission| permission.tool_use_id.clone())
        );
        assert_eq!(
            state.last_event_at_ms, 13,
            "rule 1 still applies to every one of them; expected last_event_at_ms 13, got {}",
            state.last_event_at_ms
        );
    }

    /// What the guard above must not reject: the ordinary events, which do name their
    /// call and must go on being tracked exactly as before.
    #[test]
    fn a_named_tool_call_is_still_tracked() {
        let mut state = LiveState::default();
        apply_named(
            &mut state,
            "PreToolUse",
            10,
            json!({"tool_use_id": "call-1", "tool_name": "Bash", "tool_input": {"command": "ls"}}),
        );
        apply_named(
            &mut state,
            "PermissionRequest",
            11,
            json!({"tool_use_id": "call-2", "tool_name": "Write", "tool_input": {"file_path": "/a"}}),
        );
        apply_named(
            &mut state,
            "PreToolUse",
            12,
            json!({
                "tool_use_id": "call-3",
                "tool_name": "AskUserQuestion",
                "tool_input": {"questions": [{"question": "which?"}]}
            }),
        );

        let running: Vec<&str> = state
            .running_tools
            .iter()
            .map(|tool| tool.tool_use_id.as_str())
            .collect();
        assert_eq!(
            running,
            vec!["call-1", "call-3"],
            "expected the two named calls to be running, got {running:?}"
        );
        assert_eq!(
            state
                .pending_permission
                .as_ref()
                .map(|permission| permission.tool_use_id.as_str()),
            Some("call-2"),
            "expected the named permission request to be pending, got {:?}",
            state
                .pending_permission
                .as_ref()
                .map(|permission| permission.tool_use_id.clone())
        );
        assert_eq!(
            state
                .pending_question
                .as_ref()
                .map(|question| question.tool_use_id.as_str()),
            Some("call-3"),
            "expected the named question to be pending, got {:?}",
            state
                .pending_question
                .as_ref()
                .map(|question| question.tool_use_id.clone())
        );
    }

    #[test]
    fn parse_channel_inbox_line_keeps_known_kinds_and_never_drops_unknown() {
        assert_eq!(parse_channel_inbox_line("  "), None);
        assert_eq!(
            parse_channel_inbox_line(r#"{"kind":"ready","at_ms":1}"#),
            Some(ChannelInboxEvent::Ready { at_ms: 1 })
        );
        assert_eq!(
            parse_channel_inbox_line(
                r#"{"kind":"permission_request","at_ms":2,"request_id":"abcde","tool_name":"Bash","description":"run","input_preview":"ls"}"#
            ),
            Some(ChannelInboxEvent::PermissionRequest {
                at_ms: 2,
                request_id: "abcde".to_string(),
                tool_name: "Bash".to_string(),
                description: "run".to_string(),
                input_preview: "ls".to_string(),
            })
        );
        assert_eq!(
            parse_channel_inbox_line(
                r#"{"kind":"permission_answered","at_ms":3,"request_id":"abcde","behavior":"allow","source":"zed"}"#
            ),
            Some(ChannelInboxEvent::PermissionAnswered {
                at_ms: 3,
                request_id: "abcde".to_string(),
                behavior: "allow".to_string(),
            })
        );
        match parse_channel_inbox_line(r#"{"kind":"future_kind","at_ms":9}"#) {
            Some(ChannelInboxEvent::Unknown { raw }) => {
                assert!(raw.contains("future_kind"), "got {raw}");
            }
            other => panic!("unknown kinds must be kept, got {other:?}"),
        }
        match parse_channel_inbox_line("not json") {
            Some(ChannelInboxEvent::Unknown { raw }) => assert_eq!(raw, "not json"),
            other => panic!("unparseable lines must be kept, got {other:?}"),
        }
    }

    #[test]
    fn parse_channel_inbox_line_accepts_string_and_float_at_ms_and_keeps_interrupt_kinds() {
        assert_eq!(
            parse_channel_inbox_line(r#"{"kind":"ready","at_ms":2.9}"#),
            Some(ChannelInboxEvent::Ready { at_ms: 2 }),
            "a float at_ms is a legal JSON number and must date the line, not be dropped"
        );
        assert_eq!(
            parse_channel_inbox_line(r#"{"kind":"ready","at_ms":"7"}"#),
            Some(ChannelInboxEvent::Ready { at_ms: 7 }),
            "a numeric string at_ms must date the line; expected Ready at_ms=7, got {:?}",
            parse_channel_inbox_line(r#"{"kind":"ready","at_ms":"7"}"#)
        );
        match parse_channel_inbox_line(r#"{"kind":"interrupt","reason":"user"}"#) {
            Some(ChannelInboxEvent::Unknown { raw }) => {
                assert!(raw.contains("interrupt"), "got {raw}");
            }
            other => panic!(
                "outbox interrupt kind on the inbox must become Unknown, not choke, got {other:?}"
            ),
        }
        assert_eq!(
            parse_channel_inbox_line(
                r#"{"kind":"interrupted","at_ms":9,"claude_pid":42,"reason":"user"}"#,
            ),
            Some(ChannelInboxEvent::Interrupted {
                at_ms: 9,
                claude_pid: 42,
                reason: "user".to_string(),
            }),
            "inbox interrupted is a known kind"
        );
        match parse_channel_inbox_line(
            r#"{"kind":"error","at_ms":1,"reason":"interrupt_throttled"}"#,
        ) {
            Some(ChannelInboxEvent::Error { reason, .. }) => {
                assert_eq!(reason, "interrupt_throttled");
            }
            other => panic!("error.reason is an open set, got {other:?}"),
        }
    }

    #[test]
    fn note_interrupted_idles_and_clears_live_work() {
        let mut state = LiveState::default();
        apply_named(&mut state, "UserPromptSubmit", 1, json!({}));
        apply_named(
            &mut state,
            "PreToolUse",
            2,
            json!({"tool_use_id": "t", "tool_name": "Read"}),
        );
        apply_named(
            &mut state,
            "PreToolUse",
            3,
            json!({
                "tool_use_id": "q",
                "tool_name": "AskUserQuestion",
                "tool_input": {"questions": [{"question": "which?"}]}
            }),
        );
        apply_named(
            &mut state,
            "PermissionRequest",
            4,
            json!({"tool_use_id": "p", "tool_name": "Bash"}),
        );

        assert!(
            matches!(state.turn, Turn::Running { .. }),
            "the fixture must be mid-turn before note_interrupted"
        );
        assert!(
            !state.running_tools.is_empty(),
            "the fixture must have running tools before note_interrupted, got {}",
            state.running_tools.len()
        );
        assert!(
            state.pending_permission.is_some(),
            "the fixture must have a pending permission before note_interrupted"
        );
        assert!(
            state.pending_question.is_some(),
            "the fixture must have a pending question before note_interrupted"
        );

        state.note_interrupted();

        let turn = match state.turn {
            Turn::Idle => "Idle",
            Turn::Running { .. } => "Running",
        };
        assert_eq!(
            turn, "Idle",
            "note_interrupted must idle the turn, got {turn}"
        );
        assert_eq!(
            state.running_tools.len(),
            0,
            "note_interrupted must clear running tools, got {}",
            state.running_tools.len()
        );
        assert!(
            state.pending_permission.is_none(),
            "note_interrupted must clear pending_permission, still Some"
        );
        assert!(
            state.pending_question.is_none(),
            "note_interrupted must clear pending_question, still Some"
        );
        assert!(
            state.is_idle(),
            "note_interrupted must leave is_idle true, got false"
        );
    }
}
