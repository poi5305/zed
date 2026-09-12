//! Renders the sessions of Claude Code running on this machine, and the conversation of
//! the selected one.
//!
//! One type draws both halves, in two views that share a store: the dock panel draws the
//! list of sessions, and an editor tab — the same type with `in_pane` set — draws the
//! conversation and the input that talks to it. A transcript is not readable at the
//! width of a dock, and the selection lives in the store rather than in either view, so
//! choosing a session in the dock is what the tab shows.
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
    AbsoluteLength, Animation, AnimationExt as _, AnyElement, AsyncWindowContext, DragMoveEvent,
    Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, Image, ImageFormat, Length,
    ListAlignment, ListSizingBehavior, ListState, MouseButton, MouseDownEvent, Pixels, Rems,
    Render, Subscription, Task, TextStyleRefinement, WeakEntity, img, list, pulsating_between,
    relative,
};
use markdown::{HeadingLevelStyles, Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use serde_json::Value;
use settings::{DockSide, Settings as _};
use task::{RevealStrategy, SpawnInTerminal, TaskId};
use terminal_view::TerminalView;
use theme::Appearance;
use ui::{
    Button, ButtonStyle, Checkbox, Disclosure, Divider, ListItem, ListItemSpacing, ScrollAxes,
    Scrollbars, SelectableButton as _, TintColor, ToggleState, Tooltip, WithScrollbar as _,
    prelude::*,
};
use util::{ResultExt as _, truncate_and_trailoff};
use workspace::{
    Item, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use zed_actions::editor::{MoveDown, MoveUp};

use crate::{
    ClaudeSessionStore, ClaudeSessionsSettings, Interrupt, ModelRates, NextMessage, OpenInEditor,
    PreviousMessage, RegisteredSession, SendMessage, SubagentSummary, ToggleFocus,
    TranscriptRecord, TranscriptTarget, Usage, rates_for_model,
    session_registry::{Digit, PaneKey, Question, SlashCommand, SlashCommandScope},
    session_registry::{tmux_session_name, workflow_run_id_in_tool_result},
    session_source::{FileContents, LocalSource, RemoteSource, SessionInput, SessionSource},
    transcript::Spend,
};

const CLAUDE_SESSIONS_PANEL_KEY: &str = "ClaudeSessionsPanel";

const COMPACT_BOUNDARY_SUBTYPE: &str = "compact_boundary";
const LOCAL_COMMAND_SUBTYPE: &str = "local_command";

/// Timing the CLI writes for its own use: how long a turn took and how many messages
/// were in it, and nothing a reader of the conversation is looking for. It is the one
/// record on the conversation's chain with no content at all, so it is left out rather
/// than drawn as a block of unrecognized JSON.
const TURN_DURATION_SUBTYPE: &str = "turn_duration";
const ATTACHMENT_RECORD_TYPE: &str = "attachment";

/// The attachment Claude Code writes for a message typed while it was answering.
const QUEUED_COMMAND_ATTACHMENT: &str = "queued_command";

/// Said above a message the session is holding behind the turn it is running.
const QUEUED_NOTE: &str = "Queued";
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
const IMAGE_BLOCK_TYPE: &str = "image";

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

/// How much of the session's terminal is mirrored. Enough for a prompt and its options,
/// the queued messages behind a running turn, and the status line under them — the parts
/// of the screen that are not in the transcript.
const PANE_MIRROR_LINES: usize = 14;

/// Multiples of the text's own size, for the lines within a paragraph.
const CONVERSATION_LINE_HEIGHT: f32 = 1.5;

/// What Claude Code writes beside the permission mode it is in, at the foot of its
/// screen: `⏵⏵ auto mode on (shift+tab to cycle) · ← for agents`.
const PERMISSION_MODE_HINT: &str = "(shift+tab to cycle)";

/// Said in place of the mode's name when the screen does not carry one. The CLI writes
/// the line above only for the modes it considers worth announcing, so its absence is
/// the ordinary mode rather than a session whose mode is unknowable.
const UNNAMED_PERMISSION_MODE: &str = "Permission mode";

/// Longer than any mode Claude Code has named, and short enough that a line of the
/// conversation quoting the hint is not mistaken for the footer.
const MAX_PERMISSION_MODE_CHARACTERS: usize = 40;

/// The mode that turns the permission prompts off altogether. Named here only to draw
/// its button as the warning it is.
const BYPASSING_PERMISSIONS: &str = "bypass";

/// The permission mode the session is in, read off the foot of its own screen.
///
/// The name is taken out of the line rather than matched against a list of the modes,
/// because Claude Code renames them — what was `accept edits` is `auto mode` now — and a
/// list would quietly show nothing the first time one changed.
fn permission_mode(pane_contents: &str) -> Option<SharedString> {
    // Searched from the bottom: the hint sits in the CLI's footer, and anything above
    // that which happens to quote it is conversation.
    pane_contents.lines().rev().find_map(|line| {
        let before_hint = line.split(PERMISSION_MODE_HINT).next()?;
        if before_hint.len() == line.len() {
            return None;
        }
        let named = before_hint.trim();
        // `on` is the line's own grammar rather than part of the mode's name.
        let named = named.strip_suffix(" on").unwrap_or(named).trim_end();
        // The glyphs the CLI marks the mode with are decoration, and differ per mode.
        let mode = named
            .trim_start_matches(|character: char| !character.is_alphanumeric())
            .trim_end();
        (!mode.is_empty() && mode.chars().count() <= MAX_PERMISSION_MODE_CHARACTERS)
            .then(|| SharedString::from(mode.to_string()))
    })
}

/// What the toolbar says about the session, left to right: the model answering it, the
/// effort it is answering at, how much context its newest answer was given, and what it
/// has cost.
///
/// Each is left out when the transcript does not say it, rather than shown as a blank or
/// a zero — a session that has not answered yet knows none of them.
fn session_facts(spend: &Spend) -> Vec<SharedString> {
    let mut facts = Vec::new();
    if let Some(model) = spend.model.clone() {
        facts.push(model);
    }
    if let Some(effort) = spend.effort.clone() {
        facts.push(effort);
    }
    if spend.context_tokens > 0 {
        let context = compact_token_count(spend.context_tokens);
        // Marked when it came from a compaction rather than an answer: that figure counts
        // the kept conversation alone, so the next answer measures a larger context once
        // the system prompt, the tools and the skills are re-sent. Without the word, that
        // jump reads as the panel having been wrong.
        facts.push(SharedString::from(if spend.context_is_post_compaction {
            format!("{context} ctx (compacted)")
        } else {
            format!("{context} ctx")
        }));
    }
    // What the session has spent, as against what it is carrying: the context figure
    // above is the size of one request, and says nothing about how much work has gone
    // through the session to reach it.
    let read = spend
        .usage
        .input_tokens
        .saturating_add(spend.usage.cache_read_tokens)
        .saturating_add(spend.usage.cache_write_1h_tokens)
        .saturating_add(spend.usage.cache_write_5m_tokens);
    if read > 0 || spend.usage.output_tokens > 0 {
        facts.push(SharedString::from(format!(
            "{} in · {} out",
            compact_token_count(read),
            compact_token_count(spend.usage.output_tokens)
        )));
    }
    if let Some(cost) = session_cost(spend) {
        facts.push(cost);
    }
    facts
}

/// What one answer cost, and the tokens behind the figure.
///
/// Priced by the model the session is on: an answer's own record names the model, but the
/// usage read here has already been separated from it, and a session's model changes
/// rarely enough that the newest one is the right guess for all of them. Nothing is said
/// at all for a model with no known rates.
fn answer_cost(usage: Usage, store: &ClaudeSessionStore) -> Option<SharedString> {
    let spend = store.transcript().spend();
    let rates = rates_for_model(spend.model.as_deref()?)?;
    Some(SharedString::from(answer_summary(usage, rates)))
}

/// The line drawn under an answer when the reader has asked what it cost.
///
/// The three kinds of input are named separately because they are priced twenty-fold
/// apart — fresh input at the full rate, a cache write at twice it or a quarter more, a
/// cache read at a tenth of it — so one combined "in" figure says nothing about where an
/// answer's money went. Each is left out when it is zero, which keeps the line short on
/// the answers that only read from the cache.
fn answer_summary(usage: Usage, rates: ModelRates) -> String {
    let mut parts = vec![format_usd(usage.cost(rates))];

    if usage.input_tokens > 0 {
        parts.push(format!("{} in", compact_token_count(usage.input_tokens)));
    }
    if usage.cache_read_tokens > 0 {
        parts.push(format!(
            "{} cache read",
            compact_token_count(usage.cache_read_tokens)
        ));
    }
    let cache_write = usage
        .cache_write_1h_tokens
        .saturating_add(usage.cache_write_5m_tokens);
    if cache_write > 0 {
        parts.push(format!("{} cache write", compact_token_count(cache_write)));
    }
    parts.push(format!("{} out", compact_token_count(usage.output_tokens)));
    if usage.thinking_tokens > 0 {
        // Bracketed because it is part of the output above rather than additional to it.
        parts.push(format!(
            "({} thinking)",
            compact_token_count(usage.thinking_tokens)
        ));
    }

    parts.join(" · ")
}

/// What the session has cost.
///
/// Claude Code's own figure when it has written one, because it prices every model it
/// ran — including ones this code has no rates for. Otherwise the total derived from the
/// token counts, marked `~` to say it was derived here and from which rates. A model
/// with no known rates yields nothing rather than a wrong number.
fn session_cost(spend: &Spend) -> Option<SharedString> {
    if let Some(reported) = &spend.reported {
        return Some(SharedString::from(format_usd(reported.total_usd)));
    }

    let rates = rates_for_model(spend.model.as_deref()?)?;
    Some(SharedString::from(format!(
        "~{}",
        format_usd(spend.usage.cost(rates))
    )))
}

/// Cents for anything under ten dollars and whole dollars above it: at a hundred dollars
/// the cents are noise, and under a dollar they are the whole figure.
fn format_usd(amount: f64) -> String {
    if amount < 10. {
        format!("${amount:.2}")
    } else {
        format!("${amount:.0}")
    }
}

/// Token counts as a reader reads them: `535K`, `2.3M`.
fn compact_token_count(tokens: u64) -> String {
    match tokens {
        0..=9_999 => format!("{tokens}"),
        10_000..=999_999 => format!("{}K", tokens / 1_000),
        _ => format!("{:.1}M", tokens as f64 / 1_000_000.),
    }
}

/// A step through the commands menu, or none at all when the highlight is only being
/// read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuStep {
    Up,
    Down,
    Stay,
}

/// Where the highlight lands after `step`, over a menu of `offered` rows.
///
/// The highlight is kept as a plain index rather than as the command it names, because
/// the menu is rebuilt from what is typed on every keystroke — so it is clamped here
/// rather than trusted: a row that was highlighted before the name was typed out further
/// can be past the end of what is left.
fn step_through_menu(highlighted: usize, offered: usize, step: MenuStep) -> usize {
    let Some(last) = offered.checked_sub(1) else {
        return 0;
    };
    let highlighted = highlighted.min(last);

    // Held at both ends rather than wrapped: a menu that jumps from its last row to its
    // first reads as having lost the keypress.
    match step {
        MenuStep::Up => highlighted.saturating_sub(1),
        MenuStep::Down => highlighted.saturating_add(1).min(last),
        MenuStep::Stay => highlighted,
    }
}

/// What Enter does while the commands menu is open.
#[derive(Debug, PartialEq, Eq)]
enum EnterInMenu {
    /// Put this command in the box: what is typed does not name it in full yet.
    Complete(String),
    /// Send what is typed, which is already the whole command.
    Send,
}

fn enter_in_slash_menu(typed: &str, highlighted: &SlashCommand) -> EnterInMenu {
    // A command that takes arguments is not whole when its name is: what it is about has
    // still to be typed, so Enter fills the name in and leaves the reader there.
    let whole = highlighted.argument_hint.is_none()
        && typed.strip_prefix('/') == Some(highlighted.name.as_str());
    if whole {
        EnterInMenu::Send
    } else {
        EnterInMenu::Complete(highlighted.name.clone())
    }
}

/// Which way the arrow key moves the cursor when the box holds a draft rather than a
/// recalled message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorDirection {
    Up,
    Down,
}

/// What an arrow key does to the message box, decided before anything is touched.
#[derive(Debug, PartialEq)]
enum HistoryStep {
    /// Put this message in the box, standing at `index` in the history.
    Recall { index: usize, message: SharedString },
    /// Empty the box: the walk is back where it started.
    Draft,
    /// The key belongs to the cursor — the box holds a draft the reader is writing, and
    /// a message box that threw a half-written message away would be worse than no
    /// history at all.
    MoveCursor(CursorDirection),
    /// The walk has run out of history this way; the box keeps what it is holding.
    Stay,
}

/// The messages the reader has already sent, newest first.
///
/// Read back out of the conversation rather than remembered as they are sent: this panel
/// is opened on sessions it has sent nothing to, and the terminal's Up walks the whole
/// session either way.
fn message_history(entries: &[Entry]) -> Vec<SharedString> {
    let mut history: Vec<SharedString> = Vec::new();
    for entry in entries.iter().rev() {
        let EntryKind::Message {
            role: MessageRole::User,
            source,
            ..
        } = &entry.kind
        else {
            continue;
        };
        let text = source.trim();
        if text.is_empty() {
            continue;
        }
        // Consecutive repeats are one entry, as a shell's history has them: a message
        // sent twice is not two things to walk past.
        if history.last().is_some_and(|last| last.as_ref() == text) {
            continue;
        }
        history.push(SharedString::from(text.to_string()));
    }
    history
}

/// Where in the history the box is standing, or `None` when what it holds is the
/// reader's own draft.
///
/// The recorded position and the box's contents are checked against each other rather
/// than the position being trusted on its own: the moment the reader edits a recalled
/// message it is a draft again, and the arrows are the cursor's.
fn standing_in_history(at: Option<usize>, history: &[SharedString], typed: &str) -> Option<usize> {
    at.filter(|index| history.get(*index).is_some_and(|message| message == typed))
}

fn step_back_through_history(
    history: &[SharedString],
    at: Option<usize>,
    typed: &str,
) -> HistoryStep {
    let older = match standing_in_history(at, history, typed) {
        Some(index) => index + 1,
        // An empty box is where a walk starts, whether or not one was under way: the
        // reader cleared what they had.
        None if typed.is_empty() => 0,
        None => return HistoryStep::MoveCursor(CursorDirection::Up),
    };

    match history.get(older) {
        Some(message) => HistoryStep::Recall {
            index: older,
            message: message.clone(),
        },
        None => HistoryStep::Stay,
    }
}

fn step_forward_through_history(
    history: &[SharedString],
    at: Option<usize>,
    typed: &str,
) -> HistoryStep {
    let Some(index) = standing_in_history(at, history, typed) else {
        return HistoryStep::MoveCursor(CursorDirection::Down);
    };

    // Walked back past the newest message, which is where the walk started.
    let Some(newer) = index.checked_sub(1) else {
        return HistoryStep::Draft;
    };

    match history.get(newer) {
        Some(message) => HistoryStep::Recall {
            index: newer,
            message: message.clone(),
        },
        None => HistoryStep::Stay,
    }
}

/// Wall-clock milliseconds, as the registrations record them.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since_epoch| since_epoch.as_millis() as i64)
        .unwrap_or(0)
}

/// How long a session has sat without changing what it is doing.
///
/// `None` for anything recent: every session is quiet between turns, and a row that says
/// so for all of them says nothing. What it answers is the opposite question — which of
/// these was last touched days ago.
fn idle_for(updated_at: Option<i64>, now_millis: i64) -> Option<SharedString> {
    const MINUTE: i64 = 60 * 1000;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const TWO_DAYS: i64 = 2 * DAY;
    /// Below this a session is simply between turns.
    const WORTH_SAYING: i64 = 10 * MINUTE;

    let idle = now_millis.saturating_sub(updated_at?);
    if idle < WORTH_SAYING {
        return None;
    }

    // Hours are kept past the first day, because "33h" says something "1d" does not —
    // it would read the same as 47h. Past two days the hours stop being a unit anyone
    // converts in their head.
    Some(SharedString::from(match idle {
        ..HOUR => format!("idle {}m", idle / MINUTE),
        HOUR..TWO_DAYS => format!("idle {}h", idle / HOUR),
        _ => format!("idle {}d", idle / DAY),
    }))
}

/// The ground under the reader's own messages. A neutral grey rather than a wash of the
/// theme's foreground, which carries the theme's hue and reads as a faint tint rather
/// than as grey; and light enough to be a step away from the conversation's ground
/// without becoming a block of its own.
const USER_MESSAGE_GROUND: Hsla = Hsla {
    h: 0.,
    s: 0.,
    l: 0.5,
    a: 0.02,
};

/// The conversation's scrollbar is drawn in its own colour rather than the theme's, so
/// that how far up the history the reader is sitting is legible at a glance against the
/// transcript behind it.
const CONVERSATION_SCROLLBAR_COLOR: Hsla = Hsla {
    h: 28. / 360.,
    s: 0.9,
    l: 0.55,
    a: 1.,
};

/// Tall enough for a prompt and its options without taking the conversation's room. The
/// reader drags it from here.
const DEFAULT_TERMINAL_HEIGHT: Pixels = px(260.);

/// The grab area on the terminal's top edge.
const TERMINAL_RESIZE_HANDLE_HEIGHT: Pixels = px(4.);

const MIN_TERMINAL_HEIGHT: Pixels = px(80.);

const MAX_TERMINAL_HEIGHT: Pixels = px(1200.);

/// Dragged when the terminal's top edge is pulled. Nothing is carried across the screen:
/// the edge itself is what moves, so the preview draws nothing.
#[derive(Clone)]
struct DraggedTerminalDivider;

