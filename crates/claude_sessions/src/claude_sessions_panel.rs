//! Renders the conversation of a Claude Code session running on this machine.
//!
//! The panel is a better view of a transcript the CLI is already writing, not a second
//! place to talk to it: everything here is read-only.
//!
//! Two properties drive the shape of this file. The conversation order comes from
//! [`crate::Transcript`], never from the file's line order, because rewinds and
//! compaction leave abandoned branches behind in the same file. And the transcript is
//! large — single files of several megabytes and thousands of records are normal — so
//! the conversation is drawn with the virtualizing [`list`] element and every expensive
//! per-record artifact (a parsed [`Markdown`], a decoded image, a persisted output read
//! back from disk) is built only once the record is actually on screen, then cached.

use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use collections::{HashMap, HashSet};
use editor::Editor;
use fs::Fs;
use gpui::{
    Animation, AnimationExt as _, AnyElement, AsyncWindowContext, Entity, EventEmitter,
    FocusHandle, Focusable, Image, ImageFormat, ListAlignment, ListSizingBehavior, ListState,
    Render, Subscription, Task, WeakEntity, img, list, pulsating_between,
};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use serde_json::Value;
use settings::{DockSide, Settings as _};
use ui::{
    Button, ButtonStyle, Disclosure, Divider, ListItem, ListItemSpacing, SelectableButton as _,
    TintColor, Tooltip, prelude::*,
};
use util::{ResultExt as _, truncate_and_trailoff};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::{
    ClaudeSessionStore, ClaudeSessionsSettings, Interrupt, RegisteredSession, SendMessage,
    SubagentSummary, ToggleFocus, TranscriptRecord, TranscriptTarget,
    session_registry::workflow_run_id_in_tool_result,
    session_source::{FileContents, LocalSource, RemoteSource, SessionInput, SessionSource},
};

const CLAUDE_SESSIONS_PANEL_KEY: &str = "ClaudeSessionsPanel";

const COMPACT_BOUNDARY_SUBTYPE: &str = "compact_boundary";
const LOCAL_COMMAND_SUBTYPE: &str = "local_command";
const ATTACHMENT_RECORD_TYPE: &str = "attachment";
const ATTACHMENTS_ENTRY_KEY: &str = "context-attachments";

/// The blocks Claude Code injects into the text of a user record: context it assembled
/// itself, not words the user typed. A real message often carries one of these appended to
/// its tail, and a record is sometimes nothing but one, so they are cut out of the text
/// rather than the record being dropped whole.
const INJECTED_WRAPPERS: [&str; 6] = [
    "system-reminder",
    "task-notification",
    "persisted-output",
    "codegraph_context",
    "local-command-stdout",
    // Claude Code's note to itself that the user ran the command rather than the model,
    // written alongside the output. Like the output, it is nothing the user typed.
    "local-command-caveat",
];

const PERSISTED_OUTPUT_MARKER: &str = "<persisted-output>";
const PERSISTED_OUTPUT_PATH_PREFIX: &str = "Full output saved to: ";
const PERSISTED_OUTPUT_SIZE_PREFIX: &str = "Output too large (";
const PERSISTED_OUTPUT_PREVIEW_PREFIX: &str = "Preview (";

/// Comfortably above the tens of kilobytes that tool outputs are persisted at in
/// practice. A larger file is still shown, truncated, together with its path, because
/// holding an unbounded amount of text in one list item would stall the whole panel.
const MAX_PERSISTED_OUTPUT_BYTES: u64 = 4 * 1024 * 1024;

const NO_SESSIONS: &str = "No Claude Code sessions are running on this project's host.";
const SELECT_A_SESSION: &str = "Select a session to read its conversation.";
const WAITING_FOR_TRANSCRIPT: &str =
    "This session has not written a transcript file yet. It will appear as soon as it does.";
const EMPTY_TRANSCRIPT: &str = "This conversation has no messages yet.";
const READ_ONLY_NOTE: &str = "This session is not running inside tmux, so it can only be read.";
const SELECT_A_SESSION_TO_REPLY: &str = "Select a session to reply to it.";
const MESSAGE_PLACEHOLDER: &str = "Message this session…";
const SENDING_NOTE: &str = "Sending…";
const THINKING_NOTE: &str = "Thinking…";

const ASSISTANT_RECORD_TYPE: &str = "assistant";
const USER_RECORD_TYPE: &str = "user";
const THINKING_BLOCK_TYPE: &str = "thinking";
const TOOL_USE_BLOCK_TYPE: &str = "tool_use";
const TOOL_RESULT_BLOCK_TYPE: &str = "tool_result";

/// The two blocks Claude Code writes into a user record for a slash command. Unlike
/// [`INJECTED_WRAPPERS`] these are not cut out: what is inside them is the command the
/// user ran and the arguments they typed after it.
const COMMAND_NAME_WRAPPER: &str = "command-name";
const COMMAND_ARGS_WRAPPER: &str = "command-args";

/// How much of a running tool's target is shown beside its name. Counted in characters:
/// a `Bash` command is often a whole pipeline, and the head of it is what says which
/// call is running.
const TOOL_TARGET_CHARACTERS: usize = 40;

/// One cycle of the pulse that marks something as still in progress — a message that has
/// not arrived, a session that is answering. Matches the period the agent panel pulses
/// its own icons at.
const PULSE_PERIOD: Duration = Duration::from_secs(1);

/// How many lines of a tool's output are drawn before the rest is put behind a
/// disclosure. A build log or a test run is thousands of lines, and one of them drawn
/// whole leaves no room in the panel for the conversation around it.
const MAX_UNCLAMPED_OUTPUT_LINES: usize = 12;

/// The one tool whose input is a command rather than a description of one, so the
/// command itself is what is drawn, in the shell's own syntax.
const BASH_TOOL_NAME: &str = "Bash";

/// The tools that carry the text of the file they are about, and the field of their
/// input it sits in. That text is what the call is, so it is what is drawn, in the
/// language of the file it names.
///
/// `Read` is deliberately absent: its input names a path and carries none of the file, so
/// there is nothing of the file to draw and its input is JSON however the file is
/// written.
const FILE_TOOL_CONTENT_FIELDS: [(&str, &str); 3] = [
    ("Write", "content"),
    ("Edit", "new_string"),
    ("NotebookEdit", "new_source"),
];

/// The two tools that start conversations of their own: one agent, and a run of several.
const AGENT_TOOL_NAME: &str = "Agent";
const WORKFLOW_TOOL_NAME: &str = "Workflow";

/// How much of an agent's id a chip carries when the agent recorded no description of
/// what it was asked to do. Enough to tell two agents apart without the id taking the
/// whole chip.
const AGENT_ID_CHIP_CHARACTERS: usize = 6;

const MAIN_CONVERSATION_CHIP: &str = "Main";
const AGENT_READ_ONLY_NOTE: &str =
    "This is an agent's conversation, and can only be read. Switch to Main to reply.";

const NO_OUTPUT_NOTE: &str = "(no output)";

/// How much of the speaker's own color the rail beside a message carries. Full strength
/// beside every message competes with the text for attention.
const ROLE_RAIL_OPACITY: f32 = 0.5;

pub struct ClaudeSessionsPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    /// Only used to write the dock side back to the settings file, which is where it
    /// lives so that it survives a restart.
    fs: Arc<dyn Fs>,
    store: Entity<ClaudeSessionStore>,
    /// The same source the store reads through, kept here for the one read the panel
    /// performs on its own behalf: a persisted tool output.
    source: Arc<dyn SessionSource>,
    /// What the user is typing to the selected session. Disabled, rather than hidden,
    /// while there is no pane to type into.
    message_editor: Entity<Editor>,
    /// Used only to shorten the working directory of sessions opened inside this project.
    project_root: Option<PathBuf>,
    /// The flattened conversation, one entry per rendered item. Rebuilt from the
    /// transcript whenever the store reports a change, and spliced into `list_state` so
    /// that the items which did not move keep their measured height.
    entries: Vec<Entry>,
    /// The messages that have left for the selected session but have not appeared in its
    /// transcript yet. Drawn after the entries built from the transcript, and never
    /// mixed into them: see [`PendingSends`].
    pending_sends: PendingSends,
    /// What the selected session is doing, as of the last change to its transcript; see
    /// [`activity`].
    activity: Activity,
    /// The calls in the conversation on screen that started conversations of their own,
    /// keyed by the entry each is drawn as; see [`agent_calls`]. Rebuilt with the entries
    /// rather than cached with them, because the state of what a call started changes
    /// while the call's own record never does.
    agent_calls: HashMap<SharedString, AgentCall>,
    list_state: ListState,
    /// Whether the list of sessions is showing its rows. Collapsing it gives the whole
    /// panel to the conversation, which is what the user is here to read.
    session_list_expanded: bool,
    /// See [`UnreadBelow`].
    unread_below: UnreadBelow,
    /// Keys of the entries — and of the attachments nested inside the context section —
    /// the user has opened.
    expanded: HashSet<SharedString>,
    /// Whether the conversation is read with [`crate::Transcript::full_path`], which
    /// includes everything compaction dropped from the model's context.
    show_full_history: bool,
    markdowns: HashMap<SharedString, Entity<Markdown>>,
    /// Keyed by record uuid, and so unaffected by the conversation growing around an
    /// entry; see [`EntryCache`].
    entry_cache: EntryCache,
    /// The store's transcript generation the cache was filled from. A change means the
    /// transcript was thrown away, and with it everything derived from it.
    cached_transcript_generation: u64,
    /// The home directory the cache's entries were derived against; see
    /// [`Self::refresh_entry_cache`].
    cached_home_directory: Option<PathBuf>,
    /// How many of the transcript's overwritten uuids the cache has already dropped.
    overwrites_dropped: usize,
    loaded_outputs: HashMap<SharedString, OutputLoad>,
    output_loads: HashMap<SharedString, Task<()>>,
    _store_subscription: Subscription,
}

#[derive(Clone, PartialEq)]
struct Entry {
    /// Stable across rebuilds — a record uuid, plus the block's position within the
    /// record — so caches and expansion state survive the transcript growing.
    key: SharedString,
    kind: EntryKind,
}

#[derive(Clone, PartialEq)]
enum EntryKind {
    Message {
        role: MessageRole,
        source: SharedString,
    },
    Thinking {
        source: SharedString,
    },
    ToolUse {
        name: SharedString,
        input: SharedString,
    },
    ToolResult {
        label: SharedString,
        is_error: bool,
        body: ToolResultBody,
    },
    Image {
        image: Arc<Image>,
        media_type: SharedString,
    },
    LocalCommand {
        text: SharedString,
    },
    /// The slash command a user record holds instead of words the user typed; see
    /// [`rewrite_slash_commands`].
    SlashCommand {
        text: SharedString,
    },
    /// A message that has left for the session but has not appeared in its transcript
    /// yet; see [`PendingSends`]. The only entry that does not come from a record.
    Pending {
        id: u64,
        text: SharedString,
    },
    CompactBoundary {
        trigger: SharedString,
        pre_tokens: Option<u64>,
        post_tokens: Option<u64>,
    },
    Attachments {
        items: Vec<AttachmentItem>,
    },
    /// A record or block this panel has no display rule for. Shown as raw JSON rather
    /// than dropped, so that a transcript format that has moved on is still readable.
    Unknown {
        label: SharedString,
        raw: SharedString,
    },
}

#[derive(Clone, Debug, PartialEq)]
enum ToolResultBody {
    Inline(SharedString),
    Persisted(PersistedOutput),
}

/// A tool output too large to be written into the transcript, which Claude Code saved to
/// its own file and replaced with a preview.
#[derive(Clone, Debug, PartialEq)]
struct PersistedOutput {
    size: SharedString,
    path: PathBuf,
    preview: SharedString,
}

#[derive(Clone, PartialEq)]
struct AttachmentItem {
    key: SharedString,
    label: SharedString,
    body: SharedString,
    /// Set when the attachment's text names a file Claude Code wrote the real output to
    /// and that file is one the panel will read back.
    persisted: Option<PersistedOutput>,
}

/// One collapsible row's header, as the row it draws rather than as an argument list:
/// six of them are built and each named the same seven things in the same order.
struct DisclosureHeader<'key> {
    entry_index: usize,
    key: &'key SharedString,
    icon: IconName,
    icon_color: Color,
    title: SharedString,
    summary: Option<SharedString>,
    is_expanded: bool,
}

/// The per-record artifacts that cost real work to derive — a decoded image, a
/// pretty-printed unrecognized record, terminal output with its escapes stripped, the
/// source string a [`Markdown`] is parsed from — kept across rebuilds of the
/// conversation.
///
/// A rebuild runs on every store notification, which is up to four times a second while
/// a reply is streaming, and deriving these again each time is what makes a long session
/// with pasted screenshots stall: a screenshot is around a megabyte and a half of base64.
/// A record never changes once absorbed, so a hit needs no validation. The two things
/// that do invalidate what was derived — the transcript being read again from the start,
/// and `absorb` replacing one record — drop keys explicitly instead.
#[derive(Default)]
struct EntryCache {
    /// Keyed by entry key: the record's uuid, plus the block's index within the record.
    kinds: HashMap<SharedString, EntryKind>,
    attachments: HashMap<SharedString, AttachmentItem>,
    /// Counts the artifacts actually derived, so that a test can show a rebuild reused
    /// them rather than only showing that it produced the same result.
    #[cfg(test)]
    derived: usize,
}

impl EntryCache {
    fn kind(
        &mut self,
        key: Option<&SharedString>,
        derive: impl FnOnce() -> EntryKind,
    ) -> EntryKind {
        if let Some(kind) = key.and_then(|key| self.kinds.get(key)) {
            return kind.clone();
        }

        #[cfg(test)]
        {
            self.derived += 1;
        }
        let kind = derive();
        if let Some(key) = key {
            self.kinds.insert(key.clone(), kind.clone());
        }
        kind
    }

    /// The same as [`Self::kind`] for a derivation that may decide the record has nothing
    /// to show. Nothing is cached for such a record: the answer is a scan of the record's
    /// own text rather than an artifact, and there is no entry to cache it under.
    fn optional_kind(
        &mut self,
        key: Option<&SharedString>,
        derive: impl FnOnce() -> Option<EntryKind>,
    ) -> Option<EntryKind> {
        if let Some(kind) = key.and_then(|key| self.kinds.get(key)) {
            return Some(kind.clone());
        }

        #[cfg(test)]
        {
            self.derived += 1;
        }
        let kind = derive()?;
        if let Some(key) = key {
            self.kinds.insert(key.clone(), kind.clone());
        }
        Some(kind)
    }

    fn attachment(
        &mut self,
        key: Option<&SharedString>,
        derive: impl FnOnce() -> AttachmentItem,
    ) -> AttachmentItem {
        if let Some(item) = key.and_then(|key| self.attachments.get(key)) {
            return item.clone();
        }

        #[cfg(test)]
        {
            self.derived += 1;
        }
        let item = derive();
        if let Some(key) = key {
            self.attachments.insert(key.clone(), item.clone());
        }
        item
    }

    /// Drops everything derived from one record, for when `absorb` replaces it with a
    /// later record carrying the same uuid.
    fn forget_record(&mut self, uuid: &str) {
        self.kinds
            .retain(|key, _| !key_belongs_to_record(key, uuid));
        self.attachments
            .retain(|key, _| !key_belongs_to_record(key, uuid));
    }

    /// Forgets the records that have left the conversation being shown, so that the
    /// images of a compacted-away turn do not stay in memory for the rest of the session.
    fn retain_keys(&mut self, live_keys: &HashSet<SharedString>) {
        self.kinds.retain(|key, _| live_keys.contains(key));
        self.attachments.retain(|key, _| live_keys.contains(key));
    }
}

/// An entry key is a record's uuid on its own, or that uuid and the index of the block
/// within the record.
fn key_belongs_to_record(key: &str, uuid: &str) -> bool {
    key.strip_prefix(uuid)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('#'))
}

#[derive(Clone, Copy, PartialEq)]
enum MessageRole {
    User,
    Assistant,
    Other,
    /// The record compaction writes in the user's place, which carries `"type":"user"`
    /// but was not typed by the user.
    CompactSummary,
}

/// How many entries have arrived since the reader was last at the bottom of the
/// conversation.
///
/// The list follows the tail only while it is already at the bottom, because a reader who
/// has scrolled up is reading something and must not be yanked away from it. That leaves
/// the case this counts: new messages landing below the window, out of sight, which is
/// what makes a session that is streaming look like a session that has stopped.
#[derive(Default)]
struct UnreadBelow {
    count: usize,
}

impl UnreadBelow {
    /// `at_bottom` is where the list sat *before* the new entries were spliced in, which
    /// is what decides whether they were scrolled into view or landed below the window.
    fn entries_arrived(&mut self, added: usize, at_bottom: bool) {
        if at_bottom {
            self.count = 0;
        } else {
            self.count = self.count.saturating_add(added);
        }
    }

    /// Called with where the list sits now: the count is what has arrived since the
    /// reader was last at the bottom, so being there again is having read it.
    fn scrolled(&mut self, at_bottom: bool) {
        if at_bottom {
            self.count = 0;
        }
    }

    /// A different session is a different conversation, and the reader is shown the
    /// newest of it, so nothing is owed to them from the one they left.
    fn reset(&mut self) {
        self.count = 0;
    }

    fn count(&self) -> usize {
        self.count
    }
}

#[derive(Clone)]
enum OutputLoad {
    Loading,
    Loaded(SharedString),
    Failed(SharedString),
}

impl MessageRole {
    fn from_record_type(record_type: &str) -> Self {
        match record_type {
            "user" => Self::User,
            "assistant" => Self::Assistant,
            _ => Self::Other,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::User => "You",
            Self::Assistant => "Claude",
            Self::Other => "Message",
            Self::CompactSummary => "Summary",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::User => IconName::Person,
            Self::Assistant => IconName::AiClaude,
            Self::Other => IconName::Chat,
            Self::CompactSummary => IconName::Compact,
        }
    }

    /// Each speaker keeps a color of its own, because the panel is often narrow enough
    /// that a label is truncated and the color is what is left to tell the rows apart.
    fn color(self) -> Color {
        match self {
            Self::User => Color::Info,
            Self::Assistant => Color::Accent,
            Self::Other => Color::Muted,
            Self::CompactSummary => Color::Warning,
        }
    }
}

