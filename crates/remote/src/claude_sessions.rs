//! The parts of Claude Code session tracking that have to run on the machine the
//! sessions live on, whether that is this one or the far end of a remote connection.
//!
//! Everything here is either a pure function over text a registration or a transcript
//! contains, or the thinnest possible wrapper around the one piece of IO it needs. The
//! liveness rules in particular must exist exactly once: a second copy of them drifts
//! from this one and the two answers cannot both be right.

use std::{
    fs,
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};
use serde::Deserialize;
use smol::io::AsyncWriteExt as _;
use util::{ResultExt as _, command::Stdio};

/// No cutoff is applied to `updatedAt`, because it is not a heartbeat: on this machine
/// every registration carries `updatedAt == statusUpdatedAt`, so it only moves when the
/// session changes status. A session sitting idle waiting for its user, or busy on one
/// long tool call, leaves it untouched for hours while the process is plainly alive.
/// That makes the process itself the only liveness signal there is.
pub const HEARTBEAT_CUTOFF_MILLIS: i64 = i64::MAX;

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

/// Reads every registration in the directory. Liveness is decided later, by a caller
/// that pairs these with [`visible_sessions`], because it needs a process lookup this
/// cannot perform.
pub fn read_registrations(registry_directory: &Path) -> Result<Vec<RegisteredSession>> {
    let directory_entries = match fs::read_dir(registry_directory) {
        Ok(directory_entries) => directory_entries,
        // A Claude Code that writes no registrations leaves no directory behind, which
        // means this machine is running no sessions — not a failure to report to the
        // user. Any other failure, a directory that cannot be listed included, is real.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", registry_directory.display()));
        }
    };

    let mut sessions = Vec::new();
    for directory_entry in directory_entries {
        let Some(directory_entry) = directory_entry.log_err() else {
            continue;
        };
        let path = directory_entry.path();

        // Only `*.json` is read. The sibling `*.key` files hold the credential for the
        // session's messaging socket and are none of this store's business.
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }

        // One unreadable or half-written registration must not hide every other session,
        // so each failure is logged and skipped instead of ending the scan.
        let Some(contents) = fs::read_to_string(&path).log_err() else {
            continue;
        };
        let Some(mut session) = parse_registered_session(&contents).log_err() else {
            continue;
        };

        // Normalized on both sides of the comparison below, because `ps` pads a
        // single-digit day of month with an extra space that the registration lacks.
        session.process_start = normalize_whitespace(&session.process_start);
        sessions.push(session);
    }

    Ok(sessions)
}

/// Start times of the given processes, in the same format the registration files use.
///
/// Claude Code records `procStart` in whatever form its own platform reads it in, and the
/// two platforms do not agree, so neither does this:
///
/// * On Linux it records field 22 of `/proc/<pid>/stat` verbatim — the process's start
///   time in clock ticks since boot — so that file is read back directly. Comparing a
///   registration written there against a wall-clock date is what hid every session on a
///   Linux host: the two strings can never be equal, so every live session was read as a
///   reused pid and filtered out of the list with nothing reported.
/// * On macOS it records the date `ps -o lstart=` prints.
///
/// Reading another process's environment is not an option on either: on macOS
/// `ps eww` returns nothing for processes owned by other sessions.
#[cfg(target_os = "linux")]
pub async fn process_start_times(process_ids: Vec<u32>) -> HashMap<u32, String> {
    let mut start_times = HashMap::default();
    for process_id in process_ids {
        // A process that has gone leaves no entry, which is what an absent start time
        // says to `liveness`.
        let Ok(stat) = smol::fs::read_to_string(format!("/proc/{process_id}/stat")).await else {
            continue;
        };
        if let Some(start_time) = start_time_in_proc_stat(&stat) {
            start_times.insert(process_id, start_time);
        }
    }
    start_times
}

/// Field 22 of a `/proc/<pid>/stat` line, the process's start time in clock ticks since
/// boot.
///
/// Split at the last `)` rather than counted from the left: field 2 is the executable's
/// name in parentheses, and a name is free to hold both spaces and parentheses of its
/// own, which would shift every field after it.
pub fn start_time_in_proc_stat(stat: &str) -> Option<String> {
    let after_name = stat.rsplit_once(')')?.1;
    // The fields after the name begin at field 3, so field 22 is the twentieth of them.
    after_name
        .split_whitespace()
        .nth(19)
        .map(|start_time| start_time.to_string())
}

#[cfg(all(unix, not(target_os = "linux")))]
pub async fn process_start_times(process_ids: Vec<u32>) -> HashMap<u32, String> {
    let mut start_times = HashMap::default();
    if process_ids.is_empty() {
        return start_times;
    }

    let pid_argument = process_ids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");

    let mut command = util::command::new_command("ps");
    command.args(["-o", "pid=,lstart=", "-p", &pid_argument]);
    // `lstart` is rendered in the zone `ps` runs in, while Claude Code writes `procStart`
    // in UTC, so without this the two strings differ by the machine's offset and every
    // running session compares as a reused pid.
    command.env("TZ", "UTC");

    // A non-zero exit only means none of the pids are running, and the empty map that
    // results says exactly that.
    let Some(output) = command.output().await.log_err() else {
        return start_times;
    };

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split_whitespace();
        let Some(process_id) = fields.next().and_then(|field| field.parse::<u32>().ok()) else {
            continue;
        };
        let start_time = fields.collect::<Vec<_>>().join(" ");
        if !start_time.is_empty() {
            start_times.insert(process_id, start_time);
        }
    }

    start_times
}

/// Windows has no equivalent of the registration files this reads, so there is nothing to
/// check liveness against and every session is reported as gone.
#[cfg(not(unix))]
pub async fn process_start_times(_process_ids: Vec<u32>) -> HashMap<u32, String> {
    HashMap::default()
}

pub fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A clock that is behind the epoch cannot happen in practice; reporting zero keeps every
/// session visible, which is the safer failure for a list the user is looking at.
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// Locates `<home>/.claude/projects/*/<sessionId>.jsonl`.
///
/// The directory under `projects` is a slug derived from the session's working directory
/// by an undocumented rule, so the directory is searched rather than the name computed.
pub fn find_transcript(home_directory: &Path, session_id: &str) -> Option<PathBuf> {
    let projects_directory = home_directory.join(".claude").join("projects");
    let file_name = transcript_file_name(session_id)?;

    for directory_entry in fs::read_dir(projects_directory).ok()? {
        let Some(directory_entry) = directory_entry.log_err() else {
            continue;
        };
        let candidate = directory_entry.path().join(&file_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    None
}

/// `name` when it can only ever name one entry inside whichever directory it is joined
/// onto, and `None` otherwise.
///
/// Every id this module joins onto a path is untrusted text: session ids arrive from a
/// registration file and, for a remote project, straight out of a request, while agent
/// and workflow run ids come out of a transcript's JSON. Joined unchecked, an id carrying
/// a separator or a parent component names something outside the projects directory, and
/// what this module opens is handed back to its caller line by line — the sibling
/// `<home>/.claude/sessions` holds the credentials for the sessions' messaging sockets.
fn single_path_component(name: &str) -> Option<&str> {
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(only_component)), None)
            if only_component == std::ffi::OsStr::new(name) =>
        {
            Some(name)
        }
        _ => None,
    }
}

/// The name a session's transcript file has, or `None` when the session id could name
/// something other than a file inside one project's directory.
fn transcript_file_name(session_id: &str) -> Option<String> {
    // Appending a suffix to a lone normal component cannot introduce a component of its
    // own, so the composed name stays inside one project's directory.
    Some(format!("{}.jsonl", single_path_component(session_id)?))
}

#[derive(Clone)]
pub struct TailState {
    pub path: Option<PathBuf>,
    pub offset: u64,
    pub pending: Vec<u8>,
}

pub struct TailProgress {
    pub path: Option<PathBuf>,
    /// The offset this read began at. A tail only ever accepts progress that starts
    /// where it currently is, which is what distinguishes a read that was in flight when
    /// the tail restarted from one belonging to the tail that is being followed now.
    pub start_offset: u64,
    pub offset: u64,
    pub pending: Vec<u8>,
    pub lines: Vec<String>,
    /// The file was replaced or truncated, so everything absorbed so far is stale.
    pub restarted: bool,
}

pub fn read_transcript_tail(
    home_directory: &Path,
    session_id: &str,
    mut state: TailState,
) -> Result<TailProgress> {
    let start_offset = state.offset;
    let mut restarted = false;

    let path = match state.path.clone().filter(|path| path.is_file()) {
        Some(path) => path,
        None => {
            let Some(path) = find_transcript(home_directory, session_id) else {
                return Ok(tail_of_no_file(&state));
            };
            // Either the file has only just appeared, or the one being followed was
            // replaced; in both cases it has to be read from the beginning.
            restarted = state.offset > 0 || !state.pending.is_empty();
            state.offset = 0;
            state.pending.clear();
            path
        }
    };

    read_appended_lines(path, state, start_offset, restarted)
}

/// Follows one subagent's conversation, naming the file by the three ids on every read.
///
/// Separate from [`read_transcript_tail`] rather than a path handed to it, because that
/// function reads a path it cannot open as "the session has started a new conversation"
/// and looks the session's own transcript up instead. For an agent that fallback is a
/// disclosure rather than a recovery: the main conversation's lines would be handed back
/// under the agent's name, and the caller has no way to tell. An agent whose transcript
/// cannot be named — a deleted file, or an id that could name something outside the
/// session's own directory — therefore reports no file and no lines.
pub fn read_subagent_transcript_tail(
    home_directory: &Path,
    session_id: &str,
    agent_id: &str,
    workflow_run_id: Option<&str>,
    state: TailState,
) -> Result<TailProgress> {
    // Resolved here rather than taken from `state.path`, so that the three ids go through
    // the same boundary on every read and no path can be followed that they do not name.
    let Some(path) =
        subagent_transcript_path(home_directory, session_id, agent_id, workflow_run_id)
    else {
        return Ok(tail_of_no_file(&state));
    };

    let start_offset = state.offset;
    read_appended_lines(path, state, start_offset, false)
}

/// The answer to a read whose file cannot be named at all: nothing was read, and anything
/// the caller has absorbed so far belongs to a file that is no longer there.
fn tail_of_no_file(state: &TailState) -> TailProgress {
    TailProgress {
        path: None,
        start_offset: state.offset,
        offset: 0,
        pending: Vec::new(),
        lines: Vec::new(),
        restarted: state.offset > 0 || !state.pending.is_empty(),
    }
}

/// Reads whatever `path` has grown by since `state.offset` and splits it into whole lines.
fn read_appended_lines(
    path: PathBuf,
    mut state: TailState,
    start_offset: u64,
    mut restarted: bool,
) -> Result<TailProgress> {
    let size = fs::metadata(&path)
        .with_context(|| format!("reading metadata of {}", path.display()))?
        .len();

    // Transcripts are only ever appended to, so a file shorter than what has already been
    // read is a different file: discard the recorded position and read it from the start.
    if size < state.offset {
        restarted = true;
        state.offset = 0;
        state.pending.clear();
    }

    let mut lines = Vec::new();
    if size > state.offset {
        let mut file =
            fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        file.seek(SeekFrom::Start(state.offset))
            .with_context(|| format!("seeking in {}", path.display()))?;

        let mut appended = Vec::new();
        file.read_to_end(&mut appended)
            .with_context(|| format!("reading {}", path.display()))?;

        state.offset = state.offset.saturating_add(appended.len() as u64);
        state.pending.extend_from_slice(&appended);
        lines = split_complete_lines(&mut state.pending);
    }

    Ok(TailProgress {
        path: Some(path),
        start_offset,
        offset: state.offset,
        pending: state.pending,
        lines,
        restarted,
    })
}

/// The record Claude Code writes when it compacts a conversation, which says how much
/// context the compaction left behind.
const COMPACT_BOUNDARY_SUBTYPE: &str = "compact_boundary";

/// How much of a transcript's end is read to find what the session is costing. Large
/// enough to hold the last few dozen records — an answer's usage and one of Claude Code's
/// cost snapshots are both within a handful of records of the end — and small enough to
/// read for every listed session on every scan of a directory of multi-megabyte files.
const SPEND_TAIL_BYTES: u64 = 256 * 1024;

/// What a session is costing, as the end of its transcript reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TranscriptSpend {
    /// The context the newest answer was given: what was sent fresh, plus what was read
    /// from the cache, plus what was written into it.
    pub context_tokens: u64,
    /// Claude Code's own total for the session, when one of its `cost-state` snapshots is
    /// within the tail that was read.
    ///
    /// `None` rather than a figure derived from the tail's own token counts: those cover
    /// only the answers in the tail, and a total that is silently partial is worse than
    /// no total at all.
    pub total_cost_usd: Option<f64>,
}

/// Reads the end of a transcript for what its session is costing.
///
/// Only the end of it. This is read for every session in the list on every scan, and a
/// transcript is megabytes of conversation whose last few records hold both answers.
pub fn read_transcript_spend(path: &Path) -> Result<TranscriptSpend> {
    let size = fs::metadata(path)
        .with_context(|| format!("reading metadata of {}", path.display()))?
        .len();

    let mut file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let offset = size.saturating_sub(SPEND_TAIL_BYTES);
    file.seek(SeekFrom::Start(offset))
        .with_context(|| format!("seeking in {}", path.display()))?;

    let mut tail = Vec::new();
    file.read_to_end(&mut tail)
        .with_context(|| format!("reading {}", path.display()))?;

    let text = String::from_utf8_lossy(&tail);
    let mut lines = text.lines();
    if offset > 0 {
        // The read began mid-file, so the first line is whatever it landed in the middle
        // of rather than a record.
        lines.next();
    }

    Ok(spend_in_lines(lines))
}

/// The newest answer's context and the newest cost snapshot among `lines`.
fn spend_in_lines<'a>(lines: impl Iterator<Item = &'a str>) -> TranscriptSpend {
    let mut spend = TranscriptSpend::default();

    for line in lines {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        if let Some(total) = record
            .get("totalCostUSD")
            .and_then(serde_json::Value::as_f64)
        {
            spend.total_cost_usd = Some(total);
            continue;
        }

        // A compaction replaces the conversation the next answer will be given, so the
        // context the last answer measured is no longer what the session carries.
        // `postTokens` counts the conversation the compaction kept and not the system
        // prompt, tool definitions or skills that the next request re-sends, so it reads
        // low until that request measures the whole context and supersedes it below.
        if record.get("subtype").and_then(serde_json::Value::as_str)
            == Some(COMPACT_BOUNDARY_SUBTYPE)
        {
            if let Some(post_tokens) = record
                .get("compactMetadata")
                .and_then(|metadata| metadata.get("postTokens"))
                .and_then(serde_json::Value::as_u64)
            {
                spend.context_tokens = post_tokens;
            }
            continue;
        }

        let Some(usage) = record
            .get("message")
            .and_then(|message| message.get("usage"))
        else {
            continue;
        };
        let tokens = |key: &str| -> u64 {
            usage
                .get(key)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
        };
        // Later lines are newer, so the last answer in the tail is the one that stands.
        spend.context_tokens = tokens("input_tokens")
            .saturating_add(tokens("cache_read_input_tokens"))
            .saturating_add(tokens("cache_creation_input_tokens"));
    }

    spend
}

/// Splits every complete line out of `buffer`, leaving any trailing partial line behind.
///
/// The buffer holds bytes rather than text because a read can stop in the middle of a
/// multi-byte character; only a run of bytes terminated by a newline is known to be a
/// whole line, and only then is it interpreted as text.
pub fn split_complete_lines(buffer: &mut Vec<u8>) -> Vec<String> {
    let mut lines = Vec::new();
    let mut consumed = 0;

    for (index, byte) in buffer.iter().enumerate() {
        if *byte == b'\n' {
            let line = buffer.get(consumed..index).unwrap_or_default();
            // A line that is not valid UTF-8 cannot be valid JSON either, so replacing the
            // bad bytes lets the JSON parser reject just this line.
            lines.push(String::from_utf8_lossy(line).into_owned());
            consumed = index + 1;
        }
    }

    buffer.drain(..consumed);
    lines
}

/// A session as the machine it runs on sees it: the registration, plus where its
/// transcript was found if it has written one yet.
pub struct SessionSummary {
    pub session: RegisteredSession,
    pub transcript_path: Option<PathBuf>,
    /// What the end of the session's transcript says it is costing. `None` when there is
    /// no transcript yet, or when reading its tail failed — a listing is still worth
    /// having without it.
    pub spend: Option<TranscriptSpend>,
}

/// Scans the registry under `home_directory`, keeps the sessions whose process is still
/// the one that registered, and locates each one's transcript.
///
/// `home_directory` is a parameter rather than read from the environment because the
/// answer has to be the home directory of the machine being described, which for a
/// remote project is not the one this process is running under.
pub async fn list_sessions(
    home_directory: &Path,
    project_root: Option<&Path>,
) -> Result<Vec<SessionSummary>> {
    let registry_directory = home_directory.join(".claude").join("sessions");
    let registrations = read_registrations(&registry_directory)?;

    let process_ids: Vec<u32> = registrations
        .iter()
        .map(|session| session.process_id)
        .collect();
    let process_starts = process_start_times(process_ids).await;
    // Both sides of the comparison are normalized here, because `ps` pads a single-digit
    // day of month with a second space that the registration does not have.
    let process_start_of_pid = |process_id: u32| {
        process_starts
            .get(&process_id)
            .map(|start_time| normalize_whitespace(start_time))
    };

    let sessions = visible_sessions(
        registrations,
        project_root,
        now_millis(),
        HEARTBEAT_CUTOFF_MILLIS,
        &process_start_of_pid,
    );

    Ok(sessions
        .into_iter()
        .map(|session| {
            let transcript_path = find_transcript(home_directory, &session.session_id);
            // Read for every session on every scan, which is why only the end of each
            // file is read; see `read_transcript_spend`.
            let spend = transcript_path
                .as_deref()
                .and_then(|path| read_transcript_spend(path).log_err());
            SessionSummary {
                session,
                transcript_path,
                spend,
            }
        })
        .collect())
}

const SUBAGENTS_DIRECTORY: &str = "subagents";
const WORKFLOWS_DIRECTORY: &str = "workflows";
const SUBAGENT_FILE_PREFIX: &str = "agent-";
const SUBAGENT_META_SUFFIX: &str = ".meta.json";
const SUBAGENT_TRANSCRIPT_SUFFIX: &str = ".jsonl";
const WORKFLOW_RUN_ID_LABEL: &str = "Run ID:";
const WORKFLOW_JOURNAL_FILE: &str = "journal.jsonl";
const WORKFLOW_JOURNAL_RESULT_TYPE: &str = "result";
const PENDING_QUESTIONS_DIRECTORY: &str = "pending-questions";
const QUESTION_HOOK_SCRIPT: &str = "hooks/record-pending-question.sh";
const QUESTION_HOOK_TOOL: &str = "AskUserQuestion";
const LIVE_MESSAGES_DIRECTORY: &str = "live-messages";
const LIVE_MESSAGE_HOOK_SCRIPT: &str = "hooks/record-live-message.sh";
const LIVE_MESSAGE_HOOK_EVENT: &str = "MessageDisplay";

