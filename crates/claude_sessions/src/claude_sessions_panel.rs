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
};

use collections::{HashMap, HashSet};
use editor::Editor;
use gpui::{
    AnyElement, AsyncWindowContext, Entity, EventEmitter, FocusHandle, Focusable, Image,
    ImageFormat, ListAlignment, ListSizingBehavior, ListState, Render, Subscription, Task,
    WeakEntity, img, list,
};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use serde_json::Value;
use ui::{Button, Divider, ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::{
    ClaudeSessionStore, Interrupt, RegisteredSession, SendMessage, ToggleFocus, TranscriptRecord,
    session_source::{FileContents, LocalSource, RemoteSource, SessionInput, SessionSource},
};

const CLAUDE_SESSIONS_PANEL_KEY: &str = "ClaudeSessionsPanel";

const COMPACT_BOUNDARY_SUBTYPE: &str = "compact_boundary";
const LOCAL_COMMAND_SUBTYPE: &str = "local_command";
const ATTACHMENT_RECORD_TYPE: &str = "attachment";
const ATTACHMENTS_ENTRY_KEY: &str = "context-attachments";

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

pub struct ClaudeSessionsPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    position: DockPosition,
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
    list_state: ListState,
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

    fn color(self) -> Color {
        match self {
            Self::User => Color::Muted,
            Self::Assistant => Color::Accent,
            Self::Other => Color::Muted,
            Self::CompactSummary => Color::Accent,
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
                position: DockPosition::Right,
                store,
                source,
                message_editor,
                project_root,
                entries: Vec::new(),
                list_state: ListState::new(0, ListAlignment::Bottom, px(1024.)),
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
        // Whether the previous conversation had been compacted says nothing about this
        // one, so the history toggle starts closed again.
        self.show_full_history = false;
        self.store
            .update(cx, |store, cx| store.select(process_id, cx));
    }

    fn toggle_full_history(&mut self, cx: &mut Context<Self>) {
        self.show_full_history = !self.show_full_history;
        // A different path is a different conversation as far as the list is concerned;
        // none of the measured heights line up with the new indices.
        self.entries.clear();
        self.list_state.reset(0);
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
        let new_entries = {
            let store = self.store.read(cx);
            let home_directory = store.home_directory();
            let transcript = store.transcript();
            let path = if self.show_full_history {
                transcript.full_path()
            } else {
                transcript.active_path()
            };
            build_entries(&path, home_directory, &mut cache)
        };
        self.entry_cache = cache;

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
            if let EntryKind::Attachments { items } = &entry.kind {
                for item in items {
                    keys.insert(item.key.clone());
                }
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
        let session_count = sessions.len();

        v_flex()
            .child(
                h_flex()
                    .p_1()
                    .gap_1()
                    .justify_between()
                    .child(
                        Label::new("Claude Sessions")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(format!("{session_count}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
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
            .child(
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

        ListItem::new(SharedString::from(format!("claude-session-{index}")))
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(is_selected)
            .start_slot(
                Icon::new(IconName::Indicator)
                    .size(IconSize::XSmall)
                    .color(if is_busy {
                        Color::Success
                    } else {
                        Color::Hidden
                    }),
            )
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

        list(
            self.list_state.clone(),
            cx.processor(|this, index: usize, window, cx| this.render_entry(index, window, cx)),
        )
        .with_sizing_behavior(ListSizingBehavior::Auto)
        .flex_grow_1()
        .into_any_element()
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

    /// A session Zed has no pane to type into can only be read, and so can no session at
    /// all; the input is disabled in both cases rather than hidden, so that the reason is
    /// visible where the reply would be typed.
    fn sync_input_availability(&mut self, cx: &mut Context<Self>) {
        let read_only = self.store.read(cx).pane_target().is_none();
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
        if self.store.read(cx).pane_target().is_none() {
            return;
        }

        let text = self.message_editor.read(cx).text(cx);
        if text.trim().is_empty() {
            return;
        }

        let send = self.store.update(cx, |store, cx| {
            store.send_input(SessionInput::Text(text.clone()), cx)
        });
        // Emptied before the send is answered: the answer can be a round trip to
        // another machine away, and the input has to be usable again at once.
        self.message_editor
            .update(cx, |editor, cx| editor.clear(window, cx));

        cx.spawn_in(window, async move |this, cx| {
            let outcome = send.await;
            this.update_in(cx, |this, window, cx| {
                apply_send_outcome(&this.message_editor, &text, outcome, window, cx);
            })
            .log_err();
        })
        // Detached rather than held in a field: dropping it would cancel the send
        // itself, and a second send must not cut the first one short.
        .detach();
    }

    fn interrupt_session(&mut self, _: &Interrupt, _window: &mut Window, cx: &mut Context<Self>) {
        if self.store.read(cx).pane_target().is_none() {
            return;
        }

        self.store.update(cx, |store, cx| {
            // There is no text to hand back for an interrupt, and the store reports the
            // failure itself, so nothing here waits for the answer.
            store.send_input(SessionInput::Escape, cx).detach()
        });
    }

    fn render_input(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let has_selection = store.selected().is_some();
        let can_send = store.pane_target().is_some();
        let note = if can_send {
            None
        } else if has_selection {
            Some(READ_ONLY_NOTE)
        } else {
            Some(SELECT_A_SESSION_TO_REPLY)
        };

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
                                    .color(Color::Muted),
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
                    index,
                    &key,
                    IconName::ToolThink,
                    Color::Muted,
                    "Thinking".into(),
                    Some(first_line(&source)),
                    is_expanded,
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
                let header = self.render_disclosure_header(
                    index,
                    &key,
                    IconName::ToolHammer,
                    Color::Muted,
                    name,
                    Some(first_line(&input)),
                    is_expanded,
                    cx,
                );
                let body = if is_expanded {
                    let markdown = self.markdown_for(&key, fenced_code(&input, "json"), cx);
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
                    index,
                    &key,
                    if is_error {
                        IconName::XCircle
                    } else {
                        IconName::ToolTerminal
                    },
                    if is_error { Color::Error } else { Color::Muted },
                    label,
                    Some(summary),
                    is_expanded,
                    cx,
                );

                let rendered_body = if is_expanded {
                    Some(match body {
                        ToolResultBody::Inline(text) => {
                            let markdown = self.markdown_for(&key, fenced_code(&text, ""), cx);
                            div()
                                .w_full()
                                .px_2()
                                .pb_1()
                                .child(MarkdownElement::new(markdown, markdown_style(window, cx)))
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
                    index,
                    &key,
                    IconName::Paperclip,
                    Color::Muted,
                    "Context".into(),
                    Some(SharedString::from(format!("{} attachments", items.len()))),
                    is_expanded,
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
                    index,
                    &key,
                    IconName::Json,
                    Color::Hidden,
                    SharedString::from(format!("Unrecognized: {label}")),
                    None,
                    is_expanded,
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
        let markdown = self.markdown_for(key, fenced_code(&displayed_text, ""), cx);

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
            .child(MarkdownElement::new(markdown, markdown_style(window, cx)))
            .children(action)
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_disclosure_header(
        &self,
        entry_index: usize,
        key: &SharedString,
        icon: IconName,
        icon_color: Color,
        title: SharedString,
        summary: Option<SharedString>,
        is_expanded: bool,
        cx: &mut Context<Self>,
    ) -> ListItem {
        let toggle_key = key.clone();
        let click_key = key.clone();

        ListItem::new(SharedString::from(format!("entry-{entry_index}")))
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
            .child(self.render_transcript_section(cx))
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

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.position = position;
        cx.notify();
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

fn markdown_style(window: &Window, cx: &App) -> MarkdownStyle {
    MarkdownStyle::themed(MarkdownFont::Agent, window, cx)
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
            let kind = cache.kind(cache_key.as_ref(), || message_kind(record, text));
            entries.push(Entry {
                key: base_key,
                kind,
            });
        }
        Some(Value::Array(blocks)) => {
            for (block_index, block) in blocks.iter().enumerate() {
                let key = SharedString::from(format!("{base_key}#{block_index}"));
                // Registered outside the cache: a `tool_result` block being built for the
                // first time looks up the name of its call here, and the `tool_use` block
                // that named it may itself have come from the cache.
                register_tool_name(block, tool_names);
                let cache_key = cache_key(record, &key);
                let kind = cache.kind(cache_key.as_ref(), || {
                    block_kind(record, block, tool_names, home_directory)
                });
                entries.push(Entry { key, kind });
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
fn message_kind(record: &TranscriptRecord, text: &str) -> EntryKind {
    EntryKind::Message {
        // The summary compaction leaves behind is recorded as a user message, so the
        // record type alone would present a machine-written recap of the conversation as
        // something the user had typed.
        role: if record.is_compact_summary {
            MessageRole::CompactSummary
        } else {
            MessageRole::from_record_type(&record.record_type)
        },
        source: SharedString::from(text.to_string()),
    }
}

fn block_kind(
    record: &TranscriptRecord,
    block: &Value,
    tool_names: &HashMap<String, SharedString>,
    home_directory: Option<&Path>,
) -> EntryKind {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => {
            let text = block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            message_kind(record, text)
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
            let structured_result = record.raw.get("toolUseResult");
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
    }
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
}
