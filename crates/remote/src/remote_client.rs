#[cfg(any(test, feature = "test-support"))]
use crate::transport::mock::ConnectGuard;
use crate::{
    SshConnectionOptions,
    protocol::MessageId,
    proxy::ProxyLaunchError,
    transport::{
        docker::{DockerConnectionOptions, DockerExecConnection},
        ssh::SshRemoteConnection,
        wsl::{WslConnectionOptions, WslRemoteConnection},
    },
};
use anyhow::{Context as _, Result, anyhow};
use askpass::EncryptedPassword;
use async_trait::async_trait;
use collections::HashMap;
use futures::{
    Future, FutureExt as _, StreamExt as _,
    channel::{
        mpsc::{self, Sender, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    future::{BoxFuture, Shared, WeakShared},
    select, select_biased,
    stream::BoxStream,
};
use gpui::{
    App, AppContext as _, AsyncApp, BackgroundExecutor, BorrowAppContext, Context, Entity,
    EventEmitter, FutureExt, Global, Task, TaskExt, WeakEntity,
};
use parking_lot::Mutex;

use release_channel::ReleaseChannel;
use rpc::{
    AnyProtoClient, ErrorExt, ProtoClient, ProtoMessageHandlerSet, RpcError,
    proto::{self, Envelope, EnvelopedMessage, PeerId, RequestMessage, build_typed_envelope},
};
use semver::Version;
use std::{
    collections::VecDeque,
    fmt,
    ops::ControlFlow,
    path::PathBuf,
    sync::{
        Arc, Weak,
        atomic::{AtomicU32, AtomicU64, Ordering::SeqCst},
    },
    time::Duration,
};
use util::{
    ResultExt,
    paths::{PathStyle, RemotePathBuf},
};
use web_time::Instant;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RemoteOs {
    Linux,
    MacOs,
    Windows,
}

impl RemoteOs {
    pub fn as_str(&self) -> &'static str {
        match self {
            RemoteOs::Linux => "linux",
            RemoteOs::MacOs => "macos",
            RemoteOs::Windows => "windows",
        }
    }

    pub fn is_windows(&self) -> bool {
        matches!(self, RemoteOs::Windows)
    }

    /// A human-readable OS name for telemetry. Matches `client::telemetry::os_name`
    /// ignoring the compositor (as we run headless on remotes).
    pub fn display_name(&self) -> &'static str {
        match self {
            RemoteOs::Linux => "Linux",
            RemoteOs::MacOs => "macOS",
            RemoteOs::Windows => "Windows",
        }
    }
}

impl std::fmt::Display for RemoteOs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RemoteArch {
    X86_64,
    Aarch64,
}

impl RemoteArch {
    pub fn as_str(&self) -> &'static str {
        match self {
            RemoteArch::X86_64 => "x86_64",
            RemoteArch::Aarch64 => "aarch64",
        }
    }
}

impl std::fmt::Display for RemoteArch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Copy, Clone, Debug)]
pub struct RemotePlatform {
    pub os: RemoteOs,
    pub arch: RemoteArch,
}

#[derive(Clone, Debug)]
pub struct CommandTemplate {
    pub program: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

/// Whether a command should be run with TTY allocation for interactive use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interactive {
    /// Allocate a pseudo-TTY for interactive terminal use.
    Yes,
    /// Do not allocate a TTY - for commands that communicate via piped stdio.
    No,
}

pub trait RemoteClientDelegate: Send + Sync {
    fn ask_password(
        &self,
        prompt: String,
        tx: oneshot::Sender<EncryptedPassword>,
        cancellation: oneshot::Receiver<()>,
        cx: &mut AsyncApp,
    );
    fn get_download_url(
        &self,
        platform: RemotePlatform,
        release_channel: ReleaseChannel,
        version: Option<Version>,
        cx: &mut AsyncApp,
    ) -> Task<Result<Option<String>>>;
    fn download_server_binary_locally(
        &self,
        platform: RemotePlatform,
        release_channel: ReleaseChannel,
        version: Option<Version>,
        cx: &mut AsyncApp,
    ) -> Task<Result<PathBuf>>;
    fn set_status(&self, status: Option<&str>, cx: &mut AsyncApp);
}

const MAX_MISSED_HEARTBEATS: usize = 5;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(5);
const INITIAL_CONNECTION_TIMEOUT: Duration =
    Duration::from_secs(if cfg!(debug_assertions) { 5 } else { 60 });

pub const MAX_RECONNECT_ATTEMPTS: usize = 3;

enum State {
    Connecting,
    Connected {
        remote_connection: Arc<dyn RemoteConnection>,
        delegate: Arc<dyn RemoteClientDelegate>,

        multiplex_task: Task<Result<()>>,
        heartbeat_task: Task<Result<()>>,
    },
    HeartbeatMissed {
        missed_heartbeats: usize,

        remote_connection: Arc<dyn RemoteConnection>,
        delegate: Arc<dyn RemoteClientDelegate>,

        multiplex_task: Task<Result<()>>,
        heartbeat_task: Task<Result<()>>,
    },
    Reconnecting,
    ReconnectFailed {
        remote_connection: Arc<dyn RemoteConnection>,
        delegate: Arc<dyn RemoteClientDelegate>,

        error: anyhow::Error,
        attempts: usize,
    },
    ReconnectExhausted,
    ServerNotRunning,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connecting => write!(f, "connecting"),
            Self::Connected { .. } => write!(f, "connected"),
            Self::Reconnecting => write!(f, "reconnecting"),
            Self::ReconnectFailed { .. } => write!(f, "reconnect failed"),
            Self::ReconnectExhausted => write!(f, "reconnect exhausted"),
            Self::HeartbeatMissed { .. } => write!(f, "heartbeat missed"),
            Self::ServerNotRunning { .. } => write!(f, "server not running"),
        }
    }
}

impl State {
    fn remote_connection(&self) -> Option<Arc<dyn RemoteConnection>> {
        match self {
            Self::Connected {
                remote_connection, ..
            } => Some(remote_connection.clone()),
            Self::HeartbeatMissed {
                remote_connection, ..
            } => Some(remote_connection.clone()),
            Self::ReconnectFailed {
                remote_connection, ..
            } => Some(remote_connection.clone()),
            _ => None,
        }
    }

    fn can_reconnect(&self) -> bool {
        match self {
            Self::Connected { .. }
            | Self::HeartbeatMissed { .. }
            | Self::ReconnectFailed { .. } => true,
            State::Connecting
            | State::Reconnecting
            | State::ReconnectExhausted
            | State::ServerNotRunning => false,
        }
    }

    fn is_reconnect_failed(&self) -> bool {
        matches!(self, Self::ReconnectFailed { .. })
    }

    fn is_reconnect_exhausted(&self) -> bool {
        matches!(self, Self::ReconnectExhausted { .. })
    }

    fn is_server_not_running(&self) -> bool {
        matches!(self, Self::ServerNotRunning)
    }

    fn is_reconnecting(&self) -> bool {
        matches!(self, Self::Reconnecting { .. })
    }

    fn heartbeat_recovered(self) -> Self {
        match self {
            Self::HeartbeatMissed {
                remote_connection,
                delegate,
                multiplex_task,
                heartbeat_task,
                ..
            } => Self::Connected {
                remote_connection,
                delegate,
                multiplex_task,
                heartbeat_task,
            },
            _ => self,
        }
    }

    fn heartbeat_missed(self) -> Self {
        match self {
            Self::Connected {
                remote_connection,
                delegate,
                multiplex_task,
                heartbeat_task,
            } => Self::HeartbeatMissed {
                missed_heartbeats: 1,
                remote_connection,
                delegate,
                multiplex_task,
                heartbeat_task,
            },
            Self::HeartbeatMissed {
                missed_heartbeats,
                remote_connection,
                delegate,
                multiplex_task,
                heartbeat_task,
            } => Self::HeartbeatMissed {
                missed_heartbeats: missed_heartbeats + 1,
                remote_connection,
                delegate,
                multiplex_task,
                heartbeat_task,
            },
            _ => self,
        }
    }
}

/// The state of the ssh connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Connected,
    HeartbeatMissed,
    Reconnecting,
    Disconnected,
}

impl From<&State> for ConnectionState {
    fn from(value: &State) -> Self {
        match value {
            State::Connecting => Self::Connecting,
            State::Connected { .. } => Self::Connected,
            State::Reconnecting | State::ReconnectFailed { .. } => Self::Reconnecting,
            State::HeartbeatMissed { .. } => Self::HeartbeatMissed,
            State::ReconnectExhausted => Self::Disconnected,
            State::ServerNotRunning => Self::Disconnected,
        }
    }
}

pub struct RemoteClient {
    client: Arc<ChannelClient>,
    unique_identifier: String,
    connection_options: RemoteConnectionOptions,
    path_style: PathStyle,
    platform: RemotePlatform,
    os_version: Option<String>,
    state: Option<State>,
}

#[derive(Debug)]
pub enum RemoteClientEvent {
    Disconnected { server_not_running: bool },
    Reconnected,
}

impl EventEmitter<RemoteClientEvent> for RemoteClient {}