/// The `agent-<agentId>.meta.json` sidecar written beside a subagent's transcript.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SubagentMeta {
    /// Free-form text rather than a closed set: beside `general-purpose` and
    /// `workflow-subagent`, this machine holds `fork`, `Explore` and locally defined
    /// types, so a value this version has never seen has to survive the parse.
    #[serde(rename = "agentType")]
    pub agent_type: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Absent on most agents — including some spawned by the `Task` tool — so it cannot
    /// be relied on to pair an agent with the tool call that spawned it.
    #[serde(rename = "toolUseId", default)]
    pub tool_use_id: Option<String>,
    #[serde(rename = "spawnDepth", default = "default_spawn_depth")]
    pub spawn_depth: u32,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(rename = "workflowPhase", default)]
    pub workflow_phase: Option<String>,
}

/// An agent whose meta records no depth was spawned by the session itself.
fn default_spawn_depth() -> u32 {
    1
}

/// Parses one subagent's meta sidecar. Unknown fields are ignored, because the writer
/// ships independently of this reader and adds fields between releases.
pub fn parse_subagent_meta(contents: &str) -> anyhow::Result<SubagentMeta> {
    let meta = serde_json::from_str(contents)?;
    Ok(meta)
}

/// A subagent conversation as the machine that ran it sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentSummary {
    pub agent_id: String,
    /// `Some` for an agent belonging to a `Workflow` run, which keeps its agents one
    /// level deeper, in a directory named after the run.
    pub workflow_run_id: Option<String>,
    pub meta: SubagentMeta,
    pub transcript_path: PathBuf,
    pub size: u64,
    /// What the run's journal says about this agent, for an agent belonging to a
    /// `Workflow` run. `None` for every other agent, and for one whose run has written no
    /// journal this scan could read.
    pub workflow_agent_finished: Option<bool>,
}

/// A question a session has put to its user and is waiting on.
///
/// The CLI writes nothing about a question into the transcript until it has been
/// answered — the assistant message holding the call is not flushed while the call is
/// outstanding — so the terminal is otherwise the only place a waiting question exists.
/// A `PreToolUse` hook records the call as it is made, and this is what it recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingQuestion {
    /// The call this question is. Whether it is still waiting is answered by the
    /// conversation rather than by this file: the hook records a question being asked and
    /// has no way to record it being answered, so a reader tells the two apart by looking
    /// for the tool result that answers this id.
    pub tool_use_id: String,
    pub questions: Vec<Question>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// The short name the terminal shows in its tab strip when a call carries several
    /// questions, and what a reader picks one of them out by.
    pub header: String,
    pub question: String,
    pub options: Vec<QuestionOption>,
    pub multi_select: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    pub description: Option<String>,
}

/// The hook payload, which is the tool call's own input inside the envelope the hook is
/// given. Only the parts a reader draws are named; the envelope carries several other
/// fields, and the writer ships independently of this reader and adds more between
/// releases.
#[derive(Deserialize)]
struct QuestionHookPayload {
    #[serde(rename = "tool_use_id", alias = "toolUseId", default)]
    tool_use_id: Option<String>,
    #[serde(rename = "tool_input", default)]
    tool_input: Option<QuestionHookInput>,
}

#[derive(Deserialize, Default)]
struct QuestionHookInput {
    #[serde(default)]
    questions: Vec<QuestionHookQuestion>,
}

#[derive(Deserialize)]
struct QuestionHookQuestion {
    #[serde(default)]
    header: String,
    #[serde(default)]
    question: String,
    #[serde(default)]
    options: Vec<QuestionHookOption>,
    #[serde(rename = "multiSelect", default)]
    multi_select: bool,
}

#[derive(Deserialize)]
struct QuestionHookOption {
    #[serde(default)]
    label: String,
    #[serde(default)]
    description: Option<String>,
}

/// Parses one recorded question. Unknown fields are ignored, because the CLI that writes
/// the payload ships independently of this reader.
///
/// A payload naming no call, or carrying no question with any option in it, is not a
/// question this side can draw: offering a reader a question with nothing to pick would
/// be worse than leaving the terminal to show it.
pub fn parse_pending_question(contents: &str) -> Result<Option<PendingQuestion>> {
    let payload: QuestionHookPayload = serde_json::from_str(contents)?;
    let Some(tool_use_id) = payload.tool_use_id.filter(|id| !id.is_empty()) else {
        return Ok(None);
    };

    let questions: Vec<Question> = payload
        .tool_input
        .unwrap_or_default()
        .questions
        .into_iter()
        .filter(|question| !question.options.is_empty())
        .map(|question| Question {
            header: question.header,
            question: question.question,
            options: question
                .options
                .into_iter()
                .map(|option| QuestionOption {
                    label: option.label,
                    description: option
                        .description
                        .map(|description| description.trim().to_string())
                        .filter(|description| !description.is_empty()),
                })
                .collect(),
            multi_select: question.multi_select,
        })
        .collect();

    if questions.is_empty() {
        return Ok(None);
    }

    Ok(Some(PendingQuestion {
        tool_use_id,
        questions,
    }))
}

/// A slash command a session will answer to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    /// Without the leading slash, as the reader types it.
    pub name: String,
    pub description: Option<String>,
    /// What the command expects after its name, when it said so.
    pub argument_hint: Option<String>,
    pub scope: SlashCommandScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SlashCommandScope {
    /// Built into the CLI. Listed from this side rather than read from the machine,
    /// because the CLI writes its own commands down nowhere.
    Builtin,
    /// A `.md` under the project's own `.claude/commands`.
    Project,
    /// A `.md` under the user's `~/.claude/commands`.
    User,
}

/// The commands the CLI answers to whatever the machine holds.
///
/// Kept deliberately short: a list this side maintains goes stale, and a stale entry that
/// does nothing is worse than a command the reader types out in full. These are the ones
/// worth a row of their own — everything else is still typed, and still sent.
const BUILTIN_SLASH_COMMANDS: [(&str, &str); 14] = [
    ("clear", "Start a new conversation"),
    ("compact", "Summarise the conversation so far"),
    ("context", "Show what is in the context window"),
    ("cost", "Show what this session has cost"),
    ("agents", "Manage the agents this session can spawn"),
    ("workflows", "Watch the workflows this session is running"),
    ("model", "Change the model"),
    ("effort", "Change the reasoning effort"),
    ("resume", "Resume an earlier conversation"),
    ("status", "Show the session's status"),
    ("usage", "Show what is left of the usage limit"),
    ("config", "Open the settings"),
    ("export", "Export the conversation"),
    ("help", "List every command"),
];

/// Every slash command a session in `project_root` would answer to.
///
/// Ordered so that rows do not move between reads — directory enumeration is in no
/// particular order — with the machine's own commands before the built-in ones: a command
/// someone wrote is the one they are looking for.
pub fn list_slash_commands(
    home_directory: &Path,
    project_root: Option<&Path>,
) -> Vec<SlashCommand> {
    let mut commands = Vec::new();

    if let Some(project_root) = project_root {
        read_slash_commands_in(
            &project_root.join(".claude").join("commands"),
            SlashCommandScope::Project,
            &mut commands,
        );
    }
    read_slash_commands_in(
        &home_directory.join(".claude").join("commands"),
        SlashCommandScope::User,
        &mut commands,
    );

    // Gathered by name first so that two commands of the same name are next to each other
    // — `dedup_by` only removes neighbours — with the project's before the user's, which
    // is the order the scopes are declared in and the order the CLI resolves them.
    commands.sort_by(|left, right| (&left.name, left.scope).cmp(&(&right.name, right.scope)));
    commands.dedup_by(|left, right| left.name == right.name);

    commands.sort_by(|left, right| (left.scope, &left.name).cmp(&(right.scope, &right.name)));

    for (name, description) in BUILTIN_SLASH_COMMANDS {
        if commands.iter().any(|command| command.name == name) {
            continue;
        }
        commands.push(SlashCommand {
            name: name.to_string(),
            description: Some(description.to_string()),
            argument_hint: None,
            scope: SlashCommandScope::Builtin,
        });
    }

    commands
}

/// Appends every command under `directory`, including those in directories of their own,
/// which the CLI names `<directory>:<command>`.
fn read_slash_commands_in(
    directory: &Path,
    scope: SlashCommandScope,
    commands: &mut Vec<SlashCommand>,
) {
    read_slash_commands_under(directory, scope, None, commands);
}

/// How deep a tree of command directories is walked.
///
/// A depth alone is not enough — a symlink cycle would still be walked to the bottom of
/// it — but it is what stops an honestly deep tree from costing an unbounded walk, and
/// it bounds the recursion whatever the filesystem does.
const SLASH_COMMAND_DIRECTORY_DEPTH: usize = 8;

fn read_slash_commands_under(
    directory: &Path,
    scope: SlashCommandScope,
    namespace: Option<&str>,
    commands: &mut Vec<SlashCommand>,
) {
    read_slash_commands_to_depth(
        directory,
        scope,
        namespace,
        commands,
        SLASH_COMMAND_DIRECTORY_DEPTH,
    )
}

fn read_slash_commands_to_depth(
    directory: &Path,
    scope: SlashCommandScope,
    namespace: Option<&str>,
    commands: &mut Vec<SlashCommand>,
    depth_left: usize,
) {
    let Ok(entries) = fs::read_dir(directory) else {
        // A machine with no commands of its own is the ordinary case, not a failure.
        return;
    };

    for entry in entries {
        let Some(entry) = entry.log_err() else {
            continue;
        };
        let path = entry.path();
        let Ok(file_name) = entry.file_name().into_string() else {
            continue;
        };

        // `.git` and the like are the machine's own business, and a command the reader
        // never wrote is not one they can be offered.
        if file_name.starts_with('.') {
            continue;
        }

        if path.is_dir() {
            // A directory symlink pointing at an ancestor is walked forever otherwise,
            // and the walk ends in a stack overflow rather than an error.
            let Some(depth_left) = depth_left.checked_sub(1) else {
                continue;
            };
            let nested = match namespace {
                Some(namespace) => format!("{namespace}:{file_name}"),
                None => file_name,
            };
            read_slash_commands_to_depth(&path, scope, Some(&nested), commands, depth_left);
            continue;
        }

        let Some(stem) = file_name.strip_suffix(".md") else {
            continue;
        };
        let name = match namespace {
            Some(namespace) => format!("{namespace}:{stem}"),
            None => stem.to_string(),
        };
        // One unreadable command must not hide the rest.
        let Some(contents) = fs::read_to_string(&path).log_err() else {
            continue;
        };
        let (description, argument_hint) = slash_command_front_matter(&contents);

        commands.push(SlashCommand {
            name,
            description,
            argument_hint,
            scope,
        });
    }
}

/// The `description` and `argument-hint` a command's front matter records.
///
/// Read line by line rather than as YAML: only two keys are wanted, the file is a prompt
/// with a header rather than a document, and a header this reader cannot parse should
/// cost the description rather than the command.
fn slash_command_front_matter(contents: &str) -> (Option<String>, Option<String>) {
    let mut lines = contents.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None);
    }

    let mut description = None;
    let mut argument_hint = None;
    for line in lines {
        let line = line.trim();
        if line == "---" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches(['"', '\'']).trim();
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "description" => description = Some(value.to_string()),
            "argument-hint" => argument_hint = Some(value.to_string()),
            _ => {}
        }
    }

    (description, argument_hint)
}

/// One piece of a message as the terminal drew it.
#[derive(Deserialize)]
struct LiveMessagePiece {
    #[serde(default)]
    message_id: Option<String>,
    #[serde(default)]
    index: u64,
    #[serde(default)]
    delta: String,
}

/// What the session is saying right now, assembled from the pieces the terminal drew.
///
/// `None` when nothing has been recorded, which includes a machine without the hook and a
/// session that has not said anything since it was installed.
pub fn read_live_message(home_directory: &Path, session_id: &str) -> Result<Option<String>> {
    let Some(session_id) = single_path_component(session_id) else {
        return Ok(None);
    };
    let path = home_directory
        .join(".claude")
        .join(LIVE_MESSAGES_DIRECTORY)
        .join(format!("{session_id}.jsonl"));

    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };

    Ok(assemble_live_message(&contents))
}

/// Puts one message back together from the pieces recorded for it.
///
/// Only the newest message in the file is assembled: the file is truncated when a message
/// begins, but a truncation that did not happen — the hook could not write, or a message
/// began while the file was being read — would otherwise show two messages run together.
///
/// Pieces are ordered by the index they carry rather than by the order they were written,
/// and a piece that is unreadable or repeats an index already seen is skipped: a line is
/// appended while this is being read, so the last one is regularly half-written.
fn assemble_live_message(contents: &str) -> Option<String> {
    let mut pieces: Vec<LiveMessagePiece> = Vec::new();
    for line in contents.lines() {
        let Ok(piece) = serde_json::from_str::<LiveMessagePiece>(line) else {
            continue;
        };
        pieces.push(piece);
    }

    let newest_message = pieces.last()?.message_id.clone();
    if newest_message.is_some() {
        pieces.retain(|piece| piece.message_id == newest_message);
    } else {
        // Nothing names which message these belong to, so the only boundary left is a
        // message starting over. Taken as the last such start rather than by clearing as
        // they are read, because the pieces are not in the order they are numbered.
        let last_start = pieces
            .iter()
            .rposition(|piece| piece.index == 0)
            .unwrap_or(0);
        pieces.drain(..last_start);
    }

    pieces.sort_by_key(|piece| piece.index);
    pieces.dedup_by_key(|piece| piece.index);

    let message: String = pieces.into_iter().map(|piece| piece.delta).collect();
    (!message.trim().is_empty()).then_some(message)
}

/// The question the hook last recorded for this session, if it recorded one.
///
/// A file left behind by a question that has since been answered is not filtered here:
/// only the session's conversation says whether the call was answered, and this function
/// is the one place that does not have it. The caller checks; see [`PendingQuestion`].
pub fn read_pending_question(
    home_directory: &Path,
    session_id: &str,
) -> Result<Option<PendingQuestion>> {
    let Some(session_id) = single_path_component(session_id) else {
        return Ok(None);
    };
    let path = home_directory
        .join(".claude")
        .join(PENDING_QUESTIONS_DIRECTORY)
        .join(format!("{session_id}.json"));

    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        // A session that has never asked anything leaves no file, which is the ordinary
        // case rather than a failure.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };

    parse_pending_question(&contents)
        .with_context(|| format!("parsing {}", path.display()))
        .or_else(|error| {
            // The hook writes this file whole, but a reader that refused to draw anything
            // because one payload was unreadable would lose the terminal mirror as well.
            log::warn!("{error:#}");
            Ok(None)
        })
}

/// The shell script the hook runs. It records the call and decides nothing about it:
/// anything on stdout would be read by the CLI as a ruling on the tool call it is
/// watching, so this prints nothing and always succeeds.
const QUESTION_HOOK_SOURCE: &str = r#"#!/bin/sh
# Written by Zed. Records the question a session is waiting on, so that a reader outside
# the terminal can draw it as something to click. The CLI writes nothing about a question
# into its transcript until the question has been answered, so this is the only place the
# structure of a waiting question can be read from.
#
# Nothing is printed and the exit status is always 0: this hook rules on nothing.
payload=$(cat)
directory="$HOME/.claude/pending-questions"
mkdir -p "$directory" || exit 0
session=$(printf '%s' "$payload" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("session_id",""))' 2>/dev/null)
# Not every machine a session runs on has python3, and a hook that silently recorded
# nothing there would look exactly like a session that has never asked anything.
if [ -z "$session" ]; then
  session=$(printf '%s' "$payload" | tr -d '\n' | sed -n 's/.*"session_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
fi
[ -n "$session" ] || exit 0
case "$session" in */*|*..*) exit 0 ;; esac
printf '%s' "$payload" > "$directory/$session.json.tmp" 2>/dev/null || exit 0
mv "$directory/$session.json.tmp" "$directory/$session.json" 2>/dev/null || exit 0
exit 0
"#;

/// The shell script that records what a session is saying as it says it.
///
/// The CLI does not write an assistant message into its transcript until the turn holding
/// it is over, which on a long turn is tens of seconds after the words were on screen.
/// This event carries them as they are drawn.
///
/// Each message starts again at index 0, which is the only thing keeping this file to one
/// message: the first piece of a message truncates it and the rest are appended.
const LIVE_MESSAGE_HOOK_SOURCE: &str = r#"#!/bin/sh
# Written by Zed. Records what a session is saying while it says it, because the
# transcript does not get the words until the turn is over.
#
# Nothing is printed and the exit status is always 0: this hook rules on nothing.
payload=$(cat)
directory="$HOME/.claude/live-messages"
mkdir -p "$directory" || exit 0
session=$(printf '%s' "$payload" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("session_id",""))' 2>/dev/null)
if [ -z "$session" ]; then
  session=$(printf '%s' "$payload" | tr -d '\n' | sed -n 's/.*"session_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
fi
[ -n "$session" ] || exit 0
case "$session" in */*|*..*) exit 0 ;; esac

file="$directory/$session.jsonl"
# A message begins again at index 0, so its first piece is what clears the one before it.
# Without this the file is every message the session has ever drawn.
case "$payload" in
  *'"index":0,'*|*'"index": 0,'*|*'"index":0}'*|*'"index": 0}'*) : > "$file" 2>/dev/null || exit 0 ;;
esac
# A message that somehow never starts over must not grow without limit.
if [ -f "$file" ]; then
  size=$(wc -c < "$file" 2>/dev/null || echo 0)
  [ "$size" -lt 262144 ] || : > "$file" 2>/dev/null
fi
printf '%s\n' "$payload" >> "$file" 2>/dev/null || exit 0
exit 0
"#;

/// The hooks Zed installs, as the event they answer and the script that answers it.
///
/// Both exist for the same reason — the CLI writes neither a waiting question nor the
/// words of a turn in progress anywhere a reader can see them — so they are installed
/// together and reported together. A machine with one and not the other is a machine
/// where installing was interrupted, and is offered the install again.
const INSTALLED_HOOKS: [(&str, &str, &str); 2] = [
    (
        "PreToolUse",
        QUESTION_HOOK_SCRIPT,
        QUESTION_HOOK_SOURCE_PLACEHOLDER,
    ),
    (
        LIVE_MESSAGE_HOOK_EVENT,
        LIVE_MESSAGE_HOOK_SCRIPT,
        LIVE_MESSAGE_HOOK_SOURCE_PLACEHOLDER,
    ),
];

