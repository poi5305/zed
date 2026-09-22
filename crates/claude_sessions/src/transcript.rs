//! Parsing and traversal of Claude Code transcript files
//! (`~/.claude/projects/<slug>/<sessionId>.jsonl`).
//!
//! A transcript file is append-only JSON Lines, but the conversation it encodes is a
//! tree: every record carries a `uuid` and a `parentUuid`. Rewinds, resumed branches
//! and compaction all leave abandoned branches behind in the same file, so replaying
//! the file linearly would render messages that are no longer part of the
//! conversation. The only faithful reading is to start from the newest record and
//! walk back along `parentUuid`.
//!
//! This module performs no file I/O; it only consumes lines and records.

use crate::usage::Usage;
use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};
use gpui::SharedString;
use serde::Deserialize;
use serde_json::Value;
use std::collections::VecDeque;

const COMPACT_BOUNDARY_SUBTYPE: &str = "compact_boundary";

/// The log Claude Code writes for the messages queued behind a running turn.
const QUEUE_OPERATION_RECORD_TYPE: &str = "queue-operation";

/// Claude Code's own accounting of what the session has cost.
const COST_STATE_RECORD_TYPE: &str = "cost-state";

const PERMISSION_MODE_RECORD_TYPE: &str = "permission-mode";
const AI_TITLE_RECORD_TYPE: &str = "ai-title";
const BRIDGE_SESSION_RECORD_TYPE: &str = "bridge-session";
const ATTACHMENT_RECORD_TYPE: &str = "attachment";
const TOTAL_TOKENS_REMINDER_ATTACHMENT: &str = "total_tokens_reminder";
const AUTO_MODE_ATTACHMENT: &str = "auto_mode";
const TOTAL_TOKENS_OPEN: &str = "<total_tokens>";

/// Flags the `auto_mode` attachment records on a session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AutoModeFlags {
    pub bash_first: bool,
    pub steer_only: bool,
    pub bypass: bool,
}

#[derive(Debug, Clone)]
pub struct TranscriptRecord {
    pub uuid: Option<String>,
    pub parent_uuid: Option<String>,
    pub logical_parent_uuid: Option<String>,
    pub is_sidechain: bool,
    pub record_type: String,
    pub subtype: Option<String>,
    pub is_compact_summary: bool,
    pub compact_metadata: Option<CompactMetadata>,
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct CompactMetadata {
    pub trigger: String,
    pub pre_tokens: Option<u64>,
    pub post_tokens: Option<u64>,
}

/// Mirrors only the fields this module needs to reconstruct the tree. Unknown fields
/// are intentionally accepted and left to [`TranscriptRecord::raw`]: the transcript
/// format is private to Claude Code and gains fields without notice. In particular,
/// `message.content` is sometimes a string and sometimes an array, so `message` is
/// never given a type here.
#[derive(Deserialize)]
struct RawRecord {
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default, rename = "parentUuid")]
    parent_uuid: Option<String>,
    #[serde(default, rename = "logicalParentUuid")]
    logical_parent_uuid: Option<String>,
    #[serde(default, rename = "isSidechain")]
    is_sidechain: Option<bool>,
    #[serde(rename = "type")]
    record_type: String,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default, rename = "isCompactSummary")]
    is_compact_summary: Option<bool>,
    #[serde(default, rename = "compactMetadata")]
    compact_metadata: Option<RawCompactMetadata>,
}

#[derive(Deserialize)]
struct RawCompactMetadata {
    #[serde(default)]
    trigger: Option<String>,
    #[serde(default, rename = "preTokens")]
    pre_tokens: Option<u64>,
    #[serde(default, rename = "postTokens")]
    post_tokens: Option<u64>,
}

/// Parses one line of a transcript file. Blank and whitespace-only lines yield
/// `Ok(None)`. Malformed JSON yields `Err` so that the caller can skip the line and
/// keep reading: a transcript being appended to can be read mid-write.
pub fn parse_record(line: &str) -> Result<Option<TranscriptRecord>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let raw: Value =
        serde_json::from_str(trimmed).context("parsing transcript line as JSON value")?;
    let parsed =
        RawRecord::deserialize(&raw).context("reading transcript record's known fields")?;

    Ok(Some(TranscriptRecord {
        uuid: parsed.uuid,
        parent_uuid: parsed.parent_uuid,
        logical_parent_uuid: parsed.logical_parent_uuid,
        is_sidechain: parsed.is_sidechain.unwrap_or(false),
        record_type: parsed.record_type,
        subtype: parsed.subtype,
        is_compact_summary: parsed.is_compact_summary.unwrap_or(false),
        compact_metadata: parsed.compact_metadata.map(|metadata| CompactMetadata {
            trigger: metadata.trigger.unwrap_or_default(),
            pre_tokens: metadata.pre_tokens,
            post_tokens: metadata.post_tokens,
        }),
        raw,
    }))
}

pub struct Transcript {
    records: Vec<TranscriptRecord>,
    /// The messages waiting behind the turn that is running, oldest first; see
    /// [`Self::queued_messages`].
    queued: VecDeque<SharedString>,
    index_by_uuid: HashMap<String, usize>,
    leaf_uuid: Option<String>,
    overwritten_uuids: Vec<String>,
    /// Whether this transcript is itself one sub-agent's conversation rather than a
    /// session's own thread; see [`Self::for_sidechain`].
    reads_sidechain: bool,
    last_permission_mode_index: Option<usize>,
    last_ai_title_index: Option<usize>,
    last_bridge_session_index: Option<usize>,
    last_tokens_left_index: Option<usize>,
    last_auto_mode_index: Option<usize>,
}

impl Transcript {
    /// A session's own thread, in which a sidechain record belongs to a sub-agent and is
    /// therefore not part of the conversation being read.
    pub fn new() -> Self {
        Self::with_reading(false)
    }

    /// One sub-agent's own conversation.
    ///
    /// Every line of such a file carries `isSidechain`, because that is how Claude Code
    /// marks a record as belonging to an agent rather than to the thread the user is
    /// talking in. Read as a session's own thread the file therefore has no conversation
    /// in it at all: the leaf never moves and every record is skipped on the way back.
    /// This reading takes the file for what it is, and is only ever used on a file that
    /// is one agent's.
    pub fn for_sidechain() -> Self {
        Self::with_reading(true)
    }