/// Identifies the socket on the remote server so that reconnects
/// can re-join the same project.
pub enum ConnectionIdentifier {
    Setup(u64),
    Workspace(i64),
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl ConnectionIdentifier {
    pub fn setup() -> Self {
        Self::Setup(NEXT_ID.fetch_add(1, SeqCst))
    }

    // This string gets used in a socket name, and so must be relatively short.
    // The total length of:
    //   /home/{username}/.local/share/zed/server_state/{name}/stdout.sock
    // Must be less than about 100 characters
    //   https://unix.stackexchange.com/questions/367008/why-is-socket-path-length-limited-to-a-hundred-chars
    // So our strings should be at most 20 characters or so.
    fn to_string(&self, cx: &App) -> String {
        let identifier_prefix = match ReleaseChannel::global(cx) {
            ReleaseChannel::Stable => "".to_string(),
            release_channel => format!("{}-", release_channel.dev_name()),
        };
        match self {
            Self::Setup(setup_id) => format!("{identifier_prefix}setup-{setup_id}"),
            Self::Workspace(workspace_id) => {
                format!("{identifier_prefix}workspace-{workspace_id}",)
            }
        }
    }
}

pub async fn connect(
    connection_options: RemoteConnectionOptions,
    delegate: Arc<dyn RemoteClientDelegate>,
    cx: &mut AsyncApp,
) -> Result<Arc<dyn RemoteConnection>> {
    cx.update(|cx| {
        cx.update_default_global(|pool: &mut ConnectionPool, cx| {
            pool.connect(connection_options.clone(), delegate.clone(), cx)
        })
    })
    .await
    .map_err(|e| e.cloned())
}

/// Returns `true` if the global [`ConnectionPool`] already has a live
/// connection for the given options. Callers can use this to decide
/// whether to show interactive UI (e.g., a password modal) before
/// connecting.
pub fn has_active_connection(opts: &RemoteConnectionOptions, cx: &App) -> bool {
    cx.try_global::<ConnectionPool>().is_some_and(|pool| {
        matches!(
            pool.connections.get(opts),
            Some(ConnectionPoolEntry::Connected(remote))
                if remote.upgrade().is_some_and(|r| !r.has_been_killed())
        )
    })
}

impl RemoteClient {
    pub fn new(
        unique_identifier: ConnectionIdentifier,
        remote_connection: Arc<dyn RemoteConnection>,
        cancellation: oneshot::Receiver<()>,
        delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut App,
    ) -> Task<Result<Option<Entity<Self>>>> {
        let unique_identifier = unique_identifier.to_string(cx);
        cx.spawn(async move |cx| {
            let success = Box::pin(async move {
                let (outgoing_tx, outgoing_rx) = mpsc::unbounded::<Envelope>();
                let (incoming_tx, incoming_rx) = mpsc::unbounded::<Envelope>();
                let (connection_activity_tx, connection_activity_rx) = mpsc::channel::<()>(1);

                let client = cx.update(|cx| {
                    ChannelClient::new(
                        incoming_rx,
                        outgoing_tx,
                        cx,
                        "client",
                        remote_connection.has_wsl_interop(),
                    )
                });

                let path_style = remote_connection.path_style();
                let platform = remote_connection.remote_platform();
                let os_version = remote_connection.remote_os_version();
                let connection_options = remote_connection.connection_options();
                let connection_type = connection_options.connection_type();
                let this = cx.new(|_| Self {
                    client: client.clone(),
                    unique_identifier: unique_identifier.clone(),
                    connection_options,
                    path_style,
                    platform,
                    os_version: os_version.clone(),
                    state: Some(State::Connecting),
                });

                let io_task = remote_connection.start_proxy(
                    unique_identifier,
                    false,
                    incoming_tx,
                    outgoing_rx,
                    connection_activity_tx,
                    delegate.clone(),
                    cx,
                );

                let ready = client
                    .wait_for_remote_started()
                    .with_timeout(INITIAL_CONNECTION_TIMEOUT, cx.background_executor())
                    .await;
                match ready {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        let mut error = "remote client exited before becoming ready".to_owned();
                        if let Some(status) = io_task.now_or_never() {
                            match status {
                                Ok(exit_code) => {
                                    error.push_str(&format!(", exit_code={exit_code:?}"))
                                }
                                Err(e) => error.push_str(&format!(", error={e:?}")),
                            }
                        }
                        let error = anyhow::anyhow!("{error}");
                        log::error!("failed to establish connection: {}", error);
                        return Err(error);
                    }
                    Err(_) => {
                        let mut error = String::new();
                        if let Some(status) = io_task.now_or_never() {
                            error.push_str("Client exited with ");
                            match status {
                                Ok(exit_code) => {
                                    error.push_str(&format!("exit_code {exit_code:?}"))
                                }
                                Err(e) => error.push_str(&format!("error {e:?}")),
                            }
                        } else {
                            error.push_str("client did not become ready within the timeout");
                        }
                        let error = anyhow::anyhow!("{error}");
                        log::error!("failed to establish connection: {error}");
                        return Err(error);
                    }
                }
                let multiplex_task = Self::monitor(this.downgrade(), io_task, cx);
                if let Err(error) = client.ping(HEARTBEAT_TIMEOUT).await {
                    log::error!("failed to establish connection: {}", error);
                    return Err(error);
                }

                let heartbeat_task = Self::heartbeat(this.downgrade(), connection_activity_rx, cx);

                this.update(cx, |this, _| {
                    this.state = Some(State::Connected {
                        remote_connection,
                        delegate,
                        multiplex_task,
                        heartbeat_task,
                    });
                });

                // Use the same `remote_*` property schema as the forwarded
                // remote events (see `client::telemetry::report_remote_event`)
                // so all remote-origin telemetry can be queried uniformly.
                telemetry::event!(
                    "Remote Connection Established",
                    remote = true,
                    remote_connection_type = connection_type,
                    remote_os_name = platform.os.display_name(),
                    remote_os_version = os_version,
                    remote_architecture = platform.arch.as_str(),
                );

                Ok(Some(this))
            });

            select! {
                _ = cancellation.fuse() => {
                    Ok(None)
                }
                result = success.fuse() =>  result
            }
        })
    }

    pub fn proto_client_from_channels(
        incoming_rx: mpsc::UnboundedReceiver<Envelope>,
        outgoing_tx: mpsc::UnboundedSender<Envelope>,
        cx: &App,
        name: &'static str,
        has_wsl_interop: bool,
    ) -> AnyProtoClient {
        ChannelClient::new(incoming_rx, outgoing_tx, cx, name, has_wsl_interop).into()
    }

    pub fn shutdown_processes<T: RequestMessage>(
        &mut self,
        shutdown_request: Option<T>,
        executor: BackgroundExecutor,
    ) -> Option<impl Future<Output = ()> + use<T>> {
        let state = self.state.take()?;
        log::info!("shutting down remote processes");

        let State::Connected {
            multiplex_task,
            heartbeat_task,
            remote_connection,
            delegate,
        } = state
        else {
            return None;
        };

        let client = self.client.clone();

        Some(async move {
            if let Some(shutdown_request) = shutdown_request {
                client.send(shutdown_request).log_err();
                // We wait 50ms instead of waiting for a response, because
                // waiting for a response would require us to wait on the main thread
                // which we want to avoid in an `on_app_quit` callback.
                executor.timer(Duration::from_millis(50)).await;
            }

            // Drop `multiplex_task` because it owns our remote_connection_proxy_process, which is a
            // child of master_process.
            drop(multiplex_task);
            // Now drop the rest of state, which kills master process.
            drop(heartbeat_task);
            drop(remote_connection);
            drop(delegate);
        })
    }

    fn reconnect(&mut self, cx: &mut Context<Self>) -> Result<()> {
        let can_reconnect = self
            .state
            .as_ref()
            .map(|state| state.can_reconnect())
            .unwrap_or(false);
        if !can_reconnect {
            let state = if let Some(state) = self.state.as_ref() {
                state.to_string()
            } else {
                "no state set".to_string()
            };
            log::info!(
                "aborting reconnect, because not in state that allows reconnecting: {state}"
            );
            anyhow::bail!(
                "aborting reconnect, because not in state that allows reconnecting: {state}"
            );
        }

        let state = self.state.take().unwrap();
        let (attempts, remote_connection, delegate) = match state {
            State::Connected {
                remote_connection,
                delegate,
                multiplex_task,
                heartbeat_task,
            }
            | State::HeartbeatMissed {
                remote_connection,
                delegate,
                multiplex_task,
                heartbeat_task,
                ..
            } => {
                drop(multiplex_task);
                drop(heartbeat_task);
                (0, remote_connection, delegate)
            }
            State::ReconnectFailed {
                attempts,
                remote_connection,
                delegate,
                ..
            } => (attempts, remote_connection, delegate),
            State::Connecting
            | State::Reconnecting
            | State::ReconnectExhausted
            | State::ServerNotRunning => unreachable!(),
        };

        let attempts = attempts + 1;
        if attempts > MAX_RECONNECT_ATTEMPTS {
            log::error!(
                "Failed to reconnect to after {} attempts, giving up",
                MAX_RECONNECT_ATTEMPTS
            );
            self.set_state(State::ReconnectExhausted, cx);
            return Ok(());
        }

        self.set_state(State::Reconnecting, cx);

        log::info!(
            "Trying to reconnect to remote server... Attempt {}",
            attempts
        );

        let unique_identifier = self.unique_identifier.clone();
        let client = self.client.clone();
        let reconnect_task = cx.spawn(async move |this, cx| {
            macro_rules! failed {
                ($error:expr, $attempts:expr, $remote_connection:expr, $delegate:expr) => {
                    delegate.set_status(Some(&format!("{error:#}", error = $error)), cx);
                    return State::ReconnectFailed {
                        error: anyhow!($error),
                        attempts: $attempts,
                        remote_connection: $remote_connection,
                        delegate: $delegate,
                    };
                };
            }

            if let Err(error) = remote_connection
                .kill()
                .await
                .context("Failed to kill remote_connection process")
            {
                failed!(error, attempts, remote_connection, delegate);
            };

            let connection_options = remote_connection.connection_options();

            let (outgoing_tx, outgoing_rx) = mpsc::unbounded::<Envelope>();
            let (incoming_tx, incoming_rx) = mpsc::unbounded::<Envelope>();
            let (connection_activity_tx, connection_activity_rx) = mpsc::channel::<()>(1);

            let (remote_connection, io_task) = match async {
                let remote_connection = cx
                    .update_global(|pool: &mut ConnectionPool, cx| {
                        pool.connect(connection_options, delegate.clone(), cx)
                    })
                    .await
                    .map_err(|error| error.cloned())?;

                let io_task = remote_connection.start_proxy(
                    unique_identifier,
                    true,
                    incoming_tx,
                    outgoing_rx,
                    connection_activity_tx,
                    delegate.clone(),
                    cx,
                );
                anyhow::Ok((remote_connection, io_task))
            }
            .await
            {
                Ok((remote_connection, io_task)) => (remote_connection, io_task),
                Err(error) => {
                    failed!(error, attempts, remote_connection, delegate);
                }
            };

            let multiplex_task = Self::monitor(this.clone(), io_task, cx);
            client.reconnect(incoming_rx, outgoing_tx, cx);

            if let Err(error) = client.resync(HEARTBEAT_TIMEOUT).await {
                failed!(error, attempts, remote_connection, delegate);
            };

            State::Connected {
                remote_connection,
                delegate,
                multiplex_task,
                heartbeat_task: Self::heartbeat(this.clone(), connection_activity_rx, cx),
            }
        });

        cx.spawn(async move |this, cx| {
            let new_state = reconnect_task.await;
            this.update(cx, |this, cx| {
                let reconnected = this.state_is(State::is_reconnecting)
                    && matches!(&new_state, State::Connected { .. });
                this.try_set_state(cx, |old_state| {
                    if old_state.is_reconnecting() {
                        match &new_state {
                            State::Connecting
                            | State::Reconnecting
                            | State::HeartbeatMissed { .. }
                            | State::ServerNotRunning => {}
                            State::Connected { .. } => {
                                log::info!("Successfully reconnected");
                            }
                            State::ReconnectFailed {
                                error, attempts, ..
                            } => {
                                log::error!(
                                    "Reconnect attempt {} failed: {:?}. Starting new attempt...",
                                    attempts,
                                    error
                                );
                            }
                            State::ReconnectExhausted => {
                                log::error!("Reconnect attempt failed and all attempts exhausted");
                            }
                        }
                        Some(new_state)
                    } else {
                        None
                    }
                });

                if reconnected {
                    cx.emit(RemoteClientEvent::Reconnected);
                }

                if this.state_is(State::is_reconnect_failed) {
                    this.reconnect(cx)
                } else if this.state_is(State::is_reconnect_exhausted) {
                    Ok(())
                } else {
                    log::debug!("State has transition from Reconnecting into new state while attempting reconnect.");
                    Ok(())
                }
            })
        })
        .detach_and_log_err(cx);

        Ok(())
    }