/// The two sources, kept out of [`INSTALLED_HOOKS`] because a `const` array cannot hold
/// them by reference and repeating them would be two copies to keep in step.
const QUESTION_HOOK_SOURCE_PLACEHOLDER: &str = "question";
const LIVE_MESSAGE_HOOK_SOURCE_PLACEHOLDER: &str = "live-message";

fn hook_source(placeholder: &str) -> &'static str {
    match placeholder {
        LIVE_MESSAGE_HOOK_SOURCE_PLACEHOLDER => LIVE_MESSAGE_HOOK_SOURCE,
        _ => QUESTION_HOOK_SOURCE,
    }
}

/// Only the question hook is matched to a tool; the message hook answers every message
/// its event is raised for.
fn hook_matcher(event: &str) -> Option<&'static str> {
    (event == "PreToolUse").then_some(QUESTION_HOOK_TOOL)
}

/// Whether the hooks Zed installs are in place on this machine.
pub fn question_hook_is_installed(home_directory: &Path) -> bool {
    let Some(settings) = read_claude_settings(home_directory).log_err().flatten() else {
        return false;
    };

    INSTALLED_HOOKS.iter().all(|(event, script, _)| {
        let script = home_directory.join(".claude").join(script);
        script.is_file() && settings_name_the_hook(&settings, event, &script.to_string_lossy())
    })
}

fn read_claude_settings(home_directory: &Path) -> Result<Option<serde_json::Value>> {
    let path = home_directory.join(".claude").join("settings.json");
    match fs::read_to_string(&path) {
        Ok(contents) => {
            let settings = serde_json::from_str(&contents)
                .with_context(|| format!("parsing {}", path.display()))?;
            Ok(Some(settings))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn settings_name_the_hook(settings: &serde_json::Value, event: &str, script: &str) -> bool {
    settings
        .get("hooks")
        .and_then(|hooks| hooks.get(event))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|entries| {
            entries.iter().any(|entry| {
                entry
                    .get("hooks")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|hooks| {
                        hooks.iter().any(|hook| {
                            hook.get("command").and_then(serde_json::Value::as_str) == Some(script)
                        })
                    })
            })
        })
}

/// Installs the hook that records waiting questions, and reports the path the previous
/// settings were copied to when there were settings to copy.
///
/// The settings file belongs to its user and holds everything else they have configured,
/// so it is read, added to, and written back rather than replaced, and a copy of what was
/// there is kept first. Installing twice changes nothing.
pub fn install_question_hook(home_directory: &Path) -> Result<Option<PathBuf>> {
    let claude_directory = home_directory.join(".claude");
    let settings_path = claude_directory.join("settings.json");

    // Written before the settings are touched: a settings file naming a script that is
    // not there yet would have the CLI reporting a broken hook until this finished.
    let mut commands = Vec::new();
    for (event, script, source) in INSTALLED_HOOKS {
        let script = claude_directory.join(script);
        if let Some(parent) = script.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::write(&script, hook_source(source))
            .with_context(|| format!("writing {}", script.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
                .with_context(|| format!("making {} executable", script.display()))?;
        }
        commands.push((event, script.to_string_lossy().into_owned()));
    }

    let existing = read_claude_settings(home_directory)?;
    let missing: Vec<&(&str, String)> = commands
        .iter()
        .filter(|(event, command)| match &existing {
            Some(settings) => !settings_name_the_hook(settings, event, command),
            None => true,
        })
        .collect();
    if missing.is_empty() {
        return Ok(None);
    }

    let backup = match &existing {
        Some(_) => {
            let backup = settings_path.with_extension(format!("json.bak.{}", now_millis()));
            fs::copy(&settings_path, &backup)
                .with_context(|| format!("copying {} aside", settings_path.display()))?;
            Some(backup)
        }
        None => None,
    };

    let mut settings = existing.unwrap_or_else(|| serde_json::json!({}));
    let settings_object = settings
        .as_object_mut()
        .with_context(|| format!("{} does not hold a JSON object", settings_path.display()))?;
    let hooks = settings_object
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("`hooks` does not hold a JSON object")?;

    for (event, command) in missing {
        let mut hook = serde_json::json!({
            "hooks": [{ "type": "command", "command": command }],
        });
        if let Some(matcher) = hook_matcher(event)
            && let Some(hook) = hook.as_object_mut()
        {
            hook.insert("matcher".to_string(), serde_json::json!(matcher));
        }
        hooks
            .entry(*event)
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .with_context(|| format!("`hooks.{event}` does not hold a JSON array"))?
            .push(hook);
    }

    fs::write(&settings_path, format!("{:#}\n", settings))
        .with_context(|| format!("writing {}", settings_path.display()))?;

    Ok(backup)
}

/// Which agents of one workflow run that run's journal says have returned.
///
/// A run's agents carry no tool use id, and the `Workflow` call that started the run is
/// answered the moment the run is launched rather than when it ends, so the session's own
/// conversation cannot say when any one of them finishes. The journal the run writes
/// beside its agents is the only record of that.
#[derive(Debug, Default)]
struct WorkflowJournal {
    returned: HashSet<String>,
}

impl WorkflowJournal {
    /// `None` when the run has written no journal this scan could read, which is the
    /// ordinary state of a run in the moments after it launches.
    fn read(directory: &Path) -> Option<Self> {
        let path = directory.join(WORKFLOW_JOURNAL_FILE);
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => {
                log::warn!("reading {}: {error}", path.display());
                return None;
            }
        };

        // Taken a line at a time: the journal is appended to while it is being read, so
        // the last line is regularly half-written, and one unreadable line must not
        // decide the state of every other agent in the run.
        let mut returned = HashSet::default();
        for line in contents.lines() {
            let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if entry.get("type").and_then(serde_json::Value::as_str)
                != Some(WORKFLOW_JOURNAL_RESULT_TYPE)
            {
                continue;
            }
            if let Some(agent_id) = entry.get("agentId").and_then(serde_json::Value::as_str) {
                returned.insert(agent_id.to_string());
            }
        }

        Some(Self { returned })
    }

    fn has_returned(&self, agent_id: &str) -> bool {
        self.returned.contains(agent_id)
    }
}

/// Every subagent conversation a session has spawned.
///
/// The two on-disk layouts — `subagents/agent-<id>.jsonl` and
/// `subagents/workflows/<runId>/agent-<id>.jsonl` — are both scanned, and the result is
/// ordered by run id and then agent id so that a caller rendering it does not see rows
/// move around between polls: directory enumeration is in no particular order.
pub async fn list_subagents(
    home_directory: &Path,
    session_id: &str,
) -> Result<Vec<SubagentSummary>> {
    let Some(session_directory) = find_session_directory(home_directory, session_id) else {
        // A session that has written nothing of its own leaves no directory behind, which
        // means it has spawned no subagents rather than that the scan failed.
        return Ok(Vec::new());
    };
    let subagents_directory = session_directory.join(SUBAGENTS_DIRECTORY);

    let mut subagents = Vec::new();
    read_subagents_in(&subagents_directory, None, &mut subagents)?;

    let workflows_directory = subagents_directory.join(WORKFLOWS_DIRECTORY);
    match fs::read_dir(&workflows_directory) {
        Ok(directory_entries) => {
            for directory_entry in directory_entries {
                let Some(directory_entry) = directory_entry.log_err() else {
                    continue;
                };
                // Only the run directories hold agents; anything else `workflows` may
                // contain names none.
                if !directory_entry.path().is_dir() {
                    continue;
                }
                let Ok(run_directory_name) = directory_entry.file_name().into_string() else {
                    continue;
                };
                let Some(workflow_run_id) = single_path_component(&run_directory_name) else {
                    continue;
                };
                read_subagents_in(
                    &directory_entry.path(),
                    Some(workflow_run_id),
                    &mut subagents,
                )?;
            }
        }
        // Most sessions run no workflows at all, so a missing directory is the common
        // case and not a failure. Any other error is real and belongs to the caller.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading {}", workflows_directory.display()));
        }
    }

    subagents.sort_by(|left, right| {
        (&left.workflow_run_id, &left.agent_id).cmp(&(&right.workflow_run_id, &right.agent_id))
    });

    Ok(subagents)
}

/// Appends every subagent whose meta sidecar sits directly in `directory`.
///
/// The scan is driven off the sidecars rather than the transcripts, because a transcript
/// on its own says nothing about which agent wrote it, and because a run directory keeps
/// a `journal.jsonl` beside its agents that pairs with no sidecar. That journal names no
/// agent of its own to list, but it is what says which of the listed ones have returned;
/// see [`WorkflowJournal`].
fn read_subagents_in(
    directory: &Path,
    workflow_run_id: Option<&str>,
    subagents: &mut Vec<SubagentSummary>,
) -> Result<()> {
    let directory_entries = match fs::read_dir(directory) {
        Ok(directory_entries) => directory_entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", directory.display()));
        }
    };

    // One journal answers for every agent in the run, so it is read once here rather than
    // once per agent. Only a run directory has one.
    let journal = workflow_run_id.and_then(|_| WorkflowJournal::read(directory));

    for directory_entry in directory_entries {
        let Some(directory_entry) = directory_entry.log_err() else {
            continue;
        };
        // Agent ids are hexadecimal, so a name that is not UTF-8 is not one of ours.
        let Ok(file_name) = directory_entry.file_name().into_string() else {
            continue;
        };
        let Some(agent_id) = file_name
            .strip_prefix(SUBAGENT_FILE_PREFIX)
            .and_then(|remainder| remainder.strip_suffix(SUBAGENT_META_SUFFIX))
        else {
            continue;
        };
        // The id is read back out of a file name here, but it is also what a later
        // request names an agent by, so it goes through the same boundary as the rest.
        let Some(agent_id) = single_path_component(agent_id) else {
            continue;
        };

        // One half-written or unreadable sidecar must not hide every other agent the
        // session spawned, so each failure is logged and that agent alone is skipped.
        let Some(contents) = fs::read_to_string(directory_entry.path()).log_err() else {
            continue;
        };
        let Some(meta) = parse_subagent_meta(&contents).log_err() else {
            continue;
        };

        let transcript_path = directory.join(format!(
            "{SUBAGENT_FILE_PREFIX}{agent_id}{SUBAGENT_TRANSCRIPT_SUFFIX}"
        ));
        // An agent whose transcript cannot be measured — not written yet, or replaced
        // between the two reads — is still an agent worth listing, so the size falls
        // back to zero instead of dropping the row.
        let size = fs::metadata(&transcript_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);

        subagents.push(SubagentSummary {
            agent_id: agent_id.to_string(),
            workflow_run_id: workflow_run_id.map(str::to_string),
            meta,
            transcript_path,
            size,
            // An agent the journal does not name has not returned: the sidecar this scan
            // just read is written when the agent is spawned, before the journal records
            // it starting, so the gap between the two files is the start of its work
            // rather than the end of it.
            workflow_agent_finished: journal
                .as_ref()
                .map(|journal| journal.has_returned(agent_id)),
        });
    }

    Ok(())
}

/// The path of one subagent's transcript, or `None` when it does not exist or one of the
/// ids could name something outside the session's own directory.
///
/// `workflow_run_id` is required for an agent belonging to a workflow run, because the
/// two layouts put the same agent id in different directories and only the caller knows
/// which one it read the id from.
pub fn subagent_transcript_path(
    home_directory: &Path,
    session_id: &str,
    agent_id: &str,
    workflow_run_id: Option<&str>,
) -> Option<PathBuf> {
    let agent_id = single_path_component(agent_id)?;
    let mut directory =
        find_session_directory(home_directory, session_id)?.join(SUBAGENTS_DIRECTORY);
    if let Some(workflow_run_id) = workflow_run_id {
        let workflow_run_id = single_path_component(workflow_run_id)?;
        directory = directory.join(WORKFLOWS_DIRECTORY).join(workflow_run_id);
    }

    let path = directory.join(format!(
        "{SUBAGENT_FILE_PREFIX}{agent_id}{SUBAGENT_TRANSCRIPT_SUFFIX}"
    ));
    path.is_file().then_some(path)
}

/// Locates `<home>/.claude/projects/*/<sessionId>/`.
///
/// Searched rather than computed for the same reason [`find_transcript`] searches: the
/// directory under `projects` is a slug derived from the session's working directory by
/// an undocumented rule.
fn find_session_directory(home_directory: &Path, session_id: &str) -> Option<PathBuf> {
    let projects_directory = home_directory.join(".claude").join("projects");
    let session_id = single_path_component(session_id)?;

    for directory_entry in fs::read_dir(projects_directory).ok()? {
        let Some(directory_entry) = directory_entry.log_err() else {
            continue;
        };
        let candidate = directory_entry.path().join(session_id);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }

    None
}

/// The run id a `Workflow` tool result announces, which is the only thing linking a
/// workflow's agents on disk — stored under the run id — to the tool call that started
/// them, whose `tool_use_id` pairs it with the assistant message.
///
/// The value becomes a path component, so a run id that could name a directory other
/// than one run's own is treated as no run id at all.
pub fn workflow_run_id_in_tool_result(text: &str) -> Option<String> {
    text.match_indices(WORKFLOW_RUN_ID_LABEL)
        .filter_map(|(index, label)| {
            let remainder = text.get(index + label.len()..)?;
            let candidate = remainder
                .trim_start_matches([' ', '\t'])
                .split(char::is_whitespace)
                .next()?;
            workflow_run_id(candidate)
        })
        .next()
        .map(str::to_string)
}

fn workflow_run_id(candidate: &str) -> Option<&str> {
    // The separator of the machine that wrote the text is unknown, so a backslash is
    // rejected here as well, even though this side would treat it as an ordinary
    // character in a name.
    if candidate.contains(['/', '\\']) || candidate.contains("..") {
        return None;
    }
    single_path_component(candidate)
}

/// The name of the tmux session a registration's `tmux` field points into.
///
/// The field is shaped `session:@window.%pane`, and `:` is tmux's own separator between
/// a session and the window inside it, so a session name can never contain one and
/// everything before the first is the name. Only read for display — sends and captures
/// address the pane by its id; see [`pane_target`].
pub fn tmux_session_name(tmux_field: &str) -> Option<&str> {
    let name = tmux_field.split(':').next()?;
    (!name.is_empty()).then_some(name)
}

/// The pane a registration's `tmux` field names, as a target `tmux` accepts.
///
/// The field is shaped `session:@window.%pane`, for example `awp:@1.%1`. Only the pane id
/// is kept: it is unique across the whole tmux server, so the session name — which may
/// contain spaces, quotes or semicolons — never has to be quoted or reasoned about.
/// Anything that is not `%` followed by decimal digits is rejected outright, so a value
/// out of this JSON file can never reach tmux as a flag or as a second command.
pub fn pane_target(tmux_field: &str) -> Option<String> {
    let pane_identifier = tmux_field.rsplit('.').next()?;
    let digits = pane_identifier.strip_prefix('%')?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(format!("%{digits}"))
}

/// The window a registration's `tmux` field names, as a target `tmux` accepts.
///
/// The field is shaped `session:@window.%pane`, for example `awp:@1.%1`. As in
/// [`pane_target`], only the id is kept — it is unique across the whole tmux server — and
/// anything that is not `@` followed by decimal digits is rejected outright, so a value
/// out of this JSON file can never reach tmux as a flag or as a second command.
pub fn window_target(tmux_field: &str) -> Option<String> {
    let after_session_name = tmux_field.split_once(':')?.1;
    let window_identifier = after_session_name.split('.').next()?;
    let digits = window_identifier.strip_prefix('@')?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(format!("@{digits}"))
}

/// Names the sessions Zed groups with a user's own to hold one window of it; see
/// [`attach_arguments`]. The prefix is what keeps Zed's own bookkeeping out of the
/// session list the user is shown.
const MIRROR_SESSION_PREFIX: &str = "zed-claude-mirror-";

/// Whether a tmux session is one of Zed's mirrors rather than one the user started.
pub fn is_zed_mirror_session(session_name: &str) -> bool {
    session_name.starts_with(MIRROR_SESSION_PREFIX)
}

