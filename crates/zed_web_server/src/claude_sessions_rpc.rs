use std::{
    ffi::OsStr,
    io::Read as _,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use anyhow::{Context as _, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use gpui::BackgroundExecutor;
use serde_json::{Value, json};

use crate::fs_rpc::FsRpc;

/// The gpui executor `remote::claude_sessions::compact_session` times its tmux calls on.
/// Requests are answered on tokio's blocking pool, which has none of its own.
static BACKGROUND_EXECUTOR: OnceLock<BackgroundExecutor> = OnceLock::new();

pub fn set_background_executor(executor: BackgroundExecutor) {
    if BACKGROUND_EXECUTOR.set(executor).is_err() {
        tracing::warn!("the Claude sessions RPC executor was already set");
    }
}

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
        "ClaudeSessions::read_events_tail" => read_events_tail(params),
        "ClaudeSessions::read_session_status" => read_session_status(params),
        "ClaudeSessions::install_zed_hooks" => install_zed_hooks(),
        "ClaudeSessions::uninstall_zed_hooks" => uninstall_zed_hooks(),
        "ClaudeSessions::zed_hooks_installed" => zed_hooks_installed(),
        "ClaudeSessions::read_subagent_transcript_tail" => read_subagent_transcript_tail(params),
        "ClaudeSessions::subagent_transcript_path" => subagent_transcript_path(params),
        "ClaudeSessions::pane_target" => pane_target(params),
        "ClaudeSessions::channel_status" => channel_status(params),
        "ClaudeSessions::channel_send_message" => channel_send_message(params),
        "ClaudeSessions::channel_interrupt" => channel_interrupt(params),
        "ClaudeSessions::channel_answer_permission" => channel_answer_permission(params),
        "ClaudeSessions::read_channel_inbox_tail" => read_channel_inbox_tail(params),
        "ClaudeSessions::read_keep_alive" => read_keep_alive(&home_directory(), params),
        "ClaudeSessions::write_keep_alive" => write_keep_alive(&home_directory(), params),
        "ClaudeSessions::compact_session" => compact_session(params),
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

fn required_u64(params: &Value, key: &str) -> Result<u64> {
    params
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("missing {key}"))
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

fn session_json(summary: remote::claude_sessions::SessionSummary) -> Value {
    json!({
        "process_id": summary.session.process_id,
        "session_id": summary.session.session_id,
        "working_directory": path_string(&summary.session.working_directory),
        "version": summary.session.version,
        "name": summary.session.name,
        "status": summary.session.status,
        "updated_at": summary.session.updated_at,
        "started_at": summary.session.started_at,
        "tmux_target": summary.session.tmux_target,
        "transcript_path": summary.transcript_path.as_deref().map(path_string),
        "context_tokens": summary.spend.map(|spend| spend.context_tokens).unwrap_or(0),
        "total_cost_usd": summary.spend.and_then(|spend| spend.total_cost_usd),
        "bridge_session_id": summary.session.bridge_session_id,
        "last_answer_at_ms": summary.spend.and_then(|spend| spend.last_answer_at_ms),
        "cache_ttl": summary.spend.map(|spend| cache_ttl_code(spend.cache_ttl)).unwrap_or(0),
    })
}

// The codes `proto::ClaudeSession::cache_ttl` uses, copied from the SSH handler so both
// transports mean the same thing by them.
fn cache_ttl_code(cache_ttl: remote::claude_sessions::CacheTtl) -> u32 {
    match cache_ttl {
        remote::claude_sessions::CacheTtl::Unknown => 0,
        remote::claude_sessions::CacheTtl::FiveMinutes => 1,
        remote::claude_sessions::CacheTtl::OneHour => 2,
    }
}

fn claude_pid_param(params: &Value) -> Result<u32> {
    let claude_pid = params
        .get("claude_pid")
        .and_then(Value::as_u64)
        .and_then(|claude_pid| u32::try_from(claude_pid).ok())
        .unwrap_or(0);
    if claude_pid == 0 {
        bail!("claude_pid must not be 0");
    }
    Ok(claude_pid)
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
        "skipped_bytes": progress.skipped_bytes,
    })
}

