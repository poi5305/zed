#[cfg(test)]
mod blind_anchor_tests {
    use crate::terminal_anchors::*;

    fn screen(lines: &[&str]) -> Vec<ScreenRow> {
        lines
            .iter()
            .enumerate()
            .map(|(row, text)| ScreenRow {
                row,
                text: (*text).to_string(),
            })
            .collect()
    }

    fn anchor(row: usize, glyph: AnchorGlyph, text: &str) -> AnchorRow {
        AnchorRow {
            row,
            glyph,
            text: text.to_string(),
        }
    }

    fn entry(key: &str, glyph: AnchorGlyph, text: &str) -> TranscriptAnchor {
        TranscriptAnchor {
            key: key.to_string(),
            glyph,
            text: text.to_string(),
        }
    }

    fn found(rows: &[ScreenRow]) -> Vec<AnchorRow> {
        anchor_rows(rows, &Glyphs::default())
    }

    fn keys(anchorings: &[Anchoring]) -> Vec<String> {
        anchorings
            .iter()
            .map(|anchoring| anchoring.key.clone())
            .collect()
    }

    fn rows_of(anchorings: &[Anchoring]) -> Vec<usize> {
        anchorings.iter().map(|anchoring| anchoring.row).collect()
    }

    // ---------------------------------------------------------------- skeleton

    /// The transcript holds raw markdown while the screen holds what the TUI rendered, so
    /// both sides have to reduce to the same string.
    #[test]
    fn skeleton_makes_rendered_and_raw_emphasis_agree() {
        let raw = skeleton("**Done.** I fixed the bug");
        let rendered = skeleton("Done. I fixed the bug");
        assert_eq!(
            raw, rendered,
            "raw markdown skeleton {raw:?} must equal the rendered skeleton {rendered:?}"
        );
        assert_eq!(raw, "doneifixedthebug", "got {raw:?}");
    }

    #[test]
    fn skeleton_drops_heading_markers_and_spaces() {
        let got = skeleton("# Heading One");
        assert_eq!(got, "headingone", "expected \"headingone\", got {got:?}");
    }

    #[test]
    fn skeleton_drops_code_ticks_and_list_bullets() {
        let got = skeleton("- run `cargo test` now");
        assert_eq!(
            got, "runcargotestnow",
            "expected \"runcargotestnow\", got {got:?}"
        );
    }

    #[test]
    fn skeleton_lowercases_letters() {
        let got = skeleton("ReadFile");
        assert_eq!(got, "readfile", "expected \"readfile\", got {got:?}");
    }

    #[test]
    fn skeleton_keeps_unicode_alphanumerics() {
        let got = skeleton("修好了 3 個 bug！");
        assert_eq!(
            got, "修好了3個bug",
            "CJK characters and digits are alphanumeric and must survive, got {got:?}"
        );
    }

    #[test]
    fn skeleton_of_punctuation_and_space_only_is_empty() {
        let got = skeleton("  ***  --- ...  ");
        assert_eq!(got, "", "expected an empty skeleton, got {got:?}");
    }

    // ------------------------------------------------------------- anchor_rows

    #[test]
    fn anchor_rows_finds_prompt_and_assistant_rows_in_order() {
        let rows = screen(&[
            "> fix the login bug",
            "⏺ I'll look at auth.rs first.",
            "⏺ Read(auth.rs)",
        ]);
        let got = found(&rows);
        assert_eq!(
            got,
            vec![
                anchor(0, AnchorGlyph::UserPrompt, "fix the login bug"),
                anchor(1, AnchorGlyph::Assistant, "I'll look at auth.rs first."),
                anchor(2, AnchorGlyph::Assistant, "Read(auth.rs)"),
            ],
            "got {got:?}"
        );
    }

    /// Tool result rows begin with `⎿`, which is neither glyph.
    #[test]
    fn anchor_rows_skips_tool_result_rows() {
        let rows = screen(&["⏺ Read(auth.rs)", "  ⎿ Read 250 lines (ctrl+o to expand)"]);
        let got = found(&rows);
        assert_eq!(
            got,
            vec![anchor(0, AnchorGlyph::Assistant, "Read(auth.rs)")],
            "the ⎿ result row is not an anchor, got {got:?}"
        );
    }