    fn with_reading(reads_sidechain: bool) -> Self {
        Self {
            records: Vec::new(),
            queued: VecDeque::new(),
            index_by_uuid: HashMap::default(),
            leaf_uuid: None,
            overwritten_uuids: Vec::new(),
            reads_sidechain,
            last_permission_mode_index: None,
            last_ai_title_index: None,
            last_bridge_session_index: None,
            last_tokens_left_index: None,
            last_auto_mode_index: None,
        }
    }

    /// Absorbs records in file order. Safe to call repeatedly as the file grows.
    pub fn absorb(&mut self, records: impl IntoIterator<Item = TranscriptRecord>) {
        for record in records {
            // Applied as the record arrives rather than replayed on demand: these have no
            // uuid, so they are only ever appended and never rewritten, which makes the
            // queue a running total rather than something to recompute.
            self.apply_queue_operation(&record);

            let Some(uuid) = record.uuid.clone() else {
                // Records such as `mode`, `last-prompt` and `ai-title` have no uuid, so
                // nothing can reference them and they can never be a leaf.
                let index = self.records.len();
                match record.record_type.as_str() {
                    PERMISSION_MODE_RECORD_TYPE => self.last_permission_mode_index = Some(index),
                    AI_TITLE_RECORD_TYPE => self.last_ai_title_index = Some(index),
                    BRIDGE_SESSION_RECORD_TYPE => self.last_bridge_session_index = Some(index),
                    _ => {}
                }
                self.note_attachment_facts(index, &record);
                self.records.push(record);
                continue;
            };

            // Sidechain records belong to sub-agent conversations, which are not part of
            // the main thread the user is watching, so they must not move the leaf —
            // unless the conversation being read is one agent's own, in which case they
            // are the only records there are.
            if self.reads_sidechain || !record.is_sidechain {
                self.leaf_uuid = Some(uuid.clone());
            }

            let existing_index = self.index_by_uuid.get(&uuid).copied();
            if existing_index
                .and_then(|index| self.records.get(index))
                .is_some()
            {
                self.overwritten_uuids.push(uuid.clone());
            }
            match existing_index.filter(|index| *index < self.records.len()) {
                Some(index) => {
                    self.note_attachment_facts(index, &record);
                    if let Some(slot) = self.records.get_mut(index) {
                        *slot = record;
                    }
                }
                None => {
                    let index = self.records.len();
                    self.note_attachment_facts(index, &record);
                    self.index_by_uuid.insert(uuid, index);
                    self.records.push(record);
                }
            }
        }
    }

