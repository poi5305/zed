#[cfg(test)]
mod blind_transcript_tests {
    use crate::transcript::*;
    use serde_json::json;

    fn parse_one(line: &str) -> TranscriptRecord {
        match parse_record(line) {
            Ok(Some(record)) => record,
            Ok(None) => panic!("expected a record from {line}, got Ok(None)"),
            Err(error) => panic!("expected a record from {line}, got Err({error})"),
        }
    }

    fn record(value: serde_json::Value) -> TranscriptRecord {
        parse_one(&value.to_string())
    }

    /// A plain, non-sidechain message record.
    fn message(uuid: &str, parent_uuid: Option<&str>) -> TranscriptRecord {
        record(json!({
            "type": "assistant",
            "uuid": uuid,
            "parentUuid": parent_uuid,
            "message": { "role": "assistant", "content": "hello" },
        }))
    }

    fn sidechain(uuid: &str, parent_uuid: Option<&str>) -> TranscriptRecord {
        record(json!({
            "type": "assistant",
            "uuid": uuid,
            "parentUuid": parent_uuid,
            "isSidechain": true,
            "message": { "role": "assistant", "content": "sub-agent" },
        }))
    }

    fn boundary(
        uuid: &str,
        parent_uuid: Option<&str>,
        logical_parent_uuid: Option<&str>,
    ) -> TranscriptRecord {
        record(json!({
            "type": "system",
            "subtype": "compact_boundary",
            "uuid": uuid,
            "parentUuid": parent_uuid,
            "logicalParentUuid": logical_parent_uuid,
            "isCompactSummary": true,
            "compactMetadata": { "trigger": "auto", "preTokens": 150000, "postTokens": 20000 },
        }))
    }

    /// A record with no uuid at all, like the mode / last-prompt / ai-title lines.
    fn metadata_line(record_type: &str) -> TranscriptRecord {
        record(json!({ "type": record_type, "payload": { "anything": [1, 2, 3] } }))
    }

    fn uuids(records: &[&TranscriptRecord]) -> Vec<String> {
        records
            .iter()
            .map(|record| record.uuid.clone().unwrap_or_default())
            .collect()
    }

    fn transcript_of(records: Vec<TranscriptRecord>) -> Transcript {
        let mut transcript = Transcript::new();
        transcript.absorb(records);
        transcript
    }

    // ---------- parse_record ----------

    #[test]
    fn an_empty_line_is_skipped() {
        let result = parse_record("");
        match result {
            Ok(None) => {}
            Ok(Some(record)) => panic!(
                "expected Ok(None) for an empty line, got a record of type {:?}",
                record.record_type
            ),
            Err(error) => panic!("expected Ok(None) for an empty line, got Err({error})"),
        }
    }

    #[test]
    fn a_whitespace_only_line_is_skipped() {
        for line in ["   ", "\t", " \t \r\n", "\n"] {
            match parse_record(line) {
                Ok(None) => {}
                Ok(Some(record)) => panic!(
                    "expected Ok(None) for whitespace line {:?}, got a record of type {:?}",
                    line, record.record_type
                ),
                Err(error) => {
                    panic!("expected Ok(None) for whitespace line {line:?}, got Err({error})")
                }
            }
        }
    }

    #[test]
    fn json_null_is_an_error_because_it_carries_no_type() {
        // Spec gap: "null" is valid JSON but not an object. `type` is a required field, so the
        // only spec-consistent outcome is Err (the caller skips the line either way).
        let result = parse_record("null");
        assert!(
            result.is_err(),
            "expected Err for the literal null, got {:?}",
            result.map(|record| record.map(|record| record.record_type))
        );
    }

    #[test]
    fn a_json_array_is_an_error() {
        let result = parse_record("[]");
        assert!(
            result.is_err(),
            "expected Err for an array line, got {:?}",
            result.map(|record| record.map(|record| record.record_type))
        );
    }