    /// The composer's own line carries a glyph and no text.
    #[test]
    fn anchor_rows_skips_the_empty_input_line() {
        let rows = screen(&["> ", ">"]);
        let got = found(&rows);
        assert!(
            got.is_empty(),
            "an input line with no text after the glyph is not an anchor, got {got:?}"
        );
    }

    #[test]
    fn anchor_rows_skips_an_input_line_holding_only_spaces() {
        let rows = screen(&[">      "]);
        let got = found(&rows);
        assert!(
            got.is_empty(),
            "only whitespace follows the glyph, so this is the input line, got {got:?}"
        );
    }

    #[test]
    fn anchor_rows_allows_leading_spaces_before_the_glyph() {
        let rows = screen(&["   ⏺ indented assistant line"]);
        let got = found(&rows);
        assert_eq!(
            got,
            vec![anchor(0, AnchorGlyph::Assistant, "indented assistant line")],
            "leading spaces are allowed before the glyph, got {got:?}"
        );
    }

    #[test]
    fn anchor_rows_require_a_space_after_the_glyph() {
        let rows = screen(&["⏺text with no gap", ">text with no gap"]);
        let got = found(&rows);
        assert!(
            got.is_empty(),
            "a glyph glued to the text is not an anchor, got {got:?}"
        );
    }

    /// A wrapped long line repeats no glyph, so only its first row anchors.
    #[test]
    fn anchor_rows_ignore_wrapped_continuation_rows() {
        let rows = screen(&[
            "⏺ This is a very long assistant message that the terminal had",
            "  to wrap onto a second row without any glyph.",
        ]);
        let got = found(&rows);
        assert_eq!(
            got,
            vec![anchor(
                0,
                AnchorGlyph::Assistant,
                "This is a very long assistant message that the terminal had"
            )],
            "only the first row of a wrapped message is an anchor, got {got:?}"
        );
    }

    #[test]
    fn anchor_rows_trim_the_text_after_the_glyph() {
        let rows = screen(&["⏺    Read(auth.rs)   "]);
        let got = found(&rows);
        assert_eq!(
            got,
            vec![anchor(0, AnchorGlyph::Assistant, "Read(auth.rs)")],
            "the text after the glyph is trimmed on both sides, got {got:?}"
        );
    }

    #[test]
    fn anchor_rows_use_the_glyphs_they_are_given() {
        let rows = screen(&["❯ fix the login bug", "● doing that now", "> old glyph"]);
        let glyphs = Glyphs {
            user_prompt: '❯',
            assistant: '●',
        };
        let got = anchor_rows(&rows, &glyphs);
        assert_eq!(
            got,
            vec![
                anchor(0, AnchorGlyph::UserPrompt, "fix the login bug"),
                anchor(1, AnchorGlyph::Assistant, "doing that now"),
            ],
            "with custom glyphs the default '>' is no longer an anchor, got {got:?}"
        );
    }

    #[test]
    fn default_glyphs_are_the_claude_code_ones() {
        let glyphs = Glyphs::default();
        assert_eq!(
            (glyphs.user_prompt, glyphs.assistant),
            ('>', '⏺'),
            "got ({:?}, {:?})",
            glyphs.user_prompt,
            glyphs.assistant
        );
    }

    #[test]
    fn anchor_rows_on_an_empty_screen_find_nothing() {
        let got = found(&[]);
        assert!(got.is_empty(), "got {got:?}");
    }

    // -------------------------------------------------------------- rows_match

    #[test]
    fn constants_hold_their_agreed_values() {
        assert_eq!(MIN_SKELETON, 6, "MIN_SKELETON is {MIN_SKELETON}");
        assert_eq!(
            MAX_TRANSCRIPT_ANCHORS, 512,
            "MAX_TRANSCRIPT_ANCHORS is {MAX_TRANSCRIPT_ANCHORS}"
        );
    }

    /// The screen truncates at the terminal width, so the screen skeleton is a prefix.
    #[test]
    fn rows_match_when_the_screen_row_is_the_shorter_prefix() {
        let screen_row = anchor(4, AnchorGlyph::Assistant, "I'll look at auth.rs");
        let transcript = entry(
            "uuid-1",
            AnchorGlyph::Assistant,
            "I'll look at auth.rs first and then the session store.",
        );
        assert!(
            rows_match(&screen_row, &transcript),
            "screen skeleton {:?} is a prefix of transcript skeleton {:?}",
            skeleton(&screen_row.text),
            skeleton(&transcript.text)
        );
    }

