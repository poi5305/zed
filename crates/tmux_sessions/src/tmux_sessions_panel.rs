use collections::HashSet;
use gpui::{
    Animation, AnimationExt as _, AnyElement, AsyncWindowContext, ElementId, Entity, EventEmitter,
    FocusHandle, Focusable, FutureExt as _, Render, Subscription, Task, WeakEntity,
    pulsating_between,
};
use project_manager::{OpenFolders, mark_open_project_row};
use rpc::{AnyProtoClient, proto};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use task::{RevealStrategy, SpawnInTerminal, TaskId};
use terminal_view::{REATTACHABLE_TASK_ID_PREFIX, terminal_panel::TerminalPanel};
use ui::{Disclosure, ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use remote::tmux_sessions::list_tmux_sessions;

use crate::{
    BadgeTone, ClaudeActivity, KeepAliveBadge, LinkedClaudeSession, ToggleFocus,
    claude_session_links, linked_claude_session, tmux_attach_command,
};

const TMUX_SESSIONS_PANEL_KEY: &str = "TmuxSessionsPanel";

const TMUX_MISSING_REMOTE: &str = "No tmux binary was found on the remote host.";

const TMUX_MISSING_LOCAL: &str = "No tmux binary was found on this machine.";

const NO_SESSIONS_REMOTE: &str = "No tmux sessions are running on the remote host.";

const NO_SESSIONS_LOCAL: &str = "No tmux sessions are running on this machine.";

const ASKING_REMOTE: &str = "Asking the remote host for its tmux sessions…";

const ASKING_LOCAL: &str = "Looking for tmux sessions on this machine…";

const UNANSWERED_REMOTE: &str = "The remote host did not answer. Refresh to ask again.";

const UNANSWERED_LOCAL: &str = "The listing failed. Refresh to try again.";

/// How long a listing waits for the host before it stops calling itself unanswered.
///
/// The panel asks once and has no poll of its own, so a request that is never answered
/// is not merely slow: it is the panel's final state. Generous enough that a host merely
/// busy enough to take a while still gets to answer, and the refresh button re-asks.
const TMUX_LISTING_TIMEOUT: Duration = Duration::from_secs(20);

pub struct TmuxSessionsPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    position: DockPosition,
    /// `None` when the window is open on a local project, whose tmux server is
    /// listed by running tmux here rather than by asking a remote host.
    remote_client: Option<AnyProtoClient>,
    sessions: Vec<proto::TmuxSession>,
    /// False only when the remote host has no tmux binary; a host with tmux
    /// installed but no server running reports true with no sessions.
    tmux_available: bool,
    /// Sessions whose windows are hidden, by session name. A name that is absent is
    /// expanded, so the windows (and any Claude session in them) are visible before a click.
    collapsed_sessions: HashSet<String>,
    loading: bool,
    error: Option<SharedString>,
    _refresh: Task<()>,
    /// Retried from `render` until the Claude panel exists to subscribe to.
    _claude_links: Option<Subscription>,
}