impl Render for DraggedTerminalDivider {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

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
    /// True for the copy opened as an editor tab, which is already where opening one
    /// would take the reader.
    in_pane: bool,
    /// Whether the session's terminal is showing. Closed by default: what it carried
    /// that the conversation did not — a question waiting to be answered, the words of a
    /// turn still running — is drawn as this panel's own now, so the terminal is the way
    /// back to the raw screen rather than the way to read the session.
    pane_expanded: bool,
    /// The terminal attached to the selected session's tmux pane, and the session it
    /// belongs to. Attaching is asynchronous and the reader can select another session
    /// while it runs, so what it was attached for is kept beside it rather than assumed.
    terminal: Option<Entity<TerminalView>>,
    terminal_process_id: Option<u32>,
    /// How tall the terminal section is. Dragged by its top edge, and kept here rather
    /// than measured, because the reader's choice has to survive every redraw.
    terminal_height: Pixels,
    /// Whether the message input is showing. Open by default: with the terminal closed
    /// this is where the session is answered, and a question that arrives has nowhere to
    /// draw itself if this is shut.
    input_expanded: bool,
    /// How far up the messages already sent the reader has walked with the Up key, as
    /// the terminal's own history walks. `None` while the box holds their own draft.
    history_index: Option<usize>,
    /// Whether the Quit button has been pressed once already. Quitting ends the session
    /// and nothing gives it back, so the first press arms the button rather than
    /// sending anything.
    quit_armed: bool,
    /// The answer being filled in for the question on screen, for a question that takes
    /// several. Kept here rather than read back from the terminal because nothing is sent
    /// until the whole answer is submitted: until then the terminal knows nothing about
    /// what has been ticked.
    ticked: Option<TickedAnswer>,
    /// The call the reader chose to answer in their own words rather than by picking, so
    /// that choosing it for one question does not leave the next one waiting for typing.
    typing_answer_for: Option<SharedString>,
    /// Which row of the commands menu the arrow keys are on. Kept as an index rather
    /// than as the command it names, because the menu is rebuilt from what is typed on
    /// every keystroke; see [`step_through_menu`].
    slash_highlight: usize,
    /// The text the commands menu was closed at, so that Escape can put it away without
    /// taking what is typed with it — and so that typing on brings it back.
    dismissed_slash_menu_for: Option<String>,
    /// The slash commands the selected session answers to, as the machine it runs on last
    /// listed them. Read once per session rather than per keystroke: the files behind
    /// them change far less often than the reader types.
    slash_commands: Vec<SlashCommand>,
    /// Held so that dropping it cancels a listing that is still running.
    _listing_slash_commands: Task<()>,
    /// Held so that dropping it cancels an answer that is still being keyed into the
    /// pane, and so that a second press cannot interleave with the first.
    _answering: Task<()>,
    /// What installing the hook did, kept on screen afterwards because it says where the
    /// settings it changed were copied to. Cleared when a question can be read, which is
    /// the install having taken effect.
    hook_install_note: Option<SharedString>,
    /// Held so that dropping it cancels an install that is still running.
    _installing_hook: Task<()>,
    /// Held so that dropping it cancels an attach that is still running.
    _terminal_attach: Task<()>,
    /// See [`UnreadBelow`].
    unread_below: UnreadBelow,
    /// Whether the reader was at the tail of the conversation the last time the list
    /// could say so.
    ///
    /// `ListState::is_scrolled_to_end` answers `None` while the items a splice added
    /// have not been measured, and every arrival splices. Reading that as "at the tail"
    /// scrolls a reader who is part-way up the history down to the tail on the next
    /// arrival — so the last definite answer is kept here and used instead.
    scrolled_to_end: bool,
    /// Keys of the entries — and of the attachments nested inside the context section —
    /// the user has opened.
    expanded: HashSet<SharedString>,
    /// Whether the conversation is read with [`crate::Transcript::full_path`], which
    /// includes everything compaction dropped from the model's context.
    show_full_history: bool,
    /// Whether what each answer cost is drawn beneath it.
    show_costs: bool,
    /// Whether the session's tool calls, their results, and its thinking are drawn.
    /// Closed by default: one turn can hold dozens of them, and a conversation read for
    /// what was said is buried under them.
    show_tool_calls: bool,
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
        /// What the answer this message is part of was billed. `None` for the reader's
        /// own messages, which are not billed on their own.
        usage: Option<Usage>,
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
    /// A message the CLI is holding behind the turn it is running, read from the queue
    /// log. Unlike [`Self::Pending`] it carries no id: the queue is the session's, so
    /// there is nothing here for the panel to take back.
    Queued {
        text: SharedString,
    },
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
            // Read apart from the assistant's at a glance: both were blues, and the rail
            // beside a message is the only thing that says whose it is.
            Self::User => Color::Error,
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
            Self::new_reading(
                workspace_handle,
                fs,
                store,
                source,
                project_root,
                window,
                cx,
            )
        })
    }

    /// Opens the session selected in the dock as a tab in the editor area, for a
    /// conversation that is easier to read at the width of a pane than at the width of a
    /// dock.
    pub fn open_in_pane(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(panel) = workspace.panel::<Self>(cx) else {
            return;
        };
        let (fs, source, project_root, process_id, target) = {
            let panel = panel.read(cx);
            let store = panel.store.read(cx);
            (
                panel.fs.clone(),
                panel.source.clone(),
                panel.project_root.clone(),
                store.selected(),
                // What the reader is looking at, not just which session it belongs to: a
                // tab opened from an agent's conversation is opened to read that agent.
                store.transcript_target().clone(),
            )
        };
        Self::reveal_session_in_pane(
            workspace,
            fs,
            source,
            project_root,
            process_id,
            target,
            window,
            cx,
        );
    }

    /// Brings the tab that reads `process_id` forward, opening one when the window has
    /// none for it. Called from the dock, where selecting a session would otherwise
    /// leave the reader with a selection and nowhere it is shown.
    ///
    /// The fields are taken from `self` rather than read back off the workspace, which
    /// would read this entity while it is the one being updated.
    fn reveal_in_pane(
        &mut self,
        process_id: Option<u32>,
        target: TranscriptTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let fs = self.fs.clone();
        let source = self.source.clone();
        let project_root = self.project_root.clone();
        workspace.update(cx, |workspace, cx| {
            Self::reveal_session_in_pane(
                workspace,
                fs,
                source,
                project_root,
                process_id,
                target,
                window,
                cx,
            );
        });
    }

    /// Opens one of the session's agent conversations as a tab of its own, so that a run
    /// of several can be watched beside the conversation that started it.
    fn open_agent_in_pane(
        &mut self,
        target: TranscriptTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let process_id = self.store.read(cx).selected();
        self.reveal_in_pane(process_id, target, window, cx);
    }

    /// One tab per session, identified by the process it is reading.
    ///
    /// A tab holds a store of its own, pinned to the session it was opened for, so that
    /// opening a second conversation leaves the first tab reading what it was reading
    /// instead of being retargeted under the reader. Asking for a session a tab already
    /// holds activates that tab rather than opening a second one: two tabs of the same
    /// conversation would only be two copies of the same scroll position.
    ///
    /// Matched on the process id rather than on the name in the tab, because two
    /// sessions run from the same directory carry the same name, and the point of a tab
    /// of its own is that it keeps showing the conversation it was opened for.
    fn reveal_session_in_pane(
        workspace: &mut Workspace,
        fs: Arc<dyn Fs>,
        source: Arc<dyn SessionSource>,
        project_root: Option<PathBuf>,
        process_id: Option<u32>,
        target: TranscriptTarget,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        // Bound before the call below so that the iterator's borrow of the workspace has
        // ended by the time the item is activated through it.
        //
        // Matched on the conversation as well as the session: two agents of one session
        // are two things to read, and a tab showing one of them is not the tab for the
        // other.
        let existing = workspace.items_of_type::<Self>(cx).find(|item| {
            let store = item.read(cx).store.read(cx);
            store.selected() == process_id && *store.transcript_target() == target
        });
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }

        let workspace_handle = workspace.weak_handle();
        let item = cx.new(|cx| {
            let store = cx.new(|cx| {
                let mut store = ClaudeSessionStore::new(source.clone(), project_root.clone(), cx);
                if let Some(process_id) = process_id {
                    store.select(process_id, cx);
                }
                // After selecting, which follows the session's own conversation.
                if target != TranscriptTarget::Main {
                    store.select_transcript_target(target, cx);
                }
                store
            });
            let mut this = Self::new_reading(
                workspace_handle,
                fs,
                store,
                source,
                project_root,
                window,
                cx,
            );
            this.in_pane = true;
            this
        });
        workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
    }

    fn new_reading(
        workspace: WeakEntity<Workspace>,
        fs: Arc<dyn Fs>,
        store: Entity<ClaudeSessionStore>,
        source: Arc<dyn SessionSource>,
        project_root: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store_subscription = cx.observe(&store, |this: &mut Self, _, cx| {
            this.rebuild_entries(cx);
            this.sync_input_availability(cx);
            if this.in_pane {
                cx.emit(ItemNameChanged);
            }
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

        let mut this = Self {
            workspace,
            focus_handle: cx.focus_handle(),
            fs,
            store,
            source,
            message_editor,
            project_root,
            in_pane: false,
            pane_expanded: false,
            terminal: None,
            terminal_process_id: None,
            terminal_height: DEFAULT_TERMINAL_HEIGHT,
            input_expanded: true,
            history_index: None,
            quit_armed: false,
            ticked: None,
            typing_answer_for: None,
            slash_highlight: 0,
            dismissed_slash_menu_for: None,
            slash_commands: Vec::new(),
            _listing_slash_commands: Task::ready(()),
            _answering: Task::ready(()),
            hook_install_note: None,
            _installing_hook: Task::ready(()),
            _terminal_attach: Task::ready(()),
            entries: Vec::new(),
            pending_sends: PendingSends::default(),
            activity: Activity::Idle,
            agent_calls: HashMap::default(),
            list_state: scroll_tracking_list_state(cx),
            session_list_expanded: true,
            unread_below: UnreadBelow::default(),
            scrolled_to_end: true,
            expanded: HashSet::default(),
            show_full_history: false,
            show_tool_calls: false,
            show_costs: false,
            markdowns: HashMap::default(),
            entry_cache: EntryCache::default(),
            cached_transcript_generation: 0,
            cached_home_directory: None,
            overwrites_dropped: 0,
            loaded_outputs: HashMap::default(),
            output_loads: HashMap::default(),
            _store_subscription: store_subscription,
        };
        // Read once here rather than per keystroke: the files behind these change far
        // less often than the reader types.
        this.load_slash_commands(cx);
        this
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
        self.scrolled_to_end = true;
        // Both are about the session being left: a place in its history, and a quit
        // aimed at it. Neither means anything against the one arrived at.
        self.history_index = None;
        self.quit_armed = false;
        self.ticked = None;
        self.typing_answer_for = None;
        self._answering = Task::ready(());
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

    /// Attaches a terminal to the selected session's pane, so that the CLI can be typed
    /// into and scrolled the way it would be in any other terminal.
    ///
    /// The transcript above stays the way the conversation is read — it is built for
    /// that, and a terminal only ever shows the last screenful. This is for the parts
    /// the transcript does not have: a prompt waiting to be answered, the messages
    /// queued behind the running turn, the status line.
    ///
    /// Nothing is attached until the terminal is on screen, and what is attached is
    /// dropped when the reader selects another session: a session's pane is one
    /// terminal, and attaching to one a reader has left would leave a client on it for
    /// as long as the panel lived.
    fn sync_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.store.read(cx).selected();
        let wanted = self
            .pane_expanded
            .then_some(selected)
            .flatten()
            .filter(|_| self.store.read(cx).pane_target().is_some());

        if wanted == self.terminal_process_id {
            return;
        }

        self.terminal_process_id = wanted;
        self.terminal = None;
        self._terminal_attach = Task::ready(());

        let Some(process_id) = wanted else {
            cx.notify();
            return;
        };
        let Some(arguments) = self.store.read(cx).attach_arguments() else {
            cx.notify();
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = workspace.read(cx).project().clone();

        // `command` is spawned as a program with `args`, never through a shell, so the
        // two are kept apart here: a whole command line in `command` is looked up as one
        // file name and never found. It also means every target reaches tmux as its own
        // argument, with no quoting to get right.
        let label = format!("tmux {}", arguments.join(" "));
        let spawn = SpawnInTerminal {
            id: TaskId(format!("claude-session-attach-{process_id}")),
            full_label: label.clone(),
            label: label.clone(),
            command: Some("tmux".to_string()),
            args: arguments,
            command_label: label,
            use_new_terminal: true,
            allow_concurrent_runs: true,
            reveal: RevealStrategy::NoFocus,
            env: std::collections::HashMap::default(),
            ..Default::default()
        };

        let terminal = project.update(cx, |project, cx| project.create_terminal_task(spawn, cx));
        self._terminal_attach = cx.spawn_in(window, async move |this, cx| {
            // A failure leaves the mirror standing in for the terminal, which is what
            // the reader sees until an attach succeeds.
            let Some(terminal) = terminal.await.log_err() else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                // The reader may have selected another session while this ran; what was
                // attached for that one is not shown under this one.
                if this.terminal_process_id != Some(process_id) {
                    return;
                }
                let workspace = this.workspace.clone();
                let project = project.downgrade();
                this.terminal =
                    Some(cx.new(|cx| {
                        TerminalView::new(terminal, workspace, None, project, window, cx)
                    }));
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    /// Collapsing takes the terminal down rather than hiding it: an attached client that
    /// nobody is looking at still holds the pane's size down to its own.
    fn toggle_pane_mirror(&mut self, cx: &mut Context<Self>) {
        self.pane_expanded = !self.pane_expanded;
        cx.notify();
    }

    /// Answers a prompt the CLI has drawn in the pane, with one of the keys it is
    /// waiting for. A prompt is answered in the terminal or not at all, so without this
    /// a session that stopped to ask something can only be unblocked by leaving Zed.
    fn send_pane_key(&mut self, key: PaneKey, cx: &mut Context<Self>) {
        if !self.can_send(cx) {
            return;
        }

        self.store.update(cx, |store, cx| {
            // There is no text to hand back for a keypress, and the store reports the
            // failure itself, so nothing here waits for the answer.
            store.send_input(SessionInput::Key(key), cx).detach()
        });
    }

    /// Quits the session's CLI, as Ctrl+D does in the terminal — but only on the second
    /// press.
    ///
    /// The first press arms the button and sends nothing. This ends the session, and a
    /// session ended by a stray click cannot be got back: what it was doing is over, and
    /// what it knew is only in its transcript.
    fn quit_session(&mut self, cx: &mut Context<Self>) {
        if !self.can_send(cx) {
            return;
        }

        if !self.quit_armed {
            self.quit_armed = true;
            cx.notify();
            return;
        }

        self.quit_armed = false;
        self.send_pane_key(PaneKey::Quit, cx);
    }

    /// Puts the previous message sent to this session in the box, as Up does in the
    /// terminal.
    fn recall_previous_message(
        &mut self,
        _: &PreviousMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.step_through_slash_menu(MenuStep::Up, cx) {
            return;
        }

        let history = message_history(&self.entries);
        let typed = self.message_editor.read(cx).text(cx);
        let step = step_back_through_history(&history, self.history_index, &typed);
        self.take_history_step(step, window, cx);
    }

    /// Walks back down towards the box's own draft, as Down does in the terminal.
    fn recall_next_message(
        &mut self,
        _: &NextMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.step_through_slash_menu(MenuStep::Down, cx) {
            return;
        }

        let history = message_history(&self.entries);
        let typed = self.message_editor.read(cx).text(cx);
        let step = step_forward_through_history(&history, self.history_index, &typed);
        self.take_history_step(step, window, cx);
    }

    /// Moves the highlight, and reports whether the menu was open to take the key.
    fn step_through_slash_menu(&mut self, step: MenuStep, cx: &mut Context<Self>) -> bool {
        let Some(offered) = self.offered_slash_commands(cx).map(|offered| offered.len()) else {
            return false;
        };
        self.slash_highlight = step_through_menu(self.slash_highlight, offered, step);
        cx.notify();
        true
    }

    /// The command the menu's highlight is on, clamped to what the menu is still
    /// offering.
    fn highlighted_slash_command(&self, cx: &Context<Self>) -> Option<SlashCommand> {
        let offered = self.offered_slash_commands(cx)?;
        let at = step_through_menu(self.slash_highlight, offered.len(), MenuStep::Stay);
        offered.get(at).map(|command| (*command).clone())
    }

    /// Puts the menu away without taking what is typed with it. Typing on brings it back.
    fn dismiss_slash_menu(&mut self, cx: &mut Context<Self>) -> bool {
        if self.offered_slash_commands(cx).is_none() {
            return false;
        }
        self.dismissed_slash_menu_for = Some(self.message_editor.read(cx).text(cx));
        self.slash_highlight = 0;
        cx.notify();
        true
    }

    fn take_history_step(
        &mut self,
        step: HistoryStep,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = match step {
            HistoryStep::Stay => return,
            HistoryStep::MoveCursor(direction) => {
                self.message_editor
                    .update(cx, |editor, cx| match direction {
                        CursorDirection::Up => editor.move_up(&MoveUp, window, cx),
                        CursorDirection::Down => editor.move_down(&MoveDown, window, cx),
                    });
                return;
            }
            HistoryStep::Draft => {
                self.history_index = None;
                String::new()
            }
            HistoryStep::Recall { index, message } => {
                self.history_index = Some(index);
                message.to_string()
            }
        };

        self.message_editor.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
            editor.move_to_end(&Default::default(), window, cx);
        });
        cx.notify();
    }

    /// Reads the slash commands the machine holds, once, in the background.
    ///
    /// The files behind them change far less often than the reader types, so this is not
    /// redone per keystroke — and a listing that fails leaves the menu with what it had,
    /// which for a first read is nothing and for a later one is still true enough to
    /// choose from.
    fn load_slash_commands(&mut self, cx: &mut Context<Self>) {
        let list = self.source.list_slash_commands(self.project_root.clone());
        self._listing_slash_commands = cx.spawn(async move |this, cx| {
            let Ok(commands) = list.await else {
                return;
            };
            this.update(cx, |this, cx| {
                this.slash_commands = commands;
                cx.notify();
            })
            .log_err();
        });
    }

    /// The commands the menu is offering, or `None` when what is typed names none.
    fn offered_slash_commands(&self, cx: &Context<Self>) -> Option<Vec<&SlashCommand>> {
        if !self.can_send(cx) {
            return None;
        }
        let typed = self.message_editor.read(cx).text(cx);
        if self.dismissed_slash_menu_for.as_deref() == Some(typed.as_str()) {
            return None;
        }
        let named = slash_command_being_named(&typed)?;

        let matches = matching_slash_commands(&self.slash_commands, named);
        // Nothing matching is not a menu: the reader is typing a command this machine
        // does not hold, and an empty box under the input says less than no box.
        (!matches.is_empty()).then_some(matches)
    }

    /// Puts a command into the message box, ready for whatever it takes after its name.
    ///
    /// Not sent: a command that takes arguments is only half typed at this point, and one
    /// that takes none is a keypress away from going.
    fn use_slash_command(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let takes_arguments = self
            .slash_commands
            .iter()
            .find(|command| command.name == name)
            .is_some_and(|command| command.argument_hint.is_some());
        let text = if takes_arguments {
            format!("/{name} ")
        } else {
            format!("/{name}")
        };

        self.message_editor.update(cx, |editor, cx| {
            editor.set_text(text.clone(), window, cx);
            editor.move_to_end(&Default::default(), window, cx);
        });
        // A command whose name is now whole would otherwise leave the menu open on the
        // one row it still matches, with Enter completing what is already complete.
        self.dismissed_slash_menu_for = Some(text);
        self.slash_highlight = 0;
        cx.notify();
    }

    /// Installs the hook that records waiting questions on the machine the sessions run
    /// on.
    ///
    /// What it did stays on screen afterwards: it changed a file the user owns, and where
    /// the copy of it went is the thing they would want to know. A session already
    /// running may have read its settings before this, so the note says so rather than
    /// leaving a reader wondering why nothing changed.
    fn install_question_hook(&mut self, cx: &mut Context<Self>) {
        let install = self.store.read(cx).install_question_hook();

        self.hook_install_note = Some(SharedString::from("Installing…"));
        self._installing_hook = cx.spawn(async move |this, cx| {
            let result = install.await;
            this.update(cx, |this, cx| {
                this.hook_install_note = Some(match result {
                    Ok(Some(backup)) => SharedString::from(format!(
                        "Installed. Your settings were copied to {}. Sessions already \
                         running pick it up when they next start.",
                        backup.display()
                    )),
                    Ok(None) => SharedString::from(
                        "Installed. Sessions already running pick it up when they next \
                         start.",
                    ),
                    Err(error) => SharedString::from(format!("Could not install: {error:#}")),
                });
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    /// The question the selected session is waiting on, and which of its questions is
    /// being asked.
    ///
    /// A question the hook recorded is only waiting until its call is answered: the hook
    /// records a question being asked and has no way to record it being answered, so a
    /// file left behind by an answered call is told apart by the conversation having the
    /// result that answers it.
    fn waiting_question(&self, cx: &Context<Self>) -> Option<(Question, usize, SharedString)> {
        let store = self.store.read(cx);
        let recorded = store.recorded_question()?;
        let answered = store
            .main_transcript()
            .active_path()
            .iter()
            .any(|record| tool_result_ids(record).contains(&recorded.tool_use_id.as_str()));
        if answered {
            return None;
        }

        let index = question_being_asked(
            store.pane_contents().map(SharedString::as_ref),
            &recorded.questions,
        );
        let question = recorded.questions.get(index)?.clone();
        Some((
            question,
            index,
            SharedString::from(recorded.tool_use_id.clone()),
        ))
    }

    /// Answers the question on screen by picking one option, which a numbered menu takes
    /// as the whole answer.
    fn pick_option(&mut self, option_index: usize, cx: &mut Context<Self>) {
        let Some(keys) = keys_for_single_choice(option_index) else {
            return;
        };
        self.send_answer(keys, cx);
    }

    /// Ticks or unticks one option of a question that takes several. Nothing is sent: the
    /// answer leaves when it is submitted.
    fn toggle_tick(&mut self, option_index: usize, cx: &mut Context<Self>) {
        let Some((_, question_index, tool_use_id)) = self.waiting_question(cx) else {
            return;
        };

        let ticked = match &mut self.ticked {
            Some(ticked)
                if ticked.tool_use_id == tool_use_id && ticked.question_index == question_index =>
            {
                ticked
            }
            // Ticks belong to the question they were made on, so a different question —
            // or a different call — starts an answer of its own rather than inheriting.
            _ => self.ticked.insert(TickedAnswer {
                tool_use_id,
                question_index,
                ticked: HashSet::default(),
            }),
        };

        if !ticked.ticked.remove(&option_index) {
            ticked.ticked.insert(option_index);
        }
        cx.notify();
    }

    fn submit_ticked(&mut self, cx: &mut Context<Self>) {
        let Some((question, question_index, tool_use_id)) = self.waiting_question(cx) else {
            return;
        };
        let option_count = question.options.len();

        let Some(ticked) = self.ticked.as_ref().filter(|ticked| {
            ticked.tool_use_id == tool_use_id && ticked.question_index == question_index
        }) else {
            return;
        };
        let Some(keys) = keys_for_several_choices(&ticked.ticked, option_count) else {
            return;
        };

        self.ticked = None;
        self.send_answer(keys, cx);
    }

    /// Takes the reader to typing their own answer, which the terminal offers as one more
    /// row below the options it was given.
    fn answer_in_own_words(&mut self, cx: &mut Context<Self>) {
        let Some((question, question_index, tool_use_id)) = self.waiting_question(cx) else {
            return;
        };
        let already_ticked = self
            .ticked
            .as_ref()
            .filter(|ticked| {
                ticked.tool_use_id == tool_use_id && ticked.question_index == question_index
            })
            .map(|ticked| ticked.ticked.clone())
            .unwrap_or_default();
        let Some(keys) = keys_for_own_words(
            &already_ticked,
            question.options.len(),
            question.multi_select,
        ) else {
            return;
        };

        // The ticks have left with the answer; a question drawn again after this starts
        // its own.
        self.ticked = None;

        self.typing_answer_for = Some(tool_use_id);
        self.input_expanded = true;
        self.send_answer(keys, cx);
    }

    /// Keys one answer into the pane, one key at a time.
    ///
    /// Sequenced rather than sent at once because the terminal reads each key against the
    /// menu it has drawn, and held in a field so that a second press replaces the first
    /// instead of interleaving with it.
    fn send_answer(&mut self, keys: Vec<PaneKey>, cx: &mut Context<Self>) {
        if !self.can_send(cx) {
            return;
        }

        let store = self.store.clone();
        self._answering = cx.spawn(async move |_, cx| {
            for key in keys {
                // The store has gone, or the send failed and the store has already
                // reported it. Either way the rest of the sequence would be answering a
                // menu that is no longer the one it was built for.
                let sent =
                    store.update(cx, |store, cx| store.send_input(SessionInput::Key(key), cx));
                if sent.await.is_err() {
                    return;
                }
                cx.background_executor().timer(ANSWER_KEY_INTERVAL).await;
            }
        });
        cx.notify();
    }

    fn toggle_input(&mut self, cx: &mut Context<Self>) {
        self.input_expanded = !self.input_expanded;
        cx.notify();
    }

    fn toggle_session_list(&mut self, cx: &mut Context<Self>) {
        self.session_list_expanded = !self.session_list_expanded;
        cx.notify();
    }

    /// Takes the reader to the newest of the conversation, which is what the count of
    /// what arrived below their window offers.
    fn scroll_to_latest(&mut self, cx: &mut Context<Self>) {
        self.list_state.scroll_to_end();
        self.scrolled_to_end = true;
        self.unread_below.reset();
        cx.notify();
    }

    /// Only the drawing changes, so the entries stand — but every entry that gains or
    /// loses a line of cost changes height, and the list holds the heights it measured.
    fn toggle_costs(&mut self, cx: &mut Context<Self>) {
        self.show_costs = !self.show_costs;
        let count = self.entries.len();
        self.list_state.remeasure_items(0..count);
        cx.notify();
    }

    /// The entries are rebuilt rather than filtered at draw time, because the list
    /// measures what it is given: hiding an item the list still holds would leave its
    /// height behind as a gap.
    fn toggle_tool_calls(&mut self, cx: &mut Context<Self>) {
        self.show_tool_calls = !self.show_tool_calls;
        self.entries.clear();
        self.list_state.reset(0);
        self.rebuild_entries(cx);
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

        // Dropped after the build rather than skipped during it, so that the cache still
        // holds every record's artifacts and showing them again costs no rederivation.
        // The chips above stay: which agents a turn started is read from the calls, not
        // from the entries, so hiding these does not hide those.
        if !self.show_tool_calls {
            new_entries.retain(|entry| {
                !matches!(
                    entry.kind,
                    EntryKind::ToolUse { .. }
                        | EntryKind::ToolResult { .. }
                        | EntryKind::Thinking { .. }
                )
            });
        }

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
            // Drawn after the pending sends and behind the same rule, because both are
            // messages that have not reached the conversation yet. A message sent from
            // Zed is in the session's queue as well, and is already drawn as a pending
            // send — the one the panel can take down when the send fails — so the queue
            // entry matching it is left to that.
            let store = self.store.read(cx);
            new_entries.extend(
                store
                    .transcript()
                    .queued_messages()
                    .iter()
                    .filter(|text| !self.pending_sends.holds_text(text))
                    // A background task reporting in is queued exactly as a message
                    // typed mid-turn is, and its text is nothing but the block the CLI
                    // wrote to tell itself. Held to the same rule as the conversation's
                    // own records — what is left once the injected blocks are cut — so a
                    // notification is dropped and typed words, which are never one of
                    // those blocks, are kept whole.
                    .filter_map(|text| user_visible_text(text))
                    .map(|text| Entry {
                        // Keyed on the text: the queue log gives these no id of their own,
                        // and the key has to be the same across rebuilds or the list
                        // remeasures an item that did not change.
                        key: SharedString::from(format!("queued-{text}")),
                        kind: EntryKind::Queued {
                            text: SharedString::from(text),
                        },
                    }),
            );
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

        // Captured before the splice, which itself moves the scroll position. A `None`
        // from the list is not an answer, so the last definite one stands.
        if let Some(at_end) = self.list_state.is_scrolled_to_end() {
            self.scrolled_to_end = at_end;
        }
        let was_scrolled_to_end = self.scrolled_to_end;

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
        // Offered only once there is a machine to install onto: with no session listed,
        // nothing has said which machine the button would be for.
        let offer_question_hook = !store.question_hook_installed() && !sessions.is_empty();
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
                        h_flex()
                            .gap_1()
                            .child(
                                Label::new(summary)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .single_line(),
                            )
                            .when(offer_question_hook, |this| {
                                this.child(
                                    Button::new(
                                        "claude-sessions-install-question-hook",
                                        "Enable questions",
                                    )
                                    .label_size(LabelSize::XSmall)
                                    .style(ButtonStyle::Tinted(TintColor::Accent))
                                    .tooltip(Tooltip::text(
                                        "A question a session is waiting on is written \
                                         nowhere until it has been answered. This adds a \
                                         hook that records them, so they can be answered \
                                         here. Your settings are copied aside first.",
                                    ))
                                    .on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.install_question_hook(cx)
                                        }),
                                    ),
                                )
                            })
                            .when(!self.in_pane, |this| {
                                this.child(
                                    IconButton::new(
                                        "claude-sessions-open-in-editor",
                                        IconName::ArrowUpRight,
                                    )
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Open in Editor"))
                                    .on_click(cx.listener(
                                        |_this, _, window, cx| {
                                            window.dispatch_action(Box::new(OpenInEditor), cx);
                                        },
                                    )),
                                )
                            }),
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
            .when_some(self.hook_install_note.clone(), |this, note| {
                this.child(
                    div()
                        .px_2()
                        .pb_1()
                        .child(Label::new(note).size(LabelSize::XSmall).color(Color::Muted)),
                )
            })
            .when(is_expanded, |this| {
                this.child(
                    v_flex()
                        .id("claude-sessions-list")
                        .flex_1()
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
        // What the session is carrying and what it has cost, read from the end of its own
        // transcript rather than from the one the panel is following — every row is a
        // different session, and only one of them is the one being read.
        let spend = self.store.read(cx).session_spend(process_id);
        let context = spend.filter(|spend| spend.context_tokens > 0).map(|spend| {
            SharedString::from(format!("{} ctx", compact_token_count(spend.context_tokens)))
        });
        let cost = spend
            .and_then(|spend| spend.total_cost_usd)
            .map(|total| SharedString::from(format_usd(total)));
        // How long the session has sat without changing what it is doing. Said because
        // every listed process is alive — `updatedAt` is not a heartbeat, so a session is
        // never filtered out for being quiet — and this is what tells one in use from one
        // left open two days ago.
        let idle = idle_for(session.updated_at, now_millis());
        // Bracketed and before the name, because a session is found by which tmux
        // session it is running in as often as by what Claude Code called it.
        let tmux_session = session
            .tmux_target
            .as_deref()
            .and_then(tmux_session_name)
            .map(|name| SharedString::from(format!("[{name}]")));
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
                            .children(tmux_session.map(|tmux_session| {
                                Label::new(tmux_session)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .single_line()
                            }))
                            .child(Label::new(name).size(LabelSize::Small).single_line())
                            .when(is_bridged, |this| {
                                this.child(
                                    Icon::new(IconName::Link)
                                        .size(IconSize::XSmall)
                                        .color(Color::Accent),
                                )
                            })
                            .children(context.map(|context| {
                                Label::new(context)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .single_line()
                            }))
                            .children(cost.map(|cost| {
                                Label::new(cost).size(LabelSize::XSmall).color(Color::Muted)
                            })),
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
                            .children(idle.map(|idle| {
                                Label::new(idle)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Hidden)
                                    .single_line()
                            })),
                    ),
            )
            .tooltip(Tooltip::text(format!("pid {process_id}")))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_session(process_id, cx);
                // Selecting a session in the dock is asking for that session's own
                // conversation, whichever one the dock happened to be reading.
                this.reveal_in_pane(Some(process_id), TranscriptTarget::Main, window, cx);
            }))
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
                phase: summary
                    .meta
                    .workflow_phase
                    .as_deref()
                    .map(str::trim)
                    .filter(|phase| !phase.is_empty())
                    .map(|phase| SharedString::from(phase.to_string())),
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
        // An agent that recorded no phase says nothing about one: a card reading
        // `Running · unknown` is less than a card reading `Running`.
        let state_note = match card.phase {
            Some(phase) => SharedString::from(format!("{state_note} · {phase}")),
            None => SharedString::from(state_note),
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
                h_flex()
                    .flex_none()
                    .gap_0p5()
                    .child(
                        Button::new(
                            SharedString::from(format!("claude-session-open-agent-{key}-{index}")),
                            "Open",
                        )
                        .end_icon(Icon::new(IconName::ArrowRight).size(IconSize::XSmall))
                        .label_size(LabelSize::XSmall)
                        .tooltip(Tooltip::text("Read this agent's conversation here"))
                        .on_click({
                            let target = target.clone();
                            cx.listener(move |this, _, _, cx| {
                                this.select_transcript_target(target.clone(), cx)
                            })
                        }),
                    )
                    // A run of several agents is worth watching beside the conversation
                    // that started it, rather than in place of it.
                    .when(!self.in_pane, |this| {
                        this.child(
                            IconButton::new(
                                SharedString::from(format!(
                                    "claude-session-open-agent-tab-{key}-{index}"
                                )),
                                IconName::ArrowUpRight,
                            )
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Read this agent in a tab of its own"))
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.open_agent_in_pane(target.clone(), window, cx)
                                },
                            )),
                        )
                    }),
            )
            .into_any_element()
    }

    /// What the session is saying right now, drawn under the conversation until the
    /// conversation itself has it.
    ///
    /// The CLI does not write an assistant message into its transcript until the turn
    /// carrying it is over, so on a long turn the panel is tens of seconds behind the
    /// terminal. This is the same words, read from what the terminal drew.
    ///
    /// Drawn below the conversation rather than spliced into it: the words change several
    /// times a second while the turn runs, and an entry in the list would remeasure the
    /// whole conversation each time.
    fn render_live_message(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let store = self.store.read(cx);
        // An agent's conversation is read from its own file and streams nowhere, so this
        // would be the session talking under an agent's transcript.
        if !matches!(store.transcript_target(), TranscriptTarget::Main) {
            return None;
        }

        let live = store.live_message()?;
        let live = live.trim();
        if live.is_empty() {
            return None;
        }

        // Once the turn is over the same words arrive in the conversation as a record of
        // their own, and drawing both would show the message twice. Only the newest few
        // entries are checked: the words arrive at the end or not at all.
        let already_in_the_conversation = self.entries.iter().rev().take(8).any(|entry| {
            matches!(
                &entry.kind,
                EntryKind::Message {
                    role: MessageRole::Assistant,
                    source,
                    ..
                } if source.trim() == live
            )
        });
        if already_in_the_conversation {
            return None;
        }

        Some(
            v_flex()
                .w_full()
                .px_2()
                .py_1()
                .gap_0p5()
                .border_t_1()
                .border_color(cx.theme().colors().border)
                .child(
                    Label::new("Saying now")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(Label::new(SharedString::from(live.to_string())).size(LabelSize::Small))
                .into_any_element(),
        )
    }

    fn render_transcript_section(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
        if let Some(at_end) = self.list_state.is_scrolled_to_end() {
            self.scrolled_to_end = at_end;
            if at_end {
                self.unread_below.scrolled(true);
            }
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
            .custom_scrollbars(
                Scrollbars::new(ScrollAxes::Vertical)
                    .tracked_scroll_handle(&self.list_state)
                    .thumb_color(CONVERSATION_SCROLLBAR_COLOR),
                window,
                cx,
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

        // Enter belongs to the commands menu while it is open, as it does in every other
        // completion menu — except when what is typed is already the whole command, for
        // which completing it would cost a second Enter every time.
        if let Some(highlighted) = self.highlighted_slash_command(cx) {
            let typed = self.message_editor.read(cx).text(cx);
            if let EnterInMenu::Complete(name) = enter_in_slash_menu(&typed, &highlighted) {
                self.use_slash_command(&name, window, cx);
                return;
            }
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
        // The walk up the history ended when what it found was sent, and the message
        // just sent is about to become its newest entry.
        self.history_index = None;
        self.quit_armed = false;
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

        // Escape puts the commands menu away first. Interrupting the session as well
        // would make closing a menu cost a turn.
        if self.dismiss_slash_menu(cx) {
            return;
        }

        // Stopping the session is the opposite of ending it: a quit aimed at it must
        // not stay armed behind this.
        self.quit_armed = false;
        self.store.update(cx, |store, cx| {
            // There is no text to hand back for an interrupt, and the store reports the
            // failure itself, so nothing here waits for the answer.
            store.send_input(SessionInput::Escape, cx).detach()
        });
    }

    /// The one control the conversation needs of its own: whether the session's tool
    /// calls and thinking are part of what is read.
    fn render_conversation_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let showing = self.show_tool_calls;

        let showing_costs = self.show_costs;
        let spend = self.store.read(cx).transcript().spend();
        // Read from the newest answer, so a session whose model or effort changed
        // part-way says what it is on now rather than what it started on.
        let facts = session_facts(&spend);
        let unpriced = spend
            .reported
            .as_ref()
            .is_some_and(|reported| reported.has_unknown_model_cost);

        h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_1()
            .justify_between()
            .child(
                h_flex()
                    .gap_1p5()
                    .overflow_hidden()
                    .children(facts.into_iter().map(|fact| {
                        Label::new(fact)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .single_line()
                    }))
                    // Said only because Claude Code says it: a total it could not price
                    // in full is worth less than the admission that it is short.
                    .when(unpriced, |this| {
                        this.child(
                            Label::new("unpriced models")
                                .size(LabelSize::XSmall)
                                .color(Color::Warning),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        IconButton::new("claude-session-costs", IconName::CurrencyDollar)
                            .icon_size(IconSize::Small)
                            .toggle_state(showing_costs)
                            .tooltip(Tooltip::text(if showing_costs {
                                "Hide what each answer cost"
                            } else {
                                "Show what each answer cost"
                            }))
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_costs(cx))),
                    )
                    .child(
                        IconButton::new(
                            "claude-session-tool-calls",
                            if showing {
                                IconName::Eye
                            } else {
                                IconName::EyeOff
                            },
                        )
                        .icon_size(IconSize::Small)
                        .toggle_state(showing)
                        .tooltip(Tooltip::text(if showing {
                            "Hide tool calls and thinking"
                        } else {
                            "Show tool calls and thinking"
                        }))
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_tool_calls(cx))),
                    ),
            )
    }

    /// Draws the session's terminal as it currently looks.
    ///
    /// Claude Code draws three things there that it never writes to the transcript: a
    /// prompt waiting to be answered, the messages queued behind the turn it is running,
    /// and its status line. None of them can be read from the conversation, so they are
    /// shown as the terminal itself has them rather than parsed into this panel's own
    /// elements — a parser would have to be right about the CLI's layout, and would go
    /// wrong the first time that layout changed.
    ///
    /// Only the tail is drawn. The rest of the screen is the conversation, which is above
    /// this in a form built for reading.
    /// What stands in for the terminal until it is attached, and after an attach that
    /// failed: the same screen, read through tmux rather than driven by it.
    fn render_pane_mirror(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let can_send = self.can_send(cx);
        let contents = self.store.read(cx).pane_contents()?;
        let tail: String = {
            let lines: Vec<&str> = contents.lines().collect();
            let from = lines.len().saturating_sub(PANE_MIRROR_LINES);
            lines.get(from..).unwrap_or_default().join("\n")
        };
        if tail.trim().is_empty() {
            return None;
        }

        let expanded = self.pane_expanded;

        Some(
            v_flex()
                .w_full()
                .border_t_1()
                .border_color(cx.theme().colors().border)
                .child(
                    h_flex()
                        .px_2()
                        .py_1()
                        .gap_1()
                        .child(
                            Disclosure::new("claude-session-pane-mirror", expanded).on_click(
                                cx.listener(|this, _, _, cx| this.toggle_pane_mirror(cx)),
                            ),
                        )
                        .child(
                            Label::new("Terminal")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(div().flex_1())
                        .children(
                            expanded
                                .then(|| {
                                    [
                                        (PaneKey::Up, IconName::ChevronUp, "Up"),
                                        (PaneKey::Down, IconName::ChevronDown, "Down"),
                                        (PaneKey::Enter, IconName::Return, "Enter"),
                                    ]
                                    .map(
                                        |(key, icon, name)| {
                                            IconButton::new(
                                                SharedString::from(format!(
                                                    "claude-session-pane-key-{name}"
                                                )),
                                                icon,
                                            )
                                            .icon_size(IconSize::XSmall)
                                            .disabled(!can_send)
                                            .tooltip(Tooltip::text(format!(
                                                "Send {name} to the terminal"
                                            )))
                                            .on_click(
                                                cx.listener(move |this, _, _, cx| {
                                                    this.send_pane_key(key, cx)
                                                }),
                                            )
                                        },
                                    )
                                })
                                .into_iter()
                                .flatten(),
                        ),
                )
                .when(expanded, |this| {
                    this.child(
                        div()
                            .id("claude-session-pane-mirror-contents")
                            .w_full()
                            .px_2()
                            .pb_2()
                            .overflow_x_scroll()
                            .child(
                                div()
                                    .font_buffer(cx)
                                    .text_size(TextSize::XSmall.rems(cx))
                                    .text_color(cx.theme().colors().text_muted)
                                    .whitespace_nowrap()
                                    .child(tail),
                            ),
                    )
                })
                .into_any_element(),
        )
    }

    /// The terminal, framed the way the mirror it stands in for is.
    ///
    /// The section owns the dragged height and the terminal fills what is left of it
    /// under the header, so that the height the reader set is the height they see rather
    /// than that plus a header.
    fn render_terminal(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let terminal = self.terminal.clone()?;
        let expanded = self.pane_expanded;

        Some(
            v_flex()
                .w_full()
                .when(expanded, |this| this.h(self.terminal_height))
                .border_t_1()
                .border_color(cx.theme().colors().border)
                // The bounds this reports are the section's, so the height the drag
                // wants is the distance from the pointer down to the section's bottom,
                // which does not move while the top edge is dragged.
                .on_drag_move(cx.listener(
                    |this, event: &DragMoveEvent<DraggedTerminalDivider>, _, cx| {
                        let height = event.bounds.bottom() - event.event.position.y;
                        this.terminal_height =
                            height.clamp(MIN_TERMINAL_HEIGHT, MAX_TERMINAL_HEIGHT);
                        cx.notify();
                    },
                ))
                .when(expanded, |this| {
                    this.child(
                        div()
                            .id("claude-session-terminal-resize")
                            .w_full()
                            .h(TERMINAL_RESIZE_HANDLE_HEIGHT)
                            .cursor_row_resize()
                            .on_drag(DraggedTerminalDivider, |_, _, _, cx| {
                                cx.stop_propagation();
                                cx.new(|_| DraggedTerminalDivider)
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|_, _: &MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                }),
                            )
                            .occlude(),
                    )
                })
                .child(
                    h_flex()
                        .px_2()
                        .py_1()
                        .gap_1()
                        .child(
                            Disclosure::new("claude-session-terminal", expanded).on_click(
                                cx.listener(|this, _, _, cx| this.toggle_pane_mirror(cx)),
                            ),
                        )
                        .child(
                            Label::new("Terminal")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
                .when(expanded, |this| {
                    this.child(
                        div()
                            .flex_1()
                            .w_full()
                            .overflow_hidden()
                            // A terminal that is not focused takes no keys. Nothing else
                            // focuses this one: in a pane that is the pane's job, and
                            // here there is no pane.
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                                    if let Some(terminal) = this.terminal.as_ref() {
                                        terminal.focus_handle(cx).focus(window, cx);
                                    }
                                }),
                            )
                            .child(terminal),
                    )
                })
                .into_any_element(),
        )
    }

    /// The commands what is typed could be naming, drawn as rows to pick from.
    ///
    /// Sits between the message box and the buttons under it, which is where the reader
    /// is looking while they type. Picking one fills the box rather than sending it: a
    /// command that takes arguments is only half typed when its name is.
    fn render_slash_commands(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let offered = self.offered_slash_commands(cx)?;
        let highlighted = step_through_menu(self.slash_highlight, offered.len(), MenuStep::Stay);
        // Taken by value here: the rows outlive the borrow of the commands, because each
        // row's handler carries the name it will put in the box.
        let rows: Vec<(SharedString, Option<SharedString>, SharedString, String)> = offered
            .into_iter()
            .map(|command| {
                let scope = match command.scope {
                    SlashCommandScope::Builtin => "built in",
                    SlashCommandScope::Project => "this project",
                    SlashCommandScope::User => "yours",
                };
                let shown = match command.argument_hint.as_deref() {
                    Some(hint) => format!("/{} {hint}", command.name),
                    None => format!("/{}", command.name),
                };
                (
                    SharedString::from(shown),
                    command.description.clone().map(SharedString::from),
                    SharedString::from(scope),
                    command.name.clone(),
                )
            })
            .collect();

        let mut menu = v_flex().w_full().px_2().pb_1().gap_0p5().child(
            h_flex()
                .w_full()
                .gap_1()
                .flex_wrap()
                .justify_between()
                .child(
                    Label::new("Commands")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                // Said because the menu is new: one nobody knows takes the arrow keys is
                // a menu picked with the mouse.
                .child(
                    Label::new("\u{2191}\u{2193} choose \u{b7} enter use \u{b7} esc close")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
        );

        for (index, (name, description, scope, command_name)) in rows.into_iter().enumerate() {
            let is_highlighted = index == highlighted;
            menu = menu.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .justify_between()
                    // On the row rather than only on the button, so that which row Enter
                    // would take is legible across the whole width of the menu.
                    .when(is_highlighted, |this| {
                        this.rounded_sm().bg(cx.theme().colors().element_selected)
                    })
                    .child(
                        Button::new(
                            SharedString::from(format!("claude-session-slash-{index}")),
                            name,
                        )
                        .toggle_state(is_highlighted)
                        .label_size(LabelSize::XSmall)
                        .tooltip(Tooltip::text(
                            description
                                .clone()
                                .unwrap_or_else(|| SharedString::from("Put this in the box")),
                        ))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.use_slash_command(&command_name, window, cx)
                            },
                        )),
                    )
                    .child(
                        Label::new(scope)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            );
        }

        Some(menu.into_any_element())
    }

    /// The question the session is waiting on, drawn as something to answer.
    ///
    /// Sits above the message box rather than replacing it: a question always offers
    /// typing an answer of your own as well, and a reader who wants to say something else
    /// entirely is not stopped from doing it.
    /// Sends the answers the terminal has listed back, or throws them away and starts
    /// the call's questions again.
    fn confirm_answers(&mut self, send: bool, cx: &mut Context<Self>) {
        let Some(keys) = keys_for_confirmation(send) else {
            return;
        };
        self.ticked = None;
        self.send_answer(keys, cx);
    }

    /// Whether the terminal is on the screen that asks whether to send the answers.
    ///
    /// A call that has already returned is not waiting on anything, so a screen left
    /// over from one is not drawn as a choice: what the panel offers has to be something
    /// pressing it would actually do.
    fn answers_are_being_confirmed(&self, cx: &Context<Self>) -> bool {
        let store = self.store.read(cx);
        let Some(recorded) = store.recorded_question() else {
            return false;
        };
        let answered = store
            .main_transcript()
            .active_path()
            .iter()
            .any(|record| tool_result_ids(record).contains(&recorded.tool_use_id.as_str()));
        if answered {
            return false;
        }

        store
            .pane_contents()
            .is_some_and(|pane| answers_awaiting_confirmation(pane))
    }

    /// The last step of a call that asked several questions, drawn as the two things it
    /// offers.
    fn render_answer_confirmation(&self, cx: &mut Context<Self>) -> AnyElement {
        let can_send = self.can_send(cx);

        v_flex()
            .w_full()
            .gap_1()
            .px_2()
            .pb_1()
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(IconName::Chat)
                            .size(IconSize::XSmall)
                            .color(Color::Accent),
                    )
                    .child(
                        Label::new("Every question answered")
                            .size(LabelSize::XSmall)
                            .color(Color::Accent),
                    ),
            )
            .child(Label::new(REVIEW_PROMPT).size(LabelSize::Small))
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .flex_wrap()
                    .child(
                        Button::new("claude-session-answers-send", REVIEW_SUBMIT_ROW)
                            .label_size(LabelSize::XSmall)
                            .disabled(!can_send)
                            .style(ButtonStyle::Tinted(TintColor::Accent))
                            .tooltip(Tooltip::text("Send these answers to the session"))
                            .on_click(cx.listener(|this, _, _, cx| this.confirm_answers(true, cx))),
                    )
                    .child(
                        Button::new("claude-session-answers-cancel", REVIEW_CANCEL_ROW)
                            .label_size(LabelSize::XSmall)
                            .disabled(!can_send)
                            .tooltip(Tooltip::text("Throw these answers away and ask again"))
                            .on_click(
                                cx.listener(|this, _, _, cx| this.confirm_answers(false, cx)),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_question(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        // Drawn instead of the questions, not beside them: at this point every question
        // has been answered and the last one drawn on its own would be the only thing on
        // screen that is not what the terminal is waiting for.
        if self.answers_are_being_confirmed(cx) {
            return Some(self.render_answer_confirmation(cx));
        }

        let (question, question_index, tool_use_id) = self.waiting_question(cx)?;
        let can_send = self.can_send(cx);
        let multi_select = question.multi_select;
        let option_count = question.options.len();
        let ticked: HashSet<usize> = self
            .ticked
            .as_ref()
            .filter(|ticked| {
                ticked.tool_use_id == tool_use_id && ticked.question_index == question_index
            })
            .map(|ticked| ticked.ticked.clone())
            .unwrap_or_default();

        let mut section = v_flex()
            .w_full()
            .gap_1()
            .px_2()
            .pb_1()
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(IconName::Chat)
                            .size(IconSize::XSmall)
                            .color(Color::Accent),
                    )
                    .child(
                        Label::new(if question.header.trim().is_empty() {
                            SharedString::from("Waiting on you")
                        } else {
                            SharedString::from(question.header.clone())
                        })
                        .size(LabelSize::XSmall)
                        .color(Color::Accent),
                    )
                    // Which of several questions this is. A call with one question says
                    // nothing, because "1 of 1" is not a thing a reader needs told.
                    .when_some(
                        self.store
                            .read(cx)
                            .recorded_question()
                            .map(|recorded| recorded.questions.len())
                            .filter(|count| *count > 1),
                        |this, count| {
                            this.child(
                                Label::new(format!("{} of {count}", question_index + 1))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                        },
                    ),
            )
            .child(
                Label::new(SharedString::from(question.question.clone())).size(LabelSize::Small),
            );

        for (option_index, option) in question.options.iter().enumerate() {
            let is_ticked = ticked.contains(&option_index);
            let label = SharedString::from(option.label.clone());
            // A question taking several answers is filled in and then submitted, so its
            // options are ticked; one taking a single answer is over the moment an option
            // is pressed, so its options are pressed.
            let control = if multi_select {
                Checkbox::new(
                    SharedString::from(format!("claude-session-option-{option_index}")),
                    ToggleState::from(is_ticked),
                )
                .label(label)
                .label_size(LabelSize::XSmall)
                .disabled(!can_send)
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_tick(option_index, cx)))
                .into_any_element()
            } else {
                Button::new(
                    SharedString::from(format!("claude-session-option-{option_index}")),
                    label,
                )
                .label_size(LabelSize::XSmall)
                .disabled(!can_send)
                .tooltip(Tooltip::text("Answer with this"))
                .on_click(cx.listener(move |this, _, _, cx| this.pick_option(option_index, cx)))
                .into_any_element()
            };

            section = section.child(
                v_flex()
                    .w_full()
                    .child(control)
                    // Under the option rather than beside it: a description is a sentence
                    // and the panel is narrow, and beside it the two wrap into each other.
                    .when_some(option.description.clone(), |this, description| {
                        this.child(
                            div().pl_4().child(
                                Label::new(SharedString::from(description))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                        )
                    }),
            );
        }

        section = section.child(
            h_flex()
                .w_full()
                .gap_1()
                // The dock is narrow and these two do not fit side by side in it at every
                // width; without this `Submit` is the one drawn off the edge.
                .flex_wrap()
                .child(
                    Button::new("claude-session-answer-typed", "Type something")
                        .label_size(LabelSize::XSmall)
                        .disabled(!can_send)
                        .tooltip(Tooltip::text("Answer in your own words, in the box below"))
                        .on_click(cx.listener(|this, _, _, cx| this.answer_in_own_words(cx))),
                )
                .when(multi_select, |this| {
                    this.child(
                        // The count is in the label rather than only in the tooltip: the
                        // button is disabled until something is ticked, and a grey button
                        // that says nothing reads as one that is not there.
                        Button::new(
                            "claude-session-answer-submit",
                            format!("Submit {}/{option_count}", ticked.len()),
                        )
                        .label_size(LabelSize::XSmall)
                        // Nothing ticked is not an answer, and submitting it would
                        // send a walk to `Submit` that answers nothing.
                        .disabled(!can_send || ticked.is_empty())
                        .style(ButtonStyle::Tinted(TintColor::Accent))
                        .tooltip(Tooltip::text(format!(
                            "Send {} of {option_count}",
                            ticked.len()
                        )))
                        .on_click(cx.listener(|this, _, _, cx| this.submit_ticked(cx))),
                    )
                }),
        );

        Some(section.into_any_element())
    }

    fn render_input(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let can_send = self.can_send(cx);
        // A session that is answering is the one case where interrupting is what the
        // reader wants, so the button says so and is tinted then rather than sitting
        // there as one more grey control.
        let is_answering = !matches!(self.activity, Activity::Idle);
        let store = self.store.read(cx);
        let has_selection = store.selected().is_some();
        let is_reading_an_agent =
            matches!(store.transcript_target(), TranscriptTarget::Subagent { .. });
        let note = input_note(can_send, is_reading_an_agent, has_selection);
        // The terminal's own controls, drawn here because the terminal is closed by
        // default now: without them the only way to change the permission mode, or to
        // take back what the CLI is holding, is to open the terminal and press the key.
        let mode = store
            .pane_contents()
            .and_then(|contents| permission_mode(contents));
        let bypassing = mode
            .as_ref()
            .is_some_and(|mode| mode.to_lowercase().contains(BYPASSING_PERMISSIONS));
        let mode_label = mode.unwrap_or_else(|| SharedString::from(UNNAMED_PERMISSION_MODE));
        let quit_armed = self.quit_armed;

        // A question stops the session until it is answered, so the block that answers it
        // opens itself: a reader who does not know one is waiting reads the session as
        // stuck, and the answer is behind a disclosure they have no reason to open.
        let question = self.render_question(cx);
        let slash_commands = self.render_slash_commands(cx);
        let expanded = self.input_expanded || question.is_some();

        v_flex()
            .w_full()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            // Outside the collapsed part, so that the keys still reach the session while
            // the input is closed: interrupting does not need somewhere to type.
            .key_context("ClaudeSessionsInput")
            .on_action(cx.listener(Self::send_message))
            .on_action(cx.listener(Self::interrupt_session))
            .on_action(cx.listener(Self::recall_previous_message))
            .on_action(cx.listener(Self::recall_next_message))
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .child(
                        Disclosure::new("claude-session-input", expanded)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_input(cx))),
                    )
                    .child(
                        Label::new("Message")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .when(expanded, |this| this.p_2().gap_1())
            .children(question)
            .when_some(note.filter(|_| expanded), |this, note| {
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
            .when(expanded, |this| {
                this.child(
                    div().w_full().px_2().child(
                        div()
                            .w_full()
                            .px_1()
                            .py_0p5()
                            .rounded_sm()
                            .border_1()
                            .border_color(cx.theme().colors().border)
                            .bg(cx.theme().colors().editor_background)
                            .child(self.message_editor.clone()),
                    ),
                )
            })
            .children(slash_commands.filter(|_| expanded))
            .when(expanded, |this| {
                this.child(
                    h_flex()
                        .w_full()
                        .px_2()
                        .pb_2()
                        .gap_1()
                        .justify_between()
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Button::new("claude-session-permission-mode", mode_label)
                                        .start_icon(
                                            Icon::new(IconName::ArrowCircle).size(IconSize::XSmall),
                                        )
                                        .label_size(LabelSize::XSmall)
                                        .disabled(!can_send)
                                        // Tinted only while the prompts are off: that is
                                        // the mode whose consequences a reader who forgot
                                        // they were in it would meet by surprise.
                                        .when(bypassing, |this| {
                                            this.style(ButtonStyle::Tinted(TintColor::Warning))
                                        })
                                        .tooltip(Tooltip::text(
                                            "Cycle this session's permission mode, as \
                                             shift+tab does in the terminal",
                                        ))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.send_pane_key(PaneKey::CyclePermissionMode, cx)
                                        })),
                                )
                                .child(
                                    IconButton::new("claude-session-cancel", IconName::Stop)
                                        .icon_size(IconSize::XSmall)
                                        .disabled(!can_send)
                                        .tooltip(Tooltip::text(
                                            "Send Ctrl+C: takes back what the terminal is \
                                             holding, and stops the turn it is running",
                                        ))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.send_pane_key(PaneKey::Cancel, cx)
                                        })),
                                )
                                .child(
                                    Button::new(
                                        "claude-session-quit",
                                        if quit_armed { "Quit?" } else { "Quit" },
                                    )
                                    .start_icon(Icon::new(IconName::Power).size(IconSize::XSmall))
                                    .label_size(LabelSize::XSmall)
                                    .disabled(!can_send)
                                    .when(quit_armed, |this| {
                                        this.style(ButtonStyle::Tinted(TintColor::Error))
                                    })
                                    .tooltip(Tooltip::text(if quit_armed {
                                        "Press again to send Ctrl+D, which ends this session"
                                    } else {
                                        "Send Ctrl+D, which ends this session"
                                    }))
                                    .on_click(cx.listener(|this, _, _, cx| this.quit_session(cx))),
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Button::new(
                                        "claude-session-interrupt",
                                        if is_answering { "Stop" } else { "Esc" },
                                    )
                                    .start_icon(Icon::new(IconName::Escape).size(IconSize::XSmall))
                                    .label_size(LabelSize::XSmall)
                                    .disabled(!can_send)
                                    .when(is_answering, |this| {
                                        this.style(ButtonStyle::Tinted(TintColor::Error))
                                    })
                                    .tooltip(Tooltip::for_action_title(
                                        if is_answering {
                                            "Stop this session"
                                        } else {
                                            "Interrupt this session"
                                        },
                                        &Interrupt,
                                    ))
                                    .on_click(cx.listener(
                                        |this, _, window, cx| {
                                            this.interrupt_session(&Interrupt, window, cx)
                                        },
                                    )),
                                )
                                .child(
                                    Button::new("claude-session-send", "Send")
                                        .start_icon(
                                            Icon::new(IconName::Send).size(IconSize::XSmall),
                                        )
                                        .label_size(LabelSize::XSmall)
                                        .disabled(!can_send)
                                        .tooltip(Tooltip::text("Send to this session"))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.send_message(&SendMessage, window, cx)
                                        })),
                                ),
                        ),
                )
            })
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
            EntryKind::Message {
                role,
                source,
                usage,
            } => {
                // The recap a compaction writes is the length of the conversation it
                // replaced, and it arrives at the top of what the reader is about to
                // read. Drawn closed, like a tool call, so that the session's own
                // messages are what the view opens on.
                if role == MessageRole::CompactSummary {
                    let header = self.render_disclosure_header(
                        DisclosureHeader {
                            entry_index: index,
                            key: &key,
                            icon: role.icon(),
                            icon_color: role.color(),
                            title: role.label().into(),
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
                    return v_flex()
                        .w_full()
                        .child(header)
                        .children(body)
                        .into_any_element();
                }

                let markdown = self.markdown_for(&key, source, cx);
                let cost = self
                    .show_costs
                    .then_some(usage)
                    .flatten()
                    .and_then(|usage| answer_cost(usage, self.store.read(cx)));
                v_flex()
                    .w_full()
                    .px_4()
                    .py_1()
                    // What the reader typed is given a ground of its own, so that their
                    // own words are told apart from the session's answers by more than
                    // the colour of a rail two characters wide. A wash of the foreground
                    // rather than a fixed grey: it lifts off a dark background and
                    // settles onto a light one, so either theme reads as a step.
                    .when(role == MessageRole::User, |this| {
                        this.bg(USER_MESSAGE_GROUND).rounded_sm()
                    })
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
                    .children(cost.map(|cost| {
                        Label::new(cost)
                            .size(LabelSize::XSmall)
                            .color(Color::Hidden)
                    }))
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
                // A run spawns agents as it goes, so the count is the run's shape so far
                // rather than what it will end up being. Only a run has one: a `Task`
                // call spawns the single agent its own card already accounts for.
                let run_note = self
                    .agent_calls
                    .get(&key)
                    .filter(|call| call.is_workflow && !cards.is_empty())
                    .map(|_| workflow_run_note(&cards));
                let mut element = v_flex().w_full().child(header).children(body);
                if let Some(run_note) = run_note {
                    element = element.child(
                        div().w_full().px_2().pb_1().child(
                            Label::new(run_note)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                    );
                }
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

            // The session is holding this one behind the turn it is running. Drawn like a
            // pending send, without the button: the queue belongs to the session, and
            // nothing here can take a message out of it.
            EntryKind::Queued { text } => v_flex()
                .w_full()
                .px_4()
                .py_1()
                .gap_0p5()
                .border_l_2()
                .border_color(
                    MessageRole::User
                        .color()
                        .color(cx)
                        .opacity(ROLE_RAIL_OPACITY),
                )
                .bg(USER_MESSAGE_GROUND)
                .rounded_sm()
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Icon::new(MessageRole::User.icon())
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(QUEUED_NOTE)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
                .child(
                    div()
                        .w_full()
                        .child(Label::new(text).size(LabelSize::Small).color(Color::Muted)),
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
    /// The two halves are never drawn together: the dock draws the list of sessions and
    /// an editor tab draws the conversation of the selected one, because a transcript is
    /// not readable at the width of a dock and the list is what the dock is for.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.in_pane {
            // Both halves of what decides it — the selection and whether the terminal is
            // showing — are known here, and this runs on every change to either.
            self.sync_terminal(window, cx);
        }

        v_flex()
            .key_context("ClaudeSessionsPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .map(|this| {
                if self.in_pane {
                    this.bg(conversation_background(cx))
                        .child(self.render_conversation_toolbar(cx))
                        .children(self.render_agent_chips(cx))
                        .child(self.render_transcript_section(window, cx))
                        .children(self.render_live_message(cx))
                        .children(self.render_activity())
                        .children(
                            self.render_terminal(cx)
                                .or_else(|| self.render_pane_mirror(cx)),
                        )
                        .child(self.render_input(cx))
                } else {
                    this.child(self.render_session_section(cx))
                }
            })
    }
}

impl Focusable for ClaudeSessionsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ClaudeSessionsPanel {}

/// Emitted when the tab's name is out of date, which is whenever the selected session
/// changes — the tab is named after the conversation it holds.
pub struct ItemNameChanged;

impl EventEmitter<ItemNameChanged> for ClaudeSessionsPanel {}

/// Lets the same view be opened as a tab in the editor area, where a conversation has
/// the width of a pane to be read at. The tab is the same element tree as the dock
/// panel, input included, rather than a second rendering of the transcript to keep in
/// step with this one.
impl Item for ClaudeSessionsPanel {
    type Event = ItemNameChanged;

    fn to_item_events(_event: &Self::Event, f: &mut dyn FnMut(workspace::item::ItemEvent)) {
        f(workspace::item::ItemEvent::UpdateTab);
    }

    /// The tab holds one conversation, so it is named after that session rather than
    /// after the panel. Selecting another session in the dock renames it, because both
    /// views read the same store.
    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        let store = self.store.read(cx);
        let selected = store.selected();
        store
            .sessions()
            .iter()
            .find(|session| selected == Some(session.process_id))
            .map(|session| {
                let name = session
                    .name
                    .clone()
                    .unwrap_or_else(|| session.session_id.clone());
                match session.tmux_target.as_deref().and_then(tmux_session_name) {
                    Some(tmux_session) => SharedString::from(format!("[{tmux_session}] {name}")),
                    None => SharedString::from(name),
                }
            })
            .unwrap_or_else(|| "Claude Sessions".into())
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::AiClaude))
    }
}

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
                DockPosition::Right => DockSide::Right,
                // `position_is_valid` refuses the bottom dock, so this is only reached by
                // a caller that ignored it; the left-hand side is the default.
                DockPosition::Left | DockPosition::Bottom => DockSide::Left,
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

    /// Whether one of these is the same message as `text`, which is how a queue entry
    /// read from the log is recognised as one of these rather than a second message.
    fn holds_text(&self, text: &str) -> bool {
        self.sends.iter().any(|send| send.text == text)
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
                ..
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
        let base_key = match record.uuid.as_ref() {
            Some(uuid) => SharedString::from(uuid.clone()),
            None => SharedString::from(format!("path-{path_index}")),
        };

        // A message typed while the session was answering is written as an attachment
        // and never as a user record, so it is the only record that message will ever
        // get. Left out here, the send that carried it is paired with nothing and its
        // `Sending…` message stays on screen for the rest of the session, beside the
        // conversation drawing the very same words.
        if record.record_type == ATTACHMENT_RECORD_TYPE {
            if let Some(prompt) = queued_command_prompt(record) {
                entries.push(Entry {
                    key: base_key,
                    kind: EntryKind::Message {
                        role: MessageRole::User,
                        source: prompt,
                        usage: None,
                    },
                });
            }
            continue;
        }

        // A record of another kind holds no message the user typed: an assistant record
        // is the model's, and the two subtypes below are drawn as something other than a
        // message.
        if record.record_type != USER_RECORD_TYPE
            || matches!(
                record.subtype.as_deref(),
                Some(COMPACT_BOUNDARY_SUBTYPE) | Some(LOCAL_COMMAND_SUBTYPE)
            )
        {
            continue;
        }

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

/// How many commands the menu offers at once. Enough to choose from without the menu
/// taking the room the conversation is in.
const SLASH_COMMAND_ROWS: usize = 8;

/// The part of what has been typed that is a command being named, or `None` when what is
/// typed is not naming one.
///
/// Only a message that is nothing but a command names one: `/` partway through a sentence
/// is a path, a date, or a fraction, and a menu that opened on those would be in the way
/// of ordinary typing. A command with its arguments already typed is no longer being
/// named either — the reader has moved on to what it takes.
fn slash_command_being_named(text: &str) -> Option<&str> {
    let text = text.strip_prefix('/')?;
    if text.starts_with('/') {
        return None;
    }
    (!text.contains(char::is_whitespace)).then_some(text)
}

/// The commands worth offering for what has been typed, best first.
///
/// A command whose name starts with what was typed is what the reader is reaching for; one
/// that merely contains it is offered behind those, because a name is usually typed from
/// its beginning. Beyond that the order the machine listed them in stands, which puts the
/// commands someone wrote before the built-in ones.
fn matching_slash_commands<'commands>(
    commands: &'commands [SlashCommand],
    typed: &str,
) -> Vec<&'commands SlashCommand> {
    let typed = typed.to_lowercase();
    let mut starting = Vec::new();
    let mut containing = Vec::new();

    for command in commands {
        let name = command.name.to_lowercase();
        if name.starts_with(&typed) {
            starting.push(command);
        } else if !typed.is_empty() && name.contains(&typed) {
            containing.push(command);
        }
    }

    starting.extend(containing);
    starting.truncate(SLASH_COMMAND_ROWS);
    starting
}