    fn heartbeat(
        this: WeakEntity<Self>,
        mut connection_activity_rx: mpsc::Receiver<()>,
        cx: &mut AsyncApp,
    ) -> Task<Result<()>> {
        let Ok(client) = this.read_with(cx, |this, _| this.client.clone()) else {
            return Task::ready(Err(anyhow!("remote_connectionRemoteClient lost")));
        };

        cx.spawn(async move |cx| {
            let mut missed_heartbeats = 0;

            let keepalive_timer = cx.background_executor().timer(HEARTBEAT_INTERVAL).fuse();
            futures::pin_mut!(keepalive_timer);

            loop {
                select_biased! {
                    result = connection_activity_rx.next().fuse() => {
                        if result.is_none() {
                            log::warn!("remote heartbeat: connection activity channel has been dropped. stopping.");
                            return Ok(());
                        }

                        if missed_heartbeats != 0 {
                            missed_heartbeats = 0;
                            let _ =this.update(cx, |this, cx| {
                                this.handle_heartbeat_result(missed_heartbeats, cx)
                            })?;
                        }
                    }
                    _ = keepalive_timer => {
                        log::debug!("Sending heartbeat to server...");

                        let result = select_biased! {
                            _ = connection_activity_rx.next().fuse() => {
                                Ok(())
                            }
                            ping_result = client.ping(HEARTBEAT_TIMEOUT).fuse() => {
                                ping_result
                            }
                        };

                        if result.is_err() {
                            missed_heartbeats += 1;
                            log::warn!(
                                "No heartbeat from server after {:?}. Missed heartbeat {} out of {}.",
                                HEARTBEAT_TIMEOUT,
                                missed_heartbeats,
                                MAX_MISSED_HEARTBEATS
                            );
                        } else if missed_heartbeats != 0 {
                            missed_heartbeats = 0;
                        } else {
                            continue;
                        }

                        let result = this.update(cx, |this, cx| {
                            this.handle_heartbeat_result(missed_heartbeats, cx)
                        })?;
                        if result.is_break() {
                            return Ok(());
                        }
                    }
                }

                keepalive_timer.set(cx.background_executor().timer(HEARTBEAT_INTERVAL).fuse());
            }
        })
    }

    fn handle_heartbeat_result(
        &mut self,
        missed_heartbeats: usize,
        cx: &mut Context<Self>,
    ) -> ControlFlow<()> {
        let state = self.state.take().unwrap();
        let next_state = if missed_heartbeats > 0 {
            state.heartbeat_missed()
        } else {
            state.heartbeat_recovered()
        };

        self.set_state(next_state, cx);

        if missed_heartbeats >= MAX_MISSED_HEARTBEATS {
            log::error!(
                "Missed last {} heartbeats. Reconnecting...",
                missed_heartbeats
            );

            self.reconnect(cx)
                .context("failed to start reconnect process after missing heartbeats")
                .log_err();
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }

    fn monitor(
        this: WeakEntity<Self>,
        io_task: Task<Result<i32>>,
        cx: &AsyncApp,
    ) -> Task<Result<()>> {
        cx.spawn(async move |cx| {
            let result = io_task.await;

            match result {
                Ok(exit_code) => {
                    if let Some(error) = ProxyLaunchError::from_exit_code(exit_code) {
                        match error {
                            ProxyLaunchError::ServerNotRunning => {
                                log::error!("failed to reconnect because server is not running");
                                this.update(cx, |this, cx| {
                                    this.set_state(State::ServerNotRunning, cx);
                                })?;
                            }
                        }
                    } else {
                        log::error!("proxy process terminated unexpectedly: {exit_code}");
                        this.update(cx, |this, cx| {
                            this.reconnect(cx).ok();
                        })?;
                    }
                }
                Err(error) => {
                    log::warn!(
                        "remote io task died with error: {:?}. reconnecting...",
                        error
                    );
                    this.update(cx, |this, cx| {
                        this.reconnect(cx).ok();
                    })?;
                }
            }

            Ok(())
        })
    }

    fn state_is(&self, check: impl FnOnce(&State) -> bool) -> bool {
        self.state.as_ref().is_some_and(check)
    }

    fn try_set_state(&mut self, cx: &mut Context<Self>, map: impl FnOnce(&State) -> Option<State>) {
        let new_state = self.state.as_ref().and_then(map);
        if let Some(new_state) = new_state {
            self.state.replace(new_state);
            cx.notify();
        }
    }

    fn set_state(&mut self, state: State, cx: &mut Context<Self>) {
        log::info!("setting state to '{state}'");

        let is_reconnect_exhausted = state.is_reconnect_exhausted();
        let is_server_not_running = state.is_server_not_running();
        self.state.replace(state);

        if is_reconnect_exhausted || is_server_not_running {
            cx.emit(RemoteClientEvent::Disconnected {
                server_not_running: is_server_not_running,
            });
        }
        cx.notify();
    }

    pub fn shell(&self) -> Option<String> {
        Some(self.remote_connection()?.shell())
    }

    pub fn default_system_shell(&self) -> Option<String> {
        Some(self.remote_connection()?.default_system_shell())
    }

    pub fn shares_network_interface(&self) -> bool {
        self.remote_connection()
            .map_or(false, |connection| connection.shares_network_interface())
    }

    pub fn has_wsl_interop(&self) -> bool {
        self.remote_connection()
            .map_or(false, |connection| connection.has_wsl_interop())
    }

    pub fn build_command(
        &self,
        program: Option<String>,
        args: &[String],
        env: &HashMap<String, String>,
        working_dir: Option<String>,
        port_forward: Option<(u16, String, u16)>,
        interactive: Interactive,
    ) -> Result<CommandTemplate> {
        let Some(connection) = self.remote_connection() else {
            return Err(anyhow!("no remote connection"));
        };
        connection.build_command(program, args, env, working_dir, port_forward, interactive)
    }

    pub fn build_forward_ports_command(
        &self,
        forwards: Vec<(u16, String, u16)>,
    ) -> Result<CommandTemplate> {
        let Some(connection) = self.remote_connection() else {
            return Err(anyhow!("no remote connection"));
        };
        connection.build_forward_ports_command(forwards)
    }

    pub fn upload_directory(
        &self,
        src_path: PathBuf,
        dest_path: RemotePathBuf,
        cx: &App,
    ) -> Task<Result<()>> {
        let Some(connection) = self.remote_connection() else {
            return Task::ready(Err(anyhow!("no remote connection")));
        };
        connection.upload_directory(src_path, dest_path, cx)
    }

    pub fn proto_client(&self) -> AnyProtoClient {
        self.client.clone().into()
    }

    pub fn connection_options(&self) -> RemoteConnectionOptions {
        self.connection_options.clone()
    }

    pub fn connection(&self) -> Option<Arc<dyn RemoteConnection>> {
        if let State::Connected {
            remote_connection, ..
        } = self.state.as_ref()?
        {
            Some(remote_connection.clone())
        } else {
            None
        }
    }

    pub fn connection_state(&self) -> ConnectionState {
        self.state
            .as_ref()
            .map(ConnectionState::from)
            .unwrap_or(ConnectionState::Disconnected)
    }

    pub fn is_disconnected(&self) -> bool {
        self.connection_state() == ConnectionState::Disconnected
    }

    pub fn path_style(&self) -> PathStyle {
        self.path_style
    }

    /// The platform (OS and architecture) of the remote host, detected during
    /// connection setup.
    pub fn remote_platform(&self) -> RemotePlatform {
        self.platform
    }

    /// The OS version of the remote host (e.g. `"ubuntu 24.04"`), detected
    /// during connection setup. `None` if it could not be determined.
    pub fn remote_os_version(&self) -> Option<String> {
        self.os_version.clone()
    }

    /// A stable identifier for the kind of remote connection (e.g. `"ssh"`,
    /// `"wsl"`, `"docker"`, `"podman"`).
    pub fn connection_type(&self) -> &'static str {
        self.connection_options.connection_type()
    }

