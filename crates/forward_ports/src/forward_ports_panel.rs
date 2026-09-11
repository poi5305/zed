use collections::HashSet;
use editor::{Editor, EditorElement, EditorStyle};
use gpui::{
    AsyncWindowContext, Entity, EntityId, EventEmitter, FocusHandle, Focusable, FontStyle, Global,
    ReadGlobal as _, Render, Subscription, TextStyle, WeakEntity,
};
use recent_projects::RemoteSettings;
use remote::{
    DockerConnectionOptions, PortForwardStatus, PortForwardStore, RemoteClient,
    RemoteConnectionOptions,
};
use rpc::{AnyProtoClient, proto::REMOTE_SERVER_PROJECT_ID};
use settings::{
    RemoteSettingsContent, Settings as _, SettingsFile, SettingsStore, SshPortForwardOption,
    update_settings_file,
};
use std::collections::HashMap;
use theme_settings::ThemeSettings;
use ui::{ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotificationId,
};

use crate::{
    ConnectionEntry as _, ConnectionKey, DEFAULT_FORWARD_HOST, DEFAULT_LOCAL_BIND_HOST,
    ForwardConnection, LOCAL_PORT_SEARCH_LIMIT, PortForwardDraft, ToggleFocus,
    apply_port_forward_edit, can_add_forward, choose_local_port, connection_is_editable,
    connection_key_for_options, connection_label, connection_label_for_key, describe_port_forward,
    local_port_is_configured,
    port_detection::{AutoForwardAction, OnAutoForward, auto_forward_action, on_auto_forward},
    port_detector::{PortDetector, PortDetectorEvent},
    port_forward_endpoints, port_forwards_for_key_mut, remove_port_forward, tunnelled_forwards,
    validate_port_forward,
};

const FORWARD_PORTS_PANEL_KEY: &str = "ForwardPortsPanel";

/// Shown for a connection that is not currently open, where there is nothing to
/// report a live state for.
const FORWARD_STATUS_INACTIVE: &str = "Inactive";

/// Said for a forward the reader disconnected by hand, which has no live state of its
/// own to report and is not the same thing as one belonging to another connection.
const FORWARD_STATUS_DISCONNECTED: &str = "Disconnected";

const CONNECTION_NOT_EDITABLE: &str = "This connection is not defined in your user settings file, so its port forwards cannot be changed here.";

/// A dev container entry is created from the options of the live connection,
/// so there is nothing to write one from until this window is open on it.
const DEV_CONTAINER_NOT_CONNECTED: &str = "This dev container has no entry in your settings yet. Open it in this window once so Zed can record the running container's details, then add forwards from anywhere.";

const EXTERNAL_FORWARD_EXPLANATION: &str = "Established by ssh -L when the connection was made. Zed did not bind this port and cannot confirm whether it is actually listening.";

#[derive(Clone)]
struct ConnectionForwards {
    key: ConnectionKey,
    label: String,
    forwards: Vec<SshPortForwardOption>,
}

struct PortForwardForm {
    connection: ConnectionKey,
    /// The entry being edited, or `None` when adding a new forward.
    original: Option<SshPortForwardOption>,
    local_host: Entity<Editor>,
    local_port: Entity<Editor>,
    remote_host: Entity<Editor>,
    remote_port: Entity<Editor>,
    error: Option<SharedString>,
}

pub struct ForwardPortsPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    connections: Vec<ConnectionForwards>,
    form: Option<PortForwardForm>,
    error: Option<SharedString>,
    position: DockPosition,
    /// The tunnels for the connection this window is open on, when it is remote.
    port_forwards: Option<Entity<PortForwardStore>>,
    connected_connection: Option<ConnectionKey>,
    /// The live options of the dev container this window is open on. Nothing
    /// else writes `remote.dev_container_connections`, so these are what a new
    /// entry is built from when the user adds their first forward to it.
    connected_dev_container: Option<DockerConnectionOptions>,
    /// Forwards that were already configured when the connection was made are
    /// carried by the transport itself (`ssh -L`), so binding them again here
    /// would only collide with the ssh process.
    established_at_connect: Vec<SshPortForwardOption>,
    /// Forwards the reader has disconnected by hand. Held for this session rather
    /// than written to the settings file: the button says "disconnect", not
    /// "remove", and a forward that is configured is expected back when the window
    /// is opened on the connection again. Delete is what removes one for good.
    disconnected: HashSet<SshPortForwardOption>,
    /// Kept so that a detector which gave up can be built again without
    /// reopening the connection.
    proto_client: Option<AnyProtoClient>,
    /// Watches the remote host for servers that were started after the
    /// connection was made.
    _port_detector: Option<Entity<PortDetector>>,
    _port_detector_subscription: Option<Subscription>,
    /// Why the detector gave up, while there is no detector running.
    port_detector_stopped: Option<SharedString>,
    auto_forward: OnAutoForward,
    _subscriptions: Vec<Subscription>,
}

