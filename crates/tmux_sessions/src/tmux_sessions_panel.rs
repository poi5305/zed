use collections::HashSet;
use gpui::{
    AnyElement, AsyncWindowContext, Entity, EventEmitter, FocusHandle, Focusable, FutureExt as _,
    Render, Task, WeakEntity,
};
use rpc::{AnyProtoClient, proto};
use std::collections::HashMap;
use std::time::Duration;
use task::{RevealStrategy, SpawnInTerminal, TaskId};
use terminal_view::terminal_panel::TerminalPanel;
use ui::{Disclosure, ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use remote::tmux_sessions::list_tmux_sessions;

use crate::{ToggleFocus, tmux_attach_command};

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
                self.expanded_sessions.retain(|name| names.contains(name));
            }
            Err(error) => {
                // Logged as well as drawn: the panel has to be open to be read, and in
                // the browser these panels have already spent a phase being impossible
                // to open, which made everything they had to say unreachable.
                log::error!("tmux sessions: {error:#}");
                self.error = Some(error.to_string().into());
            }
        }
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
                    .disabled(self.loading)
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

        // The disclosure is a child of the row rather than `ListItem::toggle`,
        // which draws it at `left(rems(-1.))` — outside a row whose indent level
        // is zero, where the panel clips it. `ButtonLike` stops click
        // propagation, so toggling here does not also attach to the session.
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
                    .child(Icon::new(IconName::Terminal).size(IconSize::Small)),
            )
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
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;

    /// The text of a listing failure this test recognizes among whatever else the test
    /// binary logs.
    const LISTING_FAILURE: &str = "tmux-listing-probe: the host refused";

    /// Every `log` record this test binary emits, so that a test can prove a failure was
    /// reported to the log and not only to the panel's own state.
    ///
    /// Keyed by the thread that logged it: these tests run concurrently in one binary,
    /// and an assertion that something was NOT logged would otherwise be reading another
    /// test's records. Every site under test logs from the foreground thread the test
    /// itself drives.
    static CAPTURED_RECORDS: Mutex<Vec<(std::thread::ThreadId, String)>> = Mutex::new(Vec::new());

    struct CapturingLogger;

    static CAPTURING_LOGGER: CapturingLogger = CapturingLogger;

    impl log::Log for CapturingLogger {
        fn enabled(&self, _metadata: &log::Metadata) -> bool {
            true
        }

        fn log(&self, record: &log::Record) {
            if let Ok(mut captured) = CAPTURED_RECORDS.lock() {
                captured.push((
                    std::thread::current().id(),
                    format!("{}: {}", record.level(), record.args()),
                ));
            }
        }

        fn flush(&self) {}
    }

    /// `log`'s logger slot is global and write-once, so this is installed once per test
    /// binary and never removed. Records are never cleared: tests run concurrently, and
    /// each one finds its own by the text it put in the failure.
    fn capture_log_records() {
        static INSTALLED: OnceLock<()> = OnceLock::new();
        INSTALLED.get_or_init(|| {
            log::set_logger(&CAPTURING_LOGGER)
                .expect("no other logger may be installed in this test binary");
            log::set_max_level(log::LevelFilter::Trace);
        });
    }

    /// What this thread has logged so far.
    fn captured_records() -> Vec<String> {
        let this_thread = std::thread::current().id();
        CAPTURED_RECORDS
            .lock()
            .expect("reading the captured records")
            .iter()
            .filter(|(thread, _)| *thread == this_thread)
            .map(|(_, record)| record.clone())
            .collect()
    }

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
            expanded_sessions: HashSet::default(),
            loading: false,
            error: None,
            _refresh: Task::ready(()),
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

    /// A listing that failed has to reach the log, not only the panel.
    ///
    /// The panel is the only place this failure is written down today, and a panel has
    /// to be open to be read. In the browser this fork's three panels spent a whole
    /// phase being impossible to open at all, so anything they had to say about a failed
    /// load was said to nobody. The log is the channel that does not depend on the panel
    /// being reachable.
    #[gpui::test]
    async fn a_failed_listing_is_logged_as_well_as_drawn(cx: &mut TestAppContext) {
        capture_log_records();
        let client = AnyProtoClient::new(SilentHost::never_answering(cx.executor()));
        let panel = panel_for(client, cx);

        panel.update(cx, |panel, cx| {
            panel.apply_listing(Err(anyhow::anyhow!("{LISTING_FAILURE}")), cx)
        });

        let drawn = panel.read_with(cx, |panel, _| panel.error.clone());
        assert_eq!(
            drawn.as_deref(),
            Some(LISTING_FAILURE),
            "the panel must still say so itself; expected {LISTING_FAILURE:?}, got {drawn:?}"
        );

        let records = captured_records();
        let about_this_failure: Vec<&String> = records
            .iter()
            .filter(|record| record.contains(LISTING_FAILURE))
            .collect();
        assert_eq!(
            about_this_failure,
            vec![&format!("ERROR: tmux sessions: {LISTING_FAILURE}")],
            "a failed listing must be logged once, at error level so that the browser's \
             Info-level release filter keeps it; expected \
             [\"ERROR: tmux sessions: {LISTING_FAILURE}\"], got {about_this_failure:?} \
             out of {} records this test binary logged",
            records.len()
        );
    }

    /// What the log line above must not wrongly announce: a listing that worked.
    ///
    /// A panel that logs an error every time it refreshes trains its reader to ignore
    /// the log, which puts the failure back where it started.
    #[gpui::test]
    async fn a_listing_that_succeeded_logs_nothing(cx: &mut TestAppContext) {
        capture_log_records();
        let client = AnyProtoClient::new(SilentHost::answering_after(
            Duration::ZERO,
            one_session(),
            cx.executor(),
        ));
        let panel = panel_for(client, cx);

        panel.update(cx, |panel, cx| panel.refresh(cx));
        cx.run_until_parked();

        let drawn = panel.read_with(cx, |panel, _| panel.error.clone());
        assert_eq!(
            drawn, None,
            "a listing that arrived leaves nothing to report; expected None, got {drawn:?}"
        );
        let records = captured_records();
        let about_tmux: Vec<&String> = records
            .iter()
            .filter(|record| record.contains("tmux sessions: "))
            .collect();
        assert!(
            about_tmux.is_empty(),
            "a listing that succeeded must log nothing about itself; expected [], got \
             {about_tmux:?}"
        );
    }
}