/// Distinguishes the mirror sessions of concurrent attaches, so that an attach can never
/// fail on the name of a mirror whose client is still going away.
static MIRROR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The arguments of the one tmux invocation that shows a registration's window in a
/// terminal of its own.
///
/// Attaching to the pane's own session is what does not work: a session has one current
/// window and every client of it is shown that window, so two Claude Code sessions living
/// in two windows of one tmux session are both shown whichever window the user happens to
/// be on — never the window each of them is running in. A session grouped with theirs
/// (`new-session -t`) shares its windows but keeps a current window of its own, so the
/// terminal can sit on this registration's window while the user's own client stays where
/// it is.
///
/// A field with no window id is attached the way it was before there were mirrors, which
/// is still right for a session that has only the one window.
pub fn attach_arguments(tmux_field: &str) -> Option<Vec<String>> {
    let pane_target = pane_target(tmux_field)?;
    let (Some(session_name), Some(window_target)) =
        (tmux_session_name(tmux_field), window_target(tmux_field))
    else {
        return Some(vec!["attach".to_string(), "-t".to_string(), pane_target]);
    };

    let mirror_name = format!(
        "{MIRROR_SESSION_PREFIX}{}-{}",
        std::process::id(),
        MIRROR_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    Some(mirror_arguments(session_name, &window_target, &mirror_name))
}

/// The order of this list is what makes it safe, and each step is load-bearing:
///
/// * `new-session -d` creates the mirror without attaching, because a `new-session` that
///   attaches leaves the commands after it addressing the session the client came from.
/// * `=` in front of the session name asks tmux for that name exactly, rather than
///   letting a name that reads as a pattern group the terminal with another session.
/// * `select-window` comes before there is a client, so that the terminal opens on the
///   right window instead of visibly jumping to it.
/// * `destroy-unattached` comes last, because a session carrying it while nothing is
///   attached is destroyed on the spot. Set here, the mirror goes away with the terminal
///   rather than being left behind on the user's tmux server.
///
/// The name carries a sequence number because a command that fails makes tmux abandon the
/// rest of the list: reusing the name of a mirror whose client has not finished detaching
/// would fail the `new-session` and leave the reader with no terminal at all.
fn mirror_arguments(session_name: &str, window_target: &str, mirror_name: &str) -> Vec<String> {
    vec![
        "new-session".to_string(),
        "-d".to_string(),
        "-t".to_string(),
        format!("={session_name}"),
        "-s".to_string(),
        mirror_name.to_string(),
        ";".to_string(),
        "select-window".to_string(),
        "-t".to_string(),
        format!("{mirror_name}:{window_target}"),
        ";".to_string(),
        "attach-session".to_string(),
        "-t".to_string(),
        mirror_name.to_string(),
        ";".to_string(),
        "set-option".to_string(),
        "-t".to_string(),
        mirror_name.to_string(),
        "destroy-unattached".to_string(),
        "on".to_string(),
    ]
}

/// Distinguishes the buffers of concurrent sends, so that one send cannot paste the text
/// of another that is still in flight.
static PASTE_BUFFER_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Whether the send of `text` has to travel as a bracketed paste.
///
/// Bracketed paste exists to keep the newlines of a multi-line message from being read as
/// submissions, so a message without one has nothing for it to protect — and asking for it
/// anyway is not free. The sequence that opens a bracketed paste puts the pane's TUI into
/// paste mode, where everything arriving is accumulated as pasted content rather than
/// shown as it is typed, and only the closing sequence takes it out again. A pane left in
/// that state swallows what its user types next: the characters reach Claude Code but
/// never appear on screen. A single line goes without it, so the common case never enters
/// that state at all.
fn needs_bracketed_paste(text: &str) -> bool {
    text.contains('\n')
}

/// The arguments of the one tmux invocation a send performs.
///
/// Loading the buffer, pasting it and submitting it travel as a single tmux command list,
/// separated by the literal `;` arguments, because the tmux server is one event loop that
/// runs a client's whole command list before it takes the next client's commands. As
/// three invocations there is a gap between them for a concurrent send to slip into:
/// load, paste, load, paste, Enter, Enter submits both messages as one and then submits
/// an empty line.
///
/// `bracketed` adds the `-p` that pastes as a bracketed paste; see
/// [`needs_bracketed_paste`] for why only a multi-line message asks for it.
fn send_text_arguments(buffer_name: &str, pane_target: &str, bracketed: bool) -> Vec<String> {
    let mut arguments = vec![
        // `-` is where load-buffer reads the buffer from, and it means standard input.
        "load-buffer",
        "-b",
        buffer_name,
        "-",
        ";",
        // `-d` deletes the buffer once it has been pasted, so that the text is not left
        // behind for the next paste to pick up.
        "paste-buffer",
        "-d",
    ];
    if bracketed {
        arguments.push("-p");
    }
    arguments.extend([
        "-b",
        buffer_name,
        "-t",
        pane_target,
        ";",
        "send-keys",
        "-t",
        pane_target,
        "Enter",
    ]);

    arguments.into_iter().map(str::to_string).collect()
}

/// Types `text` into the pane and submits it.
///
/// The text travels through a tmux paste buffer rather than as `send-keys` arguments:
/// bracketed paste is what makes a multi-line message arrive as one message, where
/// `send-keys` would submit at every newline. The buffer is loaded from stdin so that the
/// text never appears on a command line.
pub async fn send_text(pane_target: &str, text: &str) -> Result<()> {
    if pane_target.is_empty() {
        anyhow::bail!("no tmux pane to send to");
    }

    let buffer_name = format!(
        "zed-claude-{}-{}",
        std::process::id(),
        PASTE_BUFFER_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );

    let arguments = send_text_arguments(&buffer_name, pane_target, needs_bracketed_paste(text));
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    // A command that fails makes tmux abandon the rest of the command list, so a paste
    // that fails — the pane is gone, most likely — never reaches the deletion that
    // `paste-buffer -d` would have done, while the `load-buffer` before it has already
    // succeeded. The message would stay on top of the buffer stack for the user's next
    // manual paste to pick up, so it is removed here instead. The paste's own failure is
    // what gets reported: this cleanup is not the news.
    match run_with_stdin("tmux", &arguments, text.as_bytes()).await {
        Ok(()) => Ok(()),
        Err(error) => {
            run_tmux(&["delete-buffer", "-b", &buffer_name])
                .await
                .log_err();
            Err(error)
        }
    }
}

/// Interrupts whatever the session is doing, which is what Escape means to Claude Code.
/// The keys a prompt drawn in a session's pane can be answered with.
///
/// An enum rather than a key name, so that nothing but these three can reach
/// `tmux send-keys`: the caller is a UI button, and a pane that accepts arbitrary key
/// names from one would accept whatever a registration could be made to carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneKey {
    Up,
    Down,
    Enter,
    /// The digit that picks a numbered option outright, without walking to it first.
    Choice(Digit),
    /// Shift+Tab, which the CLI cycles its permission mode on. tmux calls it back-tab.
    CyclePermissionMode,
    /// Ctrl+C: takes back what has been typed into the CLI, and stops a turn it is
    /// running.
    Cancel,
    /// Ctrl+D: quits the CLI, taking the session with it.
    Quit,
}

/// The digits a numbered menu answers to.
///
/// A closed set rather than a number, for the same reason the rest of [`PaneKey`] is one:
/// what this becomes is a key sent to a terminal, and a value that could be anything
/// could be sent as something other than a choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Digit {
    One,
    Two,
    Three,
    Four,
    Five,
    Six,
    Seven,
    Eight,
    Nine,
}

impl Digit {
    /// The digit that picks the option at `index`, counting from zero as the options
    /// themselves are. `None` past the ninth, which a menu does not number.
    pub fn for_option(index: usize) -> Option<Self> {
        Some(match index {
            0 => Digit::One,
            1 => Digit::Two,
            2 => Digit::Three,
            3 => Digit::Four,
            4 => Digit::Five,
            5 => Digit::Six,
            6 => Digit::Seven,
            7 => Digit::Eight,
            8 => Digit::Nine,
            _ => return None,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Digit::One => "1",
            Digit::Two => "2",
            Digit::Three => "3",
            Digit::Four => "4",
            Digit::Five => "5",
            Digit::Six => "6",
            Digit::Seven => "7",
            Digit::Eight => "8",
            Digit::Nine => "9",
        }
    }
}

impl PaneKey {
    /// The name tmux knows the key by.
    pub fn tmux_name(self) -> &'static str {
        match self {
            PaneKey::Up => "Up",
            PaneKey::Down => "Down",
            PaneKey::Enter => "Enter",
            PaneKey::Choice(digit) => digit.as_str(),
            PaneKey::CyclePermissionMode => "BTab",
            PaneKey::Cancel => "C-c",
            PaneKey::Quit => "C-d",
        }
    }

    /// Reads back what [`Self::tmux_name`] wrote. `None` for anything else, which is what
    /// keeps the set closed when the name has crossed a connection.
    pub fn from_tmux_name(name: &str) -> Option<Self> {
        match name {
            "Up" => Some(PaneKey::Up),
            "Down" => Some(PaneKey::Down),
            "Enter" => Some(PaneKey::Enter),
            "1" => Some(PaneKey::Choice(Digit::One)),
            "2" => Some(PaneKey::Choice(Digit::Two)),
            "3" => Some(PaneKey::Choice(Digit::Three)),
            "4" => Some(PaneKey::Choice(Digit::Four)),
            "5" => Some(PaneKey::Choice(Digit::Five)),
            "6" => Some(PaneKey::Choice(Digit::Six)),
            "7" => Some(PaneKey::Choice(Digit::Seven)),
            "8" => Some(PaneKey::Choice(Digit::Eight)),
            "9" => Some(PaneKey::Choice(Digit::Nine)),
            "BTab" => Some(PaneKey::CyclePermissionMode),
            "C-c" => Some(PaneKey::Cancel),
            "C-d" => Some(PaneKey::Quit),
            _ => None,
        }
    }
}

/// Answers a prompt drawn in the pane, by sending it one of the keys it is waiting for.
pub async fn send_key(pane_target: &str, key: PaneKey) -> Result<()> {
    if pane_target.is_empty() {
        anyhow::bail!("no tmux pane to send to");
    }

    run_tmux(&["send-keys", "-t", pane_target, key.tmux_name()]).await
}

/// The visible contents of a session's tmux pane, top line first.
///
/// This is the only way to read what Claude Code draws but never records: a prompt
/// waiting for an answer, the messages queued behind the running turn, and the status
/// line. `-p` writes the pane to stdout; only the visible screen is taken, because the
/// scrollback behind it is the conversation, which is read from the transcript instead.
pub async fn capture_pane(pane_target: &str) -> Result<String> {
    if pane_target.is_empty() {
        anyhow::bail!("no tmux pane to capture");
    }

    let output = capture_tmux(&["capture-pane", "-p", "-t", pane_target]).await?;
    Ok(output)
}

pub async fn send_escape(pane_target: &str) -> Result<()> {
    if pane_target.is_empty() {
        anyhow::bail!("no tmux pane to send to");
    }

    run_tmux(&["send-keys", "-t", pane_target, "Escape"]).await
}

