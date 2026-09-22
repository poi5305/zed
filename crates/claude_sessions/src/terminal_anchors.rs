use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnchorGlyph {
    UserPrompt,
    Assistant,
}

/// One visible terminal row. `row` is 0-based from the top of the visible screen.
/// `text` is the row's characters joined in column order with trailing spaces trimmed
/// (wide characters appear once; the spacer cell is skipped).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenRow {
    pub row: usize,
    pub text: String,
}

/// A row that starts an anchor: after optional leading spaces, `>` or `⏺`, then at least
/// one space, then the text. `text` is what follows the glyph, trimmed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnchorRow {
    pub row: usize,
    pub glyph: AnchorGlyph,
    pub text: String,
}

/// A transcript message the screen may show. `key` is the entry key the panel already
/// uses; `text` is the message's first line as the transcript holds it (raw markdown for
/// text, `Name(target)` for a tool call).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptAnchor {
    pub key: String,
    pub glyph: AnchorGlyph,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Anchoring {
    pub row: usize,
    pub key: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Glyphs {
    pub user_prompt: char,
    pub assistant: char,
}

impl Default for Glyphs {
    fn default() -> Self {
        Self {
            user_prompt: '>',
            assistant: '⏺',
        }
    }
}

pub const MIN_SKELETON: usize = 6;
pub const MAX_TRANSCRIPT_ANCHORS: usize = 512;

/// Keeps only alphanumeric characters, lowercased (Unicode alphanumerics included).
pub fn skeleton(text: &str) -> String {
    text.chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(|character| character.to_lowercase())
        .collect()
}

/// Picks the rows that start an anchor, top to bottom. A row whose text after the glyph
/// is empty is not an anchor (that is the input line).
pub fn anchor_rows(rows: &[ScreenRow], glyphs: &Glyphs) -> Vec<AnchorRow> {
    let mut anchors: Vec<AnchorRow> = rows
        .iter()
        .filter_map(|row| anchor_from_row(row, glyphs))
        .collect();
    anchors.sort_by_key(|anchor| anchor.row);
    anchors
}

/// Whether a screen row can stand for a transcript anchor: same glyph, and the shorter
/// skeleton is a prefix of the longer one. Skeletons under `MIN_SKELETON` (= 6) chars
/// must be equal.
pub fn rows_match(screen: &AnchorRow, transcript: &TranscriptAnchor) -> bool {
    skeletons_match(
        screen.glyph,
        &skeleton(&screen.text),
        transcript.glyph,
        &skeleton(&transcript.text),
    )
}

/// Order-preserving alignment (longest common subsequence under `rows_match`): screen
/// rows top→bottom, transcript oldest→newest. Each screen row and each transcript
/// anchor is used at most once. Ties prefer the newest transcript anchors (the screen
/// shows the tail of the conversation). Returns anchorings sorted by `row`.
/// Only the last `MAX_TRANSCRIPT_ANCHORS` (= 512) transcript anchors are considered.
pub fn align(screen: &[AnchorRow], transcript: &[TranscriptAnchor]) -> Vec<Anchoring> {
    let transcript = transcript_window(transcript);
    if screen.is_empty() || transcript.is_empty() {
        return Vec::new();
    }

    let screen_skeletons: Vec<(AnchorGlyph, String)> = screen
        .iter()
        .map(|row| (row.glyph, skeleton(&row.text)))
        .collect();
    let transcript_skeletons: Vec<(AnchorGlyph, String)> = transcript
        .iter()
        .map(|anchor| (anchor.glyph, skeleton(&anchor.text)))
        .collect();

    let screen_count = screen.len();
    let transcript_count = transcript.len();
    let mut lengths = vec![0u32; (screen_count + 1) * (transcript_count + 1)];
    let index = |screen_index: usize, transcript_index: usize| {
        screen_index * (transcript_count + 1) + transcript_index
    };

    for screen_index in 1..=screen_count {
        for transcript_index in 1..=transcript_count {
            let matched = skeletons_match(
                screen_skeletons[screen_index - 1].0,
                &screen_skeletons[screen_index - 1].1,
                transcript_skeletons[transcript_index - 1].0,
                &transcript_skeletons[transcript_index - 1].1,
            );
            lengths[index(screen_index, transcript_index)] = if matched {
                lengths[index(screen_index - 1, transcript_index - 1)] + 1
            } else {
                lengths[index(screen_index - 1, transcript_index)]
                    .max(lengths[index(screen_index, transcript_index - 1)])
            };
        }
    }

    // Walk the table from the tail. A match uses the newest transcript still in
    // range; when skipping, keep that column and drop the screen row instead, so
    // a later (newer) transcript index wins a tie of the same length.
    let mut screen_index = screen_count;
    let mut transcript_index = transcript_count;
    let mut matched = Vec::new();
    while screen_index > 0 && transcript_index > 0 {
        let pair_matches = skeletons_match(
            screen_skeletons[screen_index - 1].0,
            &screen_skeletons[screen_index - 1].1,
            transcript_skeletons[transcript_index - 1].0,
            &transcript_skeletons[transcript_index - 1].1,
        );
        if pair_matches
            && lengths[index(screen_index, transcript_index)]
                == lengths[index(screen_index - 1, transcript_index - 1)] + 1
        {
            matched.push(Anchoring {
                row: screen[screen_index - 1].row,
                key: transcript[transcript_index - 1].key.clone(),
            });
            screen_index -= 1;
            transcript_index -= 1;
        } else if lengths[index(screen_index, transcript_index)]
            == lengths[index(screen_index - 1, transcript_index)]
        {
            screen_index -= 1;
        } else {
            transcript_index -= 1;
        }
    }
    matched.reverse();
    matched
}

/// Builds `ScreenRow`s from what the terminal reports: cells with `line` in
/// `0..screen_lines` only (negative lines are scrollback), grouped by line, ordered by column.
/// Rows with no cells at all still appear (with empty text) so `row` stays the screen row.
pub fn screen_rows(
    cells: impl Iterator<Item = (i32, usize, char)>,
    screen_lines: usize,
) -> Vec<ScreenRow> {
    let mut columns_by_line: Vec<HashMap<usize, char>> =
        (0..screen_lines).map(|_| HashMap::new()).collect();
    for (line, column, character) in cells {
        let Ok(line) = usize::try_from(line) else {
            continue;
        };
        if line >= screen_lines {
            continue;
        }
        columns_by_line[line].insert(column, character);
    }

    columns_by_line
        .into_iter()
        .enumerate()
        .map(|(row, columns)| ScreenRow {
            row,
            text: row_text(columns),
        })
        .collect()
}

fn transcript_window(transcript: &[TranscriptAnchor]) -> &[TranscriptAnchor] {
    let skipped = transcript.len().saturating_sub(MAX_TRANSCRIPT_ANCHORS);
    &transcript[skipped..]
}

fn anchor_from_row(row: &ScreenRow, glyphs: &Glyphs) -> Option<AnchorRow> {
    let mut characters = row.text.chars().skip_while(|character| *character == ' ');
    let glyph_character = characters.next()?;
    let glyph = if glyph_character == glyphs.user_prompt {
        AnchorGlyph::UserPrompt
    } else if glyph_character == glyphs.assistant {
        AnchorGlyph::Assistant
    } else {
        return None;
    };
    if characters.next() != Some(' ') {
        return None;
    }
    let text: String = characters.collect();
    let text = text.trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some(AnchorRow {
        row: row.row,
        glyph,
        text,
    })
}

/// [`rows_match`] over skeletons the caller already has. A caller that compares the
/// same text many times computes each skeleton once and comes here instead.
pub fn skeletons_match(
    screen_glyph: AnchorGlyph,
    screen_skeleton: &str,
    transcript_glyph: AnchorGlyph,
    transcript_skeleton: &str,
) -> bool {
    if screen_glyph != transcript_glyph {
        return false;
    }
    // Both sides under MIN_SKELETON compare by equality, and "" == "" , so two
    // symbol-only rows (a checkmark, a rule, an emoji) would match each other.
    // Nothing identifiable is not a match, including a row with itself.
    if screen_skeleton.is_empty() && transcript_skeleton.is_empty() {
        return false;
    }
    let screen_len = screen_skeleton.chars().count();
    let transcript_len = transcript_skeleton.chars().count();
    if screen_len < MIN_SKELETON || transcript_len < MIN_SKELETON {
        return screen_skeleton == transcript_skeleton;
    }
    if screen_len <= transcript_len {
        transcript_skeleton.starts_with(screen_skeleton)
    } else {
        screen_skeleton.starts_with(transcript_skeleton)
    }
}

fn row_text(columns: HashMap<usize, char>) -> String {
    let Some(max_column) = columns.keys().copied().max() else {
        return String::new();
    };
    let mut text = String::new();
    for column in 0..=max_column {
        text.push(columns.get(&column).copied().unwrap_or(' '));
    }
    let trimmed = text.trim_end_matches(' ').len();
    text.truncate(trimmed);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(row: usize, text: &str) -> ScreenRow {
        ScreenRow {
            row,
            text: text.to_string(),
        }
    }

    fn anchor(row: usize, glyph: AnchorGlyph, text: &str) -> AnchorRow {
        AnchorRow {
            row,
            glyph,
            text: text.to_string(),
        }
    }

    fn sample(key: &str, glyph: AnchorGlyph, text: &str) -> TranscriptAnchor {
        TranscriptAnchor {
            key: key.to_string(),
            glyph,
            text: text.to_string(),
        }
    }

    #[test]
    fn skeleton_drops_markup_and_keeps_unicode_letters() {
        assert_eq!(skeleton("**Done.** I fixed the bug"), "doneifixedthebug");
        assert_eq!(skeleton("# Heading"), "heading");
        assert_eq!(skeleton("`code`"), "code");
        assert_eq!(skeleton("- item"), "item");
        assert_eq!(skeleton("修復登入"), "修復登入");
        assert_eq!(skeleton("fix-the_bug"), skeleton("fixthebug"));
        // Lowercasing does not expand ß; "SS" lowercases to "ss".
        assert_eq!(skeleton("ß"), "ß");
        assert_eq!(skeleton("SS"), "ss");
        assert_eq!(skeleton("row 12"), "row12");
        assert_eq!(skeleton(""), "");
    }

    #[test]
    fn anchor_rows_require_a_real_space_and_keep_short_text() {
        let glyphs = Glyphs::default();
        let rows = [
            screen(0, "> fix the login bug"),
            screen(1, "  ⏺ I'll look at auth.rs first."),
            screen(2, "⏺ Read(auth.rs)"),
            screen(3, "  continuation of the edit"),
            screen(4, "⎿ Read 250 lines (ctrl+o to expand)"),
            screen(5, "> "),
            screen(6, ">\tnot an anchor"),
            screen(7, "\t> tabbed"),
            screen(8, ">   hello  "),
            screen(9, "⏺text"),
            screen(10, "> ......"),
            screen(11, "hello > world"),
            screen(12, "> a"),
        ];
        assert_eq!(
            anchor_rows(&rows, &glyphs),
            vec![
                anchor(0, AnchorGlyph::UserPrompt, "fix the login bug"),
                anchor(1, AnchorGlyph::Assistant, "I'll look at auth.rs first."),
                anchor(2, AnchorGlyph::Assistant, "Read(auth.rs)"),
                anchor(8, AnchorGlyph::UserPrompt, "hello"),
                anchor(10, AnchorGlyph::UserPrompt, "......"),
                anchor(12, AnchorGlyph::UserPrompt, "a"),
            ]
        );
    }

    #[test]
    fn swapped_glyphs_stop_matching_the_claude_defaults() {
        let glyphs = Glyphs {
            user_prompt: '❯',
            assistant: '●',
        };
        let rows = [
            screen(0, "> fix"),
            screen(1, "❯ fix"),
            screen(2, "● Read(auth.rs)"),
        ];
        assert_eq!(
            anchor_rows(&rows, &glyphs),
            vec![
                anchor(1, AnchorGlyph::UserPrompt, "fix"),
                anchor(2, AnchorGlyph::Assistant, "Read(auth.rs)"),
            ]
        );
    }

    #[test]
    fn identical_glyphs_are_user_prompts() {
        let glyphs = Glyphs {
            user_prompt: 'X',
            assistant: 'X',
        };
        assert_eq!(
            anchor_rows(&[screen(0, "X hello")], &glyphs),
            vec![anchor(0, AnchorGlyph::UserPrompt, "hello")]
        );
    }

    #[test]
    fn rows_match_uses_prefix_at_six_and_equality_under_it() {
        let assistant = AnchorGlyph::Assistant;
        let long = anchor(0, assistant, &"a".repeat(MIN_SKELETON + 1));
        let exact = anchor(0, assistant, &"a".repeat(MIN_SKELETON));
        let short = anchor(0, assistant, &"a".repeat(MIN_SKELETON - 1));
        let transcript_long = sample("k", assistant, &"a".repeat(MIN_SKELETON + 4));
        let transcript_exact = sample("k", assistant, &"a".repeat(MIN_SKELETON));
        let transcript_short = sample("k", assistant, &"a".repeat(MIN_SKELETON - 1));

        assert!(rows_match(&exact, &transcript_long));
        assert!(rows_match(&long, &transcript_exact));
        assert!(rows_match(&short, &transcript_short));
        assert!(!rows_match(&short, &transcript_long));
        assert!(!rows_match(
            &anchor(0, AnchorGlyph::UserPrompt, &"a".repeat(MIN_SKELETON)),
            &transcript_exact
        ));

        let chinese_short = anchor(0, assistant, "修復錯誤");
        let chinese_longer = sample("k", assistant, "修復錯誤了");
        let chinese_six = anchor(0, assistant, "修復錯誤問題");
        let chinese_eight = sample("k", assistant, "修復錯誤問題額外");
        assert!(!rows_match(&chinese_short, &chinese_longer));
        assert!(rows_match(&chinese_six, &chinese_eight));
    }

    #[test]
    fn empty_skeletons_do_not_match_each_other() {
        let assistant = AnchorGlyph::Assistant;
        // `✅`, `---`, and `🎉` all drop to "" because skeleton keeps only
        // alphanumerics. Equal empty strings used to pair any symbol-only row
        // with any other.
        assert!(!rows_match(
            &anchor(0, assistant, "✅"),
            &sample("dash", assistant, "---")
        ));
        assert!(!rows_match(
            &anchor(1, assistant, "🎉"),
            &sample("check", assistant, "✅")
        ));
        assert!(!rows_match(
            &anchor(2, assistant, "✅"),
            &sample("same", assistant, "✅")
        ));
        assert!(
            align(
                &[anchor(0, assistant, "✅"), anchor(1, assistant, "---"),],
                &[sample("a", assistant, "---"), sample("b", assistant, "🎉"),],
            )
            .is_empty()
        );
    }

    #[test]
    fn one_empty_skeleton_still_mismatches_only_when_the_other_side_differs() {
        let assistant = AnchorGlyph::Assistant;
        // One side empty was already a length mismatch. The both-empty rule
        // must leave that, and must leave a short equal pair, alone.
        assert!(!rows_match(
            &anchor(0, assistant, "✅"),
            &sample("word", assistant, "hello")
        ));
        assert!(!rows_match(
            &anchor(0, assistant, "hello"),
            &sample("marks", assistant, "---")
        ));
        assert!(rows_match(
            &anchor(0, assistant, "ab"),
            &sample("k", assistant, "ab")
        ));
        assert!(rows_match(
            &anchor(0, assistant, "hello"),
            &sample("k", assistant, "hello")
        ));
        assert!(rows_match(
            &anchor(0, assistant, "abcdef"),
            &sample("k", assistant, "abcdefgh")
        ));
    }

    #[test]
    fn align_prefers_the_newest_transcript_anchors() {
        let assistant = AnchorGlyph::Assistant;
        let screen = [
            anchor(0, assistant, "bbbbbb"),
            anchor(1, assistant, "aaaaaa"),
        ];
        let transcript = [
            sample("A", assistant, "aaaaaa"),
            sample("B", assistant, "bbbbbb"),
        ];
        assert_eq!(
            align(&screen, &transcript),
            vec![Anchoring {
                row: 0,
                key: "B".to_string(),
            }]
        );

        let repeated = [
            anchor(0, assistant, "Read(auth.rs)"),
            anchor(1, assistant, "Read(auth.rs)"),
        ];
        let copies = ["k1", "k2", "k3"]
            .into_iter()
            .map(|key| sample(key, assistant, "Read(auth.rs)"))
            .collect::<Vec<_>>();
        assert_eq!(
            align(&repeated, &copies),
            vec![
                Anchoring {
                    row: 0,
                    key: "k2".to_string(),
                },
                Anchoring {
                    row: 1,
                    key: "k3".to_string(),
                },
            ]
        );
    }

    #[test]
    fn align_keeps_an_older_anchor_when_that_makes_a_longer_match() {
        let assistant = AnchorGlyph::Assistant;
        let screen = [
            anchor(0, assistant, "oldunique"),
            anchor(4, assistant, "newunique"),
        ];
        let transcript = [
            sample("old", assistant, "oldunique"),
            sample("skip", assistant, "nomatchhere"),
            sample("new", assistant, "newunique"),
        ];
        assert_eq!(
            align(&screen, &transcript),
            vec![
                Anchoring {
                    row: 0,
                    key: "old".to_string(),
                },
                Anchoring {
                    row: 4,
                    key: "new".to_string(),
                },
            ]
        );
    }

    #[test]
    fn align_considers_only_the_last_transcript_window() {
        let assistant = AnchorGlyph::Assistant;
        let mut transcript = Vec::new();
        transcript.push(sample("dropped", assistant, "uniqueoldanchor"));
        for index in 0..MAX_TRANSCRIPT_ANCHORS {
            transcript.push(sample(
                &format!("kept-{index}"),
                assistant,
                "othertextvalue",
            ));
        }
        // The oldest of the kept window is `kept-0`, which is still inside the 512.
        transcript[1] = sample("kept-0", assistant, "oldestkeptanchor");

        assert!(align(&[anchor(0, assistant, "uniqueoldanchor")], &transcript).is_empty());
        assert_eq!(
            align(&[anchor(3, assistant, "oldestkeptanchor")], &transcript),
            vec![Anchoring {
                row: 3,
                key: "kept-0".to_string(),
            }]
        );
    }

    #[test]
    fn screen_rows_fill_holes_and_keep_empty_lines() {
        let rows = screen_rows(
            [
                (-1, 0, 'z'),
                (1, 3, 'b'),
                (1, 0, 'a'),
                (1, 0, 'A'),
                (0, 0, 'q'),
                (0, 2, ' '),
                (4, 0, 'x'),
                (2, 1, 'c'),
            ]
            .into_iter(),
            4,
        );
        assert_eq!(
            rows,
            vec![
                screen(0, "q"),
                screen(1, "A  b"),
                screen(2, " c"),
                screen(3, ""),
            ]
        );
    }
}
