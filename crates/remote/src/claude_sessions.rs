//! The parts of Claude Code session tracking that have to run on the machine the
//! sessions live on, whether that is this one or the far end of a remote connection.
//!
//! Everything here is either a pure function over text a registration or a transcript
//! contains, or the thinnest possible wrapper around the one piece of IO it needs. The
//! liveness rules in particular must exist exactly once: a second copy of them drifts
//! from this one and the two answers cannot both be right.

use std::{
    fs,
    io::{BufRead as _, BufReader, Read as _, Seek as _, SeekFrom, Write as _},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail};
use collections::{HashMap, HashSet};
use futures::future::try_join;
use gpui::BackgroundExecutor;
use serde::Deserialize;
use smol::io::AsyncReadExt as _;
use util::ResultExt as _;

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

/// One row of `claude agents --json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentListing {
    pub id: Option<String>,
    pub kind: String,
    pub state: Option<String>,
    pub status: Option<String>,
    pub waiting_for: Option<String>,
    pub process_id: Option<u32>,
    pub session_id: Option<String>,
    pub name: Option<String>,
    pub working_directory: Option<PathBuf>,
    pub started_at: Option<String>,
}

/// Parses the JSON array `claude agents --json` prints. Unknown fields are ignored,
/// missing optional fields stay `None`, and a non-array is an error.
pub fn parse_claude_agents_json(contents: &str) -> Result<Vec<AgentListing>> {
    anyhow::ensure!(
        contents.len() <= MAX_COMMAND_OUTPUT_BYTES,
        "claude agents --json is {} bytes, and the limit is {MAX_COMMAND_OUTPUT_BYTES}",
        contents.len()
    );
    let value: serde_json::Value =
        serde_json::from_str(contents).with_context(|| "parsing claude agents --json")?;
    let Some(array) = value.as_array() else {
        bail!("claude agents --json did not print an array");
    };
    Ok(array.iter().filter_map(agent_listing_from_json).collect())
}

fn agent_listing_from_json(value: &serde_json::Value) -> Option<AgentListing> {
    let kind = value.get("kind")?.as_str()?.to_string();
    Some(AgentListing {
        id: json_opt_string(value, "id"),
        kind,
        state: json_opt_string(value, "state"),
        status: json_opt_string(value, "status"),
        waiting_for: json_opt_string(value, "waitingFor"),
        process_id: value
            .get("pid")
            .and_then(serde_json::Value::as_u64)
            .and_then(|process_id| u32::try_from(process_id).ok()),
        session_id: json_opt_string(value, "sessionId"),
        name: json_opt_string(value, "name"),
        working_directory: json_opt_string(value, "cwd").map(PathBuf::from),
        started_at: json_opt_string(value, "startedAt"),
    })
}

fn json_opt_string(value: &serde_json::Value, key: &str) -> Option<String> {
    match value.get(key)? {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

pub const RESUME_SESSION_TIMEOUT: Duration = Duration::from_secs(30);
pub const LIST_AGENTS_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs `command`, refusing when it has not finished within `timeout`.
///
/// The child is spawned when this function is entered rather than when the wait is first
/// polled, so a bound that only stops waiting would leave the process running with
/// nothing left watching it, and every retry would leave another. `kill_on_drop` is what
/// makes dropping the future end the process instead. Stdout and stderr are read up to
/// [`MAX_COMMAND_OUTPUT_BYTES`] each; past that the child is dropped (and so killed)
/// rather than held as a growing buffer.
async fn output_within(
    command: &mut smol::process::Command,
    what: &str,
    timeout: Duration,
    executor: &BackgroundExecutor,
) -> Result<std::process::Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(anyhow::Error::from)?;
    let stdout = child.stdout.take().context("stdout pipe")?;
    let stderr = child.stderr.take().context("stderr pipe")?;
    let collect = async {
        let (stdout, stderr) = try_join(
            read_capped_output(stdout, MAX_COMMAND_OUTPUT_BYTES),
            read_capped_output(stderr, MAX_COMMAND_OUTPUT_BYTES),
        )
        .await?;
        let status = child.status().await.map_err(anyhow::Error::from)?;
        Ok(std::process::Output {
            status,
            stdout,
            stderr,
        })
    };
    let timeout_wait = async {
        executor.timer(timeout).await;
        bail!("{what} has not answered in {} s", timeout.as_secs())
    };
    smol::future::or(Box::pin(collect), Box::pin(timeout_wait)).await
}

async fn read_capped_output(
    mut reader: impl smol::io::AsyncRead + Unpin,
    cap: usize,
) -> Result<Vec<u8>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Ok(buffer);
        }
        anyhow::ensure!(
            buffer.len().saturating_add(read) <= cap,
            "the command printed more than {cap} bytes"
        );
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// Runs `claude` with `args` in `working_directory`, which must be an absolute
/// existing directory with no NUL.
pub async fn run_claude_command(
    args: &[String],
    working_directory: &Path,
    timeout: Duration,
    executor: &BackgroundExecutor,
) -> Result<String> {
    anyhow::ensure!(
        claude_command_working_directory(working_directory).is_some(),
        "{} is not an absolute existing directory",
        working_directory.display()
    );
    let mut command = smol::process::Command::new("claude");
    command.args(args).current_dir(working_directory);
    let output = output_within(
        &mut command,
        &format!("claude {}", args.join(" ")),
        timeout,
        executor,
    )
    .await
    .with_context(|| "running claude")?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "claude {} exited with {}: {stderr}",
            args.join(" "),
            output.status
        );
    }
    Ok(stdout)
}

/// `claude --bg --resume <session_id>` in `cwd`. `session_id` must be a single path component.
pub async fn resume_session_in_background(
    session_id: &str,
    cwd: &Path,
    executor: &BackgroundExecutor,
) -> Result<String> {
    let session_id = claude_command_operand(session_id)
        .with_context(|| "session_id is not a value this may hand to claude")?;
    run_claude_command(
        &[
            "--bg".to_string(),
            "--resume".to_string(),
            session_id.to_string(),
        ],
        cwd,
        RESUME_SESSION_TIMEOUT,
        executor,
    )
    .await
}

/// `claude respawn <id>` or `claude stop <id>`. The id must be a single path component.
pub async fn run_claude_agent_command(
    args: &[String],
    cwd: &Path,
    executor: &BackgroundExecutor,
) -> Result<String> {
    anyhow::ensure!(
        args.len() == 2 && (args[0] == "respawn" || args[0] == "stop"),
        "expected respawn or stop and an id"
    );
    claude_command_operand(&args[1])
        .with_context(|| "the id is not a value this may hand to claude")?;
    run_claude_command(args, cwd, RESUME_SESSION_TIMEOUT, executor).await
}

pub async fn list_claude_agents(executor: &BackgroundExecutor) -> Result<Vec<AgentListing>> {
    let mut command = smol::process::Command::new("claude");
    command.args(["agents", "--json"]);
    let output = output_within(
        &mut command,
        "claude agents --json",
        LIST_AGENTS_TIMEOUT,
        executor,
    )
    .await
    .with_context(|| "running claude agents --json")?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "claude agents --json exited with {}: {stderr}",
            output.status
        );
    }
    parse_claude_agents_json(&stdout)
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

#[cfg(unix)]
pub fn liveness_unavailable_reason() -> Option<&'static str> {
    None
}

#[cfg(not(unix))]
pub fn liveness_unavailable_reason() -> Option<&'static str> {
    Some("Claude session liveness checks are unavailable on this host.")
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
pub fn single_path_component(name: &str) -> Option<&str> {
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

/// Longer than any id Claude Code writes — a session id is a 36 character uuid — and
/// short enough that nothing of substance can be smuggled through as one.
const MAX_COMMAND_OPERAND_BYTES: usize = 64;

/// Status files in this module refuse to load more than 1 MiB; a spawned `claude` is held
/// to the channel-message cap so a host that prints without bound cannot grow the process
/// with it. Larger than any agents listing or resume line Claude Code writes.
pub const MAX_COMMAND_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

/// The directory `claude` may be started in: an absolute path that already exists as a
/// directory and that does not contain a NUL. A relative path is the process's, not the
/// session's, and on a remote project the path is the far end's answer.
pub fn claude_command_working_directory(path: &Path) -> Option<&Path> {
    if !path.is_absolute() || path.as_os_str().as_encoded_bytes().contains(&0) {
        return None;
    }
    path.is_dir().then_some(path)
}

/// The value an id may take when it is handed to `claude` as an operand, or to a shell
/// as one word of a command line.
///
/// Stricter than [`single_path_component`], which is all a file name has to satisfy: an
/// id also has to be unable to be read as an option or to close the word it sits in, and
/// on a remote project it is the far end that chose it. Every id Claude Code writes — the
/// short one an agents listing gives, and a session id — is alphanumeric with hyphens, so
/// refusing the rest turns nothing legal away.
pub fn claude_command_operand(value: &str) -> Option<&str> {
    let value = single_path_component(value)?;
    let is_plain =
        |character: char| character.is_ascii_alphanumeric() || character == '-' || character == '_';
    (value.len() <= MAX_COMMAND_OPERAND_BYTES
        && !value.starts_with('-')
        && value.chars().all(is_plain))
    .then_some(value)
}

/// The name a session's transcript file has, or `None` when the session id could name
/// something other than a file inside one project's directory.
fn transcript_file_name(session_id: &str) -> Option<String> {
    // Appending a suffix to a lone normal component cannot introduce a component of its
    // own, so the composed name stays inside one project's directory.
    Some(format!("{}.jsonl", single_path_component(session_id)?))
}

/// The directories a session's scratch files live in, which are not under its working
/// directory and are named as absolutely as the working directory is. `/private/tmp` is
/// the same directory as `/tmp` on macOS, and it is the spelling the paths are recorded
/// with there, so both are listed rather than resolved.
const TEMPORARY_DIRECTORIES: [&str; 2] = ["/tmp", "/private/tmp"];

/// The directory a session was started in, as its own registration records it, or `None`
/// when no registration names that session.
///
/// This is the same value the panel lists the session under, and it is read here rather
/// than accepted from the caller because it is what a request to read a file is judged
/// against: a working directory the requester supplies is no boundary at all.
pub fn session_working_directory(home_directory: &Path, session_id: &str) -> Option<PathBuf> {
    let registry_directory = home_directory.join(".claude").join("sessions");
    read_registrations(&registry_directory)
        .log_err()?
        .into_iter()
        .find(|registration| registration.session_id == session_id)
        .map(|registration| registration.working_directory)
}

/// Whether `path` stays inside `directory`: absolute, no `..`, and a prefix of `directory`.
///
/// The same shape [`attachment_is_readable`] uses, without the `/tmp` exception — a
/// listing or slash-command walk is not an attachment.
pub fn path_stays_inside(path: &Path, directory: &Path) -> bool {
    if !path.is_absolute()
        || !directory.is_absolute()
        || directory.parent().is_none()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return false;
    }
    path.starts_with(directory)
}

/// The directory `list_session_files` may walk for `session_id`: the session's working
/// directory, or a sub-path of it named by `requested`.
pub fn session_files_directory(
    home_directory: &Path,
    session_id: &str,
    requested: &Path,
) -> Result<PathBuf> {
    let working_directory = session_working_directory(home_directory, session_id)
        .with_context(|| format!("no session is registered as {session_id}"))?;
    let directory = if requested.as_os_str().is_empty() {
        working_directory.clone()
    } else if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        working_directory.join(requested)
    };
    anyhow::ensure!(
        path_stays_inside(&directory, &working_directory),
        "{} is outside the working directory of session {session_id}",
        directory.display(),
    );
    Ok(directory)
}

/// Whether `project_root` is the named session's working directory or inside it.
///
/// When `session_id` is absent, any live registration whose working directory contains
/// `project_root` is enough: slash commands are listed for the project, not a pid.
pub fn slash_command_project_root(
    home_directory: &Path,
    session_id: Option<&str>,
    project_root: Option<&Path>,
) -> Result<Option<PathBuf>> {
    let Some(project_root) = project_root else {
        return Ok(None);
    };
    let working_directory = match session_id {
        Some(session_id) => session_working_directory(home_directory, session_id)
            .with_context(|| format!("no session is registered as {session_id}"))?,
        None => {
            let registry_directory = home_directory.join(".claude").join("sessions");
            read_registrations(&registry_directory)?
                .into_iter()
                .map(|registration| registration.working_directory)
                .find(|working_directory| path_stays_inside(project_root, working_directory))
                .with_context(|| {
                    format!(
                        "{} is not inside any session's working directory",
                        project_root.display()
                    )
                })?
        }
    };
    anyhow::ensure!(
        path_stays_inside(project_root, &working_directory),
        "{} is outside the working directory of the session",
        project_root.display(),
    );
    Ok(Some(project_root.to_path_buf()))
}

/// Whether a file a session recorded as a `SendUserFile` attachment may be read back.
///
/// The attachment sits wherever the session wrote it — under the project it is working
/// on, or in the temporary directory its scratch files go to — so the boundary that
/// guards offloaded tool outputs, `~/.claude/projects`, refuses every one of them. The
/// boundary here is the session's own working directory instead, which is the same
/// subtree the session can already read and write on its user's behalf, plus the
/// temporary directories.
///
/// Requiring an absolute path with no parent component ("..") is what makes the prefix
/// check mean what it says: without it, a path under the working directory could climb
/// out of it and name a key file.
pub fn attachment_is_readable(path: &Path, working_directory: &Path) -> bool {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return false;
    }

    // A working directory that is the filesystem root is not a boundary, and a relative
    // one cannot be compared against an absolute path at all.
    let working_directory_bounds =
        working_directory.is_absolute() && working_directory.parent().is_some();

    (working_directory_bounds && path.starts_with(working_directory))
        || TEMPORARY_DIRECTORIES
            .iter()
            .any(|directory| path.starts_with(directory))
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
        cap_pending_buffer(&mut state.pending);
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

/// Follows `~/.claude/zed-events/<session_id>.jsonl`. No file yet is progress with no
/// lines and path None, the same answer a transcript that has not been written yet gives.
pub fn read_events_tail(
    home_directory: &Path,
    session_id: &str,
    mut state: TailState,
) -> Result<TailProgress> {
    let start_offset = state.offset;
    let Some(path) = events_file_path(home_directory, session_id) else {
        return Ok(tail_of_no_file(&state));
    };
    if !path.is_file() {
        return Ok(tail_of_no_file(&state));
    }

    // A remote tail does not send the path back: the file is always this one, named by
    // session id. Restart only when a previously followed path disagrees, not when the
    // path is still unknown.
    let mut restarted = false;
    if let Some(followed) = state.path.as_ref()
        && followed != &path
    {
        restarted = state.offset > 0 || !state.pending.is_empty();
        state.offset = 0;
        state.pending.clear();
    }

    read_appended_lines(path, state, start_offset, restarted)
}

/// The ceiling on the statusLine snapshot, which is read whole on every poll and framed
/// onto the wire behind it. The CLI's own status JSON is a couple of kilobytes, so
/// anything past this is not a status line and is refused rather than carried.
const MAX_STATUS_FILE_BYTES: u64 = 1024 * 1024;