/// How much of the end of a file a first read may deliver. A client that sends none is
/// one that cannot be told what was skipped, so it is given the whole file.
fn window_param(params: &Value) -> u64 {
    params
        .get("window")
        .and_then(Value::as_u64)
        .unwrap_or(u64::MAX)
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
        "liveness_unavailable_reason": remote::claude_sessions::liveness_unavailable_reason(),
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
    let progress = remote::claude_sessions::read_transcript_tail_within(
        &home_directory,
        &session_id,
        state,
        window_param(params),
    )?;
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
    let session_id = optional_string(params, "session_id");
    let project_root = remote::claude_sessions::slash_command_project_root(
        &home_directory,
        session_id.as_deref(),
        project_root.as_deref(),
    )?;
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
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let directory = remote::claude_sessions::session_files_directory(
        &home_directory(),
        session_id,
        &PathBuf::from(required_string(params, "directory")?),
    )?;
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

fn read_events_tail(params: &Value) -> Result<Value> {
    let home_directory = home_directory();
    let session_id = required_string(params, "session_id")?;
    let state = remote::claude_sessions::TailState {
        path: None,
        offset: u64_param(params, "offset"),
        pending: optional_base64(params, "pending")?,
    };
    let progress = remote::claude_sessions::read_events_tail_within(
        &home_directory,
        &session_id,
        state,
        window_param(params),
    )?;
    Ok(tail_json(progress))
}

fn read_session_status(params: &Value) -> Result<Value> {
    let session_id = required_string(params, "session_id")?;
    Ok(json!(remote::claude_sessions::read_session_status(
        &home_directory(),
        &session_id
    )?))
}

fn install_zed_hooks() -> Result<Value> {
    Ok(
        match remote::claude_sessions::install_zed_hooks(&home_directory())? {
            remote::claude_sessions::HookInstallOutcome::AlreadyCurrent => {
                json!({ "outcome": "already_current" })
            }
            remote::claude_sessions::HookInstallOutcome::ScriptsRefreshed => {
                json!({ "outcome": "scripts_refreshed" })
            }
            remote::claude_sessions::HookInstallOutcome::Installed { backup_path } => json!({
                "outcome": "installed",
                "backup_path": backup_path.as_deref().map(path_string),
            }),
        },
    )
}

fn uninstall_zed_hooks() -> Result<Value> {
    remote::claude_sessions::uninstall_zed_hooks(&home_directory())?;
    Ok(Value::Null)
}

fn zed_hooks_installed() -> Result<Value> {
    Ok(Value::Bool(remote::claude_sessions::zed_hooks_installed(
        &home_directory(),
    )))
}

fn channel_status(params: &Value) -> Result<Value> {
    let claude_pid = claude_pid_param(params)?;
    let status = remote::claude_sessions::channel_status(
        &home_directory(),
        claude_pid,
        remote::claude_sessions::now_millis(),
    );
    Ok(json!({
        "live": status.live,
        "heartbeat_at_ms": status.heartbeat_at_ms,
        "server_pid": status.server_pid,
        "features": status.features,
    }))
}

fn channel_send_message(params: &Value) -> Result<Value> {
    let claude_pid = claude_pid_param(params)?;
    let content = required_string(params, "content")?;
    if content.len() > remote::claude_sessions::CHANNEL_MESSAGE_MAX_BYTES {
        bail!("message content exceeds 4 MiB");
    }
    let outbox_file =
        remote::claude_sessions::channel_send_message(&home_directory(), claude_pid, &content)?;
    Ok(json!({ "outbox_file": outbox_file }))
}

fn channel_interrupt(params: &Value) -> Result<Value> {
    let claude_pid = claude_pid_param(params)?;
    let reason = required_string(params, "reason")?;
    if reason.chars().count() > remote::claude_sessions::CHANNEL_INTERRUPT_REASON_MAX_CHARS {
        bail!(
            "interrupt reason exceeds {} characters",
            remote::claude_sessions::CHANNEL_INTERRUPT_REASON_MAX_CHARS
        );
    }
    let outbox_file =
        remote::claude_sessions::channel_interrupt(&home_directory(), claude_pid, &reason)?;
    Ok(json!({ "outbox_file": outbox_file }))
}

fn channel_answer_permission(params: &Value) -> Result<Value> {
    let claude_pid = claude_pid_param(params)?;
    let request_id = required_string(params, "request_id")?;
    let allow = params
        .get("allow")
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow!("missing allow"))?;
    let outbox_file = remote::claude_sessions::channel_answer_permission(
        &home_directory(),
        claude_pid,
        &request_id,
        allow,
    )?;
    Ok(json!({ "outbox_file": outbox_file }))
}