    /// Forcibly disconnects from the remote server by killing the underlying connection.
    /// This will trigger the reconnection logic if reconnection attempts remain.
    /// Useful for testing reconnection behavior in real environments.
    pub fn force_disconnect(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let Some(connection) = self.remote_connection() else {
            return Task::ready(Err(anyhow!("no active remote connection to disconnect")));
        };

        log::info!("force_disconnect: killing remote connection");

        cx.spawn(async move |_, _| {
            connection.kill().await?;
            Ok(())
        })
    }

    /// Simulates a timeout by pausing heartbeat responses.
    /// This will cause heartbeat failures and eventually trigger reconnection
    /// after MAX_MISSED_HEARTBEATS are missed.
    /// Useful for testing timeout behavior in real environments.
    pub fn force_heartbeat_timeout(&mut self, attempts: usize, cx: &mut Context<Self>) {
        log::info!("force_heartbeat_timeout: triggering heartbeat failure state");

        if let Some(State::Connected {
            remote_connection,
            delegate,
            multiplex_task,
            heartbeat_task,
        }) = self.state.take()
        {
            self.set_state(
                if attempts == 0 {
                    State::HeartbeatMissed {
                        missed_heartbeats: MAX_MISSED_HEARTBEATS,
                        remote_connection,
                        delegate,
                        multiplex_task,
                        heartbeat_task,
                    }
                } else {
                    State::ReconnectFailed {
                        remote_connection,
                        delegate,
                        error: anyhow!("forced heartbeat timeout"),
                        attempts,
                    }
                },
                cx,
            );

            self.reconnect(cx)
                .context("failed to start reconnect after forced timeout")
                .log_err();
        } else {
            log::warn!("force_heartbeat_timeout: not in Connected state, ignoring");
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn force_server_not_running(&mut self, cx: &mut Context<Self>) {
        self.set_state(State::ServerNotRunning, cx);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn simulate_disconnect(&self, client_cx: &mut App) -> Task<()> {
        let opts = self.connection_options();
        client_cx.spawn(async move |cx| {
            let connection = cx.update_global(|c: &mut ConnectionPool, _| {
                if let Some(ConnectionPoolEntry::Connected(c)) = c.connections.get(&opts) {
                    if let Some(connection) = c.upgrade() {
                        connection
                    } else {
                        panic!("connection was dropped")
                    }
                } else {
                    panic!("missing test connection")
                }
            });

            connection.simulate_disconnect(cx);
        })
    }

    /// Creates a mock connection pair for testing.
    ///
    /// This is the recommended way to create mock remote connections for tests.
    /// It returns the `MockConnectionOptions` (which can be passed to create a
    /// `HeadlessProject`), an `AnyProtoClient` for the server side and a
    /// `ConnectGuard` for the client side which blocks the connection from
    /// being established until dropped.
    ///
    /// # Example
    /// ```ignore
    /// let (opts, server_session, connect_guard) = RemoteClient::fake_server(cx, server_cx);
    /// // Set up HeadlessProject with server_session...
    /// drop(connect_guard);
    /// let client = RemoteClient::fake_client(opts, cx).await;
    /// ```
    #[cfg(any(test, feature = "test-support"))]
    pub fn fake_server(
        client_cx: &mut gpui::TestAppContext,
        server_cx: &mut gpui::TestAppContext,
    ) -> (RemoteConnectionOptions, AnyProtoClient, ConnectGuard) {
        use crate::transport::mock::MockConnection;
        let (opts, server_client, connect_guard) = MockConnection::new(client_cx, server_cx);
        (opts.into(), server_client, connect_guard)
    }

    /// Registers a new mock server for existing connection options.
    ///
    /// Use this to simulate reconnection: after forcing a disconnect, register
    /// a new server so the next `connect()` call succeeds.
    #[cfg(any(test, feature = "test-support"))]
    pub fn fake_server_with_opts(
        opts: &RemoteConnectionOptions,
        client_cx: &mut gpui::TestAppContext,
        server_cx: &mut gpui::TestAppContext,
    ) -> (AnyProtoClient, ConnectGuard) {
        use crate::transport::mock::MockConnection;
        let mock_opts = match opts {
            RemoteConnectionOptions::Mock(mock_opts) => mock_opts.clone(),
            _ => panic!("fake_server_with_opts requires Mock connection options"),
        };
        MockConnection::new_with_opts(mock_opts, client_cx, server_cx)
    }

    /// Creates a `RemoteClient` connected to a mock server.
    ///
    /// Call `fake_server` first to get the connection options, set up the
    /// `HeadlessProject` with the server session, then call this method
    /// to create the client.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn connect_mock(
        opts: RemoteConnectionOptions,
        client_cx: &mut gpui::TestAppContext,
    ) -> Entity<Self> {
        assert!(matches!(opts, RemoteConnectionOptions::Mock(..)));
        use crate::transport::mock::MockDelegate;
        let (_tx, rx) = oneshot::channel();
        let mut cx = client_cx.to_async();
        let connection = connect(opts, Arc::new(MockDelegate), &mut cx)
            .await
            .unwrap();
        client_cx
            .update(|cx| {
                Self::new(
                    ConnectionIdentifier::setup(),
                    connection,
                    rx,
                    Arc::new(MockDelegate),
                    cx,
                )
            })
            .await
            .unwrap()
            .unwrap()
    }

    pub fn remote_connection(&self) -> Option<Arc<dyn RemoteConnection>> {
        self.state
            .as_ref()
            .and_then(|state| state.remote_connection())
    }
}

enum ConnectionPoolEntry {
    Connecting(WeakShared<Task<Result<Arc<dyn RemoteConnection>, Arc<anyhow::Error>>>>),
    Connected(Weak<dyn RemoteConnection>),
}

#[derive(Default)]
struct ConnectionPool {
    connections: HashMap<RemoteConnectionOptions, ConnectionPoolEntry>,
}

impl Global for ConnectionPool {}

impl ConnectionPool {
    fn connect(
        &mut self,
        opts: RemoteConnectionOptions,
        delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut App,
    ) -> Shared<Task<Result<Arc<dyn RemoteConnection>, Arc<anyhow::Error>>>> {
        let connection = self.connections.get(&opts);
        match connection {
            Some(ConnectionPoolEntry::Connecting(task)) => {
                if let Some(task) = task.upgrade() {
                    log::debug!("Connecting task is still alive");
                    cx.spawn(async move |cx| {
                        delegate.set_status(Some("Waiting for existing connection attempt"), cx)
                    })
                    .detach();
                    return task;
                }
                log::debug!("Connecting task is dead, removing it and restarting a connection");
                self.connections.remove(&opts);
            }
            Some(ConnectionPoolEntry::Connected(remote)) => {
                if let Some(remote) = remote.upgrade()
                    && !remote.has_been_killed()
                {
                    log::debug!("Connection is still alive");
                    return Task::ready(Ok(remote)).shared();
                }
                log::debug!("Connection is dead, removing it and restarting a connection");
                self.connections.remove(&opts);
            }
            None => {
                log::debug!("No existing connection found, starting a new one");
            }
        }

        let task = cx
            .spawn({
                let opts = opts.clone();
                let delegate = delegate.clone();
                async move |cx| {
                    let connection = match opts.clone() {
                        RemoteConnectionOptions::Ssh(opts) => {
                            SshRemoteConnection::new(opts, delegate, cx)
                                .await
                                .map(|connection| Arc::new(connection) as Arc<dyn RemoteConnection>)
                        }
                        RemoteConnectionOptions::Wsl(opts) => {
                            WslRemoteConnection::new(opts, delegate, cx)
                                .await
                                .map(|connection| Arc::new(connection) as Arc<dyn RemoteConnection>)
                        }
                        RemoteConnectionOptions::Docker(opts) => {
                            DockerExecConnection::new(opts, delegate, cx)
                                .await
                                .map(|connection| Arc::new(connection) as Arc<dyn RemoteConnection>)
                        }
                        #[cfg(any(test, feature = "test-support"))]
                        RemoteConnectionOptions::Mock(opts) => match cx.update(|cx| {
                            cx.default_global::<crate::transport::mock::MockConnectionRegistry>()
                                .take(&opts)
                        }) {
                            Some(connection) => Ok(connection.await as Arc<dyn RemoteConnection>),
                            None => Err(anyhow!(
                                "Mock connection not found. Call MockConnection::new() first."
                            )),
                        },
                    };

                    cx.update_global(|pool: &mut Self, _| {
                        debug_assert!(matches!(
                            pool.connections.get(&opts),
                            Some(ConnectionPoolEntry::Connecting(_))
                        ));
                        match connection {
                            Ok(connection) => {
                                pool.connections.insert(
                                    opts.clone(),
                                    ConnectionPoolEntry::Connected(Arc::downgrade(&connection)),
                                );
                                Ok(connection)
                            }
                            Err(error) => {
                                pool.connections.remove(&opts);
                                Err(Arc::new(error))
                            }
                        }
                    })
                }
            })
            .shared();
        if let Some(task) = task.downgrade() {
            self.connections
                .insert(opts.clone(), ConnectionPoolEntry::Connecting(task));
        }
        task
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum RemoteConnectionOptions {
    Ssh(SshConnectionOptions),
    Wsl(WslConnectionOptions),
    Docker(DockerConnectionOptions),
    #[cfg(any(test, feature = "test-support"))]
    Mock(crate::transport::mock::MockConnectionOptions),
}

impl RemoteConnectionOptions {
    pub fn display_name(&self) -> String {
        match self {
            RemoteConnectionOptions::Ssh(opts) => opts
                .nickname
                .clone()
                .unwrap_or_else(|| opts.host.to_string()),
            RemoteConnectionOptions::Wsl(opts) => opts.distro_name.clone(),
            RemoteConnectionOptions::Docker(opts) => {
                if opts.use_podman {
                    format!("[podman] {}", opts.name)
                } else {
                    opts.name.clone()
                }
            }
            #[cfg(any(test, feature = "test-support"))]
            RemoteConnectionOptions::Mock(opts) => format!("mock-{}", opts.id),
        }
    }

    /// A stable identifier for the kind of remote connection, suitable for
    /// telemetry (e.g. `"ssh"`, `"wsl"`, `"docker"`, `"podman"`).
    pub fn connection_type(&self) -> &'static str {
        match self {
            RemoteConnectionOptions::Ssh(_) => "ssh",
            RemoteConnectionOptions::Wsl(_) => "wsl",
            RemoteConnectionOptions::Docker(opts) => {
                if opts.use_podman {
                    "podman"
                } else {
                    "docker"
                }
            }
            #[cfg(any(test, feature = "test-support"))]
            RemoteConnectionOptions::Mock(_) => "mock",
        }
    }

    pub fn host(&self) -> String {
        match self {
            RemoteConnectionOptions::Ssh(opts) => opts.host.to_string(),
            RemoteConnectionOptions::Wsl(opts) => opts.distro_name.clone(),
            RemoteConnectionOptions::Docker(opts) => opts.name.clone(),
            #[cfg(any(test, feature = "test-support"))]
            RemoteConnectionOptions::Mock(opts) => format!("mock-{}", opts.id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use rpc::{
        AnyProtoClient, ErrorCodeExt, MAX_QUEUED_EARLY_MESSAGES, MAX_QUEUED_EARLY_MESSAGES_AGE,
        proto::ErrorCode,
    };

    #[test]
    fn test_ssh_display_name_prefers_nickname() {
        let options = RemoteConnectionOptions::Ssh(SshConnectionOptions {
            host: "1.2.3.4".into(),
            nickname: Some("My Cool Project".to_string()),
            ..Default::default()
        });

        assert_eq!(options.display_name(), "My Cool Project");
    }

    #[test]
    fn test_ssh_display_name_falls_back_to_host() {
        let options = RemoteConnectionOptions::Ssh(SshConnectionOptions {
            host: "1.2.3.4".into(),
            ..Default::default()
        });

        assert_eq!(options.display_name(), "1.2.3.4");
    }

    #[test]
    fn test_connection_type() {
        assert_eq!(
            RemoteConnectionOptions::Ssh(SshConnectionOptions::default()).connection_type(),
            "ssh"
        );
        assert_eq!(
            RemoteConnectionOptions::Wsl(WslConnectionOptions {
                distro_name: "Ubuntu".to_string(),
                user: None,
            })
            .connection_type(),
            "wsl"
        );
        assert_eq!(
            RemoteConnectionOptions::Docker(DockerConnectionOptions {
                use_podman: false,
                ..Default::default()
            })
            .connection_type(),
            "docker"
        );
        assert_eq!(
            RemoteConnectionOptions::Docker(DockerConnectionOptions {
                use_podman: true,
                ..Default::default()
            })
            .connection_type(),
            "podman"
        );
    }

    #[gpui::test]
    async fn test_channel_client_request_stream_terminates_on_error(cx: &mut TestAppContext) {
        let (incoming_tx, incoming_rx) = mpsc::unbounded::<Envelope>();
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded::<Envelope>();

        let client =
            cx.update(|cx| ChannelClient::new(incoming_rx, outgoing_tx, cx, "test-client", false));

        // The client sends RemoteStarted on startup; drain the outgoing channel
        // so it doesn't block.
        let _drain_outgoing = cx
            .executor()
            .spawn(async move { while outgoing_rx.next().await.is_some() {} });

        let mut stream = client
            .request_stream_dynamic(proto::Test { id: 0 }.into_envelope(0, None, None), "Test")
            .await
            .unwrap();

        let request_id = 0;

        incoming_tx
            .unbounded_send(proto::Test { id: 1 }.into_envelope(100, Some(request_id), None))
            .unwrap();

        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(
            proto::Test::from_envelope(first).unwrap(),
            proto::Test { id: 1 }
        );

        // Send an Error without a trailing EndStream. The Error alone should
        // terminate the stream.
        incoming_tx
            .unbounded_send(
                ErrorCode::Internal
                    .message("boom".to_string())
                    .to_proto()
                    .into_envelope(101, Some(request_id), None),
            )
            .unwrap();

        let second = stream.next().await.unwrap();
        let error = second.unwrap_err();
        assert!(
            format!("{error}").contains("boom"),
            "expected error to surface server message, got: {error}"
        );

        assert!(stream.next().await.is_none());
        assert_eq!(client.stream_response_channels.lock().len(), 0);
    }

    #[gpui::test]
    async fn test_channel_client_dropping_stream_request_before_response_cleans_up_channel(
        cx: &mut TestAppContext,
    ) {
        let (_incoming_tx, incoming_rx) = mpsc::unbounded::<Envelope>();
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded::<Envelope>();

        let client =
            cx.update(|cx| ChannelClient::new(incoming_rx, outgoing_tx, cx, "test-client", false));

        let _drain_outgoing = cx
            .executor()
            .spawn(async move { while outgoing_rx.next().await.is_some() {} });

        let stream = client
            .request_stream_dynamic(proto::Test { id: 0 }.into_envelope(0, None, None), "Test")
            .await
            .unwrap();

        assert_eq!(client.stream_response_channels.lock().len(), 1);

        drop(stream);
        cx.run_until_parked();

        assert_eq!(
            client.stream_response_channels.lock().len(),
            0,
            "dropping a stream before any responses arrive should remove response channel bookkeeping"
        );
    }

    #[gpui::test]
    async fn test_channel_client_dropping_stream_request_before_completion(
        cx: &mut TestAppContext,
    ) {
        let (incoming_tx, incoming_rx) = mpsc::unbounded::<Envelope>();
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded::<Envelope>();

        let client =
            cx.update(|cx| ChannelClient::new(incoming_rx, outgoing_tx, cx, "test-client", false));

        let _drain_outgoing = cx
            .executor()
            .spawn(async move { while outgoing_rx.next().await.is_some() {} });

        let mut stream = client
            .request_stream_dynamic(proto::Test { id: 0 }.into_envelope(0, None, None), "Test")
            .await
            .unwrap();

        let request_id = 0;

        incoming_tx
            .unbounded_send(proto::Test { id: 1 }.into_envelope(100, Some(request_id), None))
            .unwrap();
        let _ = stream.next().await.unwrap().unwrap();

        assert_eq!(client.stream_response_channels.lock().len(), 1);

        drop(stream);

        // Inject an orphaned non-terminal response. The read loop should detect
        // that the consumer has been dropped and clean up its bookkeeping (no
        // EndStream sent here on purpose, otherwise the cleanup would happen
        // via the terminal-response path and mask the bug under test).
        incoming_tx
            .unbounded_send(proto::Test { id: 2 }.into_envelope(101, Some(request_id), None))
            .unwrap();

        cx.run_until_parked();

        assert_eq!(
            client.stream_response_channels.lock().len(),
            0,
            "stream channel should be removed once the consumer has dropped the stream"
        );
    }

    /// ExternalAgentsUpdated is pushed as soon as the remote server has a session,
    /// which is before this client has a Project to register AgentServerStore
    /// handlers. Answering that push as "no handler" is not a failure of the
    /// connection: it is the handshake order, and logging it at ERROR on every
    /// SSH/WSL/Docker connect hides real errors.
    #[gpui::test]
    async fn an_external_agents_updated_push_before_handlers_is_not_answered_as_unhandled(
        cx: &mut TestAppContext,
    ) {
        let (incoming_tx, incoming_rx) = mpsc::unbounded::<Envelope>();
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded::<Envelope>();

        let client =
            cx.update(|cx| ChannelClient::new(incoming_rx, outgoing_tx, cx, "test-client", false));

        let remote_started = outgoing_rx
            .next()
            .with_timeout(Duration::from_secs(5), &cx.executor())
            .await;
        let remote_started = match remote_started {
            Ok(Some(envelope)) => envelope,
            other => panic!(
                "ChannelClient sends RemoteStarted on startup; expected that envelope within 5s, got {other:?}"
            ),
        };
        assert!(
            matches!(
                remote_started.payload,
                Some(proto::envelope::Payload::RemoteStarted(_))
            ),
            "ChannelClient sends RemoteStarted on startup; expected that envelope, got {remote_started:?}"
        );

        incoming_tx
            .unbounded_send(
                proto::ExternalAgentsUpdated {
                    project_id: proto::REMOTE_SERVER_PROJECT_ID,
                    names: vec!["claude".to_string()],
                }
                .into_envelope(1, None, None),
            )
            .expect("incoming channel should be open");

        cx.run_until_parked();

        let reply = outgoing_rx.next().now_or_never();
        assert!(
            reply.is_none(),
            "ExternalAgentsUpdated is a one-way handshake push, not a request waiting \
             for a handler; expected no outgoing envelope, got {reply:?}"
        );

        struct ReceivedAgents {
            names: Vec<String>,
        }

        let received = cx.update(|cx| {
            let received = cx.new(|_| ReceivedAgents { names: Vec::new() });
            let proto_client = AnyProtoClient::from(client.clone());
            proto_client.subscribe_to_entity(proto::REMOTE_SERVER_PROJECT_ID, &received);
            proto_client.add_entity_message_handler(
                |this: Entity<ReceivedAgents>,
                 envelope: rpc::TypedEnvelope<proto::ExternalAgentsUpdated>,
                 mut cx| async move {
                    this.update(&mut cx, |this, _cx| {
                        this.names = envelope.payload.names;
                    });
                    Ok(())
                },
            );
            proto_client.flush_queued_early_messages(cx);
            received
        });
        cx.run_until_parked();

        let names = received.read_with(cx, |received, _| received.names.clone());
        assert_eq!(
            names,
            vec!["claude".to_string()],
            "the push that arrived before handlers must still be delivered once they exist; \
             expected [\"claude\"], got {names:?}"
        );

        drop((client, incoming_tx));
    }

    /// A ChannelClient that never reaches Project::remote must not hold every
    /// unmatched envelope for the rest of its life, and must not keep answering
    /// those envelopes as handled. After the early-message cap, later pushes
    /// are unhandled again — the ERROR that this queue exists to delay, not to
    /// delete.
    #[gpui::test]
    async fn an_unflushed_early_message_queue_stops_growing_and_reports_later_messages_as_unhandled(
        cx: &mut TestAppContext,
    ) {
        let (incoming_tx, incoming_rx) = mpsc::unbounded::<Envelope>();
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded::<Envelope>();

        let client =
            cx.update(|cx| ChannelClient::new(incoming_rx, outgoing_tx, cx, "test-client", false));

        let remote_started = outgoing_rx
            .next()
            .with_timeout(Duration::from_secs(5), &cx.executor())
            .await;
        let remote_started = match remote_started {
            Ok(Some(envelope)) => envelope,
            other => panic!(
                "ChannelClient sends RemoteStarted on startup; expected that envelope within 5s, got {other:?}"
            ),
        };
        assert!(
            matches!(
                remote_started.payload,
                Some(proto::envelope::Payload::RemoteStarted(_))
            ),
            "ChannelClient sends RemoteStarted on startup; expected that envelope, got {remote_started:?}"
        );

        let over_cap = MAX_QUEUED_EARLY_MESSAGES + 8;
        for message_id in 1..=over_cap as u32 {
            incoming_tx
                .unbounded_send(
                    proto::ExternalAgentsUpdated {
                        project_id: proto::REMOTE_SERVER_PROJECT_ID,
                        names: vec!["claude".to_string()],
                    }
                    .into_envelope(message_id, None, None),
                )
                .expect("incoming channel should be open");
        }

        cx.run_until_parked();

        let held = client.message_handlers.lock().queued_early_message_count();
        assert!(
            held <= MAX_QUEUED_EARLY_MESSAGES,
            "early-message queue must not grow without bound on a connection that never flushes; \
             cap is {MAX_QUEUED_EARLY_MESSAGES}, held {held}"
        );
        assert_eq!(
            held, MAX_QUEUED_EARLY_MESSAGES,
            "the handshake allowance should be kept for a late flush; cap is {MAX_QUEUED_EARLY_MESSAGES}, held {held}"
        );
        assert!(
            !client.message_handlers.lock().queue_early_messages,
            "exceeding the cap must turn queueing off so later envelopes are unhandled again"
        );

        let mut unhandled = Vec::new();
        while let Some(envelope) = outgoing_rx.next().now_or_never().flatten() {
            match envelope.payload {
                Some(proto::envelope::Payload::Error(_)) => {
                    unhandled.push(envelope.responding_to);
                }
                other => panic!(
                    "after the handshake pushes, only unhandled-error responses should go out; got {other:?}"
                ),
            }
        }
        let expected_unhandled: Vec<Option<u32>> = ((MAX_QUEUED_EARLY_MESSAGES as u32 + 1)
            ..=over_cap as u32)
            .map(Some)
            .collect();
        assert_eq!(
            unhandled, expected_unhandled,
            "envelopes past the cap must be reported unhandled (error response to their id), \
             not swallowed as handled; expected {expected_unhandled:?}, got {unhandled:?}"
        );

        drop((client, incoming_tx));
    }

    /// The cap is there so a handshake that never completes cannot grow
    /// forever. A Project::remote that is only a few seconds late is still
    /// that handshake: the push must be delivered, not cut off by the bound.
    #[gpui::test]
    async fn a_slow_handshake_flush_within_the_early_message_bounds_still_delivers(
        cx: &mut TestAppContext,
    ) {
        let slow_handshake = Duration::from_secs(5);
        assert!(
            slow_handshake < MAX_QUEUED_EARLY_MESSAGES_AGE,
            "this delay is a slow handshake, not an expired one; bound is {:?}, delay is {:?}",
            MAX_QUEUED_EARLY_MESSAGES_AGE,
            slow_handshake
        );

        let (incoming_tx, incoming_rx) = mpsc::unbounded::<Envelope>();
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded::<Envelope>();

        let client =
            cx.update(|cx| ChannelClient::new(incoming_rx, outgoing_tx, cx, "test-client", false));

        let remote_started = outgoing_rx
            .next()
            .with_timeout(Duration::from_secs(5), &cx.executor())
            .await;
        let remote_started = match remote_started {
            Ok(Some(envelope)) => envelope,
            other => panic!(
                "ChannelClient sends RemoteStarted on startup; expected that envelope within 5s, got {other:?}"
            ),
        };
        assert!(
            matches!(
                remote_started.payload,
                Some(proto::envelope::Payload::RemoteStarted(_))
            ),
            "ChannelClient sends RemoteStarted on startup; expected that envelope, got {remote_started:?}"
        );

        incoming_tx
            .unbounded_send(
                proto::ExternalAgentsUpdated {
                    project_id: proto::REMOTE_SERVER_PROJECT_ID,
                    names: vec!["claude".to_string()],
                }
                .into_envelope(1, None, None),
            )
            .expect("incoming channel should be open");

        cx.run_until_parked();
        cx.executor().advance_clock(slow_handshake);

        struct ReceivedAgents {
            names: Vec<String>,
        }

        let received = cx.update(|cx| {
            let received = cx.new(|_| ReceivedAgents { names: Vec::new() });
            let proto_client = AnyProtoClient::from(client.clone());
            proto_client.subscribe_to_entity(proto::REMOTE_SERVER_PROJECT_ID, &received);
            proto_client.add_entity_message_handler(
                |this: Entity<ReceivedAgents>,
                 envelope: rpc::TypedEnvelope<proto::ExternalAgentsUpdated>,
                 mut cx| async move {
                    this.update(&mut cx, |this, _cx| {
                        this.names = envelope.payload.names;
                    });
                    Ok(())
                },
            );
            proto_client.flush_queued_early_messages(cx);
            received
        });
        cx.run_until_parked();

        let names = received.read_with(cx, |received, _| received.names.clone());
        assert_eq!(
            names,
            vec!["claude".to_string()],
            "a flush that arrives inside the early-message bounds must still deliver the \
             handshake push; expected [\"claude\"], got {names:?}"
        );

        drop((client, incoming_tx));
    }

    /// One consumer that has stopped reading must not silence the whole connection.
    ///
    /// Every response on a connection is handed to its requester by one loop, and that
    /// loop waits for each hand-off to be taken before it reads the next envelope. A
    /// requester whose future is still alive but is no longer being polled never takes
    /// it, so the loop stops there — and every later response, for every other panel and
    /// every other store on that connection, is never delivered. That is one stalled
    /// reader turning into a connection that answers nothing, with no error anywhere.
    #[gpui::test]
    async fn a_response_no_one_is_polling_does_not_stop_the_rest_of_the_connection(
        cx: &mut TestAppContext,
    ) {
        let (incoming_tx, incoming_rx) = mpsc::unbounded::<Envelope>();
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded::<Envelope>();

        let client =
            cx.update(|cx| ChannelClient::new(incoming_rx, outgoing_tx, cx, "test-client", false));

        let sent = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let _drain_outgoing = cx.executor().spawn({
            let sent = sent.clone();
            async move {
                while let Some(envelope) = outgoing_rx.next().await {
                    sent.lock().push(envelope);
                }
            }
        });

        // Issued and then left alone: the future is alive, holds its half of the
        // hand-off, and is never polled again. This is a consumer awaiting something
        // else, not one that has gone away — a dropped one is already handled.
        let stalled = client.request_dynamic(
            proto::Test { id: 1 }.into_envelope(0, None, None),
            "Test",
            true,
        );
        let _stalled = Box::pin(stalled);

        let answered = client.request_dynamic(
            proto::Test { id: 2 }.into_envelope(0, None, None),
            "Test",
            true,
        );

        cx.run_until_parked();
        let request_ids: Vec<u32> = sent
            .lock()
            .iter()
            .filter(|envelope| matches!(envelope.payload, Some(proto::envelope::Payload::Test(_))))
            .map(|envelope| envelope.id)
            .collect();
        assert_eq!(
            request_ids.len(),
            2,
            "both requests must have been sent before either is answered; got {request_ids:?}"
        );

        // The host answers both, the stalled one first, which is the order that puts the
        // stalled hand-off in front of the one that is being waited on.
        incoming_tx
            .unbounded_send(proto::Test { id: 11 }.into_envelope(500, Some(request_ids[0]), None))
            .unwrap();
        incoming_tx
            .unbounded_send(proto::Test { id: 12 }.into_envelope(501, Some(request_ids[1]), None))
            .unwrap();

        let answer = answered
            .with_timeout(Duration::from_secs(30), &cx.executor())
            .await;
        let answer = match answer {
            Ok(answer) => answer.expect("the second request was answered by the host"),
            Err(_) => panic!(
                "the second request was answered by the host and still never arrived: it was \
                 waited on for 30s while the response before it sat in a hand-off nobody took"
            ),
        };
        assert_eq!(
            proto::Test::from_envelope(answer).expect("a Test response"),
            proto::Test { id: 12 },
            "each requester must get its own response"
        );
    }

    /// What taking the hand-off wait away could let through: a response going to the
    /// wrong requester, or one being dropped, when a consumer is slow rather than stuck.
    #[gpui::test]
    async fn a_slow_consumer_still_gets_its_own_response(cx: &mut TestAppContext) {
        let (incoming_tx, incoming_rx) = mpsc::unbounded::<Envelope>();
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded::<Envelope>();

        let client =
            cx.update(|cx| ChannelClient::new(incoming_rx, outgoing_tx, cx, "test-client", false));

        let sent = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let _drain_outgoing = cx.executor().spawn({
            let sent = sent.clone();
            async move {
                while let Some(envelope) = outgoing_rx.next().await {
                    sent.lock().push(envelope);
                }
            }
        });

        let first = client.request_dynamic(
            proto::Test { id: 1 }.into_envelope(0, None, None),
            "Test",
            true,
        );
        let second = client.request_dynamic(
            proto::Test { id: 2 }.into_envelope(0, None, None),
            "Test",
            true,
        );

        cx.run_until_parked();
        let request_ids: Vec<u32> = sent
            .lock()
            .iter()
            .filter(|envelope| matches!(envelope.payload, Some(proto::envelope::Payload::Test(_))))
            .map(|envelope| envelope.id)
            .collect();
        assert_eq!(request_ids.len(), 2, "both requests are in flight");

        incoming_tx
            .unbounded_send(proto::Test { id: 21 }.into_envelope(500, Some(request_ids[0]), None))
            .unwrap();
        incoming_tx
            .unbounded_send(proto::Test { id: 22 }.into_envelope(501, Some(request_ids[1]), None))
            .unwrap();

        // Read out of order and after a delay: the responses were already delivered, so
        // which one each requester gets cannot depend on when it looks.
        cx.executor().advance_clock(Duration::from_secs(5));
        cx.run_until_parked();

        let second = second
            .with_timeout(Duration::from_secs(30), &cx.executor())
            .await
            .expect("the second response arrived")
            .expect("the second request succeeded");
        let first = first
            .with_timeout(Duration::from_secs(30), &cx.executor())
            .await
            .expect("the first response arrived")
            .expect("the first request succeeded");

        assert_eq!(
            (
                proto::Test::from_envelope(first).expect("a Test response"),
                proto::Test::from_envelope(second).expect("a Test response"),
            ),
            (proto::Test { id: 21 }, proto::Test { id: 22 }),
            "each request must be answered with the response the host sent for it"
        );
    }

    #[test]
    fn test_ssh_host_ignores_nickname() {
        let options = RemoteConnectionOptions::Ssh(SshConnectionOptions {
            host: "1.2.3.4".into(),
            nickname: Some("My Cool Project".to_string()),
            ..Default::default()
        });

        assert_eq!(options.host(), "1.2.3.4");
    }
}

impl From<SshConnectionOptions> for RemoteConnectionOptions {
    fn from(opts: SshConnectionOptions) -> Self {
        RemoteConnectionOptions::Ssh(opts)
    }
}

impl From<WslConnectionOptions> for RemoteConnectionOptions {
    fn from(opts: WslConnectionOptions) -> Self {
        RemoteConnectionOptions::Wsl(opts)
    }
}

#[cfg(any(test, feature = "test-support"))]
impl From<crate::transport::mock::MockConnectionOptions> for RemoteConnectionOptions {
    fn from(opts: crate::transport::mock::MockConnectionOptions) -> Self {
        RemoteConnectionOptions::Mock(opts)
    }
}

#[cfg(target_os = "windows")]
/// Open a wsl path (\\wsl.localhost\<distro>\path)
#[derive(Debug, Clone, PartialEq, Eq, gpui::Action)]
#[action(namespace = workspace, no_json, no_register)]
pub struct OpenWslPath {
    pub distro: WslConnectionOptions,
    pub paths: Vec<PathBuf>,
}

#[async_trait(?Send)]
pub trait RemoteConnection: Send + Sync {
    fn start_proxy(
        &self,
        unique_identifier: String,
        reconnect: bool,
        incoming_tx: UnboundedSender<Envelope>,
        outgoing_rx: UnboundedReceiver<Envelope>,
        connection_activity_tx: Sender<()>,
        delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Task<Result<i32>>;
    fn upload_directory(
        &self,
        src_path: PathBuf,
        dest_path: RemotePathBuf,
        cx: &App,
    ) -> Task<Result<()>>;
    async fn kill(&self) -> Result<()>;
    fn has_been_killed(&self) -> bool;
    fn shares_network_interface(&self) -> bool {
        false
    }
    fn build_command(
        &self,
        program: Option<String>,
        args: &[String],
        env: &HashMap<String, String>,
        working_dir: Option<String>,
        port_forward: Option<(u16, String, u16)>,
        interactive: Interactive,
    ) -> Result<CommandTemplate>;
    fn build_forward_ports_command(
        &self,
        forwards: Vec<(u16, String, u16)>,
    ) -> Result<CommandTemplate>;
    fn connection_options(&self) -> RemoteConnectionOptions;
    fn path_style(&self) -> PathStyle;
    /// The remote platform (OS and architecture), detected during connection setup.
    fn remote_platform(&self) -> RemotePlatform;
    /// The remote host's OS version (e.g. `"ubuntu 24.04"` or `"15.6.1"`),
    /// detected during connection setup. `None` if it could not be determined.
    fn remote_os_version(&self) -> Option<String>;
    fn shell(&self) -> String;
    fn default_system_shell(&self) -> String;
    fn has_wsl_interop(&self) -> bool;

    #[cfg(any(test, feature = "test-support"))]
    fn simulate_disconnect(&self, _: &AsyncApp) {}
}

type ResponseChannels = Mutex<HashMap<MessageId, oneshot::Sender<(Envelope, oneshot::Sender<()>)>>>;
type StreamResponseChannels =
    Arc<Mutex<HashMap<MessageId, UnboundedSender<(Result<Envelope>, oneshot::Sender<()>)>>>>;

struct Signal<T: 'static> {
    tx: Mutex<Option<oneshot::Sender<T>>>,
    rx: Shared<Task<Option<T>>>,
}

impl<T: Send + Clone + 'static> Signal<T> {
    pub fn new(cx: &App) -> Self {
        let (tx, rx) = oneshot::channel();

        let task = cx
            .background_executor()
            .spawn(async move { rx.await.ok() })
            .shared();

        Self {
            tx: Mutex::new(Some(tx)),
            rx: task,
        }
    }

    fn set(&self, value: T) {
        if let Some(tx) = self.tx.lock().take() {
            let _ = tx.send(value);
        }
    }

    fn wait(&self) -> Shared<Task<Option<T>>> {
        self.rx.clone()
    }
}

/// How long the read loop holds a streamed response back for a reader that is not
/// keeping up, before it goes on delivering to everyone else.
const STREAM_CONSUMER_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct ChannelClient {
    next_message_id: AtomicU32,
    outgoing_tx: Mutex<mpsc::UnboundedSender<Envelope>>,
    buffer: Mutex<VecDeque<Envelope>>,
    response_channels: ResponseChannels,
    stream_response_channels: StreamResponseChannels,
    message_handlers: Mutex<ProtoMessageHandlerSet>,
    max_received: AtomicU32,
    name: &'static str,
    task: Mutex<Task<Result<()>>>,
    remote_started: Signal<()>,
    has_wsl_interop: bool,
    executor: BackgroundExecutor,
}

impl ChannelClient {
    pub(crate) fn new(
        incoming_rx: mpsc::UnboundedReceiver<Envelope>,
        outgoing_tx: mpsc::UnboundedSender<Envelope>,
        cx: &App,
        name: &'static str,
        has_wsl_interop: bool,
    ) -> Arc<Self> {
        Arc::new_cyclic(|this| Self {
            outgoing_tx: Mutex::new(outgoing_tx),
            next_message_id: AtomicU32::new(0),
            max_received: AtomicU32::new(0),
            response_channels: ResponseChannels::default(),
            stream_response_channels: StreamResponseChannels::default(),
            message_handlers: Mutex::new({
                let mut handlers = ProtoMessageHandlerSet::default();
                // The socket is read from the moment this client exists, which
                // is before Project::remote can subscribe stores. Hold those
                // envelopes instead of answering them as unhandled.
                handlers.queue_early_messages = true;
                handlers
            }),
            buffer: Mutex::new(VecDeque::new()),
            name,
            executor: cx.background_executor().clone(),
            task: Mutex::new(Self::start_handling_messages(
                this.clone(),
                incoming_rx,
                &cx.to_async(),
            )),
            remote_started: Signal::new(cx),
            has_wsl_interop,
        })
    }

    fn wait_for_remote_started(&self) -> Shared<Task<Option<()>>> {
        self.remote_started.wait()
    }

    fn start_handling_messages(
        this: Weak<Self>,
        mut incoming_rx: mpsc::UnboundedReceiver<Envelope>,
        cx: &AsyncApp,
    ) -> Task<Result<()>> {
        cx.spawn(async move |cx| {
            if let Some(this) = this.upgrade() {
                let envelope = proto::RemoteStarted {}.into_envelope(0, None, None);
                this.outgoing_tx.lock().unbounded_send(envelope).ok();
            };

            let peer_id = PeerId { owner_id: 0, id: 0 };
            while let Some(incoming) = incoming_rx.next().await {
                let Some(this) = this.upgrade() else {
                    return anyhow::Ok(());
                };
                if let Some(ack_id) = incoming.ack_id {
                    let mut buffer = this.buffer.lock();
                    while buffer.front().is_some_and(|msg| msg.id <= ack_id) {
                        buffer.pop_front();
                    }
                }
                if let Some(proto::envelope::Payload::FlushBufferedMessages(_)) = &incoming.payload
                {
                    log::debug!(
                        "{}:remote message received. name:FlushBufferedMessages",
                        this.name
                    );
                    {
                        let buffer = this.buffer.lock();
                        for envelope in buffer.iter() {
                            this.outgoing_tx
                                .lock()
                                .unbounded_send(envelope.clone())
                                .ok();
                        }
                    }
                    let mut envelope = proto::Ack {}.into_envelope(0, Some(incoming.id), None);
                    envelope.id = this.next_message_id.fetch_add(1, SeqCst);
                    this.outgoing_tx.lock().unbounded_send(envelope).ok();
                    continue;
                }

                if let Some(proto::envelope::Payload::RemoteStarted(_)) = &incoming.payload {
                    this.remote_started.set(());
                    let mut envelope = proto::Ack {}.into_envelope(0, Some(incoming.id), None);
                    envelope.id = this.next_message_id.fetch_add(1, SeqCst);
                    this.outgoing_tx.lock().unbounded_send(envelope).ok();
                    continue;
                }

                this.max_received.store(incoming.id, SeqCst);

                if let Some(request_id) = incoming.responding_to {
                    let request_id = MessageId(request_id);
                    // An incoming response with no payload is malformed; drop
                    // it. The request future and any stream consumers will
                    // remain pending until either a real response arrives or
                    // the connection is torn down.
                    if incoming.payload.is_none() {
                        continue;
                    }
                    let sender = this.response_channels.lock().remove(&request_id);
                    if let Some(sender) = sender {
                        // Handed over without waiting for it to be taken. The loop that
                        // runs this is the only one delivering responses on the
                        // connection, so anything it waits for is something every later
                        // response waits for too: a requester whose future is alive but
                        // not currently being polled — one task awaiting several requests
                        // the far end answered out of order is enough — would never take
                        // the hand-off, and the connection would answer nothing again,
                        // for any panel, with no error raised anywhere.
                        //
                        // Nothing is lost by not waiting. The channel is a one-shot, so
                        // it holds one response and the in-flight requests already bound
                        // how many there can be; and the requester drops the barrier
                        // before it looks at the response, so the wait never ordered any
                        // work either.
                        let (tx, _barrier) = oneshot::channel();
                        sender.send((incoming, tx)).ok();
                    } else {
                        let terminal_stream_response = matches!(
                            &incoming.payload,
                            Some(proto::envelope::Payload::Error(_))
                                | Some(proto::envelope::Payload::EndStream(_))
                        );
                        let sender = if terminal_stream_response {
                            this.stream_response_channels.lock().remove(&request_id)
                        } else {
                            this.stream_response_channels
                                .lock()
                                .get(&request_id)
                                .cloned()
                        };
                        if let Some(sender) = sender {
                            let (tx, rx) = oneshot::channel();
                            if sender.unbounded_send((Ok(incoming), tx)).is_err() {
                                this.stream_response_channels.lock().remove(&request_id);
                                continue;
                            }
                            // A stream's channel is unbounded and one request may answer
                            // with many envelopes, so unlike a one-shot response this
                            // wait is the only thing holding a fast sender to the speed
                            // of its reader. It is bounded all the same: a reader that
                            // has stopped reading must cost memory rather than cost every
                            // other request on the connection its answer.
                            if rx
                                .with_timeout(STREAM_CONSUMER_TIMEOUT, &this.executor)
                                .await
                                .is_err()
                            {
                                log::warn!(
                                    "{}: a streamed response has not been read for {:?}; \
                                     delivering the rest without waiting for its reader",
                                    this.name,
                                    STREAM_CONSUMER_TIMEOUT
                                );
                            }
                        }
                    }
                } else if let Some(envelope) =
                    build_typed_envelope(peer_id, Instant::now(), incoming)
                {
                    let type_name = envelope.payload_type_name();
                    let message_id = envelope.message_id();
                    if let Some(future) = ProtoMessageHandlerSet::handle_message(
                        &this.message_handlers,
                        envelope,
                        this.clone().into(),
                        cx.clone(),
                    ) {
                        log::debug!("{}:remote message received. name:{type_name}", this.name);
                        cx.foreground_executor()
                            .spawn(async move {
                                match future.await {
                                    Ok(_) => {
                                        log::debug!(
                                            "{}:remote message handled. name:{type_name}",
                                            this.name
                                        );
                                    }
                                    Err(error) => {
                                        log::error!(
                                            "{}:error handling message. type:{}, error:{:#}",
                                            this.name,
                                            type_name,
                                            format!("{error:#}").lines().fold(
                                                String::new(),
                                                |mut message, line| {
                                                    if !message.is_empty() {
                                                        message.push(' ');
                                                    }
                                                    message.push_str(line);
                                                    message
                                                }
                                            )
                                        );
                                    }
                                }
                            })
                            .detach()
                    } else {
                        log::error!("{}:unhandled remote message name:{type_name}", this.name);
                        if let Err(e) = AnyProtoClient::from(this.clone()).send_response(
                            message_id,
                            anyhow::anyhow!("no handler registered for {type_name}").to_proto(),
                        ) {
                            log::error!(
                                "{}:error sending error response for {type_name}:{e:#}",
                                this.name
                            );
                        }
                    }
                }
            }
            anyhow::Ok(())
        })
    }

    pub(crate) fn reconnect(
        self: &Arc<Self>,
        incoming_rx: UnboundedReceiver<Envelope>,
        outgoing_tx: UnboundedSender<Envelope>,
        cx: &AsyncApp,
    ) {
        *self.outgoing_tx.lock() = outgoing_tx;
        *self.task.lock() = Self::start_handling_messages(Arc::downgrade(self), incoming_rx, cx);
    }

    fn request<T: RequestMessage>(
        &self,
        payload: T,
    ) -> impl 'static + Future<Output = Result<T::Response>> {
        self.request_internal(payload, true)
    }

    fn request_internal<T: RequestMessage>(
        &self,
        payload: T,
        use_buffer: bool,
    ) -> impl 'static + Future<Output = Result<T::Response>> {
        log::debug!("remote request start. name:{}", T::NAME);
        let response =
            self.request_dynamic(payload.into_envelope(0, None, None), T::NAME, use_buffer);
        async move {
            let response = response.await?;
            log::debug!("remote request finish. name:{}", T::NAME);
            T::Response::from_envelope(response).context("received a response of the wrong type")
        }
    }

    async fn resync(&self, timeout: Duration) -> Result<()> {
        smol::future::or(
            async {
                self.request_internal(proto::FlushBufferedMessages {}, false)
                    .await?;

                for envelope in self.buffer.lock().iter() {
                    self.outgoing_tx
                        .lock()
                        .unbounded_send(envelope.clone())
                        .ok();
                }
                Ok(())
            },
            async {
                self.executor.timer(timeout).await;
                anyhow::bail!("Timed out resyncing remote client")
            },
        )
        .await
    }

    async fn ping(&self, timeout: Duration) -> Result<()> {
        smol::future::or(
            async {
                self.request(proto::Ping {}).await?;
                Ok(())
            },
            async {
                self.executor.timer(timeout).await;
                anyhow::bail!("Timed out pinging remote client")
            },
        )
        .await
    }

    fn send<T: EnvelopedMessage>(&self, payload: T) -> Result<()> {
        log::debug!("remote send name:{}", T::NAME);
        self.send_dynamic(payload.into_envelope(0, None, None))
    }

    fn request_dynamic(
        &self,
        mut envelope: proto::Envelope,
        type_name: &'static str,
        use_buffer: bool,
    ) -> impl 'static + Future<Output = Result<proto::Envelope>> {
        envelope.id = self.next_message_id.fetch_add(1, SeqCst);
        let (tx, rx) = oneshot::channel();
        let mut response_channels_lock = self.response_channels.lock();
        response_channels_lock.insert(MessageId(envelope.id), tx);
        drop(response_channels_lock);

        let result = if use_buffer {
            self.send_buffered(envelope)
        } else {
            self.send_unbuffered(envelope)
        };
        async move {
            if let Err(error) = &result {
                log::error!("failed to send message: {error}");
                anyhow::bail!("failed to send message: {error}");
            }

            let response = rx.await.context("connection lost")?.0;
            if let Some(proto::envelope::Payload::Error(error)) = &response.payload {
                return Err(RpcError::from_proto(error, type_name));
            }
            Ok(response)
        }
    }

    fn request_stream_dynamic(
        &self,
        mut envelope: proto::Envelope,
        type_name: &'static str,
    ) -> impl 'static + Future<Output = Result<BoxStream<'static, Result<proto::Envelope>>>> {
        envelope.id = self.next_message_id.fetch_add(1, SeqCst);
        let message_id = MessageId(envelope.id);
        let (tx, rx) = mpsc::unbounded();
        let stream_response_channels = self.stream_response_channels.clone();
        stream_response_channels.lock().insert(message_id, tx);

        let result = self.send_buffered(envelope);
        async move {
            if let Err(error) = &result {
                log::error!("failed to send message: {error}");
                anyhow::bail!("failed to send message: {error}");
            }

            let cleanup_stream_response_channel = util::defer({
                let stream_response_channels = stream_response_channels.clone();
                move || {
                    stream_response_channels.lock().remove(&message_id);
                }
            });

            Ok(rx
                .filter_map(move |(response, _barrier)| {
                    // Keep the cleanup guard alive until the returned stream is dropped.
                    let _keep_cleanup_guard_alive = &cleanup_stream_response_channel;
                    futures::future::ready(match response {
                        Ok(response) => {
                            if let Some(proto::envelope::Payload::Error(error)) = &response.payload
                            {
                                Some(Err(RpcError::from_proto(error, type_name)))
                            } else if let Some(proto::envelope::Payload::EndStream(_)) =
                                &response.payload
                            {
                                None
                            } else {
                                Some(Ok(response))
                            }
                        }
                        Err(error) => Some(Err(error)),
                    })
                })
                .boxed())
        }
    }

    pub fn send_dynamic(&self, mut envelope: proto::Envelope) -> Result<()> {
        envelope.id = self.next_message_id.fetch_add(1, SeqCst);
        self.send_buffered(envelope)
    }

    fn send_buffered(&self, mut envelope: proto::Envelope) -> Result<()> {
        envelope.ack_id = Some(self.max_received.load(SeqCst));
        self.buffer.lock().push_back(envelope.clone());
        // ignore errors on send (happen while we're reconnecting)
        // assume that the global "disconnected" overlay is sufficient.
        self.outgoing_tx.lock().unbounded_send(envelope).ok();
        Ok(())
    }

    fn send_unbuffered(&self, mut envelope: proto::Envelope) -> Result<()> {
        envelope.ack_id = Some(self.max_received.load(SeqCst));
        self.outgoing_tx.lock().unbounded_send(envelope).ok();
        Ok(())
    }
}

impl ProtoClient for ChannelClient {
    fn request(
        &self,
        envelope: proto::Envelope,
        request_type: &'static str,
    ) -> BoxFuture<'static, Result<proto::Envelope>> {
        self.request_dynamic(envelope, request_type, true).boxed()
    }

    fn request_stream(
        &self,
        envelope: proto::Envelope,
        request_type: &'static str,
    ) -> BoxFuture<'static, Result<BoxStream<'static, Result<proto::Envelope>>>> {
        self.request_stream_dynamic(envelope, request_type).boxed()
    }

    fn send(&self, envelope: proto::Envelope, _message_type: &'static str) -> Result<()> {
        self.send_dynamic(envelope)
    }

    fn send_response(&self, envelope: Envelope, _message_type: &'static str) -> anyhow::Result<()> {
        self.send_dynamic(envelope)
    }

    fn message_handler_set(&self) -> &Mutex<ProtoMessageHandlerSet> {
        &self.message_handlers
    }

    fn is_via_collab(&self) -> bool {
        false
    }

    fn has_wsl_interop(&self) -> bool {
        self.has_wsl_interop
    }
}