impl ClaudeSessionsPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            ClaudeSessionsPanel::new(workspace, window, cx)
        })
    }

    pub fn new(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let workspace_handle = workspace.weak_handle();
        let project = workspace.project().clone();
        let fs = workspace.app_state().fs.clone();

        cx.new(|cx| {
            let project_root = project
                .read(cx)
                .visible_worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf());
            // The sessions worth showing are the ones on the machine the project is
            // opened from: on a remote project they are read over that project's
            // connection, and the panel is otherwise the same on both.
            let source: Arc<dyn SessionSource> = match project.read(cx).remote_client() {
                Some(remote_client) => Arc::new(RemoteSource::new(
                    remote_client.read(cx).proto_client(),
                    cx.background_executor().clone(),
                )),
                None => Arc::new(LocalSource::new(cx.background_executor().clone())),
            };
            let store =
                cx.new(|cx| ClaudeSessionStore::new(source.clone(), project_root.clone(), cx));
            let store_subscription = cx.observe(&store, |this: &mut Self, _, cx| {
                this.rebuild_entries(cx);
                this.sync_input_availability(cx);
                // The session list changes on scans that leave the conversation
                // untouched, so the panel is redrawn whether or not entries moved.
                cx.notify();
            });

            let message_editor = cx.new(|cx| {
                let mut editor = Editor::auto_height(1, 8, window, cx);
                editor.set_placeholder_text(MESSAGE_PLACEHOLDER, window, cx);
                // Nothing is selected yet, so there is nothing to type into.
                editor.set_read_only(true);
                editor
            });

            Self {
                workspace: workspace_handle,
                focus_handle: cx.focus_handle(),
                fs,
                store,
                source,
                message_editor,
                project_root,
                entries: Vec::new(),
                pending_sends: PendingSends::default(),
                activity: Activity::Idle,
                agent_calls: HashMap::default(),
                list_state: ListState::new(0, ListAlignment::Bottom, px(1024.)),
                session_list_expanded: true,
                unread_below: UnreadBelow::default(),
                expanded: HashSet::default(),
                show_full_history: false,
                markdowns: HashMap::default(),
                entry_cache: EntryCache::default(),
                cached_transcript_generation: 0,
                cached_home_directory: None,
                overwrites_dropped: 0,
                loaded_outputs: HashMap::default(),
                output_loads: HashMap::default(),
                _store_subscription: store_subscription,
            }
        })
    }

    fn select_session(&mut self, process_id: u32, cx: &mut Context<Self>) {
        self.show_the_newest_of_another_conversation();
        self.store
            .update(cx, |store, cx| store.select(process_id, cx));
    }

    /// What is owed to a reader who has been moved to another conversation, whichever
    /// gesture moved them: whether the last one had been compacted says nothing about
    /// this one, so the history toggle starts closed; and what they are shown of it is
    /// its newest end, not the place they had scrolled the last one to, so nothing of it
    /// is owed to them as unread either.
    fn show_the_newest_of_another_conversation(&mut self) {
        self.show_full_history = false;
        self.unread_below.reset();
        self.list_state.scroll_to_end();
    }

    /// Follows another of the selected session's conversations, from a chip or from the
    /// card under the call that started it.
    ///
    /// Everything the reader is owed about the conversation they are leaving is settled
    /// the same way selecting another session settles it: another conversation is another
    /// thing to read, and what they are shown of it is its newest end.
    fn select_transcript_target(&mut self, target: TranscriptTarget, cx: &mut Context<Self>) {
        self.show_the_newest_of_another_conversation();
        self.store
            .update(cx, |store, cx| store.select_transcript_target(target, cx));
    }

    fn toggle_session_list(&mut self, cx: &mut Context<Self>) {
        self.session_list_expanded = !self.session_list_expanded;
        cx.notify();
    }

    /// Takes the reader to the newest of the conversation, which is what the count of
    /// what arrived below their window offers.
    fn scroll_to_latest(&mut self, cx: &mut Context<Self>) {
        self.list_state.scroll_to_end();
        self.unread_below.reset();
        cx.notify();
    }

    fn toggle_full_history(&mut self, cx: &mut Context<Self>) {
        self.show_full_history = !self.show_full_history;
        // A different path is a different conversation as far as the list is concerned;
        // none of the measured heights line up with the new indices.
        self.entries.clear();
        self.list_state.reset(0);
        self.unread_below.reset();
        self.rebuild_entries(cx);
        cx.notify();
    }

    fn toggle_expanded(&mut self, key: SharedString, entry_index: usize, cx: &mut Context<Self>) {
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
        // Only this item changed height, and remeasuring just it keeps the scroll
        // position anchored where the user left it.
        self.list_state
            .remeasure_items(entry_index..entry_index.saturating_add(1));
        cx.notify();
    }

    fn rebuild_entries(&mut self, cx: &mut Context<Self>) {
        self.refresh_entry_cache(cx);

        // Moved out for the duration of the build so that the transcript can be borrowed
        // from the store, which is reached through `self`, at the same time.
        let mut cache = std::mem::take(&mut self.entry_cache);
        let (mut new_entries, activity, agent_calls, sent_to_the_session) = {
            let store = self.store.read(cx);
            let home_directory = store.home_directory();
            let transcript = store.transcript();
            let path = if self.show_full_history {
                transcript.full_path()
            } else {
                transcript.active_path()
            };
            // What the session is doing, and which records a sent message may have been
            // written as, are questions about the session rather than about the
            // conversation on screen: both are read from the session's own conversation
            // whichever one the reader has open.
            let session_path = store.main_transcript().active_path();
            // Derived here, where the paths have already been walked: the panel is drawn
            // far more often than the transcript changes, and this is the only place the
            // transcript is read.
            (
                build_entries(&path, home_directory, &mut cache),
                activity(&session_path),
                agent_calls(&path),
                // Read only while a message is waiting, which is rare and brief; see
                // [`user_message_entries`].
                (!self.pending_sends.is_empty()).then(|| user_message_entries(&session_path)),
            )
        };
        self.entry_cache = cache;
        self.activity = activity;
        self.agent_calls = agent_calls;

        self.pending_sends
            .retain_session(self.store.read(cx).selected());
        // Paired against the session's own conversation and never against an agent's: an
        // agent is given its task as a user record of its own conversation, so an agent's
        // records hold the very text a send carries, and a pairing there would take down a
        // message that had never arrived. Against records only, so that a pending message
        // is never paired with another pending message and nothing derived from one
        // reaches the entry cache.
        if let Some(sent_to_the_session) = sent_to_the_session {
            self.pending_sends.pair_with(&sent_to_the_session);
        }
        if pending_is_drawn_in(self.store.read(cx).transcript_target()) {
            new_entries.extend(self.pending_sends.entries());
        }

        let old_length = self.entries.len();
        let new_length = new_entries.len();
        let common_prefix = self
            .entries
            .iter()
            .zip(new_entries.iter())
            .take_while(|(old, new)| old == new)
            .count();
        // A suffix is matched as well as a prefix because the entries that change are
        // rarely at one end: the context section sits at the top and gains an attachment
        // on nearly every turn, while the message being streamed is at the bottom.
        // Splicing only the span between them keeps every other item's measured height,
        // and with it the reader's scroll position.
        let unmatched = old_length.min(new_length).saturating_sub(common_prefix);
        let common_suffix = (0..unmatched)
            .take_while(|offset| {
                let old = self.entries.get(old_length - 1 - offset);
                let new = new_entries.get(new_length - 1 - offset);
                matches!((old, new), (Some(old), Some(new)) if old == new)
            })
            .count();

        let changed_old = common_prefix..(old_length - common_suffix);
        let changed_count = (new_length - common_suffix) - common_prefix;
        if changed_old.is_empty() && changed_count == 0 {
            return;
        }

        // Captured before the splice, which itself moves the scroll position.
        let was_scrolled_to_end = self.list_state.is_scrolled_to_end().unwrap_or(true);

        // What arrived is counted against where the reader was sitting: at the bottom it
        // is scrolled into view below, and anywhere else it lands out of sight, which is
        // what makes a session that is still answering look like one that has stopped.
        let arrived = new_length.saturating_sub(old_length);
        self.unread_below
            .entries_arrived(arrived, was_scrolled_to_end);

        self.entries = new_entries;
        self.list_state.splice(changed_old, changed_count);

        let live_keys = self.live_cache_keys();
        self.markdowns.retain(|key, _| live_keys.contains(key));
        self.expanded.retain(|key| live_keys.contains(key));
        self.loaded_outputs.retain(|key, _| live_keys.contains(key));
        self.output_loads.retain(|key, _| live_keys.contains(key));
        self.entry_cache.retain_keys(&live_keys);

        if was_scrolled_to_end {
            self.list_state.scroll_to_end();
        }
    }

    /// Drops what the entry cache can no longer be trusted with: everything, when the
    /// transcript has been thrown away and is being read again from the start, and one
    /// record's artifacts when `absorb` has replaced that record.
    fn refresh_entry_cache(&mut self, cx: &mut Context<Self>) {
        let (generation, home_directory, overwritten_uuids) = {
            let store = self.store.read(cx);
            (
                store.transcript_generation(),
                store.home_directory().map(Path::to_path_buf),
                store.transcript().overwritten_uuids().to_vec(),
            )
        };

        // A cached entry carries the decision of whether its persisted output can be
        // read back, and that decision was made against the home directory known at the
        // time. The first scan of a remote project turns that from unknown into an
        // answer, so entries derived before it have to be derived again.
        if generation != self.cached_transcript_generation
            || home_directory != self.cached_home_directory
        {
            self.cached_transcript_generation = generation;
            self.cached_home_directory = home_directory;
            self.entry_cache = EntryCache::default();
            self.overwrites_dropped = 0;
        }

        if let Some(new_overwrites) = overwritten_uuids.get(self.overwrites_dropped..) {
            for uuid in new_overwrites {
                self.entry_cache.forget_record(uuid);
            }
        }
        self.overwrites_dropped = overwritten_uuids.len();
    }

    /// Every key that may address a cached artifact, including the attachment keys that
    /// live inside the single context entry rather than being entries themselves.
    fn live_cache_keys(&self) -> HashSet<SharedString> {
        let mut keys = HashSet::default();
        for entry in &self.entries {
            keys.insert(entry.key.clone());
            match &entry.kind {
                // An output is clamped behind an expansion key of its own, which has to
                // survive the rebuild that runs on every change to the transcript, or an
                // output the reader has opened closes itself four times a second while
                // the session is answering.
                EntryKind::ToolResult { .. } => {
                    keys.insert(output_expansion_key(&entry.key));
                }
                EntryKind::Attachments { items } => {
                    for item in items {
                        keys.insert(item.key.clone());
                        keys.insert(output_expansion_key(&item.key));
                    }
                }
                _ => {}
            }
        }
        keys
    }

    fn markdown_for(
        &mut self,
        key: &SharedString,
        source: SharedString,
        cx: &mut Context<Self>,
    ) -> Entity<Markdown> {
        if let Some(existing) = self.markdowns.get(key).cloned() {
            if existing.read(cx).source() != &source {
                existing.update(cx, |markdown, cx| markdown.reset(source, cx));
            }
            return existing;
        }

        // Fetched here rather than held in a field so that this crate does not have to
        // depend on `language` just to name the registry's type.
        let language_registry = self
            .workspace
            .upgrade()
            .map(|workspace| workspace.read(cx).project().read(cx).languages().clone());
        let markdown = cx.new(|cx| Markdown::new(source, language_registry, None, cx));
        self.markdowns.insert(key.clone(), markdown.clone());
        markdown
    }

    fn load_full_output(
        &mut self,
        key: SharedString,
        path: PathBuf,
        entry_index: usize,
        cx: &mut Context<Self>,
    ) {
        // The boundary is enforced here as well as where the button is drawn, so that the
        // path is checked on the line before it is opened rather than only where the
        // offer was made.
        let home_directory = self.store.read(cx).home_directory().map(Path::to_path_buf);
        if !persisted_output_is_loadable(&path, home_directory.as_deref()) {
            return;
        }

        if matches!(
            self.loaded_outputs.get(&key),
            Some(OutputLoad::Loading) | Some(OutputLoad::Loaded(_))
        ) {
            return;
        }

        self.loaded_outputs.insert(key.clone(), OutputLoad::Loading);
        let read = self.source.read_file(path, MAX_PERSISTED_OUTPUT_BYTES);
        let load = cx.spawn({
            let key = key.clone();
            async move |this, cx| {
                let result = read.await;
                this.update(cx, |this, cx| {
                    let load = match result {
                        Ok(contents) => OutputLoad::Loaded(persisted_output_text(contents).into()),
                        Err(error) => OutputLoad::Failed(format!("{error:#}").into()),
                    };
                    this.loaded_outputs.insert(key, load);
                    this.list_state
                        .remeasure_items(entry_index..entry_index.saturating_add(1));
                    cx.notify();
                })
                .log_err();
            }
        });
        // Held so that the read is cancelled if the entry it belongs to goes away.
        self.output_loads.insert(key, load);
        cx.notify();
    }

    fn display_working_directory(&self, working_directory: &Path) -> SharedString {
        if let Some(project_root) = self.project_root.as_deref() {
            if let Ok(relative) = working_directory.strip_prefix(project_root) {
                let relative = relative.to_string_lossy();
                return if relative.is_empty() {
                    SharedString::from(".")
                } else {
                    SharedString::from(relative.into_owned())
                };
            }
        }

        match working_directory.strip_prefix(paths::home_dir()) {
            Ok(relative) => SharedString::from(format!("~/{}", relative.to_string_lossy())),
            Err(_) => SharedString::from(working_directory.to_string_lossy().into_owned()),
        }
    }

    fn render_session_section(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let sessions = store.sessions().to_vec();
        let selected_process_id = store.selected();
        let error = store.error().cloned();
        let selected_name = sessions
            .iter()
            .find(|session| selected_process_id == Some(session.process_id))
            .map(|session| {
                session
                    .name
                    .clone()
                    .unwrap_or_else(|| session.session_id.clone())
            });
        let summary = collapsed_sessions_summary(sessions.len(), selected_name.as_deref());
        let is_expanded = self.session_list_expanded;

        v_flex()
            .child(
                h_flex()
                    .p_1()
                    .gap_1()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_1()
                            .overflow_hidden()
                            .child(
                                Disclosure::new("claude-sessions-list-disclosure", is_expanded)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.toggle_session_list(cx)),
                                    ),
                            )
                            .child(
                                Label::new("Claude Sessions")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    // Collapsed, this line is all that is left of the list, so it carries
                    // what the rows were saying: how many are running and which one the
                    // conversation below belongs to.
                    .child(
                        Label::new(summary)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .single_line(),
                    ),
            )
            .when_some(error, |this, error| {
                this.child(
                    div().px_2().pb_1().child(
                        Label::new(error)
                            .size(LabelSize::XSmall)
                            .color(Color::Error),
                    ),
                )
            })
            .when(is_expanded, |this| {
                this.child(
                    v_flex()
                        .id("claude-sessions-list")
                        .max_h(px(200.))
                        .overflow_y_scroll()
                        .when(sessions.is_empty(), |this| {
                            this.child(
                                div().p_2().child(
                                    Label::new(NO_SESSIONS)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                            )
                        })
                        .children(sessions.iter().enumerate().map(|(index, session)| {
                            let is_selected = selected_process_id == Some(session.process_id);
                            self.render_session_row(index, session, is_selected, cx)
                        })),
                )
            })
    }

    fn render_session_row(
        &self,
        index: usize,
        session: &RegisteredSession,
        is_selected: bool,
        cx: &mut Context<Self>,
    ) -> ListItem {
        let process_id = session.process_id;
        let is_busy = session.status.as_deref() == Some("busy");
        let name = SharedString::from(
            session
                .name
                .clone()
                .unwrap_or_else(|| session.session_id.clone()),
        );
        let working_directory = self.display_working_directory(&session.working_directory);
        let version = SharedString::from(format!("v{}", session.version));
        let tmux_target = session.tmux_target.clone().map(SharedString::from);
        let is_bridged = session.bridge_session_id.is_some();

        // Wrapped so that the pulse can be applied to the element: the icon itself
        // carries a colour rather than an opacity.
        let indicator = div().child(Icon::new(IconName::Indicator).size(IconSize::XSmall).color(
            if is_busy {
                Color::Success
            } else {
                Color::Hidden
            },
        ));
        // A still dot says a session is busy; a pulsing one says it is busy right now.
        let indicator = if is_busy {
            indicator
                .with_animation(
                    SharedString::from(format!("claude-session-indicator-{index}")),
                    Animation::new(PULSE_PERIOD)
                        .repeat()
                        .with_easing(pulsating_between(0.2, 0.8)),
                    |indicator, delta| indicator.opacity(delta),
                )
                .into_any_element()
        } else {
            indicator.into_any_element()
        };

        ListItem::new(SharedString::from(format!("claude-session-{index}")))
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(is_selected)
            .start_slot(indicator)
            .child(
                v_flex()
                    .gap_0p5()
                    .overflow_hidden()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Label::new(name).size(LabelSize::Small).single_line())
                            .when(is_bridged, |this| {
                                this.child(
                                    Icon::new(IconName::Link)
                                        .size(IconSize::XSmall)
                                        .color(Color::Accent),
                                )
                            })
                            .child(
                                Label::new(version)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .overflow_hidden()
                            .child(
                                Label::new(working_directory)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .single_line(),
                            )
                            .children(tmux_target.map(|target| {
                                Label::new(target)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Hidden)
                                    .single_line()
                            })),
                    ),
            )
            .tooltip(Tooltip::text(format!("pid {process_id}")))
            .on_click(cx.listener(move |this, _, _, cx| this.select_session(process_id, cx)))
    }

    /// The row of conversations the selected session has to offer: its own, then one chip
    /// per agent it has spawned.
    ///
    /// `None` when there is nothing to choose between — no session, or a session that has
    /// spawned nothing — so that the row does not take a line of the panel from the
    /// conversation for the sake of a single chip.
    fn render_agent_chips(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        // Read out whole before any listener is built: the store's borrow and the
        // `cx.listener` calls below cannot be held at the same time.
        let (subagents, target, states) = {
            let store = self.store.read(cx);
            store.selected()?;
            let subagents = store.subagents().to_vec();
            // The states come off the session's own conversation, which is followed
            // whichever one is on screen, so a chip stays truthful while its own agent's
            // records are the ones being drawn.
            let main_path = store.main_transcript().active_path();
            let states: Vec<AgentState> = subagents
                .iter()
                .map(|summary| agent_state(summary, &main_path))
                .collect();
            (subagents, store.transcript_target().clone(), states)
        };

        if subagents.is_empty() && target == TranscriptTarget::Main {
            return None;
        }

        let mut row = h_flex()
            .id("claude-session-agent-chips")
            .w_full()
            .flex_none()
            .px_2()
            .py_1()
            .gap_1()
            // The chips are as many as the session has spawned, which is more than fits
            // across a side panel; scrolling them keeps the panel its own width.
            .overflow_x_scroll()
            .child(self.render_agent_chip(
                "main",
                SharedString::from(MAIN_CONVERSATION_CHIP),
                None,
                target == TranscriptTarget::Main,
                false,
                TranscriptTarget::Main,
                cx,
            ));

        for (index, summary) in subagents.iter().enumerate() {
            let chip_target = TranscriptTarget::Subagent {
                agent_id: summary.agent_id.clone(),
                workflow_run_id: summary.workflow_run_id.clone(),
            };
            let is_selected = target == chip_target;
            let is_running = states.get(index).copied() == Some(AgentState::Running);
            row = row.child(self.render_agent_chip(
                &format!("agent-{index}"),
                agent_chip_label(summary),
                Some(agent_chip_tooltip(summary)),
                is_selected,
                is_running,
                chip_target,
                cx,
            ));
        }

        Some(row.into_any_element())
    }

    /// One chip. An agent that has returned stays on the row rather than being taken off
    /// it — its conversation is the thing worth reading afterwards — and says so by being
    /// the quiet one.
    fn render_agent_chip(
        &self,
        id: &str,
        label: SharedString,
        tooltip: Option<SharedString>,
        is_selected: bool,
        is_running: bool,
        target: TranscriptTarget,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let button = Button::new(
            SharedString::from(format!("claude-session-chip-{id}")),
            label,
        )
        .label_size(LabelSize::XSmall)
        .color(if is_running {
            Color::Default
        } else {
            Color::Muted
        })
        .toggle_state(is_selected)
        .selected_style(ButtonStyle::Tinted(TintColor::Accent))
        .when_some(tooltip, |this, tooltip| {
            this.tooltip(Tooltip::text(tooltip))
        })
        .on_click(
            cx.listener(move |this, _, _, cx| this.select_transcript_target(target.clone(), cx)),
        );

        if !is_running {
            return button.into_any_element();
        }

        // Wrapped so that the pulse applies to the element: the icon itself carries a
        // colour rather than an opacity.
        let indicator = div()
            .child(
                Icon::new(IconName::Indicator)
                    .size(IconSize::XSmall)
                    .color(Color::Success),
            )
            .with_animation(
                SharedString::from(format!("claude-session-chip-pulse-{id}")),
                Animation::new(PULSE_PERIOD)
                    .repeat()
                    .with_easing(pulsating_between(0.2, 0.8)),
                |indicator, delta| indicator.opacity(delta),
            );

        h_flex()
            .flex_none()
            .gap_0p5()
            .child(indicator)
            .child(button)
            .into_any_element()
    }

    /// The agents one call started, as the card under that call draws them. Empty for a
    /// call this panel cannot pair with anything on disk: a card saying nothing about an
    /// agent is worse than no card.
    fn agent_cards(&self, key: &SharedString, cx: &Context<Self>) -> Vec<AgentCardRow> {
        let Some(call) = self.agent_calls.get(key) else {
            return Vec::new();
        };

        let store = self.store.read(cx);
        let main_path = store.main_transcript().active_path();
        subagents_of_call(call, store.subagents(), &main_path)
            .into_iter()
            .map(|summary| AgentCardRow {
                label: agent_chip_label(summary),
                state: agent_state(summary, &main_path),
                target: TranscriptTarget::Subagent {
                    agent_id: summary.agent_id.clone(),
                    workflow_run_id: summary.workflow_run_id.clone(),
                },
            })
            .collect()
    }

    fn render_agent_card(
        &self,
        key: &SharedString,
        index: usize,
        card: AgentCardRow,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (state_note, state_color) = match card.state {
            AgentState::Running => ("Running", Color::Success),
            AgentState::Finished => ("Finished", Color::Muted),
        };
        let target = card.target;

        h_flex()
            .w_full()
            .px_2()
            .pb_1()
            .gap_1()
            .justify_between()
            .child(
                v_flex()
                    .overflow_hidden()
                    .child(Label::new(card.label).size(LabelSize::XSmall).single_line())
                    .child(
                        Label::new(state_note)
                            .size(LabelSize::XSmall)
                            .color(state_color),
                    ),
            )
            .child(
                Button::new(
                    SharedString::from(format!("claude-session-open-agent-{key}-{index}")),
                    "Open",
                )
                .end_icon(Icon::new(IconName::ArrowRight).size(IconSize::XSmall))
                .label_size(LabelSize::XSmall)
                .tooltip(Tooltip::text("Read this agent's conversation"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.select_transcript_target(target.clone(), cx)
                })),
            )
            .into_any_element()
    }

    fn render_transcript_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let has_selection = store.selected().is_some();
        let has_transcript_file = store.transcript_path().is_some();

        if !has_selection {
            return Self::render_placeholder(SELECT_A_SESSION);
        }

        if self.entries.is_empty() {
            return Self::render_placeholder(if has_transcript_file {
                EMPTY_TRANSCRIPT
            } else {
                WAITING_FOR_TRANSCRIPT
            });
        }

        // Arriving back at the bottom is having read what landed there, and this is where
        // a scroll that got there is noticed: the list notifies this view on every
        // scroll, so the panel is drawn again with the answer below already updated.
        // `None` — a list whose new items have not been measured yet, which is exactly
        // what a splice leaves behind — is not an answer, and clearing the count on it
        // would throw away the arrival that caused the splice.
        if self.list_state.is_scrolled_to_end() == Some(true) {
            self.unread_below.scrolled(true);
        }
        let unread_below = self.unread_below.count();

        v_flex()
            .flex_grow_1()
            .overflow_hidden()
            .child(
                list(
                    self.list_state.clone(),
                    cx.processor(|this, index: usize, window, cx| {
                        this.render_entry(index, window, cx)
                    }),
                )
                .with_sizing_behavior(ListSizingBehavior::Auto)
                .flex_grow_1(),
            )
            // A reader who has scrolled up is not followed to the tail, so the messages
            // that arrive land out of sight and the session looks like it has stopped.
            // This is the only sign that it has not.
            .when(unread_below > 0, |this| {
                this.child(
                    h_flex().w_full().px_2().pb_1().justify_center().child(
                        Button::new(
                            "claude-session-scroll-to-latest",
                            // Counted in entries rather than messages, because one
                            // reply arrives as several of them: thinking, a tool call,
                            // its result, then the text. Saying "messages" would name a
                            // number the reader cannot match to what they scroll past.
                            if unread_below == 1 {
                                "1 new below".to_string()
                            } else {
                                format!("{unread_below} new below")
                            },
                        )
                        .end_icon(Icon::new(IconName::ArrowDown).size(IconSize::XSmall))
                        .label_size(LabelSize::XSmall)
                        .tooltip(Tooltip::text("Jump to the newest message"))
                        .on_click(cx.listener(|this, _, _, cx| this.scroll_to_latest(cx))),
                    ),
                )
            })
            .into_any_element()
    }

    /// The one line above the input that says what the session is doing, or nothing at
    /// all for a session that is not doing anything: a line that is sometimes there and
    /// sometimes not would move the input under the reader's hands at the end of every
    /// turn. Read-only — the reader is being told something, not offered anything.
    fn render_activity(&self) -> Option<AnyElement> {
        let note = activity_label(&self.activity)?;

        Some(
            h_flex()
                .w_full()
                .px_2()
                .pb_1()
                .gap_1()
                .child(
                    Label::new(note)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .single_line()
                        .with_animation(
                            "claude-session-activity",
                            Animation::new(PULSE_PERIOD)
                                .repeat()
                                .with_easing(pulsating_between(0.4, 0.8)),
                            |note, delta| note.alpha(delta),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_placeholder(message: &'static str) -> AnyElement {
        div()
            .flex_grow_1()
            .p_2()
            .child(
                Label::new(message)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    /// Whether a reply can be typed at all.
    ///
    /// There has to be a pane to type into, and the conversation on screen has to be the
    /// session's own. An agent's conversation is something to read: the session's pane is
    /// the only thing there is to type into, so a reply sent from under an agent's records
    /// would arrive in a conversation the reader is not looking at.
    fn can_send(&self, cx: &App) -> bool {
        let store = self.store.read(cx);
        store.pane_target().is_some() && *store.transcript_target() == TranscriptTarget::Main
    }

    /// A session Zed has no pane to type into can only be read, and so can no session at
    /// all; the input is disabled in both cases rather than hidden, so that the reason is
    /// visible where the reply would be typed.
    fn sync_input_availability(&mut self, cx: &mut Context<Self>) {
        let read_only = !self.can_send(cx);
        self.message_editor.update(cx, |editor, cx| {
            if editor.read_only(cx) != read_only {
                editor.set_read_only(read_only);
                cx.notify();
            }
        });
    }

    /// Only ever reached from a user gesture — the Send button, or the binding on the
    /// input's `enter`.
    fn send_message(&mut self, _: &SendMessage, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_send(cx) {
            return;
        }

        let text = self.message_editor.read(cx).text(cx);
        if text.trim().is_empty() {
            return;
        }

        // Nothing to send to means nothing to draw the message against; `pane_target`
        // has already answered that there is a session here.
        let Some(process_id) = self.store.read(cx).selected() else {
            return;
        };

        let send = self.store.update(cx, |store, cx| {
            store.send_input(SessionInput::Text(text.clone()), cx)
        });
        // Emptied before the send is answered: the answer can be a round trip to
        // another machine away, and the input has to be usable again at once.
        self.message_editor
            .update(cx, |editor, cx| editor.clear(window, cx));
        // Drawn from here rather than when the send is answered: the CLI writes the
        // user's record only when it starts the turn, so on a busy session the text
        // would be nowhere at all for as long as that turn takes.
        let pending_send = self
            .pending_sends
            .remember(process_id, &text, &self.entries);
        self.rebuild_entries(cx);
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let outcome = send.await;
            this.update_in(cx, |this, window, cx| {
                this.pending_sends.resolve(pending_send, &outcome);
                this.rebuild_entries(cx);
                apply_send_outcome(&this.message_editor, &text, outcome, window, cx);
                cx.notify();
            })
            .log_err();
        })
        // Detached rather than held in a field: dropping it would cancel the send
        // itself, and a second send must not cut the first one short.
        .detach();
    }

    /// Takes down one pending message. Reached only from the button on that message: a
    /// message that never turns up in the transcript is left where it is until the user
    /// says otherwise, because dropping it on a timer is the very thing that made the
    /// text look lost in the first place.
    fn dismiss_pending_send(&mut self, id: u64, cx: &mut Context<Self>) {
        self.pending_sends.dismiss(id);
        self.rebuild_entries(cx);
        cx.notify();
    }

    fn interrupt_session(&mut self, _: &Interrupt, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_send(cx) {
            return;
        }

        self.store.update(cx, |store, cx| {
            // There is no text to hand back for an interrupt, and the store reports the
            // failure itself, so nothing here waits for the answer.
            store.send_input(SessionInput::Escape, cx).detach()
        });
    }

    fn render_input(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let can_send = self.can_send(cx);
        let store = self.store.read(cx);
        let has_selection = store.selected().is_some();
        let is_reading_an_agent =
            matches!(store.transcript_target(), TranscriptTarget::Subagent { .. });
        let note = input_note(can_send, is_reading_an_agent, has_selection);

        v_flex()
            .w_full()
            .p_2()
            .gap_1()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .key_context("ClaudeSessionsInput")
            .on_action(cx.listener(Self::send_message))
            .on_action(cx.listener(Self::interrupt_session))
            .when_some(note, |this, note| {
                this.child(
                    h_flex()
                        .gap_1()
                        .child(
                            Icon::new(IconName::Lock)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(Label::new(note).size(LabelSize::XSmall).color(Color::Muted)),
                )
            })
            .child(
                div()
                    .w_full()
                    .px_1()
                    .py_0p5()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .child(self.message_editor.clone()),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .justify_end()
                    .child(
                        Button::new("claude-session-interrupt", "Esc")
                            .start_icon(Icon::new(IconName::Escape).size(IconSize::XSmall))
                            .label_size(LabelSize::XSmall)
                            .disabled(!can_send)
                            .tooltip(Tooltip::text("Interrupt this session"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.interrupt_session(&Interrupt, window, cx)
                            })),
                    )
                    .child(
                        Button::new("claude-session-send", "Send")
                            .start_icon(Icon::new(IconName::Send).size(IconSize::XSmall))
                            .label_size(LabelSize::XSmall)
                            .disabled(!can_send)
                            .tooltip(Tooltip::text("Send to this session"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.send_message(&SendMessage, window, cx)
                            })),
                    ),
            )
    }

    fn render_entry(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(entry) = self.entries.get(index).cloned() else {
            return div().into_any_element();
        };
        let key = entry.key;
        let is_expanded = self.expanded.contains(&key);

        match entry.kind {
            EntryKind::Message { role, source } => {
                let markdown = self.markdown_for(&key, source, cx);
                v_flex()
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_0p5()
                    .border_l_2()
                    .border_color(role.color().color(cx).opacity(ROLE_RAIL_OPACITY))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Icon::new(role.icon())
                                    .size(IconSize::XSmall)
                                    .color(role.color()),
                            )
                            .child(
                                Label::new(role.label())
                                    .size(LabelSize::XSmall)
                                    .color(role.color()),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .child(MarkdownElement::new(markdown, markdown_style(window, cx))),
                    )
                    .into_any_element()
            }

            EntryKind::Thinking { source } => {
                let header = self.render_disclosure_header(
                    DisclosureHeader {
                        entry_index: index,
                        key: &key,
                        icon: IconName::ToolThink,
                        icon_color: Color::Muted,
                        title: "Thinking".into(),
                        summary: Some(first_line(&source)),
                        is_expanded,
                    },
                    cx,
                );
                let body = if is_expanded {
                    let markdown = self.markdown_for(&key, source, cx);
                    Some(
                        div()
                            .w_full()
                            .px_2()
                            .pb_1()
                            .child(MarkdownElement::new(markdown, markdown_style(window, cx))),
                    )
                } else {
                    None
                };
                v_flex()
                    .w_full()
                    .child(header)
                    .children(body)
                    .into_any_element()
            }

            EntryKind::ToolUse { name, input } => {
                let display = tool_input_display(&name, &input);
                let header = self.render_disclosure_header(
                    DisclosureHeader {
                        entry_index: index,
                        key: &key,
                        icon: IconName::ToolHammer,
                        icon_color: Color::Success,
                        title: name,
                        summary: Some(first_line(&display.text)),
                        is_expanded,
                    },
                    cx,
                );
                let body = if is_expanded {
                    let markdown = self.markdown_for(&key, display.code_block(), cx);
                    Some(
                        div()
                            .w_full()
                            .px_2()
                            .pb_1()
                            .child(MarkdownElement::new(markdown, markdown_style(window, cx))),
                    )
                } else {
                    None
                };
                // Drawn whether or not the call is expanded: what the call started, and
                // whether it is still going, is the part of an `Agent` call worth seeing
                // without opening anything.
                let cards = self.agent_cards(&key, cx);
                let mut element = v_flex().w_full().child(header).children(body);
                for (card_index, card) in cards.into_iter().enumerate() {
                    element = element.child(self.render_agent_card(&key, card_index, card, cx));
                }
                element.into_any_element()
            }

            EntryKind::ToolResult {
                label,
                is_error,
                body,
            } => {
                let summary = match &body {
                    ToolResultBody::Inline(text) => first_line(text),
                    // The inline text of a persisted result is the output's own first
                    // tens of kilobytes, so the collapsed row still shows where it
                    // starts, with the size of the whole file after it.
                    ToolResultBody::Persisted(persisted) => {
                        let first_line = first_line(&persisted.preview);
                        if first_line.is_empty() {
                            SharedString::from(format!("{} saved to a file", persisted.size))
                        } else {
                            SharedString::from(format!(
                                "{first_line} ({} saved to a file)",
                                persisted.size
                            ))
                        }
                    }
                };
                let header = self.render_disclosure_header(
                    DisclosureHeader {
                        entry_index: index,
                        key: &key,
                        icon: if is_error {
                            IconName::XCircle
                        } else {
                            IconName::ToolTerminal
                        },
                        icon_color: if is_error { Color::Error } else { Color::Muted },
                        title: label,
                        summary: Some(summary),
                        is_expanded,
                    },
                    cx,
                );

                let rendered_body = if is_expanded {
                    Some(match body {
                        ToolResultBody::Inline(text) => {
                            let output = self.render_tool_output(index, &key, &text, window, cx);
                            div()
                                .w_full()
                                .px_2()
                                .pb_1()
                                .child(output)
                                .into_any_element()
                        }
                        ToolResultBody::Persisted(persisted) => {
                            self.render_persisted_output(index, &key, &persisted, window, cx)
                        }
                    })
                } else {
                    None
                };

                v_flex()
                    .w_full()
                    .child(header)
                    .children(rendered_body)
                    .into_any_element()
            }

            EntryKind::Image { image, media_type } => v_flex()
                .w_full()
                .px_2()
                .py_1()
                .gap_0p5()
                .child(
                    Label::new(media_type)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(img(image).max_w_full().max_h(px(480.)))
                .into_any_element(),

            EntryKind::LocalCommand { text } => {
                let markdown = self.markdown_for(&key, fenced_code(&text, ""), cx);
                v_flex()
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Icon::new(IconName::Terminal)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new("Local command")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(MarkdownElement::new(markdown, markdown_style(window, cx)))
                    .into_any_element()
            }

            EntryKind::SlashCommand { text } => v_flex()
                .w_full()
                .px_2()
                .py_1()
                .gap_0p5()
                .border_l_2()
                .border_color(
                    MessageRole::User
                        .color()
                        .color(cx)
                        .opacity(ROLE_RAIL_OPACITY),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Icon::new(MessageRole::User.icon())
                                .size(IconSize::XSmall)
                                .color(MessageRole::User.color()),
                        )
                        .child(
                            Label::new(MessageRole::User.label())
                                .size(LabelSize::XSmall)
                                .color(MessageRole::User.color()),
                        ),
                )
                // Drawn as code rather than as prose: what the user typed was a command
                // to the CLI, and the rest of the record was Claude Code's markup for it.
                .child(
                    Label::new(text)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .inline_code(cx),
                )
                .into_any_element(),

            EntryKind::Pending { id, text } => {
                v_flex()
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_0p5()
                    .border_l_2()
                    .border_color(
                        MessageRole::User
                            .color()
                            .color(cx)
                            .opacity(ROLE_RAIL_OPACITY),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .justify_between()
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Icon::new(MessageRole::User.icon())
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        Label::new(SENDING_NOTE)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .with_animation(
                                        SharedString::from(format!("pending-send-{id}")),
                                        Animation::new(PULSE_PERIOD)
                                            .repeat()
                                            .with_easing(pulsating_between(0.4, 0.8)),
                                        |header, delta| header.opacity(delta),
                                    ),
                            )
                            .child(
                                IconButton::new(
                                    SharedString::from(format!("dismiss-pending-send-{id}")),
                                    IconName::Close,
                                )
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text("Remove this message"))
                                .on_click(cx.listener(
                                    move |this, _, _, cx| this.dismiss_pending_send(id, cx),
                                )),
                            ),
                    )
                    // A plain label rather than markdown: nothing about a message that has
                    // not arrived is worth deriving, and the record that replaces it is
                    // where the rendered form belongs.
                    .child(
                        Label::new(text)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .italic(),
                    )
                    .into_any_element()
            }

            EntryKind::CompactBoundary {
                trigger,
                pre_tokens,
                post_tokens,
            } => {
                let tokens = match (pre_tokens, post_tokens) {
                    (Some(pre), Some(post)) => format!("{pre} -> {post} tokens"),
                    (Some(pre), None) => format!("from {pre} tokens"),
                    (None, Some(post)) => format!("to {post} tokens"),
                    (None, None) => String::new(),
                };
                let toggle_label = if self.show_full_history {
                    "Hide pre-compaction history"
                } else {
                    "Show pre-compaction history"
                };

                v_flex()
                    .w_full()
                    .px_2()
                    .py_2()
                    .gap_1()
                    .child(Divider::horizontal_dashed())
                    .child(
                        h_flex()
                            .gap_1()
                            .flex_wrap()
                            .child(
                                Icon::new(IconName::Compact)
                                    .size(IconSize::XSmall)
                                    .color(Color::Accent),
                            )
                            .child(
                                Label::new(format!("Compacted ({trigger})"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Accent),
                            )
                            .when(!tokens.is_empty(), |this| {
                                this.child(
                                    Label::new(tokens)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                            }),
                    )
                    .child(
                        Button::new(
                            SharedString::from(format!("compact-history-{index}")),
                            toggle_label,
                        )
                        .label_size(LabelSize::XSmall)
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_full_history(cx))),
                    )
                    .child(Divider::horizontal_dashed())
                    .into_any_element()
            }

            EntryKind::Attachments { items } => {
                let header = self.render_disclosure_header(
                    DisclosureHeader {
                        entry_index: index,
                        key: &key,
                        icon: IconName::Paperclip,
                        icon_color: Color::Muted,
                        title: "Context".into(),
                        summary: Some(SharedString::from(format!("{} attachments", items.len()))),
                        is_expanded,
                    },
                    cx,
                );

                let mut section = v_flex().w_full().child(header);
                if is_expanded {
                    for item in items {
                        section = section.child(self.render_attachment(index, &item, window, cx));
                    }
                }
                section.into_any_element()
            }

            EntryKind::Unknown { label, raw } => {
                let header = self.render_disclosure_header(
                    DisclosureHeader {
                        entry_index: index,
                        key: &key,
                        icon: IconName::Json,
                        icon_color: Color::Hidden,
                        title: SharedString::from(format!("Unrecognized: {label}")),
                        summary: None,
                        is_expanded,
                    },
                    cx,
                );
                let body = if is_expanded {
                    let markdown = self.markdown_for(&key, fenced_code(&raw, "json"), cx);
                    Some(
                        div()
                            .w_full()
                            .px_2()
                            .pb_1()
                            .child(MarkdownElement::new(markdown, markdown_style(window, cx))),
                    )
                } else {
                    None
                };
                v_flex()
                    .w_full()
                    .child(header)
                    .children(body)
                    .into_any_element()
            }
        }
    }

    fn render_attachment(
        &mut self,
        entry_index: usize,
        item: &AttachmentItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_expanded = self.expanded.contains(&item.key);
        let toggle_key = item.key.clone();
        let click_key = item.key.clone();

        let header = ListItem::new(SharedString::from(format!("attachment-{}", item.key)))
            .spacing(ListItemSpacing::Sparse)
            .indent_level(1)
            .toggle(is_expanded)
            .on_toggle(cx.listener(move |this, _, _, cx| {
                this.toggle_expanded(toggle_key.clone(), entry_index, cx)
            }))
            .child(
                Label::new(item.label.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .single_line(),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_expanded(click_key.clone(), entry_index, cx)
            }));

        let body = if is_expanded {
            Some(match &item.persisted {
                Some(persisted) => {
                    self.render_persisted_output(entry_index, &item.key, persisted, window, cx)
                }
                None => {
                    let markdown = self.markdown_for(&item.key, fenced_code(&item.body, ""), cx);
                    div()
                        .w_full()
                        .pl_4()
                        .pr_2()
                        .pb_1()
                        .child(MarkdownElement::new(markdown, markdown_style(window, cx)))
                        .into_any_element()
                }
            })
        } else {
            None
        };

        v_flex()
            .w_full()
            .child(header)
            .children(body)
            .into_any_element()
    }

    fn render_persisted_output(
        &mut self,
        entry_index: usize,
        key: &SharedString,
        persisted: &PersistedOutput,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let load = self.loaded_outputs.get(key).cloned();
        let home_directory = self.store.read(cx).home_directory().map(Path::to_path_buf);
        let displayed_text = match &load {
            Some(OutputLoad::Loaded(text)) => text.clone(),
            _ => persisted.preview.clone(),
        };
        let path_label = SharedString::from(persisted.path.to_string_lossy().into_owned());
        let load_key = key.clone();
        let load_path = persisted.path.clone();

        let action = match &load {
            Some(OutputLoad::Loaded(_)) => None,
            Some(OutputLoad::Failed(error)) => Some(
                Label::new(error.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Error)
                    .into_any_element(),
            ),
            Some(OutputLoad::Loading) => Some(
                Label::new("Loading…")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .into_any_element(),
            ),
            None if persisted_output_is_loadable(&persisted.path, home_directory.as_deref()) => {
                Some(
                    Button::new(
                        SharedString::from(format!("load-output-{entry_index}")),
                        "Load full output",
                    )
                    .start_icon(Icon::new(IconName::CloudDownload).size(IconSize::XSmall))
                    .label_size(LabelSize::XSmall)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.load_full_output(load_key.clone(), load_path.clone(), entry_index, cx)
                    }))
                    .into_any_element(),
                )
            }
            None => None,
        };
        let output = self.render_tool_output(entry_index, key, &displayed_text, window, cx);

        v_flex()
            .w_full()
            .px_2()
            .pb_1()
            .gap_1()
            .child(
                Label::new(format!("Output too large ({}); saved to", persisted.size))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new(path_label)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .single_line(),
            )
            .child(output)
            .children(action)
            .into_any_element()
    }

    /// The framed card a tool's output is drawn in: clamped to its first lines, with a
    /// disclosure for the rest, when the output is long.
    fn render_tool_output(
        &mut self,
        entry_index: usize,
        key: &SharedString,
        text: &SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let card = div()
            .w_full()
            .rounded_sm()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().editor_background);

        if text.trim().is_empty() {
            return card
                .px_1p5()
                .py_1()
                .child(
                    Label::new(NO_OUTPUT_NOTE)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element();
        }

        let output_key = output_expansion_key(key);
        let is_expanded = self.expanded.contains(&output_key);
        let is_clamped = output_is_clamped(text, MAX_UNCLAMPED_OUTPUT_LINES);
        let shown = if is_clamped && !is_expanded {
            SharedString::from(clamped_output(text, MAX_UNCLAMPED_OUTPUT_LINES).to_string())
        } else {
            text.clone()
        };
        let markdown = self.markdown_for(key, fenced_code(&shown, ""), cx);
        let disclosure = is_clamped.then(|| {
            let title = if is_expanded {
                "Show less"
            } else {
                "Show the whole output"
            };
            self.render_disclosure_header(
                DisclosureHeader {
                    entry_index,
                    key: &output_key,
                    icon: IconName::Ellipsis,
                    icon_color: Color::Muted,
                    title: title.into(),
                    summary: None,
                    is_expanded,
                },
                cx,
            )
        });

        v_flex()
            .w_full()
            .gap_0p5()
            .child(card.child(MarkdownElement::new(
                markdown,
                tool_output_markdown_style(window, cx),
            )))
            .children(disclosure)
            .into_any_element()
    }

    fn render_disclosure_header(
        &self,
        header: DisclosureHeader<'_>,
        cx: &mut Context<Self>,
    ) -> ListItem {
        let DisclosureHeader {
            entry_index,
            key,
            icon,
            icon_color,
            title,
            summary,
            is_expanded,
        } = header;
        let toggle_key = key.clone();
        let click_key = key.clone();

        // Identified by the entry's key rather than by its position, so that the row a
        // reader is hovering keeps its state as the conversation grows above it, and so
        // that an output's own disclosure is not the same element as its entry's header.
        ListItem::new(SharedString::from(format!("entry-{key}")))
            .spacing(ListItemSpacing::Sparse)
            .toggle(is_expanded)
            .on_toggle(cx.listener(move |this, _, _, cx| {
                this.toggle_expanded(toggle_key.clone(), entry_index, cx)
            }))
            .start_slot(Icon::new(icon).size(IconSize::XSmall).color(icon_color))
            .child(
                h_flex()
                    .gap_1()
                    .overflow_hidden()
                    .child(Label::new(title).size(LabelSize::Small).single_line())
                    .children(summary.map(|summary| {
                        Label::new(summary)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .single_line()
                    })),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_expanded(click_key.clone(), entry_index, cx)
            }))
    }
}

