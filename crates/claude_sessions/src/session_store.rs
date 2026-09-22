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
use collections::{HashMap, HashSet};
use gpui::{Context, FutureExt as _, SharedString, Task};
use serde_json::Value;
use util::ResultExt as _;

use crate::{
    Turn,
    live_state::{
        ChannelInboxEvent, LiveState, StatusSnapshot, parse_channel_inbox_line, parse_hook_event,
        timestamp_ms,
    },
    session_registry::{
        AgentListing, ChannelStatus, HookInstallOutcome, RegisteredSession, SubagentSummary,
        TailProgress, TailState, TranscriptSpend, now_millis,
    },
    session_source::{SessionListing, SessionSource},
    transcript::{Transcript, TranscriptRecord, parse_record},
};

const REGISTRY_POLL_INTERVAL: Duration = Duration::from_millis(1000);
const TRANSCRIPT_POLL_INTERVAL: Duration = Duration::from_millis(250);
const EVENTS_POLL_INTERVAL: Duration = Duration::from_millis(250);
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(1000);
const HOOKS_POLL_INTERVAL: Duration = Duration::from_secs(30);
const CHANNEL_INBOX_POLL_INTERVAL: Duration = Duration::from_millis(250);
const CHANNEL_STATUS_POLL_TICKS: u32 = 4;
const CHANNEL_INBOX_EVENTS_CAP: usize = 200;
/// How far apart the hook's `PermissionRequest` and the channel's `permission_request`
/// line may be written and still be one prompt; see `permission_line_is_for_the_prompt`.
/// Wide enough for a hook script that took its time on a loaded machine, far short of the
/// half hour a line can outlive the prompt it was written for.
const CHANNEL_PERMISSION_MATCH_WINDOW_MS: u64 = 5_000;
const CHANNEL_NOT_LOADED: &str = "Zed channel is not loaded in this session";
const SESSION_ENDED: &str = "session ended";
const INTERRUPT_REASON: &str = "Stop requested from Zed";
const INTERRUPT_CHANNEL_NOT_LOADED: &str = "Interrupt: channel not loaded";
const INTERRUPT_SERVER_TOO_OLD: &str = "Interrupt: channel server is too old";
const INTERRUPT_IDLE: &str = "Interrupt: session is idle";
const ENDED_KEEP: usize = 20;
const HIDDEN_POLL_INTERVAL: Duration = Duration::from_secs(5);
const TRANSCRIPT_STALE_MS: u64 = 5_000;
const EVENTS_STALE_MS: u64 = 5_000;
const STATUS_STALE_MS: u64 = 15_000;
const REGISTRY_STALE_MS: u64 = 5_000;

pub type SessionKey = String;

fn timeout_message(what: &str) -> String {
    format!(
        "{what} has not answered in {} s",
        REGISTRY_SCAN_TIMEOUT.as_secs()
    )
}

fn interrupt_refusal_message(channel: Option<&ChannelStatus>, turn: &Turn) -> &'static str {
    if !channel.is_some_and(|status| status.live) {
        INTERRUPT_CHANNEL_NOT_LOADED
    } else if !channel
        .is_some_and(|status| status.features.iter().any(|feature| feature == "interrupt"))
    {
        INTERRUPT_SERVER_TOO_OLD
    } else if !matches!(turn, Turn::Running { .. }) {
        INTERRUPT_IDLE
    } else {
        INTERRUPT_CHANNEL_NOT_LOADED
    }
}

fn can_interrupt_now(channel: Option<&ChannelStatus>, turn: &Turn) -> bool {
    channel.is_some_and(|status| {
        status.live && status.features.iter().any(|feature| feature == "interrupt")
    }) && matches!(turn, Turn::Running { .. })
}

/// How long one registry scan may be in flight before the poll stops waiting on it.
///
/// A scan that is neither answered nor refused is the one failure the store had no
/// answer for: `error` stays empty, `sessions` stays empty, and every view built on it
/// draws its own initial state as though it were the host's answer. The bound turns that
/// into a refusal, which the store already reports and the next turn of the poll retries.
/// Generous enough that a host merely slow to walk its registry still gets to answer.
pub(crate) const REGISTRY_SCAN_TIMEOUT: Duration = Duration::from_secs(20);

