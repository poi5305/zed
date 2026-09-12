//! Listing of the tmux sessions and windows of the machine this code runs on.
//! The parsers are deliberately free of IO so that the exact output of the
//! `-F` format strings can be tested from a captured sample, and the IO wrapper
//! does nothing but run the two commands and hand their text to the parsers.

use anyhow::Result;

use crate::claude_sessions::is_zed_mirror_session;

/// Chosen over tmux's default output so that the columns are fixed rather than
/// dependent on the tmux version's human-readable formatting.
pub const LIST_SESSIONS_FORMAT: &str =
    "#{session_name}\t#{?session_attached,1,0}\t#{session_windows}";

pub const LIST_WINDOWS_FORMAT: &str =
    "#{session_name}\t#{window_index}\t#{window_name}\t#{?window_active,1,0}";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TmuxWindow {
    pub index: u32,
    pub name: String,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TmuxSession {
    pub name: String,
    pub attached: bool,
    pub window_count: u32,
    pub windows: Vec<TmuxWindow>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TmuxSessionList {
    pub sessions: Vec<TmuxSession>,
    /// False only when the host has no `tmux` binary at all. A host with tmux
    /// installed but no server running reports true with no sessions.
    pub tmux_available: bool,
}

/// Parses the output of `tmux list-sessions -F LIST_SESSIONS_FORMAT`.
///
/// Splitting from the right keeps a session name that itself contains a tab
/// attached to the row it came from, since only the two trailing columns have a
/// fixed shape. Rows that are short of columns or whose window count is not a
/// number are dropped individually, so that one malformed row cannot hide the
/// rows after it. `no server running on ...` has no tab at all and so parses as
/// no sessions rather than as an error.
pub fn parse_tmux_sessions(list_sessions_stdout: &str) -> Vec<TmuxSession> {
    let mut sessions = Vec::new();
    for line in list_sessions_stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut columns = line.rsplitn(3, '\t');
        let (Some(window_count), Some(attached), Some(name)) =
            (columns.next(), columns.next(), columns.next())
        else {
            continue;
        };
        let Ok(window_count) = window_count.trim().parse::<u32>() else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        sessions.push(TmuxSession {
            name: name.to_string(),
            attached: attached == "1",
            window_count,
            windows: Vec::new(),
        });
    }
    sessions
}

/// Parses the output of `tmux list-windows -a -F LIST_WINDOWS_FORMAT`, pairing
/// each window with the name of the session that owns it.
///
/// A row whose window index is not a number is dropped on its own; the rows
/// after it are still parsed.
pub fn parse_tmux_windows(list_windows_stdout: &str) -> Vec<(String, TmuxWindow)> {
    let mut windows = Vec::new();
    for line in list_windows_stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let columns: Vec<&str> = line.split('\t').collect();
        let [session_name, index, name, active] = columns.as_slice() else {
            continue;
        };
        let Ok(index) = index.trim().parse::<u32>() else {
            continue;
        };
        if session_name.is_empty() {
            continue;
        }
        windows.push((
            session_name.to_string(),
            TmuxWindow {
                index,
                name: name.to_string(),
                active: *active == "1",
            },
        ));
    }
    windows
}

/// Files each window under the session it names, ordered by window index.
/// Windows whose session was not listed are dropped, which is what happens when
/// a session is killed between the two commands.
pub fn merge_sessions_and_windows(
    mut sessions: Vec<TmuxSession>,
    windows: Vec<(String, TmuxWindow)>,
) -> Vec<TmuxSession> {
    for (session_name, window) in windows {
        let Some(session) = sessions
            .iter_mut()
            .find(|session| session.name == session_name)
        else {
            continue;
        };
        session.windows.push(window);
    }
    for session in sessions.iter_mut() {
        session.windows.sort_by_key(|window| window.index);
    }
    sessions
}