pub fn read_session_status(home_directory: &Path, session_id: &str) -> Result<Option<String>> {
    let Some(path) = status_file_path(home_directory, session_id) else {
        return Ok(None);
    };
    match fs::metadata(&path) {
        Ok(metadata) => anyhow::ensure!(
            metadata.len() <= MAX_STATUS_FILE_BYTES,
            "{} is {} bytes, past the {} a status line may be",
            path.display(),
            metadata.len(),
            MAX_STATUS_FILE_BYTES,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("reading metadata of {}", path.display()));
        }
    }

    match fs::read_to_string(&path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn events_file_path(home_directory: &Path, session_id: &str) -> Option<PathBuf> {
    let session_id = single_path_component(session_id)?;
    Some(
        home_directory
            .join(".claude")
            .join(EVENTS_DIRECTORY)
            .join(format!("{session_id}.jsonl")),
    )
}

fn status_file_path(home_directory: &Path, session_id: &str) -> Option<PathBuf> {
    let session_id = single_path_component(session_id)?;
    Some(
        home_directory
            .join(".claude")
            .join(STATUS_DIRECTORY)
            .join(format!("{session_id}.json")),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelStatus {
    pub live: bool,
    pub heartbeat_at_ms: Option<i64>,
    pub server_pid: Option<u32>,
    pub features: Vec<String>,
}

fn channel_root(home_directory: &Path) -> PathBuf {
    home_directory.join(".claude").join(CHANNEL_DIRECTORY)
}

pub fn channel_server_path(home_directory: &Path) -> PathBuf {
    channel_root(home_directory).join(CHANNEL_SERVER_FILE)
}

fn channel_session_directory(home_directory: &Path, claude_pid: u32) -> Result<PathBuf> {
    ensure_nonzero_claude_pid(claude_pid)?;
    Ok(channel_root(home_directory).join(claude_pid.to_string()))
}

fn ensure_nonzero_claude_pid(claude_pid: u32) -> Result<()> {
    if claude_pid == 0 {
        bail!("claude_pid must not be 0");
    }
    Ok(())
}

#[derive(Deserialize)]
struct ChannelServerFile {
    pid: Option<u32>,
    heartbeat_at_ms: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_channel_features")]
    features: Vec<String>,
}

/// Missing → empty (serde default). Null, a string, or any non-array → empty so a
/// live heartbeat is not thrown away with the rest of server.json. Array elements
/// that are not strings are skipped so `["interrupt", 1]` still advertises interrupt.
fn deserialize_channel_features<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(serde_json::Value::Array(items)) => items
            .into_iter()
            .filter_map(|item| match item {
                serde_json::Value::String(feature) => Some(feature),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    })
}

/// Whether the Zed channel server for `claude_pid` is live: `server.json` exists as a
/// regular file (never a symlink) and its heartbeat is no older than 20 seconds.
pub fn channel_status(home: &Path, claude_pid: u32, now_ms: i64) -> ChannelStatus {
    let absent = ChannelStatus {
        live: false,
        heartbeat_at_ms: None,
        server_pid: None,
        features: Vec::new(),
    };
    if claude_pid == 0 {
        return absent;
    }
    let Ok(session_directory) = channel_session_directory(home, claude_pid) else {
        return absent;
    };
    let path = session_directory.join("server.json");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(_) => return absent,
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return absent;
    }
    let Ok(contents) = fs::read_to_string(&path) else {
        return absent;
    };
    let Ok(file) = serde_json::from_str::<ChannelServerFile>(&contents) else {
        return absent;
    };
    let heartbeat_at_ms = file.heartbeat_at_ms;
    let live = heartbeat_at_ms.is_some_and(|heartbeat_at_ms| {
        now_ms.saturating_sub(heartbeat_at_ms) <= CHANNEL_HEARTBEAT_STALE_MS
    });
    ChannelStatus {
        live,
        heartbeat_at_ms,
        server_pid: file.pid,
        features: file.features,
    }
}

static CHANNEL_OUTBOX_SEQUENCE: AtomicU32 = AtomicU32::new(0);

fn next_outbox_sequence() -> u32 {
    CHANNEL_OUTBOX_SEQUENCE.fetch_add(1, Ordering::Relaxed) % CHANNEL_OUTBOX_SEQ_MODULUS
}

/// Publishes one outbox payload under a name the server reads in lexical order.
///
/// The sequence that makes those names unique is this process's own and starts over in
/// every Zed, so a second Zed sending to the same session in the same millisecond reaches
/// for the very same name. A name something already holds is left alone and the next one
/// taken instead, and the temporary file is named after this process, so neither writer
/// renames over the other's message or writes through the other's temporary file. What is
/// left is the instant between the two, which nothing this side of the protocol can close:
/// only the server, which deletes a name within a poll of it appearing, ever sees both.
fn publish_outbox_file(
    outbox: &Path,
    now_ms: i64,
    encoded: &[u8],
    next_sequence: &mut dyn FnMut() -> u32,
) -> Result<String> {
    for _ in 0..CHANNEL_OUTBOX_NAME_ATTEMPTS {
        let name = format!("{now_ms:013}-{:04}.json", next_sequence());
        let destination = outbox.join(&name);
        if fs::symlink_metadata(&destination).is_ok() {
            continue;
        }
        let temporary = outbox.join(format!("{name}.{}.tmp", std::process::id()));
        fs::write(&temporary, encoded)
            .with_context(|| format!("writing {}", temporary.display()))?;
        fs::rename(&temporary, &destination)
            .with_context(|| format!("publishing {}", destination.display()))?;
        return Ok(name);
    }
    bail!(
        "no free channel outbox name in {} after {CHANNEL_OUTBOX_NAME_ATTEMPTS} tries",
        outbox.display()
    );
}

fn write_outbox_json(home: &Path, claude_pid: u32, payload: &serde_json::Value) -> Result<String> {
    let session_directory = channel_session_directory(home, claude_pid)?;
    let outbox = session_directory.join("outbox");
    fs::create_dir_all(&outbox).with_context(|| format!("creating {}", outbox.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&session_directory, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("locking down {}", session_directory.display()))?;
    }

    let encoded = serde_json::to_vec(payload).context("encoding a channel outbox payload")?;
    if encoded.len() > CHANNEL_MESSAGE_MAX_BYTES {
        bail!("channel outbox file exceeds 4 MiB");
    }
    publish_outbox_file(&outbox, now_millis(), &encoded, &mut next_outbox_sequence)
}

pub fn channel_send_message(home: &Path, claude_pid: u32, content: &str) -> Result<String> {
    ensure_nonzero_claude_pid(claude_pid)?;
    if content.trim().is_empty() {
        bail!("message content must not be empty");
    }
    if content.len() > CHANNEL_MESSAGE_MAX_BYTES {
        bail!("message content exceeds 4 MiB");
    }
    write_outbox_json(
        home,
        claude_pid,
        &serde_json::json!({
            "kind": "message",
            "content": content,
            "meta": { "from": "zed" },
        }),
    )
}

/// How long an interrupt reason may be. Counted in Unicode scalar values, matching the
/// bound the channel server applies before it will send SIGINT.
pub const CHANNEL_INTERRUPT_REASON_MAX_CHARS: usize = 200;

pub fn channel_interrupt(home: &Path, claude_pid: u32, reason: &str) -> Result<String> {
    ensure_nonzero_claude_pid(claude_pid)?;
    if reason.chars().count() > CHANNEL_INTERRUPT_REASON_MAX_CHARS {
        bail!("interrupt reason exceeds {CHANNEL_INTERRUPT_REASON_MAX_CHARS} characters");
    }
    write_outbox_json(
        home,
        claude_pid,
        &serde_json::json!({
            "kind": "interrupt",
            "reason": reason,
        }),
    )
}

fn request_id_is_valid(request_id: &str) -> bool {
    let length = request_id.len();
    (1..=16).contains(&length)
        && request_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

pub fn channel_answer_permission(
    home: &Path,
    claude_pid: u32,
    request_id: &str,
    allow: bool,
) -> Result<String> {
    ensure_nonzero_claude_pid(claude_pid)?;
    if !request_id_is_valid(request_id) {
        bail!("invalid permission request_id");
    }
    let behavior = if allow { "allow" } else { "deny" };
    write_outbox_json(
        home,
        claude_pid,
        &serde_json::json!({
            "kind": "permission",
            "request_id": request_id,
            "behavior": behavior,
        }),
    )
}

/// Follows `~/.claude/zed-channel/<claude_pid>/inbox.jsonl`. Same semantics as
/// [`read_events_tail`]: no file is empty progress, a shrink restarts.
pub fn read_channel_inbox_tail(
    home: &Path,
    claude_pid: u32,
    mut state: TailState,
) -> Result<TailProgress> {
    ensure_nonzero_claude_pid(claude_pid)?;
    let start_offset = state.offset;
    let path = channel_session_directory(home, claude_pid)?.join("inbox.jsonl");
    if !path.is_file() {
        return Ok(tail_of_no_file(&state));
    }

    let mut restarted = false;
    if let Some(followed) = state.path.as_ref()
        && followed != &path
    {
        restarted = state.offset > 0 || !state.pending.is_empty();
        state.offset = 0;
        state.pending.clear();
    }

    read_appended_lines(path, state, start_offset, restarted)
}

pub fn channel_setup_commands(home: &Path) -> String {
    let server = channel_server_path(home);
    format!(
        "claude mcp add --scope user zed-claude -- node '{}'\nexport CLAUDE_EXTRA_ARGS='--dangerously-load-development-channels server:zed-claude'",
        server.display()
    )
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
/// How large the unfinished line of a tail may grow before it is dropped. A line that
/// never ends would otherwise be held forever on both sides of the wire.
pub const TAIL_PENDING_CAP_BYTES: usize = 4 * 1024 * 1024;

static PENDING_CAP_LOGGED: AtomicBool = AtomicBool::new(false);

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
    cap_pending_buffer(buffer);
    lines
}

/// Drops a partial line that has grown past [`TAIL_PENDING_CAP_BYTES`]. The next newline
/// is what resynchronises the tail; nothing of the dropped bytes is emitted.
fn cap_pending_buffer(buffer: &mut Vec<u8>) {
    if buffer.len() <= TAIL_PENDING_CAP_BYTES {
        return;
    }
    if !PENDING_CAP_LOGGED.swap(true, Ordering::Relaxed) {
        log::warn!(
            "dropping a tailed line past {TAIL_PENDING_CAP_BYTES} bytes; resyncing at the next newline"
        );
    }
    buffer.clear();
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
const WORKFLOW_JOURNAL_STARTED_TYPE: &str = "started";
const EVENTS_DIRECTORY: &str = "zed-events";
const STATUS_DIRECTORY: &str = "zed-status";
const CHANNEL_DIRECTORY: &str = "zed-channel";
const CHANNEL_SERVER_FILE: &str = "server.mjs";
/// How old a heartbeat may be and still count as live. The same bound dates the age
/// the panel draws beside it.
pub const CHANNEL_HEARTBEAT_STALE_MS: i64 = 20_000;
pub const CHANNEL_MESSAGE_MAX_BYTES: usize = 4 * 1024 * 1024;
const CHANNEL_OUTBOX_SEQ_MODULUS: u32 = 10_000;
/// How many names one send may reach for before giving up. The name space for one
/// millisecond is [`CHANNEL_OUTBOX_SEQ_MODULUS`] wide; stopping earlier would refuse a
/// send while free names of that millisecond were still unused.
const CHANNEL_OUTBOX_NAME_ATTEMPTS: usize = CHANNEL_OUTBOX_SEQ_MODULUS as usize;
const EVENTS_HOOK_SCRIPT: &str = "hooks/zed-claude-events.sh";
const STATUS_HOOK_SCRIPT: &str = "hooks/zed-claude-status.sh";
const CHAINED_STATUS_COMMAND_FILE: &str = "chained-command.txt";
const LEGACY_QUESTION_HOOK_SCRIPT: &str = "hooks/record-pending-question.sh";
const LEGACY_LIVE_MESSAGE_HOOK_SCRIPT: &str = "hooks/record-live-message.sh";
const LEGACY_QUESTION_HOOK_NAME: &str = "record-pending-question.sh";
const LEGACY_LIVE_MESSAGE_HOOK_NAME: &str = "record-live-message.sh";
const HOOK_EVENTS: [&str; 13] = [
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "PermissionDenied",
    "Notification",
    "Stop",
    "SubagentStop",
    "PreCompact",
    "PostCompact",
    "PostModelSwitch",
    "MessageDisplay",
    "SessionEnd",
];

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
    /// `background` for an agent whose `Task` call is answered the moment it is launched
    /// rather than when it returns; see [`SubagentSummary::task_agent_finished`]. Absent
    /// on an agent whose sidecar does not record one, which is most of them.
    #[serde(rename = "requestShape", default)]
    pub request_shape: Option<String>,
}

/// The `requestShape` of an agent that is launched and left to work, whose call is
/// answered with `Async agent launched successfully` rather than with what it found.
pub const BACKGROUND_REQUEST_SHAPE: &str = "background";

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
    /// Whether this `Task` agent has returned, read out of its own session's
    /// conversation. `None` for an agent this scan could not answer for: one that belongs
    /// to a workflow run, one that records no `toolUseId`, or one whose session's
    /// conversation could not be read.
    ///
    /// What says an agent has returned depends on how it was spawned, which is why this
    /// is answered here rather than by the caller. An agent run in the foreground returns
    /// with the `tool_result` answering its call. One launched in the background —
    /// `requestShape: background` — has that call answered the moment it launches, the
    /// same way a `Workflow` call is, and what says it is over is the task notification
    /// its session records when it stops.
    ///
    /// The caller could not answer either question for most of what it draws in any case:
    /// it follows one session's conversation and draws every listed session's agents.
    pub task_agent_finished: Option<bool>,
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

/// Slash commands that open a terminal dialog. Sent as text only when they take an
/// argument (`/mcp …`); otherwise the panel tells the reader to use the terminal.
pub const CHANNEL_DIALOG_SLASH_COMMANDS: &[&str] = &[
    "permissions",
    "login",
    "logout",
    "resume",
    "agents",
    "plugin",
    "hooks",
    "memory",
    "theme",
    "export",
    "bug",
    "vim",
    "ide",
    "doctor",
    "mcp",
];

/// Slash commands that are sent as text only when an argument is present.
pub const CHANNEL_ARGUMENT_SLASH_COMMANDS: &[&str] = &[
    "model",
    "effort",
    "config",
    "autocompact",
    "output-style",
    "advisor",
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
        let claude = project_root.join(".claude");
        read_slash_commands_in(
            &claude.join("commands"),
            SlashCommandScope::Project,
            &mut commands,
        );
        read_skills_in(
            &claude.join("skills"),
            SlashCommandScope::Project,
            &mut commands,
        );
    }
    let claude = home_directory.join(".claude");
    read_slash_commands_in(
        &claude.join("commands"),
        SlashCommandScope::User,
        &mut commands,
    );
    read_skills_in(
        &claude.join("skills"),
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

/// How deep the walk for `@` goes under a session's working directory.
///
/// Deep enough for the source tree of a real project, and bounded so that a symlink
/// pointing at an ancestor cannot be walked forever.
const MENTION_DIRECTORY_DEPTH: usize = 8;

/// How many paths are reported. The menu shows a handful; the rest would be scrolled
/// past, and counting them costs the whole tree.
const MENTION_MATCH_LIMIT: usize = 50;

/// How many entries are looked at before the walk gives up, whether or not it has found
/// anything. A repository with a checked-in dependency tree is millions of files, and a
/// keystroke must not cost a walk of it.
const MENTION_VISIT_LIMIT: usize = 20_000;

/// Directories that are never what someone means by `@`, and are the ones large enough
/// to spend the whole visit budget. `.`-prefixed entries are skipped separately.
const UNMENTIONED_DIRECTORIES: [&str; 8] = [
    "node_modules",
    "target",
    "dist",
    "build",
    "vendor",
    "__pycache__",
    ".git",
    "Pods",
];

/// The paths under `directory` that `query` names, as the `@` menu offers them.
///
/// Matched on the path rather than on the file name alone, so that `src/claude` finds a
/// file by where it is as well as by what it is called. Paths come back relative to
/// `directory`, which is what a reader recognises and what the session resolves.
pub fn list_files_under(directory: &Path, query: &str) -> Vec<String> {
    let query = query.to_lowercase();
    let mut matches = Vec::new();
    let mut visited = 0usize;
    walk_for_mentions(
        directory,
        directory,
        &query,
        MENTION_DIRECTORY_DEPTH,
        &mut visited,
        &mut matches,
    );

    // Shortest first: the file itself sorts above the ones buried under it, and a walk
    // in directory order is in no order a reader would expect.
    matches.sort_by(|left: &String, right: &String| {
        (left.len(), left.as_str()).cmp(&(right.len(), right.as_str()))
    });
    matches.truncate(MENTION_MATCH_LIMIT);
    matches
}

fn walk_for_mentions(
    root: &Path,
    directory: &Path,
    query: &str,
    depth_left: usize,
    visited: &mut usize,
    matches: &mut Vec<String>,
) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };

    for entry in entries {
        if *visited >= MENTION_VISIT_LIMIT {
            return;
        }
        *visited += 1;

        let Some(entry) = entry.log_err() else {
            continue;
        };
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') || UNMENTIONED_DIRECTORIES.contains(&name.as_str()) {
            continue;
        }

        let path = entry.path();
        // `is_dir` follows symlinks, which is what makes the depth the only thing
        // standing between this and a cycle.
        if path.is_dir() {
            let Some(depth_left) = depth_left.checked_sub(1) else {
                continue;
            };
            walk_for_mentions(root, &path, query, depth_left, visited, matches);
            continue;
        }

        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let Some(relative) = relative.to_str() else {
            continue;
        };
        if query.is_empty() || relative.to_lowercase().contains(query) {
            matches.push(relative.to_string());
        }
    }
}

/// Where a file pasted into the message box is put on the machine the session runs on.
///
/// Under the user's own `.claude` rather than in the project: a pasted screenshot is not
/// part of anybody's repository, and a session whose working directory is read-only
/// still has somewhere to put one.
const PASTED_FILE_DIRECTORY: &str = "zed-pasted";

const PASTED_FILE_KEEP: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Larger than any screenshot, and small enough that a paste cannot fill a disk.
const MAX_PASTED_FILE_BYTES: usize = 32 * 1024 * 1024;

/// Writes `contents` where the session can read it, and reports the path to give it.
///
/// The name is taken apart rather than trusted: it crosses a connection, and a session
/// is not a place to write a path of someone else's choosing.
pub fn write_pasted_file(home_directory: &Path, name: &str, contents: &[u8]) -> Result<String> {
    anyhow::ensure!(
        contents.len() <= MAX_PASTED_FILE_BYTES,
        "the file is {} bytes, and the limit is {MAX_PASTED_FILE_BYTES}",
        contents.len()
    );

    let name = Path::new(name)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && !name.starts_with('.'))
        .context("the file needs a name of its own")?;

    let directory = home_directory.join(".claude").join(PASTED_FILE_DIRECTORY);
    fs::create_dir_all(&directory).with_context(|| format!("creating {}", directory.display()))?;

    let directory_metadata = fs::symlink_metadata(&directory)
        .with_context(|| format!("reading metadata for {}", directory.display()))?;
    anyhow::ensure!(
        directory_metadata.is_dir() && !directory_metadata.file_type().is_symlink(),
        "{} must be a real directory",
        directory.display()
    );
    // Housekeeping, not part of the paste: a directory this user cannot read or unlink
    // from is a reason to leave the old files alone, not a reason to lose the new one.
    if let Err(error) = prune_pasted_files(&directory, now_millis()) {
        log::warn!("pruning {}: {error:#}", directory.display());
    }

    let path = directory.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "{} must be a regular file",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("writing {}", path.display()))?;

    path.to_str()
        .map(str::to_string)
        .context("the path this was written to cannot be spelled for the session")
}

fn prune_pasted_files(directory: &Path, now_ms: i64) -> Result<()> {
    let cutoff_ms =
        now_ms.saturating_sub(i64::try_from(PASTED_FILE_KEEP.as_millis()).unwrap_or(i64::MAX));
    let entries =
        fs::read_dir(directory).with_context(|| format!("reading {}", directory.display()))?;
    // One entry is never allowed to decide the fate of the others, or of the paste that
    // asked for this: a second window removing a file between this `read_dir` and the
    // look at it is ordinary, and a file left by another user is not this one's to fix.
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                log::warn!("reading an entry in {}: {error}", directory.display());
                continue;
            }
        };
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                log::warn!("reading metadata for {}: {error}", path.display());
                continue;
            }
        };
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        let Some(written_at_ms) = pasted_file_timestamp_ms(&entry.file_name()) else {
            continue;
        };
        if written_at_ms < cutoff_ms
            && let Err(error) = fs::remove_file(&path)
        {
            log::warn!("removing {}: {error}", path.display());
        }
    }
    Ok(())
}

fn pasted_file_timestamp_ms(name: &std::ffi::OsStr) -> Option<i64> {
    let name = name.to_str()?.strip_prefix("pasted-")?;
    let timestamp = name
        .split_once('-')
        .map(|(timestamp, _)| timestamp)
        .or_else(|| name.split_once('.').map(|(timestamp, _)| timestamp))?;
    timestamp.parse().ok()
}

/// The file a skill's directory is a skill by virtue of holding.
const SKILL_FILE: &str = "SKILL.md";