/// Like [`run_tmux`], but hands back what tmux wrote rather than only whether it
/// succeeded.
async fn capture_tmux(arguments: &[&str]) -> Result<String> {
    let mut command = util::command::new_command("tmux");
    command.args(arguments);
    let output = command
        .output()
        .await
        .with_context(|| format!("running tmux {}", arguments.join(" ")))?;

    if !output.status.success() {
        anyhow::bail!(
            "tmux {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Every argument is passed as its own `argv` entry, never through a shell, so that no
/// value taken from a registration can be read as anything but one argument.
async fn run_tmux(arguments: &[&str]) -> Result<()> {
    let mut command = util::command::new_command("tmux");
    command.args(arguments);
    let output = command
        .output()
        .await
        .with_context(|| format!("running tmux {}", arguments.join(" ")))?;

    if !output.status.success() {
        anyhow::bail!(
            "tmux {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

/// Runs a command with `stdin_contents` on its standard input.
///
/// The text to send reaches tmux this way rather than as an argument, so that nothing a
/// session's user typed can be read as part of the command line.
async fn run_with_stdin(program: &str, arguments: &[&str], stdin_contents: &[u8]) -> Result<()> {
    let described = || format!("{program} {}", arguments.join(" "));

    let mut command = util::command::new_command(program);
    command.args(arguments);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .with_context(|| format!("running {}", described()))?;
    let mut stdin = child
        .stdin
        .take()
        .with_context(|| format!("{} was spawned without a stdin to write to", described()))?;
    // A command that fails before it has read its input closes the pipe, which makes this
    // write fail with a broken pipe. Reporting that would hide the command's own error, so
    // the outcome is kept and only looked at once the exit status has been seen.
    let mut write_result = stdin
        .write_all(stdin_contents)
        .await
        .with_context(|| format!("writing to the stdin of {}", described()));
    if write_result.is_ok() {
        // Closed before waiting, because a command that reads until end of input never
        // exits while this end of the pipe is still open.
        write_result = stdin
            .close()
            .await
            .with_context(|| format!("closing the stdin of {}", described()));
    }
    drop(stdin);

    let output = child
        .output()
        .await
        .with_context(|| format!("waiting for {}", described()))?;
    if !output.status.success() {
        anyhow::bail!(
            "{} failed: {}",
            described(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    // The command reported success, so nothing it did can explain a failed write and the
    // write error is the real one.
    write_result?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    /// On Linux, `procStart` is field 22 of `/proc/<pid>/stat`. Counting that field from
    /// the left is what a reader of the format gets wrong: field 2 is the executable's
    /// name in parentheses, and a name may hold spaces and parentheses of its own.
    #[test]
    fn test_start_time_in_proc_stat_is_counted_from_the_end_of_the_name() {
        // The state is field 3, so eighteen fields follow it before field 22.
        let fields_after_name = |start_time: &str| {
            let mut fields: Vec<String> = (0..18).map(|index| index.to_string()).collect();
            fields.push(start_time.to_string());
            fields.extend((0..4).map(|index| format!("tail{index}")));
            fields.join(" ")
        };

        assert_eq!(
            start_time_in_proc_stat(&format!(
                "45188 (claude) S {}",
                fields_after_name("112579206")
            )),
            Some("112579206".to_string()),
            "the value a Linux registration records as procStart"
        );
        assert_eq!(
            start_time_in_proc_stat(&format!(
                "45188 (node (worker) two) S {}",
                fields_after_name("42")
            )),
            Some("42".to_string()),
            "a name holding spaces and parentheses must not shift the field that is read"
        );
        assert_eq!(
            start_time_in_proc_stat("45188 (claude) S 1 2 3"),
            None,
            "a line with no field 22 yields nothing rather than some other field"
        );
        assert_eq!(start_time_in_proc_stat("nonsense"), None);
    }

    /// A row showing the context of the answer before a compaction says the session is
    /// holding a conversation it has already dropped.
    #[test]
    fn a_compaction_in_the_tail_replaces_the_context_before_it() {
        let compacted = spend_in_lines(
            [
                r#"{"type":"assistant","message":{"usage":{"input_tokens":2,"cache_read_input_tokens":679693,"cache_creation_input_tokens":198,"output_tokens":10}}}"#,
                r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"manual","preTokens":680651,"postTokens":14846}}"#,
            ]
            .into_iter(),
        );
        assert_eq!(
            compacted.context_tokens, 14846,
            "what the compaction kept, not the 680K the answer before it was given"
        );

        let answered_since = spend_in_lines(
            [
                r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"manual","preTokens":680651,"postTokens":14846}}"#,
                r#"{"type":"assistant","message":{"usage":{"input_tokens":2,"cache_read_input_tokens":94576,"cache_creation_input_tokens":9982,"output_tokens":10}}}"#,
            ]
            .into_iter(),
        );
        assert_eq!(
            answered_since.context_tokens, 104_560,
            "an answer measured the whole request, which supersedes the compaction"
        );
    }

    /// The newest answer's context wins, and a partial cost total is not reported at
    /// all — the tail covers only the answers inside it.
    #[test]
    fn the_tail_reports_the_newest_answer_and_only_a_whole_total() {
        let spend = spend_in_lines(
            [
                r#"{"type":"assistant","message":{"usage":{"input_tokens":2,"cache_read_input_tokens":1000,"cache_creation_input_tokens":500,"output_tokens":10}}}"#,
                r#"{"type":"assistant","message":{"usage":{"input_tokens":3,"cache_read_input_tokens":9000,"cache_creation_input_tokens":0,"output_tokens":20}}}"#,
            ]
            .into_iter(),
        );

        assert_eq!(
            spend.context_tokens, 9003,
            "the newest answer's context, not the largest or the sum"
        );
        assert_eq!(
            spend.total_cost_usd, None,
            "no cost snapshot in the tail means no total, not a partial one"
        );
    }

    #[test]
    fn a_cost_snapshot_in_the_tail_is_the_total() {
        let spend = spend_in_lines(
            [
                r#"{"type":"cost-state","totalCostUSD":12.5,"hasUnknownModelCost":false}"#,
                r#"{"type":"assistant","message":{"usage":{"input_tokens":1,"cache_read_input_tokens":7,"output_tokens":2}}}"#,
                r#"{"type":"cost-state","totalCostUSD":13.75,"hasUnknownModelCost":false}"#,
            ]
            .into_iter(),
        );

        assert_eq!(
            spend.total_cost_usd,
            Some(13.75),
            "the newest snapshot stands; each one is a total of the whole session"
        );
        assert_eq!(spend.context_tokens, 8);
    }

    /// A transcript is read mid-write and a tail begins mid-line, so unparseable lines
    /// are ordinary rather than exceptional.
    #[test]
    fn lines_that_are_not_records_are_skipped() {
        let spend = spend_in_lines(
            [
                r#"tokens":{"input_tokens":999}}} <- the half line a tail can begin with"#,
                "",
                r#"{"type":"assistant","message":{"usage":{"input_tokens":4,"output_tokens":1}}}"#,
            ]
            .into_iter(),
        );

        assert_eq!(spend.context_tokens, 4);
    }

    #[test]
    fn the_tmux_session_name_is_everything_before_the_first_separator() {
        assert_eq!(tmux_session_name("zed:@6.%8"), Some("zed"));
        assert_eq!(
            tmux_session_name("my work:@1.%1"),
            Some("my work"),
            "a name may hold anything but tmux's own separator, spaces included"
        );
        assert_eq!(
            tmux_session_name("solo"),
            Some("solo"),
            "a field with no separator is the name on its own"
        );
        assert_eq!(
            tmux_session_name(":@6.%8"),
            None,
            "an empty name is no name, not an empty label"
        );
        assert_eq!(tmux_session_name(""), None);
    }

    #[test]
    fn the_window_target_is_the_id_between_the_two_separators() {
        assert_eq!(window_target("zed:@6.%8"), Some("@6".to_string()));
        assert_eq!(window_target("my work:@1.%1"), Some("@1".to_string()));
        assert_eq!(
            window_target("zed:6.%8"),
            None,
            "a window index is not a window id, and only an id is unique across the server"
        );
        assert_eq!(
            window_target("zed:@.%8"),
            None,
            "an id of no digits is not an id"
        );
        assert_eq!(
            window_target("zed:@6x.%8"),
            None,
            "anything but digits could reach tmux as a flag or a second command"
        );
        assert_eq!(window_target("%8"), None, "a field with no session part");
        assert_eq!(window_target(""), None);
    }

    /// The mirror is grouped with the user's session and so is listed beside it. A name
    /// the user chose must never be mistaken for one, or their own session disappears
    /// from the tmux panel.
    #[test]
    fn only_zeds_own_mirrors_are_recognized_as_mirrors() {
        let mirror = attach_arguments("zed:@6.%8").expect("a pane field attaches");
        let index = mirror
            .iter()
            .position(|argument| argument == "-s")
            .expect("the mirror is created with a name of its own");
        assert!(is_zed_mirror_session(&mirror[index + 1]));

        assert!(!is_zed_mirror_session("zed"));
        assert!(!is_zed_mirror_session("my work"));
        assert!(
            !is_zed_mirror_session("mirror-zed-claude-mirror-1-0"),
            "the prefix is a prefix, not a substring"
        );
    }

    /// Two sessions in two windows of one tmux session used to be shown the same window:
    /// `attach` is per session, and a session has one current window that every client of
    /// it is shown. Each has to be attached through a mirror of its own for the terminal
    /// under a conversation to be the terminal that conversation is running in.
    #[test]
    fn attaching_goes_through_a_mirror_that_holds_this_window() {
        let first = attach_arguments("zed:@6.%8").expect("a pane field attaches");
        let second = attach_arguments("zed:@7.%9").expect("a pane field attaches");

        let mirror_of = |arguments: &[String]| {
            let index = arguments
                .iter()
                .position(|argument| argument == "-s")
                .expect("the mirror is created with a name of its own");
            arguments[index + 1].clone()
        };
        let first_mirror = mirror_of(&first);
        let second_mirror = mirror_of(&second);
        assert_ne!(
            first_mirror, second_mirror,
            "two attaches must not race for one name: a failed command abandons the rest \
             of a tmux command list, leaving the reader with no terminal"
        );

        assert_eq!(
            first,
            mirror_arguments("zed", "@6", &first_mirror),
            "the first session's terminal must hold window @6"
        );
        assert_eq!(
            second,
            mirror_arguments("zed", "@7", &second_mirror),
            "the second session's terminal must hold window @7, not the window the first \
             one is on"
        );
    }

    #[test]
    fn the_mirror_is_grouped_selected_attached_and_then_made_temporary() {
        assert_eq!(
            mirror_arguments("my work", "@6", "zed-claude-mirror-1-0"),
            vec![
                "new-session",
                "-d",
                "-t",
                // Exactly this session, not whatever a name that reads as a pattern matches.
                "=my work",
                "-s",
                "zed-claude-mirror-1-0",
                ";",
                // Before the client exists, so that the terminal opens on the window
                // rather than visibly jumping to it.
                "select-window",
                "-t",
                "zed-claude-mirror-1-0:@6",
                ";",
                "attach-session",
                "-t",
                "zed-claude-mirror-1-0",
                ";",
                // After it, because a session carrying this while nothing is attached is
                // destroyed on the spot.
                "set-option",
                "-t",
                "zed-claude-mirror-1-0",
                "destroy-unattached",
                "on",
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<String>>()
        );
    }

    /// The mirror exists to pick a window out of a session; a field that names no window
    /// has none to pick, and attaching to the pane's own session is what it did before.
    #[test]
    fn a_field_without_a_window_attaches_the_way_it_did_before() {
        assert_eq!(
            attach_arguments("zed:6.%8"),
            Some(vec![
                "attach".to_string(),
                "-t".to_string(),
                "%8".to_string()
            ])
        );
        assert_eq!(
            attach_arguments("zed:@6.pane"),
            None,
            "a field with no pane is not a session Zed can show at all"
        );
        assert_eq!(attach_arguments(""), None);
    }

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

    fn lines(buffer: &mut Vec<u8>) -> Vec<String> {
        split_complete_lines(buffer)
    }

    #[test]
    fn test_split_complete_lines_takes_every_whole_line() {
        let mut buffer = b"first\nsecond\nthird\n".to_vec();
        assert_eq!(lines(&mut buffer), vec!["first", "second", "third"]);
        assert!(
            buffer.is_empty(),
            "buffer should be drained, got {buffer:?}"
        );
    }

    #[test]
    fn test_split_complete_lines_keeps_trailing_partial_line() {
        let mut buffer = b"first\nsecond\npart".to_vec();
        assert_eq!(lines(&mut buffer), vec!["first", "second"]);
        assert_eq!(
            String::from_utf8_lossy(&buffer),
            "part",
            "the unterminated line must stay in the buffer"
        );

        buffer.extend_from_slice(b"ial\n");
        assert_eq!(lines(&mut buffer), vec!["partial"]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_split_complete_lines_survives_split_inside_multibyte_character() {
        let whole = "{\"text\":\"中文\"}\n".as_bytes().to_vec();
        // Cut one byte into the three bytes of 中, which is where a read of the file
        // being appended to can plausibly stop.
        let prefix_length = "{\"text\":\"".len() + 1;
        let (first_read, second_read) = whole.split_at(prefix_length);

        let mut buffer = first_read.to_vec();
        assert_eq!(
            lines(&mut buffer),
            Vec::<String>::new(),
            "no newline has been seen yet, so nothing may be emitted"
        );
        assert_eq!(
            buffer, first_read,
            "the partial character must be kept byte-for-byte"
        );

        buffer.extend_from_slice(second_read);
        assert_eq!(lines(&mut buffer), vec!["{\"text\":\"中文\"}"]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_split_complete_lines_on_empty_buffer() {
        let mut buffer = Vec::new();
        assert_eq!(lines(&mut buffer), Vec::<String>::new());
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_split_complete_lines_emits_blank_line_between_newlines() {
        let mut buffer = b"first\n\nsecond\n".to_vec();
        assert_eq!(lines(&mut buffer), vec!["first", "", "second"]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_normalize_whitespace_matches_padded_ps_output() {
        assert_eq!(
            normalize_whitespace("  Thu Sep  1 02:27:23 2026 "),
            "Thu Sep 1 02:27:23 2026"
        );
    }

    fn temporary_directory(label: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "claude-sessions-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("creating the temporary directory");
        path
    }

    fn write_transcript(home_directory: &Path, session_id: &str, contents: &str) -> PathBuf {
        let directory = home_directory
            .join(".claude")
            .join("projects")
            .join("-some-slug");
        std::fs::create_dir_all(&directory).expect("creating the transcript directory");
        let path = directory.join(format!("{session_id}.jsonl"));
        std::fs::write(&path, contents).expect("writing the transcript");
        path
    }

    fn write_file(path: PathBuf, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("creating the parent directory");
        }
        std::fs::write(&path, contents).expect("writing the fixture");
    }

    #[test]
    fn test_read_transcript_tail_reads_only_the_new_bytes() -> Result<()> {
        let home_directory = temporary_directory("incremental");
        let session_id = "session-a";
        let path = write_transcript(
            &home_directory,
            session_id,
            "{\"type\":\"user\",\"uuid\":\"1\"}\n",
        );

        let first = read_transcript_tail(
            &home_directory,
            session_id,
            TailState {
                path: None,
                offset: 0,
                pending: Vec::new(),
            },
        )?;
        assert_eq!(first.lines.len(), 1);
        assert_eq!(first.path.as_deref(), Some(path.as_path()));
        assert!(!first.restarted);

        std::fs::write(
            &path,
            "{\"type\":\"user\",\"uuid\":\"1\"}\n{\"type\":\"assistant\",\"uuid\":\"2\"}\n",
        )?;

        let second = read_transcript_tail(
            &home_directory,
            session_id,
            TailState {
                path: first.path,
                offset: first.offset,
                pending: first.pending,
            },
        )?;
        assert_eq!(
            second.lines,
            vec!["{\"type\":\"assistant\",\"uuid\":\"2\"}"],
            "only the appended line should be returned"
        );
        assert!(!second.restarted);

        std::fs::remove_dir_all(&home_directory).ok();
        Ok(())
    }

    #[test]
    fn test_read_transcript_tail_restarts_when_the_file_shrinks() -> Result<()> {
        let home_directory = temporary_directory("shrink");
        let session_id = "session-b";
        let path = write_transcript(
            &home_directory,
            session_id,
            "{\"type\":\"user\",\"uuid\":\"1\"}\n{\"type\":\"user\",\"uuid\":\"2\"}\n",
        );

        let first = read_transcript_tail(
            &home_directory,
            session_id,
            TailState {
                path: None,
                offset: 0,
                pending: Vec::new(),
            },
        )?;
        assert_eq!(first.lines.len(), 2);

        std::fs::write(&path, "{\"type\":\"user\",\"uuid\":\"9\"}\n")?;

        let second = read_transcript_tail(
            &home_directory,
            session_id,
            TailState {
                path: first.path,
                offset: first.offset,
                pending: first.pending,
            },
        )?;
        assert!(
            second.restarted,
            "a shorter file must be reported as a restart, got offset {} for a file of {} bytes",
            first.offset,
            std::fs::metadata(&path)?.len()
        );
        assert_eq!(second.lines, vec!["{\"type\":\"user\",\"uuid\":\"9\"}"]);
        assert_eq!(second.offset, std::fs::metadata(&path)?.len());

        std::fs::remove_dir_all(&home_directory).ok();
        Ok(())
    }

    #[test]
    fn test_read_transcript_tail_without_a_transcript_file() -> Result<()> {
        let home_directory = temporary_directory("missing");
        let progress = read_transcript_tail(
            &home_directory,
            "session-that-has-not-written-yet",
            TailState {
                path: None,
                offset: 0,
                pending: Vec::new(),
            },
        )?;
        assert_eq!(progress.path, None);
        assert!(progress.lines.is_empty());

        std::fs::remove_dir_all(&home_directory).ok();
        Ok(())
    }

    #[test]
    fn test_read_registrations_treats_a_missing_directory_as_no_sessions() {
        let home_directory = temporary_directory("no-registry");
        let missing = home_directory.join(".claude").join("sessions");

        match read_registrations(&missing) {
            Ok(sessions) => assert!(
                sessions.is_empty(),
                "expected no sessions, got {} of them",
                sessions.len()
            ),
            Err(error) => panic!(
                "a machine with no registry directory has zero sessions, not an error to show the user; got: {error:#}"
            ),
        }

        // What that must not kill: a path that exists but cannot be listed as a
        // directory is a real failure and must still be reported.
        let not_a_directory = home_directory.join("registrations.json");
        write_file(not_a_directory.clone(), "{}");
        assert!(
            read_registrations(&not_a_directory).is_err(),
            "a path that is not a directory must still report an error"
        );

        std::fs::remove_dir_all(&home_directory).ok();
    }

    /// Claude Code writes `procStart` in UTC. Reading the start time back in the machine's
    /// own zone reported a time eight hours off on a UTC+8 machine, so every running
    /// session compared as a reused pid and the panel listed nothing at all.
    // Awaiting a real `ps` cannot happen under GPUI's deterministic test scheduler, which
    // forbids parking on anything outside its own queues, so this one drives the future
    // itself instead of taking a `TestAppContext`.
    #[cfg(unix)]
    #[test]
    fn test_process_start_times_are_reported_in_utc() {
        let mut command = util::command::new_command("sleep");
        command.arg("30");
        command.kill_on_drop(true);
        let mut child = command.spawn().expect("spawning a process to look up");
        let spawned_at_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since_epoch| since_epoch.as_secs() as i64);

        let start_times = smol::block_on(process_start_times(vec![child.id()]));
        let reported = start_times
            .get(&child.id())
            .cloned()
            .unwrap_or_else(|| "<no start time reported>".to_string());
        child.kill().log_err();

        let reported_seconds =
            match chrono::NaiveDateTime::parse_from_str(&reported, "%a %b %e %H:%M:%S %Y") {
                Ok(start_time) => start_time.and_utc().timestamp(),
                Err(error) => panic!(
                    "`{reported}` is not a start time in the format the registrations use: {error}"
                ),
            };
        let drift_seconds = reported_seconds - spawned_at_seconds;
        assert!(
            drift_seconds.abs() <= 5,
            "the process started just now, so its start time has to read as now in UTC: \
             got `{reported}` ({reported_seconds}), expected about {spawned_at_seconds}, \
             which is off by {drift_seconds} seconds"
        );
    }

    #[test]
    fn test_pane_target_accepts_the_pane_id_of_a_registration() {
        assert_eq!(pane_target("awp:@1.%1").as_deref(), Some("%1"));
        assert_eq!(pane_target("a:b:@10.%234").as_deref(), Some("%234"));
    }

    #[test]
    fn test_pane_target_rejects_everything_that_is_not_a_pane_id() {
        for rejected in ["awp:@1", "awp:@1.%", "awp:@1.%x", "-t", "", "%1 ; rm -rf /"] {
            assert_eq!(
                pane_target(rejected),
                None,
                "`{rejected}` is not a pane id and must never reach tmux"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_list_sessions_reports_sessions_with_and_without_a_transcript() -> Result<()> {
        smol::block_on(async {
            let home_directory = temporary_directory("list-sessions");

            // Liveness compares a registration against a real process, so the two live
            // sessions are backed by this test process and by a child it can wait on.
            let mut command = util::command::new_command("sleep");
            command.arg("30");
            command.kill_on_drop(true);
            let mut child = command.spawn().expect("spawning a second live process");
            let with_transcript_pid = std::process::id();
            let without_transcript_pid = child.id();

            let process_starts =
                process_start_times(vec![with_transcript_pid, without_transcript_pid]).await;
            let start_time_of = |process_id: u32| -> String {
                process_starts
                    .get(&process_id)
                    .cloned()
                    .unwrap_or_else(|| panic!("`ps` reported no start time for pid {process_id}"))
            };

            let registry_directory = home_directory.join(".claude").join("sessions");
            write_file(
                registry_directory.join(format!("{with_transcript_pid}.json")),
                &format!(
                    r#"{{"pid":{with_transcript_pid},"sessionId":"has-transcript","cwd":"/tmp",
"procStart":"{}","version":"2.1.267","kind":"interactive","name":"alpha"}}"#,
                    start_time_of(with_transcript_pid)
                ),
            );
            write_file(
                registry_directory.join(format!("{without_transcript_pid}.json")),
                &format!(
                    r#"{{"pid":{without_transcript_pid},"sessionId":"no-transcript-yet","cwd":"/tmp",
"procStart":"{}","version":"2.1.267","kind":"interactive","name":"beta"}}"#,
                    start_time_of(without_transcript_pid)
                ),
            );
            let transcript_path = write_transcript(
                &home_directory,
                "has-transcript",
                "{\"type\":\"user\",\"uuid\":\"a\"}\n",
            );

            let summaries = list_sessions(&home_directory, None).await?;
            child.kill().log_err();

            let session_ids: Vec<&str> = summaries
                .iter()
                .map(|summary| summary.session.session_id.as_str())
                .collect();
            assert_eq!(
                session_ids,
                vec!["has-transcript", "no-transcript-yet"],
                "both live sessions must be listed, ordered by name"
            );

            let transcript_of = |session_id: &str| -> Option<PathBuf> {
                summaries
                    .iter()
                    .find(|summary| summary.session.session_id == session_id)
                    .and_then(|summary| summary.transcript_path.clone())
            };
            assert_eq!(
                transcript_of("has-transcript"),
                Some(transcript_path),
                "the transcript must be located by globbing the projects directory"
            );
            assert_eq!(
                transcript_of("no-transcript-yet"),
                None,
                "a session that has written no transcript is still listed, without a path"
            );

            std::fs::remove_dir_all(&home_directory).ok();
            Ok(())
        })
    }

    #[cfg(unix)]
    #[test]
    fn test_send_text_and_send_escape_reject_an_empty_pane_target() {
        smol::block_on(async {
            let send_text_error = send_text("", "hello")
                .await
                .expect_err("an empty pane target must not be sent to");
            assert!(
                send_text_error.to_string().contains("no tmux pane"),
                "the failure has to name what went wrong, got `{send_text_error:#}`"
            );

            let send_escape_error = send_escape("")
                .await
                .expect_err("an empty pane target must not be sent to");
            assert!(
                send_escape_error.to_string().contains("no tmux pane"),
                "the failure has to name what went wrong, got `{send_escape_error:#}`"
            );
        });
    }

    /// `send_text` sends the message through this, and it is the part that cannot be
    /// exercised against tmux without pasting into a live pane. `cat` stands in for
    /// `tmux load-buffer`: both read until end of input, so a write that is never closed
    /// or a wait that happens too early hangs here exactly as it would there.
    #[cfg(unix)]
    #[test]
    fn test_run_with_stdin_writes_the_whole_input_and_waits_for_the_command() {
        smol::block_on(async {
            // Larger than a pipe buffer, so that the write cannot complete until the
            // command has started draining it.
            let long_message = "line of a multi-line message\n".repeat(4096);
            run_with_stdin("/bin/cat", &[], long_message.as_bytes())
                .await
                .expect("cat reads its stdin to the end and exits successfully");
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_run_with_stdin_reports_the_stderr_of_a_command_that_fails() {
        smol::block_on(async {
            let error =
                match run_with_stdin("/bin/cat", &["/this/path/does/not/exist"], b"unused input")
                    .await
                {
                    Ok(()) => panic!("reading a path that does not exist must fail"),
                    Err(error) => format!("{error:#}"),
                };
            assert!(
                error.contains("/this/path/does/not/exist") && error.contains("failed"),
                "the failure must carry what the command printed on stderr, got `{error}`"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_send_escape_reports_the_stderr_of_a_pane_that_does_not_exist() {
        smol::block_on(async {
            // A pane id far above anything a real tmux server has handed out, so this
            // cannot reach a live pane. A machine without tmux fails just as usefully.
            let error = match send_escape("%99999999").await {
                Ok(()) => panic!("sending to a pane that does not exist must fail"),
                Err(error) => format!("{error:#}"),
            };
            assert!(
                error.contains("send-keys") || error.contains("running tmux"),
                "the failure must say which tmux command failed, got `{error}`"
            );
        });
    }

    #[test]
    fn a_session_id_cannot_lead_the_transcript_search_out_of_the_projects_directory() -> Result<()>
    {
        let home_directory = temporary_directory("session-id-boundary");
        let session_id = "4e2e3600-89c0-4cd5-9994-525c708559ab";
        let transcript = write_transcript(&home_directory, session_id, "{\"type\":\"user\"}\n");

        assert_eq!(
            find_transcript(&home_directory, session_id),
            Some(transcript),
            "the shape a real session id has must still resolve, or the boundary has \
             eaten the only thing it exists to let through"
        );

        // `<home>/.claude/projects/-some-slug` is three levels below the home directory,
        // so three parent components reach a file the tail has no business reading.
        let outside = home_directory.join("outside.jsonl");
        write_file(outside.clone(), "{\"secret\":\"not a transcript\"}\n");

        for escaping_session_id in [
            "../../../outside",
            "./../../../outside",
            "-some-slug/../../../outside",
            "../../../../../../../../../../../../etc/hosts",
        ] {
            let found = find_transcript(&home_directory, escaping_session_id);
            assert_eq!(
                found, None,
                "a session id of {escaping_session_id:?} must not resolve to a path \
                 outside the projects directory, but it resolved to {found:?}"
            );
        }

        let progress = read_transcript_tail(
            &home_directory,
            "../../../outside",
            TailState {
                path: None,
                offset: 0,
                pending: Vec::new(),
            },
        )?;
        assert_eq!(
            progress.path, None,
            "a tail must not follow a path a session id climbed out to"
        );
        assert!(
            progress.lines.is_empty(),
            "a tail must not hand back the contents of {}, but it returned {:?}",
            outside.display(),
            progress.lines
        );

        Ok(())
    }

    /// Serializes the tests that touch the machine's tmux server. One of them asserts
    /// that this process leaves no paste buffer behind while the other holds a paste
    /// buffer for as long as its send takes, and there is only one buffer stack for both
    /// of them to be right about.
    static TMUX_SERVER_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A test that panicked while holding the lock says nothing about whether the next
    /// test may run, so the poison is stepped over rather than turned into a second
    /// failure that hides the first.
    fn tmux_server_lock() -> std::sync::MutexGuard<'static, ()> {
        TMUX_SERVER_TESTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[cfg(unix)]
    #[test]
    fn a_send_that_cannot_reach_its_pane_leaves_no_paste_buffer_behind() {
        let _tmux_server = tmux_server_lock();
        smol::block_on(async {
            // A pane id far above anything a real tmux server has handed out, so the
            // paste cannot reach a live pane; `load-buffer` names no pane at all.
            let error = match send_text("%99999999", "text that must not stay in tmux").await {
                Ok(()) => panic!("sending to a pane that does not exist must fail"),
                Err(error) => format!("{error:#}"),
            };

            let buffer_prefix = format!("zed-claude-{}-", std::process::id());
            let mut list_buffers = util::command::new_command("tmux");
            list_buffers.args(["list-buffers", "-F", "#{buffer_name}"]);
            // A machine with no tmux server has no buffer stack to leak into.
            let leftover_buffers: Vec<String> = match list_buffers.output().await {
                Err(_) => Vec::new(),
                Ok(output) => String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter(|buffer_name| buffer_name.starts_with(&buffer_prefix))
                    .map(str::to_string)
                    .collect(),
            };

            // Removed before the assertion, so that a failing run does not leave behind
            // the very text it is complaining about.
            for buffer_name in &leftover_buffers {
                let mut delete_buffer = util::command::new_command("tmux");
                delete_buffer.args(["delete-buffer", "-b", buffer_name]);
                delete_buffer.output().await.log_err();
            }

            assert!(
                leftover_buffers.is_empty(),
                "a send that failed with `{error}` must leave no paste buffer behind, \
                 but tmux still held {leftover_buffers:?}"
            );
        });
    }

    #[test]
    fn a_send_runs_one_tmux_command_list_so_two_sends_cannot_interleave() {
        let arguments = send_text_arguments("zed-claude-1-0", "%12", true);

        let separators = arguments
            .iter()
            .filter(|argument| argument.as_str() == ";")
            .count();
        assert_eq!(
            separators, 2,
            "loading, pasting and submitting have to travel as one tmux command list, \
             separated by two `;` arguments, but the send runs {arguments:?}"
        );
        assert_eq!(
            arguments,
            vec![
                "load-buffer",
                "-b",
                "zed-claude-1-0",
                "-",
                ";",
                "paste-buffer",
                "-d",
                "-p",
                "-b",
                "zed-claude-1-0",
                "-t",
                "%12",
                ";",
                "send-keys",
                "-t",
                "%12",
                "Enter",
            ],
            "the three steps have to stay in this order within the one command list"
        );
    }

    /// Bracketed paste puts the pane's TUI into paste mode until the closing sequence
    /// lands, and a message with no newline in it has nothing for that mode to protect.
    #[test]
    fn only_a_multi_line_send_asks_tmux_for_a_bracketed_paste() {
        let asked_for: Vec<(&str, bool)> = ["one line", "line one\nline two", "trailing\n"]
            .into_iter()
            .map(|text| (text, needs_bracketed_paste(text)))
            .collect();
        assert_eq!(
            asked_for,
            vec![
                // No newline to protect, so the pane is never put into paste mode.
                ("one line", false),
                ("line one\nline two", true),
                ("trailing\n", true),
            ],
            "bracketed paste is what keeps a newline from submitting the message early, \
             and only a message that has one needs it"
        );

        let single_line = send_text_arguments("zed-claude-1-0", "%12", false);
        assert!(
            !single_line.iter().any(|argument| argument == "-p"),
            "a single-line send must not ask for a bracketed paste, but it runs {single_line:?}"
        );
        assert_eq!(
            single_line,
            vec![
                "load-buffer",
                "-b",
                "zed-claude-1-0",
                "-",
                ";",
                "paste-buffer",
                "-d",
                "-b",
                "zed-claude-1-0",
                "-t",
                "%12",
                ";",
                "send-keys",
                "-t",
                "%12",
                "Enter",
            ],
            "dropping `-p` must be the only difference: one command list, `-d` kept, \
             the three steps in the same order"
        );
    }

    /// Sends into a pane this test creates and kills, never into a pane a real Claude
    /// Code session is running in. `cat` has not asked for bracketed paste, so tmux
    /// leaves the paste sequences out either way and what lands in the file is the
    /// message itself: this is about the text and its newlines arriving intact, while
    /// [`only_a_multi_line_send_asks_tmux_for_a_bracketed_paste`] is what pins the flag.
    #[cfg(unix)]
    #[test]
    fn a_single_line_and_a_multi_line_send_both_arrive_verbatim() {
        let _tmux_server = tmux_server_lock();
        smol::block_on(async {
            let session_name = format!("zed-claude-send-shapes-test-{}", std::process::id());
            let output_path = std::env::temp_dir().join(format!("{session_name}.out"));
            std::fs::remove_file(&output_path).ok();

            let mut new_session = util::command::new_command("tmux");
            new_session.args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                &format!("cat >> '{}'", output_path.display()),
            ]);
            // A machine with no tmux binary, or one that cannot start a server, has no
            // pane for this to send into.
            match new_session.output().await {
                Err(_) => return,
                Ok(output) if !output.status.success() => return,
                Ok(_) => {}
            }

            let received = async {
                let mut list_panes = util::command::new_command("tmux");
                list_panes.args(["list-panes", "-t", &session_name, "-F", "#{pane_id}"]);
                let panes = list_panes
                    .output()
                    .await
                    .context("listing the panes of the test session")?;
                let pane_target = String::from_utf8_lossy(&panes.stdout).trim().to_string();
                anyhow::ensure!(
                    !pane_target.is_empty(),
                    "the test session reported no pane to send to"
                );

                // The pane's `cat` appends, so the file accumulates both sends and the
                // tail of it is what says the send being waited on has arrived.
                let await_tail = |tail: &'static str| {
                    let output_path = output_path.clone();
                    async move {
                        for _ in 0..100 {
                            let received =
                                std::fs::read_to_string(&output_path).unwrap_or_default();
                            if received.ends_with(tail) {
                                return anyhow::Ok(received);
                            }
                            // Sleeping the thread rather than awaiting a timer, because
                            // what this waits for is another process writing a file.
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                        anyhow::bail!(
                            "the pane never received a line ending in {tail:?}, only `{}`",
                            std::fs::read_to_string(&output_path).unwrap_or_default()
                        )
                    }
                };

                send_text(&pane_target, "a single line").await?;
                let after_single_line = await_tail("a single line\n").await?;

                send_text(&pane_target, "line one\nline two").await?;
                let after_multi_line = await_tail("line two\n").await?;

                anyhow::Ok((after_single_line, after_multi_line))
            }
            .await;

            // Killed before the assertions, so that a failing run leaves no session and
            // no file behind.
            let mut kill_session = util::command::new_command("tmux");
            kill_session.args(["kill-session", "-t", &session_name]);
            kill_session.output().await.log_err();
            std::fs::remove_file(&output_path).ok();

            let (after_single_line, after_multi_line) =
                received.expect("the sends into the test's own pane must succeed");
            assert_eq!(
                after_single_line, "a single line\n",
                "a single-line send has to arrive as that line, submitted once"
            );
            assert_eq!(
                after_multi_line, "a single line\nline one\nline two\n",
                "the multi-line send that follows it has to arrive whole, submitted once"
            );
        });
    }

    /// Sends into a pane this test creates and kills, never into a pane a real Claude
    /// Code session is running in.
    #[cfg(unix)]
    #[test]
    fn a_multi_line_send_arrives_in_the_pane_as_one_message() {
        let _tmux_server = tmux_server_lock();
        smol::block_on(async {
            let session_name = format!("zed-claude-send-text-test-{}", std::process::id());
            let output_path = std::env::temp_dir().join(format!("{session_name}.out"));
            std::fs::remove_file(&output_path).ok();

            let mut new_session = util::command::new_command("tmux");
            new_session.args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                &format!("cat >> '{}'", output_path.display()),
            ]);
            // A machine with no tmux binary, or one that cannot start a server, has no
            // pane for this to send into; the rest of the suite still covers the
            // arguments and the failure path.
            match new_session.output().await {
                Err(_) => return,
                Ok(output) if !output.status.success() => return,
                Ok(_) => {}
            }

            let received = async {
                let mut list_panes = util::command::new_command("tmux");
                list_panes.args(["list-panes", "-t", &session_name, "-F", "#{pane_id}"]);
                let panes = list_panes
                    .output()
                    .await
                    .context("listing the panes of the test session")?;
                let pane_target = String::from_utf8_lossy(&panes.stdout).trim().to_string();
                anyhow::ensure!(
                    !pane_target.is_empty(),
                    "the test session reported no pane to send to"
                );

                send_text(&pane_target, "line one\nline two").await?;

                // tmux accepts the paste and the Enter before the pane's `cat` has seen
                // them, and the second line only reaches the file once Enter has been
                // typed, so the trailing newline is what says the whole send arrived.
                // A missing file is what "nothing has arrived yet" looks like here.
                for _ in 0..100 {
                    let received = std::fs::read_to_string(&output_path).unwrap_or_default();
                    if received.ends_with('\n') {
                        return anyhow::Ok(received);
                    }
                    // Sleeping the thread rather than awaiting a timer, because what
                    // this waits for is another process writing a file, and nothing
                    // else is queued on this test's executor to be starved by it.
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                anyhow::bail!(
                    "the pane never received a submitted line, only `{}`",
                    std::fs::read_to_string(&output_path).unwrap_or_default()
                )
            }
            .await;

            // Killed before the assertion, so that a failing run leaves no session and
            // no file behind.
            let mut kill_session = util::command::new_command("tmux");
            kill_session.args(["kill-session", "-t", &session_name]);
            kill_session.output().await.log_err();
            std::fs::remove_file(&output_path).ok();

            let received = received.expect("the send into the test's own pane must succeed");
            assert_eq!(
                received, "line one\nline two\n",
                "both lines have to arrive as one message, submitted once"
            );
        });
    }

    /// Both sidecars are verbatim rows from this machine's `subagents` directories.
    const SUBAGENT_META_GENERAL_PURPOSE: &str = r#"{"agentType":"general-purpose",
 "description":"Phase B adversarial review round 2",
 "toolUseId":"toolu_01BEgVRSnksoz6YAyUWEEdeQ","spawnDepth":1,
 "requestShape":"background","requestNonInteractive":true,"model":"opus"}"#;

    const SUBAGENT_META_WORKFLOW: &str = r#"{"agentType":"workflow-subagent",
 "description":"R2:send-atomicity","workflowPhase":"Wave 5","spawnDepth":1,
 "requestShape":"foreground","requestNonInteractive":false,"model":"opus"}"#;

    const REAL_SESSION_ID: &str = "4e2e3600-89c0-4cd5-9994-525c708559ab";
    const REAL_AGENT_ID: &str = "af090e203ec41bc73";
    const REAL_WORKFLOW_RUN_ID: &str = "wf_b529a29d-562";

    /// Ids that must never reach a path, whichever of the three they are given as.
    const IDS_THAT_NAME_MORE_THAN_ONE_ENTRY: [&str; 6] = [
        "",
        ".",
        "..",
        "../..",
        "a/b",
        "-some-slug/4e2e3600-89c0-4cd5-9994-525c708559ab",
    ];

    /// Writes one subagent's sidecar and transcript into the layout the run id selects,
    /// and returns the transcript's path.
    fn write_subagent(
        home_directory: &Path,
        session_id: &str,
        workflow_run_id: Option<&str>,
        agent_id: &str,
        meta_contents: &str,
        transcript_contents: &str,
    ) -> PathBuf {
        let mut directory = home_directory
            .join(".claude")
            .join("projects")
            .join("-some-slug")
            .join(session_id)
            .join("subagents");
        if let Some(workflow_run_id) = workflow_run_id {
            directory = directory.join("workflows").join(workflow_run_id);
        }
        std::fs::create_dir_all(&directory).expect("creating the subagents directory");
        std::fs::write(
            directory.join(format!("agent-{agent_id}.meta.json")),
            meta_contents,
        )
        .expect("writing the subagent meta");
        let transcript_path = directory.join(format!("agent-{agent_id}.jsonl"));
        std::fs::write(&transcript_path, transcript_contents).expect("writing the transcript");
        transcript_path
    }

    /// Without the field renames every one of these reads back empty or fails to parse,
    /// so each field is asserted rather than the parse merely succeeding.
    #[test]
    fn subagent_meta_of_a_background_agent_keeps_every_recorded_field() -> Result<()> {
        let meta = parse_subagent_meta(SUBAGENT_META_GENERAL_PURPOSE)?;
        assert_eq!(meta.agent_type, "general-purpose");
        assert_eq!(
            meta.description.as_deref(),
            Some("Phase B adversarial review round 2")
        );
        assert_eq!(
            meta.tool_use_id.as_deref(),
            Some("toolu_01BEgVRSnksoz6YAyUWEEdeQ")
        );
        assert_eq!(meta.spawn_depth, 1);
        assert_eq!(meta.model.as_deref(), Some("opus"));
        assert_eq!(
            meta.workflow_phase, None,
            "an agent spawned by the Task tool belongs to no workflow phase"
        );
        Ok(())
    }

    /// A workflow's agents record a phase and, on this machine, never a `toolUseId`, so
    /// treating either as required loses 270 of the 394 sidecars here.
    #[test]
    fn subagent_meta_of_a_workflow_agent_keeps_its_phase_without_a_tool_use_id() -> Result<()> {
        let meta = parse_subagent_meta(SUBAGENT_META_WORKFLOW)?;
        assert_eq!(meta.agent_type, "workflow-subagent");
        assert_eq!(meta.description.as_deref(), Some("R2:send-atomicity"));
        assert_eq!(meta.tool_use_id, None);
        assert_eq!(meta.workflow_phase.as_deref(), Some("Wave 5"));
        assert_eq!(meta.spawn_depth, 1);
        assert_eq!(meta.model.as_deref(), Some("opus"));
        Ok(())
    }

    /// The agent type is the one field a caller cannot do without, so its absence has to
    /// be an error rather than an empty string that renders as an unnamed agent.
    #[test]
    fn subagent_meta_without_an_agent_type_is_an_error() {
        let without_agent_type = r#"{"description":"Phase B","spawnDepth":1,"model":"opus"}"#;
        assert!(parse_subagent_meta(without_agent_type).is_err());
    }

    /// The writer ships independently of this reader, so a field added between releases
    /// must not take the whole row down with it.
    #[test]
    fn subagent_meta_ignores_a_field_this_version_does_not_know() -> Result<()> {
        let with_future_field = r#"{"agentType":"fork","spawnDepth":2,
 "futureField":{"nested":[1,2,3]}}"#;
        let meta = parse_subagent_meta(with_future_field)?;
        assert_eq!(
            meta.agent_type, "fork",
            "the agent type is free-form text, not a closed set of known kinds"
        );
        assert_eq!(meta.spawn_depth, 2);
        Ok(())
    }

    /// Reading an absent depth as zero would claim the agent has no spawner at all,
    /// where every sidecar without the field describes an agent the session spawned.
    #[test]
    fn subagent_meta_without_a_spawn_depth_reads_as_one() -> Result<()> {
        let meta = parse_subagent_meta(r#"{"agentType":"Explore"}"#)?;
        assert_eq!(meta.spawn_depth, 1);
        assert_eq!(meta.description, None);
        assert_eq!(meta.tool_use_id, None);
        assert_eq!(meta.model, None);
        assert_eq!(meta.workflow_phase, None);
        Ok(())
    }

    /// Asserts both halves of the boundary. Without the checks the decoy files written
    /// below resolve: a session id of `../..` reaches `<home>/.claude`, next door to the
    /// `sessions` directory holding the sessions' socket credentials; an agent id with a
    /// separator reaches any file under `subagents`; and a workflow run id of `..`
    /// silently answers with a different agent's transcript.
    #[test]
    fn no_id_can_lead_a_subagent_path_out_of_the_directory_it_belongs_to() -> Result<()> {
        let home_directory = temporary_directory("subagent-id-boundary");
        let plain_transcript = write_subagent(
            &home_directory,
            REAL_SESSION_ID,
            None,
            REAL_AGENT_ID,
            SUBAGENT_META_GENERAL_PURPOSE,
            "{\"type\":\"user\",\"isSidechain\":true}\n",
        );
        let workflow_transcript = write_subagent(
            &home_directory,
            REAL_SESSION_ID,
            Some(REAL_WORKFLOW_RUN_ID),
            REAL_AGENT_ID,
            SUBAGENT_META_WORKFLOW,
            "{\"type\":\"user\",\"isSidechain\":true}\n",
        );

        assert_eq!(
            subagent_transcript_path(&home_directory, REAL_SESSION_ID, REAL_AGENT_ID, None),
            Some(plain_transcript.clone()),
            "the shapes a real session and agent id have must still resolve, or the \
             boundary has eaten the only thing it exists to let through"
        );
        assert_eq!(
            subagent_transcript_path(
                &home_directory,
                REAL_SESSION_ID,
                REAL_AGENT_ID,
                Some(REAL_WORKFLOW_RUN_ID)
            ),
            Some(workflow_transcript),
            "a real workflow run id must still resolve"
        );

        // `<home>/.claude/projects/-some-slug/<sessionId>` is two levels below `.claude`,
        // so two parent components in a session id land a lookup right beside the
        // `sessions` directory this module must never read.
        write_file(
            home_directory
                .join(".claude")
                .join("subagents")
                .join(format!("agent-{REAL_AGENT_ID}.jsonl")),
            "{\"secret\":\"not a subagent transcript\"}\n",
        );
        write_file(
            plain_transcript
                .with_file_name("agent-nested")
                .join("secret.jsonl"),
            "{\"secret\":\"not a subagent transcript\"}\n",
        );

        assert_eq!(
            subagent_transcript_path(&home_directory, "../..", REAL_AGENT_ID, None),
            None,
            "a session id of `../..` must not reach a file beside the sessions directory"
        );
        assert_eq!(
            subagent_transcript_path(&home_directory, REAL_SESSION_ID, "nested/secret", None),
            None,
            "an agent id carrying a separator must not reach a file that is not an \
             agent transcript"
        );
        assert_eq!(
            subagent_transcript_path(&home_directory, REAL_SESSION_ID, REAL_AGENT_ID, Some("..")),
            None,
            "a workflow run id of `..` must not answer with the transcript of the \
             agent that belongs to no run"
        );

        for rejected in IDS_THAT_NAME_MORE_THAN_ONE_ENTRY {
            let session_id_rejected =
                subagent_transcript_path(&home_directory, rejected, REAL_AGENT_ID, None);
            assert_eq!(
                session_id_rejected, None,
                "a session id of {rejected:?} must not resolve, but it resolved to \
                 {session_id_rejected:?}"
            );
            let agent_id_rejected =
                subagent_transcript_path(&home_directory, REAL_SESSION_ID, rejected, None);
            assert_eq!(
                agent_id_rejected, None,
                "an agent id of {rejected:?} must not resolve, but it resolved to \
                 {agent_id_rejected:?}"
            );
            let run_id_rejected = subagent_transcript_path(
                &home_directory,
                REAL_SESSION_ID,
                REAL_AGENT_ID,
                Some(rejected),
            );
            assert_eq!(
                run_id_rejected, None,
                "a workflow run id of {rejected:?} must not resolve, but it resolved to \
                 {run_id_rejected:?}"
            );

            let listed = smol::block_on(list_subagents(&home_directory, rejected))?;
            assert!(
                listed.is_empty(),
                "a session id of {rejected:?} must list no subagents, but it listed {}",
                listed.len()
            );
        }

        Ok(())
    }

    /// A command written by someone is the one the reader is looking for, and a project's
    /// own shadows the user's of the same name the way the CLI resolves them.
    #[test]
    fn a_machines_own_commands_come_before_the_built_in_ones() -> Result<()> {
        let home_directory = temporary_directory("slash-commands");
        let project_root = home_directory.join("project");

        write_file(
            home_directory
                .join(".claude")
                .join("commands")
                .join("review.md"),
            "---\ndescription: The user's own review\n---\nbody",
        );
        write_file(
            project_root
                .join(".claude")
                .join("commands")
                .join("review.md"),
            "---\ndescription: This project's review\nargument-hint: \"[pr]\"\n---\nbody",
        );
        write_file(
            home_directory
                .join(".claude")
                .join("commands")
                .join("deep")
                .join("audit.md"),
            "no front matter here",
        );
        // A built-in the machine also defines must appear once, as the machine's.
        write_file(
            home_directory
                .join(".claude")
                .join("commands")
                .join("cost.md"),
            "---\ndescription: My own cost\n---\nbody",
        );

        let commands = list_slash_commands(&home_directory, Some(&project_root));
        let named = |name: &str| {
            commands
                .iter()
                .filter(|command| command.name == name)
                .collect::<Vec<_>>()
        };

        let review = named("review");
        assert_eq!(review.len(), 1, "a name is one command, not two");
        assert_eq!(
            review.first().map(|command| command.scope),
            Some(SlashCommandScope::Project),
            "the project's own shadows the user's"
        );
        assert_eq!(
            review
                .first()
                .and_then(|command| command.argument_hint.as_deref()),
            Some("[pr]"),
            "the quotes belong to the front matter, not to the hint"
        );

        assert_eq!(
            named("deep:audit")
                .first()
                .map(|command| command.name.as_str()),
            Some("deep:audit"),
            "a command in a directory is named for the directory it is in"
        );
        assert_eq!(
            named("deep:audit")
                .first()
                .and_then(|c| c.description.as_deref()),
            None,
            "a command with no front matter is still a command"
        );

        assert_eq!(
            named("cost").first().map(|command| command.scope),
            Some(SlashCommandScope::User),
            "a name the machine defines is the machine's, not the built-in one"
        );
        assert!(
            commands
                .iter()
                .any(|command| command.name == "clear"
                    && command.scope == SlashCommandScope::Builtin),
            "the built-in commands are still offered"
        );

        let machine_commands = commands
            .iter()
            .take_while(|command| command.scope != SlashCommandScope::Builtin)
            .count();
        assert_eq!(
            machine_commands, 3,
            "the project's `review`, and the user's `cost` and `deep:audit` — the user's \
             `review` having been shadowed — all come before the built-in ones"
        );
        Ok(())
    }

    /// A machine with no commands of its own still answers to the built-in ones, and
    /// listing nothing would leave the reader with an empty menu.
    #[test]
    fn a_machine_with_no_commands_of_its_own_still_has_the_built_in_ones() {
        let home_directory = temporary_directory("slash-commands-none");
        let commands = list_slash_commands(&home_directory, None);

        assert_eq!(commands.len(), BUILTIN_SLASH_COMMANDS.len());
        assert!(
            commands
                .iter()
                .all(|command| command.scope == SlashCommandScope::Builtin)
        );
    }

    #[test]
    fn test_slash_commands_ignores_hidden_and_avoids_symlink_cycles() {
        let home_directory = temporary_directory("slash-commands-cycles");
        let project_root = temporary_directory("slash-project-cycles");
        let commands_dir = project_root.join(".claude").join("commands");
        fs::create_dir_all(&commands_dir).unwrap();

        write_file(
            commands_dir.join("valid.md"),
            "---\ndescription: Valid command\n---\nbody",
        );
        write_file(
            commands_dir.join(".hidden.md"),
            "---\ndescription: Hidden file\n---\nbody",
        );

        let hidden_dir = commands_dir.join(".git");
        fs::create_dir_all(&hidden_dir).unwrap();
        write_file(
            hidden_dir.join("ignored.md"),
            "---\ndescription: Hidden dir command\n---\nbody",
        );

        let commands = list_slash_commands(&home_directory, Some(&project_root));
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();

        assert!(
            names.contains(&"valid"),
            "expected `valid` command to be loaded"
        );
        assert_eq!(
            names
                .iter()
                .filter(|n| n.starts_with('.'))
                .copied()
                .collect::<Vec<_>>(),
            Vec::<&str>::new(),
            "hidden files and hidden directories must be ignored"
        );
    }

    #[test]
    fn test_live_message_hook_truncates_when_index_zero_has_no_trailing_comma() -> Result<()> {
        let temp_dir = temporary_directory("hook-truncate-test");
        let session_id = "test-session";
        let live_dir = temp_dir.join(".claude").join("live-messages");
        fs::create_dir_all(&live_dir)?;
        let message_file = live_dir.join(format!("{session_id}.jsonl"));
        write_file(
            message_file.clone(),
            "stale content from previous message\n",
        );

        let hook_path = temp_dir.join("hook.sh");
        fs::write(&hook_path, LIVE_MESSAGE_HOOK_SOURCE)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755))?;
        }

        let payload = r#"{"type":"content_block_start","session_id":"test-session","index":0}"#;

        // The hook reads its payload from stdin, and the shell is what feeds it: the
        // pattern under test is the shell's own `case` glob, so running it any other way
        // would be testing a reimplementation of it rather than the hook that ships.
        let status = smol::block_on(
            smol::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(r#"printf '%s' "$1" | /bin/sh "$2""#)
                .arg("--")
                .arg(payload)
                .arg(&hook_path)
                .env("HOME", &temp_dir)
                .status(),
        )?;
        assert!(status.success(), "the hook must always exit 0");

        let contents = fs::read_to_string(&message_file)?;
        assert_eq!(
            contents.lines().next(),
            Some(payload),
            "hook must truncate file on index:0 even when object ends without a comma"
        );
        Ok(())
    }

    /// The pieces of a message arrive in the order the terminal drew them, but the file
    /// is appended to while it is read, so the order is not what this reads them by.
    #[test]
    fn a_message_is_put_back_together_in_the_order_its_pieces_are_numbered() {
        let out_of_order = concat!(
            r#"{"message_id":"m1","index":1,"final":false,"delta":" world"}"#,
            "\n",
            r#"{"message_id":"m1","index":0,"final":false,"delta":"hello"}"#,
            "\n",
            r#"{"message_id":"m1","index":2,"final":true,"delta":"!"}"#,
            "\n",
        );
        assert_eq!(
            assemble_live_message(out_of_order).as_deref(),
            Some("hello world!")
        );
    }

    /// A message beginning is what clears the one before it, and the hook truncates on
    /// the same signal. When that truncation did not happen — the file could not be
    /// written, or a message began while it was being read — two messages would otherwise
    /// be shown run together as one.
    #[test]
    fn only_the_newest_message_in_the_file_is_assembled() {
        let two_messages = concat!(
            r#"{"message_id":"m1","index":0,"final":true,"delta":"the older one"}"#,
            "\n",
            r#"{"message_id":"m2","index":0,"final":false,"delta":"the newer"}"#,
            "\n",
            r#"{"message_id":"m2","index":1,"final":true,"delta":" one"}"#,
            "\n",
        );
        assert_eq!(
            assemble_live_message(two_messages).as_deref(),
            Some("the newer one")
        );
    }

    /// The last line is regularly half-written, because it is appended to while it is
    /// being read. Losing the piece being written is right; losing the message is not.
    #[test]
    fn a_half_written_piece_costs_only_itself() {
        let half_written = concat!(
            r#"{"message_id":"m1","index":0,"final":false,"delta":"what is here"}"#,
            "\n",
            r#"{"message_id":"m1","index":1,"final":fal"#,
        );
        assert_eq!(
            assemble_live_message(half_written).as_deref(),
            Some("what is here")
        );
    }

    /// Nothing recorded, and nothing but whitespace recorded, are both a session that has
    /// nothing to show — and a blank block under the conversation is worse than none.
    #[test]
    fn a_message_of_nothing_is_not_a_message() {
        assert_eq!(assemble_live_message(""), None);
        assert_eq!(
            assemble_live_message(r#"{"message_id":"m1","index":0,"delta":"   \n "}"#),
            None
        );
        assert_eq!(assemble_live_message("not json at all\n"), None);
    }

    /// A piece repeated — the hook ran twice for it, or a read caught a rewrite — must not
    /// double the words it carries.
    #[test]
    fn a_piece_recorded_twice_is_only_said_once() {
        let repeated = concat!(
            r#"{"message_id":"m1","index":0,"final":false,"delta":"once"}"#,
            "\n",
            r#"{"message_id":"m1","index":1,"final":false,"delta":" only"}"#,
            "\n",
            r#"{"message_id":"m1","index":1,"final":false,"delta":" only"}"#,
            "\n",
        );
        assert_eq!(
            assemble_live_message(repeated).as_deref(),
            Some("once only")
        );
    }

    /// Captured from a real session that was waiting on this call, so that the shape the
    /// CLI actually writes is what this parse is held to.
    const RECORDED_QUESTION: &str = r#"{
      "session_id": "a83d6fef-bfe3-42ee-b1a5-6d9fe13a8c4c",
      "transcript_path": "/home/coder/.claude/projects/-tmp-askq/a83d6fef.jsonl",
      "cwd": "/tmp/askq-hook-test",
      "permission_mode": "auto",
      "effort": { "level": "high" },
      "hook_event_name": "PreToolUse",
      "tool_name": "AskUserQuestion",
      "tool_input": {
        "questions": [
          {
            "question": "Which project shape should the example use?",
            "header": "Project shape",
            "options": [
              { "label": "Next.js + React", "description": "Pages and API routes" },
              { "label": "Node CLI", "description": "  " },
              { "label": "Go service" }
            ],
            "multiSelect": false
          },
          {
            "question": "Which extras should be on?",
            "header": "Extras",
            "options": [
              { "label": "CodeGraph index", "description": "Symbols and blast radius" },
              { "label": "Regression gate", "description": "Blind tests, tsc, lint" }
            ],
            "multiSelect": true
          }
        ]
      },
      "tool_use_id": "toolu_01LgCXEavHobAUwqivtgDLvs"
    }"#;

    /// Every part of this is something the panel draws, and a field that reads back empty
    /// or defaults quietly costs a question its options or its meaning, so each is
    /// asserted rather than the parse merely succeeding.
    #[test]
    fn a_recorded_question_keeps_its_options_and_which_of_them_take_several() -> Result<()> {
        let question = parse_pending_question(RECORDED_QUESTION)?
            .context("the recorded payload is a question to draw")?;

        assert_eq!(question.tool_use_id, "toolu_01LgCXEavHobAUwqivtgDLvs");
        assert_eq!(question.questions.len(), 2);

        let first = question
            .questions
            .first()
            .context("the first question survived the parse")?;
        assert_eq!(first.header, "Project shape");
        assert_eq!(
            first.question,
            "Which project shape should the example use?"
        );
        assert!(!first.multi_select);
        assert_eq!(
            first
                .options
                .iter()
                .map(|option| (option.label.as_str(), option.description.as_deref()))
                .collect::<Vec<_>>(),
            vec![
                ("Next.js + React", Some("Pages and API routes")),
                // A description of nothing but whitespace is no description: drawing it
                // leaves a blank second line under the option.
                ("Node CLI", None),
                ("Go service", None),
            ]
        );

        let second = question
            .questions
            .get(1)
            .context("the second question survived the parse")?;
        assert!(
            second.multi_select,
            "a question that takes several answers is drawn with checkboxes rather than \
             one that takes one, so losing this flag draws the wrong control"
        );
        Ok(())
    }

    /// The panel tells a question that is still waiting from one already answered by
    /// looking for the tool result that answers this id, so a payload without one cannot
    /// be drawn as waiting no matter what else it holds.
    #[test]
    fn a_payload_that_names_no_call_is_not_a_question_to_draw() -> Result<()> {
        let without_id = RECORDED_QUESTION.replace("tool_use_id", "some_other_field");
        assert_eq!(parse_pending_question(&without_id)?, None);
        Ok(())
    }

    /// Options are the whole of what the panel offers. A question with none is one the
    /// reader could look at but not answer, which is worse than leaving it to the
    /// terminal, and the last question left standing being empty must not be papered over
    /// by the others.
    #[test]
    fn a_question_with_nothing_to_pick_is_left_to_the_terminal() -> Result<()> {
        let no_options = r#"{"tool_use_id":"toolu_01","tool_input":{"questions":[
            {"question":"q","header":"h","options":[],"multiSelect":false}]}}"#;
        assert_eq!(parse_pending_question(no_options)?, None);

        let no_questions = r#"{"tool_use_id":"toolu_01","tool_input":{"questions":[]}}"#;
        assert_eq!(parse_pending_question(no_questions)?, None);

        // One question with options and one without: the one that can be drawn is kept,
        // rather than the empty one taking the whole payload down with it.
        let mixed = r#"{"tool_use_id":"toolu_01","tool_input":{"questions":[
            {"question":"a","header":"A","options":[],"multiSelect":false},
            {"question":"b","header":"B","options":[{"label":"only"}],"multiSelect":false}]}}"#;
        let parsed = parse_pending_question(mixed)?.context("the answerable question is kept")?;
        assert_eq!(
            parsed
                .questions
                .iter()
                .map(|question| question.header.as_str())
                .collect::<Vec<_>>(),
            vec!["B"]
        );
        Ok(())
    }

    /// The settings file is the user's and holds everything else they have configured, so
    /// installing has to add to it rather than write over it, and has to leave a copy of
    /// what was there.
    #[test]
    fn installing_the_hook_keeps_the_rest_of_the_settings_and_copies_them_aside() -> Result<()> {
        let home_directory = temporary_directory("question-hook-install");
        let settings_path = home_directory.join(".claude").join("settings.json");
        write_file(
            settings_path.clone(),
            r#"{"model":"opus[1m]","hooks":{"UserPromptSubmit":[{"hooks":[
                {"type":"command","command":"codegraph prompt-hook"}]}]}}"#,
        );

        assert!(
            !question_hook_is_installed(&home_directory),
            "nothing is installed before installing it"
        );

        let backup = install_question_hook(&home_directory)?
            .context("settings that existed are copied aside")?;
        assert!(
            backup.is_file(),
            "the copy has to exist to be worth anything"
        );

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path)?)?;
        assert_eq!(
            settings.get("model").and_then(serde_json::Value::as_str),
            Some("opus[1m]"),
            "a setting this install knows nothing about must survive it"
        );
        assert_eq!(
            settings
                .pointer("/hooks/UserPromptSubmit/0/hooks/0/command")
                .and_then(serde_json::Value::as_str),
            Some("codegraph prompt-hook"),
            "another hook of another event must survive it"
        );
        assert!(question_hook_is_installed(&home_directory));

        let script = home_directory.join(".claude").join(QUESTION_HOOK_SCRIPT);
        assert!(script.is_file(), "the hook has to have something to run");
        Ok(())
    }

    /// The button offering to install is drawn from what is installed, so a second press
    /// — or a press against settings someone else has already added it to — must not
    /// leave the hook named twice or bury the settings under a pile of copies.
    #[test]
    fn installing_the_hook_a_second_time_changes_nothing() -> Result<()> {
        let home_directory = temporary_directory("question-hook-twice");
        install_question_hook(&home_directory)?;
        let after_first = fs::read_to_string(home_directory.join(".claude").join("settings.json"))?;

        assert_eq!(
            install_question_hook(&home_directory)?,
            None,
            "nothing was changed, so nothing had to be copied aside"
        );
        assert_eq!(
            fs::read_to_string(home_directory.join(".claude").join("settings.json"))?,
            after_first,
            "the settings are the same file they were"
        );
        Ok(())
    }

    /// A machine with no settings at all is a machine the hook can still be installed on:
    /// refusing there would leave a fresh account unable to press the button.
    #[test]
    fn the_hook_installs_onto_a_machine_with_no_settings_yet() -> Result<()> {
        let home_directory = temporary_directory("question-hook-fresh");
        assert_eq!(
            install_question_hook(&home_directory)?,
            None,
            "there were no settings to copy aside"
        );
        assert!(question_hook_is_installed(&home_directory));
        Ok(())
    }

    /// The file the hook leaves behind is written when a question is asked and never
    /// rewritten when it is answered, so a session id that could name a file outside the
    /// directory it belongs to must not be followed.
    #[test]
    fn a_recorded_question_is_read_back_for_the_session_that_asked_it() -> Result<()> {
        let home_directory = temporary_directory("question-read-back");
        write_file(
            home_directory
                .join(".claude")
                .join(PENDING_QUESTIONS_DIRECTORY)
                .join(format!("{REAL_SESSION_ID}.json")),
            RECORDED_QUESTION,
        );

        let question = read_pending_question(&home_directory, REAL_SESSION_ID)?
            .context("the session's recorded question is read back")?;
        assert_eq!(question.tool_use_id, "toolu_01LgCXEavHobAUwqivtgDLvs");

        assert_eq!(
            read_pending_question(&home_directory, "no-such-session")?,
            None,
            "a session that has asked nothing has no question waiting"
        );
        for rejected in IDS_THAT_NAME_MORE_THAN_ONE_ENTRY {
            assert_eq!(
                read_pending_question(&home_directory, rejected)?,
                None,
                "`{rejected}` must not be followed out of the directory"
            );
        }
        Ok(())
    }

    /// The journal is the only record of a workflow agent having returned, so what it
    /// says has to reach the listing agent by agent rather than run by run.
    #[test]
    fn a_runs_journal_says_which_of_its_agents_have_returned() -> Result<()> {
        let home_directory = temporary_directory("subagent-journal");
        let transcript_contents = "{\"type\":\"user\",\"isSidechain\":true}\n";

        let returned = write_subagent(
            &home_directory,
            REAL_SESSION_ID,
            Some(REAL_WORKFLOW_RUN_ID),
            "a1111e203ec41bc73",
            SUBAGENT_META_WORKFLOW,
            transcript_contents,
        );
        write_subagent(
            &home_directory,
            REAL_SESSION_ID,
            Some(REAL_WORKFLOW_RUN_ID),
            "a2222e203ec41bc73",
            SUBAGENT_META_WORKFLOW,
            transcript_contents,
        );
        // An agent listed by a sidecar the journal has not reached yet: the run started
        // it, and its result has not been written.
        write_subagent(
            &home_directory,
            REAL_SESSION_ID,
            Some(REAL_WORKFLOW_RUN_ID),
            "a3333e203ec41bc73",
            SUBAGENT_META_WORKFLOW,
            transcript_contents,
        );
        // An agent of no run at all, which no journal answers for.
        write_subagent(
            &home_directory,
            REAL_SESSION_ID,
            None,
            REAL_AGENT_ID,
            SUBAGENT_META_GENERAL_PURPOSE,
            transcript_contents,
        );
        write_file(
            returned.with_file_name(WORKFLOW_JOURNAL_FILE),
            "{\"type\":\"launched\"}\n\
             {\"type\":\"started\",\"agentId\":\"a1111e203ec41bc73\",\"label\":\"copy:a\"}\n\
             {\"type\":\"result\",\"agentId\":\"a1111e203ec41bc73\",\"result\":\"done\"}\n\
             {\"type\":\"started\",\"agentId\":\"a2222e203ec41bc73\",\"label\":\"copy:b\"}\n",
        );

        let subagents = smol::block_on(list_subagents(&home_directory, REAL_SESSION_ID))?;
        let states: Vec<(&str, Option<bool>)> = subagents
            .iter()
            .map(|subagent| (subagent.agent_id.as_str(), subagent.workflow_agent_finished))
            .collect();

        assert_eq!(
            states,
            vec![
                (REAL_AGENT_ID, None),
                ("a1111e203ec41bc73", Some(true)),
                ("a2222e203ec41bc73", Some(false)),
                ("a3333e203ec41bc73", Some(false)),
            ],
            "only the agent the journal recorded a result for has returned, and the agent \
             belonging to no run is answered by no journal at all"
        );
        Ok(())
    }

    /// A run writes its journal after it has spawned its first agents, so a run with no
    /// journal yet is a run that has only just started rather than one with nothing in
    /// it. Reporting its agents as returned would draw a starting run as a finished one.
    #[test]
    fn a_run_that_has_written_no_journal_reports_none_of_its_agents_as_returned() -> Result<()> {
        let home_directory = temporary_directory("subagent-journal-missing");
        write_subagent(
            &home_directory,
            REAL_SESSION_ID,
            Some(REAL_WORKFLOW_RUN_ID),
            "a1111e203ec41bc73",
            SUBAGENT_META_WORKFLOW,
            "{\"type\":\"user\",\"isSidechain\":true}\n",
        );

        let subagents = smol::block_on(list_subagents(&home_directory, REAL_SESSION_ID))?;
        assert_eq!(
            subagents
                .iter()
                .map(|subagent| subagent.workflow_agent_finished)
                .collect::<Vec<_>>(),
            vec![None],
            "no journal to read is not the same as a journal saying the agent returned"
        );
        Ok(())
    }

    /// The journal is appended to while it is read, so its last line is regularly
    /// half-written, and the writer ships independently of this reader and adds entry
    /// types between releases. Either one must cost only the line it is on.
    #[test]
    fn a_journal_line_this_reader_cannot_use_costs_only_that_line() -> Result<()> {
        let home_directory = temporary_directory("subagent-journal-partial");
        let transcript_contents = "{\"type\":\"user\",\"isSidechain\":true}\n";

        let first = write_subagent(
            &home_directory,
            REAL_SESSION_ID,
            Some(REAL_WORKFLOW_RUN_ID),
            "a1111e203ec41bc73",
            SUBAGENT_META_WORKFLOW,
            transcript_contents,
        );
        write_subagent(
            &home_directory,
            REAL_SESSION_ID,
            Some(REAL_WORKFLOW_RUN_ID),
            "a2222e203ec41bc73",
            SUBAGENT_META_WORKFLOW,
            transcript_contents,
        );
        write_file(
            first.with_file_name(WORKFLOW_JOURNAL_FILE),
            "{\"type\":\"somethingThisVersionHasNeverSeen\",\"agentId\":\"a2222e203ec41bc73\"}\n\
             {\"type\":\"result\",\"agentId\":\"a1111e203ec41bc73\",\"result\":\"done\"}\n\
             {\"type\":\"result\",\"agentId\":\"a2222e2",
        );

        let subagents = smol::block_on(list_subagents(&home_directory, REAL_SESSION_ID))?;
        assert_eq!(
            subagents
                .iter()
                .map(|subagent| (subagent.agent_id.as_str(), subagent.workflow_agent_finished))
                .collect::<Vec<_>>(),
            vec![
                ("a1111e203ec41bc73", Some(true)),
                ("a2222e203ec41bc73", Some(false)),
            ],
            "the readable result still counts, the half-written one does not, and neither \
             agent was dropped from the listing"
        );
        Ok(())
    }

    /// Without the workflow scan the third row is missing; without the sort the rows
    /// arrive in whatever order the directory happens to enumerate; and without the
    /// per-row skip the broken sidecar takes the whole listing with it.
    #[test]
    fn list_subagents_reads_both_layouts_and_orders_them_stably() -> Result<()> {
        let home_directory = temporary_directory("subagent-listing");
        let transcript_contents = "{\"type\":\"user\",\"isSidechain\":true}\n";

        let first = write_subagent(
            &home_directory,
            REAL_SESSION_ID,
            None,
            "a0000e203ec41bc73",
            SUBAGENT_META_GENERAL_PURPOSE,
            transcript_contents,
        );
        let second = write_subagent(
            &home_directory,
            REAL_SESSION_ID,
            None,
            REAL_AGENT_ID,
            SUBAGENT_META_GENERAL_PURPOSE,
            transcript_contents,
        );
        // Sorts between the two rows above, so its absence from the result is what shows
        // a half-written sidecar costs only its own row.
        write_subagent(
            &home_directory,
            REAL_SESSION_ID,
            None,
            "ab000e203ec41bc73",
            "{\"agentType\":",
            transcript_contents,
        );
        let workflow = write_subagent(
            &home_directory,
            REAL_SESSION_ID,
            Some(REAL_WORKFLOW_RUN_ID),
            "a1111e203ec41bc73",
            SUBAGENT_META_WORKFLOW,
            transcript_contents,
        );
        // A run directory keeps a journal beside its agents, and it names no agent.
        write_file(
            workflow.with_file_name("journal.jsonl"),
            "{\"event\":\"wave-started\"}\n",
        );

        let subagents = smol::block_on(list_subagents(&home_directory, REAL_SESSION_ID))?;
        let listed: Vec<(Option<&str>, &str, &Path)> = subagents
            .iter()
            .map(|subagent| {
                (
                    subagent.workflow_run_id.as_deref(),
                    subagent.agent_id.as_str(),
                    subagent.transcript_path.as_path(),
                )
            })
            .collect();
        assert_eq!(
            listed,
            vec![
                (None, "a0000e203ec41bc73", first.as_path()),
                (None, REAL_AGENT_ID, second.as_path()),
                (
                    Some(REAL_WORKFLOW_RUN_ID),
                    "a1111e203ec41bc73",
                    workflow.as_path()
                ),
            ]
        );

        let workflow_subagent = subagents
            .last()
            .expect("the workflow agent must be listed last");
        assert_eq!(workflow_subagent.meta.agent_type, "workflow-subagent");
        assert_eq!(
            workflow_subagent.meta.workflow_phase.as_deref(),
            Some("Wave 5")
        );
        assert_eq!(
            workflow_subagent.size,
            transcript_contents.len() as u64,
            "the size has to be the transcript's, so a caller can tail from it"
        );

        Ok(())
    }

    /// A transcript that has not been flushed yet must not drop the agent from the
    /// listing, because its sidecar is written first and the panel has to show the agent
    /// as soon as it exists.
    #[test]
    fn a_subagent_whose_transcript_is_missing_is_listed_with_no_size() -> Result<()> {
        let home_directory = temporary_directory("subagent-unflushed");
        let transcript = write_subagent(
            &home_directory,
            REAL_SESSION_ID,
            None,
            REAL_AGENT_ID,
            SUBAGENT_META_GENERAL_PURPOSE,
            "",
        );
        std::fs::remove_file(&transcript)?;

        let subagents = smol::block_on(list_subagents(&home_directory, REAL_SESSION_ID))?;
        assert_eq!(subagents.len(), 1);
        let subagent = subagents
            .first()
            .expect("the agent whose sidecar exists must be listed");
        assert_eq!(subagent.agent_id, REAL_AGENT_ID);
        assert_eq!(subagent.size, 0);
        assert_eq!(subagent.transcript_path, transcript);

        Ok(())
    }

    /// A session with no subagents at all is the common case, so it has to answer with an
    /// empty listing rather than an error the caller would have to classify.
    #[test]
    fn a_session_with_no_subagent_directory_lists_no_subagents() -> Result<()> {
        let home_directory = temporary_directory("subagent-none");
        assert!(
            smol::block_on(list_subagents(&home_directory, REAL_SESSION_ID))?.is_empty(),
            "a home directory with no projects at all must not fail the listing"
        );

        write_transcript(&home_directory, REAL_SESSION_ID, "{\"type\":\"user\"}\n");
        assert!(
            smol::block_on(list_subagents(&home_directory, REAL_SESSION_ID))?.is_empty(),
            "a session that has written a transcript but spawned no agents must list none"
        );

        Ok(())
    }

    /// The run id is the only link from a workflow's tool call to the directory holding
    /// its agents, and it arrives inside prose, so the label has to be located rather
    /// than the whole result parsed.
    #[test]
    fn a_workflow_tool_result_yields_the_run_id_it_announces() {
        let tool_result = "Workflow run started.\nRun ID: wf_b529a29d-562\nTo resume after \
                           editing the script, pass resumeFromRunId.";
        assert_eq!(
            workflow_run_id_in_tool_result(tool_result).as_deref(),
            Some("wf_b529a29d-562")
        );
        assert_eq!(
            workflow_run_id_in_tool_result("no workflow ran here"),
            None,
            "a result that announces no run must not produce a run id"
        );
        assert_eq!(
            workflow_run_id_in_tool_result("Run ID: wf_first-001\nRun ID: wf_second-002\n")
                .as_deref(),
            Some("wf_first-001"),
            "the first run id a result announces is the run it started"
        );
        assert_eq!(
            workflow_run_id_in_tool_result("Run ID:\nwf_b529a29d-562\n"),
            None,
            "a label with nothing after it on the line names no run"
        );
    }

    /// The extracted value is joined onto a path, so this is the same boundary as the one
    /// above, applied where the text is untrusted rather than the caller. Without the
    /// backslash check the middle case passes on this platform, where a backslash is an
    /// ordinary character in a name but a separator on the machine that wrote the text.
    #[test]
    fn a_run_id_that_could_name_another_directory_is_not_a_run_id() {
        for text in [
            "Run ID: ../../../etc\n",
            "Run ID: wf_a\\..\\..\\etc\n",
            "Run ID: wf_a/wf_b\n",
            "Run ID: ..\n",
            "Run ID: .\n",
            "Run ID:   \n",
        ] {
            let extracted = workflow_run_id_in_tool_result(text);
            assert_eq!(
                extracted, None,
                "{text:?} must yield no run id, but it yielded {extracted:?}"
            );
        }

        assert_eq!(
            workflow_run_id_in_tool_result("Run ID: ../../../etc\nRun ID: wf_b529a29d-562\n")
                .as_deref(),
            Some("wf_b529a29d-562"),
            "a value the boundary rejects is not a run id, so the search continues"
        );
    }

    /// The tail of an agent whose transcript is gone must answer with nothing, and above
    /// all not with the session's own conversation: a path `read_transcript_tail` cannot
    /// open sends it looking for the main transcript, which would hand the main
    /// conversation's lines back under this agent's name.
    #[test]
    fn a_subagent_tail_never_answers_with_the_main_conversation() -> Result<()> {
        let home_directory = temporary_directory("subagent-tail-no-fallback");
        write_transcript(
            &home_directory,
            REAL_SESSION_ID,
            "{\"type\":\"user\",\"uuid\":\"main-only\"}\n",
        );
        let agent_transcript = write_subagent(
            &home_directory,
            REAL_SESSION_ID,
            None,
            REAL_AGENT_ID,
            SUBAGENT_META_GENERAL_PURPOSE,
            "{\"type\":\"user\",\"isSidechain\":true,\"uuid\":\"agent-1\"}\n",
        );
        std::fs::remove_file(&agent_transcript)?;

        // The path is passed in as well, the way a client that read it from a listing
        // would send it back, so that following it is ruled out rather than untested.
        let progress = read_subagent_transcript_tail(
            &home_directory,
            REAL_SESSION_ID,
            REAL_AGENT_ID,
            None,
            TailState {
                path: Some(agent_transcript),
                offset: 0,
                pending: Vec::new(),
            },
        )?;

        assert_eq!(
            progress.lines,
            Vec::<String>::new(),
            "the tail of agent {REAL_AGENT_ID}, whose transcript is gone, must report no \
             lines, but it reported {:?} from {:?}",
            progress.lines,
            progress.path
        );
        assert_eq!(
            progress.path, None,
            "the tail must name no file, but it named {:?}",
            progress.path
        );
        assert_eq!(progress.offset, 0);
        assert!(
            !progress.restarted,
            "nothing had been read yet, so there is nothing for the caller to discard"
        );

        // The same read once the agent has absorbed something: what it has is stale, so
        // the restart has to be reported.
        let after_absorbing = read_subagent_transcript_tail(
            &home_directory,
            REAL_SESSION_ID,
            REAL_AGENT_ID,
            None,
            TailState {
                path: None,
                offset: 64,
                pending: Vec::new(),
            },
        )?;
        assert!(
            after_absorbing.restarted,
            "64 bytes had been read from a file that is gone, so the caller must be told \
             to discard them"
        );
        assert_eq!(after_absorbing.lines, Vec::<String>::new());
        assert_eq!(after_absorbing.path, None);

        Ok(())
    }

    /// The same boundary as the one on `subagent_transcript_path`, applied where the ids
    /// arrive as a tail request: an id that names more than one entry must answer with
    /// nothing rather than with whatever it happens to reach.
    #[test]
    fn no_id_can_make_a_subagent_tail_read_another_file() -> Result<()> {
        let home_directory = temporary_directory("subagent-tail-boundary");
        write_transcript(
            &home_directory,
            REAL_SESSION_ID,
            "{\"type\":\"user\",\"uuid\":\"main-only\"}\n",
        );
        write_subagent(
            &home_directory,
            REAL_SESSION_ID,
            None,
            REAL_AGENT_ID,
            SUBAGENT_META_GENERAL_PURPOSE,
            "{\"type\":\"user\",\"isSidechain\":true,\"uuid\":\"agent-1\"}\n",
        );

        for rejected in IDS_THAT_NAME_MORE_THAN_ONE_ENTRY {
            for (label, session_id, agent_id, workflow_run_id) in [
                ("session id", rejected, REAL_AGENT_ID, None),
                ("agent id", REAL_SESSION_ID, rejected, None),
                (
                    "workflow run id",
                    REAL_SESSION_ID,
                    REAL_AGENT_ID,
                    Some(rejected),
                ),
            ] {
                let progress = read_subagent_transcript_tail(
                    &home_directory,
                    session_id,
                    agent_id,
                    workflow_run_id,
                    TailState {
                        path: None,
                        offset: 0,
                        pending: Vec::new(),
                    },
                )?;
                assert_eq!(
                    progress.path, None,
                    "a {label} of {rejected:?} must name no file, but it named {:?}",
                    progress.path
                );
                assert_eq!(
                    progress.lines,
                    Vec::<String>::new(),
                    "a {label} of {rejected:?} must read nothing, but it read {:?}",
                    progress.lines
                );
            }
        }

        Ok(())
    }

    /// What the no-fallback rule must not kill: an agent whose transcript is there is
    /// still followed, incrementally, in both on-disk layouts.
    #[test]
    fn a_subagent_tail_reads_the_agents_own_lines_as_they_are_appended() -> Result<()> {
        let home_directory = temporary_directory("subagent-tail-incremental");
        write_transcript(
            &home_directory,
            REAL_SESSION_ID,
            "{\"type\":\"user\",\"uuid\":\"main-only\"}\n",
        );
        let first_line = "{\"type\":\"user\",\"isSidechain\":true,\"uuid\":\"agent-1\"}\n";
        let agent_transcript = write_subagent(
            &home_directory,
            REAL_SESSION_ID,
            Some(REAL_WORKFLOW_RUN_ID),
            REAL_AGENT_ID,
            SUBAGENT_META_WORKFLOW,
            first_line,
        );

        let tail_from = |state: TailState| {
            read_subagent_transcript_tail(
                &home_directory,
                REAL_SESSION_ID,
                REAL_AGENT_ID,
                Some(REAL_WORKFLOW_RUN_ID),
                state,
            )
        };

        let first = tail_from(TailState {
            path: None,
            offset: 0,
            pending: Vec::new(),
        })?;
        assert_eq!(
            first.lines,
            vec![first_line.trim_end().to_string()],
            "the agent's own line is the one to read, not the main conversation's"
        );
        assert_eq!(first.path.as_deref(), Some(agent_transcript.as_path()));
        assert_eq!(first.offset, first_line.len() as u64);
        assert!(!first.restarted);

        let second_line = "{\"type\":\"assistant\",\"isSidechain\":true,\"uuid\":\"agent-2\"}\n";
        std::fs::write(
            &agent_transcript,
            format!("{first_line}{second_line}").as_bytes(),
        )?;

        let second = tail_from(TailState {
            path: first.path,
            offset: first.offset,
            pending: first.pending,
        })?;
        assert_eq!(
            second.lines,
            vec![second_line.trim_end().to_string()],
            "only the appended line belongs to this read"
        );
        assert!(!second.restarted);

        // A shorter file at the same path is a different conversation, and the caller has
        // to be told that what it holds is stale.
        std::fs::write(&agent_transcript, b"{\"uuid\":\"agent-9\"}\n")?;
        let third = tail_from(TailState {
            path: second.path,
            offset: second.offset,
            pending: second.pending,
        })?;
        assert!(
            third.restarted,
            "a file of {} bytes read from offset {} must report a restart",
            std::fs::metadata(&agent_transcript)?.len(),
            second.offset
        );
        assert_eq!(third.lines, vec!["{\"uuid\":\"agent-9\"}".to_string()]);

        Ok(())
    }
}