    /// What the session has spent and how it is configured, read from its own answers.
    ///
    /// Totalled over every answer in the file rather than over the conversation on
    /// screen: compaction shortens that, and a bill does not shrink because the history
    /// behind it was summarised. Walked on each call rather than accumulated, because an
    /// answer's record is rewritten as it streams — adding each one as it arrived would
    /// count the same answer many times.
    pub fn spend(&self) -> Spend {
        let mut spend = Spend::default();
        for record in &self.records {
            // Claude Code's own accounting, written as a snapshot of the whole session,
            // so the newest one is the answer and the ones before it are history. It is
            // preferred over the total derived from token counts: it is the CLI's own
            // figure, and it prices models this code may not know the rates for.
            if record.record_type == COST_STATE_RECORD_TYPE {
                spend.reported = ReportedCost::from_record(&record.raw);
                continue;
            }

            // A compaction replaces the conversation the next answer will be given, so
            // the context the last answer measured is no longer what the session carries.
            // `postTokens` is Claude Code's own figure for what it kept, and it counts
            // the conversation alone: the system prompt, the tool definitions and the
            // skills are re-sent on the next request, so it reads low until that request
            // measures the whole context and supersedes it below.
            if record.subtype.as_deref() == Some(COMPACT_BOUNDARY_SUBTYPE) {
                if let Some(post_tokens) = record
                    .compact_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.post_tokens)
                {
                    spend.context_tokens = post_tokens;
                    spend.context_is_post_compaction = true;
                }
                continue;
            }

            let Some(usage) = Usage::from_record(&record.raw) else {
                continue;
            };
            spend.usage = spend.usage.add(usage);
            spend.answers = spend.answers.saturating_add(1);
            // The newest answer's own numbers, which say how the session is running now
            // rather than how it ran across the whole file.
            spend.context_tokens = usage.context_tokens();
            spend.context_is_post_compaction = false;
            if let Some(model) = record
                .raw
                .get("message")
                .and_then(|message| message.get("model"))
                .and_then(Value::as_str)
            {
                spend.model = Some(SharedString::from(model.to_string()));
            }
            if let Some(effort) = record.raw.get("effort").and_then(Value::as_str) {
                spend.effort = Some(SharedString::from(effort.to_string()));
            }
        }
        spend
    }

    /// The messages the user typed while a turn was running and that have not been taken
    /// into one yet, oldest first.
    ///
    /// Claude Code writes a line of a queue log for every change to the queue and
    /// nothing that states what it holds, so this is the log played forward. The
    /// operations are:
    ///
    /// - `enqueue` — the text arrived and is waiting.
    /// - `remove` — that text left the queue, with a `reason` for why.
    /// - `dequeue` — the oldest went into a turn; it carries no text of its own.
    /// - `popAll` — the queue was taken whole.
    pub fn queued_messages(&self) -> &VecDeque<SharedString> {
        &self.queued
    }

    /// The permission mode the transcript last recorded, which is not the `mode` record.
    pub fn permission_mode(&self) -> Option<&str> {
        self.string_at(
            self.last_permission_mode_index,
            PERMISSION_MODE_RECORD_TYPE,
            "permissionMode",
        )
    }

    pub fn ai_title(&self) -> Option<&str> {
        self.string_at(self.last_ai_title_index, AI_TITLE_RECORD_TYPE, "aiTitle")
    }

    pub fn bridge_session_id(&self) -> Option<&str> {
        self.string_at(
            self.last_bridge_session_index,
            BRIDGE_SESSION_RECORD_TYPE,
            "bridgeSessionId",
        )
    }

    /// Tokens remaining in the budget, from the newest `total_tokens_reminder` attachment.
    pub fn tokens_left(&self) -> Option<u64> {
        let record = self.records.get(self.last_tokens_left_index?)?;
        // Re-checked rather than trusted: `absorb` replaces a record in place when its
        // uuid comes back, and what comes back need not be the attachment the index was
        // noted for.
        let attachment = attachment_of_type(record, TOTAL_TOKENS_REMINDER_ATTACHMENT)?;
        parse_tokens_left(attachment.get("text").and_then(Value::as_str)?)
    }

    /// The newest `auto_mode` attachment's flags, if the transcript has recorded one.
    pub fn auto_mode(&self) -> Option<AutoModeFlags> {
        let record = self.records.get(self.last_auto_mode_index?)?;
        parse_auto_mode(record)
    }

    /// The `bashFirstSteer` the newest `auto_mode` attachment carries. It is a free
    /// string rather than a flag, so it sits beside [`AutoModeFlags`] instead of in it.
    pub fn auto_mode_steer(&self) -> Option<&str> {
        let record = self.records.get(self.last_auto_mode_index?)?;
        let attachment = attachment_of_type(record, AUTO_MODE_ATTACHMENT)?;
        attachment
            .get("bashFirstSteer")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|steer| !steer.is_empty())
    }

    fn note_attachment_facts(&mut self, index: usize, record: &TranscriptRecord) {
        if record.record_type != ATTACHMENT_RECORD_TYPE {
            return;
        }
        let Some(attachment_type) = record
            .raw
            .get("attachment")
            .and_then(|attachment| attachment.get("type"))
            .and_then(Value::as_str)
        else {
            return;
        };
        match attachment_type {
            TOTAL_TOKENS_REMINDER_ATTACHMENT => self.last_tokens_left_index = Some(index),
            AUTO_MODE_ATTACHMENT => self.last_auto_mode_index = Some(index),
            _ => {}
        }
    }

    fn string_at(
        &self,
        cached_index: Option<usize>,
        record_type: &str,
        field: &str,
    ) -> Option<&str> {
        cached_index
            .and_then(|index| self.records.get(index))
            .filter(|record| record.record_type == record_type)
            .and_then(|record| record.raw.get(field).and_then(Value::as_str))
    }

    fn apply_queue_operation(&mut self, record: &TranscriptRecord) {
        if record.record_type != QUEUE_OPERATION_RECORD_TYPE {
            return;
        }
        let content = record
            .raw
            .get("content")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|content| !content.is_empty());

        match record.raw.get("operation").and_then(Value::as_str) {
            Some("enqueue") => {
                if let Some(content) = content {
                    self.queued
                        .push_back(SharedString::from(content.to_string()));
                }
            }
            // The text says which one left, because a queue can hold more than one and
            // the one removed is not always the oldest.
            Some("remove") => {
                if let Some(content) = content
                    && let Some(position) = self.queued.iter().position(|queued| queued == content)
                {
                    self.queued.remove(position);
                }
            }
            Some("dequeue") => {
                self.queued.pop_front();
            }
            Some("popAll") => self.queued.clear(),
            // An operation this does not know cannot be applied, and guessing at it would
            // leave the queue saying something untrue for the rest of the session.
            _ => {}
        }
    }

    /// The context the model currently sees: from the leaf back along `parentUuid`,
    /// stopping at the first record without a parent. After a compaction this begins
    /// at the compact boundary, because compaction deliberately severs the chain.
    pub fn active_path(&self) -> Vec<&TranscriptRecord> {
        self.walk(false)
    }

    /// The whole conversation including everything compaction dropped from the model's
    /// context: like [`Self::active_path`], but a compact boundary is followed through
    /// its `logicalParentUuid` back to the pre-compaction leaf. Boundary records stay in
    /// the result so the UI can draw the seam.
    pub fn full_path(&self) -> Vec<&TranscriptRecord> {
        self.walk(true)
    }

    pub fn compact_boundaries(&self) -> Vec<&TranscriptRecord> {
        self.records
            .iter()
            .filter(|record| record.subtype.as_deref() == Some(COMPACT_BOUNDARY_SUBTYPE))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The uuids whose record has been replaced by a later record carrying the same
    /// uuid, in the order the replacements happened. A caller that caches anything
    /// derived from a record drops these keys: absorbing is otherwise append-only, so
    /// this is the only way a record it has already seen can have changed.
    pub fn overwritten_uuids(&self) -> &[String] {
        &self.overwritten_uuids
    }

    fn walk(&self, follow_logical_parent: bool) -> Vec<&TranscriptRecord> {
        let mut path = Vec::new();
        let mut visited: HashSet<&str> = HashSet::default();
        let mut cursor = self.leaf_uuid.as_deref();

        while let Some(uuid) = cursor {
            // A corrupted or hand-edited file can contain a parent cycle; revisiting a
            // uuid is the only reliable way to notice one.
            if !visited.insert(uuid) {
                break;
            }

            // The parent may be missing because the tail of the file has not been read
            // yet, or because the file is damaged. Returning the partial path is more
            // useful than returning nothing.
            let Some(record) = self
                .index_by_uuid
                .get(uuid)
                .and_then(|index| self.records.get(*index))
            else {
                break;
            };

            let next = if follow_logical_parent
                && record.subtype.as_deref() == Some(COMPACT_BOUNDARY_SUBTYPE)
            {
                record
                    .logical_parent_uuid
                    .as_deref()
                    .or(record.parent_uuid.as_deref())
            } else {
                record.parent_uuid.as_deref()
            };

            // Skipped rather than pushed: a sidechain record must never appear in a main
            // thread's path, but its ancestors are still worth keeping if a chain runs
            // through one. When the conversation being read is one agent's own, the same
            // records are the conversation.
            if self.reads_sidechain || !record.is_sidechain {
                path.push(record);
            }

            cursor = next;
        }

        path.reverse();
        path
    }
}

fn attachment_object(record: &TranscriptRecord) -> Option<&Value> {
    record.raw.get("attachment")
}

fn attachment_of_type<'record>(
    record: &'record TranscriptRecord,
    attachment_type: &str,
) -> Option<&'record Value> {
    let attachment = attachment_object(record)?;
    (attachment.get("type").and_then(Value::as_str) == Some(attachment_type)).then_some(attachment)
}