/// The conversation list, told to report where the reader is whenever they move.
///
/// The list cannot answer [`ListState::is_scrolled_to_end`] until every item it holds has
/// been measured, and a conversation that is still arriving always holds one that has
/// not — so asking it is answered with `None` for as long as the session keeps talking,
/// and a reader who scrolled up during that is never noticed to have done so. The scroll
/// event says which items are visible, which needs no measuring.
fn scroll_tracking_list_state(cx: &mut Context<ClaudeSessionsPanel>) -> ListState {
    let list_state = ListState::new(0, ListAlignment::Bottom, px(1024.));
    list_state.set_scroll_handler({
        let panel = cx.weak_entity();
        move |event, _window, cx| {
            let at_end = event.visible_range.end >= event.count;
            panel
                .update(cx, |panel, _| {
                    panel.scrolled_to_end = at_end;
                    // Arriving back at the bottom is having read what landed there.
                    if at_end {
                        panel.unread_below.scrolled(true);
                    }
                })
                .log_err();
        }
    });
    list_state
}

/// Which question of a call the terminal is asking now.
///
/// A call carrying several questions is asked one at a time, and the terminal names them
/// in a strip along the top with the answered ones ticked. Counting the ticks is the only
/// thing read out of that strip: the reader is being asked the first one that is not.
///
/// A call with one question draws no strip, and a terminal that cannot be read leaves the
/// reader on the first question, which is where a call starts.
/// Which of a call's questions the pane is showing.
///
/// The strip of marks along the top counts what has been answered, but it scrolls — the
/// arrows it is drawn between are the terminal saying so. On a narrow window only some
/// of the questions fit and the marks for the rest are not drawn at all, so counting
/// them reads the wrong question and the reader is shown the options of one they
/// answered several questions ago.
///
/// The question's own text is on screen whatever the window is doing, so it is what the
/// index is read from. The marks are the fallback for a pane that does not carry the
/// text — one scrolled past it, or a question drawn too narrow to hold it.
fn question_being_asked(pane: Option<&str>, questions: &[Question]) -> usize {
    let Some(pane) = pane else {
        return 0;
    };

    question_named_on_screen(pane, questions).unwrap_or_else(|| questions_marked_answered(pane))
}