fn read_channel_inbox_tail(params: &Value) -> Result<Value> {
    let claude_pid = claude_pid_param(params)?;
    let state = remote::claude_sessions::TailState {
        path: None,
        offset: u64_param(params, "offset"),
        pending: optional_base64(params, "pending")?,
    };
    let progress = remote::claude_sessions::read_channel_inbox_tail_within(
        &home_directory(),
        claude_pid,
        state,
        window_param(params),
    )?;
    Ok(tail_json(progress))
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
    let progress = remote::claude_sessions::read_subagent_transcript_tail_within(
        &home_directory,
        &session_id,
        &agent_id,
        workflow_run_id.as_deref(),
        state,
        window_param(params),
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

fn keep_alive_record_json(record: remote::claude_sessions::KeepAliveRecord) -> Value {
    json!({
        "session_id": record.session_id,
        "revision": record.revision,
        "state_json": record.state_json,
    })
}

fn read_keep_alive(home_directory: &Path, params: &Value) -> Result<Value> {
    let session_ids = params
        .get("session_ids")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing session_ids"))?
        .iter()
        .map(|session_id| {
            session_id
                .as_str()
                .map(ToOwned::to_owned)
                .ok_or_else(|| anyhow!("session_ids must be strings"))
        })
        .collect::<Result<Vec<_>>>()?;
    let records = remote::claude_sessions::read_keep_alive_records(home_directory, &session_ids)?;
    Ok(json!({
        "records": records.into_iter().map(keep_alive_record_json).collect::<Vec<_>>(),
    }))
}

fn write_keep_alive(home_directory: &Path, params: &Value) -> Result<Value> {
    let session_id = required_string(params, "session_id")?;
    let expected_revision = required_u64(params, "expected_revision")?;
    let state_json = required_string(params, "state_json")?;
    let write = remote::claude_sessions::write_keep_alive_record(
        home_directory,
        &session_id,
        expected_revision,
        &state_json,
    )?;
    Ok(match write {
        remote::claude_sessions::KeepAliveWrite::Applied(record) => json!({
            "applied": true,
            "record": keep_alive_record_json(record),
        }),
        remote::claude_sessions::KeepAliveWrite::Conflict(record) => json!({
            "applied": false,
            "record": record.map(keep_alive_record_json),
        }),
    })
}

fn compact_session(params: &Value) -> Result<Value> {
    let session_id = required_string(params, "session_id")?;
    let executor = BACKGROUND_EXECUTOR
        .get()
        .context("compacting a session needs the server's gpui executor, which is not running")?;
    smol::block_on(remote::claude_sessions::compact_session(
        &home_directory(),
        &session_id,
        executor,
    ))?;
    Ok(json!({}))
}

fn pane_target(params: &Value) -> Result<Value> {
    let tmux_field = required_string(params, "tmux_field")?;
    Ok(json!(remote::claude_sessions::pane_target(&tmux_field)))
}

fn read_file(params: &Value) -> Result<Value> {
    let home_directory = home_directory();
    let file_path = PathBuf::from(required_string(params, "path")?);
    match optional_string(params, "session_id") {
        Some(session_id) => {
            validate_claude_attachment_path(&file_path, &home_directory, &session_id)?
        }
        None => validate_claude_file_path(&file_path, &home_directory)?,
    }

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

// Copied from the SSH handler: an attachment is judged against the working directory
// the session registered, never one the request names.
fn validate_claude_attachment_path(
    path: &Path,
    home_directory: &Path,
    session_id: &str,
) -> Result<()> {
    let working_directory =
        remote::claude_sessions::session_working_directory(home_directory, session_id)
            .with_context(|| format!("no session is registered as {session_id}"))?;
    anyhow::ensure!(
        remote::claude_sessions::attachment_is_readable(path, &working_directory),
        "path {} is outside the working directory of session {session_id}",
        path.display(),
    );
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
        "ClaudeSessions::read_events_tail",
        "ClaudeSessions::read_session_status",
        "ClaudeSessions::install_zed_hooks",
        "ClaudeSessions::uninstall_zed_hooks",
        "ClaudeSessions::zed_hooks_installed",
        "ClaudeSessions::read_subagent_transcript_tail",
        "ClaudeSessions::subagent_transcript_path",
        "ClaudeSessions::pane_target",
        "ClaudeSessions::channel_status",
        "ClaudeSessions::channel_send_message",
        "ClaudeSessions::channel_interrupt",
        "ClaudeSessions::channel_answer_permission",
        "ClaudeSessions::read_channel_inbox_tail",
        "ClaudeSessions::read_file",
        "ClaudeSessions::read_keep_alive",
        "ClaudeSessions::write_keep_alive",
        "ClaudeSessions::compact_session",
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

    /// The browser decides from this whether there is earlier history to offer, so a
    /// first read that skipped the start of a file has to say so on the wire.
    #[test]
    fn a_tail_answer_reports_the_bytes_it_skipped() {
        let answer = tail_json(remote::claude_sessions::TailProgress {
            path: None,
            start_offset: 0,
            offset: 100,
            pending: Vec::new(),
            lines: vec!["{}".to_string()],
            restarted: false,
            skipped_bytes: 97,
        });
        assert_eq!(answer["skipped_bytes"], json!(97), "answer was {answer}");
    }

    #[test]
    fn keep_alive_round_trips_through_the_json_rpc() {
        let home = tempfile::tempdir().expect("temp home");
        let state = r#"{"mode":"warm"}"#;

        let first = write_keep_alive(
            home.path(),
            &json!({ "session_id": "session-a", "expected_revision": 0, "state_json": state }),
        )
        .expect("writing a new record");
        assert_eq!(
            first,
            json!({
                "applied": true,
                "record": { "session_id": "session-a", "revision": 1, "state_json": state },
            })
        );

        let stale = write_keep_alive(
            home.path(),
            &json!({ "session_id": "session-a", "expected_revision": 0, "state_json": r#"{"mode":"off"}"# }),
        )
        .expect("a stale write is answered, not failed");
        assert_eq!(
            stale,
            json!({
                "applied": false,
                "record": { "session_id": "session-a", "revision": 1, "state_json": state },
            }),
            "a write at an old revision must report what the host holds instead of replacing it"
        );

        let read = read_keep_alive(
            home.path(),
            &json!({ "session_ids": ["session-a", "session-without-a-record"] }),
        )
        .expect("reading records");
        assert_eq!(
            read,
            json!({
                "records": [{ "session_id": "session-a", "revision": 1, "state_json": state }],
            })
        );
    }

    #[test]
    fn a_conflict_with_no_record_sends_a_null_record() {
        let home = tempfile::tempdir().expect("temp home");
        let answer = write_keep_alive(
            home.path(),
            &json!({ "session_id": "session-a", "expected_revision": 3, "state_json": "{}" }),
        )
        .expect("a stale write is answered, not failed");
        assert_eq!(answer, json!({ "applied": false, "record": null }));
    }

    #[test]
    fn keep_alive_params_are_required() {
        let home = tempfile::tempdir().expect("temp home");
        for (params, missing) in [
            (
                json!({ "expected_revision": 0, "state_json": "{}" }),
                "session_id",
            ),
            (
                json!({ "session_id": "a", "state_json": "{}" }),
                "expected_revision",
            ),
            (
                json!({ "session_id": "a", "expected_revision": 0 }),
                "state_json",
            ),
        ] {
            let error = write_keep_alive(home.path(), &params)
                .expect_err("a write missing a parameter must fail");
            assert_eq!(error.to_string(), format!("missing {missing}"));
        }
        let error = read_keep_alive(home.path(), &json!({ "session_ids": ["a", 1] }))
            .expect_err("a non-string session id must fail");
        assert_eq!(error.to_string(), "session_ids must be strings");
        let error = read_keep_alive(home.path(), &json!({}))
            .expect_err("a read without session ids must fail");
        assert_eq!(error.to_string(), "missing session_ids");
        assert!(
            !home.path().join(".claude").exists(),
            "a rejected request must not touch the host"
        );
    }

    #[test]
    fn a_listed_session_carries_when_its_process_started() {
        let answer = session_json(remote::claude_sessions::SessionSummary {
            session: remote::claude_sessions::parse_registered_session(
                r#"{"pid":7,"sessionId":"session-a","cwd":"/work","procStart":"","version":"2","kind":"interactive","startedAt":1700000000000}"#,
            )
            .expect("parsing a registration"),
            transcript_path: None,
            spend: None,
        });
        assert_eq!(
            answer["started_at"],
            json!(1_700_000_000_000_i64),
            "answer was {answer}"
        );
    }

    /// Prints how large the first answer for a real session is. Run by hand with
    /// `ZED_MEASURE_SESSION_ID=<id> cargo test -p zed_web_server measure_first_transcript_read -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn measure_first_transcript_read() {
        let Ok(session_id) = std::env::var("ZED_MEASURE_SESSION_ID") else {
            return;
        };
        for window in [
            None,
            Some(remote::claude_sessions::TAIL_FIRST_READ_WINDOW_BYTES),
        ] {
            let answer = read_transcript_tail(
                &json!({ "session_id": session_id, "offset": 0, "window": window }),
            )
            .expect("reading the transcript");
            let text = answer.to_string();
            println!(
                "first answer with window {window:?}: {} chars, start_offset={} offset={} skipped_bytes={} lines={}",
                text.len(),
                answer["start_offset"],
                answer["offset"],
                answer["skipped_bytes"],
                answer["lines"].as_array().map_or(0, Vec::len)
            );
        }
    }
}