    #[test]
    fn an_object_without_type_is_an_error() {
        let result = parse_record("{}");
        assert!(
            result.is_err(),
            "expected Err for an object missing the required type field, got {:?}",
            result.map(|record| record.map(|record| record.record_type))
        );
    }

    #[test]
    fn broken_json_is_an_error() {
        let result = parse_record("{\"type\":\"user\"");
        assert!(
            result.is_err(),
            "expected Err for truncated JSON, got {:?}",
            result.map(|record| record.map(|record| record.record_type))
        );
    }

    #[test]
    fn a_bare_type_parses_with_all_defaults() {
        let parsed = parse_one("{\"type\":\"whatever-future-type\"}");
        assert_eq!(
            parsed.record_type, "whatever-future-type",
            "unknown types must be preserved verbatim"
        );
        assert_eq!(parsed.uuid, None, "uuid must default to None");
        assert_eq!(parsed.parent_uuid, None, "parent_uuid must default to None");
        assert_eq!(
            parsed.logical_parent_uuid, None,
            "logical_parent_uuid must default to None"
        );
        assert!(
            !parsed.is_sidechain,
            "is_sidechain must default to false, got {}",
            parsed.is_sidechain
        );
        assert_eq!(parsed.subtype, None, "subtype must default to None");
        assert!(
            !parsed.is_compact_summary,
            "is_compact_summary must default to false, got {}",
            parsed.is_compact_summary
        );
        assert!(
            parsed.compact_metadata.is_none(),
            "compact_metadata must default to None, got {:?}",
            parsed.compact_metadata
        );
    }

    #[test]
    fn an_explicit_null_parent_uuid_becomes_none() {
        let parsed = parse_one("{\"type\":\"user\",\"uuid\":\"a\",\"parentUuid\":null}");
        assert_eq!(
            parsed.parent_uuid, None,
            "JSON null must map to None, not to Some(\"null\"); uuid was {:?}",
            parsed.uuid
        );
    }

    #[test]
    fn unknown_top_level_fields_are_ignored_and_the_raw_line_is_kept() {
        let line = "{\"type\":\"user\",\"uuid\":\"a\",\"brandNewField\":{\"x\":[1,2]},\
                    \"anotherNewOne\":false}";
        let parsed = parse_one(line);
        assert_eq!(parsed.record_type, "user", "known fields still parse");
        assert_eq!(
            parsed.raw.get("brandNewField"),
            Some(&json!({ "x": [1, 2] })),
            "unknown fields must survive in raw; raw was {:?}",
            parsed.raw
        );
        assert_eq!(
            parsed.raw.get("anotherNewOne"),
            Some(&json!(false)),
            "unknown scalar fields must survive in raw too"
        );
    }

    #[test]
    fn an_unknown_subtype_is_preserved_and_is_not_a_compact_boundary() {
        let parsed =
            parse_one("{\"type\":\"system\",\"subtype\":\"some_future_subtype\",\"uuid\":\"a\"}");
        assert_eq!(
            parsed.subtype,
            Some("some_future_subtype".to_string()),
            "unknown subtypes must be preserved verbatim"
        );
        let transcript = transcript_of(vec![parsed]);
        assert_eq!(
            uuids(&transcript.compact_boundaries()),
            Vec::<String>::new(),
            "only subtype == \"compact_boundary\" counts as a boundary"
        );
    }

    #[test]
    fn message_content_may_be_a_string_or_an_array() {
        let as_string = parse_one(
            "{\"type\":\"user\",\"uuid\":\"a\",\"message\":{\"role\":\"user\",\
             \"content\":\"plain text\"}}",
        );
        assert_eq!(
            as_string.raw.pointer("/message/content"),
            Some(&json!("plain text")),
            "string content must be kept in raw; raw was {:?}",
            as_string.raw
        );

        let as_array = parse_one(
            "{\"type\":\"assistant\",\"uuid\":\"b\",\"message\":{\"role\":\"assistant\",\
             \"content\":[{\"type\":\"text\",\"text\":\"hi\"},\
             {\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Bash\",\"input\":{}}]}}",
        );
        let content = as_array.raw.pointer("/message/content");
        assert_eq!(
            content.and_then(|content| content.as_array()).map(Vec::len),
            Some(2),
            "array content must parse and be kept in raw; raw was {:?}",
            as_array.raw
        );
    }