/// The question whose text is drawn lowest on the screen.
///
/// Lowest rather than first: the pane holds the questions already answered above the one
/// being asked, and the newest of anything in a terminal is at the bottom.
fn question_named_on_screen(pane: &str, questions: &[Question]) -> Option<usize> {
    // Compared without any of the whitespace, because the terminal wraps a question to
    // the window's width — and wraps text with no spaces in it, which is most of the
    // questions this panel is read with, by breaking mid-sentence.
    let screen = without_whitespace(pane);

    questions
        .iter()
        .enumerate()
        .filter_map(|(index, question)| {
            let asked = without_whitespace(&question.question);
            if asked.is_empty() {
                return None;
            }
            let start = screen.rfind(&asked)?;
            let end = start.checked_add(asked.len())?;
            Some((end, asked.len(), index))
        })
        .max()
        .map(|(_, _, index)| index)
}

fn without_whitespace(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn questions_marked_answered(pane: &str) -> usize {
    pane.lines()
        .rev()
        .find(|line| line.contains(QUESTION_ANSWERED_MARK) || line.contains(QUESTION_PENDING_MARK))
        .map(|strip| strip.matches(QUESTION_ANSWERED_MARK).count())
        .unwrap_or(0)
}

/// The screen Claude Code draws once the last question of a call has been answered: the
/// answers listed back, and a choice between sending them and starting again.
///
/// The call is still waiting at this point — the tool has returned nothing — so a panel
/// that only knows about questions draws the last one over a screen it no longer
/// describes, and the one thing the terminal is waiting for has no button at all.
const REVIEW_PROMPT: &str = "Ready to submit your answers?";
const REVIEW_SUBMIT_ROW: &str = "Submit answers";
const REVIEW_CANCEL_ROW: &str = "Cancel";

/// Whether the pane is showing that screen.
///
/// Both the question and the rows it offers are required: the words alone turn up in
/// what a session says, and the prompt without its rows is a screen caught half-drawn.
/// `capture-pane` reads the visible screen rather than the scrollback, so what is found
/// here is what the terminal is showing now.
fn answers_awaiting_confirmation(pane: &str) -> bool {
    let Some(after_prompt) = pane.rfind(REVIEW_PROMPT).map(|at| &pane[at..]) else {
        return false;
    };
    after_prompt.contains(REVIEW_SUBMIT_ROW) && after_prompt.contains(REVIEW_CANCEL_ROW)
}

/// The keys that answer that screen. Its rows are numbered, and a numbered menu takes
/// the digit as the whole answer.
fn keys_for_confirmation(send: bool) -> Option<Vec<PaneKey>> {
    keys_for_single_choice(if send { 0 } else { 1 })
}

/// The marks the terminal puts beside each question in its strip: answered, and not.
const QUESTION_ANSWERED_MARK: &str = "☒";
const QUESTION_PENDING_MARK: &str = "☐";

/// The keys that answer a question by picking one option outright.
///
/// A numbered menu takes the digit as the whole answer, which is why a single-answer
/// question costs one keypress and needs to know nothing about where the cursor is.
fn keys_for_single_choice(option_index: usize) -> Option<Vec<PaneKey>> {
    Some(vec![PaneKey::Choice(Digit::for_option(option_index)?)])
}

/// The keys that take the reader to typing their own answer.
///
/// `Type something` is drawn as one more row after the options the model gave, and is
/// numbered along with them. In a menu taking one answer its digit is the whole answer
/// and the box opens at once; in a menu taking several the digit only ticks the row, so
/// the answer has to be submitted like any other, carrying whatever else was ticked.
fn keys_for_own_words(
    ticked: &HashSet<usize>,
    option_count: usize,
    several: bool,
) -> Option<Vec<PaneKey>> {
    if !several {
        return keys_for_single_choice(option_count);
    }

    let mut with_own_words = ticked.clone();
    with_own_words.insert(option_count);
    keys_for_several_choices(&with_own_words, option_count)
}

/// The rows Claude Code draws below the options of a question taking several answers:
/// `Type something`, `Submit`, and `Chat about this`.
const ROWS_BELOW_THE_OPTIONS: usize = 3;

/// The keys that answer a question taking several options.
///
/// Each tick is that option's digit, which toggles it. Unlike the menu for a question
/// taking one answer, the digit does **not** move the cursor, so where the cursor stands
/// when the ticks are done is not knowable from the ticks — counting the walk to
/// `Submit` from the last ticked option lands `Enter` on whichever row it reaches,
/// ticking an option the reader never chose or opening `Type something`.
///
/// So the walk does not count from anywhere. Down stops on the last row rather than
/// wrapping, so walking further than the menu is tall puts the cursor on `Chat about
/// this` from wherever it was, and `Submit` is the row above it.
fn keys_for_several_choices(ticked: &HashSet<usize>, option_count: usize) -> Option<Vec<PaneKey>> {
    if ticked.is_empty() {
        return None;
    }

    // Sorted only so that the keys are the same sequence for the same answer, which is
    // what makes them worth reading in a test or a log.
    let mut ticks: Vec<usize> = ticked.iter().copied().collect();
    ticks.sort_unstable();

    let mut keys = Vec::new();
    for option_index in ticks {
        keys.push(PaneKey::Choice(Digit::for_option(option_index)?));
    }

    for _ in 0..option_count + ROWS_BELOW_THE_OPTIONS {
        keys.push(PaneKey::Down);
    }
    keys.push(PaneKey::Up);
    keys.push(PaneKey::Enter);
    Some(keys)
}

/// The answer being filled in for one question that takes several options.
struct TickedAnswer {
    /// The call and the question within it these ticks are for. A question replaced by
    /// the next one of the same call must not inherit them, and neither must a question
    /// of another call entirely.
    tool_use_id: SharedString,
    question_index: usize,
    ticked: HashSet<usize>,
}

/// How long to leave between the keys of one answer.
///
/// The terminal redraws its menu between keypresses and reads the next key against what
/// it has drawn, so keys sent faster than it redraws are answered against the wrong
/// menu. This is the cost of answering through a terminal rather than an API.
const ANSWER_KEY_INTERVAL: Duration = Duration::from_millis(120);

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
    /// The phase of the run this agent belongs to, for an agent of a `Workflow` run that
    /// recorded one. A run of several phases is otherwise a row of cards that say nothing
    /// about which part of the run they are.
    phase: Option<SharedString>,
    target: TranscriptTarget,
}

