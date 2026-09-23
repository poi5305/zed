mod tmux_sessions_panel;

use std::sync::Arc;

use gpui::{App, Context, Entity, Global, SharedString, Subscription, Window, actions};
use workspace::Workspace;

pub use tmux_sessions_panel::TmuxSessionsPanel;

/// What a linked Claude session is doing, in the same terms the Claude panel uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaudeActivity {
    Working,
    Waiting(SharedString),
    Idle(Option<SharedString>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkedClaudeSession {
    pub session_id: String,
    /// The tmux window id the session runs in, e.g. `@3`.
    pub window_id: String,
    pub title: SharedString,
    pub activity: ClaudeActivity,
    /// The context the newest answer was given, as the Claude panel shows it (`214K`).
    pub context: Option<SharedString>,
    /// Keep-alive chip for this window. `None` hides it.
    pub keep_alive: Option<KeepAliveBadge>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeepAliveBadge {
    pub enabled: bool,
    pub label: SharedString,
    pub tone: BadgeTone,
    pub tooltip: SharedString,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadgeTone {
    Accent,
    Warning,
    Muted,
}

pub trait ClaudeSessionLinks: 'static {
    /// Every live Claude session that runs in a tmux window, as the Claude panel of `workspace` knows them.
    fn linked_sessions(&self, workspace: &Workspace, cx: &App) -> Vec<LinkedClaudeSession>;
    /// Re-render `cx`'s panel whenever that list may have changed. `None` while the Claude panel of this
    /// workspace has not been created yet (the tmux panel retries later).
    ///
    /// Takes the workspace entity rather than `&Workspace` from `workspace.read(cx)`: that reference
    /// borrows `cx` for as long as it lives, and registering the subscription needs `&mut Context`.
    fn observe(
        &self,
        workspace: &Entity<Workspace>,
        cx: &mut Context<TmuxSessionsPanel>,
    ) -> Option<Subscription>;
    /// Turns prompt-cache keep-alive on or off for `session_id`.
    fn toggle_keep_alive(&self, session_id: &str, cx: &mut App);
    /// Opens the Claude session's tab. Errors are returned for the tmux panel to show.
    fn open(
        &self,
        workspace: Entity<Workspace>,
        session_id: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> anyhow::Result<()>;
}

struct ClaudeSessionLinksState(Arc<dyn ClaudeSessionLinks>);

impl Global for ClaudeSessionLinksState {}

pub fn set_claude_session_links(links: Arc<dyn ClaudeSessionLinks>, cx: &mut App) {
    cx.set_global(ClaudeSessionLinksState(links));
}

pub fn claude_session_links(cx: &App) -> Option<Arc<dyn ClaudeSessionLinks>> {
    cx.try_global::<ClaudeSessionLinksState>()
        .map(|state| Arc::clone(&state.0))
}

/// The linked session running in `window_id`. `None` when `window_id` is empty.
pub fn linked_claude_session<'a>(
    links: &'a [LinkedClaudeSession],
    window_id: &str,
) -> Option<&'a LinkedClaudeSession> {
    if window_id.is_empty() {
        return None;
    }
    links.iter().find(|link| link.window_id == window_id)
}

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
    use gpui::SharedString;

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

    #[test]
    fn test_linked_claude_session_matches_by_id_and_rejects_an_empty_id() {
        let links = [
            LinkedClaudeSession {
                session_id: "session-1".to_string(),
                window_id: "@12".to_string(),
                title: SharedString::from("editor"),
                activity: ClaudeActivity::Working,
                context: None,
                keep_alive: None,
            },
            LinkedClaudeSession {
                session_id: "blank".to_string(),
                window_id: String::new(),
                title: SharedString::from("untitled"),
                activity: ClaudeActivity::Idle(None),
                context: None,
                keep_alive: None,
            },
        ];

        assert_eq!(
            linked_claude_session(&links, "@12").map(|link| link.session_id.as_str()),
            Some("session-1")
        );
        assert_eq!(linked_claude_session(&links, ""), None);
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
