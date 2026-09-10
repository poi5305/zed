//! Tracks the Claude Code sessions running on this machine and follows the transcript
//! of the one the user selected.
//!
//! Two sources are polled rather than watched. Both live outside any worktree, so Zed's
//! worktree scanner cannot reach them, and polling an append-only file is cheap enough
//! that the simpler mechanism wins. The same shape will work unchanged when these reads
//! move behind a remote connection.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Result, anyhow};
use collections::HashMap;
use gpui::{Context, SharedString, Task};
use util::ResultExt as _;

use crate::{
    session_registry::{RegisteredSession, TailProgress, TailState, pane_target},
    session_source::{SessionInput, SessionListing, SessionSource},
    transcript::{Transcript, parse_record},
};

const REGISTRY_POLL_INTERVAL: Duration = Duration::from_millis(1000);
const TRANSCRIPT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Reported when a send is attempted for a session Zed has no pane to type into. The UI
/// disables the input in that case, so this is the answer to a send that got through
/// anyway rather than something the user is expected to see.
const NO_PANE_TARGET: &str = "This session is not running inside tmux, so Zed cannot type into it.";

/// What put the message in [`ClaudeSessionStore::error`]. A poll that succeeds clears the
/// error a previous poll left behind, and that must not also wipe the report of a send
/// the user has just watched fail: the polls run every second, so a send failure with no
/// source of its own would be gone before it could be read.
#[derive(PartialEq, Eq)]
enum ErrorSource {
    Poll,
    Send,
}

pub struct ClaudeSessionStore {
    source: Arc<dyn SessionSource>,
    project_root: Option<PathBuf>,
    sessions: Vec<RegisteredSession>,
    /// The home directory of the machine the sessions were scanned on, as that machine
    /// reported it. `None` until the first scan arrives: the paths that have to be
    /// checked against it come from that machine too, and this process's own home is not
    /// an answer about a remote one.
    home_directory: Option<PathBuf>,
    /// Where the last scan found each listed session's transcript, keyed by pid. Only the
    /// machine a session runs on can locate its transcript, so this comes from the scan
    /// rather than being looked for here.
    transcript_paths: HashMap<u32, PathBuf>,
    selected_process_id: Option<u32>,
    transcript: Transcript,
    /// Counts how often the transcript has been thrown away and started again, so that a
    /// caller caching anything derived from it can tell that its keys mean something
    /// else now.
    transcript_generation: u64,
    tail: Option<TranscriptTail>,
    error: Option<(ErrorSource, SharedString)>,
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

impl ClaudeSessionStore {
    pub fn new(
        source: Arc<dyn SessionSource>,
        project_root: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            source,
            project_root,
            sessions: Vec::new(),
            home_directory: None,
            transcript_paths: HashMap::default(),
            selected_process_id: None,
            transcript: Transcript::new(),
            transcript_generation: 0,
            tail: None,
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

    /// The home directory of the machine the listed sessions run on, or `None` while no
    /// scan has reported one yet. A caller checking a path the scan led to against a
    /// boundary under the home directory has nothing to check it against until then.
    pub fn home_directory(&self) -> Option<&Path> {
        self.home_directory.as_deref()
    }

    pub fn selected(&self) -> Option<u32> {
        self.selected_process_id
    }

    pub fn select(&mut self, process_id: u32, cx: &mut Context<Self>) {
        if self.selected_process_id == Some(process_id) {
            return;
        }
        self.selected_process_id = Some(process_id);
        // A send that failed for the session being left says nothing about this one.
        self.take_error_from(ErrorSource::Send);
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
        let process_id = self.selected_process_id?;
        self.transcript_paths.get(&process_id).map(PathBuf::as_path)
    }

    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref().map(|(_, message)| message)
    }

    /// Drops the error if it came from `source`, reporting whether it did.
    fn take_error_from(&mut self, source: ErrorSource) -> bool {
        if self
            .error
            .as_ref()
            .is_some_and(|(error_source, _)| *error_source == source)
        {
            self.error = None;
            return true;
        }
        false
    }

    /// The tmux pane the selected session can be typed into, or `None` when there is
    /// none — no selection, no `tmux` field, or a field that is not shaped like a pane
    /// id. The input UI is enabled by exactly this answer.
    pub fn pane_target(&self) -> Option<String> {
        let session = self.selected_session()?;
        pane_target(session.tmux_target.as_deref()?)
    }