/// The count inside `<total_tokens>…`, however it was written.
///
/// Stopping at the first character that is not a digit reads "15,000,000" as 15, and a
/// count the toolbar states as a fact is worse wrong than missing, so the groupings and
/// the magnitude suffix Claude Code may write are read rather than cut off.
fn parse_tokens_left(text: &str) -> Option<u64> {
    let start = text.find(TOTAL_TOKENS_OPEN)?;
    let after_open = text
        .get(start.saturating_add(TOTAL_TOKENS_OPEN.len())..)?
        .trim_start();

    let mut digits = String::new();
    let mut after_digits = "";
    for (index, character) in after_open.char_indices() {
        if character.is_ascii_digit() {
            digits.push(character);
            continue;
        }
        if (character == ',' || character == '_') && !digits.is_empty() {
            continue;
        }
        after_digits = after_open.get(index..).unwrap_or_default();
        break;
    }
    if digits.is_empty() {
        return None;
    }

    // Only a suffix written against the digits counts, so "15 minutes" is 15 and not 15
    // million.
    let multiplier = match after_digits.chars().next() {
        Some('K' | 'k') => 1_000,
        Some('M' | 'm') => 1_000_000,
        Some('G' | 'g') => 1_000_000_000,
        _ => 1,
    };
    digits.parse::<u64>().ok()?.checked_mul(multiplier)
}

