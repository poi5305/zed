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
    session_registry::{
        PendingQuestion, RegisteredSession, SubagentSummary, TailProgress, TailState,
        TranscriptSpend, attach_arguments, pane_target,
    },
    session_source::{QuestionState, SessionInput, SessionListing, SessionSource},
    transcript::{Transcript, parse_record},
};

const REGISTRY_POLL_INTERVAL: Duration = Duration::from_millis(1000);
const TRANSCRIPT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Slower than the transcript's: this shells out to tmux every time, and what it reads
/// is a terminal's screen, which a reader takes in whole rather than line by line.
const PANE_POLL_INTERVAL: Duration = Duration::from_millis(1000);

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

/// Which of the selected session's conversations is being followed.
///
/// A session's own conversation and each of its subagents' are separate files, and only
/// one of them is read at a time — the transcript the store holds is the one the panel
/// draws, so following a second file would mean interleaving two conversations into it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum TranscriptTarget {
    #[default]
    Main,
    Subagent {
        agent_id: String,
        /// `Some` for an agent belonging to a `Workflow` run, whose transcript lives one
        /// directory deeper. The same agent id appears in both layouts, so the run id is
        /// part of naming the file rather than extra detail about it.
        workflow_run_id: Option<String>,
    },
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
    /// What the last scan read off the end of each listed session's transcript, keyed by
    /// pid. Every listed session has one, not only the one being followed — a row says
    /// what its own session is carrying.
    session_spend: HashMap<u32, TranscriptSpend>,
    /// Where the last scan found each listed session's transcript, keyed by pid. Only the
    /// machine a session runs on can locate its transcript, so this comes from the scan
    /// rather than being looked for here.
    transcript_paths: HashMap<u32, PathBuf>,
    selected_process_id: Option<u32>,
    /// The subagent conversations of the selected session, as the last scan of it found
    /// them. Empty while nothing is selected: only the selected session is scanned, so
    /// there is nothing to report about the others.
    subagents: Vec<SubagentSummary>,
    transcript_target: TranscriptTarget,
    /// The `sessionId` both conversations are read under, or `None` while no session is
    /// selected — which is what keeps an unselected session from being read at all. It
    /// identifies the conversation rather than the process: Claude Code writes a new
    /// `sessionId` when the user runs `/clear`, and that is the signal to start over.
    followed_session_id: Option<String>,
    /// The selected session's own conversation, followed whether or not it is the one on
    /// screen. An agent's transcript never records the agent returning, so the session's
    /// own thread is the only place the state of an agent can be read off, and it is
    /// also where a `/clear` becomes visible.
    main_conversation: FollowedConversation,
    /// The agent conversation followed beside the session's own, and `None` whenever
    /// [`Self::transcript_target`] is [`TranscriptTarget::Main`].
    subagent_conversation: Option<FollowedConversation>,
    /// Counts every time either transcript has been thrown away and started again. Both
    /// conversations take their generation from this one counter, so that switching
    /// between them always reads as a change to a caller caching by record uuid: a
    /// counter per conversation would hand out the same number for both the first time
    /// each was started.
    transcript_resets: u64,
    error: Option<(ErrorSource, SharedString)>,
    /// The selected session's tmux pane as it was last seen, and `None` when there is no
    /// pane to read or its last read failed. Everything Claude Code draws without
    /// recording it — a prompt waiting for an answer, the messages queued behind the
    /// running turn, the status line — is only here.
    pane_contents: Option<SharedString>,
    /// The question the hook beside the selected session last recorded, whether or not it
    /// is still waiting to be answered. Whether it is, is not something this file can
    /// say — see [`Self::recorded_question`].
    recorded_question: Option<PendingQuestion>,
    /// Whether the machine the selected session runs on records questions at all. A
    /// machine without the hook reports no question whether or not one is waiting, which
    /// is a different thing from a session with nothing to answer.
    question_hook_installed: bool,
    /// What the selected session is saying right now. The transcript does not hold an
    /// assistant message until the turn carrying it is over, which on a long turn is tens
    /// of seconds after the words were on screen.
    live_message: Option<SharedString>,
    _registry_poll: Task<()>,
    _transcript_poll: Task<()>,
    _pane_poll: Task<()>,
}

/// One conversation being followed: the records absorbed so far, and where reading of
/// the file has got to. Recreated from scratch whenever the file it names changes —
/// another session selected, `/clear`, another agent opened.
struct FollowedConversation {
    transcript: Transcript,
    /// The value [`ClaudeSessionStore::transcript_resets`] had when this transcript was
    /// started, which is what a caller caching anything derived from it keys on.
    generation: u64,
    /// `None` until the file appears. A session that has only just started is listed
    /// with no transcript rather than hidden.
    path: Option<PathBuf>,
    offset: u64,
    /// Bytes read after the last newline. Kept as bytes because a read can stop in the
    /// middle of a multi-byte character, which cannot be held as a `String`.
    pending: Vec<u8>,
}

impl FollowedConversation {
    fn new(transcript: Transcript, generation: u64) -> Self {
        Self {
            transcript,
            generation,
            path: None,
            offset: 0,
            pending: Vec::new(),
        }
    }

