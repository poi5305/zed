use collections::HashSet;
use gpui::{
    AnyElement, AsyncWindowContext, Entity, EventEmitter, FocusHandle, Focusable, Render, Task,
    WeakEntity,
};
use rpc::{AnyProtoClient, proto};
use std::collections::HashMap;
use task::{RevealStrategy, SpawnInTerminal, TaskId};
use terminal_view::terminal_panel::TerminalPanel;
use ui::{ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::{ToggleFocus, tmux_attach_command};

const TMUX_SESSIONS_PANEL_KEY: &str = "TmuxSessionsPanel";

const NOT_REMOTE: &str = "Open a remote project to list the tmux sessions running on that host.";

const TMUX_MISSING: &str = "No tmux binary was found on the remote host.";

const NO_SESSIONS: &str = "No tmux sessions are running on the remote host.";

pub struct TmuxSessionsPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    position: DockPosition,
    /// `None` when the window is open on a local project, which has no remote
    /// host to ask.
    remote_client: Option<AnyProtoClient>,
    sessions: Vec<proto::TmuxSession>,
    /// False only when the remote host has no tmux binary; a host with tmux
    /// installed but no server running reports true with no sessions.
    tmux_available: bool,
    /// Sessions whose windows are shown, by session name.
    expanded_sessions: HashSet<String>,
    loading: bool,
    error: Option<SharedString>,
    _refresh: Task<()>,
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
                expanded_sessions: HashSet::default(),
                loading: false,
                error: None,
                _refresh: Task::ready(()),
            };
            this.refresh(cx);
            this
        })
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.remote_client.clone() else {
            self.sessions.clear();
            self.loading = false;
            cx.notify();
            return;
        };

        let response = client.request(proto::ListTmuxSessions {
            project_id: rpc::proto::REMOTE_SERVER_PROJECT_ID,
        });
        self.loading = true;
        self.error = None;
        self._refresh = cx.spawn(async move |this, cx| {
            let response = response.await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match response {
                    Ok(response) => {
                        this.tmux_available = response.tmux_available;
                        this.sessions = response.sessions;
                        // A session that went away should not keep its name in
                        // the expanded set forever.
                        let names: HashSet<String> = this
                            .sessions
                            .iter()
                            .map(|session| session.name.clone())
                            .collect();
                        this.expanded_sessions.retain(|name| names.contains(name));
                    }
                    Err(error) => this.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    fn toggle_session(&mut self, session_name: &str, cx: &mut Context<Self>) {
        if !self.expanded_sessions.remove(session_name) {
            self.expanded_sessions.insert(session_name.to_string());
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
            id: TaskId(format!("tmux-attach-{label}")),
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

        terminal_panel
            .update(cx, |terminal_panel, cx| {
                terminal_panel.spawn_task(&spawn, window, cx)
            })
            .detach_and_log_err(cx);
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .p_1()
            .gap_1()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new("Tmux Sessions")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                IconButton::new("tmux-sessions-refresh", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .disabled(self.remote_client.is_none() || self.loading)
                    .tooltip(Tooltip::text("Refresh"))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
    }

    fn render_session(&self, index: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let session = self.sessions.get(index)?;
        let name = session.name.clone();
        let expanded = self.expanded_sessions.contains(&name);
        let window_count = session.window_count;
        let attached = session.attached;

        let header = ListItem::new(SharedString::from(format!("tmux-session-{index}")))
            .spacing(ListItemSpacing::Sparse)
            .toggle(expanded)
            .on_toggle(cx.listener({
                let name = name.clone();
                move |this, _, _, cx| this.toggle_session(&name, cx)
            }))
            .start_slot(Icon::new(IconName::Terminal).size(IconSize::Small))
            .child(
                h_flex()
                    .gap_1()
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
                    }),
            )
            .tooltip(Tooltip::text(format!("Attach to {name}")))
            .on_click(cx.listener({
                let name = name.clone();
                move |this, _, window, cx| this.attach(&name, None, window, cx)
            }));

        let windows = expanded.then(|| {
            session
                .windows
                .iter()
                .enumerate()
                .map(|(window_index, tmux_window)| {
                    let session_name = name.clone();
                    let target_index = tmux_window.index;
                    ListItem::new(SharedString::from(format!(
                        "tmux-window-{index}-{window_index}"
                    )))
                    .spacing(ListItemSpacing::Sparse)
                    .indent_level(1)
                    .start_slot(Icon::new(IconName::Screen).size(IconSize::Small).color(
                        if tmux_window.active {
                            Color::Accent
                        } else {
                            Color::Muted
                        },
                    ))
                    .child(
                        h_flex()
                            .gap_1()
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
                            }),
                    )
                    .tooltip(Tooltip::text(format!(
                        "Attach to {session_name}:{target_index}"
                    )))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.attach(&session_name, Some(target_index), window, cx)
                    }))
                })
                .collect::<Vec<_>>()
        });

        Some(
            v_flex()
                .child(header)
                .children(windows.into_iter().flatten())
                .into_any_element(),
        )
    }

    /// What to show instead of the tree: there is always a reason a remote host
    /// has nothing to list, and a blank panel does not say which one it is.
    fn empty_message(&self) -> Option<&'static str> {
        if self.remote_client.is_none() {
            return Some(NOT_REMOTE);
        }
        if !self.tmux_available {
            return Some(TMUX_MISSING);
        }
        if self.sessions.is_empty() {
            return Some(NO_SESSIONS);
        }
        None
    }
}

impl Render for TmuxSessionsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let empty_message = self.empty_message();

        v_flex()
            .key_context("TmuxSessionsPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(self.render_toolbar(cx))
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
                        (0..self.sessions.len()).filter_map(|index| self.render_session(index, cx)),
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

    /// Hidden on a local project, which has no remote host to list tmux on.
    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        self.remote_client.as_ref().map(|_| IconName::TerminalAlt)
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