impl TmuxSessionsPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            TmuxSessionsPanel::new(workspace, window, cx)
        })
    }

    pub fn new(
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let workspace_handle = workspace.weak_handle();
        let project = workspace.project().clone();

        cx.new(|cx| {
            let remote_client = project
                .read(cx)
                .remote_client()
                .map(|client| client.read(cx).proto_client());

            let mut this = Self {
                workspace: workspace_handle,
                focus_handle: cx.focus_handle(),
                position: DockPosition::Left,
                remote_client,
                sessions: Vec::new(),
                tmux_available: true,
                collapsed_sessions: HashSet::default(),
                loading: false,
                error: None,
                _refresh: Task::ready(()),
                _claude_links: None,
            };
            this.refresh(cx);
            this
        })
    }

    /// Lists the tmux server of whichever machine the project is open on: the
    /// remote host through its server, a local project by running tmux here.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.error = None;
        self._refresh = match self.remote_client.clone() {
            Some(client) => {
                let response = client.request(proto::ListTmuxSessions {
                    project_id: rpc::proto::REMOTE_SERVER_PROJECT_ID,
                });
                cx.spawn(async move |this, cx| {
                    let listing = match response
                        .with_timeout(TMUX_LISTING_TIMEOUT, cx.background_executor())
                        .await
                    {
                        Ok(response) => {
                            response.map(|response| (response.tmux_available, response.sessions))
                        }
                        Err(_) => Err(unanswered_within(TMUX_LISTING_TIMEOUT)),
                    };
                    this.update(cx, |this, cx| this.apply_listing(listing, cx))
                        .log_err();
                })
            }
            None => {
                // Listing shells out twice, so it is kept off the thread that
                // draws the window.
                let listing = cx.background_spawn(list_tmux_sessions());
                cx.spawn(async move |this, cx| {
                    let listing = match listing
                        .with_timeout(TMUX_LISTING_TIMEOUT, cx.background_executor())
                        .await
                    {
                        Ok(listing) => listing.map(|listing| {
                            (
                                listing.tmux_available,
                                listing.sessions.into_iter().map(proto_session).collect(),
                            )
                        }),
                        Err(_) => Err(unanswered_within(TMUX_LISTING_TIMEOUT)),
                    };
                    this.update(cx, |this, cx| this.apply_listing(listing, cx))
                        .log_err();
                })
            }
        };
        cx.notify();
    }

    /// The tail both listing paths share, so that there is one place the panel's
    /// state is brought up to date from a listing rather than two.
    fn apply_listing(
        &mut self,
        listing: anyhow::Result<(bool, Vec<proto::TmuxSession>)>,
        cx: &mut Context<Self>,
    ) {
        self.loading = false;
        match listing {
            Ok((tmux_available, sessions)) => {
                self.tmux_available = tmux_available;
                self.sessions = sessions;
                // A session that went away should not keep its name in the
                // expanded set forever.
                let names: HashSet<String> = self
                    .sessions
                    .iter()
                    .map(|session| session.name.clone())
                    .collect();
                self.collapsed_sessions.retain(|name| names.contains(name));
            }
            Err(error) => self.error = Some(error.to_string().into()),
        }
        cx.notify();
    }

    fn toggle_session(&mut self, session_name: &str, cx: &mut Context<Self>) {
        if !self.collapsed_sessions.remove(session_name) {
            self.collapsed_sessions.insert(session_name.to_string());
        }
        cx.notify();
    }

    fn attach(
        &mut self,
        session_name: &str,
        window_index: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let command = tmux_attach_command(session_name, window_index);
        let label = match window_index {
            Some(index) => format!("tmux: {session_name}:{index}"),
            None => format!("tmux: {session_name}"),
        };

        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(terminal_panel) = workspace.read(cx).panel::<TerminalPanel>(cx) else {
            self.error = Some("The terminal panel is not available.".into());
            cx.notify();
            return;
        };

        let spawn = SpawnInTerminal {
            id: TaskId(format!("{REATTACHABLE_TASK_ID_PREFIX}{label}")),
            full_label: label.clone(),
            label,
            command: Some(command.clone()),
            command_label: command,
            // Attaching twice to the same session is a legitimate thing to want,
            // so an existing tab for it is neither reused nor killed.
            use_new_terminal: true,
            allow_concurrent_runs: true,
            reveal: RevealStrategy::Always,
            env: HashMap::default(),
            ..Default::default()
        };

        // Reported into the panel rather than only logged: a failure to attach leaves no
        // terminal behind, so a log line is the only trace of it and reads to the user as
        // a click that did nothing at all.
        let spawned = terminal_panel.update(cx, |terminal_panel, cx| {
            terminal_panel.spawn_task(&spawn, window, cx)
        });
        cx.spawn(async move |this, cx| {
            if let Err(error) = spawned.await {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("Attaching to the session: {error:#}").into());
                    cx.notify();
                })
                .log_err();
            }
        })
        .detach();
    }

    /// `None` when the host did not report the window's working directory, which is what
    /// a remote server older than this panel answers with.
    fn open_project_button(
        tmux_window: &proto::TmuxWindow,
        session_index: usize,
        window_index: usize,
        cx: &mut Context<Self>,
    ) -> Option<IconButton> {
        if tmux_window.current_path.is_empty() {
            return None;
        }
        let folder = PathBuf::from(&tmux_window.current_path);
        Some(
            IconButton::new(
                SharedString::from(format!(
                    "tmux-window-project-{session_index}-{window_index}"
                )),
                IconName::FolderOpen,
            )
            .icon_size(IconSize::XSmall)
            .tooltip(Tooltip::text(format!(
                "Open project for {} in a new window",
                tmux_window.current_path
            )))
            .on_click(cx.listener(move |this, _, window, cx| {
                let folder = folder.clone();
                this.workspace
                    .update(cx, |workspace, cx| {
                        project_manager::open_folder_in_new_window(workspace, folder, window, cx)
                    })
                    .log_err();
            })),
        )
    }

    fn observe_claude_links(&mut self, cx: &mut Context<Self>) {
        if self._claude_links.is_some() {
            return;
        }
        let Some(links) = claude_session_links(cx) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        self._claude_links = links.observe(&workspace, cx);
    }

    fn current_claude_links(&self, cx: &App) -> Vec<LinkedClaudeSession> {
        let Some(links) = claude_session_links(cx) else {
            return Vec::new();
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return Vec::new();
        };
        links.linked_sessions(workspace.read(cx), cx)
    }

    fn open_linked_claude(
        &mut self,
        session_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(links) = claude_session_links(cx) else {
            self.error = Some("The Claude sessions panel is not available.".into());
            cx.notify();
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        if let Err(error) = links.open(workspace, session_id, window, cx) {
            self.error = Some(error.to_string().into());
            cx.notify();
        }
    }

    fn render_toolbar(
        &self,
        links: &[LinkedClaudeSession],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .p_1()
            .gap_1()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Label::new("Tmux Sessions")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(listing_summary(&self.sessions, links))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(
                IconButton::new("tmux-sessions-refresh", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .disabled(self.loading)
                    .tooltip(Tooltip::text("Refresh"))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
    }

    fn render_session(
        &self,
        index: usize,
        links: &[LinkedClaudeSession],
        open_folders: &OpenFolders,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let session = self.sessions.get(index)?;
        let window_is_open: Vec<bool> = session
            .windows
            .iter()
            .map(|tmux_window| open_folders.contains(Path::new(&tmux_window.current_path)))
            .collect();
        let any_window_open = window_is_open.iter().any(|is_open| *is_open);
        let name = session.name.clone();
        let expanded = !self.collapsed_sessions.contains(&name);
        let window_count = session.window_count;
        let attached = session.attached;
        let hosted: Vec<&LinkedClaudeSession> = session
            .windows
            .iter()
            .filter_map(|tmux_window| linked_claude_session(links, &tmux_window.id))
            .collect();
        let any_waiting = hosted
            .iter()
            .any(|link| matches!(link.activity, ClaudeActivity::Waiting(_)));

        // The disclosure is a child of the row rather than `ListItem::toggle`,
        // which draws it at `left(rems(-1.))` — outside a row whose indent level
        // is zero, where the panel clips it. `ButtonLike` stops click
        // propagation, so toggling here does not also open the session.
        let header = ListItem::new(SharedString::from(format!("tmux-session-{index}")))
            .spacing(ListItemSpacing::Sparse)
            .start_slot(
                h_flex()
                    .gap_1()
                    .child(
                        Disclosure::new(
                            SharedString::from(format!("tmux-session-toggle-{index}")),
                            expanded,
                        )
                        .tooltip(Tooltip::text(if expanded {
                            "Hide windows"
                        } else {
                            "Show windows"
                        }))
                        .on_click(cx.listener({
                            let name = name.clone();
                            move |this, _, _, cx| this.toggle_session(&name, cx)
                        })),
                    )
                    .child(Icon::new(IconName::Terminal).size(IconSize::Small).color(
                        if attached {
                            Color::Accent
                        } else {
                            Color::Muted
                        },
                    )),
            )
            .child(
                h_flex()
                    .gap_1()
                    .overflow_hidden()
                    .child(Label::new(name.clone()).single_line())
                    .child(
                        Label::new(if window_count == 1 {
                            "1 window".to_string()
                        } else {
                            format!("{window_count} windows")
                        })
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .when(attached, |this| {
                        this.child(
                            Label::new("attached")
                                .size(LabelSize::Small)
                                .color(Color::Accent),
                        )
                    })
                    .when(!hosted.is_empty(), |this| {
                        this.child(Icon::new(IconName::AiClaude).size(IconSize::XSmall).color(
                            if any_waiting {
                                Color::Warning
                            } else {
                                Color::Muted
                            },
                        ))
                        .child(
                            Label::new(hosted.len().to_string())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    }),
            )
            .end_slot(
                IconButton::new(
                    SharedString::from(format!("tmux-session-terminal-{index}")),
                    IconName::Terminal,
                )
                .icon_size(IconSize::XSmall)
                .tooltip(Tooltip::text("Open session in terminal"))
                .on_click(cx.listener({
                    let name = name.clone();
                    move |this, _, window, cx| this.attach(&name, None, window, cx)
                })),
            )
            .tooltip(Tooltip::text(if expanded {
                "Hide windows"
            } else {
                "Show windows"
            }))
            .on_click(cx.listener({
                let name = name.clone();
                move |this, _, _, cx| this.toggle_session(&name, cx)
            }));

        let windows = expanded.then(|| {
            session
                .windows
                .iter()
                .enumerate()
                .map(|(window_index, tmux_window)| {
                    let row = self.render_window(
                        index,
                        &name,
                        window_index,
                        tmux_window,
                        linked_claude_session(links, &tmux_window.id),
                        cx,
                    );
                    let is_open = window_is_open.get(window_index).copied().unwrap_or(false);
                    mark_open_project_row(row, is_open, cx)
                })
                .collect::<Vec<_>>()
        });

        Some(
            v_flex()
                .child(mark_open_project_row(header, any_window_open, cx))
                .children(windows.into_iter().flatten())
                .into_any_element(),
        )
    }

    fn keep_alive_chip(
        session_index: usize,
        window_index: usize,
        session_id: &str,
        badge: KeepAliveBadge,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        let session_id = session_id.to_string();
        let tooltip = badge.tooltip.clone();
        let color = match badge.tone {
            BadgeTone::Accent => Color::Accent,
            BadgeTone::Warning => Color::Warning,
            BadgeTone::Muted => Color::Muted,
        };
        let icon = if badge.enabled {
            IconName::Flame
        } else {
            IconName::Clock
        };
        h_flex()
            .id(SharedString::from(format!(
                "tmux-keep-alive-{session_index}-{window_index}"
            )))
            .gap_0p5()
            .cursor_pointer()
            .tooltip(move |_, cx| {
                Tooltip::with_meta(
                    "Keep prompt cache warm (click: off → warm → warm + compact)",
                    None,
                    tooltip.clone(),
                    cx,
                )
            })
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                if let Some(links) = claude_session_links(cx) {
                    links.toggle_keep_alive(&session_id, cx);
                }
            })
            .child(Icon::new(icon).size(IconSize::XSmall).color(color))
            .child(
                Label::new(badge.label)
                    .size(LabelSize::XSmall)
                    .color(color)
                    .single_line(),
            )
            .into_any_element()
    }

    fn render_window(
        &self,
        session_index: usize,
        session_name: &str,
        window_index: usize,
        tmux_window: &proto::TmuxWindow,
        linked: Option<&LinkedClaudeSession>,
        cx: &mut Context<Self>,
    ) -> ListItem {
        let target_index = tmux_window.index;
        let identity = h_flex()
            .gap_1()
            .overflow_hidden()
            .child(
                Label::new(format!("{}:", tmux_window.index))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(Label::new(tmux_window.name.clone()).single_line())
            .when(tmux_window.active, |this| {
                this.child(
                    Label::new("active")
                        .size(LabelSize::Small)
                        .color(Color::Accent),
                )
            });

        let row = ListItem::new(SharedString::from(format!(
            "tmux-window-{session_index}-{window_index}"
        )))
        .spacing(ListItemSpacing::Sparse)
        .indent_level(1);

        let open_project_button =
            Self::open_project_button(tmux_window, session_index, window_index, cx);

        let Some(linked) = linked else {
            let session_name = session_name.to_string();
            return row
                .start_slot(Icon::new(IconName::Screen).size(IconSize::Small).color(
                    if tmux_window.active {
                        Color::Accent
                    } else {
                        Color::Muted
                    },
                ))
                .child(identity)
                .end_slot(
                    h_flex().gap_0p5().children(open_project_button).child(
                        IconButton::new(
                            SharedString::from(format!(
                                "tmux-window-terminal-{session_index}-{window_index}"
                            )),
                            IconName::Terminal,
                        )
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text("Open window in terminal"))
                        .on_click(cx.listener({
                            let session_name = session_name.clone();
                            move |this, _, window, cx| {
                                this.attach(&session_name, Some(target_index), window, cx)
                            }
                        })),
                    ),
                )
                .tooltip(Tooltip::text(format!(
                    "Attach to {session_name}:{target_index}"
                )))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.attach(&session_name, Some(target_index), window, cx)
                }));
        };

        let session_id = linked.session_id.clone();
        let title = linked.title.clone();
        let session_name = session_name.to_string();
        row.start_slot(claude_status_dot(
            &linked.activity,
            SharedString::from(format!(
                "tmux-window-indicator-{session_index}-{window_index}"
            )),
        ))
        .child(
            v_flex().gap_0p5().overflow_hidden().child(identity).child(
                h_flex()
                    .gap_1()
                    .overflow_hidden()
                    .child(
                        Icon::new(IconName::AiClaude)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(title.clone())
                            .size(LabelSize::XSmall)
                            .single_line()
                            .truncate(),
                    )
                    .when_some(
                        claude_activity_label(&linked.activity),
                        |this, (text, color)| {
                            this.child(
                                Label::new(text)
                                    .size(LabelSize::XSmall)
                                    .color(color)
                                    .single_line(),
                            )
                        },
                    )
                    .when_some(linked.context.clone(), |this, context| {
                        this.child(
                            Label::new(context)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .single_line(),
                        )
                    })
                    .when_some(linked.keep_alive.clone(), |this, badge| {
                        this.child(Self::keep_alive_chip(
                            session_index,
                            window_index,
                            &linked.session_id,
                            badge,
                            cx,
                        ))
                    }),
            ),
        )
        .end_slot(
            h_flex()
                .gap_0p5()
                .children(open_project_button)
                .child(
                    IconButton::new(
                        SharedString::from(format!(
                            "tmux-window-claude-{session_index}-{window_index}"
                        )),
                        IconName::AiClaude,
                    )
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text("Open Claude session"))
                    .on_click(cx.listener({
                        let session_id = session_id.clone();
                        move |this, _, window, cx| this.open_linked_claude(&session_id, window, cx)
                    })),
                )
                .child(
                    IconButton::new(
                        SharedString::from(format!(
                            "tmux-window-terminal-{session_index}-{window_index}"
                        )),
                        IconName::Terminal,
                    )
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text("Open window in terminal"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.attach(&session_name, Some(target_index), window, cx)
                    })),
                ),
        )
        .tooltip(Tooltip::text(format!("Open Claude session {title}")))
        .on_click(
            cx.listener(move |this, _, window, cx| {
                this.open_linked_claude(&session_id, window, cx)
            }),
        )
    }

    /// What to show instead of the tree: there is always a reason a host has
    /// nothing to list, and a blank panel does not say which one it is. Which
    /// machine was asked is part of that reason, so the message names it.
    fn empty_message(&self) -> Option<&'static str> {
        let remote = self.remote_client.is_some();
        if !self.sessions.is_empty() {
            return None;
        }
        // Asked and not yet answered, which is not the same as having been told there is
        // nothing. The panel asks once, so a request that is never answered would
        // otherwise leave every field at the value an empty host produces and the panel
        // stating, permanently and with nothing to retract it, something it was never
        // told.
        if self.loading {
            return Some(if remote { ASKING_REMOTE } else { ASKING_LOCAL });
        }
        if self.error.is_some() {
            return Some(if remote {
                UNANSWERED_REMOTE
            } else {
                UNANSWERED_LOCAL
            });
        }
        if !self.tmux_available {
            return Some(if remote {
                TMUX_MISSING_REMOTE
            } else {
                TMUX_MISSING_LOCAL
            });
        }
        Some(if remote {
            NO_SESSIONS_REMOTE
        } else {
            NO_SESSIONS_LOCAL
        })
    }
}