fn parse_auto_mode(record: &TranscriptRecord) -> Option<AutoModeFlags> {
    let attachment = attachment_of_type(record, AUTO_MODE_ATTACHMENT)?;
    let flag = |key: &str| {
        attachment
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    Some(AutoModeFlags {
        bash_first: flag("bashFirst"),
        steer_only: flag("steerOnly"),
        bypass: flag("bypass"),
    })
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(json: &str) -> TranscriptRecord {
        match parse_record(json) {
            Ok(Some(record)) => record,
            Ok(None) => panic!("expected a record, got a blank line for: {json}"),
            Err(error) => panic!("expected {json} to parse, got error: {error:#}"),
        }
    }

    fn transcript(lines: &[String]) -> Transcript {
        let mut transcript = Transcript::new();
        transcript.absorb(lines.iter().map(|line| record(line)));
        transcript
    }

    /// The queue log is the only statement of what is waiting: no record says what the
    /// queue holds, so every operation has to be applied or the answer drifts and stays
    /// wrong for the rest of the session.
    #[test]
    fn the_queue_log_played_forward_is_what_is_waiting() {
        let transcript = transcript(&[
            r#"{"type":"queue-operation","operation":"enqueue","content":"first"}"#.to_string(),
            r#"{"type":"queue-operation","operation":"enqueue","content":"second"}"#.to_string(),
            r#"{"type":"queue-operation","operation":"enqueue","content":"third"}"#.to_string(),
            // Names the one that left, which need not be the oldest.
            r#"{"type":"queue-operation","operation":"remove","content":"second",
"reason":"absorbed_mid_turn"}"#
                .to_string(),
            // Carries no text: the oldest went into a turn.
            r#"{"type":"queue-operation","operation":"dequeue"}"#.to_string(),
        ]);

        assert_eq!(
            transcript.queued_messages().iter().collect::<Vec<_>>(),
            vec![&SharedString::from("third")],
            "first was dequeued into a turn and second was removed by name"
        );
    }

    #[test]
    fn taking_the_queue_whole_empties_it() {
        let transcript = transcript(&[
            r#"{"type":"queue-operation","operation":"enqueue","content":"a"}"#.to_string(),
            r#"{"type":"queue-operation","operation":"enqueue","content":"b"}"#.to_string(),
            r#"{"type":"queue-operation","operation":"popAll","content":"a and b"}"#.to_string(),
        ]);

        assert!(
            transcript.queued_messages().is_empty(),
            "popAll took both, so nothing is still waiting: {:?}",
            transcript.queued_messages()
        );
    }

    /// An operation this code does not know cannot be applied, and applying it as a
    /// guess would leave the queue wrong for every message after it.
    #[test]
    fn an_unknown_queue_operation_leaves_the_queue_alone() {
        let transcript = transcript(&[
            r#"{"type":"queue-operation","operation":"enqueue","content":"kept"}"#.to_string(),
            r#"{"type":"queue-operation","operation":"reshuffle","content":"kept"}"#.to_string(),
        ]);

        assert_eq!(
            transcript.queued_messages().iter().collect::<Vec<_>>(),
            vec![&SharedString::from("kept")]
        );
    }

    fn uuids(path: &[&TranscriptRecord]) -> Vec<String> {
        path.iter()
            .map(|record| record.uuid.clone().unwrap_or_else(|| "<none>".to_string()))
            .collect()
    }

    fn expected(uuids: &[&str]) -> Vec<String> {
        uuids.iter().map(|uuid| (*uuid).to_string()).collect()
    }

    fn message(uuid: &str, parent: Option<&str>) -> String {
        let parent = match parent {
            Some(parent) => format!("\"{parent}\""),
            None => "null".to_string(),
        };
        format!(r#"{{"type":"user","uuid":"{uuid}","parentUuid":{parent}}}"#)
    }

    fn boundary(uuid: &str, logical_parent: &str, trigger: &str) -> String {
        format!(
            r#"{{"type":"system","subtype":"compact_boundary","uuid":"{uuid}","parentUuid":null,"logicalParentUuid":"{logical_parent}","compactMetadata":{{"trigger":"{trigger}","preTokens":792090,"postTokens":16995}}}}"#
        )
    }

    fn summary(uuid: &str, parent: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"{uuid}","parentUuid":"{parent}","isCompactSummary":true}}"#
        )
    }

    /// The context an answer measured stops being what the session carries the moment a
    /// compaction drops the conversation behind it. Read from the compaction until the
    /// next answer measures the whole request again, because a figure six times too large
    /// is what a reader would otherwise be told the session is holding.
    #[test]
    fn a_compaction_says_what_context_is_left_until_the_next_answer() {
        let answer = |uuid: &str, parent: &str, context: u64| {
            format!(
                r#"{{"type":"assistant","uuid":"{uuid}","parentUuid":"{parent}","message":{{"model":"claude-opus-5","usage":{{"input_tokens":2,"cache_read_input_tokens":{context},"output_tokens":10}}}}}}"#
            )
        };

        let before = transcript(&[answer("a", "start", 680_000)]);
        assert_eq!(before.spend().context_tokens, 680_002);
        assert!(
            !before.spend().context_is_post_compaction,
            "an answer measured it, so nothing is estimated"
        );

        let compacted = transcript(&[
            answer("a", "start", 680_000),
            boundary("b", "a", "manual"),
            summary("c", "b"),
        ]);
        assert_eq!(
            compacted.spend().context_tokens,
            16995,
            "the compaction's own postTokens, not the 680K the last answer was given"
        );
        assert!(
            compacted.spend().context_is_post_compaction,
            "and it is marked, because it counts the kept conversation and not the whole \
             request the next answer will be given"
        );

        let answered_since = transcript(&[
            answer("a", "start", 680_000),
            boundary("b", "a", "manual"),
            summary("c", "b"),
            answer("d", "c", 94_576),
        ]);
        assert_eq!(
            answered_since.spend().context_tokens,
            94_578,
            "a measured answer supersedes the compaction's figure"
        );
        assert!(!answered_since.spend().context_is_post_compaction);
    }

    #[test]
    fn linear_path_is_oldest_to_newest() {
        let transcript = transcript(&[
            message("m1", None),
            message("m2", Some("m1")),
            message("m3", Some("m2")),
            message("m4", Some("m3")),
            message("m5", Some("m4")),
        ]);

        assert_eq!(
            uuids(&transcript.active_path()),
            expected(&["m1", "m2", "m3", "m4", "m5"])
        );
    }

    #[test]
    fn abandoned_branch_is_excluded() {
        let transcript = transcript(&[
            message("a", None),
            message("b", Some("a")),
            message("c", Some("a")),
        ]);

        assert_eq!(
            uuids(&transcript.active_path()),
            expected(&["a", "c"]),
            "the later child of `a` must win, and the abandoned branch `b` must not appear"
        );
    }

    #[test]
    fn single_compaction_splits_active_and_full_path() {
        let transcript = transcript(&[
            message("root1", None),
            message("m1", Some("root1")),
            message("m2", Some("m1")),
            boundary("bound1", "m2", "manual"),
            summary("sum1", "bound1"),
            message("m3", Some("sum1")),
            message("m4", Some("m3")),
        ]);

        assert_eq!(
            uuids(&transcript.active_path()),
            expected(&["bound1", "sum1", "m3", "m4"]),
            "compaction severs parentUuid, so pre-compaction records are out of context"
        );
        assert_eq!(
            uuids(&transcript.full_path()),
            expected(&["root1", "m1", "m2", "bound1", "sum1", "m3", "m4"])
        );
        assert_eq!(
            uuids(&transcript.compact_boundaries()),
            expected(&["bound1"])
        );
    }

    #[test]
    fn two_compactions_produce_three_roots() {
        let transcript = transcript(&[
            message("root1", None),
            message("m1", Some("root1")),
            boundary("bound1", "m1", "manual"),
            summary("sum1", "bound1"),
            message("m2", Some("sum1")),
            boundary("bound2", "m2", "auto"),
            summary("sum2", "bound2"),
            message("m3", Some("sum2")),
        ]);

        assert_eq!(
            uuids(&transcript.active_path()),
            expected(&["bound2", "sum2", "m3"])
        );
        assert_eq!(
            uuids(&transcript.full_path()),
            expected(&[
                "root1", "m1", "bound1", "sum1", "m2", "bound2", "sum2", "m3"
            ])
        );
        assert_eq!(
            uuids(&transcript.compact_boundaries()),
            expected(&["bound1", "bound2"])
        );
    }

    #[test]
    fn unknown_compact_trigger_is_preserved_verbatim() {
        for trigger in ["auto", "something-brand-new"] {
            let parsed = record(&boundary("bound1", "m1", trigger));
            let metadata = parsed
                .compact_metadata
                .as_ref()
                .unwrap_or_else(|| panic!("expected compactMetadata for trigger {trigger}"));

            assert_eq!(
                metadata.trigger, trigger,
                "trigger must be kept as-is; got {:?}, expected {:?}",
                metadata.trigger, trigger
            );
            assert_eq!(metadata.pre_tokens, Some(792090));
            assert_eq!(metadata.post_tokens, Some(16995));
        }
    }

    #[test]
    fn missing_compact_trigger_becomes_empty_string() {
        let parsed = record(
            r#"{"type":"system","subtype":"compact_boundary","uuid":"b","parentUuid":null,"compactMetadata":{"preTokens":5}}"#,
        );
        let metadata = parsed
            .compact_metadata
            .as_ref()
            .unwrap_or_else(|| panic!("expected compactMetadata"));

        assert_eq!(metadata.trigger, "");
        assert_eq!(metadata.pre_tokens, Some(5));
        assert_eq!(metadata.post_tokens, None);
    }

    #[test]
    fn message_content_may_be_string_or_array() {
        let as_string = record(
            r#"{"type":"user","uuid":"a","parentUuid":null,"message":{"role":"user","content":"hello"}}"#,
        );
        assert_eq!(
            as_string.raw["message"]["content"],
            Value::String("hello".to_string())
        );

        let as_array = record(
            r#"{"type":"assistant","uuid":"b","parentUuid":"a","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}"#,
        );
        let content = as_array.raw["message"]["content"]
            .as_array()
            .unwrap_or_else(|| {
                panic!(
                    "expected an array, got {:?}",
                    as_array.raw["message"]["content"]
                )
            });
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], Value::String("hi".to_string()));
    }

    #[test]
    fn sidechain_records_are_absorbed_but_never_shown() {
        let mut transcript = Transcript::new();
        transcript.absorb([
            record(&message("a", None)),
            record(&message("b", Some("a"))),
            record(r#"{"type":"assistant","uuid":"side1","parentUuid":"b","isSidechain":true}"#),
        ]);

        assert!(!transcript.is_empty());
        assert_eq!(
            uuids(&transcript.active_path()),
            expected(&["a", "b"]),
            "a sidechain record must not become the leaf nor appear in the path"
        );
        assert_eq!(uuids(&transcript.full_path()), expected(&["a", "b"]));
    }

    #[test]
    fn a_sidechain_record_must_not_become_the_leaf() {
        // A sidechain that forks off an earlier record: choosing it as the leaf walks a
        // different branch and silently drops the newest main-thread record.
        let mut forked = Transcript::new();
        forked.absorb([
            record(&message("a", None)),
            record(&message("b", Some("a"))),
            record(r#"{"type":"assistant","uuid":"s1","parentUuid":"a","isSidechain":true}"#),
        ]);
        assert_eq!(
            uuids(&forked.active_path()),
            expected(&["a", "b"]),
            "the leaf must stay on the main thread; a sidechain leaf would walk back through `a` and lose `b`"
        );

        // The shape real transcripts have: a sub-agent conversation is a separate tree
        // whose root has no parent, and it is written after the main thread's last
        // record. Choosing its last record as the leaf yields an empty path, because
        // every record on the way back is skipped as a sidechain.
        let mut separate = Transcript::new();
        separate.absorb([
            record(&message("a", None)),
            record(&message("b", Some("a"))),
            record(r#"{"type":"user","uuid":"s1","parentUuid":null,"isSidechain":true}"#),
            record(r#"{"type":"assistant","uuid":"s2","parentUuid":"s1","isSidechain":true}"#),
        ]);
        assert_eq!(
            uuids(&separate.active_path()),
            expected(&["a", "b"]),
            "a trailing sidechain tree must leave the main thread's leaf alone"
        );
        assert_eq!(uuids(&separate.full_path()), expected(&["a", "b"]));
    }

    /// A subagent's transcript is a file of nothing but sidechain records, so the
    /// reading that keeps a sub-agent out of the main thread hides the whole of it. The
    /// mode is the difference between the two readings, and nothing else about it.
    #[test]
    fn a_transcript_that_is_itself_one_agents_conversation_walks_its_own_records() {
        let lines = [
            r#"{"type":"user","uuid":"s1","parentUuid":null,"isSidechain":true}"#,
            r#"{"type":"assistant","uuid":"s2","parentUuid":"s1","isSidechain":true}"#,
            r#"{"type":"user","uuid":"s3","parentUuid":"s2","isSidechain":true}"#,
        ];

        let mut as_main_thread = Transcript::new();
        as_main_thread.absorb(lines.iter().map(|line| record(line)));
        assert_eq!(
            uuids(&as_main_thread.active_path()),
            expected(&[]),
            "read as a main thread these records belong to a sub-agent and must not \
             appear at all"
        );

        let mut as_agents_own = Transcript::for_sidechain();
        as_agents_own.absorb(lines.iter().map(|line| record(line)));
        assert_eq!(
            uuids(&as_agents_own.active_path()),
            expected(&["s1", "s2", "s3"]),
            "read as the agent's own conversation the whole parentUuid chain is the \
             conversation"
        );
        assert_eq!(
            uuids(&as_agents_own.full_path()),
            expected(&["s1", "s2", "s3"])
        );
    }

    /// The mode changes which records are a leaf and which are walked; it changes nothing
    /// about how the chain itself is followed.
    #[test]
    fn an_agents_conversation_is_still_read_newest_first_along_parent_uuid() {
        let mut transcript = Transcript::for_sidechain();
        transcript.absorb([
            record(r#"{"type":"user","uuid":"s1","parentUuid":null,"isSidechain":true}"#),
            // An abandoned branch: the later child of `s1` is the one the agent kept.
            record(r#"{"type":"assistant","uuid":"dead","parentUuid":"s1","isSidechain":true}"#),
            record(r#"{"type":"assistant","uuid":"s2","parentUuid":"s1","isSidechain":true}"#),
        ]);

        assert_eq!(
            uuids(&transcript.active_path()),
            expected(&["s1", "s2"]),
            "the abandoned branch must be left out of an agent's conversation too"
        );
    }

    #[test]
    fn parent_cycle_terminates() {
        let mut transcript = Transcript::new();
        transcript.absorb([
            record(&message("a", Some("b"))),
            record(&message("b", Some("a"))),
        ]);

        let active = transcript.active_path();
        assert!(
            active.len() <= 2,
            "traversal of a 2-record cycle must visit at most 2 records, visited {}: {:?}",
            active.len(),
            uuids(&active)
        );
        assert_eq!(uuids(&active), expected(&["a", "b"]));

        let full = transcript.full_path();
        assert!(
            full.len() <= 2,
            "full_path must terminate too, visited {}: {:?}",
            full.len(),
            uuids(&full)
        );
    }

    #[test]
    fn missing_parent_yields_partial_path() {
        let transcript = transcript(&[message("b", Some("ghost")), message("c", Some("b"))]);

        assert_eq!(
            uuids(&transcript.active_path()),
            expected(&["b", "c"]),
            "traversal must stop at the unknown parent and return what it reached"
        );
        assert_eq!(uuids(&transcript.full_path()), expected(&["b", "c"]));
    }

    #[test]
    fn unknown_record_type_is_kept_whole() {
        let parsed = record(
            r#"{"type":"brand-new-thing","uuid":"x","parentUuid":null,"futureField":{"nested":[1,2]},"count":7}"#,
        );

        assert_eq!(parsed.record_type, "brand-new-thing");
        assert_eq!(parsed.subtype, None);
        assert_eq!(parsed.raw["futureField"]["nested"][1], Value::from(2));
        assert_eq!(parsed.raw["count"], Value::from(7));
        assert_eq!(parsed.raw["type"], Value::from("brand-new-thing"));
    }

    #[test]
    fn incremental_absorb_matches_single_absorb() {
        let lines = [
            message("root1", None),
            message("m1", Some("root1")),
            boundary("bound1", "m1", "manual"),
            summary("sum1", "bound1"),
            message("m2", Some("sum1")),
            message("m3", Some("m2")),
        ];

        let all_at_once = transcript(&lines);

        let mut incremental = Transcript::new();
        incremental.absorb(lines[..3].iter().map(|line| record(line)));
        let partial_active = uuids(&incremental.active_path());
        assert_eq!(partial_active, expected(&["bound1"]));
        assert_eq!(
            uuids(&incremental.full_path()),
            expected(&["root1", "m1", "bound1"])
        );

        incremental.absorb(lines[3..].iter().map(|line| record(line)));

        assert_eq!(
            uuids(&incremental.active_path()),
            uuids(&all_at_once.active_path())
        );
        assert_eq!(
            uuids(&incremental.full_path()),
            uuids(&all_at_once.full_path())
        );
    }

    #[test]
    fn blank_lines_and_malformed_json() {
        for line in ["", "   ", "\t\n"] {
            match parse_record(line) {
                Ok(None) => {}
                Ok(Some(parsed)) => panic!(
                    "expected None for blank line {line:?}, got record of type {:?}",
                    parsed.record_type
                ),
                Err(error) => panic!("expected Ok(None) for blank line {line:?}, got {error:#}"),
            }
        }

        for line in [
            "{",
            "not json",
            r#"{"uuid":"a","parentUuid":null}"#, // `type` is required
        ] {
            assert!(
                parse_record(line).is_err(),
                "expected an error for {line:?}, got a parsed record"
            );
        }
    }

    #[test]
    fn empty_transcript_has_empty_paths() {
        let transcript = Transcript::new();

        assert!(transcript.is_empty());
        assert!(transcript.active_path().is_empty());
        assert!(transcript.full_path().is_empty());
        assert!(transcript.compact_boundaries().is_empty());
    }

    #[test]
    fn repeated_uuid_is_overwritten_in_place() {
        let mut transcript = Transcript::new();
        transcript.absorb([
            record(&message("a", None)),
            record(&boundary("bound1", "a", "manual")),
            record(&boundary("bound1", "a", "auto")),
        ]);

        let boundaries = transcript.compact_boundaries();
        assert_eq!(
            uuids(&boundaries),
            expected(&["bound1"]),
            "re-absorbing a uuid must replace the record, not duplicate it"
        );
        let trigger = boundaries
            .first()
            .and_then(|record| record.compact_metadata.as_ref())
            .map(|metadata| metadata.trigger.as_str());
        assert_eq!(trigger, Some("auto"));
    }

    #[test]
    fn records_without_uuid_never_become_leaf() {
        let mut transcript = Transcript::new();
        transcript.absorb([
            record(&message("a", None)),
            record(&message("b", Some("a"))),
            record(r#"{"type":"mode","mode":"plan"}"#),
            record(r#"{"type":"last-prompt","prompt":"hello"}"#),
        ]);

        assert_eq!(uuids(&transcript.active_path()), expected(&["a", "b"]));
    }

    #[test]
    fn overwriting_a_record_reports_its_uuid_once_per_replacement() {
        let mut transcript = Transcript::new();
        transcript.absorb([
            record(&message("a", None)),
            record(&message("b", Some("a"))),
        ]);
        assert!(
            transcript.overwritten_uuids().is_empty(),
            "absorbing new records must report no overwrite, got {:?}",
            transcript.overwritten_uuids()
        );

        transcript.absorb([record(&message("b", Some("a")))]);
        assert_eq!(
            transcript.overwritten_uuids(),
            expected(&["b"]),
            "the uuid whose record was replaced must be reported"
        );

        // A record without a uuid is appended, never replaced, so it can never be
        // reported: nothing can be keyed on it.
        transcript.absorb([record(r#"{"type":"mode"}"#), record(r#"{"type":"mode"}"#)]);
        assert_eq!(transcript.overwritten_uuids(), expected(&["b"]));
    }

    #[test]
    fn uuid_less_permission_mode_title_and_bridge_are_the_newest() {
        let transcript = transcript(&[
            r#"{"type":"permission-mode","permissionMode":"default"}"#.to_string(),
            r#"{"type":"mode","mode":"normal"}"#.to_string(),
            r#"{"type":"ai-title","aiTitle":"first title"}"#.to_string(),
            r#"{"type":"bridge-session","bridgeSessionId":"session_old"}"#.to_string(),
            r#"{"type":"permission-mode","permissionMode":"auto"}"#.to_string(),
            r#"{"type":"ai-title","aiTitle":"later title"}"#.to_string(),
            r#"{"type":"bridge-session","bridgeSessionId":"session_01abc"}"#.to_string(),
        ]);

        assert_eq!(transcript.permission_mode(), Some("auto"));
        assert_eq!(transcript.ai_title(), Some("later title"));
        assert_eq!(transcript.bridge_session_id(), Some("session_01abc"));
    }

    #[test]
    fn tokens_left_parses_the_newest_total_tokens_reminder() {
        let transcript = transcript(&[
            r#"{"type":"attachment","uuid":"a","attachment":{"type":"total_tokens_reminder","text":"<total_tokens>100 tokens left</total_tokens>"}}"#.to_string(),
            r#"{"type":"attachment","uuid":"b","attachment":{"type":"total_tokens_reminder","text":"<total_tokens>15000000 tokens left</total_tokens>"}}"#.to_string(),
        ]);
        assert_eq!(transcript.tokens_left(), Some(15_000_000));
    }

    #[test]
    fn a_malformed_tokens_reminder_yields_none() {
        let transcript = transcript(&[
            r#"{"type":"attachment","uuid":"a","attachment":{"type":"total_tokens_reminder","text":"no tags here"}}"#.to_string(),
        ]);
        assert_eq!(transcript.tokens_left(), None);
    }

    #[test]
    fn auto_mode_flags_are_read_from_the_newest_attachment() {
        let transcript = transcript(&[
            r#"{"type":"attachment","uuid":"a","attachment":{"type":"auto_mode","bashFirst":false,"steerOnly":false,"bypass":true}}"#.to_string(),
            r#"{"type":"attachment","uuid":"b","attachment":{"type":"auto_mode","bashFirst":true,"bashFirstSteer":"strict","steerOnly":true,"bypass":false}}"#.to_string(),
        ]);
        assert_eq!(
            transcript.auto_mode(),
            Some(AutoModeFlags {
                bash_first: true,
                steer_only: true,
                bypass: false,
            })
        );
    }

    #[test]
    fn a_grouped_or_suffixed_token_count_is_never_read_as_its_first_digits() {
        // A count the panel states as a fact is worse wrong than missing: "15,000,000"
        // read as 15 puts "15 tokens left" in the toolbar of a session that has 15M.
        for (text, expected) in [
            (
                "<total_tokens>15000000 tokens left</total_tokens>",
                Some(15_000_000),
            ),
            (
                "<total_tokens>15,000,000 tokens left</total_tokens>",
                Some(15_000_000),
            ),
            (
                "<total_tokens>15_000_000 tokens left</total_tokens>",
                Some(15_000_000),
            ),
            (
                "<total_tokens> 15000000 tokens left</total_tokens>",
                Some(15_000_000),
            ),
            (
                "<total_tokens>750K tokens left</total_tokens>",
                Some(750_000),
            ),
            (
                "<total_tokens>15M tokens left</total_tokens>",
                Some(15_000_000),
            ),
            ("<total_tokens>no digits</total_tokens>", None),
            ("nothing at all", None),
        ] {
            assert_eq!(
                parse_tokens_left(text),
                expected,
                "for {text:?} expected {expected:?}, got {:?}",
                parse_tokens_left(text)
            );
        }
    }

    #[test]
    fn a_replaced_reminder_is_not_read_as_a_token_count() {
        let mut transcript = Transcript::new();
        transcript.absorb([record(
            r#"{"type":"attachment","uuid":"a","attachment":{"type":"total_tokens_reminder","text":"<total_tokens>15000000 tokens left</total_tokens>"}}"#,
        )]);
        assert_eq!(transcript.tokens_left(), Some(15_000_000));

        // `absorb` replaces a record in place when its uuid comes back, and what comes
        // back need not be the attachment the index was noted for.
        transcript.absorb([record(
            r#"{"type":"attachment","uuid":"a","attachment":{"type":"environment","text":"<total_tokens>7 tokens left</total_tokens>"}}"#,
        )]);
        assert_eq!(
            transcript.tokens_left(),
            None,
            "a count must come from a total_tokens_reminder, got {:?}",
            transcript.tokens_left()
        );
    }

    #[test]
    fn cost_state_exposes_lines_and_model_usage() {
        let transcript = transcript(&[r#"{"type":"cost-state","totalCostUSD":1.5,"totalLinesAdded":156,"totalLinesRemoved":23,"modelUsage":{"claude-opus-5":{"inputTokens":10,"outputTokens":20,"costUSD":1.5}}}"#.to_string()]);
        let reported = transcript
            .spend()
            .reported
            .expect("a cost-state record reports a total");
        assert_eq!(reported.lines_added, 156);
        assert_eq!(reported.lines_removed, 23);
        assert_eq!(
            reported
                .model_usage
                .get(&SharedString::from("claude-opus-5"))
                .map(|usage| usage.input_tokens),
            Some(10)
        );
    }

    #[test]
    fn a_malformed_model_usage_map_is_empty() {
        let transcript = transcript(&[
            r#"{"type":"cost-state","totalCostUSD":1.0,"modelUsage":["not","an","object"]}"#
                .to_string(),
        ]);
        let reported = transcript
            .spend()
            .reported
            .expect("a cost-state record reports a total");
        assert!(
            reported.model_usage.is_empty(),
            "a non-object modelUsage must not panic, got {:?}",
            reported.model_usage
        );
    }
}

/// What a session has spent, and how the model answering it is configured.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Spend {
    /// Totalled over every answer in the file.
    pub usage: Usage,
    /// What Claude Code itself reported, when it has written a `cost-state` record for
    /// this session yet. Shown in preference to the total derived from `usage`.
    pub reported: Option<ReportedCost>,
    /// How many answers that total covers.
    pub answers: u64,
    /// The context the newest answer was given, which is the size the next one starts
    /// from. After a compaction that no answer has followed yet, what the compaction says
    /// it kept; see `context_is_post_compaction`.
    pub context_tokens: u64,
    /// Whether `context_tokens` came from a compaction rather than from an answer. Said
    /// because the two count different things: an answer's usage is the whole request,
    /// and a compaction's figure is the conversation it kept, without the system prompt,
    /// the tool definitions or the skills that the next request re-sends. It reads far
    /// below what that request will measure, so it is marked where it is shown.
    pub context_is_post_compaction: bool,
    /// Read from the newest answer that named one, so that a session whose model or
    /// effort changed part-way reports what it is on now.
    pub model: Option<SharedString>,
    pub effort: Option<SharedString>,
}

/// What Claude Code itself says the session has cost, read from its `cost-state` record.
///
/// Preferred over any total derived from token counts: the CLI knows the rates for every
/// model it ran, including ones this code has no rates for. `has_unknown_model_cost`
/// carries its own admission that a figure is incomplete, and is passed on rather than
/// hidden.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReportedCost {
    pub total_usd: f64,
    pub has_unknown_model_cost: bool,
    /// Milliseconds spent waiting on the API, retries included.
    pub api_duration_ms: u64,
    /// Milliseconds spent running tools.
    pub tool_duration_ms: u64,
    pub lines_added: u64,
    pub lines_removed: u64,
    /// Per-model usage from `modelUsage` on the same record. Empty when the field is
    /// missing or not an object.
    pub model_usage: HashMap<SharedString, ModelUsage>,
}

/// One model's slice of a `cost-state` `modelUsage` map.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cost_usd: f64,
}

impl ReportedCost {
    fn from_record(raw: &Value) -> Option<Self> {
        let total_usd = raw.get("totalCostUSD").and_then(Value::as_f64)?;
        let number = |key: &str| raw.get(key).and_then(Value::as_u64).unwrap_or(0);

        Some(Self {
            total_usd,
            has_unknown_model_cost: raw
                .get("hasUnknownModelCost")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            api_duration_ms: number("totalAPIDuration"),
            tool_duration_ms: number("totalToolDuration"),
            lines_added: number("totalLinesAdded"),
            lines_removed: number("totalLinesRemoved"),
            model_usage: parse_model_usage(raw.get("modelUsage")),
        })
    }
}

fn parse_model_usage(value: Option<&Value>) -> HashMap<SharedString, ModelUsage> {
    let mut usage = HashMap::default();
    let Some(models) = value.and_then(Value::as_object) else {
        return usage;
    };
    for (model, entry) in models {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let number = |keys: &[&str]| {
            keys.iter()
                .find_map(|key| entry.get(*key).and_then(Value::as_u64))
                .unwrap_or(0)
        };
        let cost = entry
            .get("costUSD")
            .or_else(|| entry.get("costUsd"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        usage.insert(
            SharedString::from(model.clone()),
            ModelUsage {
                input_tokens: number(&["inputTokens", "input_tokens"]),
                output_tokens: number(&["outputTokens", "output_tokens"]),
                cache_read_tokens: number(&["cacheReadInputTokens", "cache_read_input_tokens"]),
                cache_creation_tokens: number(&[
                    "cacheCreationInputTokens",
                    "cache_creation_input_tokens",
                ]),
                cost_usd: cost,
            },
        );
    }
    usage
}