/// Lists the tmux sessions of the machine this is running on.
pub async fn list_tmux_sessions() -> Result<TmuxSessionList> {
    let list_sessions = util::command::new_command("tmux")
        .args(["list-sessions", "-F", LIST_SESSIONS_FORMAT])
        .output()
        .await;

    let list_sessions = match list_sessions {
        Ok(output) => output,
        // The only failure that means anything to the user is a missing binary;
        // everything else is reported as "tmux is there, it just has nothing to
        // show", which is also what a running-but-empty server looks like.
        Err(error) => {
            log::debug!("could not run tmux list-sessions: {error}");
            return Ok(TmuxSessionList::default());
        }
    };

    // `tmux list-sessions` exits non-zero with "no server running on ..." when
    // no server has been started yet, which is a normal state and not an error.
    let mut sessions = parse_tmux_sessions(&String::from_utf8_lossy(&list_sessions.stdout));
    // The mirrors Zed groups with a session to hold a terminal on one of its windows
    // are its own bookkeeping, not sessions the user started; see
    // [`crate::claude_sessions::attach_arguments`].
    sessions.retain(|session| !is_zed_mirror_session(&session.name));
    if sessions.is_empty() {
        return Ok(TmuxSessionList {
            sessions: Vec::new(),
            tmux_available: true,
        });
    }

    let list_windows = util::command::new_command("tmux")
        .args(["list-windows", "-a", "-F", LIST_WINDOWS_FORMAT])
        .output()
        .await;
    let windows = match list_windows {
        Ok(output) => parse_tmux_windows(&String::from_utf8_lossy(&output.stdout)),
        Err(error) => {
            log::debug!("could not run tmux list-windows: {error}");
            Vec::new()
        }
    };

    Ok(TmuxSessionList {
        sessions: merge_sessions_and_windows(sessions, windows),
        tmux_available: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_tmux_sessions_reads_every_row() {
        let stdout = "work\t1\t3\npersonal\t0\t1\n";

        let sessions = parse_tmux_sessions(stdout);

        assert_eq!(
            sessions,
            vec![
                TmuxSession {
                    name: "work".to_string(),
                    attached: true,
                    window_count: 3,
                    windows: Vec::new(),
                },
                TmuxSession {
                    name: "personal".to_string(),
                    attached: false,
                    window_count: 1,
                    windows: Vec::new(),
                },
            ]
        );
    }

    #[test]
    fn test_parse_tmux_sessions_treats_no_server_as_no_sessions() {
        assert_eq!(parse_tmux_sessions(""), Vec::new());
        assert_eq!(parse_tmux_sessions("\n\n"), Vec::new());
        assert_eq!(
            parse_tmux_sessions("no server running on /tmp/tmux-1000/default\n"),
            Vec::new(),
            "the message tmux prints when no server has been started is not a session"
        );
    }

    #[test]
    fn test_parse_tmux_sessions_keeps_names_with_spaces_and_colons() {
        let stdout = "my session\t0\t2\nrelease:1.2\t1\t1\nweird $(id) `x` \"q\"\t0\t1\n";

        let sessions = parse_tmux_sessions(stdout);

        assert_eq!(
            sessions
                .iter()
                .map(|session| session.name.as_str())
                .collect::<Vec<_>>(),
            vec!["my session", "release:1.2", "weird $(id) `x` \"q\""],
            "only tabs separate the columns, so every other character belongs to the name"
        );
        assert_eq!(sessions.get(1).map(|session| session.attached), Some(true));
    }

    #[test]
    fn test_parse_tmux_windows_groups_rows_by_session() {
        let stdout = concat!(
            "work\t0\teditor\t1\n",
            "work\t1\tserver\t0\n",
            "personal\t0\tshell\t1\n",
        );

        let windows = parse_tmux_windows(stdout);

        assert_eq!(
            windows,
            vec![
                (
                    "work".to_string(),
                    TmuxWindow {
                        index: 0,
                        name: "editor".to_string(),
                        active: true,
                    }
                ),
                (
                    "work".to_string(),
                    TmuxWindow {
                        index: 1,
                        name: "server".to_string(),
                        active: false,
                    }
                ),
                (
                    "personal".to_string(),
                    TmuxWindow {
                        index: 0,
                        name: "shell".to_string(),
                        active: true,
                    }
                ),
            ]
        );
    }

    #[test]
    fn test_parse_tmux_windows_skips_only_the_malformed_row() {
        let stdout = concat!(
            "work\t0\teditor\t1\n",
            "work\tnot-a-number\tbroken\t0\n",
            "work\t2\tlogs\t0\n",
            "work\t3\n",
            "work\t4\tbuild\t0\n",
        );

        let windows = parse_tmux_windows(stdout);

        assert_eq!(
            windows
                .iter()
                .map(|(_, window)| (window.index, window.name.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "editor"), (2, "logs"), (4, "build")],
            "a row with a non-numeric index and a row short of columns are each dropped alone, \
             and the rows after them are still parsed"
        );
    }

    #[test]
    fn test_parse_tmux_sessions_skips_only_the_malformed_row() {
        let stdout = concat!(
            "first\t1\t2\n",
            "broken\t1\tmany\n",
            "second\t0\t1\n",
            "no-tabs-here\n",
            "third\t0\t4\n",
        );

        let sessions = parse_tmux_sessions(stdout);

        assert_eq!(
            sessions
                .iter()
                .map(|session| (session.name.as_str(), session.window_count))
                .collect::<Vec<_>>(),
            vec![("first", 2), ("second", 1), ("third", 4)],
            "a non-numeric window count and a row without tabs are dropped alone"
        );
    }

    #[test]
    fn test_merge_sessions_and_windows_orders_windows_by_index() {
        let sessions = parse_tmux_sessions("work\t1\t2\npersonal\t0\t1\n");
        let windows = parse_tmux_windows(concat!(
            "work\t2\tlogs\t0\n",
            "work\t0\teditor\t1\n",
            "personal\t5\tshell\t1\n",
            "ghost\t0\tgone\t0\n",
        ));

        let merged = merge_sessions_and_windows(sessions, windows);

        assert_eq!(
            merged
                .iter()
                .map(|session| (
                    session.name.as_str(),
                    session
                        .windows
                        .iter()
                        .map(|window| window.index)
                        .collect::<Vec<_>>()
                ))
                .collect::<Vec<_>>(),
            vec![("work", vec![0, 2]), ("personal", vec![5])],
            "windows are sorted by index and a window of an unlisted session is dropped"
        );
    }
}