    /// The other direction is legal too: the transcript's first line can be the shorter of
    /// the two when the screen row carries more rendered text.
    #[test]
    fn rows_match_when_the_transcript_is_the_shorter_prefix() {
        let screen_row = anchor(
            2,
            AnchorGlyph::UserPrompt,
            "fix the login bug in the auth module",
        );
        let transcript = entry("uuid-2", AnchorGlyph::UserPrompt, "fix the login bug");
        assert!(
            rows_match(&screen_row, &transcript),
            "transcript skeleton {:?} is a prefix of screen skeleton {:?}",
            skeleton(&transcript.text),
            skeleton(&screen_row.text)
        );
    }

    #[test]
    fn rows_do_not_match_across_different_glyphs() {
        let screen_row = anchor(0, AnchorGlyph::UserPrompt, "fix the login bug");
        let transcript = entry("uuid-3", AnchorGlyph::Assistant, "fix the login bug");
        assert!(
            !rows_match(&screen_row, &transcript),
            "same text {:?} but different glyphs must never match",
            screen_row.text
        );
    }

    /// A skeleton shorter than MIN_SKELETON is too weak to carry a prefix match.
    #[test]
    fn a_short_skeleton_must_be_equal_not_merely_a_prefix() {
        let screen_row = anchor(0, AnchorGlyph::Assistant, "ok");
        let transcript = entry("uuid-4", AnchorGlyph::Assistant, "okay, on it now");
        assert!(
            !rows_match(&screen_row, &transcript),
            "screen skeleton {:?} is under MIN_SKELETON ({MIN_SKELETON}), so a prefix of {:?} is not enough",
            skeleton(&screen_row.text),
            skeleton(&transcript.text)
        );
    }

    #[test]
    fn equal_short_skeletons_still_match() {
        let screen_row = anchor(7, AnchorGlyph::Assistant, "Read()");
        let transcript = entry("uuid-5", AnchorGlyph::Assistant, "Read()");
        assert_eq!(
            skeleton(&screen_row.text),
            "read",
            "guard: this case is meant to be under MIN_SKELETON"
        );
        assert!(
            rows_match(&screen_row, &transcript),
            "identical short skeletons {:?} match",
            skeleton(&screen_row.text)
        );
    }

    #[test]
    fn a_skeleton_exactly_at_min_skeleton_may_match_by_prefix() {
        let screen_row = anchor(0, AnchorGlyph::Assistant, "abcdef");
        let transcript = entry("uuid-6", AnchorGlyph::Assistant, "abcdef ghij");
        assert_eq!(
            skeleton(&screen_row.text).chars().count(),
            MIN_SKELETON,
            "guard: the screen skeleton must be exactly MIN_SKELETON long"
        );
        assert!(
            rows_match(&screen_row, &transcript),
            "a skeleton of exactly MIN_SKELETON is not \"under\" it, so the prefix rule applies: {:?} vs {:?}",
            skeleton(&screen_row.text),
            skeleton(&transcript.text)
        );
    }

    #[test]
    fn rows_do_not_match_when_neither_skeleton_is_a_prefix() {
        let screen_row = anchor(0, AnchorGlyph::Assistant, "Read(auth.rs)");
        let transcript = entry("uuid-7", AnchorGlyph::Assistant, "Read(session_store.rs)");
        assert!(
            !rows_match(&screen_row, &transcript),
            "{:?} and {:?} diverge before either ends",
            skeleton(&screen_row.text),
            skeleton(&transcript.text)
        );
    }

    /// Markdown on one side only must not stop a match.
    #[test]
    fn rows_match_through_markdown_on_the_transcript_side() {
        let screen_row = anchor(1, AnchorGlyph::Assistant, "Done. I fixed the bug");
        let transcript = entry(
            "uuid-8",
            AnchorGlyph::Assistant,
            "**Done.** I fixed the bug",
        );
        assert!(
            rows_match(&screen_row, &transcript),
            "skeletons {:?} and {:?} must be equal",
            skeleton(&screen_row.text),
            skeleton(&transcript.text)
        );
    }