/// Identifies the notification a detected port gets, so that a port which is
/// still listening on the next scan does not stack up notifications.
struct DetectedPortNotification;

/// Identifies the notification a failed forward reports itself through.
struct ForwardFailureNotification;

/// One store per remote connection: several windows can be open on the same
/// connection, but its message handlers may only be registered once.
#[derive(Default)]
struct PortForwardStores(HashMap<EntityId, Entity<PortForwardStore>>);

impl Global for PortForwardStores {}

fn port_forward_store(client: &Entity<RemoteClient>, cx: &mut App) -> Entity<PortForwardStore> {
    let client_id = client.entity_id();
    if let Some(existing) = cx
        .default_global::<PortForwardStores>()
        .0
        .get(&client_id)
        .cloned()
    {
        return existing;
    }

    let proto_client = client.read(cx).proto_client();
    let store = cx.new(|_| PortForwardStore::new(REMOTE_SERVER_PROJECT_ID, proto_client.clone()));
    proto_client.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &store);
    PortForwardStore::init_local(&proto_client);
    cx.observe_release(client, move |_, cx| {
        cx.default_global::<PortForwardStores>()
            .0
            .remove(&client_id);
    })
    .detach();
    cx.default_global::<PortForwardStores>()
        .0
        .insert(client_id, store.clone());
    store
}

/// The icon for the far end of a forward, which is whatever the connection reaches:
/// the same icons the rest of Zed draws for an ssh host, a wsl distribution and a
/// container.
fn remote_icon_for_key(key: &ConnectionKey) -> IconName {
    match key {
        ConnectionKey::Ssh { .. } => IconName::Server,
        ConnectionKey::Wsl { .. } => IconName::Linux,
        ConnectionKey::DevContainer { .. } => IconName::Box,
    }
}

fn single_line_editor(
    placeholder: &'static str,
    text: &str,
    window: &mut Window,
    cx: &mut Context<ForwardPortsPanel>,
) -> Entity<Editor> {
    let text = text.to_string();
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder, window, cx);
        if !text.is_empty() {
            editor.set_text(text, window, cx);
        }
        editor
    })
}

