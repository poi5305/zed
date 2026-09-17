use std::{
    ffi::OsStr,
    io::Read as _,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::{Value, json};

use crate::fs_rpc::FsRpc;

pub fn handles(method: &str) -> bool {
    method.starts_with("ClaudeSessions::")
}

pub fn dispatch(fs_rpc: &FsRpc, method: &str, params: &Value) -> Result<Value> {
    refuse_if_claude_directory_restricted(fs_rpc)?;
    match method {
        "ClaudeSessions::list_sessions" => list_sessions(params),
        "ClaudeSessions::read_transcript_tail" => read_transcript_tail(params),
        "ClaudeSessions::list_subagents" => list_subagents(params),
        "ClaudeSessions::list_slash_commands" => list_slash_commands(params),
        "ClaudeSessions::list_files_under" => list_files_under(params),
        "ClaudeSessions::write_pasted_file" => write_pasted_file(params),
        "ClaudeSessions::read_pending_question" => read_pending_question(params),
        "ClaudeSessions::question_hook_is_installed" => question_hook_is_installed(),
        "ClaudeSessions::read_live_message" => read_live_message(params),
        "ClaudeSessions::install_question_hook" => install_question_hook(),
        "ClaudeSessions::read_subagent_transcript_tail" => read_subagent_transcript_tail(params),
        "ClaudeSessions::subagent_transcript_path" => subagent_transcript_path(params),
        "ClaudeSessions::pane_target" => pane_target(params),
        "ClaudeSessions::send_text" => send_text(params),
        "ClaudeSessions::send_escape" => send_escape(params),
        "ClaudeSessions::send_key" => send_key(params),
        "ClaudeSessions::capture_pane" => capture_pane(params),
        // SessionSource::read_file has no twin in remote::claude_sessions; the SSH
        // handler owns this boundary, so the JSON-RPC path copies that handler.
        "ClaudeSessions::read_file" => read_file(params),
        _ => bail!("unknown claude sessions method: {method}"),
    }
}

fn home_directory() -> PathBuf {
    paths::home_dir().to_path_buf()
}

fn refuse_if_claude_directory_restricted(fs_rpc: &FsRpc) -> Result<()> {
    let claude_directory = home_directory().join(".claude");
    if fs_rpc.path_escapes_restricted_root(&claude_directory)? {
        bail!(
            "ZED_WEB_RESTRICT_PATHS is enabled and the Claude sessions directory {} is outside the workspace, so Claude sessions are unavailable in this deployment",
            claude_directory.display()
        );
    }
    Ok(())
}

fn required_string(params: &Value, key: &str) -> Result<String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("missing {key}"))
}