impl Render for TmuxSessionsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.observe_claude_links(cx);
        let links = self.current_claude_links(cx);
        let empty_message = self.empty_message();
        let open_folders = self
            .workspace
            .upgrade()
            .map(|workspace| OpenFolders::of_workspace(workspace.read(cx), cx))
            .unwrap_or_default();

        v_flex()
            .key_context("TmuxSessionsPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(self.render_toolbar(&links, cx))
            .when_some(self.error.clone(), |this, error| {
                this.child(div().p_2().child(Label::new(error).color(Color::Error)))
            })
            .child(
                v_flex()
                    .id("tmux-sessions-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .when_some(empty_message, |this, message| {
                        this.child(
                            div().p_2().child(
                                Label::new(message)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                        )
                    })
                    .children(
                        (0..self.sessions.len()).filter_map(|index| {
                            self.render_session(index, &links, &open_folders, cx)
                        }),
                    ),
            )
    }
}

impl Focusable for TmuxSessionsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for TmuxSessionsPanel {}

impl Panel for TmuxSessionsPanel {
    fn persistent_name() -> &'static str {
        "TmuxSessionsPanel"
    }

    fn panel_key() -> &'static str {
        TMUX_SESSIONS_PANEL_KEY
    }

    fn activation_focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(300.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::TerminalAlt)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Tmux Sessions")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        10
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.refresh(cx);
        }
    }
}