impl Render for ClaudeSessionsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("ClaudeSessionsPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(self.render_session_section(cx))
            .child(Divider::horizontal())
            .children(self.render_agent_chips(cx))
            .child(self.render_transcript_section(cx))
            .children(self.render_activity())
            .child(self.render_input(cx))
    }
}

impl Focusable for ClaudeSessionsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ClaudeSessionsPanel {}

impl Panel for ClaudeSessionsPanel {
    fn persistent_name() -> &'static str {
        "ClaudeSessionsPanel"
    }

    fn panel_key() -> &'static str {
        CLAUDE_SESSIONS_PANEL_KEY
    }

    fn activation_focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn position(&self, _window: &Window, cx: &App) -> DockPosition {
        dock_position(ClaudeSessionsSettings::get_global(cx).dock)
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    /// Written to the settings file rather than kept here, which is what makes the side
    /// the user dragged the panel to still be its side after a restart.
    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            let dock = match position {
                DockPosition::Left => DockSide::Left,
                // `position_is_valid` refuses the bottom dock, so this is only reached by
                // a caller that ignored it; the right-hand side is the default.
                DockPosition::Right | DockPosition::Bottom => DockSide::Right,
            };
            settings.claude_sessions.get_or_insert_default().dock = Some(dock);
        });
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(420.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::AiClaude)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Claude Sessions")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        11
    }
}

/// Answers a send that has come back.
///
/// A send that failed hands the user their text back: there is nothing they can do about
/// the failure, and the message they typed would otherwise be gone with only a line of
/// error to show for it. A send that arrived leaves the emptied input alone.
///
/// The text only goes back into an input that is still empty. The answer arrives after
/// the user was free to type again, and anything they have typed since is newer than
/// this: an input that holds one character of theirs is theirs. Nothing is ever sent
/// again on its own — the returned text is a draft the user resends or discards.
fn apply_send_outcome(
    message_editor: &Entity<Editor>,
    sent_text: &str,
    outcome: anyhow::Result<()>,
    window: &mut Window,
    cx: &mut App,
) {
    if outcome.is_ok() {
        return;
    }

    if !message_editor.read(cx).text(cx).is_empty() {
        return;
    }

    message_editor.update(cx, |editor, cx| editor.set_text(sent_text, window, cx));
}

/// One message that has left for a session and has not appeared in its transcript yet.
#[derive(Clone, PartialEq)]
struct PendingSend {
    id: u64,
    /// The session it was sent to. A pending message belongs to that conversation and
    /// does not follow the reader to another one.
    process_id: u32,
    /// Trimmed, because that is the form it is compared in: Claude Code appends its own
    /// context to the text it records, and [`user_visible_text`] hands back what is left
    /// of it trimmed.
    text: SharedString,
    /// The records that were already showing this same text when the send left. One of
    /// them cannot be the record this send will become, and this is what stops a message
    /// sent twice from being paired with the first record both times.
    preexisting_keys: HashSet<SharedString>,
    /// The newest of the user's own messages the conversation held when the send left,
    /// whatever its text. The record this send becomes is written after it, so nothing
    /// at or before it can be this send arriving — which is the only thing that holds
    /// when the conversation later shows more of itself than the send was measured
    /// against, as the history behind a compaction does when the reader opens it.
    newest_preexisting_key: Option<SharedString>,
}

/// The messages the panel is drawing on its own behalf, because the transcript has
/// nothing for them yet.
///
/// Claude Code writes the user's record when it *starts* the turn, not when the text
/// reaches its input, so on a busy session a sent message is in the terminal's input
/// queue and nowhere in the transcript for seconds or minutes. Without this the panel
/// showed nothing at all in that window, and the message looked like it had been
/// swallowed.
///
/// Nothing here is a timer: a pending message is taken down when the record it was sent
/// as arrives, when the send comes back a failure, when the reader leaves the session, or
/// when the reader dismisses it — never after a wait, because a message quietly vanishing
/// is the problem this solves.
#[derive(Default)]
struct PendingSends {
    sends: Vec<PendingSend>,
    next_id: u64,
}

impl PendingSends {
    /// Draws `text` as sent to `process_id`, against the conversation `entries` was built
    /// from, and reports the id that addresses it.
    fn remember(&mut self, process_id: u32, text: &str, entries: &[Entry]) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);

        let text = SharedString::from(text.trim().to_string());
        // Noted now rather than looked for later: these records are already in the
        // conversation, so none of them can be the one this send will be written as, and
        // a second send of the same text must not be paired with the first send's record.
        let messages = user_messages(entries);
        let newest_preexisting_key = messages.last().map(|(key, _)| key.clone());
        let preexisting_keys = messages
            .into_iter()
            .filter(|(_, source)| source.trim() == text.as_ref())
            .map(|(key, _)| key)
            .collect();

        self.sends.push(PendingSend {
            id,
            process_id,
            text,
            preexisting_keys,
            newest_preexisting_key,
        });
        id
    }

    /// Takes down the message the user dismissed, and only that one: two sends of the
    /// same text are two messages, and the button belongs to the one it sits on.
    fn dismiss(&mut self, id: u64) {
        self.sends.retain(|send| send.id != id);
    }

    /// A send that arrived is waiting for its record to be written. A send that failed
    /// never will have one, so its message comes down; the text itself goes back to the
    /// input, which is [`apply_send_outcome`]'s job.
    fn resolve(&mut self, id: u64, outcome: &anyhow::Result<()>) {
        if outcome.is_ok() {
            return;
        }
        self.dismiss(id);
    }

    /// Drops the messages sent to a session that is no longer the one being read.
    fn retain_session(&mut self, selected_process_id: Option<u32>) {
        self.sends
            .retain(|send| Some(send.process_id) == selected_process_id);
    }

    /// Takes down the messages whose records have turned up in `entries`.
    ///
    /// The record carries nothing that ties it back to a send, so the pairing is by text
    /// and by order: the oldest send that is still waiting takes the oldest record it
    /// could be, which is what makes two sends of the same text come down as their two
    /// records arrive rather than both at the first one.
    fn pair_with(&mut self, entries: &[Entry]) {
        // Walking the conversation for arrivals costs the length of the transcript, and
        // there is nothing waiting for them the overwhelming majority of the time.
        if self.is_empty() {
            return;
        }

        let arrivals = user_messages(entries);
        let mut claimed: HashSet<SharedString> = HashSet::default();
        let mut paired: HashSet<u64> = HashSet::default();

        for send in &self.sends {
            // Only the records written after the conversation the send was measured
            // against are candidates. Without this, a conversation that shows more of
            // itself than the send was measured against — the history behind a
            // compaction being opened — offers an older record of the same text, and the
            // message comes down having never arrived.
            //
            // A conversation that no longer holds that record leaves every arrival a
            // candidate: compaction runs as a turn starts, which is the same moment the
            // user's record is written, so the record a send was measured against can
            // be gone by the time the send's own turns up.
            let written_after_the_conversation_as_it_was = send
                .newest_preexisting_key
                .as_ref()
                .and_then(|newest| {
                    arrivals
                        .iter()
                        .position(|(key, _)| key == newest)
                        .map(|index| index.saturating_add(1))
                })
                .unwrap_or(0);
            let arrival = arrivals
                .get(written_after_the_conversation_as_it_was..)
                .unwrap_or_default()
                .iter()
                .find(|(key, source)| {
                    source.trim() == send.text.as_ref()
                        && !send.preexisting_keys.contains(key)
                        && !claimed.contains(key)
                });
            if let Some((key, _)) = arrival {
                claimed.insert(key.clone());
                paired.insert(send.id);
            }
        }

        self.sends.retain(|send| !paired.contains(&send.id));
    }

    fn entries(&self) -> Vec<Entry> {
        self.sends
            .iter()
            .map(|send| Entry {
                // Keyed outside the transcript's namespace: a record is keyed by its
                // uuid, and nothing derived from a pending message may be cached under
                // one.
                key: SharedString::from(format!("pending-{}", send.id)),
                kind: EntryKind::Pending {
                    id: send.id,
                    text: send.text.clone(),
                },
            })
            .collect()
    }

    fn is_empty(&self) -> bool {
        self.sends.is_empty()
    }
}

/// The user's own messages in the conversation, keyed as the entries are and in the order
/// they appear: what a pending message is waiting to be replaced by.
fn user_messages(entries: &[Entry]) -> Vec<(SharedString, SharedString)> {
    entries
        .iter()
        .filter_map(|entry| match &entry.kind {
            EntryKind::Message {
                role: MessageRole::User,
                source,
            } => Some((entry.key.clone(), source.clone())),
            // A slash command is the user's message too, and it is what a send of
            // `/compact` turns into.
            EntryKind::SlashCommand { text } => Some((entry.key.clone(), text.clone())),
            _ => None,
        })
        .collect()
}

/// Whether the pending messages are drawn into the conversation on screen.
///
/// They belong to the session's own conversation: the reader sent them to the session,
/// not to one of the agents it spawned, so nothing of theirs is drawn among an agent's
/// records. A message that is not drawn is not taken down — it goes on waiting for the
/// record it was sent as, and is drawn again as soon as the session's own conversation is
/// back on screen.
fn pending_is_drawn_in(target: &TranscriptTarget) -> bool {
    match target {
        TranscriptTarget::Main => true,
        TranscriptTarget::Subagent { .. } => false,
    }
}

/// The user's own messages in one conversation, as the entries [`build_entries`] would
/// give the same records, and nothing else the records hold.
///
/// This is all [`PendingSends`] reads of a conversation, and building that conversation
/// whole to get it would decode every image the session has ever pasted — a screenshot is
/// around a megabyte and a half of base64 — on each of the four rebuilds a second a
/// streaming reply causes. The keys have to be the ones the drawn conversation carries,
/// because that is what a pending message's bookkeeping is written in;
/// [`the_user_messages_read_off_a_path_are_the_ones_the_built_entries_carry`] holds the
/// two together.
fn user_message_entries(path: &[&TranscriptRecord]) -> Vec<Entry> {
    let mut entries = Vec::new();

    for (path_index, record) in path.iter().enumerate() {
        // A record of another kind holds no message the user typed: an assistant record
        // is the model's, and the two subtypes below are drawn as something other than a
        // message. Attachments are of their own record type and are excluded with them.
        if record.record_type != USER_RECORD_TYPE
            || matches!(
                record.subtype.as_deref(),
                Some(COMPACT_BOUNDARY_SUBTYPE) | Some(LOCAL_COMMAND_SUBTYPE)
            )
        {
            continue;
        }

        let base_key = match record.uuid.as_ref() {
            Some(uuid) => SharedString::from(uuid.clone()),
            None => SharedString::from(format!("path-{path_index}")),
        };
        match record
            .raw
            .get("message")
            .and_then(|message| message.get("content"))
        {
            Some(Value::String(text)) => {
                if let Some(kind) = message_kind(record, text) {
                    entries.push(Entry {
                        key: base_key,
                        kind,
                    });
                }
            }
            Some(Value::Array(blocks)) => {
                for (block_index, block) in blocks.iter().enumerate() {
                    // Only a text block can be a message, and skipping the rest is what
                    // keeps an image out of a walk performed for its neighbour's text.
                    if block.get("type").and_then(Value::as_str) != Some("text") {
                        continue;
                    }
                    let text = block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if let Some(kind) = message_kind(record, text) {
                        entries.push(Entry {
                            key: SharedString::from(format!("{base_key}#{block_index}")),
                            kind,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    entries
}

/// What the selected session is doing, as far as its transcript says.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Activity {
    Thinking,
    RunningTool {
        name: SharedString,
        target: SharedString,
    },
    Idle,
}

/// Reads off what a session is doing from the conversation it has written.
///
/// The registry's `status` field is not used for this. It is rewritten only when the
/// status changes, so a session that stopped an hour ago still carries whatever it last
/// wrote there — an idle session reading as `busy` for the rest of the day. The
/// transcript is the only thing that moves with the model, and reading it needs no clock,
/// which is what makes this answer testable.
/// Read backwards from the end of the conversation, because what the session is doing is
/// whatever the newest record that says anything says — and the first record that does
/// decides, rather than the whole path being summarised.
fn activity(path: &[&TranscriptRecord]) -> Activity {
    // The calls answered by the results written after the record being looked at. A call
    // is running exactly when nothing further down the conversation answers it.
    let mut answered: HashSet<&str> = HashSet::default();

    for &record in path.iter().rev() {
        match record.record_type.as_str() {
            USER_RECORD_TYPE => {
                let results = tool_result_ids(record);
                if results.is_empty() {
                    // Words the user typed. Whatever the model was doing before them
                    // belongs to a turn the user has already spoken past, and calling
                    // that running is the same lie the registry's `status` field tells.
                    return Activity::Idle;
                }
                answered.extend(results);
            }
            ASSISTANT_RECORD_TYPE => return assistant_activity(record, &answered),
            // A compact boundary, an attachment, a `mode` line: none of them say
            // anything about what the session is doing, so the search reads past them.
            _ => {}
        }
    }

    Activity::Idle
}

/// What an assistant record says the session is doing.
///
/// Only its last block is read. The blocks are written in order, so the last one is what
/// the model was doing when it stopped writing: a record that thought and then made a
/// call is doing the call, and a record that ends in prose is a turn that has been
/// answered.
fn assistant_activity(record: &TranscriptRecord, answered: &HashSet<&str>) -> Activity {
    let Some(block) = record
        .raw
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .and_then(|blocks| blocks.last())
    else {
        return Activity::Idle;
    };

    match block.get("type").and_then(Value::as_str) {
        Some(TOOL_USE_BLOCK_TYPE) => {
            let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
            if answered.contains(id) {
                // The call is over and the model has not written its next record yet.
                return Activity::Idle;
            }
            Activity::RunningTool {
                name: SharedString::from(
                    block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("Tool")
                        .to_string(),
                ),
                target: tool_target(block),
            }
        }
        Some(THINKING_BLOCK_TYPE) => Activity::Thinking,
        // Prose, an image, a block this has no rule for: the model has said something,
        // which is a turn ending rather than work in progress.
        _ => Activity::Idle,
    }
}

/// The `tool_use_id`s of the results a user record holds. Empty for a record that is
/// words the user typed, which is what tells the two apart.
fn tool_result_ids(record: &TranscriptRecord) -> Vec<&str> {
    let Some(blocks) = record
        .raw
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some(TOOL_RESULT_BLOCK_TYPE))
        .filter_map(|block| block.get("tool_use_id").and_then(Value::as_str))
        .collect()
}

/// The words drawn beside a running tool's name to say which call it is.
///
/// Empty for a tool with no rule of its own: the inputs are the tool's business, and
/// reaching into an unknown one for something that looks like a target would put a
/// misleading line above the input. Naming the tool alone is honest and still useful.
fn tool_target(block: &Value) -> SharedString {
    let argument = |key: &str| {
        block
            .get("input")
            .and_then(|input| input.get(key))
            .and_then(Value::as_str)
    };

    let target = match block
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        // The file is what says which read or write this is; the directory it sits in
        // is the same for most of them and does not fit on the line.
        "Read" | "Write" | "Edit" | "NotebookEdit" => argument("file_path")
            .and_then(|path| Path::new(path).file_name())
            .and_then(|name| name.to_str())
            .unwrap_or_default(),
        // Often a whole pipeline, of which the head is what names the call.
        "Bash" => argument("command").unwrap_or_default(),
        _ => "",
    };

    SharedString::from(truncate_and_trailoff(target.trim(), TOOL_TARGET_CHARACTERS))
}

/// Whether one of the selected session's agents is still working.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentState {
    Running,
    Finished,
}

/// A `tool_use` block that spawned conversations of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
struct AgentCall {
    tool_use_id: SharedString,
    is_workflow: bool,
}

/// One agent as the card under the call that spawned it draws it.
struct AgentCardRow {
    label: SharedString,
    state: AgentState,
    target: TranscriptTarget,
}

/// Whether one of the selected session's agents is still working.
///
/// Read off the session's own conversation, never the agent's: an agent's transcript
/// ends when the agent stops writing and records nothing about having returned. The one
/// place the end of an agent is written down is the result of the call that spawned it.
fn agent_state(summary: &SubagentSummary, main_path: &[&TranscriptRecord]) -> AgentState {
    if let Some(tool_use_id) = summary.meta.tool_use_id.as_deref() {
        let answered = main_path
            .iter()
            .any(|record| tool_result_ids(record).contains(&tool_use_id));
        return if answered {
            AgentState::Finished
        } else {
            AgentState::Running
        };
    }

    // A `Workflow` run's agents record no tool use id, so the end of the run is the only
    // thing that says they are over, and the run announces itself by id in the result of
    // the `Workflow` call.
    if let Some(workflow_run_id) = summary.workflow_run_id.as_deref() {
        return if workflow_run_is_announced_as_over(workflow_run_id, main_path) {
            AgentState::Finished
        } else {
            AgentState::Running
        };
    }

    // Neither id, so nothing in the conversation pairs with this agent at all. A chip
    // that pulses for the rest of the session is worse than one that never pulses.
    AgentState::Finished
}

fn workflow_run_is_announced_as_over(
    workflow_run_id: &str,
    main_path: &[&TranscriptRecord],
) -> bool {
    main_path.iter().any(|record| {
        tool_result_texts(record).into_iter().any(|(_, text)| {
            workflow_run_id_in_tool_result(&text).as_deref() == Some(workflow_run_id)
        })
    })
}

/// Every `tool_result` block a record holds, as the id of the call it answers and the
/// text of the answer.
fn tool_result_texts(record: &TranscriptRecord) -> Vec<(&str, String)> {
    let Some(blocks) = record
        .raw
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some(TOOL_RESULT_BLOCK_TYPE))
        .filter_map(|block| {
            let tool_use_id = block.get("tool_use_id").and_then(Value::as_str)?;
            Some((tool_use_id, block_content_text(block.get("content"))))
        })
        .collect()
}

/// What a chip says the agent is: what it was asked to do, or failing that which agent it
/// is. A description of nothing but whitespace is no label at all.
fn agent_chip_label(summary: &SubagentSummary) -> SharedString {
    if let Some(description) = summary
        .meta
        .description
        .as_deref()
        .map(str::trim)
        .filter(|description| !description.is_empty())
    {
        return SharedString::from(description.to_string());
    }

    // Taken by characters rather than bytes: agent ids are hexadecimal in practice, but
    // slicing an id that is not would panic partway through a character.
    let short_id: String = summary
        .agent_id
        .chars()
        .take(AGENT_ID_CHIP_CHARACTERS)
        .collect();
    SharedString::from(format!("{} {short_id}", summary.meta.agent_type))
}

/// What the chip's tooltip adds to its label: only the fields the agent actually
/// recorded, because a line reading `model: unknown` says less than no line.
fn agent_chip_tooltip(summary: &SubagentSummary) -> SharedString {
    let mut lines = vec![summary.meta.agent_type.clone()];
    if let Some(model) = summary.meta.model.as_deref() {
        lines.push(model.to_string());
    }
    lines.push(format!("depth {}", summary.meta.spawn_depth));
    if let Some(workflow_phase) = summary.meta.workflow_phase.as_deref() {
        lines.push(workflow_phase.to_string());
    }
    SharedString::from(lines.join("\n"))
}

/// The calls in a conversation that started conversations of their own, keyed by the
/// entry each call is drawn as, so that the card offering what it started can be hung
/// under exactly that row.
///
/// Keyed the way [`build_entries`] keys a block's entry; the two must agree or the card
/// lands under the wrong call.
fn agent_calls(path: &[&TranscriptRecord]) -> HashMap<SharedString, AgentCall> {
    let mut calls = HashMap::default();

    for (path_index, record) in path.iter().enumerate() {
        let Some(blocks) = record
            .raw
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        let base_key = match record.uuid.as_ref() {
            Some(uuid) => SharedString::from(uuid.clone()),
            None => SharedString::from(format!("path-{path_index}")),
        };

        for (block_index, block) in blocks.iter().enumerate() {
            if block.get("type").and_then(Value::as_str) != Some(TOOL_USE_BLOCK_TYPE) {
                continue;
            }
            let is_workflow = match block.get("name").and_then(Value::as_str) {
                Some(AGENT_TOOL_NAME) => false,
                Some(WORKFLOW_TOOL_NAME) => true,
                _ => continue,
            };
            let Some(tool_use_id) = block.get("id").and_then(Value::as_str) else {
                continue;
            };
            calls.insert(
                SharedString::from(format!("{base_key}#{block_index}")),
                AgentCall {
                    tool_use_id: SharedString::from(tool_use_id.to_string()),
                    is_workflow,
                },
            );
        }
    }

    calls
}

/// The agents one call spawned, as the scan of the session's directory found them.
///
/// An `Agent` call is paired by the `tool_use_id` its agent's sidecar records. A
/// `Workflow` call spawns a run of agents that record no tool use id at all, so they are
/// paired through the run id the tool's own result announces — and until that result
/// arrives nothing links the two, which is why an unannounced run pairs with nothing
/// rather than with every agent of every run.
fn subagents_of_call<'summaries>(
    call: &AgentCall,
    subagents: &'summaries [SubagentSummary],
    main_path: &[&TranscriptRecord],
) -> Vec<&'summaries SubagentSummary> {
    if !call.is_workflow {
        return subagents
            .iter()
            .filter(|summary| {
                summary.meta.tool_use_id.as_deref() == Some(call.tool_use_id.as_ref())
            })
            .collect();
    }

    let Some(workflow_run_id) = announced_workflow_run_id(&call.tool_use_id, main_path) else {
        return Vec::new();
    };
    subagents
        .iter()
        .filter(|summary| summary.workflow_run_id.as_deref() == Some(workflow_run_id.as_str()))
        .collect()
}