impl ForwardPortsPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            ForwardPortsPanel::new(workspace, window, cx)
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
            let mut subscriptions =
                vec![cx.observe_global::<SettingsStore>(|this: &mut Self, cx| this.reload(cx))];

            let remote_client = project.read(cx).remote_client();
            let mut port_forwards = None;
            let mut connected_connection = None;
            let mut connected_dev_container = None;
            let mut established_at_connect = Vec::new();
            let mut proto_client = None;
            if let Some(remote_client) = remote_client {
                let options = remote_client.read(cx).connection_options();
                connected_connection = connection_key_for_options(&options);
                match &options {
                    // Only ssh carries forwards itself, via `-L`.
                    RemoteConnectionOptions::Ssh(options) => {
                        established_at_connect = options.port_forwards.clone().unwrap_or_default();
                    }
                    RemoteConnectionOptions::Docker(options) => {
                        connected_dev_container = Some(options.clone());
                    }
                    _ => {}
                }

                proto_client = Some(remote_client.read(cx).proto_client());
                let store = port_forward_store(&remote_client, cx);
                subscriptions.push(cx.observe(&store, |_: &mut Self, _, cx| cx.notify()));
                port_forwards = Some(store);
            }

            let mut this = Self {
                workspace: workspace_handle,
                focus_handle: cx.focus_handle(),
                connections: Vec::new(),
                form: None,
                error: None,
                position: DockPosition::Left,
                port_forwards,
                connected_connection,
                connected_dev_container,
                established_at_connect,
                disconnected: HashSet::default(),
                proto_client,
                _port_detector: None,
                _port_detector_subscription: None,
                port_detector_stopped: None,
                auto_forward: OnAutoForward::default(),
                _subscriptions: subscriptions,
            };
            this.start_port_detector(cx);
            this.reload(cx);
            this
        })
    }

    /// Builds the detector, whether for the first time or after it gave up, so
    /// that a run of failed scans does not disable detection for the rest of
    /// the session. Does nothing when this window is not open on a remote.
    fn start_port_detector(&mut self, cx: &mut Context<Self>) {
        if self._port_detector.is_some() {
            return;
        }
        let Some(proto_client) = self.proto_client.clone() else {
            return;
        };
        let detector = cx.new(|cx| PortDetector::new(REMOTE_SERVER_PROJECT_ID, proto_client, cx));
        self._port_detector_subscription =
            Some(cx.subscribe(&detector, Self::on_port_detector_event));
        self._port_detector = Some(detector);
        self.port_detector_stopped = None;
    }

    /// Turns the configuration for the connection this window is open on into
    /// running tunnels.
    fn sync_tunnels(&mut self, cx: &mut Context<Self>) {
        let (Some(store), Some(key)) = (
            self.port_forwards.clone(),
            self.connected_connection.clone(),
        ) else {
            return;
        };
        let configured = self
            .connections
            .iter()
            .find(|connection| connection.key == key)
            .map(|connection| connection.forwards.clone())
            .unwrap_or_default();
        let established = self.established_at_connect.clone();
        let tunnelled = tunnelled_forwards(&configured, &established, &self.disconnected);

        store.update(cx, |store, cx| {
            store.set_forwards(tunnelled, cx);
            store.set_externally_forwarded(established, cx);
        });
    }

    fn forward_status(
        &self,
        key: &ConnectionKey,
        forward: &SshPortForwardOption,
        cx: &App,
    ) -> Option<PortForwardStatus> {
        if self.connected_connection.as_ref() != Some(key) {
            return None;
        }
        self.port_forwards
            .as_ref()?
            .read(cx)
            .status(forward)
            .cloned()
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let connections = {
            let settings = RemoteSettings::get_global(cx);
            self.auto_forward = on_auto_forward(settings.auto_forward_ports);
            settings
                .ssh_connections()
                .map(ForwardConnection::Ssh)
                .chain(settings.wsl_connections().map(ForwardConnection::Wsl))
                .chain(
                    settings
                        .dev_container_connections()
                        .map(ForwardConnection::DevContainer),
                )
                .map(|connection| ConnectionForwards {
                    key: connection.connection_key(),
                    label: connection_label(&connection),
                    forwards: connection.port_forwards(),
                })
                .collect()
        };
        self.connections = connections;
        // The connection this window is on belongs in the list whether or not the
        // settings file has an entry for it. It is the connection whose ports are
        // being detected and the one a forward is almost always meant for, and
        // without a row of its own a reader connected from a URI or an ssh config
        // host sees a panel that does not mention the host they are on.
        if let Some(key) = self.connected_connection.clone() {
            if !self
                .connections
                .iter()
                .any(|connection| connection.key == key)
            {
                self.connections.insert(
                    0,
                    ConnectionForwards {
                        label: connection_label_for_key(&key),
                        key,
                        forwards: Vec::new(),
                    },
                );
            }
        }
        self.sync_tunnels(cx);
        cx.notify();
    }

    fn update_remote_settings(
        &mut self,
        cx: &mut Context<Self>,
        update: impl FnOnce(&mut RemoteSettingsContent, &App) + Send + Sync + 'static,
    ) {
        let Some(fs) = self
            .workspace
            .read_with(cx, |workspace, _| workspace.app_state().fs.clone())
            .log_err()
        else {
            return;
        };
        update_settings_file(fs, cx, move |content, cx| update(&mut content.remote, cx));
    }

    fn user_remote_settings<'a>(&self, cx: &'a App) -> Option<&'a RemoteSettingsContent> {
        SettingsStore::global(cx)
            .get_content_for_file(SettingsFile::User)
            .map(|content| &content.remote)
    }

    fn connection_is_editable(&self, key: &ConnectionKey, cx: &App) -> bool {
        connection_is_editable(
            self.user_remote_settings(cx),
            key,
            self.connected_dev_container.as_ref(),
        )
    }

    fn can_add_forward(&self, key: &ConnectionKey, cx: &App) -> bool {
        can_add_forward(
            self.user_remote_settings(cx),
            key,
            self.connected_dev_container.as_ref(),
        )
    }

    /// A dev container entry can be written from scratch only while this window
    /// holds the live connection whose details would fill it in.
    fn can_create_dev_container_entry(&self, key: &ConnectionKey) -> bool {
        self.connected_dev_container
            .as_ref()
            .is_some_and(|options| options.connection_key() == *key)
    }

    /// Routes a change to whichever settings list `key` belongs in, creating
    /// the dev container entry when this is its first forward.
    fn update_forwards(
        &mut self,
        key: ConnectionKey,
        cx: &mut Context<Self>,
        update: impl FnOnce(&mut Vec<SshPortForwardOption>) + Send + Sync + 'static,
    ) {
        let dev_container_options = self
            .can_create_dev_container_entry(&key)
            .then(|| self.connected_dev_container.clone())
            .flatten();
        self.update_remote_settings(cx, move |remote, _| {
            let Some(forwards) =
                port_forwards_for_key_mut(remote, &key, dev_container_options.as_ref())
            else {
                return;
            };
            update(forwards);
        });
    }

    fn open_form(
        &mut self,
        connection: ConnectionKey,
        original: Option<SshPortForwardOption>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let draft = original
            .as_ref()
            .map(PortForwardDraft::from_forward)
            .unwrap_or_default();

        let form = PortForwardForm {
            connection,
            original,
            local_host: single_line_editor(DEFAULT_FORWARD_HOST, &draft.local_host, window, cx),
            local_port: single_line_editor("8080", &draft.local_port, window, cx),
            remote_host: single_line_editor(DEFAULT_FORWARD_HOST, &draft.remote_host, window, cx),
            remote_port: single_line_editor("80", &draft.remote_port, window, cx),
            error: None,
        };
        form.local_port.focus_handle(cx).focus(window, cx);

        self.error = None;
        self.form = Some(form);
        cx.notify();
    }

    fn close_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.form.take().is_some() {
            self.focus_handle.focus(window, cx);
            cx.notify();
        }
    }

    fn save_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let key = form.connection.clone();
        let original = form.original.clone();
        let draft = PortForwardDraft {
            local_host: form.local_host.read(cx).text(cx),
            local_port: form.local_port.read(cx).text(cx),
            remote_host: form.remote_host.read(cx).text(cx),
            remote_port: form.remote_port.read(cx).text(cx),
        };

        let Some(connection) = self
            .connections
            .iter()
            .find(|connection| connection.key == key)
        else {
            self.close_form(window, cx);
            return;
        };
        let validated = validate_port_forward(
            &draft,
            &connection.label,
            &connection.forwards,
            original.as_ref(),
        );

        let forward = match validated {
            Ok(forward) => forward,
            Err(error) => {
                self.set_form_error(error.to_string(), cx);
                return;
            }
        };

        if !self.connection_is_editable(&key, cx) {
            self.set_form_error(CONNECTION_NOT_EDITABLE.to_string(), cx);
            return;
        }

        self.update_forwards(key, cx, move |forwards| {
            apply_port_forward_edit(forwards, original.as_ref(), forward);
        });

        self.close_form(window, cx);
    }

    fn set_form_error(&mut self, message: String, cx: &mut Context<Self>) {
        if let Some(form) = self.form.as_mut() {
            form.error = Some(message.into());
        }
        cx.notify();
    }

    fn delete_forward(
        &mut self,
        key: ConnectionKey,
        forward: SshPortForwardOption,
        cx: &mut Context<Self>,
    ) {
        if !self.connection_is_editable(&key, cx) {
            self.error = Some(CONNECTION_NOT_EDITABLE.into());
            cx.notify();
            return;
        }
        self.error = None;

        if self
            .form
            .as_ref()
            .is_some_and(|form| form.original.as_ref() == Some(&forward))
        {
            self.form = None;
        }

        self.update_forwards(key, cx, move |forwards| {
            remove_port_forward(forwards, &forward);
        });

        cx.notify();
    }

    fn on_port_detector_event(
        &mut self,
        _detector: Entity<PortDetector>,
        event: &PortDetectorEvent,
        cx: &mut Context<Self>,
    ) {
        let ports = match event {
            PortDetectorEvent::PortsAppeared(ports) => ports,
            PortDetectorEvent::Stopped { reason } => {
                self._port_detector = None;
                self._port_detector_subscription = None;
                self.port_detector_stopped = Some(reason.clone());
                cx.notify();
                return;
            }
        };
        let Some(key) = self.connected_connection.clone() else {
            return;
        };

        // The policy does not vary between the ports of one batch, so it is
        // decided once and `Ignore` drops the whole batch rather than one port.
        let forward_immediately = match auto_forward_action(self.auto_forward) {
            AutoForwardAction::Ignore => return,
            AutoForwardAction::Notify => None,
            AutoForwardAction::Forward { open_in_browser } => Some(open_in_browser),
        };

        for port in ports {
            if self.detected_port_is_forwarded(&key, port.port) {
                continue;
            }
            match forward_immediately {
                Some(open_in_browser) => {
                    self.forward_detected_port(key.clone(), port.port, open_in_browser, cx)
                }
                None => self.notify_detected_port(key.clone(), port.port, cx),
            }
        }
    }

    /// Offers the forward rather than making it: a port on the remote host is
    /// not something the user has asked to see on their own machine.
    fn notify_detected_port(&mut self, key: ConnectionKey, port: u16, cx: &mut Context<Self>) {
        let panel = cx.weak_entity();
        let toast = Toast::new(
            NotificationId::composite::<DetectedPortNotification>(SharedString::from(
                port.to_string(),
            )),
            format!("Port {port} is now listening on the remote host."),
        )
        .on_click("Forward", move |_window, cx| {
            let key = key.clone();
            panel
                .update(cx, |panel, cx| {
                    panel.forward_detected_port(key, port, false, cx);
                })
                .log_err();
        });

        self.workspace
            .update(cx, |workspace, cx| workspace.show_toast(toast, cx))
            .log_err();
    }

    /// Whether this forward can be connected or disconnected from here at all:
    /// only the connection this window holds has tunnels to start and stop.
    fn forward_is_tunnellable(&self, key: &ConnectionKey, forward: &SshPortForwardOption) -> bool {
        self.connected_connection.as_ref() == Some(key)
            && self.port_forwards.is_some()
            // A forward `ssh -L` already carries is not this panel's to open or close.
            && !self.established_at_connect.contains(forward)
    }

    fn connect_forward(&mut self, forward: SshPortForwardOption, cx: &mut Context<Self>) {
        if self.disconnected.remove(&forward) {
            self.sync_tunnels(cx);
            cx.notify();
        }
    }

    fn disconnect_forward(&mut self, forward: SshPortForwardOption, cx: &mut Context<Self>) {
        if self.disconnected.insert(forward) {
            self.sync_tunnels(cx);
            cx.notify();
        }
    }

    /// Shows a message as a notification as well as in the panel, for a failure
    /// that followed a gesture made outside the panel.
    fn report(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        let toast = Toast::new(
            NotificationId::unique::<ForwardFailureNotification>(),
            message.into(),
        );
        self.workspace
            .update(cx, |workspace, cx| workspace.show_toast(toast, cx))
            .log_err();
    }

    /// Whether the connection already forwards a local port that would collide
    /// with the one a detected port would take.
    fn detected_port_is_forwarded(&self, key: &ConnectionKey, port: u16) -> bool {
        self.configured_forwards(key)
            .is_some_and(|forwards| local_port_is_configured(forwards, port))
    }

    fn configured_forwards(&self, key: &ConnectionKey) -> Option<&[SshPortForwardOption]> {
        self.connections
            .iter()
            .find(|connection| &connection.key == key)
            .map(|connection| connection.forwards.as_slice())
    }

    /// Publishes a detected remote port locally. The local port is only the
    /// same number when nothing on this machine is already using it, so the
    /// choice is made against a real bind probe, off the foreground thread.
    ///
    /// The probe and the bind that [`PortForwardStore`] later performs are two
    /// separate operations, so a port can still be taken in between; that
    /// shows up as the forward's `Failed` status rather than being prevented.
    fn forward_detected_port(
        &mut self,
        key: ConnectionKey,
        port: u16,
        open_in_browser: bool,
        cx: &mut Context<Self>,
    ) {
        if self.detected_port_is_forwarded(&key, port) {
            return;
        }
        if !self.connection_is_editable(&key, cx) {
            // Said where the gesture was, not only in a panel the reader may not
            // have open: a click on the notification that reported nothing looks
            // like a click that did nothing.
            self.error = Some(CONNECTION_NOT_EDITABLE.into());
            self.report(CONNECTION_NOT_EDITABLE, cx);
            cx.notify();
            return;
        }
        self.error = None;

        let configured = self
            .configured_forwards(&key)
            .map(<[SshPortForwardOption]>::to_vec)
            .unwrap_or_default();

        cx.spawn(async move |this, cx| {
            let local_port = cx
                .background_spawn(async move {
                    choose_local_port(port, &configured, |candidate| {
                        std::net::TcpListener::bind((DEFAULT_LOCAL_BIND_HOST, candidate)).is_ok()
                    })
                })
                .await;

            this.update(cx, |this, cx| {
                let Some(local_port) = local_port else {
                    this.error = Some(
                        format!(
                            "Could not forward remote port {port}: no local port between {port} \
                             and {} is free.",
                            port.saturating_add(LOCAL_PORT_SEARCH_LIMIT - 1)
                        )
                        .into(),
                    );
                    cx.notify();
                    return;
                };

                this.update_forwards(key, cx, move |forwards| {
                    forwards.push(SshPortForwardOption {
                        local_host: None,
                        local_port,
                        remote_host: None,
                        remote_port: port,
                    });
                });
                if open_in_browser {
                    cx.open_url(&format!("http://{DEFAULT_FORWARD_HOST}:{local_port}"));
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.form.is_some() {
            self.save_form(window, cx);
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        self.close_form(window, cx);
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .p_1()
            .gap_1()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new("Forward Ports")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_px()
                    // A forward belongs to a connection, and the button beside a
                    // connection is what adds one. This one is at the level of the
                    // list, so it adds what the list holds: another connection. It
                    // opens the same flow the rest of Zed adds a remote through,
                    // rather than writing a half-filled entry from here.
                    .child(
                        IconButton::new("forward-ports-add-connection", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Add Remote Server"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    Box::new(zed_actions::OpenRemote {
                                        from_existing_connection: false,
                                        create_new_window: Some(false),
                                    }),
                                    cx,
                                );
                            }),
                    )
                    .child(
                        IconButton::new("forward-ports-refresh", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Refresh"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.start_port_detector(cx);
                                this.reload(cx);
                            })),
                    ),
            )
    }

    fn render_connection(&self, index: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let connection = self.connections.get(index)?;
        let key = connection.key.clone();
        let can_add = self.can_add_forward(&key, cx);

        let header = h_flex()
            .px_2()
            .pt_2()
            .pb_1()
            .gap_1()
            .justify_between()
            .child(
                Label::new(connection.label.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .single_line(),
            )
            .child(
                IconButton::new(
                    SharedString::from(format!("forward-ports-add-{index}")),
                    IconName::Plus,
                )
                .icon_size(IconSize::Small)
                .disabled(!can_add)
                .tooltip(Tooltip::text(if can_add {
                    "Add Port Forward"
                } else {
                    DEV_CONTAINER_NOT_CONNECTED
                }))
                .on_click(cx.listener({
                    let key = key.clone();
                    move |this, _, window, cx| {
                        this.open_form(key.clone(), None, window, cx);
                    }
                })),
            );

        let form = self.form.as_ref().filter(|form| form.connection == key);

        Some(
            v_flex()
                .child(header)
                .when(connection.forwards.is_empty() && form.is_none(), |this| {
                    this.child(
                        div().px_2().pb_1().child(
                            Label::new("No port forwards configured.")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                    )
                })
                .children(
                    connection
                        .forwards
                        .iter()
                        .enumerate()
                        .map(|(forward_index, forward)| {
                            self.render_forward(index, forward_index, &key, forward, cx)
                        }),
                )
                .when_some(form, |this, form| this.child(self.render_form(form, cx)))
                .into_any_element(),
        )
    }

    fn render_forward_status(
        &self,
        key: &ConnectionKey,
        forward: &SshPortForwardOption,
        cx: &App,
    ) -> AnyElement {
        let status = self.forward_status(key, forward, cx);
        if self.disconnected.contains(forward) && self.connected_connection.as_ref() == Some(key) {
            return Label::new(FORWARD_STATUS_DISCONNECTED)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element();
        }
        let (label, color) = match &status {
            Some(status @ PortForwardStatus::Active) => (status.label(), Color::Success),
            Some(status @ PortForwardStatus::External) => (status.label(), Color::Muted),
            Some(status @ PortForwardStatus::Failed(_)) => (status.label(), Color::Error),
            Some(status) => (status.label(), Color::Muted),
            None => (FORWARD_STATUS_INACTIVE.into(), Color::Muted),
        };
        let explanation = match &status {
            Some(PortForwardStatus::External) => {
                Some(SharedString::from(EXTERNAL_FORWARD_EXPLANATION))
            }
            other => other.as_ref().and_then(|status| status.error()).cloned(),
        };

        h_flex()
            .id(SharedString::from(format!(
                "forward-ports-status-{}-{}-{}",
                key.identifier(),
                forward.local_port,
                forward.remote_port
            )))
            .child(Label::new(label).size(LabelSize::Small).color(color))
            .when_some(explanation, |this, explanation| {
                this.tooltip(Tooltip::text(explanation))
            })
            .into_any_element()
    }

    fn render_forward(
        &self,
        connection_index: usize,
        forward_index: usize,
        key: &ConnectionKey,
        forward: &SshPortForwardOption,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let element_id =
            SharedString::from(format!("forward-ports-{connection_index}-{forward_index}"));
        let (local_end, remote_end) = port_forward_endpoints(forward);

        ListItem::new(element_id.clone())
            .spacing(ListItemSpacing::Sparse)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .justify_between()
                    // Which end is which is drawn rather than spelled out: the words
                    // took more of a narrow panel than the addresses they labelled.
                    // The same icons the rest of Zed uses for this machine and for a
                    // remote host, with the words kept in the tooltip.
                    .child(
                        h_flex()
                            .id(SharedString::from(format!(
                                "forward-ports-ends-{connection_index}-{forward_index}"
                            )))
                            .gap_1()
                            .overflow_hidden()
                            .child(
                                Icon::new(IconName::Screen)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(Label::new(local_end).single_line())
                            .child(Label::new("→").size(LabelSize::Small).color(Color::Muted))
                            .child(
                                Icon::new(remote_icon_for_key(key))
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(Label::new(remote_end).single_line())
                            .tooltip(Tooltip::text(describe_port_forward(forward))),
                    )
                    .child(self.render_forward_status(key, forward, cx)),
            )
            // Shown without hovering: a forward is a socket on this machine, and
            // whether it can be closed should not be something the reader has to
            // find by moving the mouse over the row.
            .end_slot(
                h_flex()
                    .gap_px()
                    // Only the connection this window holds has a tunnel to open or
                    // close; a row belonging to another connection is configuration
                    // and nothing more.
                    .when(self.forward_is_tunnellable(key, forward), |this| {
                        let is_disconnected = self.disconnected.contains(forward);
                        this.child(
                            IconButton::new(
                                SharedString::from(format!(
                                    "forward-ports-tunnel-{connection_index}-{forward_index}"
                                )),
                                if is_disconnected {
                                    IconName::PlayFilled
                                } else {
                                    IconName::Stop
                                },
                            )
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text(if is_disconnected {
                                "Connect Port Forward"
                            } else {
                                "Disconnect Port Forward"
                            }))
                            .on_click(cx.listener({
                                let forward = forward.clone();
                                move |this, _, _, cx| {
                                    if is_disconnected {
                                        this.connect_forward(forward.clone(), cx);
                                    } else {
                                        this.disconnect_forward(forward.clone(), cx);
                                    }
                                }
                            })),
                        )
                    })
                    .child(
                        IconButton::new(
                            SharedString::from(format!(
                                "forward-ports-edit-{connection_index}-{forward_index}"
                            )),
                            IconName::Pencil,
                        )
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Edit Port Forward"))
                        .on_click(cx.listener({
                            let key = key.clone();
                            let forward = forward.clone();
                            move |this, _, window, cx| {
                                this.open_form(key.clone(), Some(forward.clone()), window, cx);
                            }
                        })),
                    )
                    .child(
                        IconButton::new(
                            SharedString::from(format!(
                                "forward-ports-delete-{connection_index}-{forward_index}"
                            )),
                            IconName::Trash,
                        )
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Delete Port Forward"))
                        .on_click(cx.listener({
                            let key = key.clone();
                            let forward = forward.clone();
                            move |this, _, _, cx| {
                                this.delete_forward(key.clone(), forward.clone(), cx);
                            }
                        })),
                    ),
            )
            .into_any_element()
    }

    fn render_form(&self, form: &PortForwardForm, cx: &mut Context<Self>) -> impl IntoElement {
        let heading = if form.original.is_some() {
            "Edit Port Forward"
        } else {
            "Add Port Forward"
        };

        v_flex()
            .p_2()
            .gap_1()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new(heading).size(LabelSize::Small))
            .child(self.render_form_field("Local host", &form.local_host, cx))
            .child(self.render_form_field("Local port", &form.local_port, cx))
            .child(self.render_form_field("Remote host", &form.remote_host, cx))
            .child(self.render_form_field("Remote port", &form.remote_port, cx))
            .when_some(form.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .child(
                h_flex()
                    .pt_1()
                    .gap_1()
                    .justify_end()
                    .child(Button::new("forward-ports-form-cancel", "Cancel").on_click(
                        cx.listener(|this, _, window, cx| {
                            this.close_form(window, cx);
                        }),
                    ))
                    .child(
                        Button::new("forward-ports-form-save", "Save")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save_form(window, cx);
                            })),
                    ),
            )
    }

    fn render_form_field(
        &self,
        label: &'static str,
        editor: &Entity<Editor>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let settings = ThemeSettings::get_global(cx);
        let text_style = TextStyle {
            color: cx.theme().colors().text,
            font_family: settings.ui_font.family.clone(),
            font_features: settings.ui_font.features.clone(),
            font_fallbacks: settings.ui_font.fallbacks.clone(),
            font_size: rems(0.875).into(),
            font_weight: settings.ui_font.weight,
            font_style: FontStyle::Normal,
            line_height: relative(1.3),
            ..Default::default()
        };

        v_flex()
            .gap_px()
            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
            .child(
                div()
                    .p_1()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .rounded_sm()
                    .child(EditorElement::new(
                        editor,
                        EditorStyle {
                            local_player: cx.theme().players().local(),
                            text: text_style,
                            ..Default::default()
                        },
                    )),
            )
    }
}