fn optional_string(params: &Value, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn u64_param(params: &Value, key: &str) -> u64 {
    params.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn optional_base64(params: &Value, key: &str) -> Result<Vec<u8>> {
    let Some(encoded) = params.get(key).and_then(Value::as_str) else {
        return Ok(Vec::new());
    };
    BASE64
        .decode(encoded)
        .with_context(|| format!("decoding {key}"))
}

fn required_base64(params: &Value, key: &str) -> Result<Vec<u8>> {
    let encoded = params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing {key}"))?;
    BASE64
        .decode(encoded)
        .with_context(|| format!("decoding {key}"))
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn sanitized_pane_target(params: &Value) -> Result<String> {
    let pane_target = required_string(params, "pane_target")?;
    remote::claude_sessions::pane_target(&pane_target)
        .ok_or_else(|| anyhow!("invalid tmux pane target: {pane_target:?}"))
}

fn session_json(summary: remote::claude_sessions::SessionSummary) -> Value {
    json!({
        "process_id": summary.session.process_id,
        "session_id": summary.session.session_id,
        "working_directory": path_string(&summary.session.working_directory),
        "version": summary.session.version,
        "name": summary.session.name,
        "status": summary.session.status,
        "updated_at": summary.session.updated_at,
        "tmux_target": summary.session.tmux_target,
        "transcript_path": summary.transcript_path.as_deref().map(path_string),
        "context_tokens": summary.spend.map(|spend| spend.context_tokens).unwrap_or(0),
        "total_cost_usd": summary.spend.and_then(|spend| spend.total_cost_usd),
    })
}

fn subagent_json(summary: remote::claude_sessions::SubagentSummary) -> Value {
    json!({
        "agent_id": summary.agent_id,
        "workflow_run_id": summary.workflow_run_id,
        "agent_type": summary.meta.agent_type,
        "description": summary.meta.description,
        "tool_use_id": summary.meta.tool_use_id,
        "spawn_depth": summary.meta.spawn_depth,
        "model": summary.meta.model,
        "workflow_phase": summary.meta.workflow_phase,
        "transcript_path": path_string(&summary.transcript_path),
        "size": summary.size,
        "workflow_agent_finished": summary.workflow_agent_finished,
        "task_agent_finished": summary.task_agent_finished,
        "request_shape": summary.meta.request_shape,
    })
}

fn tail_json(progress: remote::claude_sessions::TailProgress) -> Value {
    json!({
        "path": progress.path.as_deref().map(path_string),
        "start_offset": progress.start_offset,
        "offset": progress.offset,
        "pending": BASE64.encode(&progress.pending),
        "lines": progress.lines,
        "restarted": progress.restarted,
    })
}

fn list_sessions(params: &Value) -> Result<Value> {
    let home_directory = home_directory();
    let project_root = optional_string(params, "project_root").map(PathBuf::from);
    let sessions = smol::block_on(remote::claude_sessions::list_sessions(
        &home_directory,
        project_root.as_deref(),
    ))?;
    Ok(json!({
        "sessions": sessions.into_iter().map(session_json).collect::<Vec<_>>(),
        "home_directory": path_string(&home_directory),
    }))
}

fn read_transcript_tail(params: &Value) -> Result<Value> {
    let home_directory = home_directory();
    let session_id = required_string(params, "session_id")?;
    let path = transcript_path_to_follow(
        optional_string(params, "path").as_deref(),
        &session_id,
        &home_directory,
    );
    let state = remote::claude_sessions::TailState {
        path,
        offset: u64_param(params, "offset"),
        pending: optional_base64(params, "pending")?,
    };
    let progress =
        remote::claude_sessions::read_transcript_tail(&home_directory, &session_id, state)?;
    Ok(tail_json(progress))
}

fn list_subagents(params: &Value) -> Result<Value> {
    let home_directory = home_directory();
    let session_id = required_string(params, "session_id")?;
    let subagents = smol::block_on(remote::claude_sessions::list_subagents(
        &home_directory,
        &session_id,
    ))?;
    Ok(json!({
        "subagents": subagents.into_iter().map(subagent_json).collect::<Vec<_>>(),
    }))
}

fn list_slash_commands(params: &Value) -> Result<Value> {
    let home_directory = home_directory();
    let project_root = optional_string(params, "project_root").map(PathBuf::from);
    let commands =
        remote::claude_sessions::list_slash_commands(&home_directory, project_root.as_deref());
    Ok(json!({
        "commands": commands
            .into_iter()
            .map(|command| {
                json!({
                    "name": command.name,
                    "description": command.description,
                    "argument_hint": command.argument_hint,
                    "scope": match command.scope {
                        remote::claude_sessions::SlashCommandScope::Builtin => 0,
                        remote::claude_sessions::SlashCommandScope::Project => 1,
                        remote::claude_sessions::SlashCommandScope::User => 2,
                    },
                })
            })
            .collect::<Vec<_>>(),
    }))
}

fn list_files_under(params: &Value) -> Result<Value> {
    let directory = PathBuf::from(required_string(params, "directory")?);
    let query = params
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok(json!({
        "paths": remote::claude_sessions::list_files_under(&directory, &query),
    }))
}

fn write_pasted_file(params: &Value) -> Result<Value> {
    let home_directory = home_directory();
    let name = required_string(params, "name")?;
    let contents = required_base64(params, "contents")?;
    let path = remote::claude_sessions::write_pasted_file(&home_directory, &name, &contents)?;
    Ok(json!({ "path": path }))
}

fn read_pending_question(params: &Value) -> Result<Value> {
    let home_directory = home_directory();
    let session_id = required_string(params, "session_id")?;
    let question = remote::claude_sessions::read_pending_question(&home_directory, &session_id)?;
    Ok(match question {
        Some(question) => json!({
            "tool_use_id": question.tool_use_id,
            "questions": question
                .questions
                .into_iter()
                .map(|question| {
                    json!({
                        "header": question.header,
                        "question": question.question,
                        "options": question
                            .options
                            .into_iter()
                            .map(|option| {
                                json!({
                                    "label": option.label,
                                    "description": option.description,
                                })
                            })
                            .collect::<Vec<_>>(),
                        "multi_select": question.multi_select,
                    })
                })
                .collect::<Vec<_>>(),
        }),
        None => Value::Null,
    })
}

fn question_hook_is_installed() -> Result<Value> {
    Ok(Value::Bool(
        remote::claude_sessions::question_hook_is_installed(&home_directory()),
    ))
}

fn read_live_message(params: &Value) -> Result<Value> {
    let home_directory = home_directory();
    let session_id = required_string(params, "session_id")?;
    Ok(json!(remote::claude_sessions::read_live_message(
        &home_directory,
        &session_id
    )?))
}

fn install_question_hook() -> Result<Value> {
    let backup = remote::claude_sessions::install_question_hook(&home_directory())?;
    Ok(json!({
        "backup_path": backup.as_deref().map(path_string),
    }))
}

fn read_subagent_transcript_tail(params: &Value) -> Result<Value> {
    let home_directory = home_directory();
    let session_id = required_string(params, "session_id")?;
    let agent_id = required_string(params, "agent_id")?;
    let workflow_run_id = optional_string(params, "workflow_run_id");
    let state = remote::claude_sessions::TailState {
        path: None,
        offset: u64_param(params, "offset"),
        pending: optional_base64(params, "pending")?,
    };
    let progress = remote::claude_sessions::read_subagent_transcript_tail(
        &home_directory,
        &session_id,
        &agent_id,
        workflow_run_id.as_deref(),
        state,
    )?;
    Ok(tail_json(progress))
}

fn subagent_transcript_path(params: &Value) -> Result<Value> {
    let home_directory = home_directory();
    let session_id = required_string(params, "session_id")?;
    let agent_id = required_string(params, "agent_id")?;
    let workflow_run_id = optional_string(params, "workflow_run_id");
    Ok(json!(
        remote::claude_sessions::subagent_transcript_path(
            &home_directory,
            &session_id,
            &agent_id,
            workflow_run_id.as_deref(),
        )
        .as_deref()
        .map(path_string)
    ))
}

fn pane_target(params: &Value) -> Result<Value> {
    let tmux_field = required_string(params, "tmux_field")?;
    Ok(json!(remote::claude_sessions::pane_target(&tmux_field)))
}

fn send_text(params: &Value) -> Result<Value> {
    let pane_target = sanitized_pane_target(params)?;
    let text = required_string(params, "text")?;
    smol::block_on(remote::claude_sessions::send_text(&pane_target, &text))?;
    Ok(Value::Null)
}

fn send_escape(params: &Value) -> Result<Value> {
    let pane_target = sanitized_pane_target(params)?;
    smol::block_on(remote::claude_sessions::send_escape(&pane_target))?;
    Ok(Value::Null)
}

fn send_key(params: &Value) -> Result<Value> {
    let pane_target = sanitized_pane_target(params)?;
    let key = required_string(params, "key")?;
    let key = remote::claude_sessions::PaneKey::from_tmux_name(&key)
        .with_context(|| format!("unknown pane key {key:?}"))?;
    smol::block_on(remote::claude_sessions::send_key(&pane_target, key))?;
    Ok(Value::Null)
}

fn capture_pane(params: &Value) -> Result<Value> {
    let pane_target = sanitized_pane_target(params)?;
    let contents = smol::block_on(remote::claude_sessions::capture_pane(&pane_target))?;
    Ok(json!({ "contents": contents }))
}

fn read_file(params: &Value) -> Result<Value> {
    let home_directory = home_directory();
    let file_path = PathBuf::from(required_string(params, "path")?);
    validate_claude_file_path(&file_path, &home_directory)?;

    const MAXIMUM_READ_BYTES: u64 = 4 * 1024 * 1024;
    let requested = u64_param(params, "max_bytes");
    let byte_limit = if requested == 0 {
        MAXIMUM_READ_BYTES
    } else {
        requested.min(MAXIMUM_READ_BYTES)
    };

    let mut file = std::fs::File::open(&file_path)?;
    let mut read_buffer = Vec::new();
    file.by_ref()
        .take(byte_limit + 1)
        .read_to_end(&mut read_buffer)?;
    let truncated = read_buffer.len() as u64 > byte_limit;
    if truncated {
        read_buffer.truncate(byte_limit as usize);
    }
    Ok(json!({
        "contents": BASE64.encode(&read_buffer),
        "truncated": truncated,
    }))
}

// Copied from the SSH handler: a path out of a request is not evidence of anything,
// so a tail only follows the transcript of the session it names.
fn transcript_path_to_follow(
    path: Option<&str>,
    session_id: &str,
    home_directory: &Path,
) -> Option<PathBuf> {
    let path = PathBuf::from(path?);
    let projects_directory = home_directory.join(".claude").join("projects");
    let expected_file_name = format!("{session_id}.jsonl");

    let names_this_sessions_transcript = path
        .file_name()
        .is_some_and(|file_name| file_name == OsStr::new(&expected_file_name));
    if !path.is_absolute()
        || !path.starts_with(&projects_directory)
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        || !names_this_sessions_transcript
    {
        return None;
    }

    Some(path)
}

// Copied from the SSH handler: the files this protocol may read back are the tool
// outputs Claude Code persists beside a transcript, under `~/.claude/projects`.
fn validate_claude_file_path(path: &Path, home_directory: &Path) -> Result<()> {
    if !path.is_absolute() {
        anyhow::bail!("path must be absolute: {}", path.display());
    }
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        anyhow::bail!("path must not contain '..' components: {}", path.display());
    }
    let allowed_directory = home_directory.join(".claude").join("projects");
    if !path.starts_with(&allowed_directory) {
        anyhow::bail!(
            "path {} is outside the allowed directory {}",
            path.display(),
            allowed_directory.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_rpc::FsRpc;
    use serde_json::json;
    use std::time::Duration;

    const DISPATCH_TIMEOUT: Duration = Duration::from_secs(15);

    #[derive(Debug, PartialEq, Eq)]
    enum ClaudeDispatchOutcome {
        Success { session_count: Option<usize> },
        Refused { mentions_setting: bool },
    }

    fn dispatch_with_timeout(
        fs_rpc: &FsRpc,
        method: &str,
        params: &Value,
    ) -> Result<Value, String> {
        let method = method.to_string();
        let params = params.clone();
        let fs_rpc = fs_rpc.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if sender
                .send(dispatch(&fs_rpc, &method, &params).map_err(|error| error.to_string()))
                .is_err()
            {
                return;
            }
        });
        receiver
            .recv_timeout(DISPATCH_TIMEOUT)
            .map_err(|_| "timed out dispatching ClaudeSessions RPC".to_string())?
    }

    fn list_sessions_outcome(fs_rpc: &FsRpc) -> Result<ClaudeDispatchOutcome, String> {
        match dispatch_with_timeout(fs_rpc, "ClaudeSessions::list_sessions", &json!({})) {
            Ok(value) => Ok(ClaudeDispatchOutcome::Success {
                session_count: value
                    .get("sessions")
                    .and_then(Value::as_array)
                    .map(Vec::len),
            }),
            Err(error) if error == "timed out dispatching ClaudeSessions RPC" => Err(error),
            Err(error) => Ok(ClaudeDispatchOutcome::Refused {
                mentions_setting: error.contains("ZED_WEB_RESTRICT_PATHS"),
            }),
        }
    }

    #[test]
    fn list_sessions_refuses_when_claude_directory_is_outside_the_restricted_root() {
        let root = tempfile::tempdir().expect("temp workspace");
        let fs_rpc = FsRpc::new(root.path().to_path_buf(), true).expect("FsRpc");
        let outcome = list_sessions_outcome(&fs_rpc)
            .expect("list_sessions should finish within the dispatch timeout");

        assert_eq!(
            outcome,
            ClaudeDispatchOutcome::Refused {
                mentions_setting: true,
            },
            "an empty session list would look like 'you have no sessions'; restriction must be an error that names ZED_WEB_RESTRICT_PATHS"
        );
    }

    #[test]
    fn list_sessions_does_not_name_restrict_paths_when_the_setting_is_off() {
        let root = tempfile::tempdir().expect("temp workspace");
        let fs_rpc = FsRpc::new(root.path().to_path_buf(), false).expect("FsRpc");
        let outcome = list_sessions_outcome(&fs_rpc)
            .expect("list_sessions should finish within the dispatch timeout");

        assert_ne!(
            outcome,
            ClaudeDispatchOutcome::Refused {
                mentions_setting: true,
            },
            "the default (restrict_paths off) must keep listing sessions rather than refusing because of ZED_WEB_RESTRICT_PATHS"
        );
    }

    const CLAUDE_SESSIONS_METHODS: &[&str] = &[
        "ClaudeSessions::list_sessions",
        "ClaudeSessions::read_transcript_tail",
        "ClaudeSessions::list_subagents",
        "ClaudeSessions::list_slash_commands",
        "ClaudeSessions::list_files_under",
        "ClaudeSessions::write_pasted_file",
        "ClaudeSessions::read_pending_question",
        "ClaudeSessions::question_hook_is_installed",
        "ClaudeSessions::read_live_message",
        "ClaudeSessions::install_question_hook",
        "ClaudeSessions::read_subagent_transcript_tail",
        "ClaudeSessions::subagent_transcript_path",
        "ClaudeSessions::pane_target",
        "ClaudeSessions::send_text",
        "ClaudeSessions::send_escape",
        "ClaudeSessions::send_key",
        "ClaudeSessions::capture_pane",
        "ClaudeSessions::read_file",
    ];

    #[test]
    fn every_claude_sessions_rpc_refuses_when_claude_directory_is_outside_the_restricted_root() {
        let root = tempfile::tempdir().expect("temp workspace");
        let fs_rpc = FsRpc::new(root.path().to_path_buf(), true).expect("FsRpc");
        for method in CLAUDE_SESSIONS_METHODS {
            let result = dispatch_with_timeout(&fs_rpc, method, &json!({}));
            assert_eq!(
                result
                    .as_ref()
                    .map(|_| "ok")
                    .map_err(|error| error.contains("ZED_WEB_RESTRICT_PATHS")),
                Err(true),
                "{method} must refuse naming ZED_WEB_RESTRICT_PATHS, got {result:?}"
            );
        }
    }

    #[test]
    fn list_sessions_is_allowed_when_home_is_the_restricted_workspace() {
        let home = paths::home_dir();
        let fs_rpc = FsRpc::new(home.to_path_buf(), true).expect("FsRpc for the home directory");
        let outcome = list_sessions_outcome(&fs_rpc)
            .expect("list_sessions should finish within the dispatch timeout");

        assert_ne!(
            outcome,
            ClaudeDispatchOutcome::Refused {
                mentions_setting: true,
            },
            "serving the user's home with ZED_WEB_RESTRICT_PATHS must still list ~/.claude, which lives inside that root; got {outcome:?}"
        );
    }
}