/// What the line above a `Workflow` call's cards says the run is doing: how many agents
/// it has spawned so far, and how many of those are still working.
fn workflow_run_note(cards: &[AgentCardRow]) -> SharedString {
    let running = cards
        .iter()
        .filter(|card| card.state == AgentState::Running)
        .count();
    let agents = if cards.len() == 1 { "agent" } else { "agents" };
    // A run with nothing left running has either finished or is between phases, and
    // `0 running` reads as a claim about which one it is.
    if running == 0 {
        return SharedString::from(format!("{} {agents} · none running", cards.len()));
    }
    SharedString::from(format!("{} {agents} · {running} running", cards.len()))
}

/// Whether one of the selected session's agents is still working.
///
/// Never read off the agent's own transcript: it ends when the agent stops writing and
/// records nothing about having returned. What says an agent is over depends on how it
/// was spawned — a `Task` agent by the result of its call, a `Workflow` agent by the
/// journal its run writes, because that call is answered while the run is still going.
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

    // A `Workflow` run's agents record no tool use id, and the call that started the run
    // is answered the moment the run is launched rather than when it ends, so the
    // conversation holds nothing that says one of them is over. The run's own journal is
    // the only record of that, and the machine the run happened on has already read it.
    if summary.workflow_run_id.is_some() {
        return match summary.workflow_agent_finished {
            Some(true) => AgentState::Finished,
            // No journal to read, or one that does not name this agent yet. Both are
            // states a run passes through while it is working, so neither is it ending.
            Some(false) | None => AgentState::Running,
        };
    }

    // Neither id, so nothing in the conversation pairs with this agent at all. A chip
    // that pulses for the rest of the session is worse than one that never pulses.
    AgentState::Finished
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

/// The ground the conversation is read against: darker than the editor's own, which
/// separates a tab of messages from a tab of code and gives the tinted code blocks
/// inside it something to sit on.
///
/// Only a dark theme is darkened. The same shift applied to a light one would turn its
/// background grey without making anything easier to read.
fn conversation_background(cx: &App) -> Hsla {
    let editor_background = cx.theme().colors().editor_background;
    match cx.theme().appearance() {
        Appearance::Dark => editor_background.blend(gpui::black().opacity(0.4)),
        Appearance::Light => editor_background,
    }
}