impl Render for ForwardPortsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("ForwardPortsPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .size_full()
            .child(self.render_toolbar(cx))
            .when_some(self.error.clone(), |this, error| {
                this.child(div().p_2().child(Label::new(error).color(Color::Error)))
            })
            .when_some(self.port_detector_stopped.clone(), |this, reason| {
                this.child(
                    div().p_2().child(
                        Label::new(format!("{reason} Press Refresh to try again."))
                            .size(LabelSize::Small)
                            .color(Color::Warning),
                    ),
                )
            })
            .child(
                v_flex()
                    .id("forward-ports-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .when(self.connections.is_empty(), |this| {
                        this.child(
                            div().p_2().child(
                                Label::new(
                                    "No remote connections are configured. Add an SSH server, WSL distribution, or dev container first, then forward its ports here.",
                                )
                                .color(Color::Muted),
                            ),
                        )
                    })
                    .children(
                        (0..self.connections.len())
                            .filter_map(|index| self.render_connection(index, cx)),
                    ),
            )
    }
}

impl Focusable for ForwardPortsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ForwardPortsPanel {}

impl Panel for ForwardPortsPanel {
    fn persistent_name() -> &'static str {
        "ForwardPortsPanel"
    }

    fn panel_key() -> &'static str {
        FORWARD_PORTS_PANEL_KEY
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
        Some(IconName::ArrowRightLeft)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Forward Ports")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        9
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.start_port_detector(cx);
            self.reload(cx);
        }
    }
}