/// The run id the `Workflow` call's own result announces.
fn announced_workflow_run_id(tool_use_id: &str, main_path: &[&TranscriptRecord]) -> Option<String> {
    main_path.iter().find_map(|record| {
        tool_result_texts(record)
            .into_iter()
            .filter(|(id, _)| *id == tool_use_id)
            .find_map(|(_, text)| workflow_run_id_in_tool_result(&text))
    })
}

/// Why the input is disabled, or `None` when it is not.
///
/// An agent's conversation is named before the missing pane is, because it is the reason
/// the reader can act on: the way back is one chip away, whereas a session outside tmux
/// is nothing they can do anything about from here.
fn input_note(
    can_send: bool,
    is_reading_an_agent: bool,
    has_selection: bool,
) -> Option<&'static str> {
    if can_send {
        return None;
    }
    if is_reading_an_agent {
        return Some(AGENT_READ_ONLY_NOTE);
    }
    if has_selection {
        return Some(READ_ONLY_NOTE);
    }
    Some(SELECT_A_SESSION_TO_REPLY)
}

/// The line drawn above the input for an activity, or `None` for one that is not worth a
/// line.
fn activity_label(activity: &Activity) -> Option<SharedString> {
    Some(match activity {
        // Withheld so that the line does not appear and disappear at the end of every
        // turn, moving the input under the reader's hands.
        Activity::Idle => return None,
        Activity::Thinking => SharedString::from(THINKING_NOTE),
        Activity::RunningTool { name, target } if target.is_empty() => {
            SharedString::from(format!("Running {name}"))
        }
        Activity::RunningTool { name, target } => {
            SharedString::from(format!("Running {name}: {target}"))
        }
    })
}

/// The panel is a side panel, so the two sides a setting can name are the two positions
/// it has.
fn dock_position(dock: DockSide) -> DockPosition {
    match dock {
        DockSide::Left => DockPosition::Left,
        DockSide::Right => DockPosition::Right,
    }
}

fn markdown_style(window: &Window, cx: &App) -> MarkdownStyle {
    MarkdownStyle::themed(MarkdownFont::Agent, window, cx)
}

/// A tool's output is drawn inside the panel's own framed card, so the code block that
/// carries the text must not draw a second frame inside that one.
fn tool_output_markdown_style(window: &Window, cx: &App) -> MarkdownStyle {
    let mut style = markdown_style(window, cx);
    style.code_block.border_widths = Default::default();
    style.code_block.margin = Default::default();
    style.code_block.background = None;
    style
}

/// Flattens a conversation path into the items the list draws, deriving each record's
/// expensive artifacts only the first time the record is seen.
fn build_entries(
    path: &[&TranscriptRecord],
    home_directory: Option<&Path>,
    cache: &mut EntryCache,
) -> Vec<Entry> {
    let mut entries = Vec::with_capacity(path.len());
    let mut attachments = Vec::new();
    // `tool_result` blocks name the call they answer by id, so the ids seen on the way
    // down the path are what lets a result be labelled with its tool's name.
    let mut tool_names: HashMap<String, SharedString> = HashMap::default();

    for (path_index, record) in path.iter().enumerate() {
        let base_key = match record.uuid.as_ref() {
            Some(uuid) => SharedString::from(uuid.clone()),
            None => SharedString::from(format!("path-{path_index}")),
        };
        append_record(
            record,
            base_key,
            home_directory,
            &mut tool_names,
            &mut entries,
            &mut attachments,
            cache,
        );
    }

    if !attachments.is_empty() {
        // Attachments are the context Claude Code injected around the turns, not turns
        // themselves, so they are folded into one section instead of interleaved.
        entries.insert(
            0,
            Entry {
                key: SharedString::from(ATTACHMENTS_ENTRY_KEY),
                kind: EntryKind::Attachments { items: attachments },
            },
        );
    }

    entries
}

fn append_record(
    record: &TranscriptRecord,
    base_key: SharedString,
    home_directory: Option<&Path>,
    tool_names: &mut HashMap<String, SharedString>,
    entries: &mut Vec<Entry>,
    attachments: &mut Vec<AttachmentItem>,
    cache: &mut EntryCache,
) {
    if record.record_type == ATTACHMENT_RECORD_TYPE {
        let cache_key = cache_key(record, &base_key);
        attachments.push(cache.attachment(cache_key.as_ref(), || {
            attachment_item(record, base_key.clone(), home_directory)
        }));
        return;
    }

    match record.subtype.as_deref() {
        Some(COMPACT_BOUNDARY_SUBTYPE) => {
            let cache_key = cache_key(record, &base_key);
            let kind = cache.kind(cache_key.as_ref(), || {
                let metadata = record.compact_metadata.as_ref();
                EntryKind::CompactBoundary {
                    trigger: metadata
                        .map(|metadata| SharedString::from(metadata.trigger.clone()))
                        .unwrap_or_else(|| SharedString::from("unknown")),
                    pre_tokens: metadata.and_then(|metadata| metadata.pre_tokens),
                    post_tokens: metadata.and_then(|metadata| metadata.post_tokens),
                }
            });
            entries.push(Entry {
                key: base_key,
                kind,
            });
            return;
        }
        Some(LOCAL_COMMAND_SUBTYPE) => {
            let cache_key = cache_key(record, &base_key);
            let kind = cache.kind(cache_key.as_ref(), || {
                // These carry terminal output verbatim, escape sequences included.
                match record.raw.get("content").and_then(Value::as_str) {
                    Some(content) => EntryKind::LocalCommand {
                        text: SharedString::from(strip_ansi_escapes(content)),
                    },
                    // A `content` that is not text is a shape this has no rule for, and
                    // showing an empty command would drop the record.
                    None => {
                        unknown_kind(&record.record_type, record.subtype.as_deref(), &record.raw)
                    }
                }
            });
            entries.push(Entry {
                key: base_key,
                kind,
            });
            return;
        }
        _ => {}
    }

    match record
        .raw
        .get("message")
        .and_then(|message| message.get("content"))
    {
        Some(Value::String(text)) => {
            let cache_key = cache_key(record, &base_key);
            // A record with nothing but injected context in it is left out of the
            // conversation rather than drawn as an empty message.
            if let Some(kind) =
                cache.optional_kind(cache_key.as_ref(), || message_kind(record, text))
            {
                entries.push(Entry {
                    key: base_key,
                    kind,
                });
            }
        }
        Some(Value::Array(blocks)) => {
            for (block_index, block) in blocks.iter().enumerate() {
                let key = SharedString::from(format!("{base_key}#{block_index}"));
                // Registered outside the cache: a `tool_result` block being built for the
                // first time looks up the name of its call here, and the `tool_use` block
                // that named it may itself have come from the cache.
                register_tool_name(block, tool_names);
                let cache_key = cache_key(record, &key);
                // The key carries the block's index within the record, so a block that
                // is left out does not shift the keys of the ones after it.
                if let Some(kind) = cache.optional_kind(cache_key.as_ref(), || {
                    block_kind(record, block, tool_names, home_directory)
                }) {
                    entries.push(Entry { key, kind });
                }
            }
        }
        _ => {
            let cache_key = cache_key(record, &base_key);
            let kind = cache.kind(cache_key.as_ref(), || {
                unknown_kind(&record.record_type, record.subtype.as_deref(), &record.raw)
            });
            entries.push(Entry {
                key: base_key,
                kind,
            });
        }
    }
}

/// The key one of a record's entries is cached under, or `None` for a record that must
/// not be cached at all: a record without a uuid is keyed by its position in the
/// conversation, and positions shift as the conversation grows, so the same key would
/// name a different record after a rebuild.
fn cache_key(record: &TranscriptRecord, key: &SharedString) -> Option<SharedString> {
    record.uuid.is_some().then(|| key.clone())
}

/// Remembers which tool a `tool_use` block called, so that the `tool_result` answering it
/// can be labelled with the tool's name.
fn register_tool_name(block: &Value, tool_names: &mut HashMap<String, SharedString>) {
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return;
    }
    if let Some(id) = block.get("id").and_then(Value::as_str) {
        let name = SharedString::from(
            block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("Tool")
                .to_string(),
        );
        tool_names.insert(id.to_string(), name);
    }
}

/// Message text is never treated as a persisted-output placeholder: it is written by the
/// user or the model, so a marker in it is forged or coincidental, and honouring it would
/// hand the panel an arbitrary path to read. Claude Code writes the placeholder into the
/// `tool_result` block, which [`block_kind`] handles.
///
/// `None` for a user record whose text is nothing but injected context, which has no
/// message in it to show; see [`user_visible_text`].
fn message_kind(record: &TranscriptRecord, text: &str) -> Option<EntryKind> {
    // The summary compaction leaves behind is recorded as a user message, so the record
    // type alone would present a machine-written recap of the conversation as something
    // the user had typed.
    let role = if record.is_compact_summary {
        MessageRole::CompactSummary
    } else {
        MessageRole::from_record_type(&record.record_type)
    };

    // Drawn as the command it was rather than as prose the user wrote: see
    // [`rewrite_slash_commands`].
    if role == MessageRole::User {
        if let Some(command) = rewrite_slash_commands(text) {
            return Some(EntryKind::SlashCommand {
                text: SharedString::from(command),
            });
        }
    }

    // Only what claims to be the user's own words is filtered: everything else in the
    // transcript is shown as it was written.
    let source = if role == MessageRole::User {
        user_visible_text(text)?
    } else {
        text.to_string()
    };

    Some(EntryKind::Message {
        role,
        source: SharedString::from(source),
    })
}

/// `None` only for a user text block that is nothing but injected context; see
/// [`message_kind`].
fn block_kind(
    record: &TranscriptRecord,
    block: &Value,
    tool_names: &HashMap<String, SharedString>,
    home_directory: Option<&Path>,
) -> Option<EntryKind> {
    let kind = match block.get("type").and_then(Value::as_str) {
        Some("text") => {
            let text = block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return message_kind(record, text);
        }

        Some("thinking") => {
            let text = block
                .get("thinking")
                .and_then(Value::as_str)
                .or_else(|| block.get("text").and_then(Value::as_str))
                .unwrap_or_default();
            EntryKind::Thinking {
                source: SharedString::from(text.to_string()),
            }
        }

        Some("tool_use") => {
            let name = SharedString::from(
                block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("Tool")
                    .to_string(),
            );
            let input = block.get("input").map(json_text).unwrap_or_default();
            EntryKind::ToolUse {
                name,
                input: SharedString::from(input),
            }
        }

        Some("tool_result") => {
            let label = block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .and_then(|id| tool_names.get(id).cloned())
                .unwrap_or_else(|| SharedString::from("Tool result"));
            // The top-level `toolUseResult` is the structured result; the content block
            // beside it is a rendering of that result meant for the model's context. The
            // structured one is preferred while it yields text, then the rendering, and
            // only a result with neither falls back to raw JSON rather than being lost.
            let structured_result = record
                .raw
                .get("toolUseResult")
                .filter(|_| record_holds_one_tool_result(record));
            let text = structured_result
                .and_then(tool_use_result_text)
                .or_else(|| {
                    let rendered = block_content_text(block.get("content"));
                    (!rendered.is_empty()).then_some(rendered)
                })
                .or_else(|| structured_result.map(json_text))
                .unwrap_or_default();
            let is_error = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let body = match structured_persisted_output(structured_result, &text, home_directory)
                // The placeholder text is only a fallback: one persisted result in
                // eighty-six carries no `persistedOutputPath` beside it.
                .or_else(|| parse_persisted_output(&text))
            {
                Some(persisted) => ToolResultBody::Persisted(persisted),
                None => ToolResultBody::Inline(SharedString::from(text)),
            };
            EntryKind::ToolResult {
                label,
                is_error,
                body,
            }
        }

        Some("image") => image_kind(block),

        other => unknown_kind(
            other.unwrap_or("block"),
            None,
            &without_base64_payload(block),
        ),
    };

    Some(kind)
}

/// Whether the record's `toolUseResult` can be read as the answer to the `tool_result`
/// block being drawn.
///
/// The field sits on the record while the blocks sit inside it, so it names one answer
/// however many the record carries: a record answering two calls at once would show the
/// first call's output — and, with a `persistedOutputPath` beside it, the first call's
/// file — under the second call's name. A record with two of them has each block's own
/// `content` to fall back on, which is the answer to that block and nothing else.
fn record_holds_one_tool_result(record: &TranscriptRecord) -> bool {
    record
        .raw
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks
                .iter()
                .filter(|block| {
                    block.get("type").and_then(Value::as_str) == Some(TOOL_RESULT_BLOCK_TYPE)
                })
                .count()
                == 1
        })
}

fn image_kind(block: &Value) -> EntryKind {
    let source = block.get("source");
    let media_type = source
        .and_then(|source| source.get("media_type"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let data = source
        .and_then(|source| source.get("data"))
        .and_then(Value::as_str)
        .unwrap_or_default();

    match (ImageFormat::from_mime_type(media_type), decode_base64(data)) {
        (Some(format), Some(bytes)) if !bytes.is_empty() => EntryKind::Image {
            image: Arc::new(Image::from_bytes(format, bytes)),
            media_type: SharedString::from(media_type.to_string()),
        },
        _ => unknown_kind("image", None, &without_base64_payload(block)),
    }
}

fn unknown_kind(record_type: &str, subtype: Option<&str>, value: &Value) -> EntryKind {
    let label = match subtype {
        Some(subtype) => format!("{record_type} / {subtype}"),
        None => record_type.to_string(),
    };
    EntryKind::Unknown {
        label: SharedString::from(label),
        raw: SharedString::from(json_text(value)),
    }
}

fn attachment_item(
    record: &TranscriptRecord,
    key: SharedString,
    home_directory: Option<&Path>,
) -> AttachmentItem {
    let attachment = record.raw.get("attachment");
    let label = SharedString::from(
        attachment
            .and_then(|attachment| attachment.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("attachment")
            .to_string(),
    );

    let rendered: Vec<&str> = record
        .raw
        .get("rendered")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("content").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();

    let rendered_text = (!rendered.is_empty()).then(|| rendered.join("\n\n"));
    let attachment_content = attachment
        .and_then(|attachment| attachment.get("content"))
        .and_then(Value::as_str);

    // An attachment's own marker text is one of the two places a persisted-output path
    // may be read from, and the one hook output arrives by. It is read from the
    // attachment's text rather than from `body`, because most of these records carry no
    // `rendered` array and `body` is then pretty-printed JSON, in which the marker's
    // newlines are escaped and the path would come out as one run of escaped text.
    let persisted = rendered_text
        .as_deref()
        .and_then(parse_persisted_output)
        .or_else(|| attachment_content.and_then(parse_persisted_output))
        .filter(|persisted| persisted_output_is_loadable(&persisted.path, home_directory));

    let body = rendered_text.unwrap_or_else(|| json_text(attachment.unwrap_or(&record.raw)));

    AttachmentItem {
        key,
        label,
        body: SharedString::from(body),
        persisted,
    }
}

/// The text of a structured `toolUseResult`, or `None` for a shape this has no rule for.
/// `None` rather than the raw JSON, because a shape such as a `Read` result holds the
/// file in a JSON string whose newlines are escaped: one unreadable line. The rendering
/// beside it in the content block is the readable form of the same result.
fn tool_use_result_text(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }

    let object = value.as_object()?;
    let stdout = object.get("stdout").and_then(Value::as_str);
    let stderr = object
        .get("stderr")
        .and_then(Value::as_str)
        .filter(|stderr| !stderr.is_empty());

    if stdout.is_some() || stderr.is_some() {
        let mut text = stdout.unwrap_or_default().to_string();
        if let Some(stderr) = stderr {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(stderr);
        }
        return Some(text);
    }

    if let Some(content) = object.get("content").and_then(Value::as_str) {
        return Some(content.to_string());
    }

    None
}

fn block_content_text(content: Option<&Value>) -> String {
    match content {
        None => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item.get("text").and_then(Value::as_str) {
                Some(text) => text.to_string(),
                None => json_text(item),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => json_text(other),
    }
}

/// The raw JSON of a block, with any base64 payload replaced by a description of it.
/// Without this an image block that failed to decode would dump megabytes of base64 into
/// the fallback view, while the substitution still records that the data was there.
fn without_base64_payload(block: &Value) -> Value {
    let mut block = block.clone();
    if let Some(data) = block
        .get_mut("source")
        .and_then(|source| source.get_mut("data"))
    {
        if let Some(encoded) = data.as_str() {
            let length = encoded.len();
            *data = Value::String(format!("<{length} base64 characters>"));
        }
    }
    block
}

/// The one line the session list is reduced to when it is collapsed: how many sessions
/// are running, and which of them is being read.
fn collapsed_sessions_summary(session_count: usize, selected_name: Option<&str>) -> SharedString {
    let sessions = if session_count == 1 {
        "1 session".to_string()
    } else {
        format!("{session_count} sessions")
    };

    SharedString::from(match selected_name {
        Some(name) => format!("{sessions} · {name}"),
        None => sessions,
    })
}

/// What is left of a user record's text once the blocks Claude Code injected into it are
/// removed, or `None` when nothing but those blocks was there.
///
/// This is a display rule and nothing more: the record keeps its place in the transcript's
/// tree and its offset in the file, and only the text drawn for it is trimmed.
fn user_visible_text(text: &str) -> Option<String> {
    // A slash command is rewritten rather than filtered: the blocks it is recorded as
    // hold the command the user ran and the words they typed after it, so cutting them
    // out the way the injected wrappers are cut would drop the message itself.
    if let Some(command) = rewrite_slash_commands(text) {
        return Some(command);
    }

    let mut visible = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(block) = next_injected_block(rest) {
        visible.push_str(rest.get(..block.start)?);
        rest = match block.end {
            Some(end) => rest.get(end..)?,
            // With no closing tag there is no telling where the block was meant to end,
            // and assuming it runs to the end of the text would throw away everything
            // after it — including the rest of a message that is still being written.
            None => {
                visible.push_str(rest.get(block.start..block.open_end)?);
                rest.get(block.open_end..)?
            }
        };
    }
    visible.push_str(rest);

    let visible = visible.trim();
    (!visible.is_empty()).then(|| visible.to_string())
}

/// The line the user typed, rebuilt from the blocks a slash command is recorded as, or
/// `None` for a record that is not one.
///
/// Claude Code does not write `/goal ship the panel` as text. It writes the command into
/// a [`COMMAND_NAME_WRAPPER`] block and the words after it into a
/// [`COMMAND_ARGS_WRAPPER`] block, with a `command-message` block of its own in between.
/// The arguments are the only part of that the user typed, and they are often the whole
/// point of the message, so the record is rebuilt into the line rather than being hidden
/// or drawn as a wall of tags.
fn rewrite_slash_commands(text: &str) -> Option<String> {
    let name = tag_contents(text, COMMAND_NAME_WRAPPER)?.trim();
    if name.is_empty() {
        return None;
    }

    let arguments = match find_opening_tag(text, COMMAND_ARGS_WRAPPER, 0) {
        // The arguments were opened and the closing tag has not been written yet, which
        // is what a record read while it is being written looks like. Where the
        // arguments were meant to end is not knowable, and guessing at it would eat the
        // rest of the record, so nothing is rewritten at all.
        Some(_) => tag_contents(text, COMMAND_ARGS_WRAPPER)?.trim(),
        // A command that takes no arguments is sometimes recorded without the block, and
        // the command is then all there is to show.
        None => "",
    };

    Some(match arguments.is_empty() {
        true => name.to_string(),
        false => format!("{name} {arguments}"),
    })
}

/// What sits between `name`'s opening and closing tags, or `None` when the text holds no
/// closing tag for it.
///
/// The contents are returned verbatim: they are words the user typed, so a `<` in them
/// opens nothing and is not looked at.
fn tag_contents<'text>(text: &'text str, name: &str) -> Option<&'text str> {
    let (_, open_end) = find_opening_tag(text, name, 0)?;
    let end = find_closing_tag(text, name, open_end)?;
    // `find_closing_tag` reports the byte after the `>` of `</name>`.
    text.get(open_end..end.checked_sub(name.len() + "</>".len())?)
}

/// Where one injected block sits in the text it was found in.
#[derive(Clone, Copy)]
struct InjectedBlock {
    /// The byte the opening tag starts at.
    start: usize,
    /// The byte after the opening tag's `>`.
    open_end: usize,
    /// The byte after the matching closing tag, or `None` when the text holds no closing
    /// tag for this block.
    end: Option<usize>,
}

/// The first injected block in `text`, whichever wrapper it belongs to.
fn next_injected_block(text: &str) -> Option<InjectedBlock> {
    let mut first: Option<(&str, usize, usize)> = None;
    for name in INJECTED_WRAPPERS {
        let Some((start, open_end)) = find_opening_tag(text, name, 0) else {
            continue;
        };
        if first.is_none_or(|(_, first_start, _)| start < first_start) {
            first = Some((name, start, open_end));
        }
    }

    let (name, start, open_end) = first?;
    Some(InjectedBlock {
        start,
        open_end,
        end: find_closing_tag(text, name, open_end),
    })
}

/// The opening tag of `name` at or after `from`, as the byte it starts at and the byte
/// after its `>`.
///
/// The tag may carry attributes — `<codegraph_context note="…">` — so the name is only
/// recognized where the character after it cannot continue a longer name, and the tag
/// then runs to the first `>`. An opening tag whose `>` was never written ends at the end
/// of the text, which is what keeps a truncated record from being scanned past its end.
fn find_opening_tag(text: &str, name: &str, from: usize) -> Option<(usize, usize)> {
    let opening = format!("<{name}");
    let mut searched = from;

    loop {
        let start = searched + text.get(searched..)?.find(&opening)?;
        let after_name = start + opening.len();
        match text.get(after_name..).and_then(|rest| rest.chars().next()) {
            None => return Some((start, text.len())),
            Some('>') => return Some((start, after_name + 1)),
            Some(character) if character.is_whitespace() || character == '/' => {
                let end = match text.get(after_name..).and_then(|rest| rest.find('>')) {
                    Some(offset) => after_name + offset + 1,
                    None => text.len(),
                };
                return Some((start, end));
            }
            // A longer name that merely starts with this one, such as
            // `<system-reminders>`; the search carries on past it.
            Some(_) => searched = after_name,
        }
    }
}

/// The byte after the closing tag that matches an opening tag ending at `open_end`, or
/// `None` when there is none.
///
/// A wrapper can hold another of its own kind, so the closing tags are counted against
/// the opening ones rather than the first closing tag being taken as the match.
fn find_closing_tag(text: &str, name: &str, open_end: usize) -> Option<usize> {
    let closing = format!("</{name}>");
    let mut depth = 1usize;
    let mut searched = open_end;

    while depth > 0 {
        let next_closing = searched + text.get(searched..)?.find(&closing)?;
        let next_opening = find_opening_tag(text, name, searched).map(|(start, _)| start);

        match next_opening {
            Some(opening_start) if opening_start < next_closing => {
                depth += 1;
                // Past the opening tag rather than past its `>`, which is enough to make
                // progress and cannot land inside the tag's name.
                searched = opening_start + 1 + name.len();
            }
            _ => {
                depth -= 1;
                searched = next_closing + closing.len();
            }
        }
    }

    Some(searched)
}

fn json_text(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn first_line(text: &str) -> SharedString {
    let line = text.lines().find(|line| !line.trim().is_empty());
    SharedString::from(line.unwrap_or_default().trim().to_string())
}

/// Wraps text in a fenced code block long enough that the text cannot close it early.
fn fenced_code(text: &str, language: &str) -> SharedString {
    let mut longest_backtick_run = 0;
    let mut current_run = 0;
    for character in text.chars() {
        if character == '`' {
            current_run += 1;
            longest_backtick_run = longest_backtick_run.max(current_run);
        } else {
            current_run = 0;
        }
    }

    let fence = "`".repeat(longest_backtick_run.max(2) + 1);
    SharedString::from(format!(
        "{fence}{language}\n{}\n{fence}",
        text.trim_end_matches('\n')
    ))
}

/// How a tool call's input is drawn: the text to show as code, and the language it is
/// highlighted in. An empty language leaves the text as plain code.
struct ToolInputDisplay {
    text: SharedString,
    language: &'static str,
}

impl ToolInputDisplay {
    fn code_block(&self) -> SharedString {
        fenced_code(&self.text, self.language)
    }
}

/// The language a file is written in, taken from its extension. An extension this panel
/// has no name for is left empty, which draws the text as plain code rather than
/// highlighting it as the wrong language.
fn language_for_path(path: &str) -> &'static str {
    let extension = Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    match extension {
        "rs" => "rust",
        "py" => "python",
        "ts" => "typescript",
        "tsx" => "tsx",
        "js" => "javascript",
        "jsx" => "jsx",
        "json" => "json",
        "md" => "markdown",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "sh" | "bash" => "bash",
        "go" => "go",
        "html" => "html",
        "css" => "css",
        _ => "",
    }
}

/// The field of a tool's input holding the text of the file the call is about, for the
/// tools that carry one; see [`FILE_TOOL_CONTENT_FIELDS`].
fn file_tool_content_field(tool_name: &str) -> Option<&'static str> {
    FILE_TOOL_CONTENT_FIELDS
        .iter()
        .find(|(name, _)| *name == tool_name)
        .map(|(_, field)| *field)
}