    // ------------------------------------------------------------------- align

    #[test]
    fn align_matches_in_order_and_skips_unmatched_transcript_anchors() {
        let rows = vec![
            anchor(0, AnchorGlyph::UserPrompt, "fix the login bug"),
            anchor(2, AnchorGlyph::Assistant, "Read(auth.rs)"),
        ];
        let transcript = vec![
            entry("k-old", AnchorGlyph::Assistant, "Earlier unrelated message"),
            entry("k-user", AnchorGlyph::UserPrompt, "fix the login bug"),
            entry("k-other", AnchorGlyph::Assistant, "Write(notes.md)"),
            entry("k-read", AnchorGlyph::Assistant, "Read(auth.rs)"),
        ];
        let got = align(&rows, &transcript);
        assert_eq!(
            got,
            vec![
                Anchoring {
                    row: 0,
                    key: "k-user".to_string()
                },
                Anchoring {
                    row: 2,
                    key: "k-read".to_string()
                },
            ],
            "got {got:?}"
        );
    }

    /// The screen shows the tail of the conversation, so an ambiguous repeat resolves to
    /// the newest anchors.
    #[test]
    fn align_ties_prefer_the_newest_transcript_anchors() {
        let rows = vec![
            anchor(3, AnchorGlyph::Assistant, "Read(auth.rs)"),
            anchor(5, AnchorGlyph::Assistant, "Read(auth.rs)"),
        ];
        let transcript = vec![
            entry("k1", AnchorGlyph::Assistant, "Read(auth.rs)"),
            entry("k2", AnchorGlyph::Assistant, "Read(auth.rs)"),
            entry("k3", AnchorGlyph::Assistant, "Read(auth.rs)"),
        ];
        let got = align(&rows, &transcript);
        assert_eq!(
            keys(&got),
            vec!["k2".to_string(), "k3".to_string()],
            "expected the two newest anchors, got {got:?}"
        );
    }

    #[test]
    fn align_uses_each_screen_row_and_each_anchor_at_most_once() {
        let rows = vec![anchor(1, AnchorGlyph::Assistant, "Read(auth.rs)")];
        let transcript = vec![
            entry("k1", AnchorGlyph::Assistant, "Read(auth.rs)"),
            entry("k2", AnchorGlyph::Assistant, "Read(auth.rs)"),
        ];
        let got = align(&rows, &transcript);
        assert_eq!(
            got.len(),
            1,
            "one screen row can take only one anchor, got {got:?}"
        );
        assert_eq!(
            keys(&got),
            vec!["k2".to_string()],
            "the tie goes to the newest, got {got:?}"
        );
    }

    #[test]
    fn align_returns_anchorings_sorted_by_row() {
        let rows = vec![
            anchor(0, AnchorGlyph::UserPrompt, "first question about auth"),
            anchor(4, AnchorGlyph::Assistant, "Read(auth.rs)"),
            anchor(9, AnchorGlyph::Assistant, "Write(auth.rs)"),
        ];
        let transcript = vec![
            entry("k1", AnchorGlyph::UserPrompt, "first question about auth"),
            entry("k2", AnchorGlyph::Assistant, "Read(auth.rs)"),
            entry("k3", AnchorGlyph::Assistant, "Write(auth.rs)"),
        ];
        let got = align(&rows, &transcript);
        assert_eq!(rows_of(&got), vec![0, 4, 9], "got {got:?}");
        assert_eq!(
            keys(&got),
            vec!["k1".to_string(), "k2".to_string(), "k3".to_string()],
            "got {got:?}"
        );
    }

    #[test]
    fn align_returns_nothing_when_no_row_matches() {
        let rows = vec![anchor(0, AnchorGlyph::Assistant, "Read(auth.rs)")];
        let transcript = vec![entry("k1", AnchorGlyph::UserPrompt, "Read(auth.rs)")];
        let got = align(&rows, &transcript);
        assert!(
            got.is_empty(),
            "the glyphs differ so nothing can be anchored, got {got:?}"
        );
    }

    #[test]
    fn align_of_empty_inputs_is_empty() {
        let got = align(&[], &[]);
        assert!(got.is_empty(), "got {got:?}");

        let rows = vec![anchor(0, AnchorGlyph::Assistant, "Read(auth.rs)")];
        let got = align(&rows, &[]);
        assert!(got.is_empty(), "no transcript anchors at all, got {got:?}");
    }