    #[test]
    fn compact_metadata_keeps_an_unknown_trigger_as_a_string() {
        let parsed = parse_one(
            "{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"uuid\":\"a\",\
             \"compactMetadata\":{\"trigger\":\"some_future_trigger\",\
             \"preTokens\":1,\"postTokens\":2}}",
        );
        let metadata = match parsed.compact_metadata {
            Some(metadata) => metadata,
            None => panic!("expected compact_metadata to be Some, got None"),
        };
        assert_eq!(
            metadata.trigger, "some_future_trigger",
            "an unknown trigger must be kept verbatim, never rejected"
        );
        assert_eq!(metadata.pre_tokens, Some(1), "preTokens maps to pre_tokens");
        assert_eq!(
            metadata.post_tokens,
            Some(2),
            "postTokens maps to post_tokens"
        );
    }

    #[test]
    fn compact_metadata_without_a_trigger_uses_the_empty_string() {
        let parsed = parse_one(
            "{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"uuid\":\"a\",\
             \"compactMetadata\":{}}",
        );
        let metadata = match parsed.compact_metadata {
            Some(metadata) => metadata,
            None => panic!("expected compact_metadata to be Some for an empty object, got None"),
        };
        assert_eq!(
            metadata.trigger, "",
            "a missing trigger must become the empty string"
        );
        assert_eq!(
            metadata.pre_tokens, None,
            "a missing preTokens must become None"
        );
        assert_eq!(
            metadata.post_tokens, None,
            "a missing postTokens must become None"
        );
    }

    // ---------- Transcript basics ----------

    #[test]
    fn a_new_transcript_is_empty_and_both_paths_are_empty() {
        let transcript = Transcript::new();
        assert!(
            transcript.is_empty(),
            "a brand new transcript must report is_empty"
        );
        assert_eq!(
            uuids(&transcript.active_path()),
            Vec::<String>::new(),
            "active_path of an empty transcript"
        );
        assert_eq!(
            uuids(&transcript.full_path()),
            Vec::<String>::new(),
            "full_path of an empty transcript"
        );
        assert_eq!(
            uuids(&transcript.compact_boundaries()),
            Vec::<String>::new(),
            "compact_boundaries of an empty transcript"
        );
    }

    #[test]
    fn default_matches_new() {
        let transcript = Transcript::default();
        assert!(
            transcript.is_empty(),
            "Transcript::default() must behave like Transcript::new()"
        );
    }

    #[test]
    fn absorbing_a_record_makes_the_transcript_non_empty() {
        let transcript = transcript_of(vec![message("a", None)]);
        assert!(
            !transcript.is_empty(),
            "after absorbing one message the transcript must not be empty; active_path was {:?}",
            uuids(&transcript.active_path())
        );
    }

    #[test]
    fn records_without_a_uuid_never_become_the_leaf() {
        let transcript = transcript_of(vec![
            metadata_line("mode"),
            message("a", None),
            message("b", Some("a")),
            metadata_line("last-prompt"),
            metadata_line("ai-title"),
        ]);
        assert_eq!(
            uuids(&transcript.active_path()),
            vec!["a".to_string(), "b".to_string()],
            "the metadata lines must not disturb the leaf"
        );
    }

    #[test]
    fn a_transcript_of_only_uuid_less_records_has_empty_paths() {
        let transcript = transcript_of(vec![
            metadata_line("mode"),
            metadata_line("last-prompt"),
            metadata_line("ai-title"),
        ]);
        assert_eq!(
            uuids(&transcript.active_path()),
            Vec::<String>::new(),
            "no leaf means an empty active_path"
        );
        assert_eq!(
            uuids(&transcript.full_path()),
            Vec::<String>::new(),
            "no leaf means an empty full_path"
        );
    }

