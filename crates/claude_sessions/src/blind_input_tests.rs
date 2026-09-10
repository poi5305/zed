#[cfg(test)]
mod blind_input_tests {
    use remote::claude_sessions::*;

    #[test]
    fn a_registration_tmux_field_yields_only_the_pane_id_segment() {
        assert_eq!(
            pane_target("awp:@1.%1").as_deref(),
            Some("%1"),
            "the window id must be dropped and only the pane id kept, \
             because the pane id alone is unique across the tmux server"
        );
    }

    #[test]
    fn a_session_name_containing_colons_does_not_prevent_extracting_the_pane_id() {
        assert_eq!(
            pane_target("a:b:@10.%234").as_deref(),
            Some("%234"),
            "colons belong to the session name, so the multi-digit pane id must still be found"
        );
    }

    #[test]
    fn a_pane_target_with_a_non_numeric_pane_id_is_rejected() {
        assert_eq!(
            pane_target("awp:@1.%x"),
            None,
            "a pane id that is not all decimal digits has the wrong shape"
        );
    }

    #[test]
    fn a_tmux_field_without_a_pane_segment_is_rejected() {
        assert_eq!(
            pane_target("awp:@1"),
            None,
            "there is no `%<digits>` tail segment to take"
        );
    }

    #[test]
    fn a_pane_id_with_no_digits_after_the_percent_sign_is_rejected() {
        assert_eq!(
            pane_target("awp:@1.%"),
            None,
            "`%` on its own is not a pane id"
        );
    }

    #[test]
    fn an_empty_tmux_field_is_rejected() {
        assert_eq!(
            pane_target(""),
            None,
            "a registration with an empty tmux field has no pane to target"
        );
    }

    #[test]
    fn a_tmux_field_that_looks_like_a_tmux_flag_is_rejected() {
        assert_eq!(
            pane_target("-t"),
            None,
            "the value is passed as the value of tmux's -t argument, \
             so anything tmux could read as a flag must be refused"
        );
    }

    #[test]
    fn a_pane_id_prefixed_with_a_dash_is_rejected() {
        assert_eq!(
            pane_target("awp:@1.%-1"),
            None,
            "a leading dash could be read by tmux as a flag"
        );
    }

    #[test]
    fn a_tmux_field_carrying_a_semicolon_and_a_shell_command_is_rejected() {
        assert_eq!(
            pane_target("%1 ; rm -rf /"),
            None,
            "a semicolon is a tmux command separator and must never reach it"
        );
    }

    #[test]
    fn a_pane_id_followed_by_whitespace_is_rejected() {
        assert_eq!(
            pane_target("awp:@1.%1 "),
            None,
            "trailing whitespace means the tail segment is not exactly `%<digits>`"
        );
    }

    #[test]
    fn a_pane_id_preceded_by_whitespace_is_rejected() {
        assert_eq!(
            pane_target("awp:@1. %1"),
            None,
            "leading whitespace means the tail segment is not exactly `%<digits>`"
        );
    }

    #[test]
    fn a_pane_id_containing_an_embedded_space_is_rejected() {
        assert_eq!(
            pane_target("awp:@1.%1 2"),
            None,
            "a space inside the pane id would split into two tmux arguments"
        );
    }

    #[test]
    fn a_pane_id_followed_by_a_newline_is_rejected() {
        assert_eq!(
            pane_target("awp:@1.%1\n"),
            None,
            "a newline is whitespace and must not reach tmux"
        );
    }

    #[test]
    fn a_tmux_field_containing_a_double_quote_is_rejected() {
        assert_eq!(
            pane_target("awp:@1.%1\""),
            None,
            "quotes are refused outright, which is why only the pane id is kept"
        );
    }

    #[test]
    fn a_tmux_field_containing_a_single_quote_is_rejected() {
        assert_eq!(
            pane_target("awp:@1.%'1'"),
            None,
            "quotes are refused outright, which is why only the pane id is kept"
        );
    }

    #[test]
    fn a_pane_id_written_with_non_ascii_digits_is_rejected() {
        assert_eq!(
            pane_target("awp:@1.%\u{0661}\u{0662}"),
            None,
            "the pane id must be decimal digits, not merely numeric characters"
        );
    }
}
