//! Tracks the Claude Code sessions running on this machine and follows the transcript
//! of the one the user selected.
//!
//! Two sources are polled rather than watched. Both live outside any worktree, so Zed's
//! worktree scanner cannot reach them, and polling an append-only file is cheap enough
//! that the simpler mechanism wins. The same shape will work unchanged when these reads
//! move behind a remote connection.

use std::{
    fs,
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result};
use collections::HashMap;
use gpui::{AppContext as _, BackgroundExecutor, Context, SharedString, Task};
use util::ResultExt as _;

use crate::{
    session_registry::{RegisteredSession, parse_registered_session, visible_sessions},
    transcript::{Transcript, parse_record},
};

const REGISTRY_POLL_INTERVAL: Duration = Duration::from_millis(1000);
const TRANSCRIPT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// No cutoff is applied to `updatedAt`, because it is not a heartbeat: on this machine
/// every registration carries `updatedAt == statusUpdatedAt`, so it only moves when the
/// session changes status. A session sitting idle waiting for its user, or busy on one
/// long tool call, leaves it untouched for hours while the process is plainly alive.
/// That makes the process itself the only liveness signal there is.
const HEARTBEAT_CUTOFF_MILLIS: i64 = i64::MAX;

/// Start times of the given processes, keyed by pid, in the format the registration files
/// use. Indirected the same way [`visible_sessions`] indirects its own lookup: the
/// production implementation shells out to `ps`, which a test cannot wait on.
type ProcessStartLookup =
    Arc<dyn Fn(Vec<u32>) -> Task<HashMap<u32, String>> + Send + Sync + 'static>;

pub struct ClaudeSessionStore {
    home_directory: PathBuf,
    project_root: Option<PathBuf>,
    sessions: Vec<RegisteredSession>,
    selected_process_id: Option<u32>,
    transcript: Transcript,
    /// Counts how often the transcript has been thrown away and started again, so that a
    /// caller caching anything derived from it can tell that its keys mean something
    /// else now.
    transcript_generation: u64,
    tail: Option<TranscriptTail>,
    process_start_lookup: ProcessStartLookup,
    error: Option<SharedString>,
    _registry_poll: Task<()>,
    _transcript_poll: Task<()>,
}

/// Where reading of the selected session's transcript has got to. Recreated from scratch
/// whenever the selection changes or the selected session starts a new conversation.
struct TranscriptTail {
    /// Identifies the conversation, not the process: Claude Code writes a new
    /// `sessionId` when the user runs `/clear`, and that is the signal to start over.
    session_id: String,
    /// `None` until the file appears. A session that has only just started is listed
    /// with no transcript rather than hidden.
    path: Option<PathBuf>,
    offset: u64,
    /// Bytes read after the last newline. Kept as bytes because a read can stop in the
    /// middle of a multi-byte character, which cannot be held as a `String`.
    pending: Vec<u8>,
}

#[derive(Clone)]
struct TailState {
    path: Option<PathBuf>,
    offset: u64,
    pending: Vec<u8>,
}

struct TailProgress {
    path: Option<PathBuf>,
    /// The offset this read began at. A tail only ever accepts progress that starts
    /// where it currently is, which is what distinguishes a read that was in flight when
    /// the tail restarted from one belonging to the tail that is being followed now.
    start_offset: u64,
    offset: u64,
    pending: Vec<u8>,
    lines: Vec<String>,
    /// The file was replaced or truncated, so everything absorbed so far is stale.
    restarted: bool,
}

impl ClaudeSessionStore {
    pub fn new(project_root: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let lookup = ps_process_start_lookup(cx.background_executor().clone());
        Self::with_home_directory(paths::home_dir().clone(), project_root, lookup, cx)
    }