    #[test]
    fn absorb_can_be_called_repeatedly_for_incremental_tails() {
        let mut transcript = Transcript::new();
        transcript.absorb(vec![message("a", None), message("b", Some("a"))]);
        assert_eq!(
            uuids(&transcript.active_path()),
            vec!["a".to_string(), "b".to_string()],
            "path after the first absorb"
        );
        transcript.absorb(vec![message("c", Some("b"))]);
        assert_eq!(
            uuids(&transcript.active_path()),
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
            "path after absorbing the tail"
        );
    }

    #[test]
    fn re_absorbing_the_same_uuid_replaces_the_earlier_record() {
        let first = record(json!({
            "type": "assistant",
            "uuid": "a",
            "parentUuid": null,
            "message": { "role": "assistant", "content": "first" },
        }));
        let second = record(json!({
            "type": "user",
            "uuid": "a",
            "parentUuid": null,
            "message": { "role": "user", "content": "second" },
        }));
        let transcript = transcript_of(vec![first, second]);
        let path = transcript.active_path();
        assert_eq!(
            uuids(&path),
            vec!["a".to_string()],
            "the duplicate must not appear twice"
        );
        let leaf = match path.first() {
            Some(leaf) => leaf,
            None => panic!("expected one record in the path, got none"),
        };
        assert_eq!(
            leaf.record_type, "user",
            "the later record must win; raw was {:?}",
            leaf.raw
        );
    }

    // ---------- active_path ----------

    #[test]
    fn active_path_is_ordered_oldest_to_newest() {
        let transcript = transcript_of(vec![
            message("a", None),
            message("b", Some("a")),
            message("c", Some("b")),
        ]);
        assert_eq!(
            uuids(&transcript.active_path()),
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
            "walking parents then reversing yields oldest first"
        );
    }

    #[test]
    fn active_path_stops_at_a_parent_that_was_never_absorbed() {
        let transcript = transcript_of(vec![message("b", Some("not-yet-read"))]);
        let path = transcript.active_path();
        assert_eq!(
            uuids(&path),
            vec!["b".to_string()],
            "a dangling parent must truncate the walk, not empty it"
        );
    }

    #[test]
    fn active_path_only_follows_the_leafs_own_ancestry() {
        // A second, unrelated root must not leak into the path of the newest branch.
        let transcript = transcript_of(vec![
            message("root-one", None),
            message("child-one", Some("root-one")),
            message("root-two", None),
        ]);
        assert_eq!(
            uuids(&transcript.active_path()),
            vec!["root-two".to_string()],
            "the leaf is root-two, whose ancestry is just itself"
        );
    }

    // ---------- sidechains ----------

    #[test]
    fn a_sidechain_record_does_not_become_the_leaf() {
        let transcript = transcript_of(vec![
            message("a", None),
            message("b", Some("a")),
            sidechain("s1", Some("b")),
            sidechain("s2", Some("s1")),
        ]);
        assert_eq!(
            uuids(&transcript.active_path()),
            vec!["a".to_string(), "b".to_string()],
            "the leaf must remain the last non-sidechain record"
        );
        assert_eq!(
            uuids(&transcript.full_path()),
            vec!["a".to_string(), "b".to_string()],
            "full_path must ignore sidechains as well"
        );
    }

    #[test]
    fn sidechain_records_never_appear_in_a_path_even_when_referenced_as_a_parent() {
        // Spec gap: it does not say whether the walk stops at a sidechain parent or skips
        // through it to that sidechain's own parent. Only the guaranteed part is asserted:
        // the sidechain record itself must never show up, and the path must not be empty.
        let transcript = transcript_of(vec![
            message("a", None),
            sidechain("s1", Some("a")),
            message("b", Some("s1")),
        ]);
        let path = uuids(&transcript.active_path());
        assert!(
            path.contains(&"b".to_string()),
            "the leaf must be present; path was {:?}",
            path
        );
        assert!(
            !path.contains(&"s1".to_string()),
            "a sidechain record must never appear in a path; path was {:?}",
            path
        );
        assert!(
            !path.is_empty(),
            "the path must not be empty; path was {:?}",
            path
        );
    }