/// The input of a tool call as it is drawn. The entry carries the input as the JSON the
/// transcript holds, which is read back here rather than at build time so that how a
/// call is drawn stays out of the cached entries.
///
/// The language is only ever the file's when the text being drawn is the file's: a call
/// drawn as its own JSON has to be labelled as JSON, or a `Read` of a Rust file is shown
/// as a paragraph of Rust that is really JSON.
fn tool_input_display(tool_name: &str, input: &str) -> ToolInputDisplay {
    let parsed = serde_json::from_str::<Value>(input).ok();
    let field = |name: &str| {
        parsed
            .as_ref()
            .and_then(|parsed| parsed.get(name))
            .and_then(Value::as_str)
    };

    if tool_name == BASH_TOOL_NAME {
        return ToolInputDisplay {
            text: SharedString::from(field("command").unwrap_or(input).to_string()),
            language: "bash",
        };
    }

    // A call whose usual field is not there — an `Edit` that only sets `old_string`, a
    // call read while it was still being written — has nothing but its input left to show.
    if let Some(text) = file_tool_content_field(tool_name).and_then(field) {
        return ToolInputDisplay {
            text: SharedString::from(text.to_string()),
            language: field("file_path").map_or("", language_for_path),
        };
    }

    ToolInputDisplay {
        text: SharedString::from(input.to_string()),
        language: "json",
    }
}

/// Whether an output has more lines than are drawn before the rest is put behind a
/// disclosure. Counted lazily because an output can be megabytes and this runs on every
/// frame the entry is on screen.
fn output_is_clamped(text: &str, line_limit: usize) -> bool {
    text.lines().nth(line_limit).is_some()
}

/// The first `line_limit` lines of an output, taken as a prefix of the text rather than
/// as lines rejoined, so that what is drawn is exactly what the output starts with.
fn clamped_output(text: &str, line_limit: usize) -> &str {
    let end: usize = text
        .split_inclusive('\n')
        .take(line_limit)
        .map(str::len)
        .sum();
    text.get(..end).unwrap_or(text)
}

/// The expansion key of the output inside an entry. It is a key of its own so that
/// unclamping an output does not close the entry the output sits in.
fn output_expansion_key(key: &SharedString) -> SharedString {
    SharedString::from(format!("{key}#output"))
}

/// Whether the panel will read this path back.
///
/// The path comes out of a file format that is private to Claude Code, so it is treated
/// as untrusted input: only an absolute path free of `..`, under the directory Claude
/// Code keeps its transcripts and their persisted outputs in, is ever opened. A path
/// outside it is still shown as text; it is only the offer to read the file that is
/// withheld, because a button that reads an arbitrary file is the risk here, and one
/// that fails is no use either.
fn persisted_output_is_loadable(path: &Path, home_directory: Option<&Path>) -> bool {
    // `None` means no scan has said yet which machine's home the path was written under.
    // There is nothing to check against then, so the offer is withheld: substituting this
    // machine's home would be an answer about the wrong machine on a remote project.
    let Some(home_directory) = home_directory else {
        return false;
    };

    path.is_absolute()
        && !path
            .components()
            .any(|component| component == Component::ParentDir)
        && path.starts_with(home_directory.join(".claude").join("projects"))
}

/// The persisted-output fields Claude Code writes beside a large tool result:
/// `persistedOutputPath` and `persistedOutputSize`. This is where the path comes from
/// whenever it is present, and the placeholder text is the fallback for the results that
/// carry no such field.
///
/// `inline_text` — the structured `stdout`, tens of kilobytes of it — stays the body that
/// is shown without asking, because the placeholder's preview holds only two. Reading the
/// whole file is a separate offer on top of it, not an alternative to it.
fn structured_persisted_output(
    result: Option<&Value>,
    inline_text: &str,
    home_directory: Option<&Path>,
) -> Option<PersistedOutput> {
    let result = result?.as_object()?;
    let path = PathBuf::from(result.get("persistedOutputPath").and_then(Value::as_str)?);
    // A path the panel will not read back is no reason to replace the text with a
    // "saved to a file" heading: the structured `stdout` is already the whole body.
    if !persisted_output_is_loadable(&path, home_directory) {
        return None;
    }

    let size = result
        .get("persistedOutputSize")
        .and_then(Value::as_u64)
        .map(describe_byte_count)
        .unwrap_or_else(|| "unknown size".to_string());

    Some(PersistedOutput {
        size: SharedString::from(size),
        path,
        preview: SharedString::from(inline_text.to_string()),
    })
}

/// Formats a byte count the way the placeholder text does, so that a size read from
/// `persistedOutputSize` and one read from the placeholder are shown alike.
fn describe_byte_count(bytes: u64) -> String {
    const KILOBYTE: f64 = 1024.;
    const MEGABYTE: f64 = KILOBYTE * KILOBYTE;

    let bytes_as_float = bytes as f64;
    if bytes_as_float >= MEGABYTE {
        format!("{:.1}MB", bytes_as_float / MEGABYTE)
    } else if bytes_as_float >= KILOBYTE {
        format!("{:.1}KB", bytes_as_float / KILOBYTE)
    } else {
        format!("{bytes}B")
    }
}

/// Recognizes the placeholder Claude Code writes in place of a tool output that was too
/// large to inline, which names the file the real output went to.
fn parse_persisted_output(text: &str) -> Option<PersistedOutput> {
    let body = text.split_once(PERSISTED_OUTPUT_MARKER)?.1;

    let path_start = body.find(PERSISTED_OUTPUT_PATH_PREFIX)? + PERSISTED_OUTPUT_PATH_PREFIX.len();
    let path_text = body.get(path_start..)?.lines().next()?.trim();
    let path = PathBuf::from(path_text);
    // A relative path has no meaningful base here, and a placeholder that does not name
    // a real file is better shown as ordinary text than as a broken load button.
    if path_text.is_empty() || !path.is_absolute() {
        return None;
    }

    let size = body
        .split_once(PERSISTED_OUTPUT_SIZE_PREFIX)
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(size, _)| size.trim().to_string())
        .unwrap_or_else(|| "unknown size".to_string());

    let preview = body
        .find(PERSISTED_OUTPUT_PREVIEW_PREFIX)
        .and_then(|index| body.get(index..))
        .and_then(|section| section.split_once(":\n"))
        .map(|(_, preview)| preview.trim_end().to_string())
        .unwrap_or_else(|| body.trim().to_string());

    Some(PersistedOutput {
        size: SharedString::from(size),
        path,
        preview: SharedString::from(preview),
    })
}

fn persisted_output_text(contents: FileContents) -> String {
    let mut text = String::from_utf8_lossy(&contents.bytes).into_owned();
    if contents.truncated {
        text.push_str("\n\n… truncated by the panel; open the file above for the rest.");
    }
    text
}

/// Removes terminal escape sequences so that captured terminal output reads as text.
fn strip_ansi_escapes(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut characters = input.chars();

    while let Some(character) = characters.next() {
        if character != '\u{1b}' {
            // The remaining control characters carry no meaning once the escapes are
            // gone, but newlines and tabs are part of the text.
            if character.is_control() && character != '\n' && character != '\t' {
                continue;
            }
            output.push(character);
            continue;
        }

        match characters.next() {
            // CSI: parameter and intermediate bytes, then a final byte in 0x40..=0x7e.
            Some('[') => {
                for following in characters.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&following) {
                        break;
                    }
                    // No escape sequence spans a line. Output captured mid-sequence has
                    // lost its final byte, and scanning on for one would eat the lines
                    // that follow, so the newline ends the sequence and is kept.
                    if following == '\n' {
                        output.push('\n');
                        break;
                    }
                }
            }
            // OSC, DCS, SOS, PM and APC run until BEL or a string terminator.
            Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => {
                let mut previous_was_escape = false;
                for following in characters.by_ref() {
                    if following == '\u{7}' || (previous_was_escape && following == '\\') {
                        break;
                    }
                    // These sequences have no final byte to fall back on, so an
                    // unterminated one would swallow the whole rest of the output.
                    if following == '\n' {
                        output.push('\n');
                        break;
                    }
                    previous_was_escape = following == '\u{1b}';
                }
            }
            // Any other escape is two characters long, and the second is consumed above.
            _ => {}
        }
    }

    output
}