    /// Order is preserved, so a crossing pair cannot both be taken.
    #[test]
    fn align_cannot_take_a_crossing_pair() {
        let rows = vec![
            anchor(0, AnchorGlyph::Assistant, "the second thing I did"),
            anchor(1, AnchorGlyph::Assistant, "the first thing I did"),
        ];
        let transcript = vec![
            entry("k-first", AnchorGlyph::Assistant, "the first thing I did"),
            entry("k-second", AnchorGlyph::Assistant, "the second thing I did"),
        ];
        let got = align(&rows, &transcript);
        assert_eq!(
            got.len(),
            1,
            "an order-preserving alignment can keep only one of a crossing pair, got {got:?}"
        );
    }

    fn numbered(index: usize) -> String {
        format!("message number {index} of the conversation")
    }

    #[test]
    fn align_ignores_transcript_anchors_before_the_last_512() {
        let transcript: Vec<TranscriptAnchor> = (0..=MAX_TRANSCRIPT_ANCHORS)
            .map(|index| {
                entry(
                    &format!("k{index}"),
                    AnchorGlyph::Assistant,
                    &numbered(index),
                )
            })
            .collect();
        assert_eq!(
            transcript.len(),
            MAX_TRANSCRIPT_ANCHORS + 1,
            "guard: one anchor past the cap"
        );

        let rows = vec![anchor(0, AnchorGlyph::Assistant, &numbered(0))];
        let got = align(&rows, &transcript);
        assert!(
            got.is_empty(),
            "the oldest anchor falls outside the last {MAX_TRANSCRIPT_ANCHORS}, got {got:?}"
        );
    }

    #[test]
    fn align_still_reaches_the_oldest_anchor_inside_the_cap() {
        let transcript: Vec<TranscriptAnchor> = (0..MAX_TRANSCRIPT_ANCHORS)
            .map(|index| {
                entry(
                    &format!("k{index}"),
                    AnchorGlyph::Assistant,
                    &numbered(index),
                )
            })
            .collect();

        let rows = vec![anchor(0, AnchorGlyph::Assistant, &numbered(0))];
        let got = align(&rows, &transcript);
        assert_eq!(
            keys(&got),
            vec!["k0".to_string()],
            "exactly {MAX_TRANSCRIPT_ANCHORS} anchors are all considered, got {got:?}"
        );
    }

    /// Truncated screen rows still align, which is the whole point of the prefix rule.
    #[test]
    fn align_matches_rows_the_terminal_truncated() {
        let rows = vec![
            anchor(0, AnchorGlyph::UserPrompt, "please fix the login bug in"),
            anchor(3, AnchorGlyph::Assistant, "Done. I fixed the bug"),
        ];
        let transcript = vec![
            entry(
                "k-user",
                AnchorGlyph::UserPrompt,
                "please fix the login bug in the auth module",
            ),
            entry(
                "k-assistant",
                AnchorGlyph::Assistant,
                "**Done.** I fixed the bug in `auth.rs`",
            ),
        ];
        let got = align(&rows, &transcript);
        assert_eq!(
            keys(&got),
            vec!["k-user".to_string(), "k-assistant".to_string()],
            "got {got:?}"
        );
    }

    // ------------------------------------------------------------- screen_rows

    fn cells(items: &[(i32, usize, char)]) -> Vec<(i32, usize, char)> {
        items.to_vec()
    }

    #[test]
    fn screen_rows_drop_scrollback_cells() {
        let got = screen_rows(
            cells(&[(-1, 0, 'o'), (-1, 1, 'd'), (0, 0, 'h'), (0, 1, 'i')]).into_iter(),
            1,
        );
        assert_eq!(
            got,
            vec![ScreenRow {
                row: 0,
                text: "hi".to_string()
            }],
            "cells with a negative line are scrollback, got {got:?}"
        );
    }

    #[test]
    fn screen_rows_drop_cells_at_or_past_screen_lines() {
        let got = screen_rows(
            cells(&[(0, 0, 'a'), (1, 0, 'b'), (2, 0, 'c')]).into_iter(),
            2,
        );
        assert_eq!(
            got,
            vec![
                ScreenRow {
                    row: 0,
                    text: "a".to_string()
                },
                ScreenRow {
                    row: 1,
                    text: "b".to_string()
                },
            ],
            "line 2 is off a 2-line screen, got {got:?}"
        );
    }