    // ---------- cycles ----------

    #[test]
    fn a_self_referencing_parent_terminates() {
        let transcript = transcript_of(vec![message("a", Some("a"))]);
        let path = transcript.active_path();
        assert!(
            path.len() <= 1,
            "a self cycle must not repeat records; path was {:?}",
            uuids(&path)
        );
        assert_eq!(
            uuids(&path),
            vec!["a".to_string()],
            "the leaf itself is still returned"
        );
    }

    #[test]
    fn a_two_record_cycle_terminates() {
        let transcript = transcript_of(vec![message("a", Some("b")), message("b", Some("a"))]);
        let path = transcript.active_path();
        assert!(
            path.len() <= 2,
            "an A->B->A cycle must visit each record at most once; path was {:?}",
            uuids(&path)
        );
        assert_eq!(
            uuids(&path),
            vec!["a".to_string(), "b".to_string()],
            "leaf b, then a, reversed"
        );
    }

    #[test]
    fn a_longer_cycle_terminates_with_a_bounded_path() {
        let transcript = transcript_of(vec![
            message("a", Some("d")),
            message("b", Some("a")),
            message("c", Some("b")),
            message("d", Some("c")),
        ]);
        let active = transcript.active_path();
        assert!(
            active.len() <= 4,
            "a four record cycle must yield at most four entries; path was {:?}",
            uuids(&active)
        );
        let full = transcript.full_path();
        assert!(
            full.len() <= 4,
            "full_path must be bounded by the same visited set; path was {:?}",
            uuids(&full)
        );
    }

    #[test]
    fn a_cycle_through_logical_parent_uuid_terminates_in_full_path() {
        let transcript = transcript_of(vec![
            boundary("x", None, Some("y")),
            message("y", Some("x")),
        ]);
        let full = transcript.full_path();
        assert!(
            full.len() <= 2,
            "the logical parent loops back to an already visited record; path was {:?}",
            uuids(&full)
        );
        assert_eq!(
            uuids(&full),
            vec!["x".to_string(), "y".to_string()],
            "leaf y, then boundary x, whose logical parent y is already visited"
        );
    }

    // ---------- full_path ----------

    #[test]
    fn full_path_equals_active_path_when_nothing_was_compacted() {
        let transcript = transcript_of(vec![
            message("a", None),
            message("b", Some("a")),
            message("c", Some("b")),
        ]);
        let active = uuids(&transcript.active_path());
        let full = uuids(&transcript.full_path());
        assert_eq!(
            full, active,
            "with zero compactions the two paths must be identical"
        );
        assert_eq!(
            active,
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
            "sanity check on the shared path"
        );
    }

    #[test]
    fn full_path_crosses_a_single_boundary_while_active_path_stops_at_it() {
        let transcript = transcript_of(vec![
            message("m1", None),
            message("m2", Some("m1")),
            boundary("b1", None, Some("m2")),
            message("m3", Some("b1")),
        ]);
        assert_eq!(
            uuids(&transcript.active_path()),
            vec!["b1".to_string(), "m3".to_string()],
            "active_path stops where the boundary's parent_uuid is None"
        );
        assert_eq!(
            uuids(&transcript.full_path()),
            vec![
                "m1".to_string(),
                "m2".to_string(),
                "b1".to_string(),
                "m3".to_string(),
            ],
            "full_path hops to the logical parent and keeps the boundary record itself"
        );
    }

    #[test]
    fn full_path_crosses_two_adjacent_boundaries_with_no_messages_between_them() {
        let transcript = transcript_of(vec![
            message("m1", None),
            boundary("b1", None, Some("m1")),
            boundary("b2", None, Some("b1")),
            message("m2", Some("b2")),
        ]);
        assert_eq!(
            uuids(&transcript.active_path()),
            vec!["b2".to_string(), "m2".to_string()],
            "active_path only sees the newest context"
        );
        assert_eq!(
            uuids(&transcript.full_path()),
            vec![
                "m1".to_string(),
                "b1".to_string(),
                "b2".to_string(),
                "m2".to_string(),
            ],
            "both boundary records stay in the result, in oldest-first order"
        );
    }