    fn with_home_directory(
        home_directory: PathBuf,
        project_root: Option<PathBuf>,
        process_start_lookup: ProcessStartLookup,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            home_directory,
            project_root,
            sessions: Vec::new(),
            selected_process_id: None,
            transcript: Transcript::new(),
            transcript_generation: 0,
            tail: None,
            process_start_lookup,
            error: None,
            _registry_poll: Task::ready(()),
            _transcript_poll: Task::ready(()),
        };
        this._registry_poll = this.spawn_registry_poll(cx);
        this._transcript_poll = this.spawn_transcript_poll(cx);
        this
    }

    pub fn sessions(&self) -> &[RegisteredSession] {
        &self.sessions
    }

    pub fn selected(&self) -> Option<u32> {
        self.selected_process_id
    }

    pub fn select(&mut self, process_id: u32, cx: &mut Context<Self>) {
        if self.selected_process_id == Some(process_id) {
            return;
        }
        self.selected_process_id = Some(process_id);
        self.restart_tail();
        cx.notify();
    }

    pub fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    pub fn transcript_generation(&self) -> u64 {
        self.transcript_generation
    }

    /// `None` while the selected session has no transcript file yet, which is normal for
    /// a session that has only just started.
    pub fn transcript_path(&self) -> Option<&Path> {
        self.tail.as_ref()?.path.as_deref()
    }

    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    fn spawn_registry_poll(&self, cx: &mut Context<Self>) -> Task<()> {
        let registry_directory = self.home_directory.join(".claude").join("sessions");
        let project_root = self.project_root.clone();
        let process_start_lookup = self.process_start_lookup.clone();

        cx.spawn(async move |this, cx| {
            loop {
                let registrations = cx
                    .background_spawn({
                        let registry_directory = registry_directory.clone();
                        async move { read_registrations(&registry_directory) }
                    })
                    .await;
                let scan = resolve_visible_sessions(
                    registrations,
                    project_root.as_deref(),
                    &process_start_lookup,
                )
                .await;

                // The only exit: the store has been dropped, so nothing is left to update.
                if this
                    .update(cx, |this, cx| this.apply_registry_scan(scan, cx))
                    .is_err()
                {
                    break;
                }

                cx.background_executor().timer(REGISTRY_POLL_INTERVAL).await;
            }
        })
    }

    fn spawn_transcript_poll(&self, cx: &mut Context<Self>) -> Task<()> {
        let home_directory = self.home_directory.clone();

        cx.spawn(async move |this, cx| {
            loop {
                // The only exit: the store has been dropped, so nothing is left to update.
                let Ok(request) = this.read_with(cx, |this, _| this.tail_read_request()) else {
                    break;
                };

                if let Some((session_id, state)) = request {
                    let progress = cx
                        .background_spawn({
                            let home_directory = home_directory.clone();
                            let session_id = session_id.clone();
                            async move { read_transcript_tail(&home_directory, &session_id, state) }
                        })
                        .await;

                    if this
                        .update(cx, |this, cx| {
                            this.apply_tail_progress(&session_id, progress, cx)
                        })
                        .is_err()
                    {
                        break;
                    }
                }

                cx.background_executor()
                    .timer(TRANSCRIPT_POLL_INTERVAL)
                    .await;
            }
        })
    }

    /// `None` when no session is selected, which is what keeps an unselected session from
    /// being tailed.
    fn tail_read_request(&self) -> Option<(String, TailState)> {
        let tail = self.tail.as_ref()?;
        Some((
            tail.session_id.clone(),
            TailState {
                path: tail.path.clone(),
                offset: tail.offset,
                pending: tail.pending.clone(),
            },
        ))
    }

    fn apply_registry_scan(
        &mut self,
        scan: Result<Vec<RegisteredSession>>,
        cx: &mut Context<Self>,
    ) {
        let sessions = match scan {
            Ok(sessions) => sessions,
            Err(error) => {
                // The previous list is kept: a directory that is momentarily unreadable
                // should not empty the panel.
                self.error = Some(format!("Reading Claude sessions: {error:#}").into());
                cx.notify();
                return;
            }
        };

        // The scan runs every second whether or not anything moved, so the view is only
        // marked dirty when this one actually changed something.
        let mut changed = self.error.take().is_some() || self.sessions != sessions;

        let selected_session_id = self.selected_process_id.and_then(|process_id| {
            sessions
                .iter()
                .find(|session| session.process_id == process_id)
                .map(|session| session.session_id.clone())
        });

        self.sessions = sessions;

        match selected_session_id {
            // A different `sessionId` on the same pid means the user ran `/clear`, so the
            // old conversation must be dropped rather than appended to.
            Some(session_id) => {
                if self.tail.as_ref().map(|tail| tail.session_id.as_str()) != Some(&session_id) {
                    self.restart_tail();
                    changed = true;
                }
            }
            None => {
                if self.selected_process_id.is_some() {
                    self.selected_process_id = None;
                    self.restart_tail();
                    changed = true;
                }
            }
        }

        if changed {
            cx.notify();
        }
    }

    fn reset_transcript(&mut self) {
        self.transcript = Transcript::new();
        self.transcript_generation = self.transcript_generation.saturating_add(1);
    }

    fn restart_tail(&mut self) {
        self.reset_transcript();
        self.tail = self
            .selected_process_id
            .and_then(|process_id| {
                self.sessions
                    .iter()
                    .find(|session| session.process_id == process_id)
            })
            .map(|session| TranscriptTail {
                session_id: session.session_id.clone(),
                path: None,
                offset: 0,
                pending: Vec::new(),
            });
    }

    fn apply_tail_progress(
        &mut self,
        session_id: &str,
        progress: Result<TailProgress>,
        cx: &mut Context<Self>,
    ) {
        let progress = match progress {
            Ok(progress) => progress,
            Err(error) => {
                // Only the transcript is affected; the session list stays as it is.
                self.error = Some(format!("Reading transcript: {error:#}").into());
                cx.notify();
                return;
            }
        };

        // The selection can change while a read is in flight, in which case this progress
        // describes a file the store is no longer following.
        let Some(tail) = self
            .tail
            .as_ref()
            .filter(|tail| tail.session_id == session_id)
        else {
            return;
        };

        // The tail can also be restarted from the beginning while a read is in flight —
        // by the selection moving away and back — and the same `sessionId` then names a
        // conversation that is being read again from the start. Absorbing a read that
        // began further into the file would move the offset past everything before it,
        // and those records would never be read.
        if progress.start_offset != tail.offset {
            return;
        }

        let path_changed = tail.path != progress.path;

        if progress.restarted {
            self.reset_transcript();
        }

        // A line that cannot be parsed is logged and skipped: a transcript read while it
        // is being written can end in a line that never becomes valid, and the rest of the
        // batch is still worth absorbing.
        let records: Vec<_> = progress
            .lines
            .iter()
            .filter_map(|line| parse_record(line).log_err().flatten())
            .collect();
        let absorbed_any = !records.is_empty();
        self.transcript.absorb(records);

        if let Some(tail) = self.tail.as_mut() {
            tail.path = progress.path;
            tail.offset = progress.offset;
            tail.pending = progress.pending;
        }

        if absorbed_any || progress.restarted || path_changed {
            cx.notify();
        }
    }
}