#[cfg(test)]
mod pane_key_tests {
    use super::*;

    /// The key name is what crosses the connection between the panel and the machine the
    /// session runs on: the panel writes it with `tmux_name` and the other end rebuilds
    /// the key with `from_tmux_name`. A variant either side does not agree on is a
    /// button that silently does nothing.
    #[test]
    fn test_every_pane_key_survives_the_name_it_crosses_a_connection_as() {
        let keys = [
            PaneKey::Up,
            PaneKey::Down,
            PaneKey::Enter,
            PaneKey::Choice(Digit::One),
            PaneKey::Choice(Digit::Nine),
            PaneKey::CyclePermissionMode,
            PaneKey::Cancel,
            PaneKey::Quit,
        ];

        for key in keys {
            assert_eq!(
                PaneKey::from_tmux_name(key.tmux_name()),
                Some(key),
                "{:?} did not survive being written as {:?}",
                key,
                key.tmux_name()
            );
        }
    }

    /// The set is closed on purpose: a name that reached this from a registration, or
    /// from a connection, must not become a keystroke of its own choosing.
    #[test]
    fn test_a_name_that_is_not_one_of_the_keys_is_refused() {
        for name in ["C-z", "kill", "BTab ", "btab", "Enter Enter", "", "C-C"] {
            assert_eq!(
                PaneKey::from_tmux_name(name),
                None,
                "{name:?} was accepted as a key to send"
            );
        }
    }
}