fn listing_summary(sessions: &[proto::TmuxSession], links: &[LinkedClaudeSession]) -> String {
    let session_count = sessions.len();
    let sessions_label = if session_count == 1 {
        "1 session".to_string()
    } else {
        format!("{session_count} sessions")
    };
    let claude_count = sessions
        .iter()
        .flat_map(|session| session.windows.iter())
        .filter(|window| linked_claude_session(links, &window.id).is_some())
        .count();
    if claude_count == 0 {
        sessions_label
    } else {
        format!("{sessions_label} · {claude_count} Claude")
    }
}

/// `idle_for` already includes the "idle" word, so `Idle(Some(text))` is shown as that text.
fn claude_activity_label(activity: &ClaudeActivity) -> Option<(SharedString, Color)> {
    match activity {
        ClaudeActivity::Working => Some(("Working".into(), Color::Success)),
        ClaudeActivity::Waiting(waiting) => {
            Some((format!("Waiting: {waiting}").into(), Color::Warning))
        }
        ClaudeActivity::Idle(Some(idle)) => Some((idle.clone(), Color::Muted)),
        ClaudeActivity::Idle(None) => None,
    }
}

fn claude_status_dot(activity: &ClaudeActivity, id: impl Into<ElementId>) -> AnyElement {
    let (color, pulse) = match activity {
        ClaudeActivity::Waiting(_) => (Color::Warning, true),
        ClaudeActivity::Working => (Color::Success, true),
        ClaudeActivity::Idle(_) => (Color::Muted, false),
    };
    let dot = div().child(
        Icon::new(IconName::Indicator)
            .size(IconSize::XSmall)
            .color(color),
    );
    if pulse {
        dot.with_animation(
            id,
            Animation::new(Duration::from_secs(2))
                .repeat()
                .with_easing(pulsating_between(0.2, 0.8)),
            |dot, delta| dot.opacity(delta),
        )
        .into_any_element()
    } else {
        dot.into_any_element()
    }
}