    #[test]
    fn a_boundary_with_a_dangling_logical_parent_stops_there() {
        let transcript = transcript_of(vec![
            boundary("b1", None, Some("never-absorbed")),
            message("m1", Some("b1")),
        ]);
        let full = uuids(&transcript.full_path());
        assert_eq!(
            full,
            vec!["b1".to_string(), "m1".to_string()],
            "a dangling logical parent truncates the walk without panicking"
        );
        assert_eq!(
            full,
            uuids(&transcript.active_path()),
            "with nothing reachable behind the boundary both paths agree"
        );
    }

    #[test]
    fn a_boundary_without_a_logical_parent_behaves_like_active_path() {
        let transcript = transcript_of(vec![
            message("m0", None),
            boundary("b1", None, None),
            message("m1", Some("b1")),
        ]);
        let active = uuids(&transcript.active_path());
        let full = uuids(&transcript.full_path());
        assert_eq!(
            active,
            vec!["b1".to_string(), "m1".to_string()],
            "active_path stops at the boundary"
        );
        assert_eq!(
            full, active,
            "with no logical parent full_path must stop in exactly the same place; \
             m0 is unreachable"
        );
    }

    #[test]
    fn a_boundary_whose_parent_uuid_is_set_still_prefers_the_logical_parent() {
        let transcript = transcript_of(vec![
            message("old", None),
            message("wrong-way", None),
            boundary("b1", Some("wrong-way"), Some("old")),
            message("m1", Some("b1")),
        ]);
        assert_eq!(
            uuids(&transcript.active_path()),
            vec!["wrong-way".to_string(), "b1".to_string(), "m1".to_string(),],
            "active_path follows parent_uuid, boundary or not"
        );
        assert_eq!(
            uuids(&transcript.full_path()),
            vec!["old".to_string(), "b1".to_string(), "m1".to_string()],
            "full_path must switch to logical_parent_uuid at the boundary"
        );
    }

    #[test]
    fn a_boundary_can_be_the_leaf_itself() {
        let transcript = transcript_of(vec![message("m1", None), boundary("b1", None, Some("m1"))]);
        assert_eq!(
            uuids(&transcript.active_path()),
            vec!["b1".to_string()],
            "the boundary is the newest record with a uuid"
        );
        assert_eq!(
            uuids(&transcript.full_path()),
            vec!["m1".to_string(), "b1".to_string()],
            "full_path still crosses it"
        );
    }

    // ---------- compact_boundaries ----------

    #[test]
    fn compact_boundaries_returns_boundaries_in_absorb_order() {
        let transcript = transcript_of(vec![
            message("m1", None),
            boundary("b1", None, Some("m1")),
            message("m2", Some("b1")),
            boundary("b2", None, Some("m2")),
            message("m3", Some("b2")),
        ]);
        assert_eq!(
            uuids(&transcript.compact_boundaries()),
            vec!["b1".to_string(), "b2".to_string()],
            "boundaries in the order they were absorbed"
        );
    }

    #[test]
    fn compact_boundaries_ignores_is_compact_summary_without_the_subtype() {
        let summary_only = record(json!({
            "type": "user",
            "uuid": "s",
            "parentUuid": null,
            "isCompactSummary": true,
            "message": { "role": "user", "content": "summary text" },
        }));
        let transcript = transcript_of(vec![summary_only]);
        assert_eq!(
            uuids(&transcript.compact_boundaries()),
            Vec::<String>::new(),
            "membership is decided by subtype == \"compact_boundary\" only"
        );
    }

    #[test]
    fn compact_boundaries_is_empty_when_nothing_was_compacted() {
        let transcript = transcript_of(vec![message("a", None), message("b", Some("a"))]);
        assert_eq!(
            uuids(&transcript.compact_boundaries()),
            Vec::<String>::new(),
            "no boundaries absorbed"
        );
    }
}