    fn state(&self) -> TailState {
        TailState {
            path: self.path.clone(),
            offset: self.offset,
            pending: self.pending.clone(),
        }
    }
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
            session_spend: HashMap::default(),
            selected_process_id: None,
            subagents: Vec::new(),
            transcript_target: TranscriptTarget::Main,
            followed_session_id: None,
            main_conversation: FollowedConversation::new(Transcript::new(), 0),
            subagent_conversation: None,
            transcript_resets: 0,
            error: None,
            pane_contents: None,
            recorded_question: None,
            question_hook_installed: false,
            live_message: None,
            _registry_poll: Task::ready(()),
            _transcript_poll: Task::ready(()),
            _pane_poll: Task::ready(()),
        };
        this._registry_poll = this.spawn_registry_poll(cx);
        this._transcript_poll = this.spawn_transcript_poll(cx);
        this._pane_poll = this.spawn_pane_poll(cx);
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
        self.follow_this_sessions_main_conversation();
        cx.notify();
    }

    /// The conversation on screen: the selected session's own, or the agent's while one
    /// is being read.
    pub fn transcript(&self) -> &Transcript {
        &self.viewed_conversation().transcript
    }

    /// The selected session's own conversation, whichever one is on screen. Read by a
    /// caller asking something only the session's own thread answers — whether an agent
    /// it spawned has returned — rather than by one drawing the conversation.
    pub fn main_transcript(&self) -> &Transcript {
        &self.main_conversation.transcript
    }

    /// The subagent conversations of the selected session, ordered as the scan found
    /// them: by run id and then agent id, so rows do not move between polls.
    pub fn subagents(&self) -> &[SubagentSummary] {
        &self.subagents
    }

    pub fn transcript_target(&self) -> &TranscriptTarget {
        &self.transcript_target
    }

    /// Follows another of the selected session's conversations.
    ///
    /// An agent's conversation is started from scratch and read from the beginning of
    /// its file — the records of two agents must never end up in the same transcript,
    /// and a caller caching anything derived from it learns that its keys mean something
    /// else now from [`Self::transcript_generation`]. The session's own conversation is
    /// left alone, because it goes on being followed either way.
    pub fn select_transcript_target(&mut self, target: TranscriptTarget, cx: &mut Context<Self>) {
        if self.transcript_target == target {
            return;
        }
        self.transcript_target = target;
        self.subagent_conversation = match &self.transcript_target {
            TranscriptTarget::Main => None,
            // Read as one agent's own conversation: every line of such a file carries
            // `isSidechain`, so the reading that keeps an agent out of a main thread
            // would leave nothing of it at all.
            TranscriptTarget::Subagent { .. } => {
                let generation = self.start_transcript();
                Some(FollowedConversation::new(
                    Transcript::for_sidechain(),
                    generation,
                ))
            }
        };
        cx.notify();
    }

    /// The generation of the conversation on screen; see
    /// [`FollowedConversation::generation`].
    pub fn transcript_generation(&self) -> u64 {
        self.viewed_conversation().generation
    }

    /// The file the conversation on screen is being read from, or `None` while there is
    /// none — no selection, or a session that has only just started and has not written
    /// its transcript yet.
    pub fn transcript_path(&self) -> Option<&Path> {
        match &self.transcript_target {
            TranscriptTarget::Main => {
                let process_id = self.selected_process_id?;
                self.transcript_paths.get(&process_id).map(PathBuf::as_path)
            }
            // The scan reports the session's transcript, never an agent's, so the only
            // answer for an agent is the file its own reads have resolved.
            TranscriptTarget::Subagent { .. } => {
                self.subagent_conversation.as_ref()?.path.as_deref()
            }
        }
    }

    fn viewed_conversation(&self) -> &FollowedConversation {
        self.conversation_for(&self.transcript_target)
            .unwrap_or(&self.main_conversation)
    }

    fn conversation_for(&self, target: &TranscriptTarget) -> Option<&FollowedConversation> {
        match target {
            TranscriptTarget::Main => Some(&self.main_conversation),
            TranscriptTarget::Subagent { .. } => self.subagent_conversation.as_ref(),
        }
    }

    fn conversation_for_mut(
        &mut self,
        target: &TranscriptTarget,
    ) -> Option<&mut FollowedConversation> {
        match target {
            TranscriptTarget::Main => Some(&mut self.main_conversation),
            TranscriptTarget::Subagent { .. } => self.subagent_conversation.as_mut(),
        }
    }

    /// The generation to give a transcript that is being started, counted across both
    /// conversations so that no two of them ever carry the same one.
    fn start_transcript(&mut self) -> u64 {
        self.transcript_resets = self.transcript_resets.saturating_add(1);
        self.transcript_resets
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

    /// What the last scan read off the end of one session's transcript.
    pub fn session_spend(&self, process_id: u32) -> Option<TranscriptSpend> {
        self.session_spend.get(&process_id).copied()
    }

    /// The tmux pane the selected session can be typed into, or `None` when there is
    /// none — no selection, no `tmux` field, or a field that is not shaped like a pane
    /// id. The input UI is enabled by exactly this answer.
    pub fn pane_target(&self) -> Option<String> {
        let session = self.selected_session()?;
        pane_target(session.tmux_target.as_deref()?)
    }

    /// The tmux invocation that shows the selected session's own window in a terminal,
    /// or `None` when there is no selection or no pane to show. What it attaches is a
    /// mirror of the session rather than the session itself; see [`attach_arguments`].
    pub fn attach_arguments(&self) -> Option<Vec<String>> {
        let session = self.selected_session()?;
        attach_arguments(session.tmux_target.as_deref()?)
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

    fn selected_session_id(&self) -> Option<String> {
        self.selected_session()
            .map(|session| session.session_id.clone())
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

                // Only the selected session's agents are looked for. Listing them costs a
                // walk of that session's directory, and no other session's agents are on
                // screen to be worth one.
                let Ok(session_id) = this.read_with(cx, |this, _| this.selected_session_id())
                else {
                    break;
                };
                if let Some(session_id) = session_id {
                    let subagents = source.list_subagents(session_id.clone()).await;
                    if this
                        .update(cx, |this, cx| {
                            this.apply_subagent_scan(&session_id, subagents, cx)
                        })
                        .is_err()
                    {
                        break;
                    }
                }

                cx.background_executor().timer(REGISTRY_POLL_INTERVAL).await;
            }
        })
    }

    /// Reads what the selected session is showing rather than what it has written: its
    /// pane, and the question it is waiting on. Both are read on the same beat because
    /// both are the state of the session right now, and a question that appeared without
    /// the pane under it catching up would draw the two disagreeing.
    ///
    /// A failure to read the pane clears what was read rather than being reported as an
    /// error: the pane is a supplement to the conversation, and a session whose pane
    /// cannot be read is still readable.
    fn spawn_pane_poll(&self, cx: &mut Context<Self>) -> Task<()> {
        let source = self.source.clone();

        cx.spawn(async move |this, cx| {
            loop {
                // The only exit: the store has been dropped, so nothing is left to update.
                let Ok((pane_target, session_id)) = this.read_with(cx, |this, _| {
                    (this.pane_target(), this.selected_session_id())
                }) else {
                    break;
                };

                let contents = match pane_target.clone() {
                    Some(pane_target) => source.capture_pane(pane_target).await.ok(),
                    None => None,
                };
                let question = match session_id.clone() {
                    Some(session_id) => source.pending_question(session_id).await.log_err(),
                    None => None,
                };

                if this
                    .update(cx, |this, cx| {
                        if this.pane_target() != pane_target
                            || this.selected_session_id() != session_id
                        {
                            return;
                        }
                        this.apply_pane_contents(contents, cx);
                        this.apply_question_state(question, cx);
                    })
                    .is_err()
                {
                    break;
                }

                cx.background_executor().timer(PANE_POLL_INTERVAL).await;
            }
        })
    }

    /// A read that failed leaves what was last known standing: reporting "no hook" for a
    /// machine that could not be reached would offer to install one that is already
    /// there, and reporting "no question" would take a waiting question off the screen
    /// over a single dropped read.
    fn apply_question_state(&mut self, state: Option<QuestionState>, cx: &mut Context<Self>) {
        let Some(state) = state else {
            return;
        };
        let live_message = state
            .live_message
            .map(|message| SharedString::from(message.trim_end().to_string()))
            .filter(|message| !message.is_empty());
        if self.recorded_question == state.question
            && self.question_hook_installed == state.hook_installed
            && self.live_message == live_message
        {
            return;
        }
        self.recorded_question = state.question;
        self.question_hook_installed = state.hook_installed;
        self.live_message = live_message;
        cx.notify();
    }

    /// The question the hook last recorded for the selected session.
    ///
    /// Whether it is still waiting is not answered here: the hook records a question
    /// being asked and has no way to record it being answered, so the caller looks for
    /// the tool result that answers [`PendingQuestion::tool_use_id`] in the session's own
    /// conversation.
    pub fn recorded_question(&self) -> Option<&PendingQuestion> {
        self.recorded_question.as_ref()
    }

    pub fn question_hook_installed(&self) -> bool {
        self.question_hook_installed
    }

    /// What the selected session is saying right now, ahead of its transcript.
    ///
    /// Whether the transcript has caught up is not answered here: the words arrive in the
    /// conversation as an ordinary record, and the caller that draws both is the one that
    /// can tell they are the same words.
    pub fn live_message(&self) -> Option<&SharedString> {
        self.live_message.as_ref()
    }

    /// Installs the hook on the machine the sessions run on, reporting where the settings
    /// that were there were copied to.
    pub fn install_question_hook(&self) -> Task<Result<Option<PathBuf>>> {
        self.source.install_question_hook()
    }

    fn apply_pane_contents(&mut self, contents: Option<String>, cx: &mut Context<Self>) {
        let contents = contents.map(|contents| SharedString::from(contents.trim_end().to_string()));
        if self.pane_contents == contents {
            return;
        }
        self.pane_contents = contents;
        cx.notify();
    }

    /// The selected session's pane as the last read of it found it.
    pub fn pane_contents(&self) -> Option<&SharedString> {
        self.pane_contents.as_ref()
    }

    fn spawn_transcript_poll(&self, cx: &mut Context<Self>) -> Task<()> {
        let source = self.source.clone();

        cx.spawn(async move |this, cx| {
            'poll: loop {
                // The only exit: the store has been dropped, so nothing is left to update.
                let Ok(requests) = this.read_with(cx, |this, _| this.tail_read_requests()) else {
                    break;
                };

                for (target, session_id, state) in requests {
                    let progress = match &target {
                        TranscriptTarget::Main => {
                            source.tail_transcript(session_id.clone(), state).await
                        }
                        TranscriptTarget::Subagent {
                            agent_id,
                            workflow_run_id,
                        } => {
                            source
                                .tail_subagent(
                                    session_id.clone(),
                                    agent_id.clone(),
                                    workflow_run_id.clone(),
                                    state,
                                )
                                .await
                        }
                    };

                    if this
                        .update(cx, |this, cx| {
                            this.apply_tail_progress(&session_id, &target, progress, cx)
                        })
                        .is_err()
                    {
                        break 'poll;
                    }
                }

                cx.background_executor()
                    .timer(TRANSCRIPT_POLL_INTERVAL)
                    .await;
            }
        })
    }

    /// The read that would be issued for the conversation on screen, or `None` when no
    /// session is selected.
    ///
    /// Only a test asks: the poll issues both conversations' reads together through
    /// [`Self::tail_read_requests`], and this is how a test captures the read that is
    /// about to happen so that it can be landed by hand afterwards.
    #[cfg(test)]
    fn tail_read_request(&self) -> Option<(String, TailState)> {
        let session_id = self.followed_session_id.clone()?;
        Some((session_id, self.viewed_conversation().state()))
    }

    /// Every read one turn of the poll issues: the session's own conversation always,
    /// and the agent's as well while one is on screen. Each state is carried with the
    /// target it belongs to, so a read is never issued for one conversation with the
    /// other's offset — both start at zero, and nothing further down could tell them
    /// apart afterwards.
    fn tail_read_requests(&self) -> Vec<(TranscriptTarget, String, TailState)> {
        let Some(session_id) = self.followed_session_id.clone() else {
            return Vec::new();
        };

        let mut requests = vec![(
            TranscriptTarget::Main,
            session_id.clone(),
            self.main_conversation.state(),
        )];
        if let Some(conversation) = self
            .subagent_conversation
            .as_ref()
            .filter(|_| matches!(self.transcript_target, TranscriptTarget::Subagent { .. }))
        {
            requests.push((
                self.transcript_target.clone(),
                session_id,
                conversation.state(),
            ));
        }
        requests
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
        let mut session_spend = HashMap::default();
        let mut sessions = Vec::with_capacity(listing.sessions.len());
        for summary in listing.sessions {
            if let Some(transcript_path) = summary.transcript_path {
                transcript_paths.insert(summary.session.process_id, transcript_path);
            }
            if let Some(spend) = summary.spend {
                session_spend.insert(summary.session.process_id, spend);
            }
            sessions.push(summary.session);
        }

        // The scan runs every second whether or not anything moved, so the view is only
        // marked dirty when this one actually changed something.
        let mut changed = self.take_error_from(ErrorSource::Poll)
            || self.sessions != sessions
            || self.transcript_paths != transcript_paths
            || self.session_spend != session_spend
            || self.home_directory != home_directory;
        self.transcript_paths = transcript_paths;
        self.session_spend = session_spend;
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
                if self.followed_session_id.as_deref() != Some(session_id.as_str()) {
                    self.follow_this_sessions_main_conversation();
                    changed = true;
                }
            }
            None => {
                if self.selected_process_id.is_some() {
                    self.selected_process_id = None;
                    self.follow_this_sessions_main_conversation();
                    changed = true;
                }
            }
        }

        if changed {
            cx.notify();
        }
    }

    fn apply_subagent_scan(
        &mut self,
        session_id: &str,
        scan: Result<Vec<SubagentSummary>>,
        cx: &mut Context<Self>,
    ) {
        // The selection can move while the scan is in flight, and the agents of the
        // session that was left are not the ones to show for the one selected now.
        if self.selected_session_id().as_deref() != Some(session_id) {
            return;
        }

        let subagents = match scan {
            Ok(subagents) => subagents,
            Err(error) => {
                // The session list and the transcript are unaffected, so only the
                // message changes.
                self.error = Some((
                    ErrorSource::Poll,
                    format!("Listing subagents: {error:#}").into(),
                ));
                cx.notify();
                return;
            }
        };

        // The scan runs every second whether or not the session spawned anything, so the
        // view is only marked dirty when this one actually changed the list.
        if self.subagents != subagents {
            self.subagents = subagents;
            cx.notify();
        }
    }

    /// Starts over on the session's own conversation, dropping what was known about
    /// another one.
    ///
    /// Every reason reading is rebuilt from outside a deliberate switch of target — the
    /// user selecting another session, `/clear` giving this pid a new conversation, the
    /// selected session disappearing — invalidates the subagents as well: their ids name
    /// files under a session directory that is no longer the one being read.
    fn follow_this_sessions_main_conversation(&mut self) {
        self.transcript_target = TranscriptTarget::Main;
        self.subagents.clear();
        self.subagent_conversation = None;
        self.pane_contents = None;
        self.recorded_question = None;
        self.live_message = None;

        let generation = self.start_transcript();
        self.main_conversation = FollowedConversation::new(Transcript::new(), generation);
        self.followed_session_id = self
            .selected_process_id
            .and_then(|process_id| {
                self.sessions
                    .iter()
                    .find(|session| session.process_id == process_id)
            })
            .map(|session| session.session_id.clone());
    }

    fn apply_tail_progress(
        &mut self,
        session_id: &str,
        target: &TranscriptTarget,
        progress: Result<TailProgress>,
        cx: &mut Context<Self>,
    ) {
        // An agent's read belongs to the agent that was on screen when it was issued, and
        // once the user has moved on there is nothing left for it to be absorbed into:
        // every conversation starts its reading at offset zero, so the offset guard below
        // cannot tell one agent's read from another's. The session's own conversation is
        // followed whichever one is on screen, so a read of it is never stale this way.
        if matches!(target, TranscriptTarget::Subagent { .. }) && &self.transcript_target != target
        {
            return;
        }

        // The selection can change while a read is in flight, in which case this progress
        // describes a file the store is no longer following. Checked before the failure
        // below is reported: a read that failed for a conversation the store has stopped
        // following says nothing about the one it is following now, and the poll's error
        // is drawn under the session list.
        if self.followed_session_id.as_deref() != Some(session_id) {
            return;
        }

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

        let Some(conversation) = self.conversation_for(target) else {
            return;
        };

        // Reading can also be restarted from the beginning while a read is in flight —
        // by the selection moving away and back — and the same `sessionId` then names a
        // conversation that is being read again from the start. Absorbing a read that
        // began further into the file would move the offset past everything before it,
        // and those records would never be read.
        if progress.start_offset != conversation.offset {
            return;
        }

        let path_changed = conversation.path != progress.path;

        if progress.restarted {
            // Counted before the transcript is borrowed: the counter is shared by both
            // conversations and lives beside them.
            let generation = self.start_transcript();
            let transcript = match target {
                TranscriptTarget::Main => Transcript::new(),
                TranscriptTarget::Subagent { .. } => Transcript::for_sidechain(),
            };
            if let Some(conversation) = self.conversation_for_mut(target) {
                *conversation = FollowedConversation::new(transcript, generation);
            }
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

        let Some(conversation) = self.conversation_for_mut(target) else {
            return;
        };
        conversation.transcript.absorb(records);
        conversation.path = progress.path;
        conversation.offset = progress.offset;
        conversation.pending = progress.pending;

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
            HEARTBEAT_CUTOFF_MILLIS, SessionSummary, find_transcript, list_subagents,
            normalize_whitespace, now_millis, read_registrations, read_subagent_transcript_tail,
            read_transcript_tail, visible_sessions,
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
        Text {
            pane_target: String,
            text: String,
        },
        Escape {
            pane_target: String,
        },
        Key {
            pane_target: String,
            key: &'static str,
        },
    }

    /// What the store asked the source to read, recorded so that a test can tell a read
    /// of the main conversation from a read of one agent's.
    #[derive(Debug, PartialEq, Eq)]
    enum TailedConversation {
        Main {
            session_id: String,
        },
        Subagent {
            session_id: String,
            agent_id: String,
            workflow_run_id: Option<String>,
        },
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
        tailed_conversations: Mutex<Vec<TailedConversation>>,
    }

    impl FakeSource {
        fn new(home_directory: PathBuf, process_starts: HashMap<u32, String>) -> Self {
            Self {
                home_directory,
                process_starts,
                sent_inputs: Mutex::new(Vec::new()),
                send_failure: None,
                tailed_conversations: Mutex::new(Vec::new()),
            }
        }

        /// The reads the store has issued, most recent last.
        fn tailed_conversations(&self) -> std::sync::MutexGuard<'_, Vec<TailedConversation>> {
            self.tailed_conversations
                .lock()
                .expect("reading the recorded reads")
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
                        // These tests are about which conversation the store reads, and a
                        // session's spend is read from the end of a file they never write.
                        spend: None,
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
            self.tailed_conversations().push(TailedConversation::Main {
                session_id: session_id.clone(),
            });
            Task::ready(read_transcript_tail(
                &self.home_directory,
                &session_id,
                state,
            ))
        }

        fn list_subagents(&self, session_id: String) -> Task<Result<Vec<SubagentSummary>>> {
            // The listing does no awaiting of its own, so blocking on it here reads the
            // fixtures the same way the local source's background task would.
            Task::ready(smol::block_on(list_subagents(
                &self.home_directory,
                &session_id,
            )))
        }

        fn tail_subagent(
            &self,
            session_id: String,
            agent_id: String,
            workflow_run_id: Option<String>,
            state: TailState,
        ) -> Task<Result<TailProgress>> {
            self.tailed_conversations()
                .push(TailedConversation::Subagent {
                    session_id: session_id.clone(),
                    agent_id: agent_id.clone(),
                    workflow_run_id: workflow_run_id.clone(),
                });
            Task::ready(read_subagent_transcript_tail(
                &self.home_directory,
                &session_id,
                &agent_id,
                workflow_run_id.as_deref(),
                state,
            ))
        }

        fn pending_question(&self, session_id: String) -> Task<Result<QuestionState>> {
            Task::ready(Ok(QuestionState {
                question: crate::session_registry::read_pending_question(
                    &self.home_directory,
                    &session_id,
                )
                .unwrap_or(None),
                hook_installed: crate::session_registry::question_hook_is_installed(
                    &self.home_directory,
                ),
                live_message: crate::session_registry::read_live_message(
                    &self.home_directory,
                    &session_id,
                )
                .unwrap_or(None),
            }))
        }

        fn install_question_hook(&self) -> Task<Result<Option<PathBuf>>> {
            Task::ready(crate::session_registry::install_question_hook(
                &self.home_directory,
            ))
        }

        fn list_slash_commands(
            &self,
            project_root: Option<PathBuf>,
        ) -> Task<Result<Vec<crate::session_registry::SlashCommand>>> {
            Task::ready(Ok(crate::session_registry::list_slash_commands(
                &self.home_directory,
                project_root.as_deref(),
            )))
        }

        fn read_file(&self, path: PathBuf, max_bytes: u64) -> Task<Result<FileContents>> {
            Task::ready(read_file_prefix(&path, max_bytes))
        }

        fn capture_pane(&self, _pane_target: String) -> Task<Result<String>> {
            Task::ready(Ok(String::new()))
        }

        fn send_input(&self, pane_target: String, input: SessionInput) -> Task<Result<()>> {
            let sent_input = match input {
                SessionInput::Text(text) => SentInput::Text { pane_target, text },
                SessionInput::Escape => SentInput::Escape { pane_target },
                SessionInput::Key(key) => SentInput::Key {
                    pane_target,
                    key: key.tmux_name(),
                },
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
            store.apply_tail_progress(&session_id, &TranscriptTarget::Main, progress, cx)
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

    #[gpui::test]
    async fn selecting_another_session_clears_the_old_sessions_live_state(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("selection-live-state");
        let registry_directory = home_directory.join(".claude").join("sessions");
        write_file(
            registry_directory.join("31.json"),
            &registration_json(31, "first-session"),
        );
        write_file(
            registry_directory.join("32.json"),
            &registration_json(32, "second-session"),
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

        store.update(cx, |store, cx| {
            store.select(31, cx);
            store.apply_pane_contents(Some("the first pane".to_string()), cx);
            store.apply_question_state(
                Some(QuestionState {
                    question: Some(PendingQuestion {
                        tool_use_id: "call-from-first-session".to_string(),
                        questions: vec![crate::session_registry::Question {
                            header: "Choice".to_string(),
                            question: "Which one?".to_string(),
                            options: vec![crate::session_registry::QuestionOption {
                                label: "First".to_string(),
                                description: None,
                            }],
                            multi_select: false,
                        }],
                    }),
                    hook_installed: true,
                    live_message: Some("the first live message".to_string()),
                }),
                cx,
            );
            store.select(32, cx);
        });

        let actual = store.read_with(cx, |store, _| {
            (
                store.pane_contents().cloned(),
                store
                    .recorded_question()
                    .map(|question| question.tool_use_id.clone()),
                store.live_message().cloned(),
                store.question_hook_installed(),
            )
        });
        assert_eq!(
            actual,
            (None, None, None, true),
            "the selected session changed, so only the machine-wide hook state may remain; expected (None, None, None, true), got {actual:?}"
        );

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

    /// Switching session is the gesture the whole panel is built around, and the
    /// transcript the store follows has to switch with it: the new session's
    /// conversation, none of the previous one's, and whatever the new session writes
    /// after the switch.
    #[gpui::test]
    async fn test_selecting_another_session_follows_that_sessions_transcript(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("switch-session");
        let registry_directory = home_directory.join(".claude").join("sessions");
        write_file(
            registry_directory.join("61.json"),
            &registration_json(61, "session-a"),
        );
        write_file(
            registry_directory.join("62.json"),
            &registration_json(62, "session-b"),
        );
        write_transcript(
            &home_directory,
            "session-a",
            "{\"type\":\"user\",\"uuid\":\"a1\"}\n",
        );
        let transcript_b = write_transcript(
            &home_directory,
            "session-b",
            "{\"type\":\"user\",\"uuid\":\"b1\"}\n",
        );

        let store = cx.new(|cx| {
            ClaudeSessionStore::new(
                Arc::new(FakeSource::new(
                    home_directory.clone(),
                    fake_process_starts(vec![61, 62]),
                )),
                None,
                cx,
            )
        });
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();

        let uuids = |store: &ClaudeSessionStore| -> Vec<Option<String>> {
            store
                .transcript()
                .active_path()
                .iter()
                .map(|record| record.uuid.clone())
                .collect()
        };

        store.update(cx, |store, cx| store.select(61, cx));
        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL * 2);
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert_eq!(
                uuids(store),
                vec![Some("a1".to_string())],
                "the first selection has to be followed at all"
            );
        });

        store.update(cx, |store, cx| store.select(62, cx));
        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL * 2);
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert_eq!(
                uuids(store),
                vec![Some("b1".to_string())],
                "the second session's conversation must replace the first one's, not be \
                 appended to it"
            );
        });

        // The session that is now being read writes another turn.
        std::fs::write(
            &transcript_b,
            "{\"type\":\"user\",\"uuid\":\"b1\"}\n{\"type\":\"assistant\",\"uuid\":\"b2\",\"parentUuid\":\"b1\"}\n",
        )
        .expect("appending to the second session's transcript");
        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL * 2);
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert_eq!(
                uuids(store),
                vec![Some("b1".to_string()), Some("b2".to_string())],
                "the tail has to keep following the session that was switched to"
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

    /// Writes one subagent's sidecar and transcript into the layout the run id selects,
    /// and returns the transcript's path. The sidecar is what the scan is driven off, so
    /// an agent without one is not an agent.
    fn write_subagent(
        home_directory: &Path,
        session_id: &str,
        workflow_run_id: Option<&str>,
        agent_id: &str,
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
        write_file(
            directory.join(format!("agent-{agent_id}.meta.json")),
            r#"{"agentType":"general-purpose","description":"round 2","spawnDepth":1}"#,
        );
        let transcript_path = directory.join(format!("agent-{agent_id}.jsonl"));
        write_file(transcript_path.clone(), transcript_contents);
        transcript_path
    }

    const FLAT_AGENT_ID: &str = "a0000e203ec41bc73";
    const WORKFLOW_AGENT_ID: &str = "a1111e203ec41bc73";
    const WORKFLOW_RUN_ID: &str = "wf_b529a29d-562";

    /// The whole of what this round adds to the store: the selected session's agents are
    /// listed, one of them can be followed instead of the session's own conversation, and
    /// the switch is reversible without either conversation leaking into the other.
    #[gpui::test]
    async fn test_the_selected_sessions_subagents_are_listed_and_followed_on_their_own(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("subagents");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("71.json"),
            &registration_json(71, "agent-session"),
        );
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("72.json"),
            &registration_json(72, "other-session"),
        );
        write_transcript(
            &home_directory,
            "agent-session",
            "{\"type\":\"user\",\"uuid\":\"m1\"}\n",
        );
        write_transcript(
            &home_directory,
            "other-session",
            "{\"type\":\"user\",\"uuid\":\"o1\"}\n",
        );
        // Every line of a real subagent transcript carries `isSidechain`, which is what
        // keeps an agent's turns out of the session's own thread.
        let flat_agent_contents = "{\"type\":\"user\",\"isSidechain\":true,\"uuid\":\"f1\"}\n";
        let flat_agent_transcript = write_subagent(
            &home_directory,
            "agent-session",
            None,
            FLAT_AGENT_ID,
            flat_agent_contents,
        );
        write_subagent(
            &home_directory,
            "agent-session",
            Some(WORKFLOW_RUN_ID),
            WORKFLOW_AGENT_ID,
            "{\"type\":\"user\",\"isSidechain\":true,\"uuid\":\"w1\"}\n",
        );

        let source = Arc::new(FakeSource::new(
            home_directory.clone(),
            fake_process_starts(vec![71, 72]),
        ));
        let store = cx.new(|cx| ClaudeSessionStore::new(source.clone(), None, cx));
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            assert!(
                store.subagents().is_empty(),
                "no session is selected, so no session's agents may be scanned for"
            );
            assert_eq!(store.transcript_target(), &TranscriptTarget::Main);
        });

        store.update(cx, |store, cx| store.select(71, cx));
        // One more scan interval: the agents of a session are only looked for once it is
        // the selected one.
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();

        let uuids = |store: &ClaudeSessionStore| -> Vec<Option<String>> {
            store
                .transcript()
                .active_path()
                .iter()
                .map(|record| record.uuid.clone())
                .collect()
        };

        let generation_on_main = store.read_with(cx, |store, _| {
            let listed: Vec<(&str, Option<&str>)> = store
                .subagents()
                .iter()
                .map(|subagent| {
                    (
                        subagent.agent_id.as_str(),
                        subagent.workflow_run_id.as_deref(),
                    )
                })
                .collect();
            assert_eq!(
                listed,
                vec![
                    (FLAT_AGENT_ID, None),
                    (WORKFLOW_AGENT_ID, Some(WORKFLOW_RUN_ID)),
                ],
                "both layouts must be listed, ordered by run id and then agent id"
            );
            assert_eq!(
                store.subagents()[0].meta.agent_type,
                "general-purpose",
                "the sidecar's fields must reach the store, not just the agent's id"
            );
            assert_eq!(
                uuids(store),
                vec![Some("m1".to_string())],
                "the session's own conversation is what a fresh selection follows"
            );
            store.transcript_generation()
        });

        store.update(cx, |store, cx| {
            store.select_transcript_target(
                TranscriptTarget::Subagent {
                    agent_id: FLAT_AGENT_ID.to_string(),
                    workflow_run_id: None,
                },
                cx,
            )
        });
        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL * 2);
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            assert_eq!(
                store.transcript_target(),
                &TranscriptTarget::Subagent {
                    agent_id: FLAT_AGENT_ID.to_string(),
                    workflow_run_id: None,
                }
            );
            assert!(
                store.transcript_generation() > generation_on_main,
                "the transcript was thrown away, so a caller caching by record uuid has \
                 to be told; generation stayed at {}",
                store.transcript_generation()
            );
            let (session_id, state) = store
                .tail_read_request()
                .expect("the agent's conversation must be being followed");
            assert_eq!(session_id, "agent-session");
            assert_eq!(
                state.path.as_deref(),
                Some(flat_agent_transcript.as_path()),
                "the file being followed must be the agent's own"
            );
            assert_eq!(
                state.offset,
                flat_agent_contents.len() as u64,
                "the whole of the agent's transcript must have been read"
            );
            assert!(
                !store.transcript().is_empty(),
                "the agent's records must have been absorbed into the transcript the \
                 panel draws"
            );
            // Written when a sidechain record could not be drawn at all, so an empty
            // path stood in for "the session's own conversation is not here". The
            // agent's records are the conversation now, which is what the assertion
            // was always about.
            assert_eq!(
                uuids(store),
                vec![Some("f1".to_string())],
                "the agent's own records are the conversation, and the session's own \
                 must not be left underneath them"
            );
        });

        let last_read = source.tailed_conversations().pop();
        assert_eq!(
            last_read,
            Some(TailedConversation::Subagent {
                session_id: "agent-session".to_string(),
                agent_id: FLAT_AGENT_ID.to_string(),
                workflow_run_id: None,
            }),
            "the read has to go through the source's subagent tail, which is the only \
             one that names the agent's file from its ids"
        );

        store.update(cx, |store, cx| {
            store.select_transcript_target(TranscriptTarget::Main, cx)
        });
        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL * 2);
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            assert_eq!(store.transcript_target(), &TranscriptTarget::Main);
            assert_eq!(
                uuids(store),
                vec![Some("m1".to_string())],
                "going back must read the session's conversation again from the start"
            );
            assert_eq!(
                store.subagents().len(),
                2,
                "the agents of the session being read are still its agents"
            );
        });

        // Another session's agents are its own, and the agent ids of the one being left
        // name files under a directory that is no longer being read.
        store.update(cx, |store, cx| {
            store.select_transcript_target(
                TranscriptTarget::Subagent {
                    agent_id: WORKFLOW_AGENT_ID.to_string(),
                    workflow_run_id: Some(WORKFLOW_RUN_ID.to_string()),
                },
                cx,
            )
        });
        cx.run_until_parked();
        store.update(cx, |store, cx| store.select(72, cx));
        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL * 2);
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            assert_eq!(
                store.transcript_target(),
                &TranscriptTarget::Main,
                "selecting another session must go back to its own conversation rather \
                 than look for an agent id that belongs to the session just left"
            );
            assert!(
                store.subagents().is_empty(),
                "the agents listed must be the selected session's, and this scan has not \
                 happened yet; got {:?}",
                store
                    .subagents()
                    .iter()
                    .map(|subagent| subagent.agent_id.clone())
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                uuids(store),
                vec![Some("o1".to_string())],
                "the newly selected session's conversation is what must be followed"
            );
        });

        std::fs::remove_dir_all(&home_directory).ok();
    }

    /// The chips above the conversation say which of a session's agents are still
    /// working, and that answer is only in the session's own conversation: an agent's
    /// transcript never records the agent returning. So the session's own conversation is
    /// followed whether or not it is the one on screen, and the two must not leak into
    /// each other.
    #[gpui::test]
    async fn test_the_sessions_own_conversation_is_still_read_while_an_agents_is_on_screen(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("both-conversations");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("101.json"),
            &registration_json(101, "dual-session"),
        );
        let main_transcript = write_transcript(
            &home_directory,
            "dual-session",
            "{\"type\":\"user\",\"uuid\":\"m1\"}\n",
        );
        let agent_transcript = write_subagent(
            &home_directory,
            "dual-session",
            None,
            FLAT_AGENT_ID,
            "{\"type\":\"user\",\"isSidechain\":true,\"uuid\":\"f1\"}\n",
        );

        let source = Arc::new(FakeSource::new(
            home_directory.clone(),
            fake_process_starts(vec![101]),
        ));
        let store = cx.new(|cx| ClaudeSessionStore::new(source.clone(), None, cx));
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        store.update(cx, |store, cx| store.select(101, cx));
        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL * 2);
        cx.run_until_parked();

        let uuids = |transcript: &Transcript| -> Vec<Option<String>> {
            transcript
                .active_path()
                .iter()
                .map(|record| record.uuid.clone())
                .collect()
        };

        let generation_on_main = store.read_with(cx, |store, _| {
            assert_eq!(
                uuids(store.main_transcript()),
                vec![Some("m1".to_string())],
                "the session's own conversation must be the one reported while it is the \
                 one on screen"
            );
            store.transcript_generation()
        });

        store.update(cx, |store, cx| {
            store.select_transcript_target(
                TranscriptTarget::Subagent {
                    agent_id: FLAT_AGENT_ID.to_string(),
                    workflow_run_id: None,
                },
                cx,
            )
        });
        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL * 2);
        cx.run_until_parked();

        let generation_on_agent = store.read_with(cx, |store, _| {
            assert_eq!(
                uuids(store.transcript()),
                vec![Some("f1".to_string())],
                "an agent's transcript is a file of nothing but sidechain records, and \
                 reading it as a main thread leaves the panel blank"
            );
            assert_eq!(
                uuids(store.main_transcript()),
                vec![Some("m1".to_string())],
                "the session's own conversation must still be there to read the agents' \
                 states off"
            );
            assert_eq!(
                store.transcript_path(),
                Some(agent_transcript.as_path()),
                "the path reported must be the conversation on screen"
            );
            store.transcript_generation()
        });
        assert_ne!(
            generation_on_agent, generation_on_main,
            "a caller caching by record uuid must be told the keys mean something else now"
        );

        // Both files grow while the agent's conversation is the one on screen.
        std::fs::write(
            &main_transcript,
            "{\"type\":\"user\",\"uuid\":\"m1\"}\n\
             {\"type\":\"assistant\",\"uuid\":\"m2\",\"parentUuid\":\"m1\"}\n",
        )
        .expect("appending to the session's own transcript");
        std::fs::write(
            &agent_transcript,
            "{\"type\":\"user\",\"isSidechain\":true,\"uuid\":\"f1\"}\n\
             {\"type\":\"assistant\",\"isSidechain\":true,\"uuid\":\"f2\",\"parentUuid\":\"f1\"}\n",
        )
        .expect("appending to the agent's transcript");
        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL * 2);
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            assert_eq!(
                uuids(store.transcript()),
                vec![Some("f1".to_string()), Some("f2".to_string())],
                "the agent's conversation must keep being followed"
            );
            assert_eq!(
                uuids(store.main_transcript()),
                vec![Some("m1".to_string()), Some("m2".to_string())],
                "and the session's own must be followed at the same time, or a chip \
                 stays pulsing after the agent it names has returned"
            );
        });

        store.update(cx, |store, cx| {
            store.select_transcript_target(TranscriptTarget::Main, cx)
        });
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            assert_eq!(
                uuids(store.transcript()),
                vec![Some("m1".to_string()), Some("m2".to_string())],
                "going back must show what the session wrote while the agent was on \
                 screen, not read the file over again"
            );
            assert_eq!(
                store.transcript_path(),
                Some(main_transcript.as_path()),
                "and the path reported goes back with it"
            );
            assert_ne!(
                store.transcript_generation(),
                generation_on_agent,
                "the records behind the transcript have changed, so the generation must too"
            );
        });

        std::fs::remove_dir_all(&home_directory).ok();
    }

    /// A read of one conversation that lands after the user has switched to another must
    /// be dropped. Both tails start at offset zero, so the offset guard cannot tell them
    /// apart: without the target on the progress, the session's own lines are absorbed
    /// into the agent's transcript.
    #[gpui::test]
    async fn test_a_main_conversation_read_that_lands_after_switching_to_an_agent_is_dropped(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("subagent-stale-read");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("81.json"),
            &registration_json(81, "switching-session"),
        );
        write_transcript(
            &home_directory,
            "switching-session",
            "{\"type\":\"user\",\"uuid\":\"m1\"}\n",
        );
        write_subagent(
            &home_directory,
            "switching-session",
            None,
            FLAT_AGENT_ID,
            "{\"type\":\"user\",\"isSidechain\":true,\"uuid\":\"f1\"}\n",
        );

        let source = Arc::new(FakeSource::new(
            home_directory.clone(),
            fake_process_starts(vec![81]),
        ));
        let store = cx.new(|cx| ClaudeSessionStore::new(source, None, cx));
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        store.update(cx, |store, cx| store.select(81, cx));
        cx.run_until_parked();

        // The read the poll is about to perform on the session's own conversation,
        // captured while it is still in flight.
        let (session_id, state) = store
            .read_with(cx, |store, _| store.tail_read_request())
            .expect("the selected session must be tailed");
        let progress = read_transcript_tail(&home_directory, &session_id, state);
        assert_eq!(
            progress
                .as_ref()
                .map(|progress| progress.lines.len())
                .unwrap_or_default(),
            1,
            "the read has to have found the session's own line for the test to mean \
             anything"
        );

        // Meanwhile the user opens an agent's conversation.
        store.update(cx, |store, cx| {
            store.select_transcript_target(
                TranscriptTarget::Subagent {
                    agent_id: FLAT_AGENT_ID.to_string(),
                    workflow_run_id: None,
                },
                cx,
            )
        });

        // The read that was in flight before the switch now lands.
        store.update(cx, |store, cx| {
            store.apply_tail_progress(&session_id, &TranscriptTarget::Main, progress, cx)
        });

        store.read_with(cx, |store, _| {
            let uuids: Vec<Option<String>> = store
                .transcript()
                .active_path()
                .iter()
                .map(|record| record.uuid.clone())
                .collect();
            assert_eq!(
                uuids,
                Vec::<Option<String>>::new(),
                "the session's own line must not reach the agent's transcript"
            );
            assert!(
                store.transcript().is_empty(),
                "nothing of the session's own conversation may be absorbed once an \
                 agent's is the one being read"
            );
        });

        std::fs::remove_dir_all(&home_directory).ok();
    }

    /// A read that failed for a conversation the store has stopped following says
    /// nothing about the one it is following now. The poll's error is what the panel
    /// draws under the session list, so reporting it there blames the session on screen
    /// for a failure that belongs to the one the user left.
    #[gpui::test]
    async fn test_a_failed_read_of_a_session_that_was_left_is_not_reported(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("left-session-read-failure");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("91.json"),
            &registration_json(91, "the-session-being-read"),
        );
        write_transcript(
            &home_directory,
            "the-session-being-read",
            "{\"type\":\"user\",\"uuid\":\"m1\"}\n",
        );

        let source = Arc::new(FakeSource::new(
            home_directory.clone(),
            fake_process_starts(vec![91]),
        ));
        let store = cx.new(|cx| ClaudeSessionStore::new(source, None, cx));
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        store.update(cx, |store, cx| store.select(91, cx));
        cx.run_until_parked();

        // The read of the conversation the user has already left comes back a failure.
        store.update(cx, |store, cx| {
            store.apply_tail_progress(
                "the-session-the-user-left",
                &TranscriptTarget::Main,
                Err(anyhow!("the connection dropped")),
                cx,
            )
        });

        let reported = store.read_with(cx, |store, _| store.error().cloned());
        assert_eq!(
            reported, None,
            "nothing is wrong with the session on screen, but its list was given the \
             message {reported:?}"
        );

        // What the guard above must not kill: the failure of a read of the conversation
        // the store really is following is the one thing the message is for.
        store.update(cx, |store, cx| {
            store.apply_tail_progress(
                "the-session-being-read",
                &TranscriptTarget::Main,
                Err(anyhow!("the transcript could not be opened")),
                cx,
            )
        });
        let reported = store.read_with(cx, |store, _| store.error().cloned());
        assert_eq!(
            reported.as_deref(),
            Some("Reading transcript: the transcript could not be opened"),
            "the read that failed was of the conversation on screen, so the reader has \
             to be told, but they were told {reported:?}"
        );

        std::fs::remove_dir_all(&home_directory).ok();
    }

    /// A subagent conversation is something to read, never something to type into. What
    /// the user sends goes to the session's pane whichever conversation is on screen,
    /// because the session is the only thing there is to type into.
    #[gpui::test]
    async fn test_send_input_reaches_the_session_while_an_agents_conversation_is_read(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("subagent-send");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("91.json"),
            &registration_json_with_tmux(91, "typing-session", "a session:@3.%7"),
        );
        write_transcript(
            &home_directory,
            "typing-session",
            "{\"type\":\"user\",\"uuid\":\"m1\"}\n",
        );
        write_subagent(
            &home_directory,
            "typing-session",
            None,
            FLAT_AGENT_ID,
            "{\"type\":\"user\",\"isSidechain\":true,\"uuid\":\"f1\"}\n",
        );

        let source = Arc::new(FakeSource::new(
            home_directory.clone(),
            fake_process_starts(vec![91]),
        ));
        let store = store_with_selection(cx, source.clone(), 91).await;
        store.update(cx, |store, cx| {
            store.select_transcript_target(
                TranscriptTarget::Subagent {
                    agent_id: FLAT_AGENT_ID.to_string(),
                    workflow_run_id: None,
                },
                cx,
            )
        });
        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL * 2);
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            assert_eq!(
                store.pane_target(),
                Some("%7".to_string()),
                "the pane to type into is the session's, whatever is being read"
            );
        });

        store.update(cx, |store, cx| {
            store
                .send_input(SessionInput::Text("hello".to_string()), cx)
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
                    text: "hello".to_string(),
                },
                SentInput::Escape {
                    pane_target: "%7".to_string(),
                },
            ],
            "both sends must reach the session's pane, unchanged by the conversation \
             being read"
        );
        drop(sent_inputs);
        store.read_with(cx, |store, _| {
            assert_eq!(store.error(), None);
            assert_eq!(
                store.transcript_target(),
                &TranscriptTarget::Subagent {
                    agent_id: FLAT_AGENT_ID.to_string(),
                    workflow_run_id: None,
                },
                "sending must not move the user off the conversation being read"
            );
        });

        std::fs::remove_dir_all(&home_directory).ok();
    }
}