/// Appends every skill under `directory`.
///
/// A skill is invoked by typing its name after a slash exactly as a command in
/// `commands/` is, so a menu that lists only `commands/` is missing most of what a
/// session answers to. The name is the directory's, which is what the CLI resolves —
/// the `name` in the front matter is what the skill calls itself, and the two can differ.
fn read_skills_in(directory: &Path, scope: SlashCommandScope, commands: &mut Vec<SlashCommand>) {
    let Ok(entries) = fs::read_dir(directory) else {
        // A machine with no skills is the ordinary case, not a failure.
        return;
    };

    for entry in entries {
        let Some(entry) = entry.log_err() else {
            continue;
        };
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }

        // A directory with no `SKILL.md` in it is not a skill, and a name read off the
        // directory alone would put a command in the menu that does not exist.
        let path = entry.path().join(SKILL_FILE);
        let Some(contents) = fs::read_to_string(&path).ok() else {
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

/// Appends every command under `directory`, including those in directories of their own,
/// which the CLI names `<directory>:<command>`.
fn read_slash_commands_in(
    directory: &Path,
    scope: SlashCommandScope,
    commands: &mut Vec<SlashCommand>,
) {
    read_slash_commands_under(directory, scope, None, commands);
}

fn read_slash_commands_under(
    directory: &Path,
    scope: SlashCommandScope,
    namespace: Option<&str>,
    commands: &mut Vec<SlashCommand>,
) {
    read_slash_commands_to_depth(directory, scope, namespace, commands)
}

fn read_slash_commands_to_depth(
    directory: &Path,
    scope: SlashCommandScope,
    namespace: Option<&str>,
    commands: &mut Vec<SlashCommand>,
) {
    let mut pending = vec![(
        directory.to_path_buf(),
        namespace.map(str::to_string),
        HashSet::default(),
    )];

    while let Some((directory, namespace, mut ancestors)) = pending.pop() {
        let Ok(identity) = fs::canonicalize(&directory) else {
            continue;
        };
        if !ancestors.insert(identity) {
            continue;
        }
        let Ok(entries) = fs::read_dir(&directory) else {
            // A machine with no commands of its own is the ordinary case, not a failure.
            continue;
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
                let nested = match &namespace {
                    Some(namespace) => format!("{namespace}:{file_name}"),
                    None => file_name,
                };
                pending.push((path, Some(nested), ancestors.clone()));
                continue;
            }

            let Some(stem) = file_name.strip_suffix(".md") else {
                continue;
            };
            let name = match &namespace {
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

/// Appends every Claude Code hook event to `~/.claude/zed-events/<session_id>.jsonl`.
///
/// Nothing is printed and the exit status is always 0: this hook rules on nothing.
const EVENTS_HOOK_SOURCE: &str = r#"#!/bin/sh
# Written by Zed. Appends every hook event so a reader outside the terminal can
# follow the session without reading the screen.
payload=$(cat)
hook_directory=$(CDPATH= cd "$(dirname "$0")" 2>/dev/null && pwd) || exit 0
case "$hook_directory" in
  */.claude/hooks) claude_directory=${hook_directory%/hooks} ;;
  *) [ -n "${HOME:-}" ] || exit 0; claude_directory="$HOME/.claude" ;;
esac
session=$(printf '%s' "$payload" | python3 -c 'import json,sys; found=json.load(sys.stdin).get("session_id"); print(found if isinstance(found, str) else "")' 2>/dev/null)
if [ -z "$session" ]; then
  session=$(printf '%s' "$payload" | tr -d '\n' | sed -n 's/.*"session_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
fi
[ -n "$session" ] || exit 0
case "$session" in */*|.|..) exit 0 ;; esac
payload=$(printf '%s' "$payload" | tr -d '\n\r')
directory="$claude_directory/zed-events"
mkdir -p "$directory" || exit 0
file="$directory/$session.jsonl"
if [ -f "$file" ]; then
  size=$(wc -c < "$file" 2>/dev/null || echo 0)
  [ "$size" -le 8388608 ] || : > "$file" 2>/dev/null
fi
received_at_ms=$(python3 -c 'import time;print(int(time.time()*1000))' 2>/dev/null)
if [ -n "$received_at_ms" ]; then have_python=yes; else have_python=; received_at_ms=$(date +%s)000; fi
line="{\"received_at_ms\":${received_at_ms},\"event\":${payload}}"
# Appended in one write(2) where that is possible. A shell redirect writes a long
# payload in several chunks, and hooks are not serialized with one another: parallel
# tool calls answer their PostToolUse together, so two chunked appends interleave and
# destroy both lines.
if [ -n "$have_python" ]; then
  printf '%s\n' "$line" | python3 -c 'import os,sys
data = sys.stdin.buffer.read()
handle = os.open(sys.argv[1], os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
while data:
    data = data[os.write(handle, data):]' "$file" 2>/dev/null
else
  printf '%s\n' "$line" >> "$file" 2>/dev/null
fi
exit 0
"#;

/// Writes the CLI's status JSON for a session, and chains the user's own statusLine
/// command when one was displaced.
const STATUS_HOOK_SOURCE: &str = r#"#!/bin/sh
# Written by Zed. Records the CLI's status JSON and optionally chains the user's
# own statusLine command so its stdout still reaches the CLI.
payload=$(cat)
hook_directory=$(CDPATH= cd "$(dirname "$0")" 2>/dev/null && pwd) || exit 0
case "$hook_directory" in
  */.claude/hooks) claude_directory=${hook_directory%/hooks} ;;
  *) [ -n "${HOME:-}" ] || exit 0; claude_directory="$HOME/.claude" ;;
esac
session=$(printf '%s' "$payload" | python3 -c 'import json,sys; found=json.load(sys.stdin).get("session_id"); print(found if isinstance(found, str) else "")' 2>/dev/null)
if [ -z "$session" ]; then
  session=$(printf '%s' "$payload" | tr -d '\n' | sed -n 's/.*"session_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
fi
directory="$claude_directory/zed-status"
if [ -n "$session" ]; then
  case "$session" in */*|.|..) session= ;; esac
fi
if [ -n "$session" ]; then
  mkdir -p "$directory" || exit 0
  file="$directory/$session.json"
  printf '%s' "$payload" > "$file.tmp" 2>/dev/null || exit 0
  mv "$file.tmp" "$file" 2>/dev/null || exit 0
fi
chained="$claude_directory/zed-status/chained-command.txt"
if [ -f "$chained" ]; then
  command=$(cat "$chained" 2>/dev/null)
  if [ -n "$command" ]; then
    printf '%s' "$payload" | sh -c "$command" || true
  fi
fi
exit 0
"#;

const CHANNEL_SERVER_SOURCE: &str = include_str!("../assets/zed-claude-channel/server.mjs");

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookInstallOutcome {
    AlreadyCurrent,
    ScriptsRefreshed,
    Installed { backup_path: Option<PathBuf> },
}

fn shell_quote(argument: &str) -> String {
    format!("'{}'", argument.replace('\'', "'\\''"))
}

fn claude_directory(home_directory: &Path) -> PathBuf {
    home_directory.join(".claude")
}

fn settings_path(home_directory: &Path) -> PathBuf {
    claude_directory(home_directory).join("settings.json")
}

fn settings_backup_path(home_directory: &Path) -> PathBuf {
    claude_directory(home_directory).join("settings.json.zed-backup")
}

fn events_hook_path(home_directory: &Path) -> PathBuf {
    claude_directory(home_directory).join(EVENTS_HOOK_SCRIPT)
}

fn status_hook_path(home_directory: &Path) -> PathBuf {
    claude_directory(home_directory).join(STATUS_HOOK_SCRIPT)
}

fn command_refers_to(command: &str, script_name: &str) -> bool {
    command.contains(script_name)
}

fn read_claude_settings(home_directory: &Path) -> Result<serde_json::Value> {
    let path = settings_path(home_directory);
    match fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents)
            .with_context(|| format!("{} could not be parsed", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_script_if_changed(path: &Path, source: &str) -> Result<bool> {
    let current = fs::read_to_string(path).ok();
    if current.as_deref() == Some(source) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(path, source).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("making {} executable", path.display()))?;
    }
    Ok(true)
}

fn write_regular_file_if_changed(
    path: &Path,
    source: &str,
    #[cfg_attr(not(unix), allow(unused_variables))] unix_mode: u32,
) -> Result<bool> {
    let current = fs::read_to_string(path).ok();
    if current.as_deref() == Some(source) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(path, source).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(unix_mode))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
    }
    Ok(true)
}

fn remove_path_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => match fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(inner) if inner.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(error).with_context(|| format!("removing {}", path.display())),
        },
    }
}

fn hook_entries_mut<'a>(
    settings: &'a mut serde_json::Value,
    event: &str,
) -> Result<&'a mut Vec<serde_json::Value>> {
    let settings_object = settings
        .as_object_mut()
        .context("settings.json does not hold a JSON object")?;
    let hooks = settings_object
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("`hooks` does not hold a JSON object")?;
    hooks
        .entry(event.to_string())
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .with_context(|| format!("`hooks.{event}` does not hold a JSON array"))
}

fn entry_command_strings(entry: &serde_json::Value) -> Vec<String> {
    entry
        .get("hooks")
        .and_then(serde_json::Value::as_array)
        .map(|hooks| {
            hooks
                .iter()
                .filter_map(|hook| {
                    hook.get("command")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn remove_legacy_hook_entries(settings: &mut serde_json::Value) -> Result<bool> {
    let Some(hooks) = settings
        .get_mut("hooks")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return Ok(false);
    };
    let mut changed = false;
    for event in hooks.values_mut() {
        let Some(entries) = event.as_array_mut() else {
            continue;
        };
        let before = entries.len();
        entries.retain(|entry| {
            !entry_command_strings(entry).iter().any(|command| {
                command_refers_to(command, LEGACY_QUESTION_HOOK_NAME)
                    || command_refers_to(command, LEGACY_LIVE_MESSAGE_HOOK_NAME)
            })
        });
        changed |= entries.len() != before;
    }
    Ok(changed)
}

fn ensure_events_hook_entries(
    settings: &mut serde_json::Value,
    events_command: &str,
) -> Result<bool> {
    let mut changed = false;
    for event in HOOK_EVENTS {
        let entries = hook_entries_mut(settings, event)?;
        let mut ours: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry_command_strings(entry)
                    .iter()
                    .any(|command| command_refers_to(command, "zed-claude-events.sh"))
            })
            .map(|(index, _)| index)
            .collect();

        let canonical = serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": events_command,
                "timeout": 5
            }]
        });

        if ours.is_empty() {
            entries.push(canonical);
            changed = true;
            continue;
        }

        let keep = ours.remove(0);
        if entries.get(keep) != Some(&canonical) {
            entries[keep] = canonical;
            changed = true;
        }
        for index in ours.into_iter().rev() {
            entries.remove(index);
            changed = true;
        }
    }
    Ok(changed)
}

fn event_names_our_script(settings: &serde_json::Value, script_name: &str) -> bool {
    HOOK_EVENTS.iter().all(|event| {
        settings
            .get("hooks")
            .and_then(|hooks| hooks.get(*event))
            .and_then(serde_json::Value::as_array)
            .is_some_and(|entries| {
                entries.iter().any(|entry| {
                    entry_command_strings(entry)
                        .iter()
                        .any(|command| command_refers_to(command, script_name))
                })
            })
    })
}

fn status_line_command(settings: &serde_json::Value) -> Option<&str> {
    settings
        .get("statusLine")
        .and_then(|status_line| status_line.get("command"))
        .and_then(serde_json::Value::as_str)
}

fn ensure_status_line(
    settings: &mut serde_json::Value,
    home_directory: &Path,
    status_command: &str,
) -> Result<bool> {
    let chained_path = claude_directory(home_directory)
        .join(STATUS_DIRECTORY)
        .join(CHAINED_STATUS_COMMAND_FILE);

    match status_line_command(settings) {
        None => {
            let settings_object = settings
                .as_object_mut()
                .context("settings.json does not hold a JSON object")?;
            settings_object.insert(
                "statusLine".to_string(),
                serde_json::json!({
                    "type": "command",
                    "command": status_command,
                }),
            );
            remove_path_if_present(&chained_path)?;
            Ok(true)
        }
        Some(existing) if command_refers_to(existing, "zed-claude-status.sh") => Ok(false),
        Some(existing) => {
            let existing = existing.to_string();
            if let Some(parent) = chained_path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            fs::write(&chained_path, existing)
                .with_context(|| format!("writing {}", chained_path.display()))?;
            let settings_object = settings
                .as_object_mut()
                .context("settings.json does not hold a JSON object")?;
            settings_object.insert(
                "statusLine".to_string(),
                serde_json::json!({
                    "type": "command",
                    "command": status_command,
                }),
            );
            Ok(true)
        }
    }
}

fn restore_or_remove_status_line(
    settings: &mut serde_json::Value,
    home_directory: &Path,
) -> Result<bool> {
    let chained_path = claude_directory(home_directory)
        .join(STATUS_DIRECTORY)
        .join(CHAINED_STATUS_COMMAND_FILE);
    match fs::read_to_string(&chained_path) {
        Ok(command) => {
            let command = command.trim().to_string();
            let names_ours = status_line_command(settings)
                .is_some_and(|current| command_refers_to(current, "zed-claude-status.sh"));
            if names_ours {
                let settings_object = settings
                    .as_object_mut()
                    .context("settings.json does not hold a JSON object")?;
                if command.is_empty() {
                    settings_object.remove("statusLine");
                } else {
                    let status_line = settings_object
                        .entry("statusLine")
                        .or_insert_with(|| serde_json::json!({"type": "command"}));
                    if let Some(status_object) = status_line.as_object_mut() {
                        status_object.insert("command".to_string(), serde_json::json!(command));
                    }
                }
            }
            remove_path_if_present(&chained_path)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if status_line_command(settings)
                .is_some_and(|command| command_refers_to(command, "zed-claude-status.sh"))
            {
                settings
                    .as_object_mut()
                    .context("settings.json does not hold a JSON object")?
                    .remove("statusLine");
                Ok(true)
            } else {
                Ok(false)
            }
        }
        Err(error) => Err(error).with_context(|| format!("reading {}", chained_path.display())),
    }
}

fn remove_our_hook_entries(settings: &mut serde_json::Value) -> Result<bool> {
    let Some(hooks) = settings
        .get_mut("hooks")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return Ok(false);
    };
    let mut changed = false;
    for event in HOOK_EVENTS {
        let Some(entries) = hooks
            .get_mut(event)
            .and_then(serde_json::Value::as_array_mut)
        else {
            continue;
        };
        let mut index = 0;
        while index < entries.len() {
            let should_remove_matcher = {
                let Some(entry) = entries.get_mut(index) else {
                    break;
                };
                match entry
                    .get_mut("hooks")
                    .and_then(serde_json::Value::as_array_mut)
                {
                    Some(hook_objects) => {
                        let hook_count = hook_objects.len();
                        hook_objects.retain(|hook| {
                            !hook
                                .get("command")
                                .and_then(serde_json::Value::as_str)
                                .is_some_and(|command| {
                                    command_refers_to(command, "zed-claude-events.sh")
                                })
                        });
                        if hook_objects.len() != hook_count {
                            changed = true;
                            hook_objects.is_empty()
                        } else {
                            false
                        }
                    }
                    None => false,
                }
            };
            if should_remove_matcher {
                entries.remove(index);
            } else {
                index += 1;
            }
        }
    }
    Ok(changed)
}

fn scripts_are_current(home_directory: &Path) -> bool {
    let events = events_hook_path(home_directory);
    let status = status_hook_path(home_directory);
    let server = channel_server_path(home_directory);
    fs::read_to_string(&events).is_ok_and(|installed| installed == EVENTS_HOOK_SOURCE)
        && fs::read_to_string(&status).is_ok_and(|installed| installed == STATUS_HOOK_SOURCE)
        && fs::read_to_string(&server).is_ok_and(|installed| installed == CHANNEL_SERVER_SOURCE)
}

/// Whether the hooks Zed installs are in place on this machine.
pub fn zed_hooks_installed(home_directory: &Path) -> bool {
    if !scripts_are_current(home_directory) {
        return false;
    }
    let Ok(settings) = read_claude_settings(home_directory) else {
        return false;
    };
    event_names_our_script(&settings, "zed-claude-events.sh")
        && status_line_command(&settings)
            .is_some_and(|command| command_refers_to(command, "zed-claude-status.sh"))
}

/// Installs the event dispatcher and statusLine wrapper. Settings are parsed before
/// anything is written; installing twice changes nothing.
pub fn install_zed_hooks(home_directory: &Path) -> Result<HookInstallOutcome> {
    let mut settings = read_claude_settings(home_directory)?;
    let original = settings.clone();
    let events_path = events_hook_path(home_directory);
    let status_path = status_hook_path(home_directory);
    let events_command = shell_quote(events_path.to_string_lossy().as_ref());
    let status_command = shell_quote(status_path.to_string_lossy().as_ref());

    let mut scripts_changed = write_script_if_changed(&events_path, EVENTS_HOOK_SOURCE)?;
    scripts_changed |= write_script_if_changed(&status_path, STATUS_HOOK_SOURCE)?;
    scripts_changed |= write_regular_file_if_changed(
        &channel_server_path(home_directory),
        CHANNEL_SERVER_SOURCE,
        0o644,
    )?;

    let claude_directory = claude_directory(home_directory);
    remove_path_if_present(&claude_directory.join(LEGACY_QUESTION_HOOK_SCRIPT))?;
    remove_path_if_present(&claude_directory.join(LEGACY_LIVE_MESSAGE_HOOK_SCRIPT))?;
    remove_path_if_present(&claude_directory.join("pending-questions"))?;
    remove_path_if_present(&claude_directory.join("live-messages"))?;

    let mut settings_changed = remove_legacy_hook_entries(&mut settings)?;
    settings_changed |= ensure_events_hook_entries(&mut settings, &events_command)?;
    settings_changed |= ensure_status_line(&mut settings, home_directory, &status_command)?;
    settings_changed |= settings != original;

    if !settings_changed {
        return Ok(if scripts_changed {
            HookInstallOutcome::ScriptsRefreshed
        } else {
            HookInstallOutcome::AlreadyCurrent
        });
    }

    let settings_file = settings_path(home_directory);
    let backup_path = if settings_file.is_file() {
        let backup = settings_backup_path(home_directory);
        fs::copy(&settings_file, &backup)
            .with_context(|| format!("copying {} aside", settings_file.display()))?;
        Some(backup)
    } else {
        None
    };

    if let Some(parent) = settings_file.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(&settings_file, format!("{:#}\n", settings))
        .with_context(|| format!("writing {}", settings_file.display()))?;

    Ok(HookInstallOutcome::Installed { backup_path })
}

pub fn uninstall_zed_hooks(home_directory: &Path) -> Result<()> {
    let settings_file = settings_path(home_directory);
    let mut settings = match fs::read_to_string(&settings_file) {
        Ok(contents) => serde_json::from_str(&contents)
            .with_context(|| format!("{} could not be parsed", settings_file.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", settings_file.display()));
        }
    };
    let original = settings.clone();

    remove_our_hook_entries(&mut settings)?;
    restore_or_remove_status_line(&mut settings, home_directory)?;
    remove_path_if_present(&events_hook_path(home_directory))?;
    remove_path_if_present(&status_hook_path(home_directory))?;
    remove_path_if_present(&channel_server_path(home_directory))?;

    if settings != original {
        if let Some(parent) = settings_file.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::write(&settings_file, format!("{:#}\n", settings))
            .with_context(|| format!("writing {}", settings_file.display()))?;
    }

    Ok(())
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
    /// Where each agent's `started` entry sits in the journal, which is the order the run
    /// actually started them in. Agent ids are hashes, so nothing else recovers it.
    started_at: HashMap<String, usize>,
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
        let mut started_at = HashMap::default();
        for line in contents.lines() {
            let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(agent_id) = entry.get("agentId").and_then(serde_json::Value::as_str) else {
                continue;
            };
            match entry.get("type").and_then(serde_json::Value::as_str) {
                Some(WORKFLOW_JOURNAL_RESULT_TYPE) => {
                    returned.insert(agent_id.to_string());
                }
                // A run that resumes writes a second `started` for an agent it is
                // replaying, and the first is the one that placed it among the others.
                Some(WORKFLOW_JOURNAL_STARTED_TYPE) => {
                    let next_position = started_at.len();
                    started_at
                        .entry(agent_id.to_string())
                        .or_insert(next_position);
                }
                _ => continue,
            }
        }

        Some(Self {
            returned,
            started_at,
        })
    }

    fn has_returned(&self, agent_id: &str) -> bool {
        self.returned.contains(agent_id)
    }

    /// Where the run started this agent, or `None` for one the journal does not name yet —
    /// its sidecar is written before the journal records it starting, so this is the
    /// ordinary state of an agent in the moments after it is spawned.
    fn started_at(&self, agent_id: &str) -> Option<usize> {
        self.started_at.get(agent_id).copied()
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
    let mut subagents = Vec::new();
    read_subagents_for_session(&session_directory, &mut subagents)?;
    resolve_task_agents(&session_transcript_path(&session_directory), &mut subagents);
    sort_subagents(&mut subagents);

    Ok(subagents)
}

/// The subagents of several sessions at once, keyed by session id.
///
/// Walks `<home>/.claude/projects` once and matches directory names against the ids
/// asked for, rather than calling [`list_subagents`] per session: that function searches
/// the whole projects directory for each id, which the panel's poll would repeat for
/// every session on screen every second.
///
/// Every id asked for is present in the result, mapping to an empty list when that
/// session has no directory or no agents, so a caller never has to tell "scanned and
/// found none" from "not scanned".
pub async fn list_subagents_for_sessions(
    home_directory: &Path,
    session_ids: &[String],
) -> Result<HashMap<String, Vec<SubagentSummary>>> {
    let mut subagents_by_session: HashMap<String, Vec<SubagentSummary>> = session_ids
        .iter()
        .cloned()
        .map(|session_id| (session_id, Vec::new()))
        .collect();
    let requested_session_ids: HashSet<&str> = session_ids
        .iter()
        .filter_map(|session_id| single_path_component(session_id))
        .collect();
    if requested_session_ids.is_empty() {
        return Ok(subagents_by_session);
    }

    let projects_directory = home_directory.join(".claude").join("projects");
    let project_entries = match fs::read_dir(&projects_directory) {
        Ok(project_entries) => project_entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(subagents_by_session);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", projects_directory.display()));
        }
    };

    for project_entry in project_entries {
        let Some(project_entry) = project_entry.log_err() else {
            continue;
        };
        let project_directory = project_entry.path();
        if !project_directory.is_dir() {
            continue;
        }
        // One project directory that cannot be read must not hide the sessions every
        // other project holds, so the failure is logged and that project alone is skipped.
        let Some(session_entries) = fs::read_dir(&project_directory)
            .with_context(|| format!("reading {}", project_directory.display()))
            .log_err()
        else {
            continue;
        };
        for session_entry in session_entries {
            let Some(session_entry) = session_entry.log_err() else {
                continue;
            };
            if !session_entry.path().is_dir() {
                continue;
            }
            let Ok(session_id) = session_entry.file_name().into_string() else {
                continue;
            };
            if !requested_session_ids.contains(session_id.as_str()) {
                continue;
            }
            if let Some(subagents) = subagents_by_session.get_mut(&session_id) {
                // Same isolation as an unreadable project directory: one session whose
                // `subagents` path cannot be listed must not hide every other session
                // this walk already found, or has yet to find.
                read_subagents_for_session(&session_entry.path(), subagents).log_err();
                resolve_task_agents(&session_transcript_path(&session_entry.path()), subagents);
            }
        }
    }

    for subagents in subagents_by_session.values_mut() {
        sort_subagents(subagents);
    }

    prune_read_conversations();

    Ok(subagents_by_session)
}

/// Whether each `Task` agent's call has been answered in the session's own conversation.
///
/// A `Task` agent's transcript ends when it stops writing and records nothing about
/// having returned, and its `meta.json` is written when it is spawned and never touched
/// again. The `tool_result` answering the call that spawned it is the only record of it
/// being over, and that lives in the session's own transcript, beside the directory this
/// scan just walked.
///
/// A call answered anywhere in the file counts, rather than only along the conversation's
/// active path: compaction severs the chain and a rewind abandons a branch, and neither
/// un-runs the agent that call started.
fn resolve_task_agents(transcript_path: &Path, subagents: &mut [SubagentSummary]) {
    // Most sessions spawn no `Task` agent at all, and their conversation — which is the
    // largest file either side of this scan touches — is then never opened.
    let any_task_agents = subagents
        .iter()
        .any(|summary| summary.workflow_run_id.is_none() && summary.meta.tool_use_id.is_some());
    if !any_task_agents {
        return;
    }

    let Some(conversation) = read_session_conversation(transcript_path).log_err() else {
        return;
    };
    for summary in subagents.iter_mut() {
        if summary.workflow_run_id.is_some() {
            continue;
        }
        let Some(tool_use_id) = summary.meta.tool_use_id.as_deref() else {
            continue;
        };
        summary.task_agent_finished = Some(
            if summary.meta.request_shape.as_deref() == Some(BACKGROUND_REQUEST_SHAPE) {
                conversation.notified_agents.contains(&summary.agent_id)
            } else {
                conversation.answered_calls.contains(tool_use_id)
            },
        );
    }
}

/// What one read of a session's conversation found about the agents it spawned.
#[derive(Clone, Debug, Default)]
struct SessionConversation {
    /// The ids of the tool calls it holds an answer to.
    answered_calls: HashSet<String>,
    /// The agents a task notification in it names. A notification is written each time a
    /// background agent stops, so an agent named here is one that has stopped at least
    /// once — which is as close to "has returned" as this file gets, because the user can
    /// send such an agent another message and start it again.
    notified_agents: HashSet<String>,
}

#[derive(Clone)]
struct CachedConversation {
    offset_scanned: u64,
    length: u64,
    modified: Option<SystemTime>,
    conversation: SessionConversation,
}

/// What the last read of a conversation found, kept so that the poll behind this does not
/// read a multi-megabyte file every second for a session nothing has appended to.
///
/// Keyed by path. On growth, only the appended bytes are folded in. On shrink or replace
/// the entry is rebuilt. Entries whose session was not listed in the last scan are dropped.
static READ_CONVERSATIONS: Mutex<Option<HashMap<PathBuf, CachedConversation>>> = Mutex::new(None);
static SCANNED_CONVERSATION_PATHS: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

fn note_conversation_path(path: &Path) {
    if let Ok(mut paths) = SCANNED_CONVERSATION_PATHS.lock() {
        paths
            .get_or_insert_with(HashSet::default)
            .insert(path.to_path_buf());
    }
}

fn prune_read_conversations() {
    let keep = match SCANNED_CONVERSATION_PATHS.lock() {
        Ok(mut paths) => paths.take().unwrap_or_default(),
        Err(_) => return,
    };
    if let Ok(mut cache) = READ_CONVERSATIONS.lock()
        && let Some(cache) = cache.as_mut()
    {
        cache.retain(|path, _| keep.contains(path));
    }
}

/// Reads the conversation at `transcript_path` for what it says about its own agents.
fn read_session_conversation(transcript_path: &Path) -> Result<SessionConversation> {
    note_conversation_path(transcript_path);
    let metadata = fs::metadata(transcript_path)
        .with_context(|| format!("reading {}", transcript_path.display()))?;
    let length = metadata.len();
    let modified = metadata.modified().ok();

    if let Ok(cache) = READ_CONVERSATIONS.lock()
        && let Some(cache) = cache.as_ref()
        && let Some(cached) = cache.get(transcript_path)
        && cached.length == length
        && cached.modified == modified
    {
        return Ok(cached.conversation.clone());
    }

    let cached = READ_CONVERSATIONS
        .lock()
        .ok()
        .and_then(|cache| cache.as_ref()?.get(transcript_path).cloned());

    let (mut conversation, mut offset_scanned) = match cached {
        Some(cached) if length >= cached.length && cached.offset_scanned <= cached.length => {
            (cached.conversation, cached.offset_scanned)
        }
        _ => (SessionConversation::default(), 0),
    };

    let mut file = fs::File::open(transcript_path)
        .with_context(|| format!("reading {}", transcript_path.display()))?;
    if offset_scanned > 0 {
        file.seek(SeekFrom::Start(offset_scanned))
            .with_context(|| format!("seeking in {}", transcript_path.display()))?;
    }
    let scanned = absorb_conversation_lines(&mut file, &mut conversation)?;
    offset_scanned = offset_scanned.saturating_add(scanned);

    if let Ok(mut cache) = READ_CONVERSATIONS.lock() {
        cache.get_or_insert_with(HashMap::default).insert(
            transcript_path.to_path_buf(),
            CachedConversation {
                offset_scanned,
                length,
                modified,
                conversation: conversation.clone(),
            },
        );
    }
    Ok(conversation)
}

/// Absorbs every whole line from where the file is positioned, reporting how many bytes
/// those lines took.
///
/// Only lines that ended are counted, because a scan can land in the middle of a line the
/// session is still writing: were those bytes counted as read, the next scan would begin
/// inside the line, neither half would parse, and the record they carry would be lost for
/// good. Undecodable bytes are replaced rather than refused for the same reason — stopping
/// at them would hide every record after them behind an offset that never goes back.
fn absorb_conversation_lines(
    file: &mut fs::File,
    conversation: &mut SessionConversation,
) -> Result<u64> {
    let mut reader = BufReader::new(file);
    let mut scanned = 0u64;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .with_context(|| "reading a line of a session's conversation")?;
        if read == 0 || !line.ends_with(b"\n") {
            break;
        }
        scanned = scanned.saturating_add(read as u64);
        let text = String::from_utf8_lossy(&line);
        absorb_conversation_line(text.trim_end_matches(['\n', '\r']), conversation);
    }
    Ok(scanned)
}

fn absorb_conversation_line(line: &str, conversation: &mut SessionConversation) {
    read_task_notification_ids(line, &mut conversation.notified_agents);
    if !line.contains(TOOL_RESULT_BLOCK_TYPE) {
        return;
    }
    let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    let Some(blocks) = record
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(serde_json::Value::as_array)
    else {
        return;
    };
    for block in blocks {
        if block.get("type").and_then(serde_json::Value::as_str) != Some(TOOL_RESULT_BLOCK_TYPE) {
            continue;
        }
        if let Some(tool_use_id) = block.get("tool_use_id").and_then(serde_json::Value::as_str) {
            conversation.answered_calls.insert(tool_use_id.to_string());
        }
    }
}

/// Every agent a task notification in `line` names.
fn read_task_notification_ids(line: &str, into: &mut HashSet<String>) {
    let mut rest = line;
    while let Some(start) = rest.find(TASK_ID_OPENING_TAG) {
        rest = &rest[start + TASK_ID_OPENING_TAG.len()..];
        let Some(end) = rest.find(TASK_ID_CLOSING_TAG) else {
            return;
        };
        into.insert(rest[..end].to_string());
        rest = &rest[end..];
    }
}

const TOOL_RESULT_BLOCK_TYPE: &str = "tool_result";
const TASK_ID_OPENING_TAG: &str = "<task-id>";
const TASK_ID_CLOSING_TAG: &str = "</task-id>";

/// A session's own conversation, which sits beside the directory holding its agents and
/// is named after it.
fn session_transcript_path(session_directory: &Path) -> PathBuf {
    // Built by appending rather than by `with_extension`, which would cut a directory
    // name at its last dot instead of adding to it.
    let mut file_name = session_directory.as_os_str().to_os_string();
    file_name.push(".jsonl");
    PathBuf::from(file_name)
}

fn read_subagents_for_session(
    session_directory: &Path,
    subagents: &mut Vec<SubagentSummary>,
) -> Result<()> {
    let subagents_directory = session_directory.join(SUBAGENTS_DIRECTORY);
    read_subagents_in(&subagents_directory, None, subagents)?;

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
                read_subagents_in(&directory_entry.path(), Some(workflow_run_id), subagents)?;
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

    Ok(())
}

/// Gathers each run's agents together without disturbing the order they were read in.
///
/// Only the run id is compared: `sort_by` is stable, so the order [`read_subagents_in`]
/// left each group in — the journal's for a run, the agent id's for the session's own
/// agents — survives. Sorting by agent id here as well would undo it, and an agent id is
/// a hash that says nothing about when its agent started.
fn sort_subagents(subagents: &mut [SubagentSummary]) {
    subagents.sort_by(|left, right| left.workflow_run_id.cmp(&right.workflow_run_id));
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
    // What this call appends is ordered on its own at the end: the caller passes the same
    // vector for every run, and one run's order is no business of another's.
    let appended_from = subagents.len();

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
            // Filled in once the whole session has been read; see
            // [`resolve_task_agents`], which needs the session's conversation rather
            // than anything in this directory.
            task_agent_finished: None,
        });
    }

    // Directory enumeration is in no particular order, so what this call appended is put
    // in one before it is handed back. A run is ordered by its journal, because that is
    // the order the reader watched the agents start in and the only record of it; an
    // agent the journal does not name yet goes after the ones it does, by id so that two
    // such agents do not swap places between polls. A session's own agents have no
    // journal, so the id is all there is to order them by.
    subagents[appended_from..].sort_by(|left, right| {
        let position_of = |summary: &SubagentSummary| {
            journal
                .as_ref()
                .and_then(|journal| journal.started_at(&summary.agent_id))
        };
        match (position_of(left), position_of(right)) {
            (Some(left_position), Some(right_position)) => left_position.cmp(&right_position),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => left.agent_id.cmp(&right.agent_id),
        }
    });

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

/// tmux ends a command and starts the next one at any argument that ends in a semicolon,
/// and a session name is allowed to end in one (`tmux rename-session 'done\;'` makes such
/// a name). Passed as it stands, `=done;` would cut the mirror's command list in two and
/// leave `-s <mirror> …` to be read as a command of its own, which tmux rejects with
/// `unknown command: -s` and then abandons the rest of the list — no mirror, and no
/// terminal for the reader. A backslash in front of that last semicolon is how tmux is
/// told it belongs to the name. Only the last one matters: a semicolon anywhere else in
/// the argument is an ordinary character to tmux.
fn with_any_trailing_semicolon_escaped(session_name: &str) -> String {
    match session_name.strip_suffix(';') {
        Some(rest) => format!("{rest}\\;"),
        None => session_name.to_string(),
    }
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
        format!("={}", with_any_trailing_semicolon_escaped(session_name)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    const EVENTS_FILE_CAP_BYTES: u64 = 8 * 1024 * 1024;

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

    /// A tmux session name is free to end in a semicolon — `tmux rename-session 'done\;'`
    /// makes one — and tmux splits an argument list at any argument that ends in one. The
    /// name unescaped turns this list into `new-session -d -t =done` followed by a second
    /// command starting `-s`, which tmux rejects with `unknown command: -s` and abandons
    /// the rest of the list, leaving the reader with no terminal at all.
    #[test]
    fn a_session_name_ending_in_a_semicolon_does_not_end_the_command_list() {
        let arguments = mirror_arguments("done;", "@6", "zed-claude-mirror-1-0");
        assert_eq!(
            arguments.get(3).map(String::as_str),
            Some("=done\\;"),
            "the trailing semicolon has to reach tmux escaped, or it separates commands"
        );
    }

    /// The escape must only touch a name that ends in a semicolon: one anywhere else is a
    /// legal name tmux takes as it stands (`tmux rename-session 'wo;rk'` keeps it whole),
    /// and escaping it would group the terminal with the wrong session or with none.
    #[test]
    fn a_semicolon_anywhere_but_the_end_of_a_name_is_left_alone() {
        for (session_name, expected) in [
            ("zed", "=zed"),
            ("my work", "=my work"),
            ("wo;rk", "=wo;rk"),
            (";lead", "=;lead"),
            ("", "="),
        ] {
            let arguments = mirror_arguments(session_name, "@6", "zed-claude-mirror-1-0");
            assert_eq!(
                arguments.get(3).map(String::as_str),
                Some(expected),
                "`{session_name}` reaches tmux as it stands"
            );
        }
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

    fn write_channel_server_json(home: &Path, claude_pid: u32, contents: &str) -> PathBuf {
        let path = home
            .join(".claude")
            .join("zed-channel")
            .join(claude_pid.to_string())
            .join("server.json");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("creating the channel session directory");
        }
        fs::write(&path, contents).expect("writing server.json");
        path
    }

    #[test]
    fn channel_status_is_live_stale_missing_or_a_symlink() {
        let home_directory = temporary_directory("channel-status");
        let now_ms = 1_000_000;
        assert!(
            !channel_status(&home_directory, 0, now_ms).live,
            "pid 0 is never live"
        );
        assert!(
            !channel_status(&home_directory, 42, now_ms).live,
            "a missing server.json is not live"
        );

        write_channel_server_json(
            &home_directory,
            42,
            r#"{"pid":7,"claude_pid":42,"started_at_ms":1,"heartbeat_at_ms":999000,"protocol":1}"#,
        );
        let live = channel_status(&home_directory, 42, now_ms);
        assert!(live.live, "a heartbeat within 20s is live");
        assert_eq!(live.heartbeat_at_ms, Some(999_000));
        assert_eq!(live.server_pid, Some(7));

        let stale = channel_status(&home_directory, 42, now_ms + 21_000);
        assert!(!stale.live, "a heartbeat older than 20s is not live");
        assert_eq!(stale.heartbeat_at_ms, Some(999_000));

        #[cfg(unix)]
        {
            let session_directory = home_directory
                .join(".claude")
                .join("zed-channel")
                .join("43");
            fs::create_dir_all(&session_directory).expect("creating the symlink fixture");
            let target = session_directory.join("real.json");
            fs::write(
                &target,
                r#"{"pid":8,"heartbeat_at_ms":999000,"protocol":1}"#,
            )
            .expect("writing the symlink target");
            std::os::unix::fs::symlink(&target, session_directory.join("server.json"))
                .expect("creating a server.json symlink");
            assert!(
                !channel_status(&home_directory, 43, now_ms).live,
                "a symlink must not be followed"
            );
        }

        fs::remove_dir_all(&home_directory).ok();
    }

    #[test]
    fn channel_status_reads_features_and_defaults_them_to_empty() {
        let home_directory = temporary_directory("channel-status-features");
        let now_ms = 1_000_000;
        write_channel_server_json(
            &home_directory,
            42,
            r#"{"pid":7,"heartbeat_at_ms":999000,"protocol":1}"#,
        );
        assert!(
            channel_status(&home_directory, 42, now_ms)
                .features
                .is_empty(),
            "a server.json with no features field is an empty list, not a missing status"
        );

        write_channel_server_json(
            &home_directory,
            42,
            r#"{"pid":7,"heartbeat_at_ms":999000,"features":["message","permission","interrupt"]}"#,
        );
        assert_eq!(
            channel_status(&home_directory, 42, now_ms).features,
            vec!["message", "permission", "interrupt"]
        );
        fs::remove_dir_all(&home_directory).ok();
    }

    #[test]
    fn channel_status_treats_a_non_array_features_field_as_empty_without_killing_live() {
        let home_directory = temporary_directory("channel-status-features-lenient");
        let now_ms = 1_000_000;
        write_channel_server_json(
            &home_directory,
            42,
            r#"{"pid":7,"heartbeat_at_ms":999000,"protocol":1,"features":null}"#,
        );
        let null_features = channel_status(&home_directory, 42, now_ms);
        assert!(
            null_features.live,
            "a live heartbeat with features:null must stay live, not fail the whole parse; got {null_features:?}"
        );
        assert!(
            null_features.features.is_empty(),
            "features:null must parse as empty, got {:?}",
            null_features.features
        );

        write_channel_server_json(
            &home_directory,
            42,
            r#"{"pid":7,"heartbeat_at_ms":999000,"protocol":1,"features":"interrupt"}"#,
        );
        let string_features = channel_status(&home_directory, 42, now_ms);
        assert!(
            string_features.live,
            "a live heartbeat with features as a string must stay live; got {string_features:?}"
        );
        assert!(
            string_features.features.is_empty(),
            "a non-array features field must parse as empty, not as a single feature, got {:?}",
            string_features.features
        );

        write_channel_server_json(
            &home_directory,
            42,
            r#"{"pid":7,"heartbeat_at_ms":999000,"protocol":1,"features":["interrupt",1]}"#,
        );
        let mixed = channel_status(&home_directory, 42, now_ms);
        assert!(mixed.live, "mixed feature elements must not fail the parse");
        assert_eq!(
            mixed.features,
            vec!["interrupt"],
            "string features in a mixed array must be kept; legal [\"interrupt\"] must not be rejected"
        );
        fs::remove_dir_all(&home_directory).ok();
    }

    #[test]
    fn channel_interrupt_refuses_pid_zero_and_an_overlong_reason() {
        let home_directory = temporary_directory("channel-interrupt-refuse");
        match channel_interrupt(&home_directory, 0, "stop") {
            Ok(name) => panic!("pid 0 must be refused, wrote {name}"),
            Err(error) => assert!(error.to_string().contains("claude_pid"), "got {error:#}"),
        }
        let too_long: String = "x".repeat(CHANNEL_INTERRUPT_REASON_MAX_CHARS + 1);
        match channel_interrupt(&home_directory, 11, &too_long) {
            Ok(name) => panic!("a 201-char reason must be refused, wrote {name}"),
            Err(error) => assert!(error.to_string().contains("200"), "got {error:#}"),
        }
        fs::remove_dir_all(&home_directory).ok();
    }

    #[test]
    fn channel_send_message_refuses_empty_and_oversize_content() {
        let home_directory = temporary_directory("channel-send-refuse");
        match channel_send_message(&home_directory, 0, "hello") {
            Ok(name) => panic!("pid 0 must be refused, wrote {name}"),
            Err(error) => assert!(error.to_string().contains("claude_pid"), "got {error:#}"),
        }
        match channel_send_message(&home_directory, 11, "   ") {
            Ok(name) => panic!("whitespace-only content must be refused, wrote {name}"),
            Err(error) => assert!(error.to_string().contains("empty"), "got {error:#}"),
        }
        let too_large = "x".repeat(CHANNEL_MESSAGE_MAX_BYTES + 1);
        match channel_send_message(&home_directory, 11, &too_large) {
            Ok(name) => panic!("oversize content must be refused, wrote {name}"),
            Err(error) => assert!(error.to_string().contains("4 MiB"), "got {error:#}"),
        }
        fs::remove_dir_all(&home_directory).ok();
    }

    #[test]
    fn channel_outbox_names_order_across_two_sends() -> Result<()> {
        let home_directory = temporary_directory("channel-outbox-order");
        let first = channel_send_message(&home_directory, 12, "one")?;
        let second = channel_send_message(&home_directory, 12, "two")?;
        assert_ne!(first, second);
        assert!(
            first < second,
            "outbox names must sort in send order, got {first} then {second}"
        );
        let outbox = home_directory
            .join(".claude")
            .join("zed-channel")
            .join("12")
            .join("outbox");
        assert!(outbox.join(&first).is_file());
        assert!(outbox.join(&second).is_file());
        assert!(!outbox.join(format!("{first}.tmp")).exists());
        fs::remove_dir_all(&home_directory).ok();
        Ok(())
    }

    /// The sequence that makes outbox names unique is this process's own and starts at
    /// zero in every Zed, so a second Zed on the same machine reaches for the same name
    /// in the same millisecond: it must neither rename over a message that is already
    /// published nor write through a temporary file it does not own.
    #[test]
    fn a_second_writer_neither_takes_a_published_name_nor_shares_its_temporary_file() -> Result<()>
    {
        let home_directory = temporary_directory("channel-outbox-collision");
        let outbox = home_directory
            .join(".claude")
            .join("zed-channel")
            .join("15")
            .join("outbox");
        fs::create_dir_all(&outbox)?;
        let now_ms = 1_700_000_000_000;

        let mine = publish_outbox_file(&outbox, now_ms, b"{\"content\":\"mine\"}", &mut || 7)?;

        let mut theirs_sequence = [7u32, 8].into_iter();
        let theirs =
            match publish_outbox_file(&outbox, now_ms, b"{\"content\":\"theirs\"}", &mut || {
                theirs_sequence.next().unwrap_or(9)
            }) {
                Ok(name) => name,
                Err(error) => panic!("a second writer must find a free name, got error {error:#}"),
            };
        assert_ne!(
            theirs, mine,
            "two messages must not be published under one name; expected a name other \
             than {mine}, got {theirs}"
        );
        let published = fs::read_to_string(outbox.join(&mine))?;
        assert_eq!(
            published, "{\"content\":\"mine\"}",
            "the message already published must survive the second writer; expected \
             {{\"content\":\"mine\"}}, got {published}"
        );

        // The other process is part-way through its own write under the name it took, so
        // the temporary file at the shared path is not this process's to write.
        let theirs_temporary = outbox.join(format!("{now_ms:013}-0009.json.tmp"));
        fs::create_dir_all(&theirs_temporary)?;
        let third =
            match publish_outbox_file(&outbox, now_ms, b"{\"content\":\"third\"}", &mut || 9) {
                Ok(name) => name,
                Err(error) => panic!(
                    "a temporary file another process owns must not stop this one, got error \
                 {error:#}"
                ),
            };
        assert_eq!(
            third,
            format!("{now_ms:013}-0009.json"),
            "the third message must be published under the free name it took"
        );

        fs::remove_dir_all(&home_directory).ok();
        Ok(())
    }

    #[test]
    fn publish_outbox_file_walks_the_whole_name_space_when_the_first_names_are_taken() -> Result<()>
    {
        let home_directory = temporary_directory("channel-outbox-full-window");
        let outbox = home_directory
            .join(".claude")
            .join("zed-channel")
            .join("16")
            .join("outbox");
        fs::create_dir_all(&outbox)?;
        let now_ms = 1_700_000_000_001i64;
        for sequence in 0..16u32 {
            fs::write(
                outbox.join(format!("{now_ms:013}-{sequence:04}.json")),
                b"{}",
            )?;
        }

        let mut sequence = 0u32;
        let published = match publish_outbox_file(
            &outbox,
            now_ms,
            b"{\"content\":\"after-the-occupied-window\"}",
            &mut || {
                let next = sequence;
                sequence += 1;
                next
            },
        ) {
            Ok(name) => name,
            Err(error) => panic!(
                "a free name past the first 16 of this millisecond must still be taken, \
                 got error {error:#}"
            ),
        };
        assert_eq!(
            published,
            format!("{now_ms:013}-0016.json"),
            "expected the 17th name of this millisecond, got {published}"
        );

        fs::remove_dir_all(&home_directory).ok();
        Ok(())
    }

    #[test]
    fn channel_answer_permission_validates_request_id() {
        let home_directory = temporary_directory("channel-answer-id");
        match channel_answer_permission(&home_directory, 13, "", true) {
            Ok(name) => panic!("an empty request_id must be refused, wrote {name}"),
            Err(error) => assert!(error.to_string().contains("request_id"), "got {error:#}"),
        }
        match channel_answer_permission(&home_directory, 13, "ABCDE", true) {
            Ok(name) => panic!("uppercase must be refused, wrote {name}"),
            Err(error) => assert!(error.to_string().contains("request_id"), "got {error:#}"),
        }
        match channel_answer_permission(&home_directory, 13, "abcdefghijklmnopq", false) {
            Ok(name) => panic!("more than 16 chars must be refused, wrote {name}"),
            Err(error) => assert!(error.to_string().contains("request_id"), "got {error:#}"),
        }
        let name = channel_answer_permission(&home_directory, 13, "abcde", true)
            .expect("a 5-letter id is valid");
        let written = fs::read_to_string(
            home_directory
                .join(".claude")
                .join("zed-channel")
                .join("13")
                .join("outbox")
                .join(&name),
        )
        .expect("reading the outbox file");
        assert!(written.contains("\"allow\""), "got {written}");
        assert!(written.contains("\"abcde\""), "got {written}");
        fs::remove_dir_all(&home_directory).ok();
    }

    #[test]
    fn channel_inbox_tail_restarts_when_the_file_shrinks() -> Result<()> {
        let home_directory = temporary_directory("channel-inbox-shrink");
        let inbox = home_directory
            .join(".claude")
            .join("zed-channel")
            .join("14")
            .join("inbox.jsonl");
        fs::create_dir_all(inbox.parent().expect("inbox parent"))?;
        fs::write(
            &inbox,
            "{\"kind\":\"ready\",\"at_ms\":1}\n{\"kind\":\"ready\",\"at_ms\":2}\n",
        )?;

        let first = read_channel_inbox_tail(
            &home_directory,
            14,
            TailState {
                path: None,
                offset: 0,
                pending: Vec::new(),
            },
        )?;
        assert_eq!(first.lines.len(), 2);

        fs::write(
            &inbox,
            "{\"kind\":\"closed\",\"at_ms\":9,\"reason\":\"signal\"}\n",
        )?;

        let second = read_channel_inbox_tail(
            &home_directory,
            14,
            TailState {
                path: first.path,
                offset: first.offset,
                pending: first.pending,
            },
        )?;
        assert!(
            second.restarted,
            "a shorter inbox must restart, got offset {} for a file of {} bytes",
            first.offset,
            fs::metadata(&inbox)?.len()
        );
        assert_eq!(
            second.lines,
            vec![r#"{"kind":"closed","at_ms":9,"reason":"signal"}"#]
        );

        let missing = read_channel_inbox_tail(
            &home_directory,
            99,
            TailState {
                path: None,
                offset: 0,
                pending: Vec::new(),
            },
        )?;
        assert!(missing.lines.is_empty());
        assert!(missing.path.is_none());

        match read_channel_inbox_tail(
            &home_directory,
            0,
            TailState {
                path: None,
                offset: 0,
                pending: Vec::new(),
            },
        ) {
            Ok(_) => panic!("pid 0 must be refused"),
            Err(error) => assert!(error.to_string().contains("claude_pid"), "got {error:#}"),
        }

        fs::remove_dir_all(&home_directory).ok();
        Ok(())
    }

    const SAMPLE_ONE_JSON: &str = r#"{"pid":10064,"sessionId":"4e2e3600-89c0-4cd5-9994-525c708559ab",
 "cwd":"/Users/user/go/src/github.com/example/zed","startedAt":1789007244364,
 "procStart":"Thu Sep 10 02:27:23 2026","version":"2.1.267","peerProtocol":1,
 "peerFeatures":["notify_idle","reply_across_default_dirs","artifact_yield"],
 "kind":"interactive","entrypoint":"cli","pidDomain":"darwin",
 "tmux":"zed:@6.%8","messagingSocketPath":"/tmp/cc-socks/10064.sock",
 "name":"zed-e4","nameSource":"derived","nameSince":1789007244364,
 "status":"busy","updatedAt":1789009278063,"statusUpdatedAt":1789009278063,
 "bridgeSessionId":"session_01Tf2BzmxZDH3YKYpdxtSrpD"}"#;

    const SAMPLE_TWO_JSON: &str = r#"{"pid":17694,"sessionId":"095bcff6-b9a8-4584-a3c6-861f16c9a807",
 "cwd":"/Users/user/go/src/github.com/example/zed","startedAt":1789008783382,
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
            PathBuf::from("/Users/user/go/src/github.com/example/zed")
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
 "cwd":"/Users/user/go/src/github.com/example/zed","startedAt":1789007244364,
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
 "cwd":"/Users/user/go/src/github.com/example/zed",
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
        let project_root = Path::new("/Users/user/go/src/github.com/example/zed");

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
        session_other_project_no_name.working_directory = PathBuf::from("/Users/user/other");

        let mut session_other_project_zebra = parse_registered_session(SAMPLE_ONE_JSON)?;
        session_other_project_zebra.process_id = 6;
        session_other_project_zebra.name = Some("zebra".to_string());
        session_other_project_zebra.working_directory = PathBuf::from("/Users/user/other");

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
        let project_root = Path::new("/Users/user/go/src/github.com/example/zed");

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
        unnamed_higher_pid.working_directory = PathBuf::from("/Users/user/other");

        let mut unnamed_lower_pid = parse_registered_session(SAMPLE_ONE_JSON)?;
        unnamed_lower_pid.process_id = 300;
        unnamed_lower_pid.name = None;
        unnamed_lower_pid.working_directory = PathBuf::from("/Users/user/other");

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
    #[cfg(unix)]
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
        write_subagent_in_project(
            home_directory,
            "-some-slug",
            session_id,
            workflow_run_id,
            agent_id,
            meta_contents,
            transcript_contents,
        )
    }

    fn write_subagent_in_project(
        home_directory: &Path,
        project_directory: &str,
        session_id: &str,
        workflow_run_id: Option<&str>,
        agent_id: &str,
        meta_contents: &str,
        transcript_contents: &str,
    ) -> PathBuf {
        let mut directory = home_directory
            .join(".claude")
            .join("projects")
            .join(project_directory)
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
    fn a_command_below_nine_directories_is_still_listed() {
        let home_directory = temporary_directory("slash-command-depth");
        let mut command_directory = home_directory.join(".claude").join("commands");
        for component in ["a", "b", "c", "d", "e", "f", "g", "h", "i"] {
            command_directory.push(component);
        }
        write_file(
            command_directory.join("command.md"),
            "---\ndescription: Deep but finite\n---\nbody",
        );

        let commands = list_slash_commands(&home_directory, None);
        let expected = "a:b:c:d:e:f:g:h:i:command";
        let actual = commands.iter().any(|command| command.name == expected);
        assert_eq!(
            actual, true,
            "a finite command tree has no cycle; expected {expected:?} to be listed, got present={actual}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_cycle_is_visited_only_once() -> Result<()> {
        use std::os::unix::fs::symlink;

        let home_directory = temporary_directory("slash-command-real-cycle");
        let commands_directory = home_directory.join(".claude").join("commands");
        write_file(
            commands_directory.join("valid.md"),
            "---\ndescription: Valid command\n---\nbody",
        );
        symlink(&commands_directory, commands_directory.join("loop"))?;

        let commands = list_slash_commands(&home_directory, None);
        let actual = commands
            .iter()
            .filter(|command| command.name.ends_with("valid"))
            .count();
        assert_eq!(
            actual, 1,
            "one filesystem directory must contribute its command once even through a cycle; expected 1, got {actual}"
        );
        Ok(())
    }

    fn run_script(script: &Path, home: &Path, payload: &str) -> Result<std::process::Output> {
        Ok(smol::block_on(
            smol::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(r#"printf '%s' "$1" | /bin/sh "$2""#)
                .arg("--")
                .arg(payload)
                .arg(script)
                .env("HOME", home)
                .output(),
        )?)
    }

    fn write_script(path: &Path, source: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, source)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    }

    #[test]
    fn events_hook_writes_nothing_without_a_session_id() -> Result<()> {
        let home_directory = temporary_directory("events-no-session");
        let script = home_directory
            .join(".claude")
            .join("hooks")
            .join("zed-claude-events.sh");
        write_script(&script, EVENTS_HOOK_SOURCE)?;
        let output = run_script(&script, &home_directory, r#"{"hook_event_name":"Stop"}"#)?;
        assert!(output.status.success(), "the hook must always exit 0");
        assert!(output.stdout.is_empty(), "the hook prints nothing");
        let events = home_directory.join(".claude").join("zed-events");
        assert!(
            !events.exists() || fs::read_dir(&events)?.next().is_none(),
            "a payload without a session id must not create an events file"
        );
        Ok(())
    }

    #[test]
    fn events_hook_appends_a_wrapped_line_for_each_payload() -> Result<()> {
        let home_directory = temporary_directory("events-append");
        let script = home_directory
            .join(".claude")
            .join("hooks")
            .join("zed-claude-events.sh");
        write_script(&script, EVENTS_HOOK_SOURCE)?;
        let first = r#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"hi"}"#;
        let second = r#"{"session_id":"s1","hook_event_name":"Stop"}"#;
        assert!(
            run_script(&script, &home_directory, first)?
                .status
                .success()
        );
        assert!(
            run_script(&script, &home_directory, second)?
                .status
                .success()
        );
        let contents = fs::read_to_string(
            home_directory
                .join(".claude")
                .join("zed-events")
                .join("s1.jsonl"),
        )?;
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "each payload is one line, got {contents:?}");
        for (line, expected) in lines.iter().zip([first, second]) {
            let wrapper: serde_json::Value = serde_json::from_str(line)?;
            assert!(
                wrapper
                    .get("received_at_ms")
                    .and_then(serde_json::Value::as_i64)
                    .is_some()
            );
            let event = serde_json::to_string(wrapper.get("event").context("event wrapper")?)?;
            let expected: serde_json::Value = serde_json::from_str(expected)?;
            assert_eq!(serde_json::from_str::<serde_json::Value>(&event)?, expected);
        }
        Ok(())
    }

    #[test]
    fn events_hook_truncates_a_file_over_the_cap() -> Result<()> {
        let home_directory = temporary_directory("events-cap");
        let script = home_directory
            .join(".claude")
            .join("hooks")
            .join("zed-claude-events.sh");
        write_script(&script, EVENTS_HOOK_SOURCE)?;
        let events_file = home_directory
            .join(".claude")
            .join("zed-events")
            .join("s1.jsonl");
        fs::create_dir_all(events_file.parent().context("events parent")?)?;
        fs::write(
            &events_file,
            vec![b'x'; (EVENTS_FILE_CAP_BYTES as usize) + 1],
        )?;
        let payload = r#"{"session_id":"s1","hook_event_name":"Stop"}"#;
        assert!(
            run_script(&script, &home_directory, payload)?
                .status
                .success()
        );
        let contents = fs::read_to_string(&events_file)?;
        assert!(
            contents.len() < EVENTS_FILE_CAP_BYTES as usize,
            "the oversized file is truncated before the new line is appended; got {} bytes",
            contents.len()
        );
        assert_eq!(contents.lines().count(), 1);
        Ok(())
    }

    #[test]
    fn status_hook_writes_the_payload_and_chains() -> Result<()> {
        let home_directory = temporary_directory("status-chain");
        let script = home_directory
            .join(".claude")
            .join("hooks")
            .join("zed-claude-status.sh");
        write_script(&script, STATUS_HOOK_SOURCE)?;
        let chained = home_directory
            .join(".claude")
            .join("zed-status")
            .join("chained-command.txt");
        fs::create_dir_all(chained.parent().context("status parent")?)?;
        fs::write(&chained, "printf 'from-chain'")?;
        let payload = r#"{"session_id":"s1","model":{"id":"opus"}}"#;
        let output = run_script(&script, &home_directory, payload)?;
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "from-chain",
            "the displaced status line command still prints"
        );
        assert_eq!(
            fs::read_to_string(
                home_directory
                    .join(".claude")
                    .join("zed-status")
                    .join("s1.json")
            )?,
            payload
        );
        Ok(())
    }

    /// Hooks are not serialized with one another: two tool calls that run in parallel
    /// answer their PostToolUse at the same moment, and MessageDisplay fires beside them
    /// continuously. An append that is more than one `write` interleaves with theirs and
    /// destroys both lines, and a destroyed PostToolUse leaves its tool on the activity
    /// line until the transcript catches up.
    #[test]
    fn events_hook_appends_whole_lines_under_concurrency() -> Result<()> {
        let home_directory = temporary_directory("events-race");
        let script = home_directory
            .join(".claude")
            .join("hooks")
            .join("zed-claude-events.sh");
        write_script(&script, EVENTS_HOOK_SOURCE)?;

        const WRITERS: usize = 12;
        let payload = format!(
            r#"{{"session_id":"race","hook_event_name":"PostToolUse","tool_use_id":"t1","tool_response":"{}"}}"#,
            "x".repeat(200_000)
        );

        let mut writers = Vec::new();
        for _ in 0..WRITERS {
            let script = script.clone();
            let home_directory = home_directory.clone();
            let payload = payload.clone();
            writers.push(std::thread::spawn(move || {
                run_script(&script, &home_directory, &payload).map(|output| output.status.success())
            }));
        }
        for writer in writers {
            let finished = writer
                .join()
                .map_err(|_| anyhow::anyhow!("a writer panicked"))??;
            assert!(finished, "every invocation of the hook exits 0");
        }

        let contents = fs::read_to_string(
            home_directory
                .join(".claude")
                .join("zed-events")
                .join("race.jsonl"),
        )?;
        let whole: Vec<&str> = contents
            .lines()
            .filter(|line| serde_json::from_str::<serde_json::Value>(line).is_ok())
            .collect();
        assert_eq!(
            whole.len(),
            WRITERS,
            "every concurrent hook must leave one whole line; expected {} parseable lines, got {} of {} written",
            WRITERS,
            whole.len(),
            contents.lines().count()
        );
        Ok(())
    }

    /// The default only covers a key that is absent. A key that is present and null is a
    /// payload that names no session, and the rule for those is to write nothing.
    #[test]
    fn events_hook_writes_nothing_for_a_null_session_id() -> Result<()> {
        let home_directory = temporary_directory("events-null-session");
        let script = home_directory
            .join(".claude")
            .join("hooks")
            .join("zed-claude-events.sh");
        write_script(&script, EVENTS_HOOK_SOURCE)?;
        let output = run_script(
            &script,
            &home_directory,
            r#"{"session_id":null,"hook_event_name":"Stop"}"#,
        )?;
        assert!(output.status.success(), "the hook must always exit 0");

        let events = home_directory.join(".claude").join("zed-events");
        let written: Vec<String> = match fs::read_dir(&events) {
            Ok(entries) => entries
                .filter_map(|entry| Some(entry.ok()?.file_name().to_string_lossy().into_owned()))
                .collect(),
            Err(_) => Vec::new(),
        };
        assert!(
            written.is_empty(),
            "a null session id names no session; expected no events file, got {written:?}"
        );
        Ok(())
    }

    /// What the guard above must not reject: an ordinary payload, and one whose session id
    /// only the sed fallback can reach.
    #[test]
    fn events_hook_still_writes_for_an_ordinary_session_id() -> Result<()> {
        let home_directory = temporary_directory("events-ordinary-session");
        let script = home_directory
            .join(".claude")
            .join("hooks")
            .join("zed-claude-events.sh");
        write_script(&script, EVENTS_HOOK_SOURCE)?;
        assert!(
            run_script(
                &script,
                &home_directory,
                r#"{"session_id":"s-ordinary","hook_event_name":"Stop"}"#,
            )?
            .status
            .success()
        );
        let contents = fs::read_to_string(
            home_directory
                .join(".claude")
                .join("zed-events")
                .join("s-ordinary.jsonl"),
        )?;
        assert_eq!(
            contents.lines().count(),
            1,
            "an ordinary payload is still one appended line, got {contents:?}"
        );
        Ok(())
    }

    /// The status file is read whole on every poll and framed onto the wire behind it, so
    /// it needs the ceiling `ReadClaudeFile` already has. Nothing the status line writes
    /// comes anywhere near it.
    #[test]
    fn a_status_file_over_the_cap_is_refused() -> Result<()> {
        let home_directory = temporary_directory("status-cap");
        let status_file = home_directory
            .join(".claude")
            .join("zed-status")
            .join("s1.json");
        fs::create_dir_all(status_file.parent().context("status parent")?)?;
        fs::write(
            &status_file,
            vec![b'x'; (MAX_STATUS_FILE_BYTES as usize) + 1],
        )?;

        let read = read_session_status(&home_directory, "s1");
        assert!(
            read.is_err(),
            "a status file past the cap must be refused rather than read whole; expected Err, got Ok({:?})",
            read.map(|status| status.map(|status| status.len()))
        );
        Ok(())
    }

    /// What the cap must not reject: a status payload far larger than the CLI writes.
    #[test]
    fn a_large_but_legal_status_file_is_still_read() -> Result<()> {
        let home_directory = temporary_directory("status-under-cap");
        let status_file = home_directory
            .join(".claude")
            .join("zed-status")
            .join("s1.json");
        fs::create_dir_all(status_file.parent().context("status parent")?)?;
        let status = format!(
            r#"{{"session_id":"s1","model":{{"id":"opus","display_name":"{}"}}}}"#,
            "n".repeat(100 * 1024)
        );
        fs::write(&status_file, &status)?;

        let read = read_session_status(&home_directory, "s1")?;
        assert_eq!(
            read.as_deref().map(str::len),
            Some(status.len()),
            "a {} byte status file is well under the cap and must still be read whole",
            status.len()
        );
        Ok(())
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

    #[test]
    fn installing_zed_hooks_on_a_fresh_home_is_idempotent() -> Result<()> {
        let home_directory = temporary_directory("zed-hooks-fresh");
        match install_zed_hooks(&home_directory)? {
            HookInstallOutcome::Installed { .. } => {}
            other => panic!("fresh install writes settings, got {other:?}"),
        }
        assert!(zed_hooks_installed(&home_directory));
        let installed_server = fs::read_to_string(channel_server_path(&home_directory))?;
        assert_eq!(
            installed_server, CHANNEL_SERVER_SOURCE,
            "installing must copy the channel server"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(channel_server_path(&home_directory))?
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode, 0o644,
                "the channel server is not executable, got {mode:o}"
            );
        }
        match install_zed_hooks(&home_directory)? {
            HookInstallOutcome::AlreadyCurrent => {}
            other => panic!("installing twice must change nothing, got {other:?}"),
        }
        uninstall_zed_hooks(&home_directory)?;
        assert!(
            !channel_server_path(&home_directory).exists(),
            "uninstall must remove the channel server"
        );
        Ok(())
    }

    #[test]
    fn uninstall_removes_zed_entries_and_server_but_keeps_claude_json() -> Result<()> {
        let home_directory = temporary_directory("zed-hooks-uninstall");
        let claude_json = home_directory.join(".claude.json");
        write_file(claude_json.clone(), r#"{"mcpServers":{"zed-claude":{}}}"#);
        install_zed_hooks(&home_directory)?;

        uninstall_zed_hooks(&home_directory)?;

        let settings = read_claude_settings(&home_directory)?;
        assert!(!event_names_our_script(&settings, "zed-claude-events.sh"));
        assert!(!channel_server_path(&home_directory).exists());
        assert_eq!(
            fs::read_to_string(&claude_json)?,
            r#"{"mcpServers":{"zed-claude":{}}}"#,
            "uninstall leaves ~/.claude.json for the user's explicit claude mcp remove command"
        );
        Ok(())
    }

    #[test]
    fn installing_removes_legacy_duplicated_hooks_and_keeps_a_foreign_one() -> Result<()> {
        let home_directory = temporary_directory("zed-hooks-legacy");
        let settings_path = home_directory.join(".claude").join("settings.json");
        write_file(
            settings_path.clone(),
            r#"{
              "hooks": {
                "PreToolUse": [
                  {"matcher":"AskUserQuestion","hooks":[{"type":"command","command":"/Users/andy/.claude/hooks/record-pending-question.sh"}]},
                  {"matcher":"AskUserQuestion","hooks":[{"type":"command","command":"'/Users/andy/.claude/hooks/record-pending-question.sh'"}]},
                  {"matcher":"Bash","hooks":[{"type":"command","command":"echo foreign"}]}
                ],
                "MessageDisplay": [
                  {"hooks":[{"type":"command","command":"/Users/andy/.claude/hooks/record-live-message.sh"}]},
                  {"hooks":[{"type":"command","command":"'/Users/andy/.claude/hooks/record-live-message.sh'"}]}
                ]
              }
            }"#,
        );
        fs::create_dir_all(home_directory.join(".claude").join("hooks"))?;
        fs::write(
            home_directory
                .join(".claude")
                .join("hooks")
                .join("record-pending-question.sh"),
            "#!/bin/sh\n",
        )?;
        fs::write(
            home_directory
                .join(".claude")
                .join("hooks")
                .join("record-live-message.sh"),
            "#!/bin/sh\n",
        )?;

        install_zed_hooks(&home_directory)?;
        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path)?)?;
        let pre = settings
            .pointer("/hooks/PreToolUse")
            .and_then(serde_json::Value::as_array)
            .context("PreToolUse entries")?;
        assert!(
            pre.iter().any(|entry| {
                entry_command_strings(entry)
                    .iter()
                    .any(|command| command.contains("echo foreign"))
            }),
            "a foreign PreToolUse matcher must survive: {pre:?}"
        );
        assert!(
            !pre.iter().any(|entry| {
                entry_command_strings(entry).iter().any(|command| {
                    command.contains("record-pending-question.sh")
                        || command.contains("record-live-message.sh")
                })
            }),
            "legacy question hooks must be gone: {pre:?}"
        );
        assert!(
            !home_directory
                .join(".claude")
                .join("hooks")
                .join("record-pending-question.sh")
                .exists()
        );
        assert!(zed_hooks_installed(&home_directory));
        Ok(())
    }

    #[test]
    fn an_existing_status_line_is_chained_and_restored_on_uninstall() -> Result<()> {
        let home_directory = temporary_directory("zed-hooks-status");
        let settings_path = home_directory.join(".claude").join("settings.json");
        write_file(
            settings_path.clone(),
            r#"{"statusLine":{"type":"command","command":"echo mine"}}"#,
        );
        match install_zed_hooks(&home_directory)? {
            HookInstallOutcome::Installed { .. } => {}
            other => panic!("chaining a status line writes settings, got {other:?}"),
        }
        let chained = fs::read_to_string(
            home_directory
                .join(".claude")
                .join("zed-status")
                .join("chained-command.txt"),
        )?;
        assert_eq!(chained, "echo mine");
        uninstall_zed_hooks(&home_directory)?;
        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path)?)?;
        assert_eq!(
            settings
                .pointer("/statusLine/command")
                .and_then(serde_json::Value::as_str),
            Some("echo mine")
        );
        assert!(
            !home_directory
                .join(".claude")
                .join("zed-status")
                .join("chained-command.txt")
                .exists()
        );
        Ok(())
    }

    #[test]
    fn uninstall_keeps_a_user_command_mixed_into_the_same_matcher() -> Result<()> {
        let home_directory = temporary_directory("zed-hooks-mixed-matcher");
        let settings_path = home_directory.join(".claude").join("settings.json");
        write_file(
            settings_path.clone(),
            r#"{
              "hooks": {
                "PreToolUse": [
                  {
                    "matcher": "Bash",
                    "hooks": [
                      {"type": "command", "command": "echo user-own"},
                      {"type": "command", "command": "'/Users/andy/.claude/hooks/zed-claude-events.sh'", "timeout": 5}
                    ]
                  },
                  {"matcher": "Edit", "hooks": [{"type": "command", "command": "echo neighbour"}]}
                ]
              }
            }"#,
        );

        uninstall_zed_hooks(&home_directory)?;

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path)?)?;
        let pre = settings
            .pointer("/hooks/PreToolUse")
            .and_then(serde_json::Value::as_array)
            .context("PreToolUse entries")?;
        let commands: Vec<String> = pre.iter().flat_map(entry_command_strings).collect();
        assert!(
            commands.iter().any(|command| command == "echo user-own"),
            "the user's command in a matcher that also named our script must survive uninstall; \
             got {commands:?}"
        );
        assert!(
            commands.iter().any(|command| command == "echo neighbour"),
            "a neighbouring matcher that never named our script must survive uninstall; \
             got {commands:?}"
        );
        assert!(
            commands
                .iter()
                .all(|command| !command_refers_to(command, "zed-claude-events.sh")),
            "our script must be gone: {commands:?}"
        );

        fs::remove_dir_all(&home_directory).ok();
        Ok(())
    }

    #[test]
    fn uninstall_does_not_restore_a_status_line_the_user_replaced() -> Result<()> {
        let home_directory = temporary_directory("zed-hooks-status-replaced");
        let settings_path = home_directory.join(".claude").join("settings.json");
        write_file(
            settings_path.clone(),
            r#"{"statusLine":{"type":"command","command":"echo mine"}}"#,
        );
        install_zed_hooks(&home_directory)?;

        let mut settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path)?)?;
        let status_line = settings
            .pointer_mut("/statusLine")
            .and_then(serde_json::Value::as_object_mut)
            .context("statusLine after install")?;
        status_line.insert(
            "command".to_string(),
            serde_json::json!("echo replaced-after-install"),
        );
        fs::write(&settings_path, format!("{:#}\n", settings))?;

        uninstall_zed_hooks(&home_directory)?;

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path)?)?;
        let command = settings
            .pointer("/statusLine/command")
            .and_then(serde_json::Value::as_str);
        assert_eq!(
            command,
            Some("echo replaced-after-install"),
            "uninstall must not overwrite a statusLine the user replaced after install; \
             expected Some(\"echo replaced-after-install\"), got {command:?}"
        );
        assert!(
            !home_directory
                .join(".claude")
                .join("zed-status")
                .join("chained-command.txt")
                .exists(),
            "the chained file is ours and must still be removed"
        );

        fs::remove_dir_all(&home_directory).ok();
        Ok(())
    }

    #[test]
    fn uninstall_leaves_a_non_array_hooks_field_and_a_hook_without_command_untouched() -> Result<()>
    {
        let home_directory = temporary_directory("zed-hooks-malformed-matcher");
        let settings_path = home_directory.join(".claude").join("settings.json");
        write_file(
            settings_path.clone(),
            r#"{
              "hooks": {
                "PreToolUse": [
                  {
                    "matcher": "Bash",
                    "hooks": {"type": "command", "command": "'/tmp/zed-claude-events.sh'"}
                  },
                  {
                    "matcher": "Read",
                    "hooks": [
                      {"type": "prompt", "prompt": "confirm"},
                      {"type": "command", "command": "'/tmp/hooks/zed-claude-events.sh'", "timeout": 5}
                    ]
                  }
                ]
              }
            }"#,
        );

        uninstall_zed_hooks(&home_directory)?;

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path)?)?;
        let pre = settings
            .pointer("/hooks/PreToolUse")
            .and_then(serde_json::Value::as_array)
            .context("PreToolUse entries")?;
        let first = pre
            .first()
            .context("the matcher whose hooks field is not an array must remain")?;
        assert_eq!(
            first.get("hooks"),
            Some(&serde_json::json!({
                "type": "command",
                "command": "'/tmp/zed-claude-events.sh'"
            })),
            "a matcher whose hooks field is not an array must be left untouched, even if that object names our script"
        );
        let second = pre
            .get(1)
            .context("the matcher that mixed a prompt hook with our command must remain")?;
        assert_eq!(
            second.get("hooks"),
            Some(&serde_json::json!([{"type": "prompt", "prompt": "confirm"}])),
            "a hook object without a command must survive while our command is stripped"
        );

        fs::remove_dir_all(&home_directory).ok();
        Ok(())
    }

    #[test]
    fn unparseable_settings_write_nothing() -> Result<()> {
        let home_directory = temporary_directory("zed-hooks-bad-settings");
        let settings_path = home_directory.join(".claude").join("settings.json");
        write_file(settings_path.clone(), "{not json");
        let error = install_zed_hooks(&home_directory).expect_err("unparseable settings");
        assert!(
            error.to_string().contains("could not be parsed"),
            "got {error:#}"
        );
        assert!(
            !home_directory
                .join(".claude")
                .join("hooks")
                .join("zed-claude-events.sh")
                .exists(),
            "scripts must not be written when settings cannot be parsed"
        );
        assert_eq!(fs::read_to_string(&settings_path)?, "{not json");
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

    /// The id of the call `SUBAGENT_META_GENERAL_PURPOSE` records as having spawned it.
    const SPAWNING_CALL_ID: &str = "toolu_01BEgVRSnksoz6YAyUWEEdeQ";

    /// One line of a session's own conversation answering `tool_use_id`.
    fn tool_result_line(tool_use_id: &str) -> String {
        format!(
            "{{\"type\":\"user\",\"uuid\":\"r1\",\"message\":{{\"content\":[\
             {{\"type\":\"tool_result\",\"tool_use_id\":\"{tool_use_id}\",\"content\":\"done\"}}]}}}}\n"
        )
    }

    /// The sidecar of an agent launched with `Task` and left to work in the background,
    /// which is what every agent this panel is watched with is. The `requestShape` is what
    /// says its call is answered at launch rather than on return.
    const SUBAGENT_META_BACKGROUND: &str = r#"{"agentType":"general-purpose",
 "description":"印字然後等30分鐘",
 "toolUseId":"toolu_01RjmJewYCiTYMAWE4JAsM61","spawnDepth":1,
 "requestShape":"background","requestNonInteractive":true,"model":"opus"}"#;

    const BACKGROUND_CALL_ID: &str = "toolu_01RjmJewYCiTYMAWE4JAsM61";

    /// The sidecar of an agent run in the foreground, whose call is answered with what it
    /// found. `SUBAGENT_META_GENERAL_PURPOSE` is not one: like every agent spawned from
    /// this panel it records `requestShape: background`.
    const SUBAGENT_META_FOREGROUND: &str = r#"{"agentType":"general-purpose",
 "description":"Phase B adversarial review round 2",
 "toolUseId":"toolu_01BEgVRSnksoz6YAyUWEEdeQ","spawnDepth":1,
 "requestShape":"foreground","requestNonInteractive":false,"model":"opus"}"#;

    /// Captured from a real run: the call of a background agent is answered within two
    /// seconds of it launching, with an acknowledgement rather than with anything the
    /// agent found.
    fn async_launch_result_line(tool_use_id: &str, agent_id: &str) -> String {
        format!(
            "{{\"type\":\"user\",\"uuid\":\"r1\",\"message\":{{\"content\":[{{\"type\":\"tool_result\",\
             \"tool_use_id\":\"{tool_use_id}\",\"content\":[{{\"type\":\"text\",\"text\":\
             \"Async agent launched successfully.\\nagentId: {agent_id}\"}}]}}]}}}}\n"
        )
    }

    /// Captured from a real run: what a session records when a background agent stops.
    fn task_notification_line(agent_id: &str, tool_use_id: &str) -> String {
        format!(
            "{{\"type\":\"queue-operation\",\"operation\":\"enqueue\",\"content\":\
             \"<task-notification>\\n<task-id>{agent_id}</task-id>\\n\
             <tool-use-id>{tool_use_id}</tool-use-id>\\n<status>completed</status>\\n\
             </task-notification>\"}}\n"
        )
    }

    /// A background agent's call is answered the moment it launches, the same way a
    /// `Workflow` call is, so the answer says nothing about the agent being over. Reading
    /// it as the agent having returned takes every background agent off the list within
    /// two seconds of it starting — which is every agent this panel is watched with.
    #[test]
    fn a_background_agent_is_over_when_its_session_is_notified_not_when_its_call_returns()
    -> Result<()> {
        let home_directory = temporary_directory("background-agent-state");
        let working_session_id = "working-session";
        let stopped_session_id = "stopped-session";
        let agent_id = "a8fecd4de0ddace74";

        for session_id in [working_session_id, stopped_session_id] {
            write_subagent_in_project(
                &home_directory,
                "-a-project",
                session_id,
                None,
                agent_id,
                SUBAGENT_META_BACKGROUND,
                "{\"type\":\"user\",\"isSidechain\":true}\n",
            );
        }
        let project_directory = home_directory
            .join(".claude")
            .join("projects")
            .join("-a-project");
        let launched = async_launch_result_line(BACKGROUND_CALL_ID, agent_id);
        write_file(
            project_directory.join(format!("{working_session_id}.jsonl")),
            &launched,
        );
        write_file(
            project_directory.join(format!("{stopped_session_id}.jsonl")),
            &format!(
                "{launched}{}",
                task_notification_line(agent_id, BACKGROUND_CALL_ID)
            ),
        );

        let subagents_by_session = smol::block_on(list_subagents_for_sessions(
            &home_directory,
            &[
                working_session_id.to_string(),
                stopped_session_id.to_string(),
            ],
        ))?;

        assert_eq!(
            subagents_by_session
                .get(working_session_id)
                .context("the working session has a result entry")?[0]
                .task_agent_finished,
            Some(false),
            "its call was answered at launch, which is not the agent having returned"
        );
        assert_eq!(
            subagents_by_session
                .get(stopped_session_id)
                .context("the stopped session has a result entry")?[0]
                .task_agent_finished,
            Some(true),
            "its session was notified that it stopped, which is the only record of that"
        );

        Ok(())
    }

    /// A `Task` agent's transcript ends when it stops writing and its sidecar is never
    /// touched again, so the only record of it having returned is the `tool_result`
    /// answering the call that spawned it — in the session's own conversation, which the
    /// panel holds for one session while drawing every session's agents.
    #[test]
    fn a_task_agents_call_is_looked_for_in_its_own_sessions_conversation() -> Result<()> {
        let home_directory = temporary_directory("task-agent-state");
        let answered_session_id = "answered-session";
        let working_session_id = "working-session";
        let transcript_contents = "{\"type\":\"user\",\"isSidechain\":true}\n";

        for session_id in [answered_session_id, working_session_id] {
            write_subagent_in_project(
                &home_directory,
                "-a-project",
                session_id,
                None,
                REAL_AGENT_ID,
                SUBAGENT_META_FOREGROUND,
                transcript_contents,
            );
        }
        let project_directory = home_directory
            .join(".claude")
            .join("projects")
            .join("-a-project");
        write_file(
            project_directory.join(format!("{answered_session_id}.jsonl")),
            &tool_result_line(SPAWNING_CALL_ID),
        );
        // The session that spawned it is still waiting on it, so its conversation holds
        // an answer to some other call and none to this one.
        write_file(
            project_directory.join(format!("{working_session_id}.jsonl")),
            &tool_result_line("toolu_01SomethingElse"),
        );

        let subagents_by_session = smol::block_on(list_subagents_for_sessions(
            &home_directory,
            &[
                answered_session_id.to_string(),
                working_session_id.to_string(),
            ],
        ))?;

        assert_eq!(
            subagents_by_session
                .get(answered_session_id)
                .context("the answered session has a result entry")?[0]
                .task_agent_finished,
            Some(true),
            "the call that spawned it has been answered, which is the agent having returned"
        );
        assert_eq!(
            subagents_by_session
                .get(working_session_id)
                .context("the working session has a result entry")?[0]
                .task_agent_finished,
            Some(false),
            "nothing has answered the call that spawned it, so it is still working"
        );

        Ok(())
    }

    /// The answer is cached against the conversation's length and modification time, so a
    /// conversation that has since answered the call must not be served the old answer.
    #[test]
    fn a_call_answered_after_the_last_scan_is_read_again() -> Result<()> {
        let home_directory = temporary_directory("task-agent-state-again");
        let session_id = "late-answer-session";
        write_subagent_in_project(
            &home_directory,
            "-a-project",
            session_id,
            None,
            REAL_AGENT_ID,
            SUBAGENT_META_FOREGROUND,
            "{\"type\":\"user\",\"isSidechain\":true}\n",
        );
        let transcript_path = home_directory
            .join(".claude")
            .join("projects")
            .join("-a-project")
            .join(format!("{session_id}.jsonl"));
        write_file(
            transcript_path.clone(),
            "{\"type\":\"user\",\"uuid\":\"m1\"}\n",
        );

        let scan = || -> Result<Option<bool>> {
            let subagents_by_session = smol::block_on(list_subagents_for_sessions(
                &home_directory,
                &[session_id.to_string()],
            ))?;
            Ok(subagents_by_session
                .get(session_id)
                .context("the session has a result entry")?[0]
                .task_agent_finished)
        };

        assert_eq!(scan()?, Some(false));

        let mut answered = std::fs::read_to_string(&transcript_path)?;
        answered.push_str(&tool_result_line(SPAWNING_CALL_ID));
        write_file(transcript_path, &answered);

        assert_eq!(
            scan()?,
            Some(true),
            "the conversation grew, so what the last scan read of it no longer stands"
        );

        Ok(())
    }

    #[test]
    fn list_subagents_for_sessions_reads_each_session_across_projects() -> Result<()> {
        let home_directory = temporary_directory("subagent-listing-many-sessions");
        let plain_session_id = "plain-session";
        let workflow_session_id = "workflow-session";
        let empty_session_id = "empty-session";
        let missing_session_id = "missing-session";
        let transcript_contents = "{\"type\":\"user\",\"isSidechain\":true}\n";

        let plain_transcript = write_subagent_in_project(
            &home_directory,
            "-first-project",
            plain_session_id,
            None,
            REAL_AGENT_ID,
            SUBAGENT_META_GENERAL_PURPOSE,
            transcript_contents,
        );
        let workflow_transcript = write_subagent_in_project(
            &home_directory,
            "-second-project",
            workflow_session_id,
            Some(REAL_WORKFLOW_RUN_ID),
            "a1111e203ec41bc73",
            SUBAGENT_META_WORKFLOW,
            transcript_contents,
        );
        write_file(
            workflow_transcript.with_file_name(WORKFLOW_JOURNAL_FILE),
            "{\"type\":\"result\",\"agentId\":\"a1111e203ec41bc73\"}\n",
        );
        std::fs::create_dir_all(
            home_directory
                .join(".claude")
                .join("projects")
                .join("-third-project")
                .join(empty_session_id),
        )?;

        let session_ids = vec![
            plain_session_id.to_string(),
            workflow_session_id.to_string(),
            empty_session_id.to_string(),
            missing_session_id.to_string(),
        ];
        let subagents_by_session =
            smol::block_on(list_subagents_for_sessions(&home_directory, &session_ids))?;

        assert_eq!(subagents_by_session.len(), session_ids.len());
        let plain_subagents = subagents_by_session
            .get(plain_session_id)
            .context("the plain session has a result entry")?;
        assert_eq!(plain_subagents.len(), 1);
        assert_eq!(plain_subagents[0].agent_id, REAL_AGENT_ID);
        assert_eq!(plain_subagents[0].transcript_path, plain_transcript);

        let workflow_subagents = subagents_by_session
            .get(workflow_session_id)
            .context("the workflow session has a result entry")?;
        assert_eq!(workflow_subagents.len(), 1);
        assert_eq!(
            workflow_subagents[0].workflow_run_id.as_deref(),
            Some(REAL_WORKFLOW_RUN_ID)
        );
        assert_eq!(workflow_subagents[0].transcript_path, workflow_transcript);
        assert_eq!(workflow_subagents[0].workflow_agent_finished, Some(true));

        assert!(
            subagents_by_session
                .get(empty_session_id)
                .context("the session without a subagents directory has a result entry")?
                .is_empty()
        );
        assert!(
            subagents_by_session
                .get(missing_session_id)
                .context("the session without a directory has a result entry")?
                .is_empty()
        );

        Ok(())
    }

    /// One session whose `subagents` path cannot be read must not hide the agents every
    /// other session holds: the panel draws them all from this one scan, and a
    /// `subagents` path that is not a directory is a property of that session alone.
    #[test]
    fn list_subagents_for_sessions_keeps_other_sessions_when_one_cannot_be_read() -> Result<()> {
        let home_directory = temporary_directory("subagent-listing-one-blocked");
        let readable_session_id = "readable-session";
        let blocked_session_id = "blocked-session";
        let transcript_contents = "{\"type\":\"user\",\"isSidechain\":true}\n";

        write_subagent_in_project(
            &home_directory,
            "-readable-project",
            readable_session_id,
            None,
            REAL_AGENT_ID,
            SUBAGENT_META_GENERAL_PURPOSE,
            transcript_contents,
        );

        let blocked_session_directory = home_directory
            .join(".claude")
            .join("projects")
            .join("-blocked-project")
            .join(blocked_session_id);
        std::fs::create_dir_all(&blocked_session_directory)?;
        // Not a directory: `read_dir` fails with something other than NotFound, which is
        // the class of error the scan used to return for the whole map.
        std::fs::write(
            blocked_session_directory.join("subagents"),
            "not a directory",
        )?;

        let session_ids = vec![
            readable_session_id.to_string(),
            blocked_session_id.to_string(),
        ];
        let result = smol::block_on(list_subagents_for_sessions(&home_directory, &session_ids));
        let subagents_by_session = match result {
            Ok(subagents_by_session) => subagents_by_session,
            Err(error) => {
                panic!("expected Ok so the readable session is still listed, got Err({error:#})")
            }
        };

        let readable_agent_ids: Vec<&str> = subagents_by_session
            .get(readable_session_id)
            .map(|subagents| {
                subagents
                    .iter()
                    .map(|subagent| subagent.agent_id.as_str())
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(
            readable_agent_ids,
            vec![REAL_AGENT_ID],
            "the readable session's agent must still be listed, got {readable_agent_ids:?}; \
             keys were {:?}",
            subagents_by_session.keys().collect::<Vec<_>>()
        );

        assert_eq!(
            subagents_by_session.get(blocked_session_id).map(Vec::len),
            Some(0),
            "the blocked session stays a key mapping to no agents, got {:?}",
            subagents_by_session.get(blocked_session_id)
        );

        Ok(())
    }

    /// Session ids as Claude Code writes them are a UUID, which is one path component.
    /// The filter that rejects `..` and separators must not drop those, and a rejected
    /// id must still appear in the result as an empty list rather than failing the scan.
    #[test]
    fn list_subagents_for_sessions_accepts_a_uuid_and_keeps_rejected_ids_empty() -> Result<()> {
        let home_directory = temporary_directory("subagent-listing-id-filter");
        write_subagent(
            &home_directory,
            REAL_SESSION_ID,
            None,
            REAL_AGENT_ID,
            SUBAGENT_META_GENERAL_PURPOSE,
            "{\"type\":\"user\",\"isSidechain\":true}\n",
        );

        let mut session_ids = vec![REAL_SESSION_ID.to_string()];
        session_ids.extend(
            IDS_THAT_NAME_MORE_THAN_ONE_ENTRY
                .iter()
                .map(|session_id| (*session_id).to_string()),
        );

        let subagents_by_session =
            smol::block_on(list_subagents_for_sessions(&home_directory, &session_ids))
                .unwrap_or_else(|error| {
                    panic!(
                        "a mix of a real session id and rejected ids must be Ok, got Err({error:#})"
                    )
                });

        assert_eq!(
            subagents_by_session.len(),
            session_ids.len(),
            "every asked-for id must be a key, got {:?}",
            subagents_by_session.keys().collect::<Vec<_>>()
        );

        let listed_agent_ids: Vec<&str> = subagents_by_session
            .get(REAL_SESSION_ID)
            .map(|subagents| {
                subagents
                    .iter()
                    .map(|subagent| subagent.agent_id.as_str())
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(
            listed_agent_ids,
            vec![REAL_AGENT_ID],
            "the real UUID session id must be scanned, not dropped as an unsafe path; \
             got {listed_agent_ids:?}"
        );

        for rejected_id in IDS_THAT_NAME_MORE_THAN_ONE_ENTRY {
            assert_eq!(
                subagents_by_session.get(rejected_id).map(Vec::len),
                Some(0),
                "{rejected_id:?} must remain an empty entry rather than being joined onto \
                 a path, got {:?}",
                subagents_by_session.get(rejected_id)
            );
        }

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

    /// Captured from a real `parallel()` run: three agents in one phase, then one in
    /// another. Their ids sort into a different order than they started in, which is what
    /// makes the journal the only record of the order a reader should see them in.
    #[test]
    fn a_runs_agents_are_listed_in_the_order_the_journal_started_them() -> Result<()> {
        let home_directory = temporary_directory("subagent-journal-order");
        let session_id = "parallel-session";
        let workflow_run_id = "wf_145c9932-8c7";
        let started_in_order = [
            "a9f5821990bd75881",
            "ae82337df519e5f65",
            "a687e098981ffb2e2",
        ];

        for (agent_id, phase) in started_in_order
            .iter()
            .map(|agent_id| (*agent_id, "Fan out"))
            .chain([("ae7502cee18bbe2b4", "Collect")])
        {
            write_subagent(
                &home_directory,
                session_id,
                Some(workflow_run_id),
                agent_id,
                &format!(
                    r#"{{"agentType":"workflow-subagent","description":"{agent_id}","workflowPhase":"{phase}"}}"#
                ),
                "",
            );
        }

        let run_directory = home_directory
            .join(".claude")
            .join("projects")
            .join("-some-slug")
            .join(session_id)
            .join("subagents")
            .join("workflows")
            .join(workflow_run_id);
        let mut journal = String::from("{\"type\":\"launched\"}\n");
        for agent_id in started_in_order.iter().chain(["ae7502cee18bbe2b4"].iter()) {
            journal.push_str(&format!(
                "{{\"type\":\"started\",\"agentId\":\"{agent_id}\",\"phase\":\"Fan out\"}}\n"
            ));
        }
        std::fs::write(run_directory.join("journal.jsonl"), journal)?;

        let listed = smol::block_on(list_subagents(&home_directory, session_id))?;

        assert_eq!(
            listed
                .iter()
                .map(|summary| summary.agent_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "a9f5821990bd75881",
                "ae82337df519e5f65",
                "a687e098981ffb2e2",
                "ae7502cee18bbe2b4",
            ],
            "the journal started them alpha, beta, gamma, collect; sorting by agent id \
             would put gamma first and collect third"
        );
        Ok(())
    }
}

#[cfg(test)]
mod hook_freshness_tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    /// A hook that is installed but out of date is not installed as far as this panel is
    /// concerned.
    ///
    /// The script is Zed's own, and every fix to one of them reaches a machine only by
    /// being written there again. Checking that the file exists and stopping there left
    /// every machine that had ever installed a hook stuck on the version it first got —
    /// which is every machine the fixes were written for.
    #[test]
    fn test_a_hook_script_from_an_older_version_does_not_count_as_installed() -> Result<()> {
        let home_directory = std::env::temp_dir().join(format!(
            "zed-hook-freshness-{}-{}",
            std::process::id(),
            HOOK_FRESHNESS_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::remove_dir_all(&home_directory).ok();
        fs::create_dir_all(&home_directory)?;

        install_zed_hooks(&home_directory)?;
        assert!(
            zed_hooks_installed(&home_directory),
            "what was just installed must count as installed"
        );

        let script = events_hook_path(&home_directory);
        fs::write(
            &script,
            "#!/bin/sh\n# an older version of this hook\nexit 0\n",
        )?;

        assert!(
            !zed_hooks_installed(&home_directory),
            "a script that is not the one this version writes is one the fixes have not \
             reached, and the reader has to be offered the install again"
        );

        install_zed_hooks(&home_directory)?;
        assert!(
            zed_hooks_installed(&home_directory),
            "installing again must bring it up to date rather than leave it behind"
        );

        fs::remove_dir_all(&home_directory).ok();
        Ok(())
    }

    static HOOK_FRESHNESS_SEQUENCE: AtomicU32 = AtomicU32::new(0);
}

#[cfg(test)]
mod mention_tests {
    use super::*;

    fn write(path: PathBuf, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("creating the parent directory");
        }
        fs::write(path, contents).expect("writing the fixture");
    }

    fn tree(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("zed-mention-{name}-{}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        root
    }

    /// `@` is matched on the whole path, not on the file name alone: what someone is
    /// naming is often where a file is rather than what it is called.
    #[test]
    fn test_files_are_matched_on_their_whole_path() {
        let root = tree("path");
        write(root.join("crates").join("remote").join("session.rs"), "");
        write(root.join("docs").join("session.md"), "");

        assert_eq!(
            list_files_under(&root, "remote/ses"),
            vec!["crates/remote/session.rs".to_string()],
            "a query naming a directory must find what is under it"
        );

        let by_name = list_files_under(&root, "session");
        assert_eq!(
            by_name,
            vec![
                "docs/session.md".to_string(),
                "crates/remote/session.rs".to_string(),
            ],
            "the shorter path sorts first, so the file itself is above what is buried"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// An empty query is every file, because the menu opens on `@` before anything has
    /// been typed after it.
    #[test]
    fn test_an_empty_query_is_every_file() {
        let root = tree("empty");
        write(root.join("one.rs"), "");
        write(root.join("two.rs"), "");

        assert_eq!(
            list_files_under(&root, ""),
            vec!["one.rs".to_string(), "two.rs".to_string()]
        );

        fs::remove_dir_all(&root).ok();
    }

    /// The directories a repository is mostly made of are never what `@` means, and are
    /// the ones big enough to spend the whole walk. A `.`-prefixed entry is the user's
    /// own business the same way it is for commands.
    #[test]
    fn test_the_walk_skips_what_no_one_means_by_an_at_sign() {
        let root = tree("skips");
        write(root.join("wanted.rs"), "");
        write(root.join("node_modules").join("wanted.rs"), "");
        write(root.join("target").join("debug").join("wanted.rs"), "");
        write(root.join(".git").join("wanted.rs"), "");
        write(root.join(".hidden").join("wanted.rs"), "");

        assert_eq!(
            list_files_under(&root, "wanted"),
            vec!["wanted.rs".to_string()],
            "only the one outside them is a file anybody meant"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// A directory symlink pointing at an ancestor is walked forever otherwise, and the
    /// walk ends in a stack overflow rather than an error. The depth is what bounds it.
    #[test]
    #[cfg(unix)]
    fn test_a_directory_that_contains_itself_is_not_walked_forever() {
        let root = tree("cycle");
        write(root.join("here.rs"), "");
        std::os::unix::fs::symlink(&root, root.join("again")).expect("making the cycle");

        let found = list_files_under(&root, "here");
        assert!(
            !found.is_empty() && found.len() <= MENTION_MATCH_LIMIT,
            "the walk must end, and end with the file in it: got {found:?}"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// The name crosses a connection, so it is taken apart rather than trusted: a
    /// session is not a place to write a path of someone else's choosing.
    #[test]
    fn test_a_pasted_file_is_written_under_the_name_it_is_given_and_nowhere_else() {
        let home_directory = tree("pasted");
        fs::create_dir_all(&home_directory).expect("creating the home");

        let written = write_pasted_file(&home_directory, "shot.png", b"bytes")
            .expect("writing the pasted file");
        assert_eq!(
            written,
            home_directory
                .join(".claude")
                .join(PASTED_FILE_DIRECTORY)
                .join("shot.png")
                .to_string_lossy(),
        );
        assert_eq!(fs::read(&written).expect("reading it back"), b"bytes");

        // A name that spells a path keeps only its last component, so however it is
        // spelled it is written in the one directory and never above or beside it.
        let directory = home_directory.join(".claude").join(PASTED_FILE_DIRECTORY);
        for spelled in [
            "../escaped.png",
            "../../escaped.png",
            "/etc/passwd",
            "somewhere/else/shot2.png",
            // A trailing slash is not part of a path component, so this is the name
            // `somewhere` — odd, but inside the directory like every other.
            "somewhere/",
        ] {
            let written = write_pasted_file(&home_directory, spelled, b"bytes")
                .unwrap_or_else(|error| panic!("{spelled:?} was refused: {error:#}"));
            let written = PathBuf::from(&written);
            assert_eq!(
                written.parent(),
                Some(directory.as_path()),
                "{spelled:?} was written to {written:?}, which is not the one directory"
            );
        }

        // Nothing that is not a name at all, and nothing hidden: a dotfile is not what
        // anybody pasted, and a name Claude Code would not list is one nobody can use.
        for refused in ["", ".", "..", "/", ".hidden.png"] {
            assert!(
                write_pasted_file(&home_directory, refused, b"bytes").is_err(),
                "{refused:?} was accepted as a name to write"
            );
        }

        fs::remove_dir_all(&home_directory).ok();
    }

    /// A paste must not be able to fill the disk of the machine the session runs on.
    #[test]
    fn test_a_file_larger_than_the_limit_is_refused() {
        let home_directory = tree("too-large");
        fs::create_dir_all(&home_directory).expect("creating the home");

        let too_large = vec![0u8; MAX_PASTED_FILE_BYTES + 1];
        assert!(write_pasted_file(&home_directory, "huge.png", &too_large).is_err());
        // The limit itself is allowed: a bound that refuses what it names is off by one.
        let at_the_limit = vec![0u8; MAX_PASTED_FILE_BYTES];
        assert!(write_pasted_file(&home_directory, "big.png", &at_the_limit).is_ok());

        fs::remove_dir_all(&home_directory).ok();
    }

    #[test]
    #[cfg(unix)]
    fn pasted_file_pruning_stays_in_its_directory_and_ignores_symlinks() {
        let home_directory = tree("pasted-pruning");
        let directory = home_directory.join(".claude").join(PASTED_FILE_DIRECTORY);
        fs::create_dir_all(&directory).expect("creating the pasted-file directory");
        let now_ms = PASTED_FILE_KEEP.as_millis() as i64 + 10_000;
        let old_path = directory.join("pasted-1-aaaaaa.png");
        fs::write(&old_path, b"old").expect("writing the old pasted file");
        let outside = home_directory.join("outside.txt");
        fs::write(&outside, b"outside").expect("writing the outside file");
        let symlink = directory.join("pasted-2-bbbbbb.png");
        std::os::unix::fs::symlink(&outside, &symlink).expect("creating the symlink");

        prune_pasted_files(&directory, now_ms).expect("pruning old pasted files");

        assert!(
            !old_path.exists(),
            "an owned pasted file older than seven days is removed"
        );
        assert!(
            fs::symlink_metadata(&symlink).is_ok_and(|metadata| metadata.file_type().is_symlink()),
            "a symlink in the pasted directory is left untouched"
        );
        assert_eq!(
            fs::read(&outside).expect("reading the outside file"),
            b"outside",
            "pruning must never follow the symlink out of the pasted directory"
        );

        fs::remove_dir_all(&home_directory).ok();
    }

    /// Pruning is housekeeping, not part of the paste: one stale file this user cannot
    /// unlink — written by another uid, or removed by a second window between this
    /// prune's `read_dir` and its `symlink_metadata` — must not be what stops every
    /// later paste from being written.
    #[test]
    #[cfg(unix)]
    fn a_paste_is_written_even_when_an_old_file_cannot_be_pruned() {
        use std::os::unix::fs::PermissionsExt as _;

        let home_directory = tree("pasted-unprunable");
        let directory = home_directory.join(".claude").join(PASTED_FILE_DIRECTORY);
        fs::create_dir_all(&directory).expect("creating the pasted-file directory");

        // Old enough to be pruned, and sitting in a directory whose entries cannot be
        // unlinked. The file being written already exists, so the write itself needs no
        // permission the directory has taken away — only the prune does.
        let old_path = directory.join("pasted-1-aaaaaa.png");
        fs::write(&old_path, b"old").expect("writing the old pasted file");
        let name = "pasted-2-bbbbbb.png";
        fs::write(directory.join(name), b"stale").expect("writing the file to be replaced");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o500))
            .expect("making the pasted-file directory unwritable");

        // Root ignores the mode bits, so there the premise of this test does not hold.
        let running_as_root = fs::write(directory.join("root-probe"), b"").is_ok();
        if running_as_root {
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).ok();
            fs::remove_dir_all(&home_directory).ok();
            return;
        }

        let written = write_pasted_file(&home_directory, name, b"fresh bytes");
        let outcome = match &written {
            Ok(path) => format!("Ok({path})"),
            Err(error) => format!("Err({error:#})"),
        };
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).ok();

        assert!(
            written.is_ok(),
            "a paste must survive a prune that cannot finish; expected Ok(<path>), got {outcome}"
        );
        assert_eq!(
            fs::read(directory.join(name)).expect("reading the pasted file back"),
            b"fresh bytes",
            "the pasted bytes must reach the file the session is told to read"
        );

        fs::remove_dir_all(&home_directory).ok();
    }

    /// The seven days are counted from the name's own timestamp, so the boundary has to
    /// keep what is merely old rather than expired.
    #[test]
    fn pruning_keeps_a_pasted_file_that_is_not_yet_seven_days_old() {
        let home_directory = tree("pasted-fresh");
        let directory = home_directory.join(".claude").join(PASTED_FILE_DIRECTORY);
        fs::create_dir_all(&directory).expect("creating the pasted-file directory");

        let keep_ms = i64::try_from(PASTED_FILE_KEEP.as_millis()).expect("the keep window in ms");
        let now_ms = keep_ms + 10_000;
        let at_the_boundary = directory.join(format!("pasted-{}-cccccc.png", now_ms - keep_ms));
        let a_moment_ago = directory.join(format!("pasted-{}-dddddd.png", now_ms - 1));
        let unnamed = directory.join("notes.txt");
        for path in [&at_the_boundary, &a_moment_ago, &unnamed] {
            fs::write(path, b"keep").expect("writing a pasted file");
        }

        prune_pasted_files(&directory, now_ms).expect("pruning old pasted files");

        for path in [&at_the_boundary, &a_moment_ago, &unnamed] {
            assert!(
                path.exists(),
                "{} is not older than the keep window and must survive pruning",
                path.display()
            );
        }

        fs::remove_dir_all(&home_directory).ok();
    }
}

#[cfg(test)]
mod slash_command_skill_tests {
    use super::*;

    fn write(path: PathBuf, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("creating the parent directory");
        }
        fs::write(path, contents).expect("writing the fixture");
    }

    /// A skill is invoked by typing its name after a slash, exactly as a command in
    /// `commands/` is, so a menu that lists only `commands/` is missing most of what the
    /// session actually answers to. On this machine that was eleven of them.
    #[test]
    fn test_skills_are_slash_commands_too() {
        let home_directory =
            std::env::temp_dir().join(format!("zed-skills-{}", std::process::id()));
        let project_root = home_directory.join("project");
        fs::remove_dir_all(&home_directory).ok();

        write(
            home_directory
                .join(".claude")
                .join("skills")
                .join("wrangler")
                .join("SKILL.md"),
            "---\nname: wrangler\ndescription: The Workers CLI\n---\n\n# Wrangler\n",
        );
        write(
            project_root
                .join(".claude")
                .join("skills")
                .join("deploy")
                .join("SKILL.md"),
            "---\ndescription: Ship it\nargument-hint: <environment>\n---\n",
        );
        // A directory with no SKILL.md in it is not a skill, and a name that would be
        // read off the directory alone would put one in the menu that does not exist.
        fs::create_dir_all(home_directory.join(".claude").join("skills").join("notes"))
            .expect("creating the directory");

        let commands = list_slash_commands(&home_directory, Some(&project_root));
        let named = |name: &str| {
            commands
                .iter()
                .find(|command| command.name == name)
                .cloned()
        };

        let wrangler = named("wrangler").expect("the user's skill is a command");
        assert_eq!(wrangler.description.as_deref(), Some("The Workers CLI"));
        assert_eq!(wrangler.scope, SlashCommandScope::User);

        let deploy = named("deploy").expect("the project's skill is a command");
        assert_eq!(deploy.argument_hint.as_deref(), Some("<environment>"));
        assert_eq!(deploy.scope, SlashCommandScope::Project);

        assert!(
            named("notes").is_none(),
            "a directory with no SKILL.md in it is not a skill, got {:?}",
            commands
        );

        fs::remove_dir_all(&home_directory).ok();
    }

    /// A skill and a command of the same name are one row, as they are one command: the
    /// CLI resolves the name once.
    #[test]
    fn test_a_skill_and_a_command_of_one_name_are_one_row() {
        let home_directory =
            std::env::temp_dir().join(format!("zed-skill-clash-{}", std::process::id()));
        fs::remove_dir_all(&home_directory).ok();

        write(
            home_directory
                .join(".claude")
                .join("commands")
                .join("deploy.md"),
            "---\ndescription: From commands\n---\n",
        );
        write(
            home_directory
                .join(".claude")
                .join("skills")
                .join("deploy")
                .join("SKILL.md"),
            "---\ndescription: From skills\n---\n",
        );

        let commands = list_slash_commands(&home_directory, None);
        let deploys: Vec<&SlashCommand> = commands
            .iter()
            .filter(|command| command.name == "deploy")
            .collect();
        assert_eq!(deploys.len(), 1, "got {deploys:?}");

        fs::remove_dir_all(&home_directory).ok();
    }
}

#[cfg(test)]
#[cfg(test)]
mod attachment_boundary_tests {
    use super::*;

    /// The boundary a sent file is judged against. Everything a session may hand its
    /// reader sits under the directory the session works in or in the temporary
    /// directory; a path that climbs out of either is what this has to refuse, because
    /// the file it names is handed straight back to the requester.
    #[test]
    fn test_an_attachment_outside_the_working_directory_is_refused() {
        let working_directory = Path::new("/home/coder/project");

        for allowed in [
            "/home/coder/project/renders/poster.png",
            "/home/coder/project/poster.png",
            "/tmp/claude-501/scratchpad/grid.mp4",
            "/private/tmp/claude-501/scratchpad/grid.mp4",
        ] {
            assert!(
                attachment_is_readable(Path::new(allowed), working_directory),
                "{allowed} is a file the session delivered and must be readable"
            );
        }

        for refused in [
            "/home/coder/.ssh/id_rsa",
            "/home/coder/project/../.ssh/id_rsa",
            "/home/coder/projectile/poster.png",
            "/tmpfile/poster.png",
            "renders/poster.png",
        ] {
            assert!(
                !attachment_is_readable(Path::new(refused), working_directory),
                "{refused} is outside the session's own directory and must be refused"
            );
        }
    }

    /// A working directory that bounds nothing must not be read as bounding everything.
    #[test]
    fn test_a_working_directory_that_is_no_boundary_allows_nothing_under_it() {
        for no_boundary in ["/", "project", ""] {
            assert!(
                !attachment_is_readable(
                    Path::new("/home/coder/.ssh/id_rsa"),
                    Path::new(no_boundary)
                ),
                "a working directory of {no_boundary:?} must not open the filesystem"
            );
        }
    }
}

#[cfg(test)]
mod session_lifecycle_tests {
    use super::*;

    fn temporary_directory(label: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "claude-lifecycle-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("creating the temporary directory");
        path
    }

    #[test]
    fn parse_claude_agents_json_reads_the_documented_shapes() {
        let listings = parse_claude_agents_json(
            r#"[
              {
                "cwd": "/tmp/project",
                "kind": "interactive",
                "startedAt": "2026-09-18T00:00:00Z",
                "pid": 4242,
                "status": "waiting",
                "waitingFor": "permission prompt",
                "sessionId": "live-session",
                "name": "live",
                "unknownField": true
              },
              {
                "cwd": "/tmp/project",
                "kind": "background",
                "startedAt": 1750000000000,
                "id": "a1b2",
                "state": "working",
                "pid": 99,
                "sessionId": "bg-session"
              }
            ]"#,
        )
        .expect("documented shapes must parse");

        assert_eq!(listings.len(), 2);
        assert_eq!(listings[0].kind, "interactive");
        assert_eq!(listings[0].process_id, Some(4242));
        assert_eq!(listings[0].status.as_deref(), Some("waiting"));
        assert_eq!(
            listings[0].waiting_for.as_deref(),
            Some("permission prompt")
        );
        assert_eq!(listings[0].session_id.as_deref(), Some("live-session"));
        assert_eq!(listings[1].kind, "background");
        assert_eq!(listings[1].id.as_deref(), Some("a1b2"));
        assert_eq!(listings[1].state.as_deref(), Some("working"));
        assert_eq!(
            listings[1].started_at.as_deref(),
            Some("1750000000000"),
            "a numeric startedAt is kept as text rather than dropping the row"
        );
    }

    #[test]
    fn parse_claude_agents_json_rejects_a_non_array() {
        let error = parse_claude_agents_json(r#"{"kind":"interactive"}"#)
            .expect_err("an object is not the documented shape");
        assert!(error.to_string().contains("array"), "got {error:#}");
    }

    #[test]
    fn split_complete_lines_drops_a_pending_line_past_the_cap() {
        let mut pending = vec![b'x'; TAIL_PENDING_CAP_BYTES + 8];
        let lines = split_complete_lines(&mut pending);
        assert!(lines.is_empty(), "no newline, so nothing is a line");
        assert!(
            pending.is_empty(),
            "the partial line is dropped so the next newline can resync"
        );

        pending.extend_from_slice(b"still-the-old-line\n{\"type\":\"user\"}\npartial");
        let lines = split_complete_lines(&mut pending);
        assert_eq!(
            lines,
            vec![
                "still-the-old-line".to_string(),
                r#"{"type":"user"}"#.to_string()
            ]
        );
        assert_eq!(pending, b"partial");
    }

    /// A scan can read a conversation while the session is half way through appending a
    /// line to it. Those bytes have to be read again by the next scan: split in two,
    /// neither half parses, and the record they carry is lost for good.
    #[test]
    fn a_record_written_across_two_scans_is_absorbed_whole() {
        let directory = temporary_directory("split-record");
        let path = directory.join("conversation.jsonl");
        let record =
            r#"{"message":{"content":[{"type":"tool_result","tool_use_id":"toolu_split"}]}}"#;
        let (half_written, _) = record.split_at(40);

        std::fs::write(&path, half_written).expect("writing the half-written line");
        read_session_conversation(&path).expect("the scan that lands mid-line");

        let scanned = READ_CONVERSATIONS
            .lock()
            .ok()
            .and_then(|cache| Some(cache.as_ref()?.get(&path)?.offset_scanned));
        assert_eq!(
            scanned,
            Some(0),
            "the file holds no whole line yet, so nothing of it has been scanned"
        );

        std::fs::write(&path, format!("{record}\n")).expect("finishing the line");
        let conversation = read_session_conversation(&path).expect("the scan after it is whole");
        assert!(
            conversation.answered_calls.contains("toolu_split"),
            "the record finished between the two scans must be absorbed, so that the agent \
             it answers stops reading as unfinished; answered_calls is {:?}",
            conversation.answered_calls
        );

        std::fs::remove_dir_all(&directory).ok();
    }

    /// One line that cannot be decoded must not hide the records after it: the offset the
    /// scan records would never go back for them.
    #[test]
    fn a_line_that_is_not_utf8_does_not_hide_the_records_after_it() {
        let directory = temporary_directory("non-utf8-conversation");
        let path = directory.join("conversation.jsonl");
        let mut contents = Vec::new();
        contents.extend_from_slice(
            br#"{"message":{"content":[{"type":"tool_result","tool_use_id":"toolu_before"}]}}"#,
        );
        contents.push(b'\n');
        contents.extend_from_slice(b"{\"raw\":\"\xff\xfe\"}");
        contents.push(b'\n');
        contents.extend_from_slice(
            br#"{"message":{"content":[{"type":"tool_result","tool_use_id":"toolu_after"}]}}"#,
        );
        contents.push(b'\n');
        std::fs::write(&path, &contents).expect("writing the conversation");

        let conversation = read_session_conversation(&path).expect("scanning the conversation");
        assert!(
            conversation.answered_calls.contains("toolu_before"),
            "the record before the undecodable line is still absorbed; answered_calls is {:?}",
            conversation.answered_calls
        );
        assert!(
            conversation.answered_calls.contains("toolu_after"),
            "the records after an undecodable line must still be absorbed; answered_calls is {:?}",
            conversation.answered_calls
        );

        std::fs::remove_dir_all(&directory).ok();
    }

    /// The bound on a command that stops answering has to end the process. The child is
    /// spawned when its future is built, so a timeout that only stops waiting leaves it
    /// running with nothing left watching it.
    #[gpui::test]
    async fn a_command_that_outlives_its_timeout_is_killed(cx: &mut gpui::TestAppContext) {
        let directory = temporary_directory("kill-on-timeout");
        let abandoned_marker = directory.join("abandoned-marker");
        let control_marker = directory.join("control-marker");
        let script = |marker: &Path| format!("sleep 1; : > '{}'", marker.display());

        // The script is awaited for real rather than on the test clock: what this test is
        // about is a process that outlives its bound, which no virtual timer can stand in
        // for.
        cx.executor().allow_parking();

        // Run to completion, the same script does write its marker, so a missing marker
        // below says the process was killed rather than that the script never worked.
        let mut control_command = smol::process::Command::new("sh");
        control_command.arg("-c").arg(script(&control_marker));
        let control = output_within(
            &mut control_command,
            "the control script",
            Duration::from_secs(60),
            &cx.executor(),
        )
        .await
        .expect("running the control script");
        assert!(
            control.status.success(),
            "the control script must run: {control:?}"
        );
        assert!(
            control_marker.is_file(),
            "the control script must write {}",
            control_marker.display()
        );

        let mut command = smol::process::Command::new("sh");
        command.arg("-c").arg(script(&abandoned_marker));
        let refusal =
            output_within(&mut command, "the script", Duration::ZERO, &cx.executor()).await;
        let refusal = refusal.expect_err("a command past its bound must be refused");
        assert!(
            format!("{refusal:#}").contains("has not answered"),
            "got {refusal:#}"
        );

        std::thread::sleep(Duration::from_secs(3));
        assert!(
            !abandoned_marker.exists(),
            "the timed-out child must have been killed; it went on to write {}",
            abandoned_marker.display()
        );

        std::fs::remove_dir_all(&directory).ok();
    }

    /// An id reaches `claude` as one word of an argument list, and on a remote project it
    /// is the far end that chose it.
    #[test]
    fn an_id_that_would_reach_claude_as_an_option_is_refused() {
        assert_eq!(
            claude_command_operand("a1b2c3d4"),
            Some("a1b2c3d4"),
            "the short id an agents listing gives must still be usable"
        );
        assert_eq!(
            claude_command_operand("0f9a2b7c-1d4e-4a6b-8c2d-5e7f9a0b1c2d"),
            Some("0f9a2b7c-1d4e-4a6b-8c2d-5e7f9a0b1c2d"),
            "a session id is a hyphenated uuid and must still be resumable"
        );
        assert_eq!(
            claude_command_operand("-p"),
            None,
            "an id must not be able to reach claude as an option"
        );
        assert_eq!(
            claude_command_operand("--dangerously-skip-permissions"),
            None
        );
        assert_eq!(claude_command_operand("x; rm -rf ~"), None);
        assert_eq!(claude_command_operand(""), None);
        assert_eq!(
            claude_command_operand("abc/def"),
            None,
            "the path bound this grew out of is still in force"
        );
    }

    #[test]
    fn resume_session_id_with_a_separator_is_not_a_single_path_component() {
        assert_eq!(single_path_component("abc/def"), None);
        assert_eq!(single_path_component("abc"), Some("abc"));
        assert_eq!(single_path_component(".."), None);
    }

    #[test]
    fn session_files_directory_rejects_a_path_outside_the_cwd() {
        let home = std::env::temp_dir().join(format!(
            "zed-session-files-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        let cwd = home.join("project");
        fs::create_dir_all(&cwd).expect("creating the session cwd");
        fs::create_dir_all(home.join(".claude").join("sessions")).expect("creating the registry");
        fs::write(
            home.join(".claude").join("sessions").join("11.json"),
            format!(
                r#"{{"pid":11,"sessionId":"listed","cwd":"{}","procStart":"Thu Sep 10 02:27:23 2026","version":"2.1.267","kind":"interactive"}}"#,
                cwd.display()
            ),
        )
        .expect("writing the registration");

        let inside = session_files_directory(&home, "listed", Path::new("src"))
            .expect("a sub-path of the cwd is allowed");
        assert_eq!(inside, cwd.join("src"));

        let outside = session_files_directory(&home, "listed", Path::new("/etc"));
        assert!(outside.is_err(), "got {outside:?}");

        fs::remove_dir_all(&home).ok();
    }

    /// Every id Claude Code writes has to pass, and every id that could close a shell
    /// word, reach `claude` as an option, or fail to be one path component has to fail.
    #[test]
    fn claude_command_operand_accepts_the_ids_claude_writes_and_refuses_the_rest() {
        assert_eq!(
            claude_command_operand("0f9a2b7c-1d4e-4a6b-8c2d-5e7f9a0b1c2d"),
            Some("0f9a2b7c-1d4e-4a6b-8c2d-5e7f9a0b1c2d"),
            "a session id is a hyphenated uuid"
        );
        assert_eq!(claude_command_operand("a1b2c3d4"), Some("a1b2c3d4"));
        assert_eq!(
            claude_command_operand("zed-41"),
            Some("zed-41"),
            "the short id an agents listing gives may carry a hyphen"
        );
        assert_eq!(
            claude_command_operand(&"a".repeat(64)),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "64 bytes is the bound's own maximum and must still be usable"
        );

        assert_eq!(claude_command_operand("-p"), None);
        assert_eq!(claude_command_operand("x;y"), None);
        assert_eq!(claude_command_operand("x`y"), None);
        assert_eq!(claude_command_operand("x y"), None);
        assert_eq!(
            claude_command_operand(&"a".repeat(65)),
            None,
            "65 bytes is past the bound"
        );
        assert_eq!(claude_command_operand(""), None);
        assert_eq!(claude_command_operand(".."), None);
        assert_eq!(
            claude_command_operand("а1b2c3d4"),
            None,
            "a Cyrillic lookalike is not ASCII alphanumeric"
        );
        assert_eq!(claude_command_operand("a\0b"), None);
    }

    #[test]
    fn a_relative_cwd_is_not_where_claude_is_started() {
        let directory = temporary_directory("relative-cwd");
        assert_eq!(
            claude_command_working_directory(Path::new(".")),
            None,
            "a relative path is the process's directory, not a session's"
        );
        assert_eq!(
            claude_command_working_directory(&directory),
            Some(directory.as_path()),
            "an absolute directory that exists is still where a resume may start"
        );
        assert_eq!(
            claude_command_working_directory(&directory.join("gone")),
            None,
            "a path that does not exist is not a working directory"
        );
        let file = directory.join("file");
        fs::write(&file, b"").expect("writing a file that is not a directory");
        assert_eq!(
            claude_command_working_directory(&file),
            None,
            "a file is not a working directory"
        );
        assert_eq!(
            claude_command_working_directory(Path::new("/tmp\0nope")),
            None,
            "a path carrying a NUL is not a working directory"
        );
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn parse_claude_agents_json_refuses_a_10mb_array() {
        let json = format!(
            r#"[{{"kind":"interactive","id":"{}"}}]"#,
            "a".repeat(10 * 1024 * 1024)
        );
        let error = match parse_claude_agents_json(&json) {
            Err(error) => error,
            Ok(listings) => panic!(
                "a 10 MB array from a host must be refused; got {} listings, first id len {:?}",
                listings.len(),
                listings
                    .first()
                    .and_then(|listing| listing.id.as_ref().map(String::len))
            ),
        };
        let message = format!("{error:#}");
        assert!(
            message.contains("bytes") || message.contains("limit"),
            "got {message}"
        );
    }

    #[test]
    fn parse_claude_agents_json_keeps_waiting_for_as_text_and_drops_a_nul_id_as_an_operand() {
        let listings = parse_claude_agents_json(
            r#"[{"kind":"background","id":"a\u0000b","waitingFor":"\u001b[31mred","sessionId":"s"}]"#,
        )
        .expect("adversarial scalars must still parse as text");
        assert_eq!(listings.len(), 1);
        assert_eq!(listings[0].id.as_deref(), Some("a\0b"));
        assert_eq!(listings[0].waiting_for.as_deref(), Some("\u{1b}[31mred"));
        assert_eq!(
            claude_command_operand(listings[0].id.as_deref().unwrap_or("")),
            None,
            "a NUL in an id must never reach claude or a shell"
        );
    }

    #[test]
    fn parse_claude_agents_json_refuses_deep_nesting_instead_of_panicking() {
        let nested = format!(
            "[{}{}{}]",
            "{\"kind\":\"interactive\",\"x\":",
            "{".repeat(200),
            "}".repeat(201)
        );
        match parse_claude_agents_json(&nested) {
            Ok(listings) => {
                panic!("deep nesting must not be absorbed as listings, got {listings:?}")
            }
            Err(_) => {}
        }
    }

    /// The bound on a command that prints without bound has to refuse before the bytes
    /// become a string the rest of this module holds.
    #[gpui::test]
    async fn a_command_whose_output_exceeds_the_cap_is_refused(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let mut command = smol::process::Command::new("head");
        command
            .arg("-c")
            .arg(format!("{}", MAX_COMMAND_OUTPUT_BYTES + 1))
            .arg("/dev/zero");
        let refusal = match output_within(
            &mut command,
            "head",
            Duration::from_secs(60),
            &cx.executor(),
        )
        .await
        {
            Err(error) => error,
            Ok(output) => panic!(
                "a command that prints past the cap must be refused; got {} stdout bytes",
                output.stdout.len()
            ),
        };
        let message = format!("{refusal:#}");
        assert!(
            message.contains("bytes") || message.contains("limit"),
            "got {message}"
        );
    }

    #[gpui::test]
    async fn a_command_whose_output_fits_the_cap_is_kept(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let mut command = smol::process::Command::new("head");
        command.arg("-c").arg("16").arg("/dev/zero");
        let output = output_within(
            &mut command,
            "head",
            Duration::from_secs(60),
            &cx.executor(),
        )
        .await
        .expect("a 16-byte answer is inside the cap");
        assert!(output.status.success(), "got {output:?}");
        assert_eq!(output.stdout.len(), 16);
    }
}