    /// Sends to the selected session. Only ever called from a user gesture: nothing in
    /// the polling or rendering path sends.
    ///
    /// The returned task reports a failure into [`Self::error`] and hands the same
    /// result back to the caller, who is holding the text that failed to arrive and is
    /// the only one that can give it back to the user. Dropping the task cancels the
    /// send half-way, so a caller with no use for the result must detach it rather than
    /// let it go.
    pub fn send_input(&mut self, input: SessionInput, cx: &mut Context<Self>) -> Task<Result<()>> {
        // Whatever the last send reported is about to be answered by this one.
        self.take_error_from(ErrorSource::Send);

        let Some(pane_target) = self.pane_target() else {
            self.error = Some((ErrorSource::Send, NO_PANE_TARGET.into()));
            cx.notify();
            return Task::ready(Err(anyhow!("{NO_PANE_TARGET}")));
        };

        let send = self.source.send_input(pane_target, input);
        cx.spawn(async move |this, cx| {
            let result = send.await;
            if let Err(error) = &result {
                this.update(cx, |this, cx| {
                    this.error = Some((
                        ErrorSource::Send,
                        format!("Sending to the session: {error:#}").into(),
                    ));
                    cx.notify();
                })
                .log_err();
            }
            result
        })
    }

    fn selected_session(&self) -> Option<&RegisteredSession> {
        let process_id = self.selected_process_id?;
        self.sessions
            .iter()
            .find(|session| session.process_id == process_id)
    }