/// Makes the conversation read as markdown rather than as flat text: headings carry
/// weight as well as size, code is tinted apart from prose, and a quote is marked by an
/// accent instead of by the same border colour as every other rule in the panel.
///
/// Refined here rather than in [`MarkdownStyle::themed`], which the agent panel shares.
/// The sizes stay below the markdown preview's: this is a conversation of many short
/// messages, and preview-sized headings would leave little room for the prose under them.
fn markdown_style(window: &Window, cx: &App) -> MarkdownStyle {
    let colors = cx.theme().colors();
    let mut style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);

    let heading = |font_size: Rems, font_weight: FontWeight| TextStyleRefinement {
        font_size: Some(font_size.into()),
        font_weight: Some(font_weight),
        line_height: Some(relative(1.3)),
        ..Default::default()
    };
    style.heading_level_styles = Some(HeadingLevelStyles {
        h1: Some(heading(rems(1.3), FontWeight::BOLD)),
        h2: Some(heading(rems(1.15), FontWeight::BOLD)),
        h3: Some(heading(rems(1.05), FontWeight::SEMIBOLD)),
        h4: Some(heading(rems(1.), FontWeight::SEMIBOLD)),
        h5: Some(heading(rems(0.95), FontWeight::SEMIBOLD)),
        h6: Some(heading(rems(0.875), FontWeight::SEMIBOLD)),
    });
    style.heading.margin.top = Some(Length::Definite(px(12.).into()));
    style.heading.margin.bottom = Some(Length::Definite(px(4.).into()));

    // Inline code is a span of text, so its background is all it can be given — neither
    // padding nor a corner radius reaches a `TextStyleRefinement`. The accent colour is
    // what separates it from the prose around it at a glance.
    style.inline_code.color = Some(colors.text_accent);

    let corner_radius = AbsoluteLength::Pixels(px(6.));
    style.code_block.corner_radii.top_left = Some(corner_radius);
    style.code_block.corner_radii.top_right = Some(corner_radius);
    style.code_block.corner_radii.bottom_left = Some(corner_radius);
    style.code_block.corner_radii.bottom_right = Some(corner_radius);

    // The themed line height is 1.75 times the *buffer* font size while this text is set
    // at the larger UI font size, which leaves the lines of a paragraph nearly touching.
    // Relative to the text's own size instead, so that changing either font size keeps
    // the same rhythm.
    style.base_text_style.line_height = relative(CONVERSATION_LINE_HEIGHT);
    style.paragraph_line_height = relative(CONVERSATION_LINE_HEIGHT);

    // A blank line between paragraphs has to read as a blank line. The themed spacing is
    // a flat 8px against lines that are around 24px tall, so two paragraphs ran together
    // as though the second were a wrapped continuation of the first. One line's height is
    // exactly what the author typed — an empty line — so that is what it is set to.
    let line =
        style.base_text_style.font_size.to_pixels(window.rem_size()) * CONVERSATION_LINE_HEIGHT;
    style.paragraph_spacing = line;

    style.block_quote.color = Some(colors.text_muted);
    style.block_quote_border_color = colors.text_accent.opacity(0.4);

    // Top-level list items run into each other without it.
    style.list_spacing = px(4.);

    style
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
    // Context Claude Code injected into the turn rather than anything either side said:
    // a skill's instructions, an expanded command, the output of a hook. These carry the
    // user's own role and can run to tens of thousands of lines, so drawn as messages
    // they bury the conversation they were injected into.
    if record
        .raw
        .get("isMeta")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return;
    }

    if record.record_type == ATTACHMENT_RECORD_TYPE {
        // A message typed while the session was answering is the reader's own, and
        // belongs in the conversation where they typed it rather than in the folded
        // context section beside the attachments Claude Code injected.
        //
        // It is recorded as an attachment because of when it arrived: the user record it
        // would have been is written when the turn it was queued behind finishes, and it
        // is written with no content at all — the text is only here. Drawn from the
        // attachment, and skipped as an empty record there, the message appears once.
        let prompt = queued_command_prompt(record);
        let images = queued_command_images(record);
        if prompt.is_some() || !images.is_empty() {
            let cache_key = cache_key(record, &base_key);
            if let Some(prompt) = prompt {
                let kind = cache.kind(cache_key.as_ref(), || EntryKind::Message {
                    role: MessageRole::User,
                    source: prompt,
                    usage: None,
                });
                entries.push(Entry {
                    key: base_key.clone(),
                    kind,
                });
            }
            for (index, block) in images.into_iter().enumerate() {
                let key = SharedString::from(format!("{base_key}#image-{index}"));
                // Keyed off the record's own cache key so that a decoded image survives
                // the conversation being rebuilt around it, as every other one does.
                let image_cache_key = cache_key
                    .as_ref()
                    .map(|cache_key| SharedString::from(format!("{cache_key}#image-{index}")));
                let kind = cache.kind(image_cache_key.as_ref(), || image_kind(block));
                entries.push(Entry { key, kind });
            }
            return;
        }

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
        Some(TURN_DURATION_SUBTYPE) => return,
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
        usage: Usage::from_record(&record.raw),
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

/// The text of a message that was queued behind a running turn, or `None` for every
/// other attachment.
fn queued_command_prompt(record: &TranscriptRecord) -> Option<SharedString> {
    let attachment = record.raw.get("attachment")?;
    if attachment.get("type").and_then(Value::as_str)? != QUEUED_COMMAND_ATTACHMENT {
        return None;
    }

    // A message with an image in it is recorded as content blocks rather than as a
    // string, and the blocks carry the image's base64 beside the text.
    let prompt = match attachment.get("prompt")? {
        Value::String(prompt) => prompt.trim().to_string(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string(),
        _ => return None,
    };

    // Same rule as the conversation's own records: a queued `<task-notification>` is the
    // CLI telling itself a background command finished, not a message anyone typed.
    user_visible_text(&prompt).map(SharedString::from)
}

/// The images pasted into a message that was queued behind a running turn.
///
/// They are only here. The user record the CLI writes when the turn finishes carries no
/// content at all, so an image dropped from the attachment is one the reader never sees
/// again — however plainly it is still on their terminal.
fn queued_command_images(record: &TranscriptRecord) -> Vec<&Value> {
    let Some(attachment) = record.raw.get("attachment") else {
        return Vec::new();
    };
    if attachment.get("type").and_then(Value::as_str) != Some(QUEUED_COMMAND_ATTACHMENT) {
        return Vec::new();
    }

    let Some(Value::Array(blocks)) = attachment.get("prompt") else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some(IMAGE_BLOCK_TYPE))
        .collect()
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

    /// The mode is read out of the CLI's own footer, which is the only place it is
    /// written down: nothing in the transcript says which mode a session is answering
    /// in.
    #[test]
    fn permission_mode_is_read_out_of_the_line_the_cli_writes_it_on() {
        let screen = |footer: &str| format!("some conversation\n\n\u{2500}\u{2500}\n{footer}");

        assert_eq!(
            permission_mode(&screen(
                "  \u{23f5}\u{23f5} auto mode on (shift+tab to cycle) \u{b7} \u{2190} for agents"
            ))
            .as_deref(),
            Some("auto mode"),
            "the glyphs and the trailing `on` are the line's, not the mode's name"
        );
        assert_eq!(
            permission_mode(&screen("  \u{23f8} plan mode on (shift+tab to cycle)")).as_deref(),
            Some("plan mode")
        );
        assert_eq!(
            permission_mode(&screen(
                "  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle)"
            ))
            .as_deref(),
            Some("bypass permissions"),
            "a mode whose own name ends in `s` must not lose it with the trailing `on`"
        );
    }

    #[test]
    fn test_permission_mode_trims_trailing_whitespace_before_on() {
        let screen = "some conversation\n\n\u{2500}\u{2500}\n  \u{23f5}\u{23f5} auto mode  on (shift+tab to cycle)";
        assert_eq!(
            permission_mode(screen).as_deref(),
            Some("auto mode"),
            "extra whitespace before `on` must not leave trailing spaces in the mode name"
        );
    }

    /// A screen with no such line is the ordinary mode, which the CLI announces by
    /// saying nothing. Naming it here would be inventing a name Claude Code does not
    /// use.
    #[test]
    fn permission_mode_is_unknown_when_the_screen_does_not_say_it() {
        assert_eq!(permission_mode("").as_deref(), None);
        assert_eq!(
            permission_mode("\u{276f} \n\n  ? for shortcuts").as_deref(),
            None
        );
    }

    /// The conversation is mirrored on the same screen the footer is, so a message that
    /// quotes the hint sits in the same text this reads. The footer is the last line to
    /// carry it, and anything implausibly long for a mode name is not one.
    #[test]
    fn permission_mode_is_taken_from_the_footer_rather_than_the_conversation_above_it() {
        let screen = concat!(
            "  I pressed shift+tab and it said `old mode on (shift+tab to cycle)`, which\n",
            "  is not what I expected at all when I was reading the manual for it\n",
            "\u{2500}\u{2500}\n",
            "  \u{23f5}\u{23f5} auto mode on (shift+tab to cycle)\n"
        );
        assert_eq!(
            permission_mode(screen).as_deref(),
            Some("auto mode"),
            "the footer is the bottom-most line carrying the hint"
        );

        let only_a_long_quote = concat!(
            "  the manual explains at some length that the line reads `whatever mode the ",
            "session happens to be in on (shift+tab to cycle)` when it is on\n"
        );
        assert_eq!(
            permission_mode(only_a_long_quote).as_deref(),
            None,
            "text far too long to be a mode name must not be shown as one"
        );
    }

    /// What the session has spent is not what it is carrying: the context figure is the
    /// size of one request, and says nothing about the work that has gone through the
    /// session to reach it.
    #[test]
    fn the_toolbar_says_the_tokens_the_session_has_spent() {
        let spend = Spend {
            usage: Usage {
                input_tokens: 12_000,
                cache_read_tokens: 480_000,
                cache_write_5m_tokens: 40_000,
                cache_write_1h_tokens: 8_000,
                output_tokens: 63_000,
                ..Default::default()
            },
            answers: 4,
            context_tokens: 94_000,
            ..Default::default()
        };

        assert!(
            session_facts(&spend).contains(&SharedString::from("540K in \u{b7} 63K out")),
            "every kind of input counts towards what was read, got {:?}",
            session_facts(&spend)
        );
    }

    /// The pane carries every strip the session has drawn, and the one being answered is
    /// the last of them. Reading the first instead answers against a question the reader
    /// settled turns ago, and draws the options of the wrong one.
    #[test]
    fn the_question_being_asked_is_the_last_strip_on_the_screen_not_the_first() {
        let pane = concat!(
            "\u{2190}  \u{2612} Architecture  \u{2612} Database  \u{2714} Submit  \u{2192}\n",
            "\n",
            "Which database should we use?\n",
            "Claude answered: Postgres.\n",
            "\n",
            "\u{2190}  \u{2612} Project shape  \u{2610} Extras  \u{2714} Submit  \u{2192}\n",
            "\n",
            "Which extras should be on?\n"
        );

        assert_eq!(
            question_being_asked(Some(pane), &[]),
            1,
            "the live strip has one question answered, and the settled one above it has two"
        );
    }

    /// A message typed while the session was answering carries its image the same way
    /// any other message does — the attachment's `prompt` is content blocks rather than
    /// a string when there is one. Reading only the text out of those blocks drops the
    /// image the reader pasted, and it is nowhere else: the user record written when the
    /// turn ends carries no content at all.
    #[test]
    fn a_message_queued_with_an_image_keeps_the_image() {
        let queued = serde_json::json!({
            "type": "attachment",
            "uuid": "q1",
            "attachment": {
                "type": "queued_command",
                "prompt": [
                    {"type": "text", "text": "look at this"},
                    {
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": "image/png",
                            "data": "aGVsbG8=",
                        },
                    },
                ],
            },
        })
        .to_string();

        let entries = entries_of(&[&queued]);

        assert_eq!(
            message_sources(&entries),
            vec![SharedString::from("look at this")],
            "the words typed with the image are still the message"
        );
        assert_eq!(
            decoded_image(&entries).bytes,
            b"hello".to_vec(),
            "the image pasted with them must be drawn too"
        );
    }

    #[test]
    fn a_message_queued_with_only_an_image_keeps_the_image() {
        let queued = serde_json::json!({
            "type": "attachment",
            "uuid": "q-image-only",
            "attachment": {
                "type": "queued_command",
                "prompt": [{
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": "aGVsbG8=",
                    },
                }],
            },
        })
        .to_string();

        let entries = entries_of(&[&queued]);
        let actual = entries
            .iter()
            .filter(|entry| matches!(&entry.kind, EntryKind::Image { .. }))
            .count();
        assert_eq!(
            actual, 1,
            "an image-only queued command must draw one image; expected 1, got {actual}"
        );
    }

    #[test]
    fn replacing_a_queued_image_for_one_uuid_invalidates_its_cached_decode() {
        let queued = |data: &str| {
            serde_json::json!({
                "type": "attachment",
                "uuid": "q-cache",
                "attachment": {
                    "type": "queued_command",
                    "prompt": [
                        {"type": "text", "text": "look"},
                        {
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": "image/png",
                                "data": data,
                            },
                        },
                    ],
                },
            })
            .to_string()
        };
        let first_record = record(&queued("aGVsbG8="));
        let replacement_record = record(&queued("d29ybGQ="));
        let mut cache = EntryCache::default();

        let first_entries = build_entries(&[&first_record], None, &mut cache);
        assert_eq!(decoded_image(&first_entries).bytes, b"hello".to_vec());

        cache.forget_record("q-cache");
        let replacement_entries = build_entries(&[&replacement_record], None, &mut cache);
        let actual = decoded_image(&replacement_entries).bytes.clone();
        assert_eq!(
            actual,
            b"world".to_vec(),
            "the replacement record carries a new image; expected bytes {:?}, got {actual:?}",
            b"world"
        );
    }

    /// After the last question of a call is answered, Claude Code does not return the
    /// answers — it draws every one of them back and asks whether to send them. Until
    /// that is answered the call is still waiting, and the panel drew the last question
    /// on: a reader looking at the panel had no button for the one thing the terminal
    /// was waiting for.
    #[test]
    fn the_screen_that_asks_whether_to_send_the_answers_is_recognised() {
        let review = concat!(
            "Review your answers\n",
            "\n",
            "  \u{25cf} which extras?\n",
            "    \u{2192} A, B\n",
            "\n",
            "Ready to submit your answers?\n",
            "\n",
            "\u{276f} 1. Submit answers\n",
            "  2. Cancel\n"
        );

        assert!(answers_awaiting_confirmation(review));
    }

    /// Every other screen a session can be on must not be read as that one — a question
    /// still being answered least of all, because drawing the confirmation over it would
    /// send the call before the reader had chosen anything.
    #[test]
    fn no_other_screen_is_read_as_the_one_that_sends_the_answers() {
        for screen in [
            "",
            "\u{276f} 1. [ ] A\n  2. [ ] B\n     Next\n",
            // The words on their own, in something the session was saying.
            "I will ask whether you are ready to submit your answers once I have them\n",
            // The prompt without the rows: a screen part-way through being redrawn.
            "Ready to submit your answers?\n",
        ] {
            assert!(
                !answers_awaiting_confirmation(screen),
                "{screen:?} was read as the confirmation screen"
            );
        }
    }

    fn command_named(name: &str, argument_hint: Option<&str>) -> SlashCommand {
        SlashCommand {
            name: name.to_string(),
            description: None,
            argument_hint: argument_hint.map(str::to_string),
            scope: SlashCommandScope::Builtin,
        }
    }

    /// Up and Down belong to the menu while it is open. They walk the messages already
    /// sent when it is not — and a reader picking a command with the arrow keys, which
    /// is how every other menu is picked, would otherwise find their half-typed command
    /// replaced by something they sent an hour ago.
    #[test]
    fn the_arrow_keys_walk_the_commands_menu_while_it_is_open() {
        let offered = 3;

        assert_eq!(step_through_menu(0, offered, MenuStep::Down), 1);
        assert_eq!(step_through_menu(1, offered, MenuStep::Up), 0);
    }

    /// Held rather than wrapped at both ends: a menu that jumps from the last row to the
    /// first reads as having lost the keypress.
    #[test]
    fn walking_past_either_end_of_the_menu_stays_where_it_is() {
        assert_eq!(step_through_menu(0, 3, MenuStep::Up), 0);
        assert_eq!(step_through_menu(2, 3, MenuStep::Down), 2);
    }

    /// The menu shrinks as the name is typed out, and the row that was highlighted can
    /// be past the end of what is left. Reading it as-is would highlight nothing and
    /// Enter would complete nothing.
    #[test]
    fn a_highlight_past_the_end_of_the_menu_falls_on_its_last_row() {
        assert_eq!(step_through_menu(7, 2, MenuStep::Stay), 1);
        assert_eq!(step_through_menu(7, 2, MenuStep::Up), 0);
        assert_eq!(step_through_menu(7, 1, MenuStep::Down), 0);
    }

    /// Enter completes what is half-typed, and sends what is whole. Completing an
    /// already-whole command would cost a second Enter for every command that takes no
    /// arguments, and sending a half-typed one would send a command that does not exist.
    #[test]
    fn enter_completes_a_half_typed_command_and_sends_a_whole_one() {
        let compact = command_named("compact", None);

        assert_eq!(
            enter_in_slash_menu("/comp", &compact),
            EnterInMenu::Complete("compact".to_string())
        );
        assert_eq!(enter_in_slash_menu("/compact", &compact), EnterInMenu::Send);
    }

    /// A command that takes arguments is not whole when its name is: the reader has
    /// still to type what it is about, so Enter fills the name in and leaves them there.
    #[test]
    fn enter_on_a_command_that_takes_arguments_completes_its_name() {
        let goal = command_named("goal", Some("<what to aim for>"));

        assert_eq!(
            enter_in_slash_menu("/goal", &goal),
            EnterInMenu::Complete("goal".to_string()),
            "the name alone is not the whole command when it takes something after it"
        );
    }

    fn question_named(header: &str, asked: &str) -> Question {
        Question {
            header: header.to_string(),
            question: asked.to_string(),
            options: Vec::new(),
            multi_select: false,
        }
    }

    /// The strip of marks scrolls: on a narrow window the terminal draws the arrows and
    /// only as many questions as fit, so the marks say the first question is being asked
    /// while the third one is on screen. Counting them showed the reader the options of a
    /// question they had answered two questions ago — and widening the window "fixed" it,
    /// which is how the strip gave itself away.
    #[test]
    fn the_question_being_asked_is_read_from_its_text_not_from_the_strip_that_scrolls() {
        let questions = [
            question_named(
                "\u{8a9e}\u{7cfb}",
                "\u{8981}\u{51fa}\u{54ea}\u{4e9b}\u{8a9e}\u{7cfb}？",
            ),
            question_named(
                "\u{7247}\u{578b}",
                "\u{7247}\u{578b}\u{600e}\u{9ebc}\u{6392}？",
            ),
            question_named(
                "\u{7d20}\u{6750}",
                "\u{7d20}\u{6750}\u{4f86}\u{6e90}\u{600e}\u{9ebc}\u{53d6}？",
            ),
        ];

        let narrow = concat!(
            "\u{2190}  \u{2612} \u{8a9e}\u{7cfb}  \u{2192}\n",
            "\n",
            "\u{7d20}\u{6750}\u{4f86}\u{6e90}\u{600e}\u{9ebc}\u{53d6}？\n",
            "\n",
            "  1. [ ] \u{6cbf}\u{7528}\u{73fe}\u{6709}\u{7d20}\u{6750}\u{5eab}\n"
        );

        assert_eq!(
            question_being_asked(Some(narrow), &questions),
            2,
            "the third question's text is on screen, whatever the strip had room to draw"
        );
    }

    /// The terminal wraps a question to the width it has, and breaks mid-sentence when
    /// the text has no spaces to break at. A match that needs the line intact reads a
    /// wrapped question as absent and falls back to the marks that are wrong.
    #[test]
    fn a_question_wrapped_across_lines_is_still_the_one_being_asked() {
        let questions = [
            question_named("a", "first question"),
            question_named(
                "b",
                "\u{7d20}\u{6750}\u{4f86}\u{6e90}\u{600e}\u{9ebc}\u{53d6}\u{5f97}\u{6bd4}\u{8f03}\u{5feb}",
            ),
        ];
        let wrapped = concat!(
            "\u{2190}  \u{2612} a  \u{2192}\n",
            "\u{7d20}\u{6750}\u{4f86}\u{6e90}\u{600e}\n",
            "\u{9ebc}\u{53d6}\u{5f97}\u{6bd4}\u{8f03}\u{5feb}\n"
        );

        assert_eq!(question_being_asked(Some(wrapped), &questions), 1);
    }

    /// The questions already answered are above the one being asked, so the text of an
    /// earlier one is on the same screen. The lowest is the live one.
    #[test]
    fn an_earlier_questions_text_in_the_scrollback_is_not_the_one_being_asked() {
        let questions = [
            question_named("a", "which database?"),
            question_named("b", "which extras?"),
        ];
        let pane = concat!(
            "which database?\n",
            "Claude answered: Postgres.\n",
            "\u{2190}  \u{2612} a  \u{2610} b  \u{2192}\n",
            "which extras?\n"
        );

        assert_eq!(question_being_asked(Some(pane), &questions), 1);
    }

    #[test]
    fn a_question_that_is_a_suffix_of_another_does_not_steal_its_match() {
        let questions = [
            question_named("whole", "Which database?"),
            question_named("suffix", "database?"),
        ];

        let actual = question_being_asked(Some("Which database?\n"), &questions);
        assert_eq!(
            actual, 0,
            "the whole question is the text on screen; expected index 0, got index {actual}"
        );
    }

    /// A pane carrying none of the questions' text falls back to the marks, which is
    /// still better than answering the first question by default.
    #[test]
    fn a_pane_that_names_no_question_falls_back_to_the_marks() {
        let questions = [question_named("a", "which database?")];
        let pane = "\u{2190}  \u{2612} a  \u{2610} b  \u{2714} Submit  \u{2192}\nsomething else entirely\n";

        assert_eq!(question_being_asked(Some(pane), &questions), 1);
    }

    /// The mode's name is what is left after the line's own grammar is taken off it, and
    /// the CLI pads that line to the width of the screen.
    #[test]
    fn the_permission_mode_carries_none_of_the_lines_padding() {
        for line in [
            "  \u{23f5}\u{23f5} auto mode  on (shift+tab to cycle)",
            "  \u{23f5}\u{23f5} auto mode   (shift+tab to cycle)",
            "\u{23f5}\u{23f5}  auto mode on  (shift+tab to cycle)",
        ] {
            assert_eq!(
                permission_mode(line).as_deref(),
                Some("auto mode"),
                "{line:?} was read as {:?}",
                permission_mode(line)
            );
        }
    }

    /// A session that has not answered yet has spent nothing, and a zero says less than
    /// nothing at all.
    #[test]
    fn the_toolbar_says_no_tokens_for_a_session_that_has_not_answered() {
        let spend = Spend::default();
        assert!(
            !session_facts(&spend)
                .iter()
                .any(|fact| fact.contains("in \u{b7}")),
            "got {:?}",
            session_facts(&spend)
        );
    }

    fn history_of(messages: &[&str]) -> Vec<SharedString> {
        messages
            .iter()
            .map(|message| SharedString::from(message.to_string()))
            .collect()
    }

    /// The history is the reader's own messages in the conversation, newest first, which
    /// is the order Up walks them in.
    #[test]
    fn the_history_is_the_readers_own_messages_newest_first() {
        let entries = entries_of(&[
            &user_message_line("u1", "first"),
            &serde_json::json!({
                "type": "assistant",
                "uuid": "a1",
                "message": { "content": [{"type": "text", "text": "an answer"}] },
            })
            .to_string(),
            &user_message_line("u2", "second"),
            &user_message_line("u3", "second"),
            &user_message_line("u4", "third"),
        ]);

        assert_eq!(
            message_history(&entries),
            history_of(&["third", "second", "first"]),
            "the session's own answers are not the reader's history, and a message \
             repeated back to back is one entry"
        );
    }

    /// The walk starts at the newest message and runs out at the oldest, rather than
    /// wrapping round to the newest again: a reader holding Up would never know they
    /// had reached the end.
    #[test]
    fn walking_back_stops_at_the_oldest_message() {
        let history = history_of(&["newest", "middle", "oldest"]);

        assert_eq!(
            step_back_through_history(&history, None, ""),
            HistoryStep::Recall {
                index: 0,
                message: "newest".into()
            }
        );
        assert_eq!(
            step_back_through_history(&history, Some(0), "newest"),
            HistoryStep::Recall {
                index: 1,
                message: "middle".into()
            }
        );
        assert_eq!(
            step_back_through_history(&history, Some(2), "oldest"),
            HistoryStep::Stay,
            "there is nothing older, and the box must keep what it is holding"
        );
        assert_eq!(
            step_back_through_history(&[], None, ""),
            HistoryStep::Stay,
            "a session with nothing sent to it has nothing to recall"
        );
    }

    /// Walking forward ends at the empty box the walk started from, not at the newest
    /// message: otherwise the reader can never get back to typing something new.
    #[test]
    fn walking_forward_ends_at_the_empty_box_the_walk_started_from() {
        let history = history_of(&["newest", "middle", "oldest"]);

        assert_eq!(
            step_forward_through_history(&history, Some(2), "oldest"),
            HistoryStep::Recall {
                index: 1,
                message: "middle".into()
            }
        );
        assert_eq!(
            step_forward_through_history(&history, Some(0), "newest"),
            HistoryStep::Draft
        );
    }

    /// The arrows belong to the cursor while the box holds something the reader wrote.
    /// This is the property the whole design turns on: a half-written message must
    /// never be thrown away by pressing Up.
    #[test]
    fn a_draft_in_the_box_keeps_the_arrow_keys_for_the_cursor() {
        let history = history_of(&["newest", "middle"]);

        assert_eq!(
            step_back_through_history(&history, None, "half a thought"),
            HistoryStep::MoveCursor(CursorDirection::Up)
        );
        assert_eq!(
            step_forward_through_history(&history, None, "half a thought"),
            HistoryStep::MoveCursor(CursorDirection::Down)
        );
        assert_eq!(
            step_back_through_history(&history, Some(0), "newest, and then some"),
            HistoryStep::MoveCursor(CursorDirection::Up),
            "a recalled message the reader has edited is a draft of theirs"
        );
        assert_eq!(
            step_forward_through_history(&history, Some(0), "newest, and then some"),
            HistoryStep::MoveCursor(CursorDirection::Down)
        );
    }

    /// A position recorded against a conversation that has since grown, or that the
    /// reader has left, must not be read as standing somewhere in the new one.
    #[test]
    fn a_position_that_no_longer_matches_the_box_is_not_walked_from() {
        let history = history_of(&["newest", "middle"]);

        assert_eq!(
            step_back_through_history(&history, Some(9), ""),
            HistoryStep::Recall {
                index: 0,
                message: "newest".into()
            },
            "an empty box starts the walk again rather than trusting the position"
        );
        assert_eq!(
            step_forward_through_history(&history, Some(9), ""),
            HistoryStep::MoveCursor(CursorDirection::Down)
        );
    }

    /// The menu Claude Code draws for a question taking several answers, as measured
    /// from a real one rather than assumed.
    ///
    /// The rows below the options are `Type something`, `Submit` and `Chat about this`.
    /// A digit toggles its own row and leaves the cursor where it was — which is the
    /// fact the first version of this got wrong. Down stops on the last row rather than
    /// wrapping, and Up from the first row wraps to the last numbered row.
    struct SeveralChoicesMenu {
        option_count: usize,
        cursor: usize,
        ticked: HashSet<usize>,
        activated: Option<usize>,
    }

    impl SeveralChoicesMenu {
        fn new(option_count: usize, cursor: usize) -> Self {
            Self {
                option_count,
                cursor,
                ticked: HashSet::default(),
                activated: None,
            }
        }

        /// `Type something` is the row after the options, and is numbered like them.
        fn last_numbered_row(&self) -> usize {
            self.option_count
        }

        fn submit_row(&self) -> usize {
            self.option_count + 1
        }

        /// The options, `Type something`, `Submit`, and `Chat about this`.
        fn rows(&self) -> usize {
            self.option_count + 3
        }

        fn toggle(&mut self, row: usize) {
            if !self.ticked.remove(&row) {
                self.ticked.insert(row);
            }
        }

        fn press(&mut self, key: PaneKey) {
            match key {
                PaneKey::Choice(_) => {
                    let row: usize = key
                        .tmux_name()
                        .parse::<usize>()
                        .expect("a choice is sent as its digit")
                        - 1;
                    assert!(
                        row <= self.last_numbered_row(),
                        "the menu has no row {}, so the digit for it answers nothing",
                        row + 1
                    );
                    self.toggle(row);
                }
                PaneKey::Down => self.cursor = (self.cursor + 1).min(self.rows() - 1),
                PaneKey::Up => {
                    self.cursor = self
                        .cursor
                        .checked_sub(1)
                        .unwrap_or(self.last_numbered_row())
                }
                PaneKey::Enter => self.activated = Some(self.cursor),
                other => panic!("the menu was sent {other:?}, which is not one of its keys"),
            }
        }

        fn play(&mut self, keys: &[PaneKey]) {
            for key in keys {
                self.press(*key);
            }
        }
    }

    /// The property the whole sequence turns on: a digit does not move the cursor, so
    /// where the walk to `Submit` starts is not knowable from the ticks. Counting rows
    /// from the last ticked option put `Enter` on whatever row it happened to reach —
    /// ticking an option the reader never chose, or opening `Type something`.
    #[test]
    fn submitting_several_choices_reaches_submit_from_wherever_the_cursor_was() {
        let option_count = 4;
        let ticked: HashSet<usize> = HashSet::from_iter([0, 1]);
        let keys = keys_for_several_choices(&ticked, option_count)
            .expect("two ticked options are an answer");

        for starting_cursor in 0..option_count + 3 {
            let mut menu = SeveralChoicesMenu::new(option_count, starting_cursor);
            menu.play(&keys);

            assert_eq!(
                menu.activated,
                Some(menu.submit_row()),
                "starting on row {starting_cursor}, Enter landed on row {:?} rather than \
                 Submit on row {}",
                menu.activated,
                menu.submit_row()
            );
            assert_eq!(
                menu.ticked, ticked,
                "starting on row {starting_cursor}, the answer submitted was {:?} rather \
                 than the {:?} that were ticked",
                menu.ticked, ticked
            );
        }
    }

    /// The same holds for one tick and for every option ticked: neither end of the
    /// menu is a special case the walk gets right by luck.
    #[test]
    fn submitting_reaches_submit_for_any_set_of_ticks() {
        for option_count in 1..=5 {
            for ticks in [vec![0], vec![option_count - 1], (0..option_count).collect()] {
                let ticked: HashSet<usize> = ticks.iter().copied().collect();
                let keys = keys_for_several_choices(&ticked, option_count)
                    .expect("a ticked option is an answer");

                let mut menu = SeveralChoicesMenu::new(option_count, 0);
                menu.play(&keys);
                assert_eq!(
                    menu.activated,
                    Some(menu.submit_row()),
                    "with {option_count} options and {ticks:?} ticked, Enter landed on \
                     {:?} rather than Submit on row {}",
                    menu.activated,
                    menu.submit_row()
                );
                assert_eq!(menu.ticked, ticked);
            }
        }
    }

    /// `Type something` is one more numbered row in a menu that takes several answers,
    /// so its digit only ticks it: the box to type in opens when the answer is
    /// submitted. Sending the digit alone leaves the reader with a ticked row and
    /// nowhere to type.
    #[test]
    fn typing_an_answer_to_a_several_choices_question_submits_the_row_rather_than_ticking_it() {
        let option_count = 3;
        let already_ticked: HashSet<usize> = HashSet::from_iter([0]);
        let keys = keys_for_own_words(&already_ticked, option_count, true)
            .expect("typing is always an answer");

        let mut menu = SeveralChoicesMenu::new(option_count, 0);
        menu.play(&keys);

        assert_eq!(
            menu.activated,
            Some(menu.submit_row()),
            "Enter landed on {:?} rather than Submit on row {}",
            menu.activated,
            menu.submit_row()
        );
        assert_eq!(
            menu.ticked,
            HashSet::from_iter([0, option_count]),
            "what is submitted must be the ticks the reader made plus `Type something`"
        );
    }

    /// A question taking one answer is a different menu: the digit is the whole answer,
    /// and `Type something` opens the box outright.
    #[test]
    fn typing_an_answer_to_a_single_choice_question_is_one_keypress() {
        let option_count = 3;
        assert_eq!(
            keys_for_own_words(&HashSet::default(), option_count, false),
            keys_for_single_choice(option_count),
            "nothing is ticked and nothing is walked past in a menu taking one answer"
        );
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
    /// nothing the panel holds itself outlives one. The default is the left-hand dock:
    /// the project panel defaults to the right one, and two panels sharing a dock take
    /// turns being the visible one instead of being readable side by side.
    #[gpui::test]
    async fn the_dock_side_is_read_from_the_settings(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);

            assert_eq!(
                ClaudeSessionsSettings::get_global(cx).dock,
                DockSide::Left,
                "the left-hand dock is the one the project panel is not already in"
            );

            <settings::SettingsStore as gpui::UpdateGlobal>::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |content| {
                    content.claude_sessions.get_or_insert_default().dock = Some(DockSide::Right);
                });
            });

            assert_eq!(
                ClaudeSessionsSettings::get_global(cx).dock,
                DockSide::Right,
                "the side in the settings is the side the panel opens on, not the default"
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

    /// Claude Code's own total is preferred because it prices every model it ran; the
    /// derived one is marked so that a reader can tell which they are looking at.
    #[test]
    fn the_reported_total_wins_and_a_derived_one_says_so() {
        let mut spend = Spend {
            model: Some("claude-opus-5".into()),
            usage: Usage {
                output_tokens: 1_000_000,
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(
            session_cost(&spend).as_deref(),
            Some("~$25"),
            "derived from the token counts, and marked as derived"
        );

        spend.reported = Some(crate::transcript::ReportedCost {
            total_usd: 173.309_849_25,
            ..Default::default()
        });
        assert_eq!(
            session_cost(&spend).as_deref(),
            Some("$173"),
            "Claude Code's own figure, unmarked"
        );
    }

    /// The two figures count different things — an answer's is the whole request, a
    /// compaction's is the conversation it kept — so the jump from one to the other has to
    /// be explained where it is shown or it reads as the panel having been wrong.
    #[test]
    fn a_context_read_from_a_compaction_says_so() {
        let measured = Spend {
            context_tokens: 94_578,
            ..Default::default()
        };
        assert!(
            session_facts(&measured).contains(&SharedString::from("94K ctx")),
            "a measured context is stated plainly: {:?}",
            session_facts(&measured)
        );

        let compacted = Spend {
            context_tokens: 14_846,
            context_is_post_compaction: true,
            ..Default::default()
        };
        assert!(
            session_facts(&compacted).contains(&SharedString::from("14K ctx (compacted)")),
            "and one left by a compaction is marked: {:?}",
            session_facts(&compacted)
        );
    }

    /// A model with no known rates must produce no figure rather than a wrong one.
    #[test]
    fn a_session_on_an_unknown_model_shows_no_cost() {
        let spend = Spend {
            model: Some("some-unreleased-model".into()),
            usage: Usage {
                output_tokens: 1_000_000,
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(session_cost(&spend), None);
        assert!(
            !session_facts(&spend).iter().any(|fact| fact.contains('$')),
            "and nothing in the toolbar claims a price: {:?}",
            session_facts(&spend)
        );
    }

    /// The three kinds of input are priced twenty-fold apart, so a single "in" figure
    /// said nothing about where an answer's money went.
    #[test]
    fn the_cost_line_names_each_kind_of_input() {
        let rates = rates_for_model("claude-opus-5").expect("opus 5 is priced");
        // The shape a real answer has: almost nothing fresh, a large cache read, and a
        // cache write when the conversation grew since the last one.
        let usage = Usage {
            input_tokens: 2,
            cache_read_tokens: 29_592,
            cache_write_1h_tokens: 27_456,
            cache_write_5m_tokens: 0,
            output_tokens: 488,
            thinking_tokens: 335,
        };

        assert_eq!(
            answer_summary(usage, rates),
            // The cache write dominates: 27,456 tokens at twice the input rate is
            // $0.275 of the $0.302, while the 29,592 read tokens are $0.015.
            "$0.30 · 2 in · 29K cache read · 27K cache write · 488 out · (335 thinking)"
        );
    }

    /// Most answers only read from the cache, and a line of zeroes reads as noise.
    #[test]
    fn the_cost_line_leaves_out_what_an_answer_did_not_use() {
        let rates = rates_for_model("claude-opus-5").expect("opus 5 is priced");
        let usage = Usage {
            cache_read_tokens: 100_000,
            output_tokens: 50,
            ..Default::default()
        };

        assert_eq!(
            answer_summary(usage, rates),
            "$0.05 · 100K cache read · 50 out",
            "no fresh input, no cache write, and no thinking to report"
        );
    }

    /// Every registered process is alive, because `updatedAt` is not a heartbeat and a
    /// quiet session cannot be filtered out for being quiet. So the row has to say which
    /// ones have sat untouched, and stay quiet about the ones simply between turns.
    #[test]
    fn only_a_session_left_sitting_reports_how_long() {
        const MINUTE: i64 = 60 * 1000;
        const HOUR: i64 = 60 * MINUTE;
        let now = 1_789_000_000_000;

        assert_eq!(
            idle_for(Some(now - 30 * 1000), now),
            None,
            "half a minute is a session between turns"
        );
        assert_eq!(
            idle_for(Some(now - 9 * MINUTE), now),
            None,
            "and so is nine minutes"
        );
        assert_eq!(
            idle_for(Some(now - 45 * MINUTE), now).as_deref(),
            Some("idle 45m")
        );
        assert_eq!(
            idle_for(Some(now - 33 * HOUR), now).as_deref(),
            Some("idle 33h")
        );
        assert_eq!(
            idle_for(Some(now - 58 * HOUR), now).as_deref(),
            Some("idle 2d"),
            "past a day the hours stop being the useful unit"
        );
        assert_eq!(
            idle_for(None, now),
            None,
            "a registration with no timestamp says nothing rather than claiming zero"
        );
    }

    #[test]
    fn small_amounts_keep_their_cents() {
        assert_eq!(format_usd(0.004), "$0.00");
        assert_eq!(format_usd(0.42), "$0.42");
        assert_eq!(format_usd(9.99), "$9.99");
        assert_eq!(format_usd(10.4), "$10", "above ten the cents are noise");
        assert_eq!(format_usd(141.75), "$142");
    }

    #[test]
    fn token_counts_read_as_a_reader_reads_them() {
        assert_eq!(compact_token_count(0), "0");
        assert_eq!(compact_token_count(9_999), "9999");
        assert_eq!(compact_token_count(534_777), "534K");
        assert_eq!(compact_token_count(2_300_000), "2.3M");
    }

    /// A message typed while the session was answering. Claude Code records the text
    /// only in a `queued_command` attachment, and writes the user record it belongs to
    /// with empty content, so the reader's own words were drawn nowhere in the
    /// conversation — they were folded into the context section with the injected
    /// attachments, and the record that should have carried them was skipped as empty.
    #[test]
    fn a_message_queued_behind_a_turn_is_drawn_as_the_readers_own() {
        let entries = entries_of(&[
            r#"{"type":"user","uuid":"a","message":{"content":"what I asked first"}}"#,
            r#"{"type":"attachment","uuid":"b","parentUuid":"a",
"attachment":{"type":"queued_command","prompt":"and this while it was busy"}}"#,
            // The record the queued message belongs to, as Claude Code writes it.
            r#"{"type":"user","uuid":"c","parentUuid":"b","message":{"content":""}}"#,
        ]);

        let messages = message_sources(&entries);
        assert_eq!(
            messages,
            vec![
                SharedString::from("what I asked first"),
                SharedString::from("and this while it was busy"),
            ],
            "the queued message is one of the reader's messages, in the order they typed \
             it, and is drawn exactly once"
        );
        assert!(
            !entries
                .iter()
                .any(|entry| matches!(entry.kind, EntryKind::Attachments { .. })),
            "it is not context injected around the turn, so it does not fold into that \
             section"
        );
    }

    /// Context Claude Code injects into a turn — a skill's instructions, an expanded
    /// command — carries the user's own role and can run to tens of thousands of lines.
    #[test]
    fn injected_context_is_not_drawn_as_a_message() {
        let entries = entries_of(&[
            r#"{"type":"user","uuid":"a","message":{"content":"what I typed"}}"#,
            r#"{"type":"user","uuid":"b","parentUuid":"a","isMeta":true,
"message":{"content":"Base directory for this skill: ..."}}"#,
        ]);

        assert_eq!(
            message_sources(&entries),
            vec![SharedString::from("what I typed")],
            "only what the reader typed is theirs"
        );
    }

    /// A background command reporting in is queued the same way a message typed mid-turn
    /// is, so the panel drew the CLI's own `<task-notification>` block as words the reader
    /// had typed — several screenfuls of them over a long session.
    #[test]
    fn a_queued_task_notification_is_not_drawn_as_a_message() {
        let entries = entries_of(&[
            r#"{"type":"attachment","uuid":"a","parentUuid":"z","attachment":{
"type":"queued_command","commandMode":"task-notification","prompt":
"<task-notification>\n<task-id>byygs214e</task-id>\n<status>completed</status>\n</task-notification>"}}"#,
            r#"{"type":"attachment","uuid":"b","parentUuid":"a","attachment":{
"type":"queued_command","prompt":"and this is mine"}}"#,
        ]);

        assert_eq!(
            message_sources(&entries),
            vec![SharedString::from("and this is mine")],
            "the notification is the CLI's, and only the typed message is the reader's"
        );
    }

    /// A queued message with an image in it is recorded as content blocks, and the blocks
    /// carry the image's base64 beside the text.
    #[test]
    fn a_queued_message_with_an_image_is_drawn_from_its_text() {
        let entries = entries_of(&[
            r#"{"type":"attachment","uuid":"a","parentUuid":"z","attachment":{
"type":"queued_command","prompt":[
{"type":"text","text":"look at this"},
{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAABBBB"}}]}}"#,
        ]);

        assert_eq!(
            message_sources(&entries),
            vec![SharedString::from("look at this")],
            "the text is the message; the encoded image is not part of it"
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

    /// The one record the CLI writes onto the conversation's chain with no content of
    /// any kind: drawn as unrecognized JSON, it was the largest block in the
    /// conversation and said only how many milliseconds the turn took.
    #[test]
    fn a_turn_duration_record_is_left_out_of_the_conversation() {
        let entries = entries_of(&[
            r#"{"type":"user","uuid":"a","message":{"content":"hello"}}"#,
            r#"{"type":"system","subtype":"turn_duration","uuid":"b","parentUuid":"a",
"durationMs":162711,"messageCount":1177}"#,
        ]);

        assert_eq!(
            entries.len(),
            1,
            "the timing record has nothing for a reader and must not be drawn at all, \
             but the conversation around it is kept; entry keys: {:?}",
            entries.iter().map(|entry| &entry.key).collect::<Vec<_>>()
        );
        assert!(
            matches!(entries[0].kind, EntryKind::Message { .. }),
            "the message before it is what is left"
        );
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
            workflow_agent_finished: None,
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

    /// A `Workflow` run's agents carry no `tool_use_id` of their own, and the call that
    /// started the run is answered while the run is still going, so the conversation is
    /// not what says they are over. The run's journal is, and the machine the run
    /// happened on has already read it into the summary.
    #[test]
    fn a_workflow_agent_is_over_when_its_runs_journal_says_it_returned() {
        let call = tool_use_line("m2", "toolu_09", "Workflow", serde_json::json!({}));
        let opening = user_message_line("m1", "go");
        let conversation: [&str; 2] = [&opening, &call];

        let mut returned = subagent("a1", Some("wf_b529a29d-562"), None);
        returned.workflow_agent_finished = Some(true);
        assert_eq!(
            state_of(&returned, &conversation),
            AgentState::Finished,
            "the journal recorded this agent's result, which is the agent having returned"
        );

        let mut working = subagent("a1", Some("wf_b529a29d-562"), None);
        working.workflow_agent_finished = Some(false);
        assert_eq!(
            state_of(&working, &conversation),
            AgentState::Running,
            "the journal has no result for this agent, so it has not returned"
        );
    }

    /// The sidecar this scan lists an agent from is written when the agent is spawned,
    /// before the run's journal records it starting, and a run that has only just
    /// launched may have written no journal at all. Both gaps are the beginning of an
    /// agent's work, and calling either one of them the end draws a working run as done.
    #[test]
    fn a_workflow_agent_no_journal_accounts_for_is_taken_to_be_working() {
        let call = tool_use_line("m2", "toolu_09", "Workflow", serde_json::json!({}));
        let opening = user_message_line("m1", "go");
        let conversation: [&str; 2] = [&opening, &call];

        let unaccounted = subagent("a1", Some("wf_b529a29d-562"), None);
        assert_eq!(
            unaccounted.workflow_agent_finished, None,
            "the scan found no journal to account for this agent"
        );
        assert_eq!(state_of(&unaccounted, &conversation), AgentState::Running);
    }

    /// `Workflow` runs in the background and its call is answered the moment the run
    /// starts, so the `Run ID:` the answer carries announces a run that has only just
    /// begun. Reading that announcement as the run being over draws every agent of a
    /// running workflow as finished for as long as it runs.
    #[test]
    fn a_workflow_run_that_has_only_been_launched_is_still_running() {
        let summary = subagent("a1", Some("wf_b25f31ee-cab"), None);
        let call = tool_use_line("m2", "toolu_09", "Workflow", serde_json::json!({}));
        // Verbatim from a `Workflow` call that was still running when it was captured.
        let launch = tool_result_line_with_content(
            "m3",
            "toolu_09",
            "Workflow launched in background. Task ID: w2gew6q96\n\
             Summary: write the copy for the fifteen pair pages\n\
             Transcript dir: /home/coder/.claude/projects/-slug/session/subagents/workflows/wf_b25f31ee-cab\n\
             Run ID: wf_b25f31ee-cab\n\
             \n\
             You will be notified when it completes. Use /workflows to watch live progress.",
        );

        assert_eq!(
            state_of(&summary, &[&user_message_line("m1", "go"), &call, &launch]),
            AgentState::Running,
            "the run id is announced when the run starts, so the announcement is not the run ending"
        );
    }

    fn card(state: AgentState) -> AgentCardRow {
        AgentCardRow {
            label: SharedString::from("copy:batch-a"),
            state,
            phase: None,
            target: TranscriptTarget::Main,
        }
    }

    /// Captured from a terminal asking the second of two questions. The strip is the only
    /// thing read out of the screen, so the rest is here to show it is not.
    const TWO_QUESTION_PANE: &str = "\
←  ☒ Project shape  ☐ Extras  ✔ Submit  →

Which extras should be on?

❯ 1. [ ] CodeGraph index
  Symbols and blast radius
  2. [ ] Regression gate
     Submit

  5. Chat about this

Enter to select · Tab/Arrow keys to navigate · Esc to cancel";

    fn command(name: &str, scope: SlashCommandScope) -> SlashCommand {
        SlashCommand {
            name: name.to_string(),
            description: None,
            argument_hint: None,
            scope,
        }
    }

    /// A slash partway through a sentence is a path, a date, or a fraction. A menu that
    /// opened on those would be in the way of every message that mentions a file.
    #[test]
    fn only_a_message_that_is_nothing_but_a_command_is_naming_one() {
        assert_eq!(slash_command_being_named("/comp"), Some("comp"));
        assert_eq!(
            slash_command_being_named("/"),
            Some(""),
            "a slash on its own is the whole menu, which is what it is for"
        );

        assert_eq!(slash_command_being_named("look at src/main.rs"), None);
        assert_eq!(
            slash_command_being_named("/compact now"),
            None,
            "once the arguments are being typed the name is settled"
        );
        assert_eq!(
            slash_command_being_named("//"),
            None,
            "a doubled slash is not a command being named"
        );
        assert_eq!(slash_command_being_named(""), None);
        assert_eq!(slash_command_being_named("compact"), None);
    }

    /// A name is usually typed from its beginning, so what starts with it is what the
    /// reader is reaching for; what merely contains it is worth offering, but behind.
    #[test]
    fn commands_starting_with_what_was_typed_come_before_ones_merely_containing_it() {
        let commands = vec![
            command("review", SlashCommandScope::Project),
            command("code-review", SlashCommandScope::User),
            command("resume", SlashCommandScope::Builtin),
            command("clear", SlashCommandScope::Builtin),
        ];

        let names: Vec<&str> = matching_slash_commands(&commands, "re")
            .into_iter()
            .map(|command| command.name.as_str())
            .collect();
        assert_eq!(names, vec!["review", "resume", "code-review"]);

        assert_eq!(
            matching_slash_commands(&commands, "REV")
                .into_iter()
                .map(|command| command.name.as_str())
                .collect::<Vec<_>>(),
            vec!["review", "code-review"],
            "what was typed is matched however it was capitalised"
        );

        assert_eq!(
            matching_slash_commands(&commands, "").len(),
            4,
            "a slash on its own offers everything"
        );
        assert!(
            matching_slash_commands(&commands, "nothing-by-this-name").is_empty(),
            "a command this machine does not hold offers no rows, and an empty menu is \
             not drawn at all"
        );
    }

    /// The menu takes room the conversation is in, so it offers a screenful at most —
    /// and must not go on drawing rows past that when everything matches.
    #[test]
    fn the_menu_offers_at_most_a_screenful() {
        let commands: Vec<SlashCommand> = (0..SLASH_COMMAND_ROWS + 5)
            .map(|index| command(&format!("command-{index}"), SlashCommandScope::User))
            .collect();

        assert_eq!(
            matching_slash_commands(&commands, "").len(),
            SLASH_COMMAND_ROWS
        );
    }

    /// A call carrying one question draws no strip at all, and the reader is on its only
    /// question. Counting nothing as "some other question" would draw the wrong one.
    #[test]
    fn a_call_with_one_question_leaves_the_reader_on_it() {
        let single = "\
 ☐ Options

Pick one:

❯ 1. Alpha
  2. Beta

Enter to select · ↑/↓ to navigate · Esc to cancel";

        assert_eq!(question_being_asked(Some(single), &[]), 0);
        assert_eq!(
            question_being_asked(None, &[]),
            0,
            "a terminal that could not be read leaves the reader where a call starts"
        );
        assert_eq!(
            question_being_asked(Some("no question is on this screen at all"), &[]),
            0
        );
    }

    /// The strip ticks the questions already answered, so the one being asked is the
    /// count of ticks. Reading it wrong draws the reader the options of another question.
    #[test]
    fn the_question_being_asked_is_the_one_after_the_answered_ones() {
        assert_eq!(question_being_asked(Some(TWO_QUESTION_PANE), &[]), 1);

        let none_answered = TWO_QUESTION_PANE.replace('☒', "☐");
        assert_eq!(question_being_asked(Some(&none_answered), &[]), 0);

        let both_answered = TWO_QUESTION_PANE.replacen('☐', "☒", 1);
        assert_eq!(question_being_asked(Some(&both_answered), &[]), 2);
    }

    /// Nothing ticked is not an answer: walking to `Submit` and pressing it would submit
    /// an empty one.
    ///
    /// What the rest of this test used to assert — the exact walk, counted from the last
    /// ticked option — was measured against a real menu and found wrong; a digit does not
    /// move the cursor there. The menu simulator above covers the walk now, against the
    /// behaviour that was measured rather than against a key sequence.
    #[test]
    fn nothing_ticked_is_not_an_answer() {
        assert_eq!(keys_for_several_choices(&HashSet::default(), 3), None);
    }

    /// The line above a run's cards is the part a reader sees without opening anything,
    /// so it has to count what is working rather than what was spawned.
    #[test]
    fn the_line_above_a_runs_cards_counts_what_is_still_working() {
        assert_eq!(
            workflow_run_note(&[card(AgentState::Running), card(AgentState::Finished)]).as_ref(),
            "2 agents · 1 running"
        );
        assert_eq!(
            workflow_run_note(&[card(AgentState::Running)]).as_ref(),
            "1 agent · 1 running",
            "one agent is an agent, not agents"
        );
        assert_eq!(
            workflow_run_note(&[card(AgentState::Finished), card(AgentState::Finished)]).as_ref(),
            "2 agents · none running",
            "a run between phases has spawned nothing new yet, and `0 running` reads as a \
             claim that the run is over"
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

    /// The records `pair_with` is matched against in the panel, which are not the ones
    /// the conversation is drawn from — the bug below lived in the gap between the two.
    fn user_message_entries_of(json_lines: &[&str]) -> Vec<Entry> {
        let records: Vec<TranscriptRecord> = json_lines.iter().map(|line| record(line)).collect();
        let path: Vec<&TranscriptRecord> = records.iter().collect();
        user_message_entries(&path)
    }

    /// A message typed while the session was answering is recorded as an attachment and
    /// never as a user record — that is the only place its text is ever written. Left out
    /// of what a send is paired against, its `Sending…` message stays on screen for the
    /// rest of the session while the conversation draws the very same words beside it.
    #[test]
    fn a_pending_message_goes_when_it_arrives_as_a_queued_message() {
        let queued = serde_json::json!({
            "type": "attachment",
            "uuid": "b",
            "parentUuid": "a",
            "attachment": {
                "type": "queued_command",
                "prompt": "run the tests",
            },
        })
        .to_string();

        let before = user_message_entries_of(&[&user_message_line("a", "an earlier message")]);
        let mut pending_sends = PendingSends::default();
        pending_sends.remember(7, "run the tests", &before);

        let after =
            user_message_entries_of(&[&user_message_line("a", "an earlier message"), &queued]);
        pending_sends.pair_with(&after);

        assert!(
            pending_sends.is_empty(),
            "the queued record is the message arriving, and it is the only record it \
             will ever get"
        );
    }

    /// The same for a message queued with an image in it, whose text is one block of
    /// several rather than the whole of the field.
    #[test]
    fn a_pending_message_goes_when_it_arrives_as_a_queued_message_with_an_image() {
        let queued = serde_json::json!({
            "type": "attachment",
            "uuid": "b",
            "parentUuid": "a",
            "attachment": {
                "type": "queued_command",
                "prompt": [
                    {"type": "text", "text": "look at this"},
                    {
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": "image/png",
                            "data": "aGVsbG8=",
                        },
                    },
                ],
            },
        })
        .to_string();

        let before = user_message_entries_of(&[&user_message_line("a", "an earlier message")]);
        let mut pending_sends = PendingSends::default();
        pending_sends.remember(7, "look at this", &before);

        let after =
            user_message_entries_of(&[&user_message_line("a", "an earlier message"), &queued]);
        pending_sends.pair_with(&after);

        assert!(pending_sends.is_empty());
    }

    /// The key has to be the one the conversation draws that record under, or the entry
    /// the send was paired with is not the entry on screen.
    #[test]
    fn a_queued_message_is_keyed_the_same_way_in_both_lists() {
        let queued = serde_json::json!({
            "type": "attachment",
            "uuid": "b",
            "parentUuid": "a",
            "attachment": {"type": "queued_command", "prompt": "run the tests"},
        })
        .to_string();

        let keys_of = |entries: Vec<Entry>| -> Vec<SharedString> {
            entries
                .into_iter()
                .filter(|entry| matches!(entry.kind, EntryKind::Message { .. }))
                .map(|entry| entry.key)
                .collect()
        };

        assert_eq!(
            keys_of(user_message_entries_of(&[&queued])),
            keys_of(entries_of(&[&queued])),
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
                    // Read from the end of a real transcript, which these tests write by
                    // hand and never through that reader.
                    spend: None,
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
                workflow_agent_finished: None,
            }]))
        }

        fn pending_question(
            &self,
            _session_id: String,
        ) -> Task<anyhow::Result<crate::session_source::QuestionState>> {
            Task::ready(Ok(crate::session_source::QuestionState {
                question: None,
                hook_installed: false,
                live_message: None,
            }))
        }

        fn install_question_hook(&self) -> Task<anyhow::Result<Option<PathBuf>>> {
            Task::ready(Ok(None))
        }

        fn list_slash_commands(
            &self,
            _project_root: Option<PathBuf>,
        ) -> Task<anyhow::Result<Vec<SlashCommand>>> {
            Task::ready(Ok(Vec::new()))
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

        fn capture_pane(&self, _pane_target: String) -> Task<anyhow::Result<String>> {
            Task::ready(Ok(String::new()))
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
                        history_index: None,
                        quit_armed: false,
                        ticked: None,
                        typing_answer_for: None,
                        slash_highlight: 0,
                        dismissed_slash_menu_for: None,
                        slash_commands: Vec::new(),
                        _listing_slash_commands: Task::ready(()),
                        _answering: Task::ready(()),
                        hook_install_note: None,
                        _installing_hook: Task::ready(()),
                        workspace: WeakEntity::new_invalid(),
                        focus_handle: cx.focus_handle(),
                        fs: fs::FakeFs::new(cx.background_executor().clone()),
                        store,
                        source,
                        message_editor,
                        project_root: None,
                        in_pane: false,
                        pane_expanded: true,
                        terminal: None,
                        terminal_process_id: None,
                        terminal_height: DEFAULT_TERMINAL_HEIGHT,
                        input_expanded: false,
                        _terminal_attach: Task::ready(()),
                        entries: Vec::new(),
                        pending_sends: PendingSends::default(),
                        activity: Activity::Idle,
                        agent_calls: HashMap::default(),
                        list_state: ListState::new(0, ListAlignment::Bottom, px(1024.)),
                        session_list_expanded: true,
                        unread_below: UnreadBelow::default(),
                        scrolled_to_end: true,
                        expanded: HashSet::default(),
                        show_full_history: false,
                        show_tool_calls: false,
                        show_costs: false,
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

    #[gpui::test]
    async fn switching_conversations_clears_the_answer_state_of_the_one_left(
        cx: &mut gpui::TestAppContext,
    ) {
        let panel = scripted_panel(&[], &[], cx);
        let actual = panel.update(cx, |panel, _cx| {
            panel.history_index = Some(3);
            panel.quit_armed = true;
            let mut ticked_options = HashSet::default();
            ticked_options.insert(0);
            panel.ticked = Some(TickedAnswer {
                tool_use_id: "call-from-the-old-conversation".into(),
                question_index: 1,
                ticked: ticked_options,
            });
            panel.typing_answer_for = Some("call-from-the-old-conversation".into());

            panel.show_the_newest_of_another_conversation();
            (
                panel.history_index,
                panel.quit_armed,
                panel.ticked.is_some(),
                panel.typing_answer_for.is_some(),
            )
        });

        assert_eq!(
            actual,
            (None, false, false, false),
            "all state aimed at the old conversation must be cleared; expected (None, false, false, false), got {actual:?}"
        );
    }

    #[gpui::test]
    async fn switching_conversations_cancels_an_answer_sequence_in_flight(
        cx: &mut gpui::TestAppContext,
    ) {
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _panel = panel_with_answer_task(&completed, cx);
        cx.run_until_parked();
        cx.executor().advance_clock(ANSWER_KEY_INTERVAL * 2);
        cx.run_until_parked();

        let actual = completed.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            actual, false,
            "the answer task belongs to the old conversation; expected completed=false, got completed={actual}"
        );
    }

    fn panel_with_answer_task(
        completed: &Arc<std::sync::atomic::AtomicBool>,
        cx: &mut gpui::TestAppContext,
    ) -> Entity<ClaudeSessionsPanel> {
        let panel = scripted_panel(&[], &[], cx);
        panel.update(cx, |panel, cx| {
            panel._answering = cx.spawn({
                let completed = completed.clone();
                async move |_, cx| {
                    cx.background_executor().timer(ANSWER_KEY_INTERVAL).await;
                    completed.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            });
            panel.show_the_newest_of_another_conversation();
        });
        panel
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