/// What put the message in [`ClaudeSessionStore::error`]. A poll that succeeds clears the
/// error a previous poll left behind, and that must not also wipe the report of a send
/// the user has just watched fail: the polls run every second, so a send failure with no
/// source of its own would be gone before it could be read.
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub(crate) enum ErrorSource {
    Poll,
    /// Kept apart from [`ErrorSource::Poll`] because the two polls run at the same
    /// interval: a registry scan that succeeds clears the source it shares, so a
    /// transcript that fails on every read was reported and wiped within the second,
    /// leaving a conversation that is empty for no stated reason.
    Transcript,
    Send,
    Subagents,
    Events,
    Status,
    Hooks,
    Channel,
    Agents,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndedReason {
    ProcessGone,
    Cleared,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EndedSession {
    pub session_id: String,
    pub name: Option<String>,
    pub ai_title: Option<String>,
    pub cwd: PathBuf,
    pub tmux: Option<String>,
    pub bridge_session_id: Option<String>,
    pub transcript_path: Option<PathBuf>,
    pub last_seen_ms: i64,
    pub ended_reason: EndedReason,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LiveSession {
    pub session: RegisteredSession,
    pub background: bool,
    pub agent_id: Option<String>,
    pub state: Option<String>,
    pub waiting_for: Option<String>,
}

impl std::ops::Deref for LiveSession {
    type Target = RegisteredSession;

    fn deref(&self) -> &RegisteredSession {
        &self.session
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SessionRow {
    Live(LiveSession),
    Ended(EndedSession),
}

impl SessionRow {
    pub fn session_id(&self) -> &str {
        match self {
            Self::Live(live) => live.session.session_id.as_str(),
            Self::Ended(ended) => ended.session_id.as_str(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoreClock {
    pub last_transcript_ok_ms: Option<u64>,
    pub last_events_ok_ms: Option<u64>,
    pub last_status_ok_ms: Option<u64>,
    pub last_registry_ok_ms: Option<u64>,
}

impl StoreClock {
    pub fn stale_for(&self, now_ms: u64) -> Option<(&'static str, u64)> {
        let mut most: Option<(&'static str, u64)> = None;
        let consider = |name, last: Option<u64>, budget: u64| {
            let last = last?;
            let elapsed = now_ms.saturating_sub(last);
            (elapsed > budget).then_some((name, elapsed.saturating_sub(budget)))
        };
        for candidate in [
            consider(
                "transcript",
                self.last_transcript_ok_ms,
                TRANSCRIPT_STALE_MS,
            ),
            consider("events", self.last_events_ok_ms, EVENTS_STALE_MS),
            consider("status", self.last_status_ok_ms, STATUS_STALE_MS),
            consider("registry", self.last_registry_ok_ms, REGISTRY_STALE_MS),
        ]
        .into_iter()
        .flatten()
        {
            if most.is_none_or(|(_, overdue)| candidate.1 > overdue) {
                most = Some(candidate);
            }
        }
        most
    }
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
    live_sessions: Vec<LiveSession>,
    ended: Vec<EndedSession>,
    agent_listings: Vec<AgentListing>,
    /// The home directory of the machine the sessions were scanned on, as that machine
    /// reported it. `None` until the first scan arrives: the paths that have to be
    /// checked against it come from that machine too, and this process's own home is not
    /// an answer about a remote one.
    home_directory: Option<PathBuf>,
    /// What the last scan read off the end of each listed session's transcript, keyed by
    /// session id. Every listed session has one, not only the one being followed — a row
    /// says what its own session is carrying.
    session_spend: HashMap<String, TranscriptSpend>,
    /// Where the last scan found each listed session's transcript, keyed by session id.
    /// Only the machine a session runs on can locate its transcript, so this comes from
    /// the scan rather than being looked for here.
    transcript_paths: HashMap<String, PathBuf>,
    selected: Option<SessionKey>,
    /// The `/clear` old→new sessionId pairs from scans that replaced the session the
    /// reader was on when each scan started. One scan records at most one pair; pairs
    /// from separate scans accumulate until the panel takes them.
    cleared_rebinds: Vec<(String, String)>,
    /// The subagent conversations of every listed session, as the last scan found them,
    /// keyed by session id. Every session is scanned rather than only the selected one,
    /// because the list draws each session's agents under it.
    ///
    /// Keyed by session rather than held as one list so that switching sessions shows the
    /// agents of the one switched to straight away, and so that an agent id is only ever
    /// read back under the session whose directory it names.
    subagents_by_session: HashMap<String, Vec<SubagentSummary>>,
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
    notice: Option<SharedString>,
    agents_unavailable_reason: Option<SharedString>,
    liveness_unavailable_reason: Option<SharedString>,
    clock: StoreClock,
    visible: bool,
    scan_in_flight: bool,
    registry_scan_started_ms: Option<u64>,
    live: LiveState,
    events: FollowedConversation,
    status: Option<StatusSnapshot>,
    status_updated_at: Option<i64>,
    hooks_are_installed: bool,
    channel: Option<ChannelStatus>,
    channel_checked_at_ms: Option<i64>,
    channel_inbox: FollowedConversation,
    channel_events: Vec<ChannelInboxEvent>,
    _registry_poll: Task<()>,
    _transcript_poll: Task<()>,
    _events_poll: Task<()>,
    _status_poll: Task<()>,
    _hooks_poll: Task<()>,
    _channel_poll: Task<()>,
    _outstanding_scan: Option<Task<()>>,
    _scan_watchdog: Option<Task<()>>,
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
        let now_ms = now_millis().max(0) as u64;
        let mut this = Self {
            source,
            project_root,
            live_sessions: Vec::new(),
            ended: Vec::new(),
            agent_listings: Vec::new(),
            home_directory: None,
            transcript_paths: HashMap::default(),
            session_spend: HashMap::default(),
            selected: None,
            cleared_rebinds: Vec::new(),
            subagents_by_session: HashMap::default(),
            transcript_target: TranscriptTarget::Main,
            followed_session_id: None,
            main_conversation: FollowedConversation::new(Transcript::new(), 0),
            subagent_conversation: None,
            transcript_resets: 0,
            error: None,
            notice: None,
            agents_unavailable_reason: None,
            liveness_unavailable_reason: None,
            clock: StoreClock {
                last_transcript_ok_ms: Some(now_ms),
                last_events_ok_ms: Some(now_ms),
                last_status_ok_ms: Some(now_ms),
                last_registry_ok_ms: Some(now_ms),
            },
            visible: true,
            scan_in_flight: false,
            registry_scan_started_ms: None,
            live: LiveState::default(),
            events: FollowedConversation::new(Transcript::new(), 0),
            status: None,
            status_updated_at: None,
            hooks_are_installed: false,
            channel: None,
            channel_checked_at_ms: None,
            channel_inbox: FollowedConversation::new(Transcript::new(), 0),
            channel_events: Vec::new(),
            _registry_poll: Task::ready(()),
            _transcript_poll: Task::ready(()),
            _events_poll: Task::ready(()),
            _status_poll: Task::ready(()),
            _hooks_poll: Task::ready(()),
            _channel_poll: Task::ready(()),
            _outstanding_scan: None,
            _scan_watchdog: None,
        };
        this._registry_poll = this.spawn_registry_poll(cx);
        this._transcript_poll = this.spawn_transcript_poll(cx);
        this._events_poll = this.spawn_events_poll(cx);
        this._status_poll = this.spawn_status_poll(cx);
        this._hooks_poll = this.spawn_hooks_poll(cx);
        this._channel_poll = this.spawn_channel_poll(cx);
        this
    }

    pub fn sessions(&self) -> &[LiveSession] {
        &self.live_sessions
    }

    pub fn ended_sessions(&self) -> &[EndedSession] {
        &self.ended
    }

    pub fn session_rows(&self) -> Vec<SessionRow> {
        let mut rows: Vec<SessionRow> = self
            .live_sessions
            .iter()
            .cloned()
            .map(SessionRow::Live)
            .collect();
        rows.extend(self.ended.iter().cloned().map(SessionRow::Ended));
        rows
    }

    /// The home directory of the machine the listed sessions run on, or `None` while no
    /// scan has reported one yet. A caller checking a path the scan led to against a
    /// boundary under the home directory has nothing to check it against until then.
    pub fn home_directory(&self) -> Option<&Path> {
        self.home_directory.as_deref()
    }

    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// The `/clear` old→new sessionId pair from the last scan that replaced the selected
    /// session, then `None` until another such scan. A scan that only changes which
    /// session is selected does not produce a pair.
    pub fn take_cleared_rebind(&mut self) -> Option<(String, String)> {
        self.take_cleared_rebinds().into_iter().next()
    }

    /// Every `/clear` old→new pair since the last take, in the order they were recorded.
    /// Empty when no selected session was replaced. One scan records at most one pair;
    /// pairs from separate scans that land before the panel takes still accumulate.
    pub fn take_cleared_rebinds(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.cleared_rebinds)
    }

    pub fn selected_process_id(&self) -> Option<u32> {
        let session_id = self.selected.as_deref()?;
        self.live_sessions
            .iter()
            .find(|session| session.session.session_id == session_id)
            .map(|session| session.session.process_id)
            .filter(|process_id| *process_id != 0)
    }

    pub fn selected_is_ended(&self) -> bool {
        let Some(session_id) = self.selected.as_deref() else {
            return false;
        };
        self.ended
            .iter()
            .any(|ended| ended.session_id == session_id)
    }

    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    /// The most overdue of the reads that were actually due, or `None` while none was.
    ///
    /// The transcript, events and status clocks belong to loops that only run while a
    /// session is being followed, and an ended row stops all three by design. Reporting
    /// them while nothing is followed lit the chip on an idle panel and left it lit.
    pub fn stale_for(&self, now_ms: u64) -> Option<(&'static str, u64)> {
        if self.followed_session_id.is_none() || self.selected_is_ended() {
            return StoreClock {
                last_registry_ok_ms: self.clock.last_registry_ok_ms,
                ..StoreClock::default()
            }
            .stale_for(now_ms);
        }
        self.clock.stale_for(now_ms)
    }

    pub fn notice(&self) -> Option<&SharedString> {
        self.notice.as_ref()
    }

    pub fn agents_unavailable_reason(&self) -> Option<&SharedString> {
        self.agents_unavailable_reason.as_ref()
    }

    pub fn liveness_unavailable_reason(&self) -> Option<&SharedString> {
        self.liveness_unavailable_reason.as_ref()
    }

    pub fn dismiss_ended(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let before = self.ended.len();
        self.ended.retain(|ended| ended.session_id != session_id);
        if self.ended.len() != before {
            if self.selected.as_deref() == Some(session_id) {
                self.selected = None;
                self.followed_session_id = None;
            }
            cx.notify();
        }
    }

    fn poll_interval(&self, visible_interval: Duration) -> Duration {
        if self.visible {
            visible_interval
        } else {
            HIDDEN_POLL_INTERVAL
        }
    }

    fn clock_ms() -> u64 {
        now_millis().max(0) as u64
    }

    /// Tells the store about a session a caller has already been told about, so that a
    /// view opened onto one can read it before its own first scan has landed — or
    /// without one ever landing.
    ///
    /// The seed is what the caller last saw rather than a second source of truth: the
    /// next scan that does land rebuilds `live_sessions` and `transcript_paths` from itself
    /// and the seed is gone with everything else the scan did not list. Must be called
    /// before [`Self::select`], which is what turns a session id into the conversation
    /// being followed.
    pub fn seed_session(
        &mut self,
        session: RegisteredSession,
        transcript_path: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if let Some(transcript_path) = transcript_path {
            self.transcript_paths
                .insert(session.session_id.clone(), transcript_path);
        }
        if !self
            .live_sessions
            .iter()
            .any(|listed| listed.session.session_id == session.session_id)
        {
            self.live_sessions.push(LiveSession {
                session,
                background: false,
                agent_id: None,
                state: None,
                waiting_for: None,
            });
        }
        cx.notify();
    }

    pub fn select(&mut self, session_id: impl Into<String>, cx: &mut Context<Self>) {
        let session_id = session_id.into();
        if self.selected.as_deref() == Some(session_id.as_str()) {
            return;
        }
        self.selected = Some(session_id);
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
        match self.selected_session() {
            Some(session) => self.subagents_of(&session.session_id),
            None => &[],
        }
    }

    /// The same for any listed session, which is what lets the list draw the agents of a
    /// session the reader has not selected. Empty for a session the last scan found none
    /// for, and for one it did not cover.
    pub fn subagents_of(&self, session_id: &str) -> &[SubagentSummary] {
        self.subagents_by_session
            .get(session_id)
            .map_or(&[], Vec::as_slice)
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
                // The scan's answer first, and the file this store has actually been
                // reading when the scan has not named one. Without the fallback a view
                // whose scan has not landed draws "no transcript yet" over a
                // conversation it has already read, which is the same answer an agent's
                // conversation has always given from the reads themselves.
                let session_id = self.selected.as_deref()?;
                self.transcript_paths
                    .get(session_id)
                    .map(PathBuf::as_path)
                    .or(self.main_conversation.path.as_deref())
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
    pub fn session_spend(&self, session_id: &str) -> Option<TranscriptSpend> {
        self.session_spend.get(session_id).copied()
    }

    pub fn channel_live(&self) -> bool {
        self.channel.as_ref().is_some_and(|status| status.live)
    }

    pub fn can_interrupt(&self) -> bool {
        can_interrupt_now(self.channel.as_ref(), &self.live.turn)
    }

    /// Why the Stop control is disabled, or `None` while interrupt is available.
    pub fn interrupt_disabled_reason(&self) -> Option<&'static str> {
        if self.can_interrupt() {
            None
        } else {
            Some(interrupt_refusal_message(
                self.channel.as_ref(),
                &self.live.turn,
            ))
        }
    }

    /// Sends SIGINT through the channel outbox. Refuses when [`Self::can_interrupt`] is
    /// false, so a click on a session that is idle, whose channel is not loaded, or whose
    /// server is too old to interrupt, is reported rather than written.
    pub fn interrupt(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        self.take_error_from(ErrorSource::Send);

        if !self.can_interrupt() {
            let message = interrupt_refusal_message(self.channel.as_ref(), &self.live.turn);
            self.error = Some((ErrorSource::Send, message.into()));
            cx.notify();
            return Task::ready(Err(anyhow!("{message}")));
        }

        let Some(claude_pid) = self.selected_process_id() else {
            let message = interrupt_refusal_message(self.channel.as_ref(), &self.live.turn);
            self.error = Some((ErrorSource::Send, message.into()));
            cx.notify();
            return Task::ready(Err(anyhow!("{message}")));
        };

        let send = self
            .source
            .channel_interrupt(claude_pid, INTERRUPT_REASON.to_string());
        cx.spawn(async move |this, cx| {
            let result = send.await.map(|_| ());
            if let Err(error) = &result {
                this.update(cx, |this, cx| {
                    this.error = Some((
                        ErrorSource::Send,
                        format!("Interrupting the session: {error:#}").into(),
                    ));
                    cx.notify();
                })
                .log_err();
            }
            result
        })
    }

    pub fn channel(&self) -> Option<&ChannelStatus> {
        self.channel.as_ref()
    }

    pub fn channel_checked_at_ms(&self) -> Option<i64> {
        self.channel_checked_at_ms
    }

    pub fn channel_events(&self) -> &[ChannelInboxEvent] {
        &self.channel_events
    }

    /// The newest unanswered channel `permission_request` written for the prompt the
    /// hook shows, and only while it shows one. Without an inbox line there is nothing
    /// to answer. The line's `tool_name` must equal the hook's `pending_permission.tool_name`
    /// (both are the CLI's own tool name); a mismatch is a different prompt.
    ///
    /// Residual: two prompts of the same tool name inside the 5 s window are still
    /// told apart only by order, because the channel's `permission_request` carries no
    /// `tool_use_id`.
    pub fn open_permission_request_id(&self) -> Option<String> {
        let Some(pending) = self.live.pending_permission.as_ref() else {
            return None;
        };
        let mut answered = HashSet::default();
        for event in self.channel_events.iter().rev() {
            match event {
                ChannelInboxEvent::PermissionAnswered { request_id, .. } => {
                    answered.insert(request_id.as_str());
                }
                ChannelInboxEvent::PermissionRequest {
                    at_ms,
                    request_id,
                    tool_name,
                    ..
                } if !answered.contains(request_id.as_str()) => {
                    if !request_id.is_empty()
                        && *tool_name == pending.tool_name
                        && Self::permission_line_is_for_the_prompt(*at_ms, pending.since_ms)
                    {
                        return Some(request_id.clone());
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Whether the line a channel wrote at `at_ms` is the prompt the hook recorded at
    /// `asked_at_ms`.
    ///
    /// The two records also share the CLI tool name, which
    /// [`Self::open_permission_request_id`] requires to match. They still do not share
    /// an id: the hook carries a `tool_use_id` the channel never sees, and the line
    /// carries a `request_id` the hook never sees. Times tell two prompts of the same
    /// tool apart — both clocks are the session host's, and an open line can outlive its
    /// prompt by half an hour, because the server writes `permission_answered` only for
    /// verdicts that came from Zed and holds a request it never heard the end of for its
    /// whole TTL.
    ///
    /// A time of `0` is a record that never carried one — a hook wrapper without
    /// `received_at_ms`, a line without `at_ms` — and cannot be placed against the
    /// other, so it is left to the ordering and the tool name.
    fn permission_line_is_for_the_prompt(at_ms: i64, asked_at_ms: i64) -> bool {
        if at_ms <= 0 || asked_at_ms <= 0 {
            return true;
        }
        at_ms.abs_diff(asked_at_ms) <= CHANNEL_PERMISSION_MATCH_WINDOW_MS
    }

    pub fn open_permission_inbox_event(&self) -> Option<&ChannelInboxEvent> {
        let request_id = self.open_permission_request_id()?;
        self.channel_events.iter().rev().find(|event| {
            matches!(
                event,
                ChannelInboxEvent::PermissionRequest { request_id: id, .. } if *id == request_id
            )
        })
    }

    /// Writes a message to the selected session's channel outbox.
    ///
    /// Returns the outbox file name so a pending send can be paired with the inbox
    /// `message_sent` event for that file.
    pub fn send_message(
        &mut self,
        content: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<String>> {
        self.take_error_from(ErrorSource::Send);

        if self.selected_is_ended() || self.live.session_ended {
            self.error = Some((ErrorSource::Send, SESSION_ENDED.into()));
            cx.notify();
            return Task::ready(Err(anyhow!("{SESSION_ENDED}")));
        }

        if !self.channel_live() {
            self.error = Some((ErrorSource::Send, CHANNEL_NOT_LOADED.into()));
            cx.notify();
            return Task::ready(Err(anyhow!("{CHANNEL_NOT_LOADED}")));
        }

        let Some(claude_pid) = self.selected_process_id() else {
            self.error = Some((ErrorSource::Send, CHANNEL_NOT_LOADED.into()));
            cx.notify();
            return Task::ready(Err(anyhow!("{CHANNEL_NOT_LOADED}")));
        };

        let send = self.source.channel_send_message(claude_pid, content);
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

    pub fn answer_permission(&mut self, allow: bool, cx: &mut Context<Self>) -> Task<Result<()>> {
        self.take_error_from(ErrorSource::Send);

        let Some(request_id) = self.open_permission_request_id() else {
            let message = "No permission request is waiting to be answered.";
            self.error = Some((ErrorSource::Send, message.into()));
            cx.notify();
            return Task::ready(Err(anyhow!("{message}")));
        };

        let Some(claude_pid) = self.selected_process_id() else {
            let message = "No permission request is waiting to be answered.";
            self.error = Some((ErrorSource::Send, message.into()));
            cx.notify();
            return Task::ready(Err(anyhow!("{message}")));
        };

        let send = self
            .source
            .channel_answer_permission(claude_pid, request_id, allow);
        cx.spawn(async move |this, cx| {
            let result = send.await.map(|_| ());
            if let Err(error) = &result {
                this.update(cx, |this, cx| {
                    this.error = Some((
                        ErrorSource::Send,
                        format!("Answering the permission: {error:#}").into(),
                    ));
                    cx.notify();
                })
                .log_err();
            }
            result
        })
    }

    /// The working directory of the selected session, on the machine it runs on. What
    /// `@` is resolved against, because a path is only a path where the session is.
    pub fn session_directory(&self) -> Option<PathBuf> {
        if let Some(live) = self.selected_live() {
            return Some(live.session.working_directory.clone());
        }
        self.selected_ended().map(|ended| ended.cwd.clone())
    }

    /// What a view opened onto the selected session needs in order to read it before a
    /// scan of its own has landed: the session as this store last saw it, and the file
    /// the scan found for it.
    ///
    /// Handed over rather than looked up again because the caller opening the view is
    /// the one holding the answer; see [`Self::seed_session`].
    pub fn session_seed(&self) -> Option<(RegisteredSession, Option<PathBuf>)> {
        if let Some(session) = self.selected_session() {
            let transcript_path = self.transcript_paths.get(&session.session_id).cloned();
            return Some((session.clone(), transcript_path));
        }
        let ended = self.selected_ended()?;
        let transcript_path = ended
            .transcript_path
            .clone()
            .or_else(|| self.transcript_paths.get(&ended.session_id).cloned());
        Some((
            RegisteredSession {
                process_id: 0,
                session_id: ended.session_id.clone(),
                working_directory: ended.cwd.clone(),
                process_start: String::new(),
                version: String::new(),
                kind: "interactive".to_string(),
                name: ended.name.clone(),
                status: None,
                updated_at: Some(ended.last_seen_ms),
                tmux_target: ended.tmux.clone(),
                bridge_session_id: ended.bridge_session_id.clone(),
            },
            transcript_path,
        ))
    }

    fn selected_live(&self) -> Option<&LiveSession> {
        let session_id = self.selected.as_deref()?;
        self.live_sessions
            .iter()
            .find(|session| session.session.session_id == session_id)
    }

    fn selected_ended(&self) -> Option<&EndedSession> {
        let session_id = self.selected.as_deref()?;
        self.ended
            .iter()
            .find(|ended| ended.session_id == session_id)
    }

    pub fn selected_session(&self) -> Option<&RegisteredSession> {
        self.selected_live().map(|live| &live.session)
    }

    /// The tmux invocation that shows the selected live session's window, or `None`
    /// when nothing live is selected or its `tmux` field is not a pane tmux can attach.
    pub fn attach_arguments(&self) -> Option<Vec<String>> {
        let session = self.selected_session()?;
        let tmux_target = session.tmux_target.as_deref()?;
        crate::session_registry::attach_arguments(tmux_target)
    }

    fn spawn_registry_poll(&self, cx: &mut Context<Self>) -> Task<()> {
        let source = self.source.clone();
        let project_root = self.project_root.clone();

        cx.spawn(async move |this, cx| {
            let mut agents_tick = 0u32;
            loop {
                let Ok((in_flight, visible_interval)) = this.read_with(cx, |this, _| {
                    (
                        this.scan_in_flight,
                        this.poll_interval(REGISTRY_POLL_INTERVAL),
                    )
                }) else {
                    break;
                };

                if !in_flight
                    && this
                        .update(cx, |this, cx| {
                            this.scan_in_flight = true;
                            this.registry_scan_started_ms = Some(Self::clock_ms());
                            let source = source.clone();
                            let project_root = project_root.clone();
                            this._outstanding_scan = Some(cx.spawn(async move |this, cx| {
                                let scan = source.list_sessions(project_root).await;
                                if this
                                    .update(cx, |this, cx| {
                                        this.scan_in_flight = false;
                                        this.registry_scan_started_ms = None;
                                        this._scan_watchdog = None;
                                        this.apply_registry_scan(scan, cx);
                                    })
                                    .is_err()
                                {
                                    return;
                                }
                            }));
                            // Bound the wait on the executor so a test clock can fire it,
                            // without dropping the request itself.
                            this._scan_watchdog = Some(cx.spawn(async move |this, cx| {
                                cx.background_executor().timer(REGISTRY_SCAN_TIMEOUT).await;
                                if this
                                    .update(cx, |this, cx| {
                                        if this.scan_in_flight {
                                            this.error = Some((
                                                ErrorSource::Poll,
                                                timeout_message(
                                                    "the machine these sessions run on",
                                                )
                                                .into(),
                                            ));
                                            cx.notify();
                                        }
                                    })
                                    .is_err()
                                {
                                    return;
                                }
                            }));
                        })
                        .is_err()
                {
                    break;
                }

                let Ok(session_ids) = this.read_with(cx, |this, _| {
                    this.live_sessions
                        .iter()
                        .map(|session| session.session.session_id.clone())
                        .collect::<Vec<_>>()
                }) else {
                    break;
                };
                if !session_ids.is_empty() {
                    let subagents = source
                        .list_subagents_for_sessions(session_ids)
                        .with_timeout(REGISTRY_SCAN_TIMEOUT, cx.background_executor())
                        .await
                        .unwrap_or_else(|_| Err(anyhow!(timeout_message("listing subagents"))));
                    if this
                        .update(cx, |this, cx| this.apply_subagent_scan(subagents, cx))
                        .is_err()
                    {
                        break;
                    }
                }

                if agents_tick.is_multiple_of(3) {
                    let agents = source
                        .list_agents()
                        .with_timeout(REGISTRY_SCAN_TIMEOUT, cx.background_executor())
                        .await
                        .unwrap_or_else(|_| Err(anyhow!(timeout_message("listing agents"))));
                    if this
                        .update(cx, |this, cx| this.apply_agent_listings(agents, cx))
                        .is_err()
                    {
                        break;
                    }
                }
                agents_tick = agents_tick.saturating_add(1);

                cx.background_executor().timer(visible_interval).await;
            }
        })
    }

    fn spawn_events_poll(&self, cx: &mut Context<Self>) -> Task<()> {
        let source = self.source.clone();

        cx.spawn(async move |this, cx| {
            loop {
                let Ok(followed) = this.read_with(cx, |this, _| {
                    if this.selected_is_ended() {
                        return None;
                    }
                    this.followed_session_id
                        .clone()
                        .map(|session_id| (session_id, this.events.state()))
                }) else {
                    break;
                };

                if let Some((session_id, state)) = followed {
                    let start_offset = state.offset;
                    let progress = source
                        .tail_events(session_id.clone(), state)
                        .with_timeout(REGISTRY_SCAN_TIMEOUT, cx.background_executor())
                        .await
                        .unwrap_or_else(|_| Err(anyhow!(timeout_message("reading events"))));

                    if this
                        .update(cx, |this, cx| {
                            this.apply_events_progress(&session_id, start_offset, progress, cx)
                        })
                        .is_err()
                    {
                        break;
                    }
                }

                let Ok(interval) =
                    this.read_with(cx, |this, _| this.poll_interval(EVENTS_POLL_INTERVAL))
                else {
                    break;
                };
                cx.background_executor().timer(interval).await;
            }
        })
    }

    fn spawn_status_poll(&self, cx: &mut Context<Self>) -> Task<()> {
        let source = self.source.clone();

        cx.spawn(async move |this, cx| {
            loop {
                let Ok(session_id) = this.read_with(cx, |this, _| {
                    if this.selected_is_ended() {
                        None
                    } else {
                        this.followed_session_id.clone()
                    }
                }) else {
                    break;
                };

                let status = match session_id.clone() {
                    Some(session_id) => Some(
                        source
                            .read_status(session_id)
                            .with_timeout(REGISTRY_SCAN_TIMEOUT, cx.background_executor())
                            .await
                            .unwrap_or_else(|_| Err(anyhow!(timeout_message("reading status")))),
                    ),
                    None => None,
                };

                if this
                    .update(cx, |this, cx| {
                        // The selection can change while a read is in flight, in which
                        // case this status belongs to the session being left.
                        if this.followed_session_id != session_id {
                            return;
                        }
                        match status {
                            Some(Ok(Some(json))) => {
                                if let Some(snapshot) = StatusSnapshot::parse(&json) {
                                    this.take_error_from(ErrorSource::Status);
                                    this.clock.last_status_ok_ms = Some(Self::clock_ms());
                                    this.status = Some(snapshot);
                                    this.status_updated_at = Some(now_millis());
                                    cx.notify();
                                }
                            }
                            Some(Ok(None)) => {
                                this.take_error_from(ErrorSource::Status);
                                this.clock.last_status_ok_ms = Some(Self::clock_ms());
                            }
                            Some(Err(error)) => {
                                this.error = Some((
                                    ErrorSource::Status,
                                    format!("Reading status: {error:#}").into(),
                                ));
                                cx.notify();
                            }
                            None => {}
                        }
                    })
                    .is_err()
                {
                    break;
                }

                let Ok(interval) =
                    this.read_with(cx, |this, _| this.poll_interval(STATUS_POLL_INTERVAL))
                else {
                    break;
                };
                cx.background_executor().timer(interval).await;
            }
        })
    }

    fn spawn_hooks_poll(&self, cx: &mut Context<Self>) -> Task<()> {
        let source = self.source.clone();

        cx.spawn(async move |this, cx| {
            loop {
                let installed = source
                    .hooks_installed()
                    .with_timeout(REGISTRY_SCAN_TIMEOUT, cx.background_executor())
                    .await
                    .unwrap_or_else(|_| {
                        Err(anyhow!(timeout_message("reading hook install state")))
                    });
                if this
                    .update(cx, |this, cx| this.apply_hooks_installed(installed, cx))
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(HOOKS_POLL_INTERVAL).await;
            }
        })
    }

    fn apply_hooks_installed(&mut self, installed: Result<bool>, cx: &mut Context<Self>) {
        match installed {
            Ok(installed) => {
                let changed = self.take_error_from(ErrorSource::Hooks)
                    || self.hooks_are_installed != installed;
                self.hooks_are_installed = installed;
                if changed {
                    cx.notify();
                }
            }
            Err(error) => {
                self.error = Some((
                    ErrorSource::Hooks,
                    format!("Reading hook install state: {error:#}").into(),
                ));
                cx.notify();
            }
        }
    }

    fn apply_events_progress(
        &mut self,
        session_id: &str,
        start_offset: u64,
        progress: Result<TailProgress>,
        cx: &mut Context<Self>,
    ) {
        if self.followed_session_id.as_deref() != Some(session_id) {
            return;
        }
        let progress = match progress {
            Ok(progress) => {
                self.take_error_from(ErrorSource::Events);
                self.clock.last_events_ok_ms = Some(Self::clock_ms());
                progress
            }
            Err(error) => {
                self.error = Some((
                    ErrorSource::Events,
                    format!("Reading events: {error:#}").into(),
                ));
                cx.notify();
                return;
            }
        };
        if progress.start_offset != start_offset || progress.start_offset != self.events.offset {
            return;
        }

        let mut changed = false;
        if progress.restarted {
            self.live = LiveState::default();
            let generation = self.events.generation;
            self.events = FollowedConversation::new(Transcript::new(), generation);
            changed = true;
        }

        for line in &progress.lines {
            if let Some(event) = parse_hook_event(line) {
                self.live.apply(&event);
                changed = true;
            }
        }

        self.events.path = progress.path;
        self.events.offset = progress.offset;
        self.events.pending = progress.pending;

        if changed {
            cx.notify();
        }
    }

    pub fn live(&self) -> &LiveState {
        &self.live
    }

    pub fn status(&self) -> Option<&StatusSnapshot> {
        self.status.as_ref()
    }

    pub fn status_updated_at(&self) -> Option<i64> {
        self.status_updated_at
    }

    pub fn permission_mode(&self) -> Option<String> {
        self.live
            .permission_mode
            .clone()
            .or_else(|| self.main_transcript().permission_mode().map(str::to_string))
    }

    pub fn hooks_installed(&self) -> bool {
        self.hooks_are_installed
    }

    pub fn install_hooks(&mut self, cx: &mut Context<Self>) -> Task<Result<HookInstallOutcome>> {
        let source = self.source.clone();
        let install = source.install_hooks();
        cx.spawn(async move |this, cx| {
            let result = install
                .with_timeout(REGISTRY_SCAN_TIMEOUT, cx.background_executor())
                .await
                .unwrap_or_else(|_| Err(anyhow!(timeout_message("installing hooks"))));
            if result.is_ok() {
                let installed = source
                    .hooks_installed()
                    .with_timeout(REGISTRY_SCAN_TIMEOUT, cx.background_executor())
                    .await
                    .unwrap_or_else(|_| {
                        Err(anyhow!(timeout_message("refreshing hook install state")))
                    });
                this.update(cx, |this, cx| this.apply_hooks_installed(installed, cx))
                    .log_err();
            }
            result
        })
    }

    pub fn uninstall_hooks(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let source = self.source.clone();
        let uninstall = source.uninstall_hooks();
        cx.spawn(async move |this, cx| {
            let result = uninstall
                .with_timeout(REGISTRY_SCAN_TIMEOUT, cx.background_executor())
                .await
                .unwrap_or_else(|_| Err(anyhow!(timeout_message("uninstalling hooks"))));
            if result.is_ok() {
                let installed = source
                    .hooks_installed()
                    .with_timeout(REGISTRY_SCAN_TIMEOUT, cx.background_executor())
                    .await
                    .unwrap_or_else(|_| {
                        Err(anyhow!(timeout_message("refreshing hook install state")))
                    });
                this.update(cx, |this, cx| this.apply_hooks_installed(installed, cx))
                    .log_err();
            }
            result
        })
    }

    fn reset_live_follow(&mut self) {
        self.live = LiveState::default();
        self.events = FollowedConversation::new(Transcript::new(), self.transcript_resets);
        self.status = None;
        self.status_updated_at = None;
        self.channel = None;
        self.channel_checked_at_ms = None;
        self.channel_events.clear();
        self.channel_inbox = FollowedConversation::new(Transcript::new(), self.transcript_resets);
    }

    fn spawn_channel_poll(&self, cx: &mut Context<Self>) -> Task<()> {
        let source = self.source.clone();

        cx.spawn(async move |this, cx| {
            let mut status_tick = 0u32;
            loop {
                let Ok(followed) = this.read_with(cx, |this, _| {
                    this.selected_process_id().map(|claude_pid| {
                        (
                            claude_pid,
                            this.channel_inbox.state(),
                            this.channel_inbox.generation,
                        )
                    })
                }) else {
                    break;
                };

                if let Some((claude_pid, state, generation)) = followed {
                    if status_tick.is_multiple_of(CHANNEL_STATUS_POLL_TICKS) {
                        let status = source
                            .channel_status(claude_pid)
                            .with_timeout(REGISTRY_SCAN_TIMEOUT, cx.background_executor())
                            .await
                            .unwrap_or_else(|_| {
                                Err(anyhow!(timeout_message("reading channel status")))
                            });
                        if this
                            .update(cx, |this, cx| {
                                if this.selected_process_id() != Some(claude_pid) {
                                    return;
                                }
                                match status {
                                    Ok(status) => {
                                        this.take_error_from(ErrorSource::Channel);
                                        let changed = this.channel.as_ref() != Some(&status);
                                        this.channel = Some(status);
                                        this.channel_checked_at_ms = Some(now_millis());
                                        if changed {
                                            cx.notify();
                                        }
                                    }
                                    Err(error) => {
                                        this.error = Some((
                                            ErrorSource::Channel,
                                            format!("Reading channel: {error:#}").into(),
                                        ));
                                        cx.notify();
                                    }
                                }
                            })
                            .is_err()
                        {
                            break;
                        }
                    }

                    let start_offset = state.offset;
                    let progress = source
                        .tail_channel_inbox(claude_pid, state)
                        .with_timeout(REGISTRY_SCAN_TIMEOUT, cx.background_executor())
                        .await
                        .unwrap_or_else(|_| Err(anyhow!(timeout_message("reading channel inbox"))));
                    if this
                        .update(cx, |this, cx| {
                            this.apply_channel_inbox_progress(
                                claude_pid,
                                generation,
                                start_offset,
                                progress,
                                cx,
                            )
                        })
                        .is_err()
                    {
                        break;
                    }
                } else if this
                    .update(cx, |this, cx| {
                        if this.channel.is_some() || this.channel_checked_at_ms.is_some() {
                            this.channel = None;
                            this.channel_checked_at_ms = None;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }

                status_tick = status_tick.saturating_add(1);
                let Ok(interval) = this.read_with(cx, |this, _| {
                    this.poll_interval(CHANNEL_INBOX_POLL_INTERVAL)
                }) else {
                    break;
                };
                cx.background_executor().timer(interval).await;
            }
        })
    }

    fn apply_channel_inbox_progress(
        &mut self,
        claude_pid: u32,
        generation: u64,
        start_offset: u64,
        progress: Result<TailProgress>,
        cx: &mut Context<Self>,
    ) {
        if self.selected_process_id() != Some(claude_pid) {
            return;
        }
        if self.channel_inbox.generation != generation {
            return;
        }
        let progress = match progress {
            Ok(progress) => {
                self.take_error_from(ErrorSource::Channel);
                progress
            }
            Err(error) => {
                self.error = Some((
                    ErrorSource::Channel,
                    format!("Reading channel: {error:#}").into(),
                ));
                cx.notify();
                return;
            }
        };
        if progress.start_offset != start_offset
            || progress.start_offset != self.channel_inbox.offset
        {
            return;
        }

        let mut changed = false;
        if progress.restarted {
            self.channel_events.clear();
            let generation = self.channel_inbox.generation;
            self.channel_inbox = FollowedConversation::new(Transcript::new(), generation);
            changed = true;
        }

        for line in &progress.lines {
            if let Some(event) = parse_channel_inbox_line(line) {
                self.channel_events.push(event);
                changed = true;
            }
        }
        if self.channel_events.len() > CHANNEL_INBOX_EVENTS_CAP {
            let excess = self.channel_events.len() - CHANNEL_INBOX_EVENTS_CAP;
            self.channel_events.drain(..excess);
        }

        self.channel_inbox.path = progress.path;
        self.channel_inbox.offset = progress.offset;
        self.channel_inbox.pending = progress.pending;

        if changed {
            cx.notify();
        }
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
                        TranscriptTarget::Main => source
                            .tail_transcript(session_id.clone(), state)
                            .with_timeout(REGISTRY_SCAN_TIMEOUT, cx.background_executor())
                            .await
                            .unwrap_or_else(|_| {
                                Err(anyhow!(timeout_message("reading transcript")))
                            }),
                        TranscriptTarget::Subagent {
                            agent_id,
                            workflow_run_id,
                        } => source
                            .tail_subagent(
                                session_id.clone(),
                                agent_id.clone(),
                                workflow_run_id.clone(),
                                state,
                            )
                            .with_timeout(REGISTRY_SCAN_TIMEOUT, cx.background_executor())
                            .await
                            .unwrap_or_else(|_| {
                                Err(anyhow!(timeout_message("reading transcript")))
                            }),
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

                let Ok(interval) = this.read_with(cx, |this, _| {
                    if this.selected_is_ended() {
                        HIDDEN_POLL_INTERVAL
                    } else {
                        this.poll_interval(TRANSCRIPT_POLL_INTERVAL)
                    }
                }) else {
                    break;
                };
                cx.background_executor().timer(interval).await;
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

        self.clock.last_registry_ok_ms = Some(Self::clock_ms());
        let mut changed = self.take_error_from(ErrorSource::Poll);

        let SessionListing {
            sessions,
            home_directory,
            liveness_unavailable_reason,
        } = listing;

        let home_directory = Some(home_directory);
        let liveness_unavailable_reason = liveness_unavailable_reason.map(SharedString::from);
        let mut transcript_paths = HashMap::default();
        let mut session_spend = HashMap::default();
        let mut new_live: Vec<LiveSession> = Vec::with_capacity(sessions.len());
        for summary in sessions {
            let session_id = summary.session.session_id.clone();
            if let Some(transcript_path) = summary.transcript_path {
                transcript_paths.insert(session_id.clone(), transcript_path);
            }
            if let Some(spend) = summary.spend {
                session_spend.insert(session_id, spend);
            }
            new_live.push(LiveSession {
                session: summary.session,
                background: false,
                agent_id: None,
                state: None,
                waiting_for: None,
            });
        }

        let previous_live = std::mem::take(&mut self.live_sessions);
        // Every pid a session id was listed under, not one of them: `claude --resume` in
        // a second terminal leaves two live registrations carrying one sessionId, and a
        // single remembered pid would read one of the two as having replaced the other on
        // every scan for as long as both run.
        let mut previous_pids_by_session: HashMap<String, HashSet<u32>> = HashMap::default();
        for session in &previous_live {
            previous_pids_by_session
                .entry(session.session.session_id.clone())
                .or_default()
                .insert(session.session.process_id);
        }
        let previous_by_pid: HashMap<u32, String> = previous_live
            .iter()
            .map(|session| {
                (
                    session.session.process_id,
                    session.session.session_id.clone(),
                )
            })
            .collect();
        let new_ids: HashSet<String> = new_live
            .iter()
            .map(|session| session.session.session_id.clone())
            .collect();

        let mut rebound_selected = false;
        let mut cleared_selected = false;
        // `previous_by_pid` is one id per pid, so two replacements in one scan always
        // come from two pids. The running `self.selected` is reassigned onto the first
        // pid's new id, and comparing against it would record the second pid's clear
        // as if the reader had moved there.
        let selected_at_scan_start = self.selected.clone();

        for live in &new_live {
            if let Some(previous_pids) = previous_pids_by_session.get(&live.session.session_id)
                && !previous_pids.contains(&live.session.process_id)
                && self.selected.as_deref() == Some(live.session.session_id.as_str())
            {
                rebound_selected = true;
                self.notice =
                    Some(format!("Session rebound to pid {}", live.session.process_id).into());
                changed = true;
            }
            if let Some(old_session_id) = previous_by_pid.get(&live.session.process_id)
                && old_session_id != &live.session.session_id
            {
                if let Some(old) = previous_live
                    .iter()
                    .find(|session| session.session.session_id == *old_session_id)
                {
                    self.push_ended(self.ended_from_live(old, EndedReason::Cleared));
                }
                if !cleared_selected
                    && selected_at_scan_start.as_deref() == Some(old_session_id.as_str())
                {
                    cleared_selected = true;
                    self.cleared_rebinds
                        .push((old_session_id.clone(), live.session.session_id.clone()));
                    self.selected = Some(live.session.session_id.clone());
                }
                changed = true;
            }
        }

        for old in &previous_live {
            if new_ids.contains(&old.session.session_id) {
                continue;
            }
            if new_live
                .iter()
                .any(|live| live.session.process_id == old.session.process_id)
            {
                continue;
            }
            self.push_ended(self.ended_from_live(old, EndedReason::ProcessGone));
            changed = true;
        }

        let ended_before = self.ended.len();
        self.ended
            .retain(|ended| !new_ids.contains(&ended.session_id));
        if self.ended.len() != ended_before {
            changed = true;
        }

        for (session_id, path) in std::mem::take(&mut self.transcript_paths) {
            transcript_paths.entry(session_id).or_insert(path);
        }

        changed = changed
            || self.live_sessions != new_live
            || self.transcript_paths != transcript_paths
            || self.session_spend != session_spend
            || self.home_directory != home_directory
            || self.liveness_unavailable_reason != liveness_unavailable_reason;

        self.live_sessions = new_live;
        self.transcript_paths = transcript_paths;
        self.session_spend = session_spend;
        self.home_directory = home_directory;
        self.liveness_unavailable_reason = liveness_unavailable_reason;
        self.merge_agent_listings();

        if rebound_selected {
            // Same sessionId, new pid: keep transcript, events, LiveState.
        } else if cleared_selected {
            self.take_error_from(ErrorSource::Send);
            self.follow_this_sessions_main_conversation();
            changed = true;
        } else if let Some(session_id) = self.selected.clone() {
            if new_ids.contains(&session_id) {
                match self.followed_session_id.as_deref() {
                    None => {
                        self.followed_session_id = Some(session_id);
                        changed = true;
                    }
                    Some(followed) if followed != session_id => {
                        self.follow_this_sessions_main_conversation();
                        changed = true;
                    }
                    Some(_) => {}
                }
            } else if self
                .ended
                .iter()
                .any(|ended| ended.session_id == session_id)
            {
                self.take_error_from(ErrorSource::Send);
                self.reset_live_follow();
                self.followed_session_id = Some(session_id);
                changed = true;
            }
        }

        if changed {
            cx.notify();
        }
    }

    fn ended_from_live(&self, live: &LiveSession, reason: EndedReason) -> EndedSession {
        let ai_title = (self.followed_session_id.as_deref()
            == Some(live.session.session_id.as_str()))
        .then(|| {
            self.main_conversation
                .transcript
                .ai_title()
                .map(str::to_string)
        })
        .flatten();
        EndedSession {
            session_id: live.session.session_id.clone(),
            name: live.session.name.clone(),
            ai_title,
            cwd: live.session.working_directory.clone(),
            tmux: live.session.tmux_target.clone(),
            bridge_session_id: live.session.bridge_session_id.clone(),
            transcript_path: self.transcript_paths.get(&live.session.session_id).cloned(),
            last_seen_ms: now_millis(),
            ended_reason: reason,
        }
    }

    fn push_ended(&mut self, ended: EndedSession) {
        self.ended
            .retain(|existing| existing.session_id != ended.session_id);
        self.ended.insert(0, ended);
        self.ended.truncate(ENDED_KEEP);
    }

    fn apply_agent_listings(
        &mut self,
        listings: Result<Vec<AgentListing>>,
        cx: &mut Context<Self>,
    ) {
        match listings {
            Ok(listings) => {
                let cleared = self.take_error_from(ErrorSource::Agents);
                let reason_cleared = self.agents_unavailable_reason.take().is_some();
                self.agent_listings = listings;
                self.merge_agent_listings();
                if cleared || reason_cleared {
                    cx.notify();
                } else {
                    cx.notify();
                }
            }
            Err(error) => {
                let message = format!("{error:#}");
                if message.contains("has not answered") {
                    self.error = Some((
                        ErrorSource::Agents,
                        format!("Listing agents: {message}").into(),
                    ));
                } else {
                    self.agents_unavailable_reason = Some(message.into());
                }
                cx.notify();
            }
        }
    }

    fn merge_agent_listings(&mut self) {
        for listing in &self.agent_listings {
            if listing.kind == "background" {
                let Some(session_id) = listing.session_id.as_ref() else {
                    continue;
                };
                let include = listing.process_id.is_some()
                    || listing.state.as_deref() == Some("working")
                    || listing.state.as_deref() == Some("blocked");
                if !include {
                    continue;
                }
                if let Some(live) = self
                    .live_sessions
                    .iter_mut()
                    .find(|session| session.session.session_id == *session_id)
                {
                    live.background = true;
                    live.agent_id = listing.id.clone();
                    live.state = listing.state.clone();
                    live.waiting_for = listing.waiting_for.clone();
                    if let Some(process_id) = listing.process_id {
                        live.session.process_id = process_id;
                    }
                } else {
                    self.live_sessions.push(LiveSession {
                        session: RegisteredSession {
                            process_id: listing.process_id.unwrap_or(0),
                            session_id: session_id.clone(),
                            working_directory: listing
                                .working_directory
                                .clone()
                                .unwrap_or_default(),
                            process_start: String::new(),
                            version: String::new(),
                            kind: "background".to_string(),
                            name: listing.name.clone(),
                            status: listing.status.clone(),
                            updated_at: None,
                            tmux_target: None,
                            bridge_session_id: None,
                        },
                        background: true,
                        agent_id: listing.id.clone(),
                        state: listing.state.clone(),
                        waiting_for: listing.waiting_for.clone(),
                    });
                    self.ended.retain(|ended| ended.session_id != *session_id);
                }
            } else if listing.kind == "interactive"
                && let Some(session_id) = listing.session_id.as_ref()
                && let Some(live) = self
                    .live_sessions
                    .iter_mut()
                    .find(|session| session.session.session_id == *session_id)
            {
                if let Some(status) = listing.status.clone() {
                    live.session.status = Some(status);
                }
                live.waiting_for = listing.waiting_for.clone();
            }
        }
    }

    fn apply_subagent_scan(
        &mut self,
        scan: Result<HashMap<String, Vec<SubagentSummary>>>,
        cx: &mut Context<Self>,
    ) {
        let subagents = match scan {
            Ok(subagents) => {
                self.take_error_from(ErrorSource::Subagents);
                subagents
            }
            Err(error) => {
                self.error = Some((
                    ErrorSource::Subagents,
                    format!("Listing subagents: {error:#}").into(),
                ));
                cx.notify();
                return;
            }
        };

        if self.subagents_by_session != subagents {
            self.subagents_by_session = subagents;
            cx.notify();
        }
    }

    /// Starts over on the session's own conversation, dropping what was known about
    /// another one.
    ///
    /// The agent conversation being followed goes with it: its ids name files under a
    /// session directory that is no longer the one being read. What the scan found is
    /// keyed by session and so stays — the session switched away from still has the
    /// agents it had, and the one switched to can draw its own without waiting a poll.
    fn follow_this_sessions_main_conversation(&mut self) {
        self.transcript_target = TranscriptTarget::Main;
        self.subagent_conversation = None;
        self.reset_live_follow();

        let generation = self.start_transcript();
        self.main_conversation = FollowedConversation::new(Transcript::new(), generation);
        self.followed_session_id = self.selected.clone();

        // The loops that only run while a session is followed start counting from here,
        // rather than from wherever they were left while nothing was.
        let now_ms = Self::clock_ms();
        self.clock.last_transcript_ok_ms = Some(now_ms);
        self.clock.last_events_ok_ms = Some(now_ms);
        self.clock.last_status_ok_ms = Some(now_ms);
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
                    ErrorSource::Transcript,
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

        // Cleared by a read that is actually absorbed rather than by any read that
        // succeeds, so that a stale one cannot report the transcript as readable again.
        let cleared_error = self.take_error_from(ErrorSource::Transcript);
        self.clock.last_transcript_ok_ms = Some(Self::clock_ms());

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

        {
            let Some(conversation) = self.conversation_for_mut(target) else {
                return;
            };
            conversation.transcript.absorb(records.clone());
            conversation.path = progress.path;
            conversation.offset = progress.offset;
            conversation.pending = progress.pending;
        }

        if matches!(target, TranscriptTarget::Main) {
            note_transcript_into_live(&mut self.live, &records);
        }

        if absorbed_any || progress.restarted || path_changed || cleared_error {
            cx.notify();
        }
    }
}

fn note_transcript_into_live(live: &mut LiveState, records: &[TranscriptRecord]) {
    for record in records {
        if record.record_type == "assistant"
            && let Some(timestamp) = record.raw.get("timestamp").and_then(Value::as_str)
            && let Some(millis) = timestamp_ms(timestamp)
        {
            live.note_transcript_assistant(millis);
        }

        if record.record_type == "user" && record_is_user_interrupt(record) {
            live.note_interrupted();
        }

        let Some(blocks) = record
            .raw
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            if let Some(tool_use_id) = block.get("tool_use_id").and_then(Value::as_str) {
                live.note_tool_result(tool_use_id);
            }
        }
    }
}

fn record_is_user_interrupt(record: &TranscriptRecord) -> bool {
    if record
        .raw
        .get("interruptedMessageId")
        .and_then(Value::as_str)
        .is_some_and(|message_id| !message_id.is_empty())
    {
        return true;
    }

    let Some(visible) = user_interrupt_text(&record.raw) else {
        return false;
    };
    let trimmed = visible.trim();
    trimmed == "[Request interrupted by user]"
        || trimmed == "[Request interrupted by user for tool use]"
}

fn user_interrupt_text(raw: &Value) -> Option<String> {
    match raw.get("message")?.get("content")? {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => Some(
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
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
            HEARTBEAT_CUTOFF_MILLIS, SessionSummary, channel_answer_permission, channel_interrupt,
            channel_send_message, channel_status, find_transcript, install_zed_hooks,
            list_subagents, list_subagents_for_sessions, normalize_whitespace, now_millis,
            read_channel_inbox_tail, read_events_tail, read_registrations, read_session_status,
            read_subagent_transcript_tail, read_transcript_tail, visible_sessions,
            zed_hooks_installed,
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

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum SentChannel {
        Message { content: String },
        Permission { request_id: String, allow: bool },
        Interrupt { reason: String },
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
    /// on a pid being alive. Channel writes are recorded in memory and also hit the
    /// real files under `home_directory`.
    struct FakeSource {
        home_directory: PathBuf,
        process_starts: HashMap<u32, String>,
        sent_channel: Mutex<Vec<SentChannel>>,
        send_failure: Option<String>,
        tailed_conversations: Mutex<Vec<TailedConversation>>,
        remote: bool,
        list_sessions_calls: AtomicU32,
        list_agents_calls: AtomicU32,
        hooks_installed_calls: AtomicU32,
        fail_subagents: bool,
        hang_transcript: bool,
        executor: Option<gpui::BackgroundExecutor>,
    }

    impl FakeSource {
        fn new(home_directory: PathBuf, process_starts: HashMap<u32, String>) -> Self {
            Self {
                home_directory,
                process_starts,
                sent_channel: Mutex::new(Vec::new()),
                send_failure: None,
                tailed_conversations: Mutex::new(Vec::new()),
                remote: false,
                list_sessions_calls: AtomicU32::new(0),
                list_agents_calls: AtomicU32::new(0),
                hooks_installed_calls: AtomicU32::new(0),
                fail_subagents: false,
                hang_transcript: false,
                executor: None,
            }
        }

        fn failing_subagents(mut self) -> Self {
            self.fail_subagents = true;
            self
        }

        fn hanging_transcript(mut self, executor: gpui::BackgroundExecutor) -> Self {
            self.hang_transcript = true;
            self.executor = Some(executor);
            self
        }

        fn list_sessions_calls(&self) -> u32 {
            self.list_sessions_calls.load(Ordering::SeqCst)
        }

        fn list_agents_calls(&self) -> u32 {
            self.list_agents_calls.load(Ordering::SeqCst)
        }

        fn hooks_installed_calls(&self) -> u32 {
            self.hooks_installed_calls.load(Ordering::SeqCst)
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

        #[allow(dead_code)]
        fn remote(mut self) -> Self {
            self.remote = true;
            self
        }

        fn sent_channel_count(&self) -> usize {
            self.sent_channel
                .lock()
                .expect("reading the recorded sends")
                .len()
        }

        fn sent_channel(&self) -> Vec<SentChannel> {
            self.sent_channel
                .lock()
                .expect("reading the recorded sends")
                .clone()
        }
    }

    impl SessionSource for FakeSource {
        fn list_sessions(&self, project_root: Option<PathBuf>) -> Task<Result<SessionListing>> {
            self.list_sessions_calls.fetch_add(1, Ordering::SeqCst);
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
                    liveness_unavailable_reason: None,
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
            if self.hang_transcript {
                let executor = self
                    .executor
                    .clone()
                    .expect("a hanging transcript source holds an executor");
                return executor.spawn({
                    let executor = executor.clone();
                    async move {
                        executor.timer(Duration::from_secs(3600)).await;
                        Err(anyhow!("hung transcript"))
                    }
                });
            }
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

        fn list_subagents_for_sessions(
            &self,
            session_ids: Vec<String>,
        ) -> Task<Result<HashMap<String, Vec<SubagentSummary>>>> {
            if self.fail_subagents {
                return Task::ready(Err(anyhow!("subagents failed")));
            }
            Task::ready(smol::block_on(list_subagents_for_sessions(
                &self.home_directory,
                &session_ids,
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

        fn tail_events(&self, session_id: String, state: TailState) -> Task<Result<TailProgress>> {
            Task::ready(read_events_tail(&self.home_directory, &session_id, state))
        }

        fn read_status(&self, session_id: String) -> Task<Result<Option<String>>> {
            Task::ready(read_session_status(&self.home_directory, &session_id))
        }

        fn install_hooks(&self) -> Task<Result<HookInstallOutcome>> {
            Task::ready(install_zed_hooks(&self.home_directory))
        }

        fn hooks_installed(&self) -> Task<Result<bool>> {
            self.hooks_installed_calls.fetch_add(1, Ordering::SeqCst);
            Task::ready(Ok(zed_hooks_installed(&self.home_directory)))
        }

        fn list_session_files(
            &self,
            _directory: PathBuf,
            _query: String,
        ) -> Task<Result<Vec<String>>> {
            Task::ready(Ok(Vec::new()))
        }

        fn write_session_file(&self, _name: String, _contents: Vec<u8>) -> Task<Result<String>> {
            Task::ready(Err(anyhow::anyhow!("nothing is written in these tests")))
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

        fn read_attachment(
            &self,
            _session_id: String,
            path: PathBuf,
            max_bytes: u64,
        ) -> Task<Result<FileContents>> {
            Task::ready(read_file_prefix(&path, max_bytes))
        }

        fn channel_status(&self, claude_pid: u32) -> Task<Result<ChannelStatus>> {
            Task::ready(Ok(channel_status(
                &self.home_directory,
                claude_pid,
                now_millis(),
            )))
        }

        fn channel_send_message(&self, claude_pid: u32, content: String) -> Task<Result<String>> {
            self.sent_channel
                .lock()
                .expect("recording the send")
                .push(SentChannel::Message {
                    content: content.clone(),
                });
            if let Some(message) = &self.send_failure {
                return Task::ready(Err(anyhow::anyhow!("{message}")));
            }
            Task::ready(channel_send_message(
                &self.home_directory,
                claude_pid,
                &content,
            ))
        }

        fn channel_interrupt(&self, claude_pid: u32, reason: String) -> Task<Result<String>> {
            self.sent_channel
                .lock()
                .expect("recording the interrupt")
                .push(SentChannel::Interrupt {
                    reason: reason.clone(),
                });
            if let Some(message) = &self.send_failure {
                return Task::ready(Err(anyhow::anyhow!("{message}")));
            }
            Task::ready(channel_interrupt(&self.home_directory, claude_pid, &reason))
        }

        fn channel_answer_permission(
            &self,
            claude_pid: u32,
            request_id: String,
            allow: bool,
        ) -> Task<Result<String>> {
            self.sent_channel
                .lock()
                .expect("recording the send")
                .push(SentChannel::Permission {
                    request_id: request_id.clone(),
                    allow,
                });
            if let Some(message) = &self.send_failure {
                return Task::ready(Err(anyhow::anyhow!("{message}")));
            }
            Task::ready(channel_answer_permission(
                &self.home_directory,
                claude_pid,
                &request_id,
                allow,
            ))
        }

        fn tail_channel_inbox(
            &self,
            claude_pid: u32,
            state: TailState,
        ) -> Task<Result<TailProgress>> {
            Task::ready(read_channel_inbox_tail(
                &self.home_directory,
                claude_pid,
                state,
            ))
        }

        fn is_remote(&self) -> bool {
            self.remote
        }

        fn list_agents(&self) -> Task<Result<Vec<AgentListing>>> {
            self.list_agents_calls.fetch_add(1, Ordering::SeqCst);
            Task::ready(Ok(Vec::new()))
        }
    }

    fn select_listed(
        store: &gpui::Entity<ClaudeSessionStore>,
        process_id: u32,
        cx: &mut gpui::TestAppContext,
    ) {
        store.update(cx, |store, cx| {
            let session_id = store
                .sessions()
                .iter()
                .find(|session| session.process_id == process_id)
                .map(|session| session.session_id.clone())
                .expect("the process must be listed before it is selected");
            store.select(session_id, cx);
        });
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

        select_listed(&store, 31, cx);
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
        select_listed(&store, 32, cx);
        select_listed(&store, 31, cx);

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

        select_listed(&store, 31, cx);
        write_file(
            home_directory
                .join(".claude")
                .join("zed-events")
                .join("first-session.jsonl"),
            r#"{"received_at_ms":1,"event":{"hook_event_name":"MessageDisplay","session_id":"first-session","index":0,"delta":"the first live message","final":false}}
"#,
        );
        cx.executor().advance_clock(EVENTS_POLL_INTERVAL);
        cx.run_until_parked();

        let before = store.read_with(cx, |store, _| {
            store
                .live()
                .live_message
                .as_ref()
                .map(|message| message.text.clone())
        });
        assert_eq!(
            before.as_deref(),
            Some("the first live message"),
            "the events file of the selected session is what the store follows"
        );

        select_listed(&store, 32, cx);

        let actual = store.read_with(cx, |store, _| {
            (
                store
                    .live()
                    .live_message
                    .as_ref()
                    .map(|message| message.text.clone()),
                store
                    .live()
                    .pending_question
                    .as_ref()
                    .map(|question| question.tool_use_id.clone()),
                store.status().is_some(),
            )
        });
        assert_eq!(
            actual,
            (None, None, false),
            "the selected session changed, so live state and status belong to the one being left; expected (None, None, false), got {actual:?}"
        );

        std::fs::remove_dir_all(&home_directory).ok();
    }

    #[gpui::test]
    async fn clear_replaces_the_session_and_clears_a_send_error(cx: &mut gpui::TestAppContext) {
        let home_directory = temporary_directory("clear-send-error");
        let registration = home_directory
            .join(".claude")
            .join("sessions")
            .join("31.json");
        write_file(registration.clone(), &registration_json(31, "before-clear"));
        let store = cx.new(|cx| {
            ClaudeSessionStore::new(
                Arc::new(FakeSource::new(
                    home_directory.clone(),
                    fake_process_starts(vec![31]),
                )),
                None,
                cx,
            )
        });
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        select_listed(&store, 31, cx);

        let failed = store.update(cx, |store, cx| store.send_message("hello".to_string(), cx));
        assert!(failed.await.is_err());
        assert!(store.read_with(cx, |store, _| store.error().is_some()));

        write_file(registration, &registration_json(31, "after-clear"));
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            assert_eq!(store.selected(), Some("after-clear"));
            assert_eq!(
                store.error(),
                None,
                "/clear clears the old session's send error"
            );
        });
        std::fs::remove_dir_all(&home_directory).ok();
    }

    /// `/clear` is the same pid with a new sessionId. The panel takes that pair once so
    /// pending/failed rows can follow the new conversation; a later take, or a scan that
    /// did not replace the selected session, yields nothing.
    #[gpui::test]
    async fn take_cleared_rebind_reports_a_selected_clear_once(cx: &mut gpui::TestAppContext) {
        let home_directory = temporary_directory("cleared-rebind");
        let registration = home_directory
            .join(".claude")
            .join("sessions")
            .join("31.json");
        write_file(registration.clone(), &registration_json(31, "before-clear"));
        let store = cx.new(|cx| {
            ClaudeSessionStore::new(
                Arc::new(FakeSource::new(
                    home_directory.clone(),
                    fake_process_starts(vec![31]),
                )),
                None,
                cx,
            )
        });
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        select_listed(&store, 31, cx);
        store.update(cx, |store, _| {
            assert_eq!(
                store.take_cleared_rebind(),
                None,
                "listing and selecting a session is not a /clear"
            );
        });

        write_file(registration, &registration_json(31, "after-clear"));
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();

        store.update(cx, |store, _| {
            assert_eq!(
                store.take_cleared_rebind(),
                Some(("before-clear".into(), "after-clear".into())),
                "the scan that replaced the selected session must hand the panel the pair"
            );
            assert_eq!(
                store.take_cleared_rebind(),
                None,
                "the pair is consumed; a second take must not replay it"
            );
        });
        std::fs::remove_dir_all(&home_directory).ok();
    }

    /// Switching which listed session is selected, including by a scan that lists another
    /// live session without replacing the selected one, is not a `/clear`.
    #[gpui::test]
    async fn take_cleared_rebind_is_none_when_selection_merely_changes(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("cleared-rebind-selection");
        let registry_directory = home_directory.join(".claude").join("sessions");
        write_file(
            registry_directory.join("41.json"),
            &registration_json(41, "session-a"),
        );
        write_file(
            registry_directory.join("42.json"),
            &registration_json(42, "session-b"),
        );
        let store = cx.new(|cx| {
            ClaudeSessionStore::new(
                Arc::new(FakeSource::new(
                    home_directory.clone(),
                    fake_process_starts(vec![41, 42]),
                )),
                None,
                cx,
            )
        });
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        select_listed(&store, 41, cx);

        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        store.update(cx, |store, cx| {
            assert_eq!(
                store.take_cleared_rebind(),
                None,
                "a scan that still lists the selected session must not look like /clear"
            );
            store.select("session-b", cx);
            assert_eq!(
                store.take_cleared_rebind(),
                None,
                "the reader switching session is not a /clear"
            );
        });
        std::fs::remove_dir_all(&home_directory).ok();
    }

    /// One scan can replace two pids at once: pid41 A→B and pid42 B→C. Only the pid the
    /// reader was on yields a pair; selection follows that pid and does not jump to the
    /// other pid's new conversation.
    #[gpui::test]
    async fn one_scan_yields_one_pair_for_the_pid_the_reader_was_reading(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("cleared-rebind-one-scan");
        let registry_directory = home_directory.join(".claude").join("sessions");
        write_file(
            registry_directory.join("41.json"),
            &registration_json(41, "session-a"),
        );
        write_file(
            registry_directory.join("42.json"),
            &registration_json(42, "session-b"),
        );
        let store = cx.new(|cx| {
            ClaudeSessionStore::new(
                Arc::new(FakeSource::new(
                    home_directory.clone(),
                    fake_process_starts(vec![41, 42]),
                )),
                None,
                cx,
            )
        });
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        select_listed(&store, 41, cx);

        write_file(
            registry_directory.join("41.json"),
            &registration_json(41, "session-b"),
        );
        write_file(
            registry_directory.join("42.json"),
            &registration_json(42, "session-c"),
        );
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();

        store.update(cx, |store, _| {
            assert_eq!(
                store.selected(),
                Some("session-b"),
                "selection follows the pid the reader was on, not another pid's /clear"
            );
            assert_eq!(
                store.take_cleared_rebinds(),
                vec![("session-a".into(), "session-b".into())],
                "one scan records only the selected pid's pair"
            );
            assert!(
                store.take_cleared_rebinds().is_empty(),
                "the pair is consumed; a second take must not replay it"
            );
        });
        std::fs::remove_dir_all(&home_directory).ok();
    }

    /// Pairs from separate scans that land before the panel takes still accumulate: A→B
    /// then B→C, each with the new id selected between them, is the chain a Vec is for.
    #[gpui::test]
    async fn take_cleared_rebinds_accumulates_pairs_from_separate_scans(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("cleared-rebind-two-scans");
        let registry_directory = home_directory.join(".claude").join("sessions");
        let registration = registry_directory.join("41.json");
        write_file(registration.clone(), &registration_json(41, "session-a"));
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
        select_listed(&store, 41, cx);

        write_file(registration.clone(), &registration_json(41, "session-b"));
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        store.update(cx, |store, cx| {
            assert_eq!(
                store.selected(),
                Some("session-b"),
                "the first /clear must select the new id before the second scan"
            );
            store.select("session-b", cx);
        });

        write_file(registration, &registration_json(41, "session-c"));
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();

        store.update(cx, |store, _| {
            assert_eq!(
                store.take_cleared_rebinds(),
                vec![
                    ("session-a".into(), "session-b".into()),
                    ("session-b".into(), "session-c".into()),
                ],
                "two scans without a take in between keep both pairs"
            );
            assert!(
                store.take_cleared_rebinds().is_empty(),
                "the chain is consumed; a second take must not replay it"
            );
        });
        std::fs::remove_dir_all(&home_directory).ok();
    }

    /// Whether the hooks are installed is a fact about the machine the sessions run on,
    /// not about the session on screen. A panel that has listed sessions but has not been
    /// pointed at one still has to know, or it offers to install what is already there.
    #[gpui::test]
    async fn hooks_are_known_to_be_installed_without_a_selection(cx: &mut gpui::TestAppContext) {
        let home_directory = temporary_directory("hooks-without-selection");
        let registry_directory = home_directory.join(".claude").join("sessions");
        write_file(
            registry_directory.join("31.json"),
            &registration_json(31, "first-session"),
        );
        install_zed_hooks(&home_directory).expect("the hooks install into the temporary home");
        assert!(
            zed_hooks_installed(&home_directory),
            "the fixture itself must be a home the hooks are installed in"
        );

        let store = cx.new(|cx| {
            ClaudeSessionStore::new(
                Arc::new(FakeSource::new(
                    home_directory.clone(),
                    fake_process_starts(vec![31]),
                )),
                None,
                cx,
            )
        });
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        cx.executor().advance_clock(STATUS_POLL_INTERVAL);
        cx.run_until_parked();

        let (listed, selected, installed) = store.read_with(cx, |store, _| {
            (
                store.sessions().len(),
                store.selected().map(str::to_string),
                store.hooks_installed(),
            )
        });
        assert_eq!(
            (listed, selected.clone()),
            (1, None),
            "the fixture is a store with one listed session and nothing selected; got ({listed}, {selected:?})"
        );
        assert!(
            installed,
            "the hooks are installed in this home, so the store must say so with nothing selected; expected true, got {installed}"
        );

        std::fs::remove_dir_all(&home_directory).ok();
    }

    #[gpui::test]
    async fn hooks_installed_is_polled_at_most_once_per_thirty_seconds(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("hooks-poll-rate");
        let source = Arc::new(FakeSource::new(home_directory.clone(), HashMap::default()));
        let _store = cx.new(|cx| ClaudeSessionStore::new(source.clone(), None, cx));

        cx.run_until_parked();
        assert_eq!(source.hooks_installed_calls(), 1);
        cx.executor()
            .advance_clock(HOOKS_POLL_INTERVAL.saturating_sub(Duration::from_millis(1)));
        cx.run_until_parked();
        assert_eq!(
            source.hooks_installed_calls(),
            1,
            "the one-second status poll must not re-read hook files"
        );
        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        assert_eq!(source.hooks_installed_calls(), 2);

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
        select_listed(&store, 41, cx);
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

        select_listed(&store, 61, cx);
        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL * 2);
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert_eq!(
                uuids(store),
                vec![Some("a1".to_string())],
                "the first selection has to be followed at all"
            );
        });

        select_listed(&store, 62, cx);
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
        select_listed(&store, process_id, cx);
        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL * 2);
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert_eq!(store.selected(), Some("first-conversation"));
            assert_eq!(store.selected_process_id(), Some(process_id));
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
            assert_eq!(
                store
                    .ended_sessions()
                    .iter()
                    .map(|ended| (ended.session_id.as_str(), ended.ended_reason))
                    .collect::<Vec<_>>(),
                vec![("first-conversation", EndedReason::Cleared)],
                "the session id left behind by /clear is kept as an ended row"
            );
            assert_eq!(store.selected(), Some("second-conversation"));
        });

        // Dropping the store must stop both polls; the clock is advanced far past both
        // intervals afterwards to give a leaked task a chance to panic on a missing entity.
        drop(store);
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL * 10);
        cx.run_until_parked();

        std::fs::remove_dir_all(&home_directory).ok();
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
        select_listed(&store, process_id, cx);
        cx.run_until_parked();
        store
    }

    fn write_live_channel(home_directory: &Path, claude_pid: u32) {
        let now = now_millis();
        write_file(
            home_directory
                .join(".claude")
                .join("zed-channel")
                .join(claude_pid.to_string())
                .join("server.json"),
            &format!(
                r#"{{"pid":1,"claude_pid":{claude_pid},"started_at_ms":1,"heartbeat_at_ms":{now},"protocol":1}}"#
            ),
        );
    }

    fn pending_permission(tool_use_id: &str) -> crate::PermissionRequest {
        crate::PermissionRequest {
            tool_use_id: tool_use_id.to_string(),
            tool_name: "Bash".to_string(),
            tool_input: serde_json::json!({"command": "ls"}),
            since_ms: 1,
        }
    }

    async fn wait_for_channel_poll(cx: &mut gpui::TestAppContext) {
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_send_message_is_refused_when_the_channel_is_not_live(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("send-without-channel");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("51.json"),
            &registration_json(51, "no-channel-session"),
        );

        let source = Arc::new(FakeSource::new(
            home_directory.clone(),
            fake_process_starts(vec![51]),
        ));
        let store = store_with_selection(cx, source.clone(), 51).await;
        wait_for_channel_poll(cx).await;

        store.update(cx, |store, cx| {
            store.send_message("hello".to_string(), cx).detach()
        });
        cx.run_until_parked();

        assert_eq!(
            source.sent_channel_count(),
            0,
            "a session without a live channel must not be sent to"
        );
        store.read_with(cx, |store, _| {
            assert!(!store.channel_live());
            let error = store
                .error()
                .cloned()
                .expect("the refusal must be reported");
            assert!(error.contains(CHANNEL_NOT_LOADED), "got {error:?}");
        });

        std::fs::remove_dir_all(&home_directory).ok();
    }

    #[gpui::test]
    async fn test_send_message_reaches_the_channel_when_it_is_live(cx: &mut gpui::TestAppContext) {
        let home_directory = temporary_directory("send-with-channel");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("52.json"),
            &registration_json(52, "channel-session"),
        );
        write_live_channel(&home_directory, 52);

        let source = Arc::new(FakeSource::new(
            home_directory.clone(),
            fake_process_starts(vec![52]),
        ));
        let store = store_with_selection(cx, source.clone(), 52).await;
        wait_for_channel_poll(cx).await;

        store.read_with(cx, |store, _| {
            assert!(store.channel_live(), "the live server.json must be seen");
        });

        store.update(cx, |store, cx| {
            store
                .send_message("first line\nsecond line".to_string(), cx)
                .detach()
        });
        cx.run_until_parked();

        assert_eq!(
            source.sent_channel(),
            vec![SentChannel::Message {
                content: "first line\nsecond line".to_string(),
            }]
        );
        store.read_with(cx, |store, _| {
            assert_eq!(
                store.error(),
                None,
                "a send that worked must not report anything"
            );
        });

        std::fs::remove_dir_all(&home_directory).ok();
    }

    #[test]
    fn can_interrupt_requires_live_channel_interrupt_feature_and_a_running_turn() {
        let running = Turn::Running { since_ms: 1 };
        let idle = Turn::Idle;
        let live_interrupt = ChannelStatus {
            live: true,
            heartbeat_at_ms: Some(1),
            server_pid: Some(1),
            features: vec!["message".into(), "interrupt".into()],
        };
        let live_old_server = ChannelStatus {
            live: true,
            heartbeat_at_ms: Some(1),
            server_pid: Some(1),
            features: vec!["message".into()],
        };
        let not_live = ChannelStatus {
            live: false,
            heartbeat_at_ms: None,
            server_pid: None,
            features: vec!["interrupt".into()],
        };

        assert!(
            can_interrupt_now(Some(&live_interrupt), &running),
            "a live interrupt-capable server during a running turn can interrupt"
        );
        assert!(
            !can_interrupt_now(Some(&live_interrupt), &idle),
            "idle must not be interruptible"
        );
        assert!(
            !can_interrupt_now(Some(&live_old_server), &running),
            "a server without the interrupt feature is too old"
        );
        assert!(
            !can_interrupt_now(Some(&not_live), &running),
            "a channel that is not loaded cannot interrupt"
        );
    }

    #[gpui::test]
    async fn test_a_send_that_fails_is_reported(cx: &mut gpui::TestAppContext) {
        let home_directory = temporary_directory("send-failure");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("54.json"),
            &registration_json(54, "failing-session"),
        );
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("55.json"),
            &registration_json(55, "another-session"),
        );
        write_live_channel(&home_directory, 54);

        let source = Arc::new(
            FakeSource::new(home_directory.clone(), fake_process_starts(vec![54, 55]))
                .failing_to_send("disk is full"),
        );
        let store = store_with_selection(cx, source.clone(), 54).await;
        wait_for_channel_poll(cx).await;

        store.update(cx, |store, cx| {
            store.send_message("hello".to_string(), cx).detach()
        });
        cx.run_until_parked();

        assert_eq!(source.sent_channel_count(), 1);
        store.read_with(cx, |store, _| {
            let error = store
                .error()
                .cloned()
                .expect("a failed send must be reported");
            assert!(
                error.contains("disk is full"),
                "the failure from the far end must reach the panel, got {error:?}"
            );
        });

        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL * 3);
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            let error = store
                .error()
                .cloned()
                .expect("a scan that succeeded must not wipe the send failure");
            assert!(
                error.contains("disk is full"),
                "the send failure must survive the polls, got {error:?}"
            );
        });

        select_listed(&store, 55, cx);
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

    #[gpui::test]
    async fn test_open_permission_request_id_picks_the_newest_unanswered(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("open-permission");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("61.json"),
            &registration_json(61, "permission-session"),
        );
        let store = store_with_selection(
            cx,
            Arc::new(FakeSource::new(
                home_directory.clone(),
                fake_process_starts(vec![61]),
            )),
            61,
        )
        .await;

        store.update(cx, |store, _| {
            store.live.pending_permission = Some(pending_permission("toolu_1"));
            store.channel_events = vec![
                ChannelInboxEvent::PermissionRequest {
                    at_ms: 10,
                    request_id: "aaaaa".to_string(),
                    tool_name: "Bash".to_string(),
                    description: "first".to_string(),
                    input_preview: String::new(),
                },
                ChannelInboxEvent::PermissionRequest {
                    at_ms: 20,
                    request_id: "bbbbb".to_string(),
                    tool_name: "Bash".to_string(),
                    description: "second".to_string(),
                    input_preview: String::new(),
                },
            ];
        });
        store.read_with(cx, |store, _| {
            assert_eq!(
                store.open_permission_request_id().as_deref(),
                Some("bbbbb"),
                "the newest unanswered request is the one to answer"
            );
        });

        store.update(cx, |store, _| {
            store
                .channel_events
                .push(ChannelInboxEvent::PermissionAnswered {
                    at_ms: 21,
                    request_id: "bbbbb".to_string(),
                    behavior: "allow".to_string(),
                });
        });
        store.read_with(cx, |store, _| {
            assert_eq!(
                store.open_permission_request_id().as_deref(),
                Some("aaaaa"),
                "answering the newest leaves the earlier unanswered request"
            );
        });

        store.update(cx, |store, _| {
            store.live.pending_permission = None;
        });
        store.read_with(cx, |store, _| {
            assert_eq!(
                store.open_permission_request_id(),
                None,
                "without a hook pending permission there is nothing to answer"
            );
        });

        std::fs::remove_dir_all(&home_directory).ok();
    }

    /// The hook and the channel are two polls of their own, so the only thing that says
    /// an inbox line and the prompt on screen are the same prompt is that they were
    /// written at the same moment. The server writes `permission_answered` only for
    /// verdicts that came from Zed and keeps an open request for half an hour, so a
    /// prompt answered in the terminal leaves a line that never closes.
    #[gpui::test]
    async fn test_open_permission_request_id_ignores_a_line_from_another_prompt(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("stale-permission");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("63.json"),
            &registration_json(63, "stale-permission-session"),
        );
        let store = store_with_selection(
            cx,
            Arc::new(FakeSource::new(
                home_directory.clone(),
                fake_process_starts(vec![63]),
            )),
            63,
        )
        .await;

        let asked_at_ms = 1_700_000_000_000;
        store.update(cx, |store, _| {
            store.live.pending_permission = Some(crate::PermissionRequest {
                tool_use_id: "toolu_now".to_string(),
                tool_name: "Bash".to_string(),
                tool_input: serde_json::json!({"command": "rm -rf ."}),
                since_ms: asked_at_ms,
            });
            store.channel_events = vec![ChannelInboxEvent::PermissionRequest {
                at_ms: asked_at_ms - 600_000,
                request_id: "aaaaa".to_string(),
                tool_name: "Read".to_string(),
                description: "a prompt from ten minutes ago".to_string(),
                input_preview: String::new(),
            }];
        });
        store.read_with(cx, |store, _| {
            let open = store.open_permission_request_id();
            assert_eq!(
                open, None,
                "a line written ten minutes before the prompt on screen belongs to \
                 another prompt and must not be answerable; expected None, got {open:?}"
            );
        });

        // The line that does belong to this prompt is written within a second of the
        // hook, either side of it, and stays answerable.
        store.update(cx, |store, _| {
            store
                .channel_events
                .push(ChannelInboxEvent::PermissionRequest {
                    at_ms: asked_at_ms - 800,
                    request_id: "bbbbb".to_string(),
                    tool_name: "Bash".to_string(),
                    description: "this prompt".to_string(),
                    input_preview: String::new(),
                });
        });
        store.read_with(cx, |store, _| {
            let open = store.open_permission_request_id();
            assert_eq!(
                open.as_deref(),
                Some("bbbbb"),
                "the line for the prompt on screen must stay answerable; expected \
                 Some(\"bbbbb\"), got {open:?}"
            );
        });

        std::fs::remove_dir_all(&home_directory).ok();
    }

    /// The hook's tool_name and the inbox line's tool_name are both the CLI's own name.
    /// A line for a different tool is a different prompt, even when it was written inside
    /// the 5 s window — the channel's permission_request does not carry tool_use_id.
    #[gpui::test]
    async fn test_open_permission_request_id_ignores_a_line_for_a_different_tool(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("permission-tool-mismatch");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("65.json"),
            &registration_json(65, "permission-tool-mismatch-session"),
        );
        let store = store_with_selection(
            cx,
            Arc::new(FakeSource::new(
                home_directory.clone(),
                fake_process_starts(vec![65]),
            )),
            65,
        )
        .await;

        let asked_at_ms = 1_700_000_000_000;
        store.update(cx, |store, _| {
            store.live.pending_permission = Some(crate::PermissionRequest {
                tool_use_id: "toolu_bash".to_string(),
                tool_name: "Bash".to_string(),
                tool_input: serde_json::json!({"command": "rm -rf ."}),
                since_ms: asked_at_ms,
            });
            store.channel_events = vec![ChannelInboxEvent::PermissionRequest {
                at_ms: asked_at_ms - 400,
                request_id: "aaaaa".to_string(),
                tool_name: "Read".to_string(),
                description: "a different tool's prompt in the same second".to_string(),
                input_preview: String::new(),
            }];
        });
        store.read_with(cx, |store, _| {
            let open = store.open_permission_request_id();
            assert_eq!(
                open, None,
                "a line whose tool_name is not the hook's pending tool must not be \
                 answerable; expected None, got {open:?}"
            );
        });

        // A matching line still is, even when an unmatched newer line sits in front of it.
        store.update(cx, |store, _| {
            store.channel_events = vec![
                ChannelInboxEvent::PermissionRequest {
                    at_ms: asked_at_ms - 400,
                    request_id: "bbbbb".to_string(),
                    tool_name: "Bash".to_string(),
                    description: "this prompt".to_string(),
                    input_preview: String::new(),
                },
                ChannelInboxEvent::PermissionRequest {
                    at_ms: asked_at_ms - 200,
                    request_id: "aaaaa".to_string(),
                    tool_name: "Read".to_string(),
                    description: "a different tool's prompt in the same second".to_string(),
                    input_preview: String::new(),
                },
            ];
        });
        store.read_with(cx, |store, _| {
            let open = store.open_permission_request_id();
            assert_eq!(
                open.as_deref(),
                Some("bbbbb"),
                "the line for the tool on screen must stay answerable; expected \
                 Some(\"bbbbb\"), got {open:?}"
            );
        });

        std::fs::remove_dir_all(&home_directory).ok();
    }

    /// What the window between the two times must not throw away: a record that never
    /// carried a time of its own cannot be placed against the other one, and dropping it
    /// would leave a prompt that is on screen with no buttons at all.
    #[gpui::test]
    async fn test_open_permission_request_id_still_answers_a_line_without_a_time(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("untimed-permission");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("64.json"),
            &registration_json(64, "untimed-permission-session"),
        );
        let store = store_with_selection(
            cx,
            Arc::new(FakeSource::new(
                home_directory.clone(),
                fake_process_starts(vec![64]),
            )),
            64,
        )
        .await;

        store.update(cx, |store, _| {
            store.live.pending_permission = Some(crate::PermissionRequest {
                tool_use_id: "toolu_untimed".to_string(),
                tool_name: "Bash".to_string(),
                tool_input: serde_json::json!({"command": "ls"}),
                since_ms: 1_700_000_000_000,
            });
            store.channel_events = vec![ChannelInboxEvent::PermissionRequest {
                at_ms: 0,
                request_id: "ccccc".to_string(),
                tool_name: "Bash".to_string(),
                description: "a line whose at_ms could not be read".to_string(),
                input_preview: String::new(),
            }];
        });
        store.read_with(cx, |store, _| {
            let open = store.open_permission_request_id();
            assert_eq!(
                open.as_deref(),
                Some("ccccc"),
                "a line with no time of its own must stay answerable; expected \
                 Some(\"ccccc\"), got {open:?}"
            );
        });

        // The same the other way round: a hook wrapper written without `received_at_ms`.
        store.update(cx, |store, _| {
            if let Some(pending) = store.live.pending_permission.as_mut() {
                pending.since_ms = 0;
            }
            store.channel_events = vec![ChannelInboxEvent::PermissionRequest {
                at_ms: 1_700_000_000_000,
                request_id: "ddddd".to_string(),
                tool_name: "Bash".to_string(),
                description: "a prompt the hook could not date".to_string(),
                input_preview: String::new(),
            }];
        });
        store.read_with(cx, |store, _| {
            let open = store.open_permission_request_id();
            assert_eq!(
                open.as_deref(),
                Some("ddddd"),
                "a prompt the hook could not date must stay answerable; expected \
                 Some(\"ddddd\"), got {open:?}"
            );
        });

        std::fs::remove_dir_all(&home_directory).ok();
    }

    #[gpui::test]
    async fn test_a_message_sent_inbox_event_marks_the_outbox_file(cx: &mut gpui::TestAppContext) {
        let home_directory = temporary_directory("message-sent-inbox");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("62.json"),
            &registration_json(62, "inbox-session"),
        );
        let store = store_with_selection(
            cx,
            Arc::new(FakeSource::new(
                home_directory.clone(),
                fake_process_starts(vec![62]),
            )),
            62,
        )
        .await;

        store.update(cx, |store, _| {
            store.channel_events.push(ChannelInboxEvent::MessageSent {
                at_ms: 50,
                outbox_file: "0000000000050-0001.json".to_string(),
                content_chars: 5,
            });
        });
        store.read_with(cx, |store, _| {
            assert!(
                store.channel_events().iter().any(|event| matches!(
                    event,
                    ChannelInboxEvent::MessageSent { outbox_file, .. }
                        if outbox_file == "0000000000050-0001.json"
                )),
                "the inbox event is what a pending send is paired with"
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

    /// Everything a tab opened straight onto one of a session's agents is built with, in
    /// the order the tab builds it: the session is selected and the agent chosen before
    /// any scan has listed either.
    fn store_opened_onto_the_flat_agent(
        source: Arc<FakeSource>,
        cx: &mut gpui::TestAppContext,
    ) -> gpui::Entity<ClaudeSessionStore> {
        cx.new(|cx| {
            let mut store = ClaudeSessionStore::new(source, None, cx);
            store.select("agent-session", cx);
            store.select_transcript_target(reading_the_flat_agent(), cx);
            store
        })
    }

    fn reading_the_flat_agent() -> TranscriptTarget {
        TranscriptTarget::Subagent {
            agent_id: FLAT_AGENT_ID.to_string(),
            workflow_run_id: None,
        }
    }

    fn uuids_on_screen(store: &ClaudeSessionStore) -> Vec<String> {
        store
            .transcript()
            .active_path()
            .iter()
            .filter_map(|record| record.uuid.clone())
            .collect()
    }

    /// Writes one session with one agent under it, and reports the home directory and a
    /// source reading it.
    fn a_session_with_an_agent(label: &str) -> (PathBuf, Arc<FakeSource>) {
        let home_directory = temporary_directory(label);
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("81.json"),
            &registration_json(81, "agent-session"),
        );
        write_transcript(
            &home_directory,
            "agent-session",
            "{\"type\":\"user\",\"uuid\":\"m1\"}\n",
        );
        write_subagent(
            &home_directory,
            "agent-session",
            None,
            FLAT_AGENT_ID,
            "{\"type\":\"user\",\"isSidechain\":true,\"uuid\":\"f1\"}\n",
        );
        let source = Arc::new(FakeSource::new(
            home_directory.clone(),
            fake_process_starts(vec![81]),
        ));
        (home_directory, source)
    }

    /// The bug: a tab opened onto one of a session's agents selects the session and then
    /// the agent, both before any scan has run, so the store follows no session id yet.
    /// The first scan to arrive read that as the session having changed under it — the
    /// test for `/clear` — and started the session's own conversation over, so a tab
    /// opened to read an agent opened onto that agent's parent instead.
    #[gpui::test]
    async fn test_a_store_opened_onto_an_agent_keeps_it_through_the_first_scan(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_home_directory, source) = a_session_with_an_agent("agent-tab");
        let store = store_opened_onto_the_flat_agent(source, cx);

        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            assert_eq!(
                store.transcript_target(),
                &reading_the_flat_agent(),
                "the first scan only named the session the store was already pointed at, \
                 which is not the session changing"
            );
            assert_eq!(
                uuids_on_screen(store),
                vec!["f1".to_string()],
                "so what is on screen is the agent's conversation, not its session's"
            );
        });
    }

    /// An agent's records are on disk and go on being readable after the session that
    /// spawned it has exited, which is the whole of what a tab reading one is for. What
    /// cannot outlive the session goes: nothing is selected any more, so nothing offers
    /// the session's other conversations.
    #[gpui::test]
    async fn test_an_agents_conversation_outlives_the_session_that_spawned_it(
        cx: &mut gpui::TestAppContext,
    ) {
        let (home_directory, source) = a_session_with_an_agent("agent-outlives");
        let store = store_opened_onto_the_flat_agent(source, cx);

        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert_eq!(uuids_on_screen(store), vec!["f1".to_string()]);
        });

        std::fs::remove_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("81.json"),
        )
        .expect("ending the session");
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            assert_eq!(
                store.selected(),
                Some("agent-session"),
                "the ended session stays selected so its transcript can still be read"
            );
            assert!(
                store.selected_is_ended(),
                "a session that has left the registry is kept as an ended row"
            );
            assert_eq!(
                store.transcript_target(),
                &reading_the_flat_agent(),
                "but what the tab was opened to read is still what it is reading"
            );
            assert_eq!(
                uuids_on_screen(store),
                vec!["f1".to_string()],
                "and the records it had read are still on screen"
            );
        });
    }

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

        select_listed(&store, 71, cx);
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
        select_listed(&store, 72, cx);
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
        select_listed(&store, 101, cx);
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
        select_listed(&store, 81, cx);
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
        select_listed(&store, 91, cx);
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

    /// Sending is a property of the selected session, not of which conversation is on
    /// screen. The store must not switch the reader off an agent's transcript.
    #[gpui::test]
    async fn test_send_message_does_not_leave_an_agents_conversation(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("subagent-send");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("91.json"),
            &registration_json(91, "typing-session"),
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
        write_live_channel(&home_directory, 91);

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
        wait_for_channel_poll(cx).await;

        store.update(cx, |store, cx| {
            store.send_message("hello".to_string(), cx).detach()
        });
        cx.run_until_parked();

        assert_eq!(
            source.sent_channel(),
            vec![SentChannel::Message {
                content: "hello".to_string(),
            }]
        );
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

    #[gpui::test]
    async fn a_new_pid_with_the_same_session_id_rebinds_without_resetting(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("rebind");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("81.json"),
            &registration_json(81, "kept-session"),
        );
        write_transcript(
            &home_directory,
            "kept-session",
            "{\"type\":\"user\",\"uuid\":\"keep\"}\n",
        );

        let store = cx.new(|cx| {
            ClaudeSessionStore::new(
                Arc::new(FakeSource::new(
                    home_directory.clone(),
                    fake_process_starts(vec![81, 82]),
                )),
                None,
                cx,
            )
        });
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        select_listed(&store, 81, cx);
        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL * 2);
        cx.run_until_parked();

        let generation_before = store.read_with(cx, |store, _| {
            assert_eq!(
                store
                    .transcript()
                    .active_path()
                    .iter()
                    .map(|record| record.uuid.as_deref())
                    .collect::<Vec<_>>(),
                vec![Some("keep")]
            );
            store.transcript_generation()
        });

        std::fs::remove_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("81.json"),
        )
        .expect("replacing the process");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("82.json"),
            &registration_json(82, "kept-session"),
        );
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            assert_eq!(store.selected(), Some("kept-session"));
            assert_eq!(store.selected_process_id(), Some(82));
            assert_eq!(store.transcript_generation(), generation_before);
            assert_eq!(
                store
                    .transcript()
                    .active_path()
                    .iter()
                    .map(|record| record.uuid.as_deref())
                    .collect::<Vec<_>>(),
                vec![Some("keep")],
                "a rebound must keep the transcript already read"
            );
            assert!(
                store
                    .notice()
                    .is_some_and(|notice| notice.contains("rebound")),
                "got {:?}",
                store.notice()
            );
            assert!(store.ended_sessions().is_empty());
        });

        std::fs::remove_dir_all(&home_directory).ok();
    }

    #[gpui::test]
    async fn an_ended_session_reappearing_is_removed_from_ended(cx: &mut gpui::TestAppContext) {
        let home_directory = temporary_directory("reappear");
        let registration = home_directory
            .join(".claude")
            .join("sessions")
            .join("91.json");
        write_file(
            registration.clone(),
            &registration_json(91, "reappear-session"),
        );
        write_transcript(
            &home_directory,
            "reappear-session",
            "{\"type\":\"user\",\"uuid\":\"r1\"}\n",
        );

        let store = cx.new(|cx| {
            ClaudeSessionStore::new(
                Arc::new(FakeSource::new(
                    home_directory.clone(),
                    fake_process_starts(vec![91]),
                )),
                None,
                cx,
            )
        });
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        select_listed(&store, 91, cx);
        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL * 2);
        cx.run_until_parked();

        std::fs::remove_file(&registration).expect("ending the session");
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert!(store.selected_is_ended());
            assert_eq!(
                store.ended_sessions()[0].ended_reason,
                EndedReason::ProcessGone
            );
        });

        write_file(registration, &registration_json(91, "reappear-session"));
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert!(!store.selected_is_ended());
            assert!(store.ended_sessions().is_empty());
            assert_eq!(store.selected_process_id(), Some(91));
        });

        std::fs::remove_dir_all(&home_directory).ok();
    }

    #[gpui::test]
    async fn a_timed_out_transcript_read_is_reported_and_the_loop_continues(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("transcript-timeout");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("101.json"),
            &registration_json(101, "timeout-session"),
        );
        let source = Arc::new(
            FakeSource::new(home_directory.clone(), fake_process_starts(vec![101]))
                .hanging_transcript(cx.executor()),
        );
        let store = cx.new(|cx| ClaudeSessionStore::new(source, None, cx));
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        select_listed(&store, 101, cx);
        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL);
        cx.run_until_parked();
        cx.executor().advance_clock(REGISTRY_SCAN_TIMEOUT);
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            let error = store
                .error()
                .cloned()
                .expect("the timeout must be reported");
            assert!(
                error.contains("reading transcript") && error.contains("has not answered"),
                "got {error:?}"
            );
        });

        cx.executor().advance_clock(REGISTRY_SCAN_TIMEOUT);
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert!(
                store.error().is_some(),
                "the poll must still be running after a timeout"
            );
        });

        std::fs::remove_dir_all(&home_directory).ok();
    }

    #[gpui::test]
    async fn a_failing_subagent_scan_does_not_toggle_the_banner(cx: &mut gpui::TestAppContext) {
        let home_directory = temporary_directory("subagent-banner");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("111.json"),
            &registration_json(111, "banner-session"),
        );
        let source = Arc::new(
            FakeSource::new(home_directory.clone(), fake_process_starts(vec![111]))
                .failing_subagents(),
        );
        let store = cx.new(|cx| ClaudeSessionStore::new(source, None, cx));
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        let first = store
            .read_with(cx, |store, _| store.error().cloned())
            .expect("the failing subagent scan must be reported");
        assert!(first.contains("Listing subagents"), "got {first:?}");

        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        let second = store
            .read_with(cx, |store, _| store.error().cloned())
            .expect("the banner must stay through a successful registry scan");
        assert_eq!(first, second);

        std::fs::remove_dir_all(&home_directory).ok();
    }

    #[test]
    fn stale_for_picks_the_most_overdue() {
        let clock = StoreClock {
            last_transcript_ok_ms: Some(0),
            last_events_ok_ms: Some(0),
            last_status_ok_ms: Some(0),
            last_registry_ok_ms: Some(10_000),
        };
        assert_eq!(clock.stale_for(6_000), Some(("transcript", 1_000)));
        assert_eq!(
            clock.stale_for(20_000),
            Some(("transcript", 15_000)),
            "transcript is 15s past a 5s budget; status is only 5s past a 15s budget"
        );
        let clock = StoreClock {
            last_transcript_ok_ms: Some(0),
            last_events_ok_ms: Some(0),
            last_status_ok_ms: Some(0),
            last_registry_ok_ms: Some(0),
        };
        assert_eq!(
            clock.stale_for(20_000),
            Some(("transcript", 15_000)),
            "equal overdue amounts keep the first of the tied names"
        );
    }

    #[gpui::test]
    async fn set_visible_false_lengthens_the_registry_interval(cx: &mut gpui::TestAppContext) {
        let home_directory = temporary_directory("hidden-polls");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("121.json"),
            &registration_json(121, "hidden-session"),
        );
        let source = Arc::new(FakeSource::new(
            home_directory.clone(),
            fake_process_starts(vec![121]),
        ));
        let store = cx.new(|cx| ClaudeSessionStore::new(source.clone(), None, cx));
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        let visible_calls = source.list_sessions_calls();

        store.update(cx, |store, _| store.set_visible(false));
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        let hidden_baseline = source.list_sessions_calls();
        assert!(
            hidden_baseline >= visible_calls,
            "the wait already in flight may still land once"
        );

        cx.executor().advance_clock(Duration::from_secs(4));
        cx.run_until_parked();
        assert_eq!(
            source.list_sessions_calls(),
            hidden_baseline,
            "a hidden store must not poll again inside four seconds of a 5s interval"
        );

        cx.executor().advance_clock(Duration::from_secs(2));
        cx.run_until_parked();
        assert!(
            source.list_sessions_calls() > hidden_baseline,
            "a hidden store still polls, just more slowly"
        );

        std::fs::remove_dir_all(&home_directory).ok();
    }

    /// A clock only reports staleness while the loop it belongs to is expected to be
    /// reading something. With nothing selected there is no conversation to follow, so no
    /// transcript, events or status read was ever due.
    #[gpui::test]
    async fn nothing_is_stale_while_no_session_is_selected(cx: &mut gpui::TestAppContext) {
        let home_directory = temporary_directory("stale-without-selection");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("131.json"),
            &registration_json(131, "unselected-session"),
        );
        let source = Arc::new(FakeSource::new(
            home_directory.clone(),
            fake_process_starts(vec![131]),
        ));
        let store = cx.new(|cx| ClaudeSessionStore::new(source, None, cx));
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();

        let long_after = ClaudeSessionStore::clock_ms() + 60_000;
        let stale = store.read_with(cx, |store, _| store.stale_for(long_after));
        assert!(
            !matches!(
                stale,
                Some(("transcript", _)) | Some(("events", _)) | Some(("status", _))
            ),
            "no session is selected, so no transcript, events or status read was ever due; \
             got {stale:?}"
        );
        assert!(
            matches!(stale, Some(("registry", _))),
            "the registry poll runs whether or not a session is selected, so its clock is the \
             one left to report; got {stale:?}"
        );

        std::fs::remove_dir_all(&home_directory).ok();
    }

    /// The bound above must not silence the report it exists for: a followed conversation
    /// whose reads stop landing is exactly what the chip is for.
    #[gpui::test]
    async fn a_followed_transcript_that_never_answers_is_still_reported_stale(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("stale-with-selection");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("132.json"),
            &registration_json(132, "hung-session"),
        );
        let source = Arc::new(
            FakeSource::new(home_directory.clone(), fake_process_starts(vec![132]))
                .hanging_transcript(cx.executor()),
        );
        let store = cx.new(|cx| ClaudeSessionStore::new(source, None, cx));
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        select_listed(&store, 132, cx);
        cx.run_until_parked();
        // One more registry poll lands while the transcript read stays hung, so the
        // registry clock is strictly newer than the transcript clock and cannot be the
        // more overdue of the two for any `now`.
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();

        // `now` is the store's own transcript clock plus a minute, not a later wall
        // sample, so the result cannot depend on how long this test took to get here.
        let stale = store.read_with(cx, |store, _| {
            let now_ms = store
                .clock
                .last_transcript_ok_ms
                .expect("selecting a listed session starts following its transcript")
                + 60_000;
            store.stale_for(now_ms)
        });
        assert!(
            matches!(stale, Some(("transcript", _))),
            "a followed transcript that never answers must still read as stale; got {stale:?}"
        );

        std::fs::remove_dir_all(&home_directory).ok();
    }

    /// `claude --resume <id>` in a second terminal leaves two live registrations carrying
    /// one sessionId. Neither process replaced the other, so neither is a rebind.
    #[gpui::test]
    async fn two_live_registrations_of_one_session_id_are_not_a_rebind(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("duplicate-session-id");
        let registry_directory = home_directory.join(".claude").join("sessions");
        write_file(
            registry_directory.join("141.json"),
            &registration_json(141, "shared-session"),
        );
        write_file(
            registry_directory.join("142.json"),
            &registration_json(142, "shared-session"),
        );
        let source = Arc::new(FakeSource::new(
            home_directory.clone(),
            fake_process_starts(vec![141, 142]),
        ));
        let store = cx.new(|cx| ClaudeSessionStore::new(source, None, cx));
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        store.update(cx, |store, cx| store.select("shared-session", cx));

        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL * 4);
        cx.run_until_parked();

        let notice = store.read_with(cx, |store, _| store.notice().cloned());
        assert_eq!(
            notice, None,
            "both processes were there before and are there still, so no scan may report a \
             rebind; got {notice:?}"
        );

        std::fs::remove_dir_all(&home_directory).ok();
    }

    /// A timeout listing agents is not a registry poll failure: a scan that then
    /// succeeds must leave the agents report standing, or the chip flickers off on the
    /// next second.
    #[gpui::test]
    async fn an_agents_timeout_is_its_own_error_and_survives_a_registry_scan(
        cx: &mut gpui::TestAppContext,
    ) {
        let home_directory = temporary_directory("agents-timeout-source");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("141.json"),
            &registration_json(141, "agents-timeout-session"),
        );
        let store = cx.new(|cx| {
            ClaudeSessionStore::new(
                Arc::new(FakeSource::new(
                    home_directory.clone(),
                    fake_process_starts(vec![141]),
                )),
                None,
                cx,
            )
        });
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();

        store.update(cx, |store, cx| {
            store.apply_agent_listings(
                Err(anyhow!("claude agents --json has not answered in 10 s")),
                cx,
            );
        });
        store.read_with(cx, |store, _| {
            let error = store.error().expect("the timeout must be reported");
            assert!(
                error.contains("Listing agents") && error.contains("has not answered"),
                "got {error:?}"
            );
            assert!(
                store.agents_unavailable_reason().is_none(),
                "a timeout is not the same as agents being unavailable"
            );
        });

        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            let error = store
                .error()
                .expect("an agents timeout is not a registry poll error");
            assert!(
                error.contains("Listing agents") && error.contains("has not answered"),
                "a successful registry scan must not wipe the agents timeout; got {error:?}"
            );
        });

        std::fs::remove_dir_all(&home_directory).ok();
    }

    #[gpui::test]
    async fn set_visible_false_does_not_stop_the_agents_poll(cx: &mut gpui::TestAppContext) {
        let home_directory = temporary_directory("hidden-agents");
        write_file(
            home_directory
                .join(".claude")
                .join("sessions")
                .join("142.json"),
            &registration_json(142, "hidden-agents-session"),
        );
        let source = Arc::new(FakeSource::new(
            home_directory.clone(),
            fake_process_starts(vec![142]),
        ));
        let store = cx.new(|cx| ClaudeSessionStore::new(source.clone(), None, cx));
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        store.update(cx, |store, _| store.set_visible(false));
        let hidden_baseline = source.list_agents_calls();
        assert!(
            hidden_baseline >= 1,
            "the visible poll must have listed agents at least once; got {hidden_baseline}"
        );

        cx.executor().advance_clock(HIDDEN_POLL_INTERVAL * 6);
        cx.run_until_parked();
        assert!(
            source.list_agents_calls() > hidden_baseline,
            "hiding the panel must not stop the agents poll; stayed at {hidden_baseline}"
        );

        std::fs::remove_dir_all(&home_directory).ok();
    }

    #[gpui::test]
    async fn an_ended_row_is_still_tailed_slowly(cx: &mut gpui::TestAppContext) {
        let home_directory = temporary_directory("ended-tail");
        let registration = home_directory
            .join(".claude")
            .join("sessions")
            .join("143.json");
        write_file(
            registration.clone(),
            &registration_json(143, "ended-tail-session"),
        );
        let source = Arc::new(FakeSource::new(
            home_directory.clone(),
            fake_process_starts(vec![143]),
        ));
        let store = cx.new(|cx| ClaudeSessionStore::new(source.clone(), None, cx));
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        select_listed(&store, 143, cx);
        cx.executor().advance_clock(TRANSCRIPT_POLL_INTERVAL);
        cx.run_until_parked();

        std::fs::remove_file(&registration).expect("ending the session");
        cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert!(store.selected_is_ended(), "the row must have ended");
        });
        let ended_baseline = source.tailed_conversations().len();

        cx.executor().advance_clock(HIDDEN_POLL_INTERVAL);
        cx.run_until_parked();
        assert!(
            source.tailed_conversations().len() > ended_baseline,
            "an ended row must still be tailed, just more slowly; stayed at {ended_baseline}"
        );

        std::fs::remove_dir_all(&home_directory).ok();
    }

    fn live_running_a_turn() -> LiveState {
        let mut live = LiveState::default();
        let event = parse_hook_event(
            r#"{"received_at_ms":10,"event":{"hook_event_name":"UserPromptSubmit","session_id":"s"}}"#,
        )
        .expect("UserPromptSubmit parses");
        live.apply(&event);
        live
    }

    fn turn_name(turn: &Turn) -> &'static str {
        match turn {
            Turn::Idle => "Idle",
            Turn::Running { .. } => "Running",
        }
    }

    fn transcript_record(json: &str) -> TranscriptRecord {
        match parse_record(json) {
            Ok(Some(record)) => record,
            Ok(None) => panic!("expected a record, got a blank line for: {json}"),
            Err(error) => panic!("expected {json} to parse, got error: {error:#}"),
        }
    }

    #[test]
    fn note_transcript_into_live_idles_on_interrupted_message_id() {
        let mut live = live_running_a_turn();
        let record = transcript_record(
            r#"{"parentUuid":"342a2382-...","isSidechain":false,"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]},"uuid":"cd73ce98-...","timestamp":"2026-08-26T15:51:10.639Z","interruptedMessageId":"msg_011CeRdHSSadUJXZ6XPLdgvd","sessionId":"b5a59996-...","version":"2.1.245"}"#,
        );

        note_transcript_into_live(&mut live, &[record]);

        assert_eq!(
            turn_name(&live.turn),
            "Idle",
            "interruptedMessageId is an interrupt; got {}",
            turn_name(&live.turn)
        );
    }

    #[test]
    fn note_transcript_into_live_idles_on_interrupted_tool_use_text() {
        let mut live = live_running_a_turn();
        let record = transcript_record(
            r#"{"type":"user","uuid":"interrupt-tool-use","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}"#,
        );

        note_transcript_into_live(&mut live, &[record]);

        assert_eq!(
            turn_name(&live.turn),
            "Idle",
            "the tool-use interrupt text is an interrupt; got {}",
            turn_name(&live.turn)
        );
    }

    #[test]
    fn note_transcript_into_live_idles_on_string_interrupt_content() {
        let mut live = live_running_a_turn();
        let record = transcript_record(
            r#"{"type":"user","uuid":"interrupt-string","message":{"role":"user","content":"[Request interrupted by user]"}}"#,
        );

        note_transcript_into_live(&mut live, &[record]);

        assert_eq!(
            turn_name(&live.turn),
            "Idle",
            "string interrupt content is an interrupt; got {}",
            turn_name(&live.turn)
        );
    }

    /// Known tradeoff: a user who types exactly `[Request interrupted by user]` produces
    /// the same record as the CLI interrupt, so thinking goes away early. Accepted.
    #[test]
    fn note_transcript_into_live_idles_when_the_user_types_the_interrupt_sentence() {
        let mut live = live_running_a_turn();
        let record = transcript_record(
            r#"{"type":"user","uuid":"typed-interrupt","message":{"content":"[Request interrupted by user]"}}"#,
        );

        note_transcript_into_live(&mut live, &[record]);

        assert_eq!(
            turn_name(&live.turn),
            "Idle",
            "typing the interrupt sentence is the accepted false positive; got {}",
            turn_name(&live.turn)
        );
    }

    #[test]
    fn note_transcript_into_live_keeps_running_for_an_ordinary_user_message() {
        let mut live = live_running_a_turn();
        let record = transcript_record(
            r#"{"type":"user","uuid":"keep-going","message":{"role":"user","content":"keep going"}}"#,
        );

        note_transcript_into_live(&mut live, &[record]);

        assert_eq!(
            turn_name(&live.turn),
            "Running",
            "an ordinary user message must not idle the turn; got {}",
            turn_name(&live.turn)
        );
    }

    #[test]
    fn note_transcript_into_live_keeps_running_when_an_assistant_quotes_the_interrupt_text() {
        let mut live = live_running_a_turn();
        let record = transcript_record(
            r#"{"type":"assistant","uuid":"quoted","message":{"role":"assistant","content":[{"type":"text","text":"Earlier you sent [Request interrupted by user] but I kept going"}]}}"#,
        );

        note_transcript_into_live(&mut live, &[record]);

        assert_eq!(
            turn_name(&live.turn),
            "Running",
            "an assistant quoting the interrupt text must not idle the turn; got {}",
            turn_name(&live.turn)
        );
    }
}
