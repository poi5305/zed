mod tmux_sessions_button;
mod tmux_sessions_panel;

use gpui::{App, actions};
use workspace::Workspace;

pub use tmux_sessions_button::TmuxSessionsButton;
pub use tmux_sessions_panel::TmuxSessionsPanel;

actions!(
    tmux_sessions,
    [
        /// Toggles focus on the tmux sessions panel.
        ToggleFocus
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<TmuxSessionsPanel>(window, cx);
        });
    })
    .detach();
}

/// Wraps a value in POSIX single quotes so that it reaches `tmux` as one
/// argument no matter which characters a session or window name contains.
///
/// A single quote cannot be escaped inside single quotes, so it is closed,
/// escaped outside, and reopened. The value is always quoted rather than only
/// when it looks dangerous, so that there is no predicate to get wrong.
pub fn shell_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for character in value.chars() {
        if character == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    quoted
}

/// Builds the shell command that attaches to a tmux session, or to one window
/// of it when `window_index` is given.
///
/// The command is handed to the terminal as a single shell command line rather
/// than as a program and arguments, so the target has to be quoted here.
pub fn tmux_attach_command(session_name: &str, window_index: Option<u32>) -> String {
    let target = match window_index {
        Some(index) => format!("={session_name}:{index}"),
        None => format!("={session_name}"),
    };
    format!("tmux attach -t {}", shell_quote(&target))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tmux_attach_command_quotes_plain_names() {
        assert_eq!(tmux_attach_command("work", None), "tmux attach -t '=work'");
        assert_eq!(
            tmux_attach_command("work", Some(2)),
            "tmux attach -t '=work:2'"
        );
    }

    #[test]
    fn test_tmux_attach_command_quotes_names_with_spaces() {
        assert_eq!(
            tmux_attach_command("my session", None),
            "tmux attach -t '=my session'",
            "a name with a space has to stay one argument"
        );
        assert_eq!(
            tmux_attach_command("my session", Some(3)),
            "tmux attach -t '=my session:3'",
            "the window index is part of the quoted target, not a separate word"
        );
    }

    #[test]
    fn test_tmux_attach_command_neutralizes_shell_metacharacters() {
        assert_eq!(
            tmux_attach_command("a; rm -rf /", None),
            "tmux attach -t '=a; rm -rf /'"
        );
        assert_eq!(
            tmux_attach_command("$(id)", None),
            "tmux attach -t '=$(id)'",
            "command substitution does not happen inside single quotes"
        );
        assert_eq!(tmux_attach_command("`id`", None), "tmux attach -t '=`id`'");
        assert_eq!(
            tmux_attach_command("a'; rm -rf /; #", None),
            "tmux attach -t '=a'\\''; rm -rf /; #'",
            "a name that closes the quote is escaped rather than ending the argument"
        );
        assert_eq!(
            tmux_attach_command("a'b", Some(1)),
            "tmux attach -t '=a'\\''b:1'"
        );
    }

    #[test]
    fn test_tmux_attach_command_quotes_glob_characters_with_exact_match() {
        assert_eq!(
            tmux_attach_command("app*", None),
            "tmux attach -t '=app*'",
            "session names with glob characters must use = prefix for exact match"
        );
        assert_eq!(
            tmux_attach_command("app*", Some(1)),
            "tmux attach -t '=app*:1'"
        );
    }

    /// Re-runs the quoting through a real shell so that the escaping is checked
    /// against `sh` rather than only against the expected string.
    #[test]
    #[cfg(unix)]
    // Blocking on `/bin/sh` is what makes this test check the escaping against
    // a real shell rather than against another copy of the expected string.
    #[allow(clippy::disallowed_methods)]
    fn test_shell_quote_round_trips_through_sh() {
        for value in [
            "work",
            "my session",
            "a'; rm -rf /; #",
            "$(id)",
            "`id`",
            "a\"b",
            "back\\slash",
            "new\nline",
        ] {
            let output = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("printf %s {}", shell_quote(value)))
                .output()
                .expect("could not run /bin/sh");
            assert!(
                output.status.success(),
                "sh rejected the quoting of {value:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                value,
                "sh must see {value:?} as exactly one unchanged argument"
            );
        }
    }
}