/// Splits every complete line out of `buffer`, leaving any trailing partial line behind.
///
/// The buffer holds bytes rather than text because a read can stop in the middle of a
/// multi-byte character; only a run of bytes terminated by a newline is known to be a
/// whole line, and only then is it interpreted as text.
fn split_complete_lines(buffer: &mut Vec<u8>) -> Vec<String> {
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

/// Reads every registration in the directory. Liveness is decided later, by
/// [`resolve_visible_sessions`], because it needs a process lookup this cannot perform.
fn read_registrations(registry_directory: &Path) -> Result<Vec<RegisteredSession>> {
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

async fn resolve_visible_sessions(
    registrations: Result<Vec<RegisteredSession>>,
    project_root: Option<&Path>,
    process_start_lookup: &ProcessStartLookup,
) -> Result<Vec<RegisteredSession>> {
    let sessions = registrations?;
    let process_ids: Vec<u32> = sessions.iter().map(|session| session.process_id).collect();
    let process_starts = process_start_lookup(process_ids).await;
    // Both sides of the comparison are normalized here, because `ps` pads a single-digit
    // day of month with a second space that the registration does not have.
    let process_start_of_pid = |process_id: u32| {
        process_starts
            .get(&process_id)
            .map(|start_time| normalize_whitespace(start_time))
    };

    Ok(visible_sessions(
        sessions,
        project_root,
        now_millis(),
        HEARTBEAT_CUTOFF_MILLIS,
        &process_start_of_pid,
    ))
}

fn ps_process_start_lookup(executor: BackgroundExecutor) -> ProcessStartLookup {
    Arc::new(move |process_ids| executor.spawn(process_start_times(process_ids)))
}

/// Start times of the given processes, in the same format the registration files use.
///
/// One `ps` invocation covers macOS and Linux: `lstart` prints the wall-clock start time
/// that Claude Code recorded, whereas `/proc/<pid>/stat` reports clock ticks since boot,
/// which could only be compared after reconstructing boot time and the locale's date
/// formatting. Reading another process's environment is not an option either — on macOS
/// `ps eww` returns nothing for processes owned by other sessions.
#[cfg(unix)]
async fn process_start_times(process_ids: Vec<u32>) -> HashMap<u32, String> {
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

/// Windows has no equivalent of the registration files this store reads, so there is
/// nothing to check liveness against and every session is reported as gone.
#[cfg(not(unix))]
async fn process_start_times(_process_ids: Vec<u32>) -> HashMap<u32, String> {
    HashMap::default()
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A clock that is behind the epoch cannot happen in practice; reporting zero keeps every
/// session visible, which is the safer failure for a list the user is looking at.
fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// Locates `<home>/.claude/projects/*/<sessionId>.jsonl`.
///
/// The directory under `projects` is a slug derived from the session's working directory
/// by an undocumented rule, so the directory is searched rather than the name computed.
fn find_transcript(home_directory: &Path, session_id: &str) -> Option<PathBuf> {
    let projects_directory = home_directory.join(".claude").join("projects");
    let file_name = format!("{session_id}.jsonl");

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

fn read_transcript_tail(
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
                return Ok(TailProgress {
                    path: None,
                    start_offset,
                    offset: 0,
                    pending: Vec::new(),
                    lines: Vec::new(),
                    restarted: state.offset > 0 || !state.pending.is_empty(),
                });
            };
            // Either the file has only just appeared, or the one being followed was
            // replaced; in both cases it has to be read from the beginning.
            restarted = state.offset > 0 || !state.pending.is_empty();
            state.offset = 0;
            state.pending.clear();
            path
        }
    };

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

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

        // The blank line is the caller's problem, and `parse_record` answers it.
        match parse_record("") {
            Ok(None) => {}
            other => panic!("a blank line should parse to no record, got {other:?}"),
        }
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

    const FAKE_PROCESS_START: &str = "Thu Sep 10 02:27:23 2026";

    /// Reports the pids given here as running, with a start time the fixtures also use.
    /// `ps` is never invoked, so nothing in these tests depends on real time passing.
    fn fake_process_start_lookup(live_process_ids: Vec<u32>) -> ProcessStartLookup {
        Arc::new(move |requested| {
            let starts = requested
                .into_iter()
                .filter(|process_id| live_process_ids.contains(process_id))
                .map(|process_id| (process_id, FAKE_PROCESS_START.to_string()))
                .collect();
            Task::ready(starts)
        })
    }

    fn registration_json(process_id: u32, session_id: &str) -> String {
        format!(
            r#"{{"pid":{process_id},"sessionId":"{session_id}","cwd":"/tmp",
"procStart":"{FAKE_PROCESS_START}","version":"2.1.267","kind":"interactive",
"messagingSocketPath":"/tmp/cc-socks/{process_id}.sock","name":"live"}}"#
        )
    }

    fn write_file(path: PathBuf, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("creating the parent directory");
        }
        std::fs::write(&path, contents).expect("writing the fixture");
    }

    #[gpui::test]
    async fn test_registry_scan_skips_key_files_and_malformed_registrations() {
        let home_directory = temporary_directory("registry");
        let registry_directory = home_directory.join(".claude").join("sessions");

        write_file(
            registry_directory.join("11.json"),
            &registration_json(11, "live-session"),
        );
        write_file(registry_directory.join("broken.json"), "{not json");
        write_file(registry_directory.join("11.key"), "this must never be read");
        // A registration whose process is gone must be filtered out rather than listed.
        write_file(
            registry_directory.join("12.json"),
            &registration_json(12, "dead-session"),
        );

        let sessions = resolve_visible_sessions(
            read_registrations(&registry_directory),
            None,
            &fake_process_start_lookup(vec![11]),
        )
        .await
        .expect("scanning the registry");
        let session_ids: Vec<&str> = sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect();
        assert_eq!(
            session_ids,
            vec!["live-session"],
            "the malformed registration must be skipped without losing the good one"
        );

        std::fs::remove_dir_all(&home_directory).ok();
    }

    /// A registration's `updatedAt` is rewritten when the session's status changes, not on
    /// a timer, so a session that is idle waiting for its user goes hours without touching
    /// it. Reading it as a heartbeat hid every session on a machine running seven of them.
    #[gpui::test]
    async fn test_a_session_that_has_not_changed_status_for_an_hour_is_still_listed() {
        let home_directory = temporary_directory("idle-session");
        let registry_directory = home_directory.join(".claude").join("sessions");
        let an_hour_ago = now_millis() - 3_600_000;

        write_file(
            registry_directory.join("13.json"),
            &format!(
                r#"{{"pid":13,"sessionId":"idle-session","cwd":"/tmp",
"procStart":"{FAKE_PROCESS_START}","version":"2.1.267","kind":"interactive",
"name":"idle","status":"idle","updatedAt":{an_hour_ago}}}"#
            ),
        );

        let sessions = resolve_visible_sessions(
            read_registrations(&registry_directory),
            None,
            &fake_process_start_lookup(vec![13]),
        )
        .await
        .expect("scanning the registry");
        let session_ids: Vec<&str> = sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect();
        assert_eq!(
            session_ids,
            vec!["idle-session"],
            "the process is running, so an hour-old status timestamp must not hide it"
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

    #[gpui::test]
    async fn test_stale_tail_progress_is_dropped_after_the_tail_restarts(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("stale-tail");
        let registry_directory = home_directory.join(".claude").join("sessions");
        write_file(
            registry_directory.join("31.json"),
            &registration_json(31, "first-session"),
        );
        write_file(
            registry_directory.join("32.json"),
            &registration_json(32, "second-session"),
        );
        let transcript_path = write_transcript(
            &home_directory,
            "first-session",
            "{\"type\":\"user\",\"uuid\":\"a\"}\n",
        );

        let store = cx.new(|cx| {
            ClaudeSessionStore::with_home_directory(
                home_directory.clone(),
                None,
                fake_process_start_lookup(vec![31, 32]),
                cx,
            )
        });
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();

        store.update(cx, |store, cx| store.select(31, cx));
        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL * 2);
        cx.run_until_parked();

        // The read the poll is about to perform, captured while it is still in flight.
        let (session_id, state) = store
            .read_with(cx, |store, _| store.tail_read_request())
            .expect("the selected session must be tailed");
        assert!(
            state.offset > 0,
            "the first record must already have been read, so the in-flight read starts mid-file"
        );
        std::fs::write(
            &transcript_path,
            "{\"type\":\"user\",\"uuid\":\"a\"}\n{\"type\":\"assistant\",\"uuid\":\"b\",\"parentUuid\":\"a\"}\n",
        )
        .expect("appending to the transcript");
        let progress = read_transcript_tail(&home_directory, &session_id, state);

        // Meanwhile the user looks at another session and comes back, which starts the
        // conversation over from the beginning of the file.
        store.update(cx, |store, cx| store.select(32, cx));
        store.update(cx, |store, cx| store.select(31, cx));

        // The read that was in flight before the restart now lands.
        store.update(cx, |store, cx| {
            store.apply_tail_progress(&session_id, progress, cx)
        });

        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL * 2);
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            let uuids: Vec<Option<&str>> = store
                .transcript()
                .active_path()
                .iter()
                .map(|record| record.uuid.as_deref())
                .collect();
            assert_eq!(
                uuids,
                vec![Some("a"), Some("b")],
                "a read from before the restart must not make the fresh tail skip the start of the file"
            );
        });

        std::fs::remove_dir_all(&home_directory).ok();
    }

    /// What the guard on [`ClaudeSessionStore::apply_tail_progress`] must not kill: a
    /// read that reports a restart of its own accord, because the file it was following
    /// was rewritten, still belongs to the tail that asked for it.
    #[gpui::test]
    async fn test_a_read_that_restarts_itself_is_still_accepted(cx: &mut gpui::TestAppContext) {
        let home_directory = temporary_directory("rewritten-file");
        let registry_directory = home_directory.join(".claude").join("sessions");
        write_file(
            registry_directory.join("41.json"),
            &registration_json(41, "only-session"),
        );
        write_transcript(
            &home_directory,
            "only-session",
            "{\"type\":\"user\",\"uuid\":\"a\"}\n{\"type\":\"assistant\",\"uuid\":\"b\",\"parentUuid\":\"a\"}\n",
        );

        let store = cx.new(|cx| {
            ClaudeSessionStore::with_home_directory(
                home_directory.clone(),
                None,
                fake_process_start_lookup(vec![41]),
                cx,
            )
        });
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        store.update(cx, |store, cx| store.select(41, cx));
        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL * 2);
        cx.run_until_parked();

        let uuids = |store: &ClaudeSessionStore| -> Vec<Option<String>> {
            store
                .transcript()
                .active_path()
                .iter()
                .map(|record| record.uuid.clone())
                .collect()
        };
        store.read_with(cx, |store, _| {
            assert_eq!(
                uuids(store),
                vec![Some("a".to_string()), Some("b".to_string())]
            );
        });

        // Shorter than what has been read, so the read itself reports a restart.
        write_transcript(
            &home_directory,
            "only-session",
            "{\"type\":\"user\",\"uuid\":\"z\"}\n",
        );
        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL * 2);
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert_eq!(
                uuids(store),
                vec![Some("z".to_string())],
                "a rewritten file must still be absorbed, not rejected as stale"
            );
        });

        std::fs::remove_dir_all(&home_directory).ok();
    }

    #[gpui::test]
    async fn test_registry_scan_normalizes_padded_process_start() {
        let home_directory = temporary_directory("padded");
        let registry_directory = home_directory.join(".claude").join("sessions");

        // `ps` pads a single-digit day of month with a second space; the registration
        // does not. Both sides must be normalized or the session reads as a reused pid.
        write_file(
            registry_directory.join("21.json"),
            r#"{"pid":21,"sessionId":"padded","cwd":"/tmp",
"procStart":"Thu Sep 1 02:27:23 2026","version":"2.1.267","kind":"interactive"}"#,
        );

        let padded_lookup: ProcessStartLookup = Arc::new(|_requested| {
            Task::ready(HashMap::from_iter([(
                21,
                "Thu Sep  1 02:27:23 2026".to_string(),
            )]))
        });
        let sessions = resolve_visible_sessions(
            read_registrations(&registry_directory),
            None,
            &padded_lookup,
        )
        .await
        .expect("scanning the registry");
        assert_eq!(
            sessions.len(),
            1,
            "expected the session to be live, got {:?}",
            sessions
                .iter()
                .map(|session| session.process_start.clone())
                .collect::<Vec<_>>()
        );

        std::fs::remove_dir_all(&home_directory).ok();
    }

    #[gpui::test]
    async fn test_store_lists_sessions_and_follows_the_selected_transcript(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("store");
        let registry_directory = home_directory.join(".claude").join("sessions");
        std::fs::create_dir_all(&registry_directory).expect("creating the registry directory");

        let process_id = 4242;
        let registration_path = registry_directory.join(format!("{process_id}.json"));
        let write_registration = |session_id: &str| {
            std::fs::write(
                &registration_path,
                registration_json(process_id, session_id),
            )
            .expect("writing the registration");
        };
        write_registration("first-conversation");

        let store = cx.new(|cx| {
            ClaudeSessionStore::with_home_directory(
                home_directory.clone(),
                None,
                fake_process_start_lookup(vec![process_id]),
                cx,
            )
        });

        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            let process_ids: Vec<u32> = store
                .sessions()
                .iter()
                .map(|session| session.process_id)
                .collect();
            assert_eq!(process_ids, vec![process_id]);
            assert_eq!(store.error(), None);
            // No selection yet, so nothing is tailed.
            assert_eq!(store.selected(), None);
            assert_eq!(store.transcript_path(), None);
        });

        // A session with no transcript file is still listed and selectable.
        store.update(cx, |store, cx| store.select(process_id, cx));
        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL * 2);
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert_eq!(store.selected(), Some(process_id));
            assert_eq!(store.transcript_path(), None);
            assert!(store.transcript().active_path().is_empty());
        });

        let transcript_path = write_transcript(
            &home_directory,
            "first-conversation",
            "{\"type\":\"user\",\"uuid\":\"a\"}\n",
        );
        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL * 2);
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert_eq!(store.transcript_path(), Some(transcript_path.as_path()));
            let uuids: Vec<Option<&str>> = store
                .transcript()
                .active_path()
                .iter()
                .map(|record| record.uuid.as_deref())
                .collect();
            assert_eq!(uuids, vec![Some("a")]);
        });

        // A malformed line must not stop the lines after it from being absorbed.
        std::fs::write(
            &transcript_path,
            "{\"type\":\"user\",\"uuid\":\"a\"}\n{not json\n{\"type\":\"assistant\",\"uuid\":\"b\",\"parentUuid\":\"a\"}\n",
        )
        .expect("appending to the transcript");
        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL * 2);
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            let uuids: Vec<Option<&str>> = store
                .transcript()
                .active_path()
                .iter()
                .map(|record| record.uuid.as_deref())
                .collect();
            assert_eq!(uuids, vec![Some("a"), Some("b")]);
        });

        // `/clear` gives the same pid a new sessionId, which must reset the transcript.
        write_registration("second-conversation");
        write_transcript(
            &home_directory,
            "second-conversation",
            "{\"type\":\"user\",\"uuid\":\"z\"}\n",
        );
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL * 2);
        cx.run_until_parked();
        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL * 2);
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            let uuids: Vec<Option<&str>> = store
                .transcript()
                .active_path()
                .iter()
                .map(|record| record.uuid.as_deref())
                .collect();
            assert_eq!(
                uuids,
                vec![Some("z")],
                "the previous conversation must not survive a new sessionId"
            );
            assert!(store.transcript().compact_boundaries().is_empty());
            assert!(!store.transcript().is_empty());
            assert_eq!(store.transcript().full_path().len(), 1);
        });

        // Dropping the store must stop both polls; the clock is advanced far past both
        // intervals afterwards to give a leaked task a chance to panic on a missing entity.
        drop(store);
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL * 10);
        cx.run_until_parked();

        std::fs::remove_dir_all(&home_directory).ok();
    }
}