/// What a listing that was never answered is reported as. The panel has no poll of its
/// own, so this is the state it is left in until the user refreshes.
fn unanswered_within(timeout: Duration) -> anyhow::Error {
    anyhow::anyhow!("no answer within {timeout:?}")
}

/// The panel holds the proto shape for both listing paths, so that the rendering
/// reads one type no matter which machine the sessions came from.
fn proto_session(session: remote::tmux_sessions::TmuxSession) -> proto::TmuxSession {
    proto::TmuxSession {
        name: session.name,
        attached: session.attached,
        window_count: session.window_count,
        windows: session
            .windows
            .into_iter()
            .map(|window| proto::TmuxWindow {
                index: window.index,
                name: window.name,
                active: window.active,
                id: window.id,
                current_path: window.current_path,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use rpc::{ProtoClient, ProtoMessageHandlerSet, proto::EnvelopedMessage};
    use std::future::Future;
    use std::sync::Arc;
    use std::time::Duration;

    /// Longer than any bound a listing may put on its own request, so that a panel that
    /// is still waiting after this has stopped waiting for a reason rather than because
    /// the test did not wait long enough.
    const LONG_ENOUGH_FOR_ANY_ANSWER: Duration = Duration::from_secs(120);

    /// A remote host that never answers, which is what this panel is being asked to
    /// survive: a request that is neither answered nor refused leaves every consumer
    /// holding its own initial state, and the question is what the panel says then.
    struct SilentHost {
        answer: parking_lot::Mutex<Option<proto::ListTmuxSessionsResponse>>,
        /// How long the answer, if there is one, takes to arrive.
        delay: Duration,
        executor: gpui::BackgroundExecutor,
        handlers: parking_lot::Mutex<ProtoMessageHandlerSet>,
    }

    impl SilentHost {
        fn never_answering(executor: gpui::BackgroundExecutor) -> Arc<Self> {
            Arc::new(Self {
                answer: parking_lot::Mutex::new(None),
                delay: Duration::ZERO,
                executor,
                handlers: parking_lot::Mutex::default(),
            })
        }

        fn answering_after(
            delay: Duration,
            answer: proto::ListTmuxSessionsResponse,
            executor: gpui::BackgroundExecutor,
        ) -> Arc<Self> {
            Arc::new(Self {
                answer: parking_lot::Mutex::new(Some(answer)),
                delay,
                executor,
                handlers: parking_lot::Mutex::default(),
            })
        }
    }

    impl ProtoClient for SilentHost {
        fn request(
            &self,
            _envelope: rpc::proto::Envelope,
            _request_type: &'static str,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = anyhow::Result<rpc::proto::Envelope>> + Send + 'static>,
        > {
            let answer = self.answer.lock().clone();
            let delay = self.delay;
            let executor = self.executor.clone();
            Box::pin(async move {
                let Some(answer) = answer else {
                    std::future::pending::<()>().await;
                    unreachable!("a host that never answers never returns");
                };
                executor.timer(delay).await;
                Ok(answer.into_envelope(0, None, None))
            })
        }

        fn send(
            &self,
            _envelope: rpc::proto::Envelope,
            _message_type: &'static str,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        fn send_response(
            &self,
            _envelope: rpc::proto::Envelope,
            _message_type: &'static str,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        fn message_handler_set(&self) -> &parking_lot::Mutex<ProtoMessageHandlerSet> {
            &self.handlers
        }

        fn is_via_collab(&self) -> bool {
            false
        }

        fn has_wsl_interop(&self) -> bool {
            false
        }
    }

    fn panel_for(client: AnyProtoClient, cx: &mut TestAppContext) -> Entity<TmuxSessionsPanel> {
        cx.new(|cx| TmuxSessionsPanel {
            workspace: WeakEntity::new_invalid(),
            focus_handle: cx.focus_handle(),
            position: DockPosition::Left,
            remote_client: Some(client),
            sessions: Vec::new(),
            tmux_available: true,
            collapsed_sessions: HashSet::default(),
            loading: false,
            error: None,
            _refresh: Task::ready(()),
            _claude_links: None,
        })
    }

    fn one_session() -> proto::ListTmuxSessionsResponse {
        proto::ListTmuxSessionsResponse {
            tmux_available: true,
            sessions: vec![proto::TmuxSession {
                name: "zed".to_string(),
                attached: true,
                window_count: 2,
                windows: Vec::new(),
            }],
        }
    }

    /// A host that has not answered has not said it has no sessions.
    ///
    /// The panel asks once, in its constructor, and every field it draws from starts at
    /// the value an empty host would produce. A request that is never answered therefore
    /// leaves it drawing "no tmux sessions are running on the remote host" over a host
    /// that is running several — an answer the user has no way to tell from a real one,
    /// and one nothing ever retracts.
    #[gpui::test]
    async fn a_host_that_has_not_answered_is_not_reported_as_having_no_sessions(
        cx: &mut TestAppContext,
    ) {
        let client = AnyProtoClient::new(SilentHost::never_answering(cx.executor()));
        let panel = panel_for(client, cx);

        panel.update(cx, |panel, cx| panel.refresh(cx));
        cx.executor().advance_clock(LONG_ENOUGH_FOR_ANY_ANSWER);
        cx.run_until_parked();

        let message = panel.read_with(cx, |panel, _| panel.empty_message());
        assert_ne!(
            message,
            Some(NO_SESSIONS_REMOTE),
            "a host that has not answered must not be reported as having no sessions; \
             expected anything but {NO_SESSIONS_REMOTE:?}, got {message:?}"
        );
        assert_ne!(
            message, None,
            "a panel with nothing to draw must say why; expected a message, got {message:?}"
        );
    }

    /// The bound above must not swallow the real answer.
    ///
    /// A host that answers "no sessions" is a normal state with its own message, and a
    /// panel that has been told that must say it rather than go on saying it is waiting.
    #[gpui::test]
    async fn a_host_that_answers_with_no_sessions_is_reported_as_having_none(
        cx: &mut TestAppContext,
    ) {
        let client = AnyProtoClient::new(SilentHost::answering_after(
            Duration::ZERO,
            proto::ListTmuxSessionsResponse {
                tmux_available: true,
                sessions: Vec::new(),
            },
            cx.executor(),
        ));
        let panel = panel_for(client, cx);

        panel.update(cx, |panel, cx| panel.refresh(cx));
        cx.run_until_parked();

        let message = panel.read_with(cx, |panel, _| panel.empty_message());
        assert_eq!(
            message,
            Some(NO_SESSIONS_REMOTE),
            "a host that answered with an empty listing has said it has no sessions; \
             expected {NO_SESSIONS_REMOTE:?}, got {message:?}"
        );
    }

    /// What a bound on the request could wrongly kill: a host that is merely slow.
    ///
    /// The answer is the same answer whether it took a millisecond or most of the bound,
    /// so a listing that arrives before the bound has to be drawn rather than discarded.
    #[gpui::test]
    async fn a_slow_host_that_does_answer_is_still_listed(cx: &mut TestAppContext) {
        let nearly_the_bound = TMUX_LISTING_TIMEOUT.saturating_sub(Duration::from_secs(1));
        let client = AnyProtoClient::new(SilentHost::answering_after(
            nearly_the_bound,
            one_session(),
            cx.executor(),
        ));
        let panel = panel_for(client, cx);

        panel.update(cx, |panel, cx| panel.refresh(cx));
        cx.executor()
            .advance_clock(nearly_the_bound + Duration::from_millis(1));
        cx.run_until_parked();

        let (message, names) = panel.read_with(cx, |panel, _| {
            (
                panel.empty_message(),
                panel
                    .sessions
                    .iter()
                    .map(|session| session.name.clone())
                    .collect::<Vec<_>>(),
            )
        });
        assert_eq!(
            names,
            vec!["zed".to_string()],
            "a host that answered just inside the bound must still be listed; \
             expected [\"zed\"], got {names:?}"
        );
        assert_eq!(
            message, None,
            "a panel with sessions to draw has no empty message; expected None, got {message:?}"
        );
    }
}