/// Decodes standard or URL-safe base64, ignoring whitespace.
///
/// `base64` is not a dependency of this crate, and the only base64 the panel meets is the
/// payload of an image block, so the alphabet is decoded here rather than pulling in a
/// dependency for one call site.
fn decode_base64(input: &str) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len() / 4 * 3);
    let mut accumulator: u32 = 0;
    let mut bits: u32 = 0;

    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return None,
        };

        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push(((accumulator >> bits) & 0xff) as u8);
        }
    }

    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use crate::SubagentMeta;
    use crate::session_registry::{SessionSummary, TailProgress, TailState};
    use crate::session_source::SessionListing;
    use crate::transcript::parse_record;

    fn record(json: &str) -> TranscriptRecord {
        match parse_record(json) {
            Ok(Some(record)) => record,
            Ok(None) => panic!("expected a record, got a blank line for: {json}"),
            Err(error) => panic!("expected {json} to parse, got error: {error:#}"),
        }
    }

    fn entries_of_with_home(json_lines: &[&str], home_directory: Option<&Path>) -> Vec<Entry> {
        let records: Vec<TranscriptRecord> = json_lines.iter().map(|line| record(line)).collect();
        let path: Vec<&TranscriptRecord> = records.iter().collect();
        build_entries(&path, home_directory, &mut EntryCache::default())
    }

    /// The fixtures of every test that is not about the boundary itself write their paths
    /// under this machine's home, which is what a local project's scan reports.
    fn entries_of(json_lines: &[&str]) -> Vec<Entry> {
        entries_of_with_home(json_lines, Some(paths::home_dir()))
    }

    /// One line of JSON for a user record whose whole message is `text`.
    fn user_message_line(uuid: &str, text: &str) -> String {
        serde_json::json!({
            "type": "user",
            "uuid": uuid,
            "message": { "content": text },
        })
        .to_string()
    }

    fn message_sources(entries: &[Entry]) -> Vec<SharedString> {
        entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Message { source, .. } => Some(source.clone()),
                _ => None,
            })
            .collect()
    }

    /// The dock side the user dragged the panel to has to be there after a restart, and
    /// nothing the panel holds itself outlives one. The default stays the right-hand
    /// dock: the left one already holds the project panel.
    #[gpui::test]
    async fn the_dock_side_is_read_from_the_settings(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);

            assert_eq!(
                ClaudeSessionsSettings::get_global(cx).dock,
                DockSide::Right,
                "the right-hand dock is the only one the project panel is not already in"
            );

            <settings::SettingsStore as gpui::UpdateGlobal>::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |content| {
                    content.claude_sessions.get_or_insert_default().dock = Some(DockSide::Left);
                });
            });

            assert_eq!(
                ClaudeSessionsSettings::get_global(cx).dock,
                DockSide::Left,
                "the side in the settings is the side the panel opens on"
            );
        });
    }

    #[test]
    fn only_the_two_side_docks_are_offered() {
        let positions: Vec<DockPosition> = vec![
            dock_position(DockSide::Left),
            dock_position(DockSide::Right),
        ];
        assert_eq!(
            positions,
            vec![DockPosition::Left, DockPosition::Right],
            "the panel is a side panel; the bottom dock is not one of its positions"
        );
    }

    /// What the count has to be right about for the button to be worth having: it names
    /// how many messages the reader has not seen, not how many the conversation holds.
    #[test]
    fn entries_that_land_below_the_window_are_counted_until_the_reader_gets_there() {
        let mut unread = UnreadBelow::default();

        unread.entries_arrived(4, true);
        assert_eq!(
            unread.count(),
            0,
            "at the bottom the list follows the tail, so these were scrolled into view"
        );

        unread.entries_arrived(2, false);
        unread.entries_arrived(1, false);
        assert_eq!(
            unread.count(),
            3,
            "the count is what has arrived since the reader was last at the bottom"
        );

        unread.scrolled(false);
        assert_eq!(
            unread.count(),
            3,
            "scrolling that does not reach the bottom has read none of them"
        );

        unread.scrolled(true);
        assert_eq!(
            unread.count(),
            0,
            "reaching the bottom is reading them, and the button has nothing left to offer"
        );

        // What arrives after that is counted from zero again, not from the total.
        unread.entries_arrived(1, false);
        assert_eq!(unread.count(), 1);
    }

    /// A session the reader has just opened shows them the newest of that conversation,
    /// so nothing about where they were in the previous one carries over.
    #[test]
    fn a_new_session_starts_with_nothing_unread() {
        let mut unread = UnreadBelow::default();
        unread.entries_arrived(9, false);
        unread.reset();
        assert_eq!(
            unread.count(),
            0,
            "the count belongs to the conversation that was being read, not to the panel"
        );
    }

    /// Collapsed, the list gives up its rows, so the one line that is left has to say
    /// what the rows were saying: how many sessions are running, and which one the
    /// conversation below belongs to.
    #[test]
    fn a_collapsed_session_list_keeps_the_count_and_the_name_of_the_open_session() {
        let summaries: Vec<SharedString> = vec![
            collapsed_sessions_summary(0, None),
            collapsed_sessions_summary(1, None),
            collapsed_sessions_summary(1, Some("alpha")),
            collapsed_sessions_summary(7, Some("zed panel")),
        ];
        assert_eq!(
            summaries,
            vec![
                SharedString::from("0 sessions"),
                SharedString::from("1 session"),
                SharedString::from("1 session · alpha"),
                SharedString::from("7 sessions · zed panel"),
            ],
            "the collapsed header is the only place these are visible"
        );
    }

    #[test]
    fn every_injected_wrapper_is_cut_out_of_a_user_message() {
        let wrappers = [
            "<system-reminder>never mention this reminder</system-reminder>",
            "<task-notification>a background task finished</task-notification>",
            "<persisted-output>the rest went to a file</persisted-output>",
            // The opening tag of this one carries attributes.
            "<codegraph_context note=\"Structural context from CodeGraph\">symbols</codegraph_context>",
            "<local-command-stdout>total 0</local-command-stdout>",
        ];

        let visible: Vec<Option<String>> = wrappers
            .iter()
            .map(|wrapper| user_visible_text(&format!("what I actually typed\n\n{wrapper}")))
            .collect();
        assert_eq!(
            visible,
            vec![Some("what I actually typed".to_string()); wrappers.len()],
            "each of {wrappers:#?} is context Claude Code injected, not words the user typed"
        );
    }

    #[test]
    fn a_user_record_of_nothing_but_injected_context_is_not_shown_at_all() {
        let only_a_wrapper =
            "<task-notification>\nthe agent you spawned has finished\n</task-notification>";
        assert_eq!(
            user_visible_text(only_a_wrapper),
            None,
            "there is no message in this record to show"
        );

        let entries = entries_of(&[
            &user_message_line("a", only_a_wrapper),
            &user_message_line("b", "a message of my own"),
        ]);
        assert_eq!(
            message_sources(&entries),
            vec![SharedString::from("a message of my own")],
            "the injected record must be left out of the conversation, not drawn as an \
             empty bubble"
        );
    }

    #[test]
    fn two_injected_blocks_in_one_message_both_go() {
        let text = "<system-reminder>first</system-reminder>the middle is mine\
                    <task-notification>second</task-notification>";
        assert_eq!(
            user_visible_text(text).as_deref(),
            Some("the middle is mine"),
            "both wrappers have to go, and what sat between them has to stay"
        );

        // Nested, and nested in one of its own kind: the block ends at the closing tag
        // that matches its opening tag, not at the first one in the text.
        let nested = "before<system-reminder>outer<system-reminder>inner</system-reminder>\
                      still the reminder</system-reminder>after";
        assert_eq!(
            user_visible_text(nested).as_deref(),
            Some("beforeafter"),
            "a wrapper nested inside one of its own kind must not end the outer one early"
        );
    }

    /// A transcript read while it is being written ends in a line that is only partly
    /// there, so an opening tag whose closing tag has not been written yet is normal.
    #[test]
    fn an_injected_block_with_no_closing_tag_takes_nothing_with_it() {
        let truncated = "what I typed\n<system-reminder>the record ends mid-";
        assert_eq!(
            user_visible_text(truncated).as_deref(),
            Some(truncated),
            "with no closing tag there is no block to cut out, and guessing where it \
             would have ended would eat the rest of the record"
        );

        // What the guard must not kill: a wrapper that is closed is still cut out, even
        // when an unterminated one sits in front of it.
        let after_the_truncated_one =
            "<system-reminder>unterminated<task-notification>closed</task-notification>";
        assert_eq!(
            user_visible_text(after_the_truncated_one).as_deref(),
            Some("<system-reminder>unterminated"),
            "the closed wrapper has to go even though the one before it never closed"
        );
    }

    #[test]
    fn only_a_user_record_is_filtered() {
        let reminder = "<system-reminder>this stays</system-reminder> and more";
        let assistant = serde_json::json!({
            "type": "assistant",
            "uuid": "a",
            "message": { "content": [ { "type": "text", "text": reminder } ] },
        })
        .to_string();
        let entries = entries_of(&[&assistant]);
        assert_eq!(
            message_sources(&entries),
            vec![SharedString::from(reminder)],
            "the model's own words are shown as it wrote them"
        );

        // A tool result is not a message either, and it is where the persisted-output
        // marker is actually meant to be read.
        let tool_result = serde_json::json!({
            "type": "user",
            "uuid": "c",
            "message": { "content": [ {
                "type": "tool_result",
                "tool_use_id": "t1",
                "content": "<system-reminder>tool output</system-reminder>",
            } ] },
        })
        .to_string();
        let entries = entries_of(&[TOOL_USE_LINE, &tool_result]);
        assert_eq!(
            entries.len(),
            2,
            "a tool result must still be shown, got {} entries",
            entries.len()
        );
        assert_eq!(
            match &entries[1].kind {
                EntryKind::ToolResult { body, .. } => body.clone(),
                _ => panic!("expected the second entry to be a tool result"),
            },
            ToolResultBody::Inline(SharedString::from(
                "<system-reminder>tool output</system-reminder>"
            )),
            "a tool result is not the user's words and must be shown verbatim"
        );
    }

    #[test]
    fn a_reminder_appended_to_a_user_message_leaves_the_message() {
        // The shape a real message has: the user's words, then the reminder Claude Code
        // appended to them.
        let entries = entries_of(&[&user_message_line(
            "a",
            "please read the file\n<system-reminder>The user opened a file.</system-reminder>",
        )]);
        assert_eq!(
            message_sources(&entries),
            vec![SharedString::from("please read the file")],
            "the message is the user's; the reminder appended to it is not"
        );
    }

    #[test]
    fn string_and_array_message_content_both_render() {
        let entries = entries_of(&[
            r#"{"type":"user","uuid":"a","message":{"content":"plain string"}}"#,
            r#"{"type":"assistant","uuid":"b","message":{"content":[{"type":"text","text":"in an array"}]}}"#,
        ]);

        assert_eq!(entries.len(), 2);
        assert!(matches!(
            &entries[0].kind,
            EntryKind::Message { source, .. } if source.as_ref() == "plain string"
        ));
        assert!(matches!(
            &entries[1].kind,
            EntryKind::Message { source, .. } if source.as_ref() == "in an array"
        ));
    }

    #[test]
    fn tool_result_prefers_the_structured_tool_use_result() {
        let entries = entries_of(&[
            r#"{"type":"assistant","uuid":"a","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"user","uuid":"b","toolUseResult":{"stdout":"from stdout","stderr":""},
                "message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"from the content block"}]}}"#,
        ]);

        let EntryKind::ToolResult { label, body, .. } = &entries[1].kind else {
            panic!("expected a tool result, got something else");
        };
        assert_eq!(label.as_ref(), "Bash");
        assert_eq!(
            body,
            &ToolResultBody::Inline(SharedString::from("from stdout"))
        );
    }

    #[test]
    fn attachments_collapse_into_one_section_at_the_top() {
        let entries = entries_of(&[
            r#"{"type":"user","uuid":"a","message":{"content":"hello"}}"#,
            r#"{"type":"attachment","uuid":"b","attachment":{"type":"environment"},"rendered":[{"content":"env text"}]}"#,
            r#"{"type":"attachment","uuid":"c","attachment":{"type":"diagnostics"},"rendered":[{"content":"diag text"}]}"#,
        ]);

        assert_eq!(entries.len(), 2);
        let EntryKind::Attachments { items } = &entries[0].kind else {
            panic!("expected the attachments section first");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label.as_ref(), "environment");
        assert_eq!(items[1].body.as_ref(), "diag text");
    }

    #[test]
    fn unknown_record_and_block_types_survive_as_raw_json() {
        let entries = entries_of(&[
            r#"{"type":"queue-operation","uuid":"a","operation":"drain"}"#,
            r#"{"type":"assistant","uuid":"b","message":{"content":[{"type":"future_block","payload":7}]}}"#,
        ]);

        assert_eq!(entries.len(), 2);
        let EntryKind::Unknown { label, raw } = &entries[0].kind else {
            panic!("expected an unknown record entry");
        };
        assert_eq!(label.as_ref(), "queue-operation");
        assert!(raw.contains("drain"));

        let EntryKind::Unknown { label, raw } = &entries[1].kind else {
            panic!("expected an unknown block entry");
        };
        assert_eq!(label.as_ref(), "future_block");
        assert!(raw.contains("payload"));
    }

    #[test]
    fn compact_boundary_carries_its_metadata() {
        let entries = entries_of(&[
            r#"{"type":"system","subtype":"compact_boundary","uuid":"a","content":"Conversation compacted",
                "compactMetadata":{"trigger":"manual","preTokens":792090,"postTokens":16995}}"#,
        ]);

        assert!(matches!(
            &entries[0].kind,
            EntryKind::CompactBoundary { trigger, pre_tokens, post_tokens }
                if trigger.as_ref() == "manual"
                    && *pre_tokens == Some(792090)
                    && *post_tokens == Some(16995)
        ));
    }

    #[test]
    fn local_command_content_is_stripped_of_ansi_escapes() {
        // The escapes are written as JSON `\u001b`, because a raw control character
        // cannot appear inside a JSON string.
        let entries = entries_of(&[
            r#"{"type":"system","subtype":"local_command","uuid":"a","content":"\u001b[1;31mred\u001b[0m plain"}"#,
        ]);

        assert!(matches!(
            &entries[0].kind,
            EntryKind::LocalCommand { text } if text.as_ref() == "red plain"
        ));
    }

    #[test]
    fn persisted_output_placeholder_is_parsed() {
        let text = concat!(
            "<persisted-output>\n",
            "Output too large (80.6KB). Full output saved to: /tmp/tool-results/abc.txt\n",
            "\n",
            "Preview (first 2KB):\n",
            "first line\nsecond line",
        );

        let persisted = parse_persisted_output(text).expect("the placeholder should be recognized");
        assert_eq!(persisted.size.as_ref(), "80.6KB");
        assert_eq!(persisted.path, PathBuf::from("/tmp/tool-results/abc.txt"));
        assert_eq!(persisted.preview.as_ref(), "first line\nsecond line");
    }

    #[test]
    fn text_without_the_placeholder_is_not_treated_as_persisted() {
        assert!(parse_persisted_output("just some output").is_none());
        // The marker alone is not enough: without an absolute path there is nothing to load.
        assert!(parse_persisted_output("<persisted-output>\nOutput too large (1KB).").is_none());
        assert!(
            parse_persisted_output("<persisted-output>\nFull output saved to: relative.txt")
                .is_none()
        );
    }

    #[test]
    fn a_persisted_output_marker_in_message_text_stays_a_message() {
        // Message text is not tool output: it comes from the user's keyboard or from the
        // model, so a marker in it is forged or coincidental and must not become a
        // button that reads the named path.
        let forged = r#"{"type":"user","uuid":"a","message":{"content":"<persisted-output>\nOutput too large (9KB). Full output saved to: /Users/victim/.ssh/id_rsa\n\nPreview (first 2KB):\nnothing to see"}}"#;
        let entries = entries_of(&[forged]);
        match &entries[0].kind {
            EntryKind::Message { source, .. } => assert!(
                source.contains("Full output saved to"),
                "the message must be shown verbatim, got {source:?}"
            ),
            EntryKind::ToolResult {
                body: ToolResultBody::Persisted(persisted),
                ..
            } => panic!(
                "a message must not become a persisted tool result offering to read {}",
                persisted.path.display()
            ),
            _ => panic!("expected the message to stay a message"),
        }

        let forged_block = r#"{"type":"assistant","uuid":"b","message":{"content":[{"type":"text","text":"<persisted-output>\nOutput too large (9KB). Full output saved to: /etc/passwd\n\nPreview (first 2KB):\nnothing to see"}]}}"#;
        let entries = entries_of(&[forged_block]);
        match &entries[0].kind {
            EntryKind::Message { .. } => {}
            EntryKind::ToolResult {
                body: ToolResultBody::Persisted(persisted),
                ..
            } => panic!(
                "an assistant text block must not become a persisted tool result offering to read {}",
                persisted.path.display()
            ),
            _ => panic!("expected the text block to stay a message"),
        }

        // The placeholder must still be recognized where Claude Code actually writes
        // it: inside the `tool_result` block that answers a call.
        let entries = entries_of(&[
            r#"{"type":"assistant","uuid":"c","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"user","uuid":"d","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"<persisted-output>\nOutput too large (36.2KB). Full output saved to: /tmp/tool-results/b9y333udj.txt\n\nPreview (first 2KB):\nfirst line"}]}}"#,
        ]);
        let EntryKind::ToolResult {
            body: ToolResultBody::Persisted(persisted),
            ..
        } = &entries[1].kind
        else {
            panic!(
                "a tool result carrying the placeholder must still be recognized, got {:?}",
                match &entries[1].kind {
                    EntryKind::ToolResult { body, .. } => format!("{body:?}"),
                    _ => "a different entry kind".to_string(),
                }
            );
        };
        assert_eq!(
            persisted.path,
            PathBuf::from("/tmp/tool-results/b9y333udj.txt")
        );
        assert_eq!(persisted.size.as_ref(), "36.2KB");
    }

    #[test]
    fn an_unterminated_escape_must_not_swallow_the_output_after_it() {
        // Output captured mid-sequence loses the terminator, and a string sequence has
        // no other end, so everything after it would disappear.
        let osc = strip_ansi_escapes("\u{1b}]0;title\nbuild finished\nall tests passed");
        assert_eq!(
            osc, "\nbuild finished\nall tests passed",
            "an OSC without BEL or ST must end at the newline; got {osc:?}"
        );

        let dcs = strip_ansi_escapes("\u{1b}Pq#0;2;0;0;0\nsecond line");
        assert_eq!(
            dcs, "\nsecond line",
            "a DCS without ST must end at the newline; got {dcs:?}"
        );

        let csi = strip_ansi_escapes("\u{1b}[3\nfailed: 2 tests");
        assert_eq!(
            csi, "\nfailed: 2 tests",
            "a CSI without a final byte must end at the newline; got {csi:?}"
        );

        // What the newline guard must not kill: every properly terminated sequence is
        // still removed whole, including ones whose payload looks like text.
        assert_eq!(strip_ansi_escapes("\u{1b}]0;title\u{7}body"), "body");
        assert_eq!(
            strip_ansi_escapes("\u{1b}]8;;https://example.com\u{1b}\\link\u{1b}]8;;\u{1b}\\ after"),
            "link after"
        );
        assert_eq!(
            strip_ansi_escapes("\u{1b}[1;31mred\u{1b}[0m plain"),
            "red plain"
        );
        assert_eq!(
            strip_ansi_escapes("\u{1b}Pq#0;2;0;0;0#0~~@@vv@@~~@@~~$\u{1b}\\kept"),
            "kept"
        );
    }

    /// `toolUseResult` sits on the record while `tool_result` blocks sit inside it, so
    /// it can only ever name one of the answers a record carries. A record answering two
    /// calls at once must not show the first call's output under the second call's name.
    #[test]
    fn two_results_in_one_record_each_keep_their_own_output() {
        let entries = entries_of(&[
            r#"{"type":"assistant","uuid":"a","message":{"content":[
                {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"echo one"}},
                {"type":"tool_use","id":"t2","name":"Bash","input":{"command":"echo two"}}]}}"#,
            r#"{"type":"user","uuid":"b","toolUseResult":{"stdout":"OUTPUT-ONE","stderr":""},
                "message":{"content":[
                {"type":"tool_result","tool_use_id":"t1","content":"OUTPUT-ONE"},
                {"type":"tool_result","tool_use_id":"t2","content":"OUTPUT-TWO"}]}}"#,
        ]);

        let bodies: Vec<SharedString> = entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::ToolResult {
                    body: ToolResultBody::Inline(text),
                    ..
                } => Some(text.clone()),
                _ => None,
            })
            .collect();

        assert_eq!(
            bodies,
            vec![
                SharedString::from("OUTPUT-ONE"),
                SharedString::from("OUTPUT-TWO")
            ],
            "the record's own `toolUseResult` answers one of the two calls, so each \
             block's own content is what says what its call returned"
        );
    }

    #[test]
    fn a_structured_result_without_text_falls_back_to_the_content_block() {
        // The shape a `Read` leaves behind: the structured result holds the raw file,
        // while the readable, line-numbered rendering sits in the content block.
        let entries = entries_of(&[
            r#"{"type":"assistant","uuid":"a","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/tmp/x.yaml"}}]}}"#,
            r#"{"type":"user","uuid":"b","toolUseResult":{"type":"text","file":{"filePath":"/tmp/x.yaml","content":"game: mapleidel\nsecond line\n"}},
                "message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"1\tgame: mapleidel\n2\tsecond line"}]}}"#,
        ]);

        let EntryKind::ToolResult { label, body, .. } = &entries[1].kind else {
            panic!("expected a tool result");
        };
        assert_eq!(label.as_ref(), "Read");
        assert_eq!(
            body,
            &ToolResultBody::Inline(SharedString::from("1\tgame: mapleidel\n2\tsecond line")),
            "the readable rendering must be preferred over escaped JSON"
        );

        // What the fallback must not kill: a result shape with neither readable text nor
        // a content block beside it is still shown as raw JSON rather than dropped.
        let entries = entries_of(&[
            r#"{"type":"assistant","uuid":"a","message":{"content":[{"type":"tool_use","id":"t1","name":"Search","input":{}}]}}"#,
            r#"{"type":"user","uuid":"b","toolUseResult":{"matches":3,"query":"needle"},
                "message":{"content":[{"type":"tool_result","tool_use_id":"t1"}]}}"#,
        ]);
        let EntryKind::ToolResult {
            body: ToolResultBody::Inline(text),
            ..
        } = &entries[1].kind
        else {
            panic!("expected an inline tool result");
        };
        assert!(
            text.contains("\"matches\": 3") && text.contains("\"query\": \"needle\""),
            "an unknown result shape must still be readable as JSON, got {text:?}"
        );
    }

    #[gpui::test]
    async fn persisted_output_is_read_up_to_the_cap_and_truncation_is_reported(
        cx: &mut gpui::TestAppContext,
    ) {
        let source = LocalSource::new(cx.executor());
        let directory =
            std::env::temp_dir().join(format!("claude-sessions-output-cap-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("creating the temporary directory");
        let cap = MAX_PERSISTED_OUTPUT_BYTES as usize;

        let read = |path: PathBuf| source.read_file(path, MAX_PERSISTED_OUTPUT_BYTES);

        // A file of exactly the cap is legal input and must arrive whole and unlabelled.
        let at_cap = directory.join("at-cap.txt");
        std::fs::write(&at_cap, vec![b'x'; cap]).expect("writing the fixture");
        let text = persisted_output_text(
            read(at_cap.clone())
                .await
                .expect("reading the file at the cap"),
        );
        assert_eq!(
            text.len(),
            cap,
            "a file of exactly the cap must be returned whole, got {} bytes for {cap}",
            text.len()
        );
        assert!(
            !text.contains("truncated"),
            "a file of exactly the cap must not be reported as truncated"
        );

        let over_cap = directory.join("over-cap.txt");
        std::fs::write(&over_cap, vec![b'x'; cap + 1]).expect("writing the fixture");
        let text = persisted_output_text(
            read(over_cap.clone())
                .await
                .expect("reading the file over the cap"),
        );
        let kept = text.chars().filter(|character| *character == 'x').count();
        assert_eq!(
            kept, cap,
            "exactly the cap of content must survive, kept {kept} of {cap}"
        );
        assert!(
            text.contains("truncated by the panel"),
            "a file over the cap must say so, got a tail of {:?}",
            &text[text.len().saturating_sub(80)..]
        );

        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn the_language_of_a_tool_call_follows_the_file_it_names() {
        for (path, language) in [
            ("/a/b/main.rs", "rust"),
            ("/a/b/migrate.py", "python"),
            ("/a/b/panel.ts", "typescript"),
            ("/a/b/settings.json", "json"),
            ("/a/b/README.md", "markdown"),
        ] {
            assert_eq!(
                language_for_path(path),
                language,
                "the language {path} is drawn in"
            );
        }
    }

    #[test]
    fn a_target_with_no_extension_this_panel_knows_is_left_as_plain_text() {
        assert_eq!(language_for_path("/a/b/notes.xyz"), "");
        assert_eq!(language_for_path("/a/b/Makefile"), "");
        assert_eq!(language_for_path(""), "");
    }

    #[test]
    fn a_bash_call_is_drawn_as_the_command_it_runs() {
        let input = json_text(&serde_json::json!({
            "command": "cd /Users/andy/zed && npx ts-node scripts/migrate.ts --apply",
            "description": "Run the migration",
        }));
        let display = tool_input_display(BASH_TOOL_NAME, &input);

        assert_eq!(
            display.text.as_ref(),
            "cd /Users/andy/zed && npx ts-node scripts/migrate.ts --apply",
            "the command itself is what the call is"
        );
        assert_eq!(
            display.code_block().as_ref(),
            "```bash\ncd /Users/andy/zed && npx ts-node scripts/migrate.ts --apply\n```",
            "a command is handed to the markdown element as shell, not as the JSON around it"
        );
    }

    /// A file tool's input is drawn as the file content it carries, in that file's
    /// language. `Read` carries no content — only the path — so it keeps its input, and
    /// that input is JSON rather than the named file's language.
    #[test]
    fn a_file_tool_is_drawn_in_the_language_of_the_file_it_names() {
        let written = "fn main() {}\n";
        let input = json_text(&serde_json::json!({
            "file_path": "/a/b/session_store.rs",
            "content": written,
        }));
        let display = tool_input_display("Write", &input);

        assert_eq!(display.language, "rust");
        assert_eq!(
            display.text.as_ref(),
            written,
            "what a write call is, is the content it writes"
        );
        assert_eq!(
            display.code_block().lines().next().unwrap_or_default(),
            "```rust",
            "the fence of {:?}",
            display.code_block()
        );

        let read_input = json_text(&serde_json::json!({ "file_path": "/a/b/session_store.rs" }));
        let read_display = tool_input_display("Read", &read_input);
        assert_eq!(
            read_display.language, "json",
            "a `Read` call holds a path and no content, so drawing its input as Rust \
             labels JSON as Rust"
        );
        assert_eq!(read_display.text.as_ref(), read_input.as_str());
    }

    #[test]
    fn each_file_tool_is_drawn_as_the_text_it_carries() {
        for (tool_name, field) in [
            ("Write", "content"),
            ("Edit", "new_string"),
            ("NotebookEdit", "new_source"),
        ] {
            let carried = "print('hi')\n";
            let input = json_text(&serde_json::json!({
                "file_path": "/a/b/script.py",
                field: carried,
            }));
            let display = tool_input_display(tool_name, &input);

            assert_eq!(
                display.text.as_ref(),
                carried,
                "{tool_name} must be drawn as its `{field}`"
            );
            assert_eq!(
                display.language, "python",
                "{tool_name} keeps the language of the file it names"
            );
        }
    }

    /// The field a tool usually carries is not always there — an `Edit` that only sets
    /// `old_string`, a half-written call read mid-write — and the whole input is the only
    /// honest thing left to draw.
    #[test]
    fn a_file_tool_missing_its_content_falls_back_to_the_whole_input() {
        let input = json_text(&serde_json::json!({
            "file_path": "/a/b/main.rs",
            "old_string": "let a = 1;",
        }));
        let display = tool_input_display("Edit", &input);

        assert_eq!(
            display.text.as_ref(),
            input.as_str(),
            "with nothing to draw as the file's language, the input itself is what there is"
        );
        assert_eq!(
            display.language, "json",
            "and it must be labelled as the JSON it is"
        );
    }

    #[test]
    fn every_other_tool_keeps_its_input_as_json() {
        let input = json_text(&serde_json::json!({ "pattern": "TODO" }));
        let display = tool_input_display("Grep", &input);

        assert_eq!(display.language, "json");
        assert_eq!(display.text.as_ref(), input.as_str());
    }

    fn subagent(
        agent_id: &str,
        workflow_run_id: Option<&str>,
        tool_use_id: Option<&str>,
    ) -> SubagentSummary {
        SubagentSummary {
            agent_id: agent_id.to_string(),
            workflow_run_id: workflow_run_id.map(str::to_string),
            meta: SubagentMeta {
                agent_type: "general-purpose".to_string(),
                description: None,
                tool_use_id: tool_use_id.map(str::to_string),
                spawn_depth: 1,
                model: None,
                workflow_phase: None,
            },
            transcript_path: PathBuf::from("/nowhere/agent.jsonl"),
            size: 0,
        }
    }

    fn state_of(summary: &SubagentSummary, json_lines: &[&str]) -> AgentState {
        let records: Vec<TranscriptRecord> = json_lines.iter().map(|line| record(line)).collect();
        let path: Vec<&TranscriptRecord> = records.iter().collect();
        agent_state(summary, &path)
    }

    fn tool_result_line_with_content(uuid: &str, tool_use_id: &str, content: &str) -> String {
        serde_json::json!({
            "type": "user",
            "uuid": uuid,
            "message": { "content": [
                { "type": "tool_result", "tool_use_id": tool_use_id, "content": content },
            ] },
        })
        .to_string()
    }

    #[test]
    fn an_agent_whose_call_has_not_been_answered_yet_is_running() {
        let summary = subagent("a0", None, Some("toolu_01"));

        assert_eq!(
            state_of(
                &summary,
                &[
                    &user_message_line("m1", "go"),
                    &tool_use_line("m2", "toolu_01", "Agent", serde_json::json!({})),
                ],
            ),
            AgentState::Running,
            "nothing in the conversation answers `toolu_01`, so the agent is still working"
        );
    }

    #[test]
    fn an_agent_whose_call_has_been_answered_is_finished() {
        let summary = subagent("a0", None, Some("toolu_01"));

        assert_eq!(
            state_of(
                &summary,
                &[
                    &user_message_line("m1", "go"),
                    &tool_use_line("m2", "toolu_01", "Agent", serde_json::json!({})),
                    &tool_result_line("m3", "toolu_01"),
                ],
            ),
            AgentState::Finished,
            "the result of the call that spawned the agent is the agent having returned"
        );
    }

    /// A `Workflow` run's agents carry no `tool_use_id` of their own, so the only thing
    /// pairing them with the conversation is the run id the tool result announces.
    #[test]
    fn a_workflow_agent_runs_until_its_run_id_is_announced_as_answered() {
        let summary = subagent("a1", Some("wf_b529a29d-562"), None);
        let call = tool_use_line("m2", "toolu_09", "Workflow", serde_json::json!({}));

        assert_eq!(
            state_of(&summary, &[&user_message_line("m1", "go"), &call]),
            AgentState::Running,
            "the run has not been announced as over, so its agents are still working"
        );

        let answer = tool_result_line_with_content(
            "m3",
            "toolu_09",
            "Workflow complete.\nRun ID: wf_b529a29d-562\n3 agents finished.",
        );
        assert_eq!(
            state_of(&summary, &[&user_message_line("m1", "go"), &call, &answer]),
            AgentState::Finished,
            "the run id in the tool result is the run having ended"
        );

        let other_run = tool_result_line_with_content(
            "m3",
            "toolu_09",
            "Workflow complete.\nRun ID: wf_something-else",
        );
        assert_eq!(
            state_of(
                &summary,
                &[&user_message_line("m1", "go"), &call, &other_run]
            ),
            AgentState::Running,
            "another run ending says nothing about this one"
        );
    }

    /// Neither id, so nothing in the conversation pairs with this agent at all. Drawing
    /// it as running would be a guess presented as a fact, and a chip that pulses forever
    /// is worse than one that does not pulse.
    #[test]
    fn an_agent_with_neither_id_is_not_claimed_to_be_running() {
        let summary = subagent("a2", None, None);

        assert_eq!(
            state_of(&summary, &[&user_message_line("m1", "go")]),
            AgentState::Finished
        );
        assert_eq!(state_of(&summary, &[]), AgentState::Finished);
    }

    #[test]
    fn a_chip_is_labelled_with_what_the_agent_was_asked_to_do() {
        let mut described = subagent("a0000e203ec41bc73", None, None);
        described.meta.description = Some("round 2 review".to_string());
        assert_eq!(agent_chip_label(&described).as_ref(), "round 2 review");

        // Without a description there is nothing to say but which agent it is, and the
        // whole id is longer than the chip row can carry.
        let plain = subagent("a0000e203ec41bc73", None, None);
        assert_eq!(agent_chip_label(&plain).as_ref(), "general-purpose a0000e");
    }

    /// An id shorter than the six characters a chip shows must not be sliced through, and
    /// a description of nothing but spaces is not a label.
    #[test]
    fn a_chip_label_survives_a_short_id_and_a_blank_description() {
        let mut short = subagent("a0", None, None);
        assert_eq!(agent_chip_label(&short).as_ref(), "general-purpose a0");

        short.meta.description = Some("   ".to_string());
        assert_eq!(
            agent_chip_label(&short).as_ref(),
            "general-purpose a0",
            "a blank description leaves the chip with no label at all"
        );
    }

    #[test]
    fn a_chip_tooltip_names_only_the_fields_the_agent_records() {
        let bare = subagent("a0", None, None);
        assert_eq!(
            agent_chip_tooltip(&bare).as_ref(),
            "general-purpose\ndepth 1"
        );

        let mut full = subagent("a1", Some("wf_1"), None);
        full.meta.model = Some("opus".to_string());
        full.meta.spawn_depth = 2;
        full.meta.workflow_phase = Some("review".to_string());
        assert_eq!(
            agent_chip_tooltip(&full).as_ref(),
            "general-purpose\nopus\ndepth 2\nreview"
        );
    }

    /// An agent's conversation is read-only, and the reader has to be told why the input
    /// under it is dead — with the one reason they can act on first.
    #[test]
    fn reading_an_agent_disables_the_input_and_says_which_chip_to_go_back_to() {
        assert_eq!(
            input_note(false, true, true),
            Some(AGENT_READ_ONLY_NOTE),
            "the way back is one chip away, so that is the reason to give"
        );
        assert!(
            AGENT_READ_ONLY_NOTE.contains("Main"),
            "the note has to name the chip that takes the reader back, got {AGENT_READ_ONLY_NOTE:?}"
        );

        assert_eq!(
            input_note(false, false, true),
            Some(READ_ONLY_NOTE),
            "a session outside tmux keeps its own reason"
        );
        assert_eq!(
            input_note(false, false, false),
            Some(SELECT_A_SESSION_TO_REPLY)
        );
        assert_eq!(
            input_note(true, false, true),
            None,
            "a session that can be typed into is owed no explanation"
        );
    }

    /// The card under a call is only drawn for a call this panel can pair with an agent
    /// on disk; an empty card would say less than no card at all.
    #[test]
    fn only_agent_and_workflow_calls_are_offered_as_conversations_to_open() {
        let lines = [
            user_message_line("m1", "go"),
            tool_use_line(
                "m2",
                "toolu_01",
                "Agent",
                serde_json::json!({ "prompt": "look" }),
            ),
            tool_use_line(
                "m3",
                "toolu_02",
                "Bash",
                serde_json::json!({ "command": "ls" }),
            ),
            tool_use_line("m4", "toolu_03", "Workflow", serde_json::json!({})),
        ];
        let records: Vec<TranscriptRecord> = lines.iter().map(|line| record(line)).collect();
        let path: Vec<&TranscriptRecord> = records.iter().collect();
        let calls = agent_calls(&path);

        let mut keys: Vec<(String, String)> = calls
            .iter()
            .map(|(key, call)| (key.to_string(), call.tool_use_id.to_string()))
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                ("m2#0".to_string(), "toolu_01".to_string()),
                ("m4#0".to_string(), "toolu_03".to_string()),
            ],
            "only the two calls that spawn conversations may carry a card, keyed by the \
             entry the call is drawn as"
        );
    }

    #[test]
    fn a_call_is_paired_with_the_agent_it_spawned() {
        let by_tool_use_id = subagent("a0", None, Some("toolu_01"));
        let by_run_id = subagent("a1", Some("wf_b529a29d-562"), None);
        let subagents = vec![by_tool_use_id, by_run_id];

        let agent_call = AgentCall {
            tool_use_id: SharedString::from("toolu_01"),
            is_workflow: false,
        };
        assert_eq!(
            subagents_of_call(&agent_call, &subagents, &[])
                .iter()
                .map(|summary| summary.agent_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a0"],
            "an `Agent` call is paired by the tool use id its agent records"
        );

        // A `Workflow` run's agents are paired through the run id its result announces.
        let workflow_call = AgentCall {
            tool_use_id: SharedString::from("toolu_09"),
            is_workflow: true,
        };
        let announcement = record(&tool_result_line_with_content(
            "m3",
            "toolu_09",
            "Run ID: wf_b529a29d-562",
        ));
        let path = vec![&announcement];
        assert_eq!(
            subagents_of_call(&workflow_call, &subagents, &path)
                .iter()
                .map(|summary| summary.agent_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a1"]
        );
        assert!(
            subagents_of_call(&workflow_call, &subagents, &[]).is_empty(),
            "a run whose id the conversation has not announced yet pairs with nothing, \
             and an empty card is worse than none"
        );
    }

    #[test]
    fn output_no_longer_than_the_clamp_is_drawn_whole() {
        let output = "one\ntwo\nthree";
        assert!(!output_is_clamped(output, 12), "three lines fit");
        assert_eq!(clamped_output(output, 12), output);

        let at_the_limit = (0..12)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !output_is_clamped(&at_the_limit, 12),
            "twelve lines is not more than twelve"
        );
        assert_eq!(clamped_output(&at_the_limit, 12), at_the_limit.as_str());
    }

    #[test]
    fn output_past_the_clamp_keeps_only_its_first_lines() {
        let output = (0..40)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");

        let clamped = clamped_output(&output, 12);
        assert_eq!(
            clamped.lines().count(),
            12,
            "the clamp keeps twelve of the forty lines, got {clamped:?}"
        );
        assert_eq!(
            output_is_clamped(&output, 12),
            output.lines().count() > 12,
            "forty lines is past a clamp of twelve"
        );
        assert_eq!(clamped.lines().next(), Some("line 0"));
        assert_eq!(clamped.lines().last(), Some("line 11"));
    }

    #[test]
    fn fences_grow_past_backticks_in_the_content() {
        let fenced = fenced_code("a ``` b", "");
        assert!(fenced.starts_with("````\n"));
        assert!(fenced.ends_with("\n````"));
    }

    #[test]
    fn base64_decodes_the_standard_and_url_safe_alphabets() {
        assert_eq!(decode_base64("aGVsbG8="), Some(b"hello".to_vec()));
        assert_eq!(decode_base64("aGVs\nbG8="), Some(b"hello".to_vec()));
        assert_eq!(decode_base64("-_8="), Some(vec![0xfb, 0xff]));
        assert_eq!(decode_base64("not base64!"), None);
    }

    #[test]
    fn ansi_stripping_handles_osc_and_bare_escapes() {
        assert_eq!(
            strip_ansi_escapes("\u{1b}]0;title\u{7}body"),
            "body".to_string()
        );
        assert_eq!(strip_ansi_escapes("\u{1b}Mtext"), "text".to_string());
        assert_eq!(
            strip_ansi_escapes("keep\ttabs\nand newlines"),
            "keep\ttabs\nand newlines"
        );
    }

    #[test]
    fn an_image_block_that_cannot_be_decoded_does_not_dump_its_payload() {
        let long_payload = "A".repeat(4096);
        let line = format!(
            r#"{{"type":"assistant","uuid":"a","message":{{"content":[{{"type":"image","source":{{"type":"base64","media_type":"image/not-a-format","data":"{long_payload}"}}}}]}}}}"#
        );
        let entries = entries_of(&[&line]);

        let EntryKind::Unknown { label, raw } = &entries[0].kind else {
            panic!("expected the undecodable image to fall back to raw JSON");
        };
        assert_eq!(label.as_ref(), "image");
        assert!(raw.contains("<4096 base64 characters>"));
        assert!(!raw.contains(&long_payload));
    }

    /// Builds the record shape 85 of the 86 persisted outputs have in practice: the
    /// structured `toolUseResult` carries stdout truncated to thirty kilobytes with no
    /// marker anywhere in it, plus the path and byte count of the full output, while the
    /// placeholder text sits in the content block beside it.
    fn structured_persisted_result(output_path: &Path) -> String {
        let output_path = output_path.to_string_lossy().into_owned();
        let placeholder = format!(
            "<persisted-output>\nOutput too large (80.6KB). Full output saved to: {output_path}\n\nPreview (first 2KB):\nthe first two kilobytes"
        );
        serde_json::json!({
            "type": "user",
            "uuid": "b",
            "toolUseResult": {
                "stdout": "line one\nline two",
                "stderr": "",
                "interrupted": false,
                "isImage": false,
                "noOutputExpected": false,
                "persistedOutputPath": output_path,
                "persistedOutputSize": 82585,
            },
            "message": {
                "content": [
                    { "type": "tool_result", "tool_use_id": "t1", "content": placeholder },
                ],
            },
        })
        .to_string()
    }

    const TOOL_USE_LINE: &str = r#"{"type":"assistant","uuid":"a","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#;

    fn tool_result_body_with_home(json: &str, home_directory: Option<&Path>) -> ToolResultBody {
        let entries = entries_of_with_home(&[TOOL_USE_LINE, json], home_directory);
        match entries.get(1).map(|entry| &entry.kind) {
            Some(EntryKind::ToolResult { body, .. }) => body.clone(),
            other => panic!(
                "expected a tool result, got {:?}",
                other.map(|kind| matches!(kind, EntryKind::ToolResult { .. }))
            ),
        }
    }

    fn tool_result_body(json: &str) -> ToolResultBody {
        tool_result_body_with_home(json, Some(paths::home_dir()))
    }

    #[test]
    fn structured_persisted_fields_inline_stdout_and_offer_the_full_output() {
        let output_path = paths::home_dir()
            .join(".claude/projects/-Users-andy-zed/4e2e3600/tool-results/b7vd9w357.txt");
        let body = tool_result_body(&structured_persisted_result(&output_path));

        let ToolResultBody::Persisted(persisted) = &body else {
            panic!(
                "a result with persistedOutputPath must offer to load the full output, got {body:?}"
            );
        };
        assert_eq!(
            persisted.preview.as_ref(),
            "line one\nline two",
            "the inline body must be the structured stdout, not the placeholder's two-kilobyte preview"
        );
        assert_eq!(
            persisted.size.as_ref(),
            "80.6KB",
            "the size must come from persistedOutputSize (82585 bytes)"
        );
        assert_eq!(persisted.path, output_path);
    }

    #[test]
    fn a_persisted_path_outside_the_claude_projects_directory_is_not_offered() {
        let offers_the_full_output = |output_path: &Path| {
            matches!(
                tool_result_body(&structured_persisted_result(output_path)),
                ToolResultBody::Persisted(_)
            )
        };

        let outside: Vec<PathBuf> = vec![
            PathBuf::from("/tmp/tool-results/b7vd9w357.txt"),
            paths::home_dir().join(".ssh/id_rsa"),
            paths::home_dir().join(".claude/projects/../../.ssh/id_rsa"),
            paths::home_dir().join(".claude/todos/4e2e3600.json"),
            PathBuf::from("tool-results/b7vd9w357.txt"),
        ];
        let wrongly_offered: Vec<&PathBuf> = outside
            .iter()
            .filter(|output_path| offers_the_full_output(output_path))
            .collect();
        assert!(
            wrongly_offered.is_empty(),
            "a path outside <home>/.claude/projects must not be offered for loading, but these were: {wrongly_offered:#?}"
        );

        // What the check must not kill: every path Claude Code actually persists an
        // output to, tool results and hook output alike, in a session's own directory
        // and in a sub-agent's.
        let inside: Vec<PathBuf> = vec![
            paths::home_dir()
                .join(".claude/projects/-Users-andy-zed/4e2e3600/tool-results/b7vd9w357.txt"),
            paths::home_dir().join(
                ".claude/projects/-Users-andy-zed/4e2e3600/tool-results/hook-6ffbcaa5-stdout.txt",
            ),
            paths::home_dir().join(
                ".claude/projects/-Users-andy-zed/4e2e3600/subagents/tool-results/b7vd9w357.txt",
            ),
        ];
        let wrongly_refused: Vec<&PathBuf> = inside
            .iter()
            .filter(|output_path| !offers_the_full_output(output_path))
            .collect();
        assert!(
            wrongly_refused.is_empty(),
            "a path Claude Code really persists output to must stay loadable, but these were refused: {wrongly_refused:#?}"
        );
    }

    /// A remote project's transcript names paths under the remote machine's home, which
    /// this machine's home says nothing about. The boundary has to be checked against the
    /// home the scan reported, and while it has reported none there is no offer to make.
    #[test]
    fn a_remote_persisted_path_is_offered_against_the_home_the_scan_reported() {
        let output_path = PathBuf::from("/home/deploy/.claude/projects/x/tool-results/a.txt");
        let result = structured_persisted_result(&output_path);
        let remote_home = PathBuf::from("/home/deploy");

        let body = tool_result_body_with_home(&result, Some(&remote_home));
        let ToolResultBody::Persisted(persisted) = &body else {
            panic!(
                "a path under the scanned machine's own .claude/projects must be offered for loading, got {body:?}"
            );
        };
        assert_eq!(persisted.path, output_path);

        let refused_homes: Vec<Option<PathBuf>> = vec![
            // No scan has reported a home yet, so there is nothing to check against.
            None,
            // A server too old to send one must not open the boundary either.
            Some(PathBuf::new()),
            // The bug this replaced: checking a remote path against the local home.
            Some(paths::home_dir().clone()),
        ];
        let wrongly_offered: Vec<&Option<PathBuf>> = refused_homes
            .iter()
            .filter(|home_directory| {
                matches!(
                    tool_result_body_with_home(&result, home_directory.as_deref()),
                    ToolResultBody::Persisted(_)
                )
            })
            .collect();
        assert!(
            wrongly_offered.is_empty(),
            "{output_path:?} must not be offered for loading against these home directories, but was: {wrongly_offered:#?}"
        );
    }

    /// One record for each artifact the cache has to keep across a rebuild: a decoded
    /// image, a pretty-printed unrecognized record, terminal output with its escapes
    /// stripped, and an attachment's rendered text.
    const EXPENSIVE_LINES: [&str; 4] = [
        r#"{"type":"user","uuid":"a","message":{"content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}}]}}"#,
        r#"{"type":"summary","uuid":"b","summary":"a record type with no display rule"}"#,
        r#"{"type":"system","subtype":"local_command","uuid":"c","content":"\u001b[1;31mred\u001b[0m plain"}"#,
        r#"{"type":"attachment","uuid":"d","attachment":{"type":"environment"},"rendered":[{"content":"cwd: /tmp"}]}"#,
    ];

    fn decoded_image(entries: &[Entry]) -> Arc<Image> {
        entries
            .iter()
            .find_map(|entry| match &entry.kind {
                EntryKind::Image { image, .. } => Some(image.clone()),
                _ => None,
            })
            .expect("the conversation contains one image")
    }

    fn local_command_texts(entries: &[Entry]) -> Vec<SharedString> {
        entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::LocalCommand { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn expensive_artifacts_are_derived_once_and_reused_as_the_conversation_grows() {
        let records: Vec<TranscriptRecord> =
            EXPENSIVE_LINES.iter().map(|line| record(line)).collect();
        let one_more_turn = record(
            r#"{"type":"assistant","uuid":"e","message":{"content":[{"type":"text","text":"one more turn"}]}}"#,
        );

        let mut cache = EntryCache::default();
        let path: Vec<&TranscriptRecord> = records.iter().collect();
        let before = build_entries(&path, Some(paths::home_dir()), &mut cache);
        let derived_at_first = cache.derived;
        assert_eq!(
            derived_at_first,
            EXPENSIVE_LINES.len(),
            "the first build must derive one artifact per record"
        );

        // The next turn arrives, which is what the store notifies about up to four times
        // a second while a reply is streaming.
        let mut grown_path = path.clone();
        grown_path.push(&one_more_turn);
        let after = build_entries(&grown_path, Some(paths::home_dir()), &mut cache);

        let derived_again = cache.derived - derived_at_first;
        assert_eq!(
            derived_again, 1,
            "only the record that just arrived may be derived, but {derived_again} artifacts were derived again"
        );
        assert!(
            Arc::ptr_eq(&decoded_image(&before), &decoded_image(&after)),
            "the screenshot must be the same decoded bytes, not a second decoding of them"
        );
        assert_eq!(after.len(), before.len() + 1);
    }

    #[test]
    fn forgetting_an_overwritten_record_keeps_what_the_others_derived() {
        let overwritten =
            record(r#"{"type":"system","subtype":"local_command","uuid":"a","content":"first"}"#);
        let replacement =
            record(r#"{"type":"system","subtype":"local_command","uuid":"a","content":"second"}"#);
        // A uuid that merely starts with the overwritten one: dropping the artifacts of
        // `a` must not drop this record's as well.
        let namesake =
            record(r#"{"type":"system","subtype":"local_command","uuid":"ab","content":"kept"}"#);

        let mut cache = EntryCache::default();
        let entries = build_entries(
            &[&overwritten, &namesake],
            Some(paths::home_dir()),
            &mut cache,
        );
        assert_eq!(
            local_command_texts(&entries),
            vec![SharedString::from("first"), SharedString::from("kept")]
        );

        let derived_at_first = cache.derived;
        cache.forget_record("a");
        let entries = build_entries(
            &[&replacement, &namesake],
            Some(paths::home_dir()),
            &mut cache,
        );

        let derived_again = cache.derived - derived_at_first;
        assert_eq!(
            derived_again, 1,
            "only the replaced record may be derived again, but {derived_again} artifacts were"
        );
        assert_eq!(
            local_command_texts(&entries),
            vec![SharedString::from("second"), SharedString::from("kept")],
            "the replacement's own content must be shown, and its namesake's must be untouched"
        );
    }

    #[test]
    fn a_placeholder_only_result_is_shown_whether_or_not_its_path_can_be_read() {
        // The one persisted output in eighty-six that arrives with no structured fields
        // beside it: the placeholder text is the whole body, so it is shown either way,
        // and only the offer to read the file turns on the path.
        let placeholder_result = |output_path: &Path| {
            let output_path = output_path.to_string_lossy().into_owned();
            serde_json::json!({
                "type": "user",
                "uuid": "b",
                "message": { "content": [ {
                    "type": "tool_result",
                    "tool_use_id": "t1",
                    "content": format!(
                        "<persisted-output>\nOutput too large (36.2KB). Full output saved to: {output_path}\n\nPreview (first 2KB):\nfirst line"
                    ),
                } ] },
            })
            .to_string()
        };

        let outside = PathBuf::from("/tmp/tool-results/b9y333udj.txt");
        let ToolResultBody::Persisted(persisted) = tool_result_body(&placeholder_result(&outside))
        else {
            panic!("the placeholder must still be recognized without structured fields");
        };
        assert_eq!(persisted.preview.as_ref(), "first line");
        assert!(
            !persisted_output_is_loadable(&persisted.path, Some(paths::home_dir())),
            "{} is outside <home>/.claude/projects and must not be offered for loading",
            persisted.path.display()
        );

        let inside = paths::home_dir()
            .join(".claude/projects/-Users-andy-zed/4e2e3600/tool-results/hook-6ffb-stdout.txt");
        let ToolResultBody::Persisted(persisted) = tool_result_body(&placeholder_result(&inside))
        else {
            panic!("the placeholder must still be recognized without structured fields");
        };
        assert!(
            persisted_output_is_loadable(&persisted.path, Some(paths::home_dir())),
            "{} is hook output Claude Code really writes and must stay loadable",
            persisted.path.display()
        );
    }

    #[test]
    fn a_compact_summary_is_not_labelled_as_something_the_user_typed() {
        let entries = entries_of(&[
            r#"{"type":"user","uuid":"a","message":{"content":"my own prompt"}}"#,
            r#"{"type":"system","subtype":"compact_boundary","uuid":"b","logicalParentUuid":"a",
                "compactMetadata":{"trigger":"manual","preTokens":792090,"postTokens":16995}}"#,
            r#"{"type":"user","uuid":"c","isCompactSummary":true,
                "message":{"content":"This session is being continued from a previous conversation"}}"#,
        ]);

        let EntryKind::Message { role, .. } = &entries[0].kind else {
            panic!("expected the user's own prompt to render as a message");
        };
        assert_eq!(
            role.label(),
            "You",
            "an ordinary user message must keep its own label"
        );

        let EntryKind::Message { role, .. } = &entries[2].kind else {
            panic!("expected the compact summary to still render as a message");
        };
        assert_eq!(
            role.label(),
            "Summary",
            "a record with isCompactSummary: true was written by the compaction, not typed \
             by the user, and must not be labelled as if it were"
        );
    }

    #[test]
    fn a_local_command_whose_content_is_not_text_is_kept_as_raw_json() {
        let entries = entries_of(&[r#"{"type":"system","subtype":"local_command","uuid":"a",
                "content":[{"type":"text","text":"ls -l"}]}"#]);

        match &entries[0].kind {
            EntryKind::Unknown { label, raw } => {
                assert_eq!(label.as_ref(), "system / local_command");
                assert!(
                    raw.contains("ls -l"),
                    "the record's payload must survive in the raw JSON, got {raw}"
                );
            }
            EntryKind::LocalCommand { text } => panic!(
                "a local_command whose content is not text must be degraded to raw JSON \
                 rather than dropped: got LocalCommand {{ text: {text:?} }}"
            ),
            _ => panic!("expected either a raw JSON entry or a local command"),
        }
    }

    #[test]
    fn an_attachment_that_names_a_persisted_output_offers_the_whole_file() {
        // Hook output: the marker sits in `attachment.content`, and 1,080 of the 1,128
        // such records in the real transcripts carry no `rendered` array, so the body
        // shown for them is pretty-printed JSON in which the marker's newlines are
        // escaped — the path has to come from the content string itself.
        // Distinct uuids because the cache these are read through is keyed by uuid: two
        // records sharing one are the same record, and `absorb` would keep only the last.
        let attachment_line = |uuid: &str, output_path: &Path| {
            serde_json::json!({
                "type": "attachment",
                "uuid": uuid,
                "attachment": {
                    "type": "hook_success",
                    "hookName": "UserPromptSubmit",
                    "content": format!(
                        "<persisted-output>\nOutput too large (15.8KB). Full output saved to: {}\n\nPreview (first 2KB):\nfirst line of the preview",
                        output_path.display()
                    ),
                },
            })
            .to_string()
        };

        let inside = paths::home_dir()
            .join(".claude/projects/-Users-andy/0f3b76a8/tool-results/hook-f975cad9-stdout.txt");
        let plain = r#"{"type":"attachment","uuid":"b","attachment":{"type":"environment"},
             "rendered":[{"content":"env text"}]}"#;
        let outside = PathBuf::from("/tmp/tool-results/hook-f975cad9-stdout.txt");
        let inside_line = attachment_line("a", &inside);
        let outside_line = attachment_line("c", &outside);

        let entries = entries_of(&[&inside_line, plain, &outside_line]);
        let EntryKind::Attachments { items } = &entries[0].kind else {
            panic!("expected the attachments section");
        };
        assert_eq!(items.len(), 3);

        let Some(persisted) = items[0].persisted.as_ref() else {
            panic!(
                "an attachment naming a persisted output must offer the whole file; got \
                 persisted: None for body {}",
                items[0].body
            )
        };
        assert_eq!(
            persisted.path, inside,
            "the path must be read from the marker text, not from the escaped JSON body"
        );
        assert_eq!(persisted.size.as_ref(), "15.8KB");
        assert_eq!(persisted.preview.as_ref(), "first line of the preview");

        assert!(
            items[1].persisted.is_none(),
            "an attachment with no marker must stay plain text, got {:?}",
            items[1].persisted
        );
        assert!(
            items[2].persisted.is_none(),
            "a marker naming a path outside <home>/.claude/projects must be shown as text \
             and never offered, got {:?}",
            items[2].persisted
        );
    }

    /// A window holding nothing but the panel's input, built exactly as the panel
    /// builds it. The panel itself needs a workspace and a project, and neither takes
    /// part in what these tests are about: the rule that decides whether the answer to
    /// a send may write into the input.
    fn message_input(
        cx: &mut gpui::TestAppContext,
    ) -> (Entity<Editor>, &mut gpui::VisualTestContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme::init(theme::LoadThemes::JustBase, cx);
        });

        cx.add_window_view(|window, cx| {
            let mut editor = Editor::auto_height(1, 8, window, cx);
            editor.set_placeholder_text(MESSAGE_PLACEHOLDER, window, cx);
            editor
        })
    }

    /// What the panel does on the way out of `send_message`: it takes the text and
    /// empties the input without waiting for the send to be answered.
    fn take_the_text(
        input: &Entity<Editor>,
        text: &str,
        cx: &mut gpui::VisualTestContext,
    ) -> String {
        input.update_in(cx, |editor, window, cx| {
            editor.set_text(text, window, cx);
            let taken = editor.text(cx);
            editor.clear(window, cx);
            taken
        })
    }

    #[gpui::test]
    async fn a_send_that_fails_hands_the_message_back(cx: &mut gpui::TestAppContext) {
        let (input, cx) = message_input(cx);
        let sent = take_the_text(&input, "the message that never arrived", cx);

        cx.update(|window, cx| {
            apply_send_outcome(
                &input,
                &sent,
                Err(anyhow::anyhow!("can't find pane %9")),
                window,
                cx,
            );
        });

        assert_eq!(
            input.update(cx, |editor, cx| editor.text(cx)),
            "the message that never arrived",
            "a send the user can do nothing about must not also cost them the message"
        );
    }

    #[gpui::test]
    async fn a_send_that_fails_does_not_overwrite_what_the_user_typed_since(
        cx: &mut gpui::TestAppContext,
    ) {
        let (input, cx) = message_input(cx);
        let sent = take_the_text(&input, "the message that never arrived", cx);

        // The send is answered a round trip later, and the user did not wait for it.
        input.update_in(cx, |editor, window, cx| {
            editor.set_text("what the user is typing now", window, cx)
        });

        cx.update(|window, cx| {
            apply_send_outcome(
                &input,
                &sent,
                Err(anyhow::anyhow!("can't find pane %9")),
                window,
                cx,
            );
        });

        assert_eq!(
            input.update(cx, |editor, cx| editor.text(cx)),
            "what the user is typing now",
            "the newer text is the user's; a failure from before it must not replace it"
        );
    }

    #[gpui::test]
    async fn a_send_that_arrives_leaves_the_input_empty(cx: &mut gpui::TestAppContext) {
        let (input, cx) = message_input(cx);
        let sent = take_the_text(&input, "the message that arrived", cx);

        cx.update(|window, cx| apply_send_outcome(&input, &sent, Ok(()), window, cx));

        assert_eq!(
            input.update(cx, |editor, cx| editor.text(cx)),
            "",
            "a send that worked must leave the emptied input alone"
        );
    }

    fn activity_of(json_lines: &[&str]) -> Activity {
        let records: Vec<TranscriptRecord> = json_lines.iter().map(|line| record(line)).collect();
        let path: Vec<&TranscriptRecord> = records.iter().collect();
        activity(&path)
    }

    fn thinking_line(uuid: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "uuid": uuid,
            "message": { "content": [ { "type": "thinking", "thinking": "weighing it up" } ] },
        })
        .to_string()
    }

    fn tool_use_line(uuid: &str, id: &str, name: &str, input: serde_json::Value) -> String {
        serde_json::json!({
            "type": "assistant",
            "uuid": uuid,
            "message": { "content": [
                { "type": "tool_use", "id": id, "name": name, "input": input },
            ] },
        })
        .to_string()
    }

    fn tool_result_line(uuid: &str, tool_use_id: &str) -> String {
        serde_json::json!({
            "type": "user",
            "uuid": uuid,
            "message": { "content": [
                { "type": "tool_result", "tool_use_id": tool_use_id, "content": "done" },
            ] },
        })
        .to_string()
    }

    /// What a session that is thinking looks like: the model's own thinking is the last
    /// thing written, and it is the only sign the model is moving at all.
    #[test]
    fn a_session_whose_last_record_is_thinking_is_thinking() {
        assert_eq!(
            activity_of(&[&user_message_line("a", "go on"), &thinking_line("b")]),
            Activity::Thinking,
            "the last record is the model's thinking, which nothing has answered yet"
        );
    }

    #[test]
    fn thinking_that_something_else_has_followed_is_not_still_thinking() {
        assert_eq!(
            activity_of(&[
                &user_message_line("a", "go on"),
                &thinking_line("b"),
                &tool_use_line(
                    "c",
                    "t1",
                    "Bash",
                    serde_json::json!({ "command": "cargo test -p claude_sessions" }),
                ),
            ]),
            Activity::RunningTool {
                name: SharedString::from("Bash"),
                target: SharedString::from("cargo test -p claude_sessions"),
            },
            "the thinking is over once the call it led to has been written, and the call \
             is the more useful thing to say"
        );

        assert_eq!(
            activity_of(&[
                &user_message_line("a", "go on"),
                &thinking_line("b"),
                &user_message_line("c", "and now this"),
            ]),
            Activity::Idle,
            "a turn the user has spoken into again is not still thinking"
        );
    }

    #[test]
    fn a_call_the_transcript_has_no_result_for_is_running() {
        assert_eq!(
            activity_of(&[
                &user_message_line("a", "go"),
                &tool_use_line(
                    "b",
                    "t1",
                    "Read",
                    serde_json::json!({
                        "file_path": "/Users/andy/zed/crates/claude_sessions/src/session_store.rs",
                    }),
                ),
            ]),
            Activity::RunningTool {
                name: SharedString::from("Read"),
                target: SharedString::from("session_store.rs"),
            },
            "the file being read is what says which read is running; the path it sits at \
             does not fit on the line"
        );

        assert_eq!(
            activity_of(&[
                &user_message_line("a", "go"),
                &tool_use_line(
                    "b",
                    "t1",
                    "WebSearch",
                    serde_json::json!({ "query": "gpui" })
                ),
            ]),
            Activity::RunningTool {
                name: SharedString::from("WebSearch"),
                target: SharedString::default(),
            },
            "a tool with no rule for naming its target is still worth reporting as running"
        );

        // A command longer than the line has room for: the head of it is what names the
        // call, and the tail is cut rather than the whole thing being dropped.
        let long_command = activity_of(&[
            &user_message_line("a", "go"),
            &tool_use_line(
                "b",
                "t1",
                "Bash",
                serde_json::json!({
                    "command": "  cargo test -p claude_sessions --all-features -- --nocapture  ",
                }),
            ),
        ]);
        let Activity::RunningTool { target, .. } = &long_command else {
            panic!("expected a running tool, got {long_command:?}");
        };
        assert!(
            target.starts_with("cargo test -p claude_sessions"),
            "the head of the command must survive, got {target:?}"
        );
        assert_eq!(
            target.chars().count(),
            TOOL_TARGET_CHARACTERS + 1,
            "{TOOL_TARGET_CHARACTERS} characters and the ellipsis that says there was \
             more, got {target:?} of {} characters",
            target.chars().count()
        );
    }

    #[test]
    fn a_call_whose_result_has_arrived_is_not_running() {
        assert_eq!(
            activity_of(&[
                &user_message_line("a", "go"),
                &tool_use_line("b", "t1", "Bash", serde_json::json!({ "command": "ls" })),
                &tool_result_line("c", "t1"),
            ]),
            Activity::Idle,
            "the result is in the transcript, so the call is over"
        );

        assert_eq!(
            activity_of(&[
                &user_message_line("a", "go"),
                &tool_use_line("b", "t1", "Bash", serde_json::json!({ "command": "ls" })),
                &tool_result_line("c", "t1"),
                &tool_use_line(
                    "d",
                    "t2",
                    "Write",
                    serde_json::json!({ "file_path": "/tmp/notes/plan.md" }),
                ),
            ]),
            Activity::RunningTool {
                name: SharedString::from("Write"),
                target: SharedString::from("plan.md"),
            },
            "the call that is running is the newest one without a result, not the first \
             one in the turn"
        );
    }

    #[test]
    fn a_call_left_unanswered_by_a_turn_that_is_over_is_not_running() {
        assert_eq!(
            activity_of(&[
                &user_message_line("a", "go"),
                &tool_use_line(
                    "b",
                    "t1",
                    "Bash",
                    serde_json::json!({ "command": "sleep 600" })
                ),
                &user_message_line("c", "never mind, do this instead"),
            ]),
            Activity::Idle,
            "a call whose result was never written belongs to a turn the user has already \
             spoken past; reporting it as running is the same lie the registry's `status` \
             field tells"
        );
    }

    #[test]
    fn a_conversation_the_model_has_not_answered_yet_is_idle() {
        assert_eq!(
            activity_of(&[]),
            Activity::Idle,
            "there is no session doing anything here"
        );
        assert_eq!(
            activity_of(&[&user_message_line("a", "hello")]),
            Activity::Idle,
            "a message the model has not started answering is not activity"
        );
    }

    #[test]
    fn nothing_is_drawn_above_the_input_for_an_idle_session() {
        assert_eq!(
            activity_label(&Activity::Idle),
            None,
            "a line that appears and disappears at the end of every turn would move the \
             input under the reader's hands"
        );

        assert_eq!(
            activity_label(&Activity::Thinking).as_deref(),
            Some(THINKING_NOTE),
            "the line is only withheld from a session that is doing nothing"
        );
        assert_eq!(
            activity_label(&Activity::RunningTool {
                name: SharedString::from("Bash"),
                target: SharedString::from("cargo test -p claude_sessions"),
            })
            .as_deref(),
            Some("Running Bash: cargo test -p claude_sessions")
        );
        assert_eq!(
            activity_label(&Activity::RunningTool {
                name: SharedString::from("WebSearch"),
                target: SharedString::default(),
            })
            .as_deref(),
            Some("Running WebSearch"),
            "a tool whose target has no rule still names the tool"
        );
    }

    #[test]
    fn a_slash_command_is_shown_as_the_line_the_user_typed() {
        let with_arguments = "<command-name>/goal</command-name>\n            \
             <command-message>goal</command-message>\n            \
             <command-args>幫我把 claude session panel 都修好</command-args>";
        assert_eq!(
            user_visible_text(with_arguments).as_deref(),
            Some("/goal 幫我把 claude session panel 都修好"),
            "the arguments are the words the user typed, so hiding this record whole \
             would break the conversation exactly where they typed them"
        );

        let entries = entries_of(&[&user_message_line("a", with_arguments)]);
        assert_eq!(
            entries.len(),
            1,
            "the record is one line of conversation, got {} entries",
            entries.len()
        );
        assert!(
            matches!(
                &entries[0].kind,
                EntryKind::SlashCommand { text }
                    if text.as_ref() == "/goal 幫我把 claude session panel 都修好"
            ),
            "a slash command must be drawn as the command it was, not as prose"
        );
    }

    #[test]
    fn a_slash_command_with_no_arguments_is_shown_on_its_own() {
        let no_arguments = "<command-name>/compact</command-name>\n            \
             <command-message>compact</command-message>\n            \
             <command-args></command-args>";
        assert_eq!(
            user_visible_text(no_arguments).as_deref(),
            Some("/compact"),
            "a command that takes no arguments still records the empty block, and the \
             command is all there is to show"
        );
    }

    #[test]
    fn a_record_of_nothing_but_a_local_command_is_not_shown() {
        for text in [
            "<local-command-stdout>total 0</local-command-stdout>",
            "<local-command-caveat>the user ran this command themselves</local-command-caveat>",
        ] {
            assert_eq!(
                user_visible_text(text),
                None,
                "{text} is Claude Code's own record of a local command, with no message \
                 in it to show"
            );
        }
    }

    #[test]
    fn arguments_holding_a_tag_do_not_swallow_the_rest_of_the_record() {
        assert_eq!(
            user_visible_text(
                "<command-name>/ask</command-name><command-args>why is a < b</command-args>"
            )
            .as_deref(),
            Some("/ask why is a < b"),
            "the arguments are text the user typed, and a `<` in them starts nothing"
        );

        // What a record read while it is being written looks like: the arguments were
        // opened and the closing tag has not been written yet.
        let truncated = "<command-name>/ask</command-name><command-args>why is a";
        assert_eq!(
            user_visible_text(truncated).as_deref(),
            Some(truncated),
            "with no closing tag there is nothing to rewrite, and guessing where the \
             arguments ended would eat the rest of the record"
        );
    }

    fn pending_texts(entries: &[Entry]) -> Vec<SharedString> {
        entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Pending { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn pending_ids(pending_sends: &PendingSends) -> Vec<u64> {
        pending_sends.sends.iter().map(|send| send.id).collect()
    }

    /// The conversation as the panel draws it: what the transcript says, then what has
    /// been sent to it and has not turned up yet.
    fn shown(entries: &[Entry], pending_sends: &PendingSends) -> Vec<Entry> {
        let mut shown = entries.to_vec();
        shown.extend(pending_sends.entries());
        shown
    }

    /// The bug this exists for: the CLI writes the user's record when it starts the turn,
    /// so on a busy session there is nothing in the transcript to draw and the message
    /// the user just sent looks like it went nowhere.
    #[test]
    fn a_sent_message_is_shown_before_the_transcript_has_it() {
        let entries = entries_of(&[&user_message_line("a", "an earlier message")]);
        let mut pending_sends = PendingSends::default();
        pending_sends.remember(7, "the message I just sent", &entries);

        let shown = shown(&entries, &pending_sends);
        assert_eq!(
            pending_texts(&shown),
            vec![SharedString::from("the message I just sent")],
            "the message has to be somewhere the moment it is sent"
        );
        assert_eq!(
            message_sources(&shown),
            vec![SharedString::from("an earlier message")],
            "and it must not be mixed into what the transcript itself said"
        );
    }

    #[test]
    fn a_pending_message_goes_when_its_record_arrives() {
        let before = entries_of(&[&user_message_line("a", "an earlier message")]);
        let mut pending_sends = PendingSends::default();
        pending_sends.remember(7, "run the tests", &before);
        assert_eq!(
            pending_texts(&shown(&before, &pending_sends)).len(),
            1,
            "the message is drawn while the transcript has nothing for it"
        );

        // The record the CLI writes carries the same text with its own context appended.
        let after = entries_of(&[
            &user_message_line("a", "an earlier message"),
            &user_message_line(
                "b",
                "run the tests\n<system-reminder>The user opened a file.</system-reminder>",
            ),
        ]);
        pending_sends.pair_with(&after);

        let shown = shown(&after, &pending_sends);
        assert_eq!(
            pending_texts(&shown),
            Vec::<SharedString>::new(),
            "the record is the message now, and drawing both would show it twice"
        );
        assert_eq!(
            message_sources(&shown),
            vec![
                SharedString::from("an earlier message"),
                SharedString::from("run the tests"),
            ],
            "the message is shown once, by the record that arrived"
        );
    }

    #[test]
    fn a_record_of_other_text_leaves_a_pending_message_alone() {
        let before = entries_of(&[&user_message_line("a", "an earlier message")]);
        let mut pending_sends = PendingSends::default();
        pending_sends.remember(7, "run the tests", &before);
        assert_eq!(pending_texts(&shown(&before, &pending_sends)).len(), 1);

        let after = entries_of(&[
            &user_message_line("a", "an earlier message"),
            &user_message_line("b", "something else entirely"),
        ]);
        pending_sends.pair_with(&after);

        assert_eq!(
            pending_texts(&shown(&after, &pending_sends)),
            vec![SharedString::from("run the tests")],
            "a record of other words is not this message arriving, and taking the \
             message down for it would lose it"
        );
    }

    /// A record the conversation was carrying before the send cannot be the send
    /// arriving, whether or not the reader had it on screen at the time. Opening the
    /// history behind a compaction reveals such records, and pairing with one takes the
    /// message down having never delivered it — the one thing a pending message exists
    /// to stop.
    #[test]
    fn opening_the_history_behind_a_compaction_leaves_a_pending_message_alone() {
        let older = user_message_line("a", "keep going");
        let newer = user_message_line("b", "and now this");

        // What the reader is looking at with the history closed: the turn before the
        // compaction is not in the active path.
        let on_screen = entries_of(&[&newer]);
        let mut pending_sends = PendingSends::default();
        pending_sends.remember(7, "keep going", &on_screen);

        // The same conversation with the compacted-away turn revealed.
        let whole_history = entries_of(&[&older, &newer]);
        pending_sends.pair_with(&whole_history);

        assert_eq!(
            pending_texts(&shown(&whole_history, &pending_sends)),
            vec![SharedString::from("keep going")],
            "the only record of this text was written before the send, so the message \
             has not arrived and must still be shown"
        );
    }

    /// What the guard above must not kill: the record a send was measured against can
    /// itself leave the conversation before the send's own record turns up — compaction
    /// runs as a turn starts, which is the same moment the user's record is written —
    /// and the send still has to be paired with the record that did arrive.
    #[test]
    fn a_pending_message_is_paired_after_the_record_it_was_measured_against_is_compacted_away() {
        let on_screen = entries_of(&[&user_message_line("a", "and now this")]);
        let mut pending_sends = PendingSends::default();
        pending_sends.remember(7, "keep going", &on_screen);

        let after_compaction = entries_of(&[&user_message_line("c", "keep going")]);
        pending_sends.pair_with(&after_compaction);

        assert!(
            pending_sends.is_empty(),
            "the send's own record has arrived and drawing both would show it twice, \
             {:?} left",
            pending_ids(&pending_sends)
        );
    }

    /// The other input the guard must not kill: the first thing said in a conversation
    /// has no record before it to be written after, and its record is still what the
    /// send became.
    #[test]
    fn the_first_message_of_a_conversation_is_paired_with_the_record_that_arrives() {
        let mut pending_sends = PendingSends::default();
        pending_sends.remember(7, "the very first thing I said", &[]);

        let arrived = entries_of(&[&user_message_line("a", "the very first thing I said")]);
        pending_sends.pair_with(&arrived);

        assert!(
            pending_sends.is_empty(),
            "the record of the first message is that message arriving, {:?} left",
            pending_ids(&pending_sends)
        );
    }

    #[test]
    fn two_sends_of_the_same_text_are_paired_with_the_two_records_in_order() {
        // A record already showing this text: it was there before either send, so it
        // cannot be what either send became.
        let before = entries_of(&[&user_message_line("a", "keep going")]);
        let mut pending_sends = PendingSends::default();
        let first = pending_sends.remember(7, "keep going", &before);
        let second = pending_sends.remember(7, "keep going", &before);

        pending_sends.pair_with(&before);
        assert_eq!(
            pending_ids(&pending_sends),
            vec![first, second],
            "the record that was already there answers neither send"
        );

        let after_one = entries_of(&[
            &user_message_line("a", "keep going"),
            &user_message_line("b", "keep going"),
        ]);
        pending_sends.pair_with(&after_one);
        assert_eq!(
            pending_ids(&pending_sends),
            vec![second],
            "the first record answers the first send, and the second send is still waiting"
        );

        let after_two = entries_of(&[
            &user_message_line("a", "keep going"),
            &user_message_line("b", "keep going"),
            &user_message_line("c", "keep going"),
        ]);
        pending_sends.pair_with(&after_two);
        assert!(
            pending_sends.is_empty(),
            "both sends have their record now, {:?} left",
            pending_ids(&pending_sends)
        );
    }

    #[test]
    fn a_send_that_failed_takes_its_pending_message_down() {
        let mut pending_sends = PendingSends::default();
        let failed = pending_sends.remember(7, "the message that never arrived", &[]);
        let arrived = pending_sends.remember(7, "the message that did", &[]);
        assert_eq!(pending_ids(&pending_sends), vec![failed, arrived]);

        pending_sends.resolve(arrived, &Ok(()));
        assert_eq!(
            pending_ids(&pending_sends),
            vec![failed, arrived],
            "a send that arrived is waiting for its record to be written, not finished"
        );

        pending_sends.resolve(failed, &Err(anyhow::anyhow!("can't find pane %9")));
        assert_eq!(
            pending_ids(&pending_sends),
            vec![arrived],
            "a send that failed will never have a record, and the text goes back to the \
             input instead"
        );
    }

    #[test]
    fn pending_messages_do_not_follow_the_reader_to_another_session() {
        let mut pending_sends = PendingSends::default();
        pending_sends.remember(7, "for the session I was reading", &[]);
        let nine = pending_sends.remember(9, "for the session I am reading now", &[]);
        assert_eq!(pending_ids(&pending_sends).len(), 2);

        pending_sends.retain_session(Some(9));
        assert_eq!(
            pending_ids(&pending_sends),
            vec![nine],
            "a pending message belongs to the conversation it was sent to"
        );

        pending_sends.retain_session(None);
        assert!(
            pending_sends.is_empty(),
            "with no session selected there is no conversation to draw it in, {:?} left",
            pending_ids(&pending_sends)
        );
    }

    #[test]
    fn dismissing_one_pending_message_leaves_the_others() {
        let mut pending_sends = PendingSends::default();
        let first = pending_sends.remember(7, "same text", &[]);
        let second = pending_sends.remember(7, "same text", &[]);
        let third = pending_sends.remember(7, "another message", &[]);
        assert_eq!(pending_ids(&pending_sends), vec![first, second, third]);

        pending_sends.dismiss(second);
        assert_eq!(
            pending_ids(&pending_sends),
            vec![first, third],
            "the button takes down the message it sits on, not the one that reads the same"
        );
    }

    const SCRIPTED_PROCESS_ID: u32 = 4242;
    const SCRIPTED_SESSION_ID: &str = "scripted-session";
    const SCRIPTED_AGENT_ID: &str = "a1";
    /// Longer than both of the store's poll intervals, so that advancing by it lets a
    /// scan and a read of each conversation land.
    const A_FEW_POLLS: Duration = Duration::from_secs(2);

    fn scripted_agent() -> TranscriptTarget {
        TranscriptTarget::Subagent {
            agent_id: SCRIPTED_AGENT_ID.to_string(),
            workflow_run_id: None,
        }
    }

    /// One line of a conversation being followed, chained to the record before it so
    /// that the traversal keeps both of them, and marked as an agent's or the session's
    /// own.
    fn chained_line(
        record_type: &str,
        uuid: &str,
        parent_uuid: Option<&str>,
        is_sidechain: bool,
        content: serde_json::Value,
    ) -> String {
        serde_json::json!({
            "type": record_type,
            "uuid": uuid,
            "parentUuid": parent_uuid,
            "isSidechain": is_sidechain,
            "message": { "content": content },
        })
        .to_string()
    }

    fn unanswered_bash_call(uuid: &str, parent_uuid: Option<&str>, is_sidechain: bool) -> String {
        chained_line(
            ASSISTANT_RECORD_TYPE,
            uuid,
            parent_uuid,
            is_sidechain,
            serde_json::json!([
                {
                    "type": "tool_use",
                    "id": "call-1",
                    "name": "Bash",
                    "input": { "command": "cargo test -p claude_sessions" },
                }
            ]),
        )
    }

    /// The two conversations of one session, handed to the store as reads rather than as
    /// files. What these tests are about is which of the two a decision is read from, and
    /// nothing else about a transcript file takes part in that.
    struct ScriptedSource {
        home_directory: PathBuf,
        session_lines: Mutex<Vec<String>>,
        agent_lines: Mutex<Vec<String>>,
    }

    impl ScriptedSource {
        fn new(session_lines: &[String], agent_lines: &[String]) -> Self {
            Self {
                home_directory: PathBuf::from("/scripted-home"),
                session_lines: Mutex::new(session_lines.to_vec()),
                agent_lines: Mutex::new(agent_lines.to_vec()),
            }
        }

        /// Hands over whatever has not been delivered yet, so that the poll's later turns
        /// report a file that has stopped growing rather than the same records again.
        fn deliver(
            lines: &Mutex<Vec<String>>,
            path: PathBuf,
            state: TailState,
        ) -> Task<anyhow::Result<TailProgress>> {
            let delivered = std::mem::take(&mut *lines.lock().expect("reading the scripted lines"));
            let offset = state
                .offset
                .saturating_add(delivered.len().try_into().unwrap_or(u64::MAX));
            Task::ready(Ok(TailProgress {
                path: Some(path),
                start_offset: state.offset,
                offset,
                pending: Vec::new(),
                lines: delivered,
                restarted: false,
            }))
        }
    }

    impl SessionSource for ScriptedSource {
        fn list_sessions(
            &self,
            _project_root: Option<PathBuf>,
        ) -> Task<anyhow::Result<SessionListing>> {
            Task::ready(Ok(SessionListing {
                sessions: vec![SessionSummary {
                    session: RegisteredSession {
                        process_id: SCRIPTED_PROCESS_ID,
                        session_id: SCRIPTED_SESSION_ID.to_string(),
                        working_directory: PathBuf::from("/scripted-project"),
                        process_start: "Thu Sep 10 02:27:23 2026".to_string(),
                        version: "2.1.267".to_string(),
                        kind: "interactive".to_string(),
                        name: Some("scripted".to_string()),
                        status: None,
                        updated_at: None,
                        // No pane to type into: nothing in these tests sends, and this
                        // is what makes a send impossible rather than merely unused.
                        tmux_target: None,
                        bridge_session_id: None,
                    },
                    transcript_path: Some(self.home_directory.join("session.jsonl")),
                }],
                home_directory: self.home_directory.clone(),
            }))
        }

        fn tail_transcript(
            &self,
            _session_id: String,
            state: TailState,
        ) -> Task<anyhow::Result<TailProgress>> {
            Self::deliver(
                &self.session_lines,
                self.home_directory.join("session.jsonl"),
                state,
            )
        }

        fn list_subagents(
            &self,
            _session_id: String,
        ) -> Task<anyhow::Result<Vec<SubagentSummary>>> {
            Task::ready(Ok(vec![SubagentSummary {
                agent_id: SCRIPTED_AGENT_ID.to_string(),
                workflow_run_id: None,
                meta: SubagentMeta {
                    agent_type: "general-purpose".to_string(),
                    description: Some("read the file".to_string()),
                    tool_use_id: None,
                    spawn_depth: 1,
                    model: None,
                    workflow_phase: None,
                },
                transcript_path: self.home_directory.join("agent-a1.jsonl"),
                size: 1,
            }]))
        }

        fn tail_subagent(
            &self,
            _session_id: String,
            _agent_id: String,
            _workflow_run_id: Option<String>,
            state: TailState,
        ) -> Task<anyhow::Result<TailProgress>> {
            Self::deliver(
                &self.agent_lines,
                self.home_directory.join("agent-a1.jsonl"),
                state,
            )
        }

        fn read_file(&self, _path: PathBuf, _max_bytes: u64) -> Task<anyhow::Result<FileContents>> {
            Task::ready(Ok(FileContents {
                bytes: Vec::new(),
                truncated: false,
            }))
        }

        fn send_input(
            &self,
            _pane_target: String,
            _input: SessionInput,
        ) -> Task<anyhow::Result<()>> {
            Task::ready(Err(anyhow::anyhow!(
                "nothing in these tests may reach a session"
            )))
        }
    }

    /// A root view for the test window. The panel is never drawn: these tests are about
    /// which conversation it reads rather than about how it looks, and drawing it would
    /// pull in the whole of the application's theme wiring.
    struct NoUi;

    impl Render for NoUi {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// The panel over a store fed by [`ScriptedSource`], with the fields
    /// [`ClaudeSessionsPanel::new`] gives it.
    ///
    /// The workspace and the project take no part in what these tests are about — which
    /// of the two conversations a decision is read from — and the panel holds the
    /// workspace only to open a file a conversation names, which none of them do.
    fn scripted_panel(
        session_lines: &[String],
        agent_lines: &[String],
        cx: &mut gpui::TestAppContext,
    ) -> Entity<ClaudeSessionsPanel> {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme::init(theme::LoadThemes::JustBase, cx);
        });

        let source: Arc<dyn SessionSource> =
            Arc::new(ScriptedSource::new(session_lines, agent_lines));
        let window = cx.add_window(|_window, _cx| NoUi);
        window
            .update(cx, |_, window, cx| {
                cx.new(|cx| {
                    let store = cx.new(|cx| ClaudeSessionStore::new(source.clone(), None, cx));
                    let store_subscription =
                        cx.observe(&store, |this: &mut ClaudeSessionsPanel, _, cx| {
                            this.rebuild_entries(cx);
                            this.sync_input_availability(cx);
                            cx.notify();
                        });
                    let message_editor = cx.new(|cx| {
                        let mut editor = Editor::auto_height(1, 8, window, cx);
                        editor.set_placeholder_text(MESSAGE_PLACEHOLDER, window, cx);
                        editor.set_read_only(true);
                        editor
                    });

                    ClaudeSessionsPanel {
                        workspace: WeakEntity::new_invalid(),
                        focus_handle: cx.focus_handle(),
                        fs: fs::FakeFs::new(cx.background_executor().clone()),
                        store,
                        source,
                        message_editor,
                        project_root: None,
                        entries: Vec::new(),
                        pending_sends: PendingSends::default(),
                        activity: Activity::Idle,
                        agent_calls: HashMap::default(),
                        list_state: ListState::new(0, ListAlignment::Bottom, px(1024.)),
                        session_list_expanded: true,
                        unread_below: UnreadBelow::default(),
                        expanded: HashSet::default(),
                        show_full_history: false,
                        markdowns: HashMap::default(),
                        entry_cache: EntryCache::default(),
                        cached_transcript_generation: 0,
                        cached_home_directory: None,
                        overwrites_dropped: 0,
                        loaded_outputs: HashMap::default(),
                        output_loads: HashMap::default(),
                        _store_subscription: store_subscription,
                    }
                })
            })
            .expect("building the panel in the test window")
    }

    /// Selects the scripted session and lets the polls deliver its own conversation.
    fn read_the_scripted_session(
        panel: &Entity<ClaudeSessionsPanel>,
        cx: &mut gpui::TestAppContext,
    ) {
        cx.executor().advance_clock(A_FEW_POLLS);
        cx.run_until_parked();
        panel.update(cx, |panel, cx| {
            panel.select_session(SCRIPTED_PROCESS_ID, cx)
        });
        cx.executor().advance_clock(A_FEW_POLLS);
        cx.run_until_parked();
    }

    fn read_the_conversation_of(
        target: TranscriptTarget,
        panel: &Entity<ClaudeSessionsPanel>,
        cx: &mut gpui::TestAppContext,
    ) {
        panel.update(cx, |panel, cx| panel.select_transcript_target(target, cx));
        cx.executor().advance_clock(A_FEW_POLLS);
        cx.run_until_parked();
    }

    /// The bug this is about: an agent that has returned leaves its own conversation
    /// ending in a call nothing answered, so reading the activity off the conversation on
    /// screen leaves `Running …` pulsing above an input the reader cannot even type into.
    #[gpui::test]
    async fn an_agent_that_has_returned_does_not_pulse_running_above_the_input(
        cx: &mut gpui::TestAppContext,
    ) {
        let session_lines = [
            chained_line(
                USER_RECORD_TYPE,
                "m1",
                None,
                false,
                serde_json::json!("go on"),
            ),
            chained_line(
                ASSISTANT_RECORD_TYPE,
                "m2",
                Some("m1"),
                false,
                serde_json::json!([{ "type": "text", "text": "all done." }]),
            ),
        ];
        let agent_lines = [
            chained_line(
                USER_RECORD_TYPE,
                "s1",
                None,
                true,
                serde_json::json!("read the file"),
            ),
            unanswered_bash_call("s2", Some("s1"), true),
        ];

        let panel = scripted_panel(&session_lines, &agent_lines, cx);
        read_the_scripted_session(&panel, cx);
        read_the_conversation_of(scripted_agent(), &panel, cx);

        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.activity,
                Activity::Idle,
                "the session has answered and is doing nothing; the unanswered call is in \
                 the returned agent's conversation, not in the session's"
            );
        });
    }

    #[gpui::test]
    async fn the_activity_line_reads_the_session_and_not_the_agent_on_screen(
        cx: &mut gpui::TestAppContext,
    ) {
        let session_lines = [
            chained_line(
                USER_RECORD_TYPE,
                "m1",
                None,
                false,
                serde_json::json!("go on"),
            ),
            chained_line(
                ASSISTANT_RECORD_TYPE,
                "m2",
                Some("m1"),
                false,
                serde_json::json!([{ "type": "thinking", "thinking": "weighing it up" }]),
            ),
        ];
        let agent_lines = [
            chained_line(
                USER_RECORD_TYPE,
                "s1",
                None,
                true,
                serde_json::json!("read the file"),
            ),
            unanswered_bash_call("s2", Some("s1"), true),
        ];

        let panel = scripted_panel(&session_lines, &agent_lines, cx);
        read_the_scripted_session(&panel, cx);
        read_the_conversation_of(scripted_agent(), &panel, cx);

        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.activity,
                Activity::Thinking,
                "the line says what this session is doing, and it is thinking whichever \
                 conversation the reader has open"
            );
        });
    }

    /// A pending message was sent to the session, so it belongs to the session's own
    /// conversation and is not drawn into an agent's records.
    #[gpui::test]
    async fn a_pending_message_is_not_drawn_into_an_agents_conversation(
        cx: &mut gpui::TestAppContext,
    ) {
        let session_lines = [chained_line(
            USER_RECORD_TYPE,
            "m1",
            None,
            false,
            serde_json::json!("an earlier message"),
        )];
        let agent_lines = [
            chained_line(
                USER_RECORD_TYPE,
                "s1",
                None,
                true,
                serde_json::json!("read the file"),
            ),
            chained_line(
                ASSISTANT_RECORD_TYPE,
                "s2",
                Some("s1"),
                true,
                serde_json::json!([{ "type": "text", "text": "read it." }]),
            ),
        ];

        let panel = scripted_panel(&session_lines, &agent_lines, cx);
        read_the_scripted_session(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel
                .pending_sends
                .remember(SCRIPTED_PROCESS_ID, "and the lint", &panel.entries);
            panel.rebuild_entries(cx);
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                pending_texts(&panel.entries),
                vec![SharedString::from("and the lint")],
                "the message is drawn in the conversation it was sent to"
            );
        });

        read_the_conversation_of(scripted_agent(), &panel, cx);
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                pending_texts(&panel.entries),
                Vec::<SharedString>::new(),
                "nothing the reader sent to the session belongs in an agent's conversation"
            );
            assert_eq!(
                pending_ids(&panel.pending_sends).len(),
                1,
                "the message is only undrawn, not taken down: it is still waiting for the \
                 record it was sent as"
            );
        });

        read_the_conversation_of(TranscriptTarget::Main, &panel, cx);
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                pending_texts(&panel.entries),
                vec![SharedString::from("and the lint")],
                "back in the session's own conversation the message is drawn again"
            );
        });
    }

    /// An agent is given its task as a user record of its own conversation, so an
    /// agent's records hold the very text a send carries. Pairing against them takes
    /// down a message that never arrived, and the reader is left with no sign of it at
    /// all once they return to the session.
    #[gpui::test]
    async fn an_agents_own_prompt_does_not_pair_with_a_message_sent_to_the_session(
        cx: &mut gpui::TestAppContext,
    ) {
        let session_lines = [chained_line(
            USER_RECORD_TYPE,
            "m1",
            None,
            false,
            serde_json::json!("an earlier message"),
        )];
        let agent_lines = [chained_line(
            USER_RECORD_TYPE,
            "s1",
            None,
            true,
            serde_json::json!("run the tests"),
        )];

        let panel = scripted_panel(&session_lines, &agent_lines, cx);
        read_the_scripted_session(&panel, cx);
        panel.update(cx, |panel, cx| {
            panel
                .pending_sends
                .remember(SCRIPTED_PROCESS_ID, "run the tests", &panel.entries);
            panel.rebuild_entries(cx);
        });

        read_the_conversation_of(scripted_agent(), &panel, cx);
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                pending_ids(&panel.pending_sends).len(),
                1,
                "the session has written no record of this message; the agent's own \
                 prompt reading the same is not it arriving"
            );
        });

        read_the_conversation_of(TranscriptTarget::Main, &panel, cx);
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                pending_texts(&panel.entries),
                vec![SharedString::from("run the tests")],
                "a message that never arrived must still be shown as waiting"
            );
        });
    }

    /// The bookkeeping of a pending message is written in the keys
    /// [`build_entries`] gives the records, and [`user_message_entries`] reads the same
    /// records without deriving the rest of the conversation. The two must agree key for
    /// key, or a pending message is measured against one set of keys and paired against
    /// another.
    #[test]
    fn the_user_messages_read_off_a_path_are_the_ones_the_built_entries_carry() {
        let lines = [
            user_message_line("u1", "a message with string content"),
            serde_json::json!({
                "type": "user",
                "uuid": "u2",
                "message": { "content": [
                    { "type": "tool_result", "tool_use_id": "call-1", "content": "done" },
                    { "type": "text", "text": "a message after a result" },
                ] },
            })
            .to_string(),
            user_message_line(
                "u3",
                "<command-name>/goal</command-name>\n<command-args>ship it</command-args>",
            ),
            user_message_line(
                "u4",
                "<system-reminder>nothing the user typed</system-reminder>",
            ),
            serde_json::json!({
                "type": "user",
                "uuid": "u5",
                "isCompactSummary": true,
                "message": { "content": "a recap of the conversation" },
            })
            .to_string(),
            serde_json::json!({
                "type": "user",
                "uuid": "u6",
                "subtype": LOCAL_COMMAND_SUBTYPE,
                "content": "the output of a command the user ran",
            })
            .to_string(),
            serde_json::json!({
                "type": ATTACHMENT_RECORD_TYPE,
                "uuid": "u7",
                "content": "context Claude Code assembled",
            })
            .to_string(),
            serde_json::json!({
                "type": "assistant",
                "uuid": "u8",
                "message": { "content": [{ "type": "text", "text": "the model's own words" }] },
            })
            .to_string(),
            // No uuid, so it is keyed by its position in the path.
            serde_json::json!({
                "type": "user",
                "message": { "content": "a message no record can reference" },
            })
            .to_string(),
        ];

        let records: Vec<TranscriptRecord> = lines.iter().map(|line| record(line)).collect();
        let path: Vec<&TranscriptRecord> = records.iter().collect();
        let built = build_entries(&path, Some(paths::home_dir()), &mut EntryCache::default());

        assert_eq!(
            user_messages(&user_message_entries(&path)),
            user_messages(&built),
            "the pairing of a pending message reads these keys, so they must be the keys \
             the drawn conversation carries"
        );
        assert!(
            !user_messages(&built).is_empty(),
            "a fixture with no user messages in it would hold nothing together"
        );
    }
}