    fn spawn_registry_poll(&self, cx: &mut Context<Self>) -> Task<()> {
        let source = self.source.clone();
        let project_root = self.project_root.clone();

        cx.spawn(async move |this, cx| {
            loop {
                let scan = source.list_sessions(project_root.clone()).await;

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
        let source = self.source.clone();

        cx.spawn(async move |this, cx| {
            loop {
                // The only exit: the store has been dropped, so nothing is left to update.
                let Ok(request) = this.read_with(cx, |this, _| this.tail_read_request()) else {
                    break;
                };

                if let Some((session_id, state)) = request {
                    let progress = source.tail_transcript(session_id.clone(), state).await;

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

    fn apply_registry_scan(&mut self, scan: Result<SessionListing>, cx: &mut Context<Self>) {
        let listing = match scan {
            Ok(listing) => listing,
            Err(error) => {
                // The previous list is kept: a directory that is momentarily unreadable
                // should not empty the panel.
                self.error = Some((
                    ErrorSource::Poll,
                    format!("Reading Claude sessions: {error:#}").into(),
                ));
                cx.notify();
                return;
            }
        };

        let home_directory = Some(listing.home_directory);
        let mut transcript_paths = HashMap::default();
        let mut sessions = Vec::with_capacity(listing.sessions.len());
        for summary in listing.sessions {
            if let Some(transcript_path) = summary.transcript_path {
                transcript_paths.insert(summary.session.process_id, transcript_path);
            }
            sessions.push(summary.session);
        }

        // The scan runs every second whether or not anything moved, so the view is only
        // marked dirty when this one actually changed something.
        let mut changed = self.take_error_from(ErrorSource::Poll)
            || self.sessions != sessions
            || self.transcript_paths != transcript_paths
            || self.home_directory != home_directory;
        self.transcript_paths = transcript_paths;
        self.home_directory = home_directory;

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
                self.error = Some((
                    ErrorSource::Poll,
                    format!("Reading transcript: {error:#}").into(),
                ));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Mutex,
        atomic::{AtomicU32, Ordering},
    };

    use gpui::AppContext as _;

    use crate::{
        session_registry::{
            HEARTBEAT_CUTOFF_MILLIS, SessionSummary, find_transcript, normalize_whitespace,
            now_millis, read_registrations, read_transcript_tail, visible_sessions,
        },
        session_source::{FileContents, read_file_prefix},
    };

    #[test]
    fn test_parse_record_on_a_blank_line() {
        // `split_complete_lines` emits a blank line between two newlines, and this is the
        // answer to it.
        match parse_record("") {
            Ok(None) => {}
            other => panic!("a blank line should parse to no record, got {other:?}"),
        }
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

    const FAKE_PROCESS_START: &str = "Thu Sep 10 02:27:23 2026";

    /// Reports the pids given here as running, with a start time the fixtures also use.
    fn fake_process_starts(live_process_ids: Vec<u32>) -> HashMap<u32, String> {
        live_process_ids
            .into_iter()
            .map(|process_id| (process_id, FAKE_PROCESS_START.to_string()))
            .collect()
    }

    #[derive(Debug, PartialEq, Eq)]
    enum SentInput {
        Text { pane_target: String, text: String },
        Escape { pane_target: String },
    }

    /// Serves the fixtures written under `home_directory` through the same registry and
    /// transcript functions the local source uses, with the process lookup injected:
    /// `ps` is never invoked, so nothing in these tests depends on real time passing or
    /// on a pid being alive. Sends are recorded rather than reaching a tmux pane.
    struct FakeSource {
        home_directory: PathBuf,
        process_starts: HashMap<u32, String>,
        sent_inputs: Mutex<Vec<SentInput>>,
        send_failure: Option<String>,
    }

    impl FakeSource {
        fn new(home_directory: PathBuf, process_starts: HashMap<u32, String>) -> Self {
            Self {
                home_directory,
                process_starts,
                sent_inputs: Mutex::new(Vec::new()),
                send_failure: None,
            }
        }

        fn failing_to_send(mut self, message: &str) -> Self {
            self.send_failure = Some(message.to_string());
            self
        }

        fn sent_input_count(&self) -> usize {
            self.sent_inputs
                .lock()
                .expect("reading the recorded sends")
                .len()
        }
    }

    impl SessionSource for FakeSource {
        fn list_sessions(&self, project_root: Option<PathBuf>) -> Task<Result<SessionListing>> {
            let registry_directory = self.home_directory.join(".claude").join("sessions");
            // Normalized on this side of the comparison the way the local source
            // normalizes what `ps` printed.
            let process_start_of_pid = |process_id: u32| {
                self.process_starts
                    .get(&process_id)
                    .map(|start_time| normalize_whitespace(start_time))
            };

            Task::ready(
                read_registrations(&registry_directory).map(|registrations| SessionListing {
                    sessions: visible_sessions(
                        registrations,
                        project_root.as_deref(),
                        now_millis(),
                        HEARTBEAT_CUTOFF_MILLIS,
                        &process_start_of_pid,
                    )
                    .into_iter()
                    .map(|session| SessionSummary {
                        transcript_path: find_transcript(&self.home_directory, &session.session_id),
                        session,
                    })
                    .collect(),
                    home_directory: self.home_directory.clone(),
                }),
            )
        }

        fn tail_transcript(
            &self,
            session_id: String,
            state: TailState,
        ) -> Task<Result<TailProgress>> {
            Task::ready(read_transcript_tail(
                &self.home_directory,
                &session_id,
                state,
            ))
        }

        fn read_file(&self, path: PathBuf, max_bytes: u64) -> Task<Result<FileContents>> {
            Task::ready(read_file_prefix(&path, max_bytes))
        }

        fn send_input(&self, pane_target: String, input: SessionInput) -> Task<Result<()>> {
            let sent_input = match input {
                SessionInput::Text(text) => SentInput::Text { pane_target, text },
                SessionInput::Escape => SentInput::Escape { pane_target },
            };
            self.sent_inputs
                .lock()
                .expect("recording the send")
                .push(sent_input);

            Task::ready(match &self.send_failure {
                Some(message) => Err(anyhow::anyhow!("{message}")),
                None => Ok(()),
            })
        }
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

        let source = FakeSource::new(home_directory.clone(), fake_process_starts(vec![11]));
        let sessions = source
            .list_sessions(None)
            .await
            .expect("scanning the registry")
            .sessions;
        let session_ids: Vec<&str> = sessions
            .iter()
            .map(|summary| summary.session.session_id.as_str())
            .collect();
        assert_eq!(
            session_ids,
            vec!["live-session"],
            "the malformed registration must be skipped without losing the good one"
        );

        std::fs::remove_dir_all(&home_directory).ok();
    }

    /// The paths a scan hands back were written by the machine it scanned, so the home
    /// directory they sit under has to come from that scan. Until one arrives there is no
    /// answer to give, and this machine's home is not one: on a remote project it belongs
    /// to a different machine.
    #[gpui::test]
    async fn test_the_scanned_home_directory_is_unknown_until_a_scan_arrives(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("scanned-home");
        let registry_directory = home_directory.join(".claude").join("sessions");
        write_file(
            registry_directory.join("41.json"),
            &registration_json(41, "listed-session"),
        );

        let store = cx.new(|cx| {
            ClaudeSessionStore::new(
                Arc::new(FakeSource::new(
                    home_directory.clone(),
                    fake_process_starts(vec![41]),
                )),
                None,
                cx,
            )
        });

        assert_eq!(
            store.read_with(cx, |store, _| store.home_directory().map(Path::to_path_buf)),
            None,
            "no scan has answered yet, so the store must report no home directory rather than a default"
        );

        cx.run_until_parked();

        assert_eq!(
            store.read_with(cx, |store, _| store.home_directory().map(Path::to_path_buf)),
            Some(home_directory.clone()),
            "the scan's home directory is the one the listed sessions' paths sit under"
        );
        assert_eq!(
            store.read_with(cx, |store, _| store.sessions().len()),
            1,
            "the same scan must still have listed the running session"
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

        let source = FakeSource::new(home_directory.clone(), fake_process_starts(vec![13]));
        let sessions = source
            .list_sessions(None)
            .await
            .expect("scanning the registry")
            .sessions;
        let session_ids: Vec<&str> = sessions
            .iter()
            .map(|summary| summary.session.session_id.as_str())
            .collect();
        assert_eq!(
            session_ids,
            vec!["idle-session"],
            "the process is running, so an hour-old status timestamp must not hide it"
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
            ClaudeSessionStore::new(
                Arc::new(FakeSource::new(
                    home_directory.clone(),
                    fake_process_starts(vec![31, 32]),
                )),
                None,
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
            ClaudeSessionStore::new(
                Arc::new(FakeSource::new(
                    home_directory.clone(),
                    fake_process_starts(vec![41]),
                )),
                None,
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

        let padded_starts = HashMap::from_iter([(21, "Thu Sep  1 02:27:23 2026".to_string())]);
        let source = FakeSource::new(home_directory.clone(), padded_starts);
        let sessions = source
            .list_sessions(None)
            .await
            .expect("scanning the registry")
            .sessions;
        assert_eq!(
            sessions.len(),
            1,
            "expected the session to be live, got {:?}",
            sessions
                .iter()
                .map(|summary| summary.session.process_start.clone())
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
            ClaudeSessionStore::new(
                Arc::new(FakeSource::new(
                    home_directory.clone(),
                    fake_process_starts(vec![process_id]),
                )),
                None,
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

    fn registration_json_with_tmux(process_id: u32, session_id: &str, tmux: &str) -> String {
        format!(
            r#"{{"pid":{process_id},"sessionId":"{session_id}","cwd":"/tmp",
"procStart":"{FAKE_PROCESS_START}","version":"2.1.267","kind":"interactive",
"tmux":"{tmux}","name":"live"}}"#
        )
    }

    /// A store reading through `source` with `process_id` selected, which is the state
    /// every send starts from.
    async fn store_with_selection(
        cx: &mut gpui::TestAppContext,
        source: Arc<FakeSource>,
        process_id: u32,
    ) -> gpui::Entity<ClaudeSessionStore> {
        let store = cx.new(|cx| ClaudeSessionStore::new(source, None, cx));
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        store.update(cx, |store, cx| store.select(process_id, cx));
        cx.run_until_parked();
        store
    }

    #[gpui::test]
    async fn test_send_input_without_a_tmux_pane_reaches_nothing(cx: &mut gpui::TestAppContext) {
        let home_directory = temporary_directory("send-without-tmux");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("51.json"),
            &registration_json(51, "no-tmux-session"),
        );

        let source = Arc::new(FakeSource::new(
            home_directory.clone(),
            fake_process_starts(vec![51]),
        ));
        let store = store_with_selection(cx, source.clone(), 51).await;

        store.update(cx, |store, cx| {
            store
                .send_input(SessionInput::Text("hello".to_string()), cx)
                .detach()
        });
        cx.run_until_parked();

        assert_eq!(
            source.sent_input_count(),
            0,
            "a session with no tmux field must not be sent to at all"
        );
        store.read_with(cx, |store, _| {
            assert_eq!(store.pane_target(), None);
            let error = store.error().cloned();
            assert!(
                error.is_some(),
                "the refusal must be reported, got no error at all"
            );
        });

        std::fs::remove_dir_all(&home_directory).ok();
    }

    #[gpui::test]
    async fn test_a_malformed_tmux_field_is_not_sent_to(cx: &mut gpui::TestAppContext) {
        let home_directory = temporary_directory("send-malformed-tmux");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("53.json"),
            // No pane id: everything that is not `%` followed by digits is rejected.
            &registration_json_with_tmux(53, "odd-tmux-session", "awp:@3.pane"),
        );

        let source = Arc::new(FakeSource::new(
            home_directory.clone(),
            fake_process_starts(vec![53]),
        ));
        let store = store_with_selection(cx, source.clone(), 53).await;

        store.update(cx, |store, cx| {
            store
                .send_input(SessionInput::Text("hello".to_string()), cx)
                .detach()
        });
        cx.run_until_parked();

        assert_eq!(
            source.sent_input_count(),
            0,
            "a tmux field that is not shaped like a pane id must not be sent to"
        );
        store.read_with(cx, |store, _| {
            assert_eq!(store.pane_target(), None);
            assert!(store.error().is_some(), "the refusal must be reported");
        });

        std::fs::remove_dir_all(&home_directory).ok();
    }

    #[gpui::test]
    async fn test_send_input_passes_the_sanitized_pane_target_and_text(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("send-with-tmux");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("52.json"),
            &registration_json_with_tmux(52, "tmux-session", "a session:@3.%7"),
        );

        let source = Arc::new(FakeSource::new(
            home_directory.clone(),
            fake_process_starts(vec![52]),
        ));
        let store = store_with_selection(cx, source.clone(), 52).await;

        store.read_with(cx, |store, _| {
            assert_eq!(
                store.pane_target(),
                Some("%7".to_string()),
                "only the pane id may be handed on, never the session name"
            );
        });

        store.update(cx, |store, cx| {
            store
                .send_input(
                    SessionInput::Text("first line\nsecond line".to_string()),
                    cx,
                )
                .detach()
        });
        store.update(cx, |store, cx| {
            store.send_input(SessionInput::Escape, cx).detach()
        });
        cx.run_until_parked();

        let sent_inputs = source.sent_inputs.lock().expect("reading the sends");
        assert_eq!(
            *sent_inputs,
            vec![
                SentInput::Text {
                    pane_target: "%7".to_string(),
                    text: "first line\nsecond line".to_string(),
                },
                SentInput::Escape {
                    pane_target: "%7".to_string(),
                },
            ]
        );
        drop(sent_inputs);
        store.read_with(cx, |store, _| {
            assert_eq!(
                store.error(),
                None,
                "a send that worked must not report anything"
            );
        });

        std::fs::remove_dir_all(&home_directory).ok();
    }

    #[gpui::test]
    async fn test_a_send_that_fails_is_reported(cx: &mut gpui::TestAppContext) {
        let home_directory = temporary_directory("send-failure");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("54.json"),
            &registration_json_with_tmux(54, "gone-pane-session", "awp:@3.%9"),
        );
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("55.json"),
            &registration_json(55, "another-session"),
        );

        let source = Arc::new(
            FakeSource::new(home_directory.clone(), fake_process_starts(vec![54, 55]))
                .failing_to_send("can't find pane %9"),
        );
        let store = store_with_selection(cx, source.clone(), 54).await;

        store.update(cx, |store, cx| {
            store
                .send_input(SessionInput::Text("hello".to_string()), cx)
                .detach()
        });
        cx.run_until_parked();

        assert_eq!(source.sent_input_count(), 1);
        store.read_with(cx, |store, _| {
            let error = store
                .error()
                .cloned()
                .expect("a failed send must be reported");
            assert!(
                error.contains("can't find pane %9"),
                "the failure from the far end must reach the panel, got {error:?}"
            );
        });

        // The polls run every second, so a failure the next scan clears is one the user
        // never gets to read.
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL * 3);
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            let error = store
                .error()
                .cloned()
                .expect("a scan that succeeded must not wipe the send failure");
            assert!(
                error.contains("can't find pane %9"),
                "the send failure must survive the polls, got {error:?}"
            );
        });

        // It belongs to the session it was sent to, though, and not to the next one.
        store.update(cx, |store, cx| store.select(55, cx));
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert_eq!(
                store.error(),
                None,
                "the failure must not follow the user to another session"
            );
        });

        std::fs::remove_dir_all(&home_directory).ok();
    }
}
