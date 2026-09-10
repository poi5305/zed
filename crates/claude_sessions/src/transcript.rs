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

use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};
use serde::Deserialize;
use serde_json::Value;

const COMPACT_BOUNDARY_SUBTYPE: &str = "compact_boundary";

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
    index_by_uuid: HashMap<String, usize>,
    leaf_uuid: Option<String>,
    overwritten_uuids: Vec<String>,
    /// Whether this transcript is itself one sub-agent's conversation rather than a
    /// session's own thread; see [`Self::for_sidechain`].
    reads_sidechain: bool,
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
            index_by_uuid: HashMap::default(),
            leaf_uuid: None,
            overwritten_uuids: Vec::new(),
            reads_sidechain,
        }
    }

    /// Absorbs records in file order. Safe to call repeatedly as the file grows.
    pub fn absorb(&mut self, records: impl IntoIterator<Item = TranscriptRecord>) {
        for record in records {
            let Some(uuid) = record.uuid.clone() else {
                // Records such as `mode`, `last-prompt` and `ai-title` have no uuid, so
                // nothing can reference them and they can never be a leaf.
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
            match existing_index.and_then(|index| self.records.get_mut(index)) {
                Some(slot) => *slot = record,
                None => {
                    self.index_by_uuid.insert(uuid, self.records.len());
                    self.records.push(record);
                }
            }
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
}