    #[test]
    fn screen_rows_order_cells_by_column() {
        let got = screen_rows(
            cells(&[(0, 2, 'c'), (0, 0, 'a'), (0, 3, 'd'), (0, 1, 'b')]).into_iter(),
            1,
        );
        assert_eq!(
            got,
            vec![ScreenRow {
                row: 0,
                text: "abcd".to_string()
            }],
            "cells arrive unordered and must be sorted by column, got {got:?}"
        );
    }

    #[test]
    fn screen_rows_trim_trailing_spaces() {
        let got = screen_rows(
            cells(&[(0, 0, 'h'), (0, 1, 'i'), (0, 2, ' '), (0, 3, ' ')]).into_iter(),
            1,
        );
        assert_eq!(
            got,
            vec![ScreenRow {
                row: 0,
                text: "hi".to_string()
            }],
            "trailing spaces are trimmed, got {got:?}"
        );
    }

    #[test]
    fn screen_rows_keep_leading_spaces() {
        let got = screen_rows(
            cells(&[(0, 0, ' '), (0, 1, ' '), (0, 2, '⏺')]).into_iter(),
            1,
        );
        assert_eq!(
            got,
            vec![ScreenRow {
                row: 0,
                text: "  ⏺".to_string()
            }],
            "only trailing spaces are trimmed, got {got:?}"
        );
    }

    /// `row` must stay the screen row, so a line with no cells still appears.
    #[test]
    fn screen_rows_keep_empty_lines_so_row_is_the_screen_row() {
        let got = screen_rows(cells(&[(2, 0, 'x')]).into_iter(), 4);
        assert_eq!(
            got,
            vec![
                ScreenRow {
                    row: 0,
                    text: String::new()
                },
                ScreenRow {
                    row: 1,
                    text: String::new()
                },
                ScreenRow {
                    row: 2,
                    text: "x".to_string()
                },
                ScreenRow {
                    row: 3,
                    text: String::new()
                },
            ],
            "got {got:?}"
        );
    }

    #[test]
    fn screen_rows_length_is_exactly_screen_lines() {
        let got = screen_rows(cells(&[(0, 0, 'a')]).into_iter(), 24);
        assert_eq!(
            got.len(),
            24,
            "expected one row per screen line, got {}",
            got.len()
        );
        assert_eq!(
            rows_of_screen(&got),
            (0..24).collect::<Vec<_>>(),
            "row must count up from 0"
        );
    }

    fn rows_of_screen(rows: &[ScreenRow]) -> Vec<usize> {
        rows.iter().map(|row| row.row).collect()
    }

    #[test]
    fn screen_rows_fill_column_gaps_with_spaces() {
        let got = screen_rows(cells(&[(0, 0, 'a'), (0, 3, 'b')]).into_iter(), 1);
        assert_eq!(
            got,
            vec![ScreenRow {
                row: 0,
                text: "a  b".to_string()
            }],
            "columns 1 and 2 have no cell and read as spaces, got {got:?}"
        );
    }

    #[test]
    fn screen_rows_of_a_zero_line_screen_is_empty() {
        let got = screen_rows(cells(&[(0, 0, 'a')]).into_iter(), 0);
        assert!(
            got.is_empty(),
            "a screen with no lines has no rows, got {got:?}"
        );
    }

    /// The end-to-end shape the panel needs: cells in, anchors out.
    #[test]
    fn screen_rows_feed_anchor_rows() {
        let mut input = Vec::new();
        for (column, character) in "⏺ Read(auth.rs)".chars().enumerate() {
            input.push((0, column, character));
        }
        for (column, character) in "> ".chars().enumerate() {
            input.push((1, column, character));
        }
        let rows = screen_rows(input.into_iter(), 2);
        let got = anchor_rows(&rows, &Glyphs::default());
        assert_eq!(
            got,
            vec![anchor(0, AnchorGlyph::Assistant, "Read(auth.rs)")],
            "the input line is not an anchor, got {got:?} from rows {rows:?}"
        );
    }
}
