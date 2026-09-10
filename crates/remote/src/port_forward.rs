//! Port forwarding over the remote RPC channel.
//!
//! `ssh -O forward` needs a ControlMaster socket, which Win32-OpenSSH does not
//! implement (see the comment in `transport/ssh.rs`), and the Docker and WSL
//! transports have no equivalent at all. Tunnelling the bytes through the RPC
//! channel that is already established is the only mechanism that covers every
//! local platform and every transport with a single code path.
//!
//! The channel underneath is an unbounded `mpsc`, so it applies no backpressure
//! of its own and `MessagePriority::Background` is a handler dispatch strategy
//! rather than a bandwidth priority. A tunnel that read a fast local socket as
//! quickly as it could would therefore grow the queue without bound and starve
//! file sync and LSP traffic sharing the same channel. Each direction of each
//! tunnel consequently carries its own credit window: at most
//! [`PORT_TUNNEL_WINDOW_SIZE`] bytes may be outstanding, credit is only returned
//! once the receiver has actually written the bytes into its socket, and a
//! sender that runs out of credit stops reading its socket so that the stall
//! propagates back over TCP to whoever is producing the data.

use anyhow::{Context as _, Result};
use collections::HashMap;
use futures::{
    AsyncReadExt as _, AsyncWriteExt as _, StreamExt as _,
    channel::{mpsc, oneshot},
};
use gpui::{AsyncApp, Context, Entity, SharedString, Task};
use parking_lot::Mutex;
use rpc::{AnyProtoClient, TypedEnvelope, proto};
use settings::SshPortForwardOption;
use smol::net::{TcpListener, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use util::ResultExt as _;

/// How much of a socket is read into a single `PortTunnelData` message.
pub const PORT_TUNNEL_CHUNK_SIZE: usize = 32 * 1024;

/// How many bytes one direction of one tunnel may have outstanding before it
/// stops reading its socket.
pub const PORT_TUNNEL_WINDOW_SIZE: u64 = 256 * 1024;

/// `ssh -L` binds localhost when the local side of a forward has no host, and
/// connects to localhost when the remote side has none.
const DEFAULT_LOCAL_HOST: &str = "127.0.0.1";
const DEFAULT_REMOTE_HOST: &str = "localhost";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortForwardStatus {
    Starting,
    Active,
    /// Established by the transport itself (`ssh -L`) rather than by a tunnel
    /// Zed opened, so whether the local bind actually succeeded is not
    /// observable from here.
    External,
    Failed(SharedString),
    Stopped,
}

impl PortForwardStatus {
    pub fn label(&self) -> SharedString {
        match self {
            PortForwardStatus::Starting => "Starting".into(),
            PortForwardStatus::Active => "Active".into(),
            PortForwardStatus::External => "External".into(),
            PortForwardStatus::Failed(_) => "Failed".into(),
            PortForwardStatus::Stopped => "Stopped".into(),
        }
    }

    pub fn error(&self) -> Option<&SharedString> {
        match self {
            PortForwardStatus::Failed(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Default)]
struct CreditWindowState {
    in_flight: u64,
    peak_in_flight: u64,
    bytes_sent: u64,
    bytes_acknowledged: u64,
    closed: bool,
    waiters: Vec<oneshot::Sender<()>>,
}

/// The sender half of one direction of a tunnel's flow control.
struct CreditWindow {
    state: Mutex<CreditWindowState>,
    /// Shared with the store so that the bound stays observable once the
    /// tunnel itself has been torn down.
    store_peak_in_flight: Arc<AtomicU64>,
}

impl CreditWindow {
    fn new(store_peak_in_flight: Arc<AtomicU64>) -> Self {
        Self {
            state: Mutex::default(),
            store_peak_in_flight,
        }
    }

    /// Waits until `bytes` fit in the window and takes them. Returns false once
    /// the tunnel is gone, which is the reader loop's signal to stop.
    async fn reserve(&self, bytes: u64) -> bool {
        loop {
            let waiter = {
                let mut state = self.state.lock();
                if state.closed {
                    return false;
                }
                if state.in_flight + bytes <= PORT_TUNNEL_WINDOW_SIZE {
                    state.in_flight += bytes;
                    state.bytes_sent += bytes;
                    state.peak_in_flight = state.peak_in_flight.max(state.in_flight);
                    self.store_peak_in_flight
                        .fetch_max(state.in_flight, Ordering::Relaxed);
                    return true;
                }
                let (sender, receiver) = oneshot::channel();
                state.waiters.push(sender);
                receiver
            };
            if waiter.await.is_err() {
                return false;
            }
        }
    }

    /// Gives back window space: either the tail of a reservation that the
    /// socket read did not fill, or bytes the peer has acknowledged.
    fn release(&self, bytes: u64, acknowledged: bool) {
        let waiters = {
            let mut state = self.state.lock();
            state.in_flight = state.in_flight.saturating_sub(bytes);
            if acknowledged {
                state.bytes_acknowledged += bytes;
            } else {
                state.bytes_sent = state.bytes_sent.saturating_sub(bytes);
            }
            std::mem::take(&mut state.waiters)
        };
        for waiter in waiters {
            waiter.send(()).ok();
        }
    }

    fn close(&self) {
        let waiters = {
            let mut state = self.state.lock();
            state.closed = true;
            std::mem::take(&mut state.waiters)
        };
        for waiter in waiters {
            waiter.send(()).ok();
        }
    }

    fn snapshot(&self) -> TunnelFlowControl {
        let state = self.state.lock();
        TunnelFlowControl {
            in_flight: state.in_flight,
            peak_in_flight: state.peak_in_flight,
            bytes_sent: state.bytes_sent,
            bytes_acknowledged: state.bytes_acknowledged,
        }
    }
}

/// A read-only view of one tunnel's outbound flow control, for the UI and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TunnelFlowControl {
    pub in_flight: u64,
    pub peak_in_flight: u64,
    pub bytes_sent: u64,
    pub bytes_acknowledged: u64,
}

enum TunnelWrite {
    Data(Vec<u8>),
    /// The peer's socket reached EOF, so ours is half-closed once the writes
    /// queued ahead of this have landed.
    Eof,
}

struct Tunnel {
    /// Which configured forward accepted this connection, on the local side.
    forward: Option<SshPortForwardOption>,
    credit: Arc<CreditWindow>,
    writes: mpsc::UnboundedSender<TunnelWrite>,
    /// Our socket reached EOF and we told the peer.
    local_read_done: bool,
    /// Everything the peer sent has been written and our socket is half-closed.
    write_done: bool,
    _open_task: Option<Task<()>>,
    _read_task: Option<Task<()>>,
    _write_task: Task<()>,
}

struct ForwardState {
    forward: SshPortForwardOption,
    status: PortForwardStatus,
    _listener_task: Task<()>,
}

/// Both ends of the tunnel protocol. On the remote side it answers
/// `OpenPortTunnel` by connecting to the target; on the local side it binds the
/// configured listeners and opens a tunnel per accepted connection. Everything
/// after the socket exists is symmetric and shared.
pub struct PortForwardStore {
    project_id: u64,
    client: AnyProtoClient,
    next_tunnel_id: u64,
    tunnels: HashMap<u64, Tunnel>,
    forwards: Vec<ForwardState>,
    peak_in_flight: Arc<AtomicU64>,
}

impl PortForwardStore {
    pub fn new(project_id: u64, client: AnyProtoClient) -> Self {
        Self {
            project_id,
            client,
            next_tunnel_id: 0,
            tunnels: HashMap::default(),
            forwards: Vec::new(),
            peak_in_flight: Arc::default(),
        }
    }

    /// Registers the handlers the remote server needs, which is everything the
    /// local side handles plus the request that opens a tunnel.
    pub fn init_remote(session: &AnyProtoClient) {
        session.add_entity_request_handler(Self::handle_open_port_tunnel);
        Self::init_local(session);
    }

    pub fn init_local(client: &AnyProtoClient) {
        client.add_entity_message_handler(Self::handle_port_tunnel_data);
        client.add_entity_message_handler(Self::handle_port_tunnel_ack);
        client.add_entity_message_handler(Self::handle_close_port_tunnel);
    }

    pub fn tunnel_count(&self) -> usize {
        self.tunnels.len()
    }

    pub fn flow_control(&self, tunnel_id: u64) -> Option<TunnelFlowControl> {
        Some(self.tunnels.get(&tunnel_id)?.credit.snapshot())
    }

    /// The most bytes any one tunnel of this store has ever had outstanding.
    pub fn peak_in_flight_bytes(&self) -> u64 {
        self.peak_in_flight.load(Ordering::Relaxed)
    }

    pub fn statuses(&self) -> impl Iterator<Item = (&SshPortForwardOption, &PortForwardStatus)> {
        self.forwards
            .iter()
            .map(|state| (&state.forward, &state.status))
    }

    pub fn status(&self, forward: &SshPortForwardOption) -> Option<&PortForwardStatus> {
        self.forwards
            .iter()
            .find(|state| &state.forward == forward)
            .map(|state| &state.status)
    }

    /// Brings the running listeners in line with `forwards`: entries that are
    /// gone release their listener and tear down their tunnels, entries that
    /// are new get a listener, and entries that are unchanged keep their state.
    pub fn set_forwards(&mut self, forwards: Vec<SshPortForwardOption>, cx: &mut Context<Self>) {
        let removed: Vec<SshPortForwardOption> = self
            .forwards
            .iter()
            .filter(|state| !forwards.contains(&state.forward))
            .map(|state| state.forward.clone())
            .collect();
        self.forwards
            .retain(|state| forwards.contains(&state.forward));

        for forward in removed {
            let stale: Vec<u64> = self
                .tunnels
                .iter()
                .filter(|(_, tunnel)| tunnel.forward.as_ref() == Some(&forward))
                .map(|(tunnel_id, _)| *tunnel_id)
                .collect();
            for tunnel_id in stale {
                self.close_tunnel(tunnel_id, Some("port forward was removed"));
            }
        }

        for forward in forwards {
            if self.forwards.iter().any(|state| state.forward == forward) {
                continue;
            }
            let listener_task = self.start_listener(forward.clone(), cx);
            self.forwards.push(ForwardState {
                forward,
                status: PortForwardStatus::Starting,
                _listener_task: listener_task,
            });
        }

        cx.notify();
    }

    /// Records a forward as running elsewhere - `ssh -L` already established the
    /// forwards that were configured when the connection was made, so binding
    /// them again here would only collide with the ssh process.
    pub fn set_externally_forwarded(
        &mut self,
        forwards: Vec<SshPortForwardOption>,
        cx: &mut Context<Self>,
    ) {
        for forward in forwards {
            if self.forwards.iter().any(|state| state.forward == forward) {
                continue;
            }
            self.forwards.push(ForwardState {
                forward,
                status: PortForwardStatus::External,
                _listener_task: Task::ready(()),
            });
        }
        cx.notify();
    }

    fn set_status(
        &mut self,
        forward: &SshPortForwardOption,
        status: PortForwardStatus,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = self
            .forwards
            .iter_mut()
            .find(|state| &state.forward == forward)
            && state.status != status
        {
            state.status = status;
            cx.notify();
        }
    }

    fn start_listener(
        &mut self,
        forward: SshPortForwardOption,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let host = forward
                .local_host
                .clone()
                .unwrap_or_else(|| DEFAULT_LOCAL_HOST.to_string());
            let listener = match TcpListener::bind((host.as_str(), forward.local_port)).await {
                Ok(listener) => listener,
                Err(error) => {
                    let message =
                        format!("could not listen on {host}:{}: {error}", forward.local_port);
                    log::error!("port forward: {message}");
                    this.update(cx, |this, cx| {
                        this.set_status(&forward, PortForwardStatus::Failed(message.into()), cx);
                    })
                    .ok();
                    return;
                }
            };

            if this
                .update(cx, |this, cx| {
                    this.set_status(&forward, PortForwardStatus::Active, cx);
                })
                .is_err()
            {
                return;
            }

            let mut incoming = listener.incoming();
            while let Some(stream) = incoming.next().await {
                match stream {
                    Ok(stream) => {
                        if this
                            .update(cx, |this, cx| this.begin_tunnel(&forward, stream, cx))
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) => {
                        log::error!(
                            "port forward: failed to accept on {host}:{}: {error}",
                            forward.local_port
                        );
                    }
                }
            }
        })
    }

    /// Registers the tunnel before asking the remote side to open it, so that
    /// data the remote side sends the moment it connects has somewhere to go.
    fn begin_tunnel(
        &mut self,
        forward: &SshPortForwardOption,
        stream: TcpStream,
        cx: &mut Context<Self>,
    ) {
        let tunnel_id = self.next_tunnel_id;
        self.next_tunnel_id += 1;
        self.open_tunnel(tunnel_id, stream.clone(), Some(forward.clone()), cx);

        let request = self.client.request(proto::OpenPortTunnel {
            project_id: self.project_id,
            tunnel_id,
            remote_host: forward
                .remote_host
                .clone()
                .unwrap_or_else(|| DEFAULT_REMOTE_HOST.to_string()),
            remote_port: forward.remote_port as u32,
        });

        let forward = forward.clone();
        let open_task = cx.spawn(async move |this, cx| {
            let result = request.await;
            this.update(cx, |this, cx| match result {
                Ok(_) => {
                    this.start_reading(tunnel_id, stream, cx);
                    this.set_status(&forward, PortForwardStatus::Active, cx);
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    log::error!("port forward: could not open tunnel: {message}");
                    this.remove_tunnel(tunnel_id);
                    this.set_status(&forward, PortForwardStatus::Failed(message.into()), cx);
                }
            })
            .ok();
        });

        if let Some(tunnel) = self.tunnels.get_mut(&tunnel_id) {
            tunnel._open_task = Some(open_task);
        }
    }

    /// Starts the half of the tunnel that writes into the socket. The half that
    /// reads from it is started separately, because the local side must not
    /// read before the remote side has confirmed the connection.
    fn open_tunnel(
        &mut self,
        tunnel_id: u64,
        stream: TcpStream,
        forward: Option<SshPortForwardOption>,
        cx: &mut Context<Self>,
    ) {
        let (writes, mut pending_writes) = mpsc::unbounded::<TunnelWrite>();
        let client = self.client.clone();
        let project_id = self.project_id;

        let write_task = cx.spawn(async move |this, cx| {
            let mut socket = stream;
            let mut failure = None;
            while let Some(write) = pending_writes.next().await {
                match write {
                    TunnelWrite::Data(data) => {
                        let bytes_consumed = data.len() as u64;
                        if let Err(error) = socket.write_all(&data).await {
                            failure = Some(format!("write failed: {error}"));
                            break;
                        }
                        client
                            .send(proto::PortTunnelAck {
                                project_id,
                                tunnel_id,
                                bytes_consumed,
                            })
                            .log_err();
                    }
                    TunnelWrite::Eof => {
                        socket.close().await.log_err();
                        break;
                    }
                }
            }
            this.update(cx, |this, _| match failure {
                Some(message) => {
                    log::error!("port forward: tunnel {tunnel_id} {message}");
                    this.close_tunnel(tunnel_id, Some(&message));
                }
                None => this.mark_write_done(tunnel_id),
            })
            .ok();
        });

        self.tunnels.insert(
            tunnel_id,
            Tunnel {
                forward,
                credit: Arc::new(CreditWindow::new(self.peak_in_flight.clone())),
                writes,
                local_read_done: false,
                write_done: false,
                _open_task: None,
                _read_task: None,
                _write_task: write_task,
            },
        );
    }

    fn start_reading(&mut self, tunnel_id: u64, stream: TcpStream, cx: &mut Context<Self>) {
        let Some(credit) = self
            .tunnels
            .get(&tunnel_id)
            .map(|tunnel| tunnel.credit.clone())
        else {
            return;
        };
        let client = self.client.clone();
        let project_id = self.project_id;

        let read_task = cx.spawn(async move |this, cx| {
            let mut socket = stream;
            let mut buffer = vec![0u8; PORT_TUNNEL_CHUNK_SIZE];
            let chunk = PORT_TUNNEL_CHUNK_SIZE as u64;
            loop {
                // Taking the credit before the read is what stops us draining
                // the socket into an unbounded queue: with no credit left the
                // socket's receive buffer fills and TCP stalls the producer.
                if !credit.reserve(chunk).await {
                    return;
                }
                match socket.read(&mut buffer).await {
                    Ok(0) => {
                        credit.release(chunk, false);
                        client
                            .send(proto::ClosePortTunnel {
                                project_id,
                                tunnel_id,
                                error: String::new(),
                            })
                            .log_err();
                        this.update(cx, |this, _| this.mark_local_read_done(tunnel_id))
                            .ok();
                        return;
                    }
                    Ok(count) => {
                        credit.release(chunk - count as u64, false);
                        let Some(data) = buffer.get(..count) else {
                            return;
                        };
                        if let Err(error) = client.send(proto::PortTunnelData {
                            project_id,
                            tunnel_id,
                            data: data.to_vec(),
                        }) {
                            let message = format!("could not forward data: {error:#}");
                            log::error!("port forward: tunnel {tunnel_id} {message}");
                            this.update(cx, |this, _| this.close_tunnel(tunnel_id, Some(&message)))
                                .ok();
                            return;
                        }
                    }
                    Err(error) => {
                        credit.release(chunk, false);
                        let message = format!("read failed: {error}");
                        log::error!("port forward: tunnel {tunnel_id} {message}");
                        this.update(cx, |this, _| this.close_tunnel(tunnel_id, Some(&message)))
                            .ok();
                        return;
                    }
                }
            }
        });

        if let Some(tunnel) = self.tunnels.get_mut(&tunnel_id) {
            tunnel._read_task = Some(read_task);
        }
    }

    fn mark_local_read_done(&mut self, tunnel_id: u64) {
        let Some(tunnel) = self.tunnels.get_mut(&tunnel_id) else {
            return;
        };
        tunnel.local_read_done = true;
        if tunnel.write_done {
            self.remove_tunnel(tunnel_id);
        }
    }

    fn mark_write_done(&mut self, tunnel_id: u64) {
        let Some(tunnel) = self.tunnels.get_mut(&tunnel_id) else {
            return;
        };
        tunnel.write_done = true;
        if tunnel.local_read_done {
            self.remove_tunnel(tunnel_id);
        }
    }

    /// Tears the tunnel down locally and tells the peer why.
    fn close_tunnel(&mut self, tunnel_id: u64, error: Option<&str>) {
        if self.tunnels.contains_key(&tunnel_id) {
            self.client
                .send(proto::ClosePortTunnel {
                    project_id: self.project_id,
                    tunnel_id,
                    error: error.unwrap_or_default().to_string(),
                })
                .log_err();
        }
        self.remove_tunnel(tunnel_id);
    }

    fn remove_tunnel(&mut self, tunnel_id: u64) {
        if let Some(tunnel) = self.tunnels.remove(&tunnel_id) {
            tunnel.credit.close();
        }
    }

    async fn handle_open_port_tunnel(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::OpenPortTunnel>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let payload = envelope.payload;
        let tunnel_id = payload.tunnel_id;
        let host = if payload.remote_host.is_empty() {
            DEFAULT_REMOTE_HOST.to_string()
        } else {
            payload.remote_host
        };
        let port = u16::try_from(payload.remote_port)
            .with_context(|| format!("invalid remote port {}", payload.remote_port))?;

        let stream = TcpStream::connect((host.as_str(), port))
            .await
            .with_context(|| format!("could not connect to {host}:{port}"))?;

        this.update(&mut cx, |this, cx| {
            this.open_tunnel(tunnel_id, stream.clone(), None, cx);
            this.start_reading(tunnel_id, stream, cx);
        });
        Ok(proto::Ack {})
    }

    async fn handle_port_tunnel_data(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::PortTunnelData>,
        mut cx: AsyncApp,
    ) -> Result<()> {
        // Queued synchronously, before this handler yields, so that chunks stay
        // in the order the channel delivered them.
        this.update(&mut cx, |this, _| {
            let payload = envelope.payload;
            let Some(tunnel) = this.tunnels.get(&payload.tunnel_id) else {
                return;
            };
            if tunnel
                .writes
                .unbounded_send(TunnelWrite::Data(payload.data))
                .is_err()
            {
                this.remove_tunnel(payload.tunnel_id);
            }
        });
        Ok(())
    }

    async fn handle_port_tunnel_ack(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::PortTunnelAck>,
        mut cx: AsyncApp,
    ) -> Result<()> {
        this.update(&mut cx, |this, _| {
            if let Some(tunnel) = this.tunnels.get(&envelope.payload.tunnel_id) {
                tunnel.credit.release(envelope.payload.bytes_consumed, true);
            }
        });
        Ok(())
    }

    async fn handle_close_port_tunnel(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::ClosePortTunnel>,
        mut cx: AsyncApp,
    ) -> Result<()> {
        this.update(&mut cx, |this, _| {
            let payload = envelope.payload;
            if !payload.error.is_empty() {
                log::error!(
                    "port forward: tunnel {} closed by peer: {}",
                    payload.tunnel_id,
                    payload.error
                );
                this.remove_tunnel(payload.tunnel_id);
                return;
            }

            // The writer drains whatever is still queued before half-closing,
            // so the tunnel outlives this message until those bytes have landed.
            let Some(tunnel) = this.tunnels.get(&payload.tunnel_id) else {
                return;
            };
            if tunnel.writes.unbounded_send(TunnelWrite::Eof).is_err() {
                this.mark_write_done(payload.tunnel_id);
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RemoteClient;
    use gpui::{AppContext as _, TestAppContext};
    use rpc::proto::REMOTE_SERVER_PROJECT_ID;
    use std::time::Duration;

    /// Every wait in these tests is bounded, so a regression stalls the
    /// assertion rather than hanging the suite.
    const WAIT_TIMEOUT: Duration = Duration::from_secs(10);
    const POLL_INTERVAL: Duration = Duration::from_millis(10);

    struct TunnelTest {
        client_store: Entity<PortForwardStore>,
        server_store: Entity<PortForwardStore>,
        /// The client owns the proxy task that moves envelopes between the two
        /// ends, so it has to outlive the test body.
        _remote_client: Entity<RemoteClient>,
    }

    async fn setup(cx: &mut TestAppContext, server_cx: &mut TestAppContext) -> TunnelTest {
        cx.executor().allow_parking();
        cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
        server_cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));

        let (options, server_session, connect_guard) = RemoteClient::fake_server(cx, server_cx);
        let server_store = server_cx
            .new(|_| PortForwardStore::new(REMOTE_SERVER_PROJECT_ID, server_session.clone()));
        server_session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &server_store);
        PortForwardStore::init_remote(&server_session);
        // The client heartbeats as soon as it connects, and only the headless
        // project registers a handler for that in the real server.
        server_session.add_request_handler(
            server_store.downgrade(),
            |_: Entity<PortForwardStore>, _: TypedEnvelope<proto::Ping>, _: AsyncApp| async {
                Ok(proto::Ack {})
            },
        );

        drop(connect_guard);
        let remote_client = RemoteClient::connect_mock(options, cx).await;
        let client_session = remote_client.read_with(cx, |client, _| client.proto_client());
        let client_store =
            cx.new(|_| PortForwardStore::new(REMOTE_SERVER_PROJECT_ID, client_session.clone()));
        client_session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &client_store);
        PortForwardStore::init_local(&client_session);

        TunnelTest {
            client_store,
            server_store,
            _remote_client: remote_client,
        }
    }

    async fn wait_for(
        cx: &mut TestAppContext,
        description: &str,
        mut condition: impl FnMut(&mut TestAppContext) -> bool,
    ) {
        let attempts = WAIT_TIMEOUT.as_millis() / POLL_INTERVAL.as_millis();
        for _ in 0..attempts {
            if condition(cx) {
                return;
            }
            cx.background_executor.timer(POLL_INTERVAL).await;
        }
        panic!("timed out after {WAIT_TIMEOUT:?} waiting for {description}");
    }

    async fn free_local_port() -> u16 {
        let probe = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to probe for a free port");
        let port = probe
            .local_addr()
            .expect("probe listener has no address")
            .port();
        drop(probe);
        port
    }

    fn forward(local_port: u16, remote_port: u16) -> SshPortForwardOption {
        SshPortForwardOption {
            local_host: Some("127.0.0.1".to_string()),
            local_port,
            remote_host: Some("127.0.0.1".to_string()),
            remote_port,
        }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|index| (index % 251) as u8).collect()
    }

    /// Ephemeral ports are handed back to the pool when the probe listener is
    /// dropped, so another test in the same run can take one before we bind it.
    /// Retrying on a bind failure closes that race: if our bind succeeds the
    /// port is ours.
    async fn start_forward_to(
        test: &TunnelTest,
        remote_port: u16,
        cx: &mut TestAppContext,
    ) -> SshPortForwardOption {
        for _ in 0..8 {
            let candidate = forward(free_local_port().await, remote_port);
            test.client_store.update(cx, |store, cx| {
                store.set_forwards(vec![candidate.clone()], cx)
            });

            let settled = candidate.clone();
            wait_for(cx, "the local listener to settle", |cx| {
                test.client_store.read_with(cx, |store, _| {
                    !matches!(
                        store.status(&settled),
                        None | Some(PortForwardStatus::Starting)
                    )
                })
            })
            .await;

            let status = test
                .client_store
                .read_with(cx, |store, _| store.status(&candidate).cloned());
            if status == Some(PortForwardStatus::Active) {
                return candidate;
            }
        }
        panic!("could not find a free local port to forward");
    }

    #[gpui::test]
    async fn test_tunnel_moves_data_byte_for_byte(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        let test = setup(cx, server_cx).await;

        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_port = target.local_addr().unwrap().port();
        let _echo = cx.background_executor.spawn(async move {
            let (mut socket, _) = target.accept().await.unwrap();
            let mut received = Vec::new();
            socket.read_to_end(&mut received).await.unwrap();
            socket.write_all(&received).await.unwrap();
            socket.close().await.unwrap();
        });

        let forward = start_forward_to(&test, target_port, cx).await;

        // Larger than one chunk and larger than one window, so the transfer
        // exercises chunking and credit return rather than a single message.
        let payload = pattern(384 * 1024);
        let mut socket = TcpStream::connect(("127.0.0.1", forward.local_port))
            .await
            .unwrap();
        socket.write_all(&payload).await.unwrap();
        socket.close().await.unwrap();

        let mut echoed = Vec::new();
        socket.read_to_end(&mut echoed).await.unwrap();

        assert_eq!(
            echoed.len(),
            payload.len(),
            "the tunnel delivered {} bytes instead of {}",
            echoed.len(),
            payload.len()
        );
        assert!(
            echoed == payload,
            "the round tripped bytes differ from what was sent at index {:?}",
            echoed
                .iter()
                .zip(payload.iter())
                .position(|(left, right)| left != right)
        );
    }

    #[gpui::test]
    async fn test_credit_window_bounds_in_flight_bytes(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        let test = setup(cx, server_cx).await;

        // A target that accepts but never reads: its receive buffer fills, the
        // remote end stops acknowledging, and the local end must stop reading.
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_port = target.local_addr().unwrap().port();
        let _stalled = cx.background_executor.spawn(async move {
            let (socket, _) = target.accept().await.unwrap();
            std::future::pending::<()>().await;
            drop(socket);
        });

        let forward = start_forward_to(&test, target_port, cx).await;

        const SOURCE_BYTES: usize = 16 * 1024 * 1024;
        let mut socket = TcpStream::connect(("127.0.0.1", forward.local_port))
            .await
            .unwrap();
        let _source = cx.background_executor.spawn(async move {
            socket.write_all(&pattern(SOURCE_BYTES)).await.ok();
        });

        let mut last_seen = None;
        wait_for(cx, "the tunnel to fill its window", |cx| {
            last_seen = test
                .client_store
                .read_with(cx, |store, _| store.flow_control(0));
            last_seen.is_some_and(|flow| {
                flow.in_flight >= PORT_TUNNEL_WINDOW_SIZE - PORT_TUNNEL_CHUNK_SIZE as u64
            })
        })
        .await;
        assert!(
            last_seen.is_some(),
            "the tunnel was never registered, so nothing was measured"
        );

        // Let the sender run on: if the window were not enforced it would keep
        // shipping the remaining source bytes into the unbounded channel.
        let mut samples = Vec::new();
        for _ in 0..5 {
            cx.background_executor
                .timer(Duration::from_millis(100))
                .await;
            let flow = test
                .client_store
                .read_with(cx, |store, _| store.flow_control(0))
                .expect("the tunnel is still open");
            samples.push(flow);
        }

        for flow in &samples {
            assert!(
                flow.in_flight <= PORT_TUNNEL_WINDOW_SIZE,
                "in flight bytes {} exceeded the {} byte window",
                flow.in_flight,
                PORT_TUNNEL_WINDOW_SIZE
            );
            assert_eq!(
                flow.bytes_sent - flow.bytes_acknowledged,
                flow.in_flight,
                "unacknowledged bytes and in flight bytes disagree"
            );
            assert!(
                flow.peak_in_flight <= PORT_TUNNEL_WINDOW_SIZE,
                "peak in flight bytes {} exceeded the {} byte window",
                flow.peak_in_flight,
                PORT_TUNNEL_WINDOW_SIZE
            );
        }

        let last = samples.last().copied().expect("sampled the flow control");
        assert!(
            last.peak_in_flight >= PORT_TUNNEL_WINDOW_SIZE - PORT_TUNNEL_CHUNK_SIZE as u64,
            "the window was never saturated (peak {}), so the bound above proves nothing",
            last.peak_in_flight
        );
        assert!(
            last.bytes_sent < 4 * 1024 * 1024,
            "the sender pushed {} bytes of a {} byte source into a stalled tunnel",
            last.bytes_sent,
            SOURCE_BYTES
        );
    }

    #[gpui::test]
    async fn test_large_transfer_stays_within_the_window(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        let test = setup(cx, server_cx).await;

        const TRANSFER_BYTES: usize = 12 * 1024 * 1024;
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_port = target.local_addr().unwrap().port();
        let (received_tx, mut received_rx) = mpsc::unbounded::<usize>();
        let _sink = cx.background_executor.spawn(async move {
            let (mut socket, _) = target.accept().await.unwrap();
            let mut buffer = vec![0u8; 64 * 1024];
            let mut total = 0usize;
            let mut expected_next = 0usize;
            loop {
                match socket.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(count) => {
                        for byte in &buffer[..count] {
                            assert_eq!(
                                *byte,
                                (expected_next % 251) as u8,
                                "byte {expected_next} arrived corrupted"
                            );
                            expected_next += 1;
                        }
                        total += count;
                    }
                    Err(error) => panic!("target read failed: {error}"),
                }
            }
            socket.close().await.ok();
            received_tx.unbounded_send(total).ok();
        });

        let forward = start_forward_to(&test, target_port, cx).await;

        let mut socket = TcpStream::connect(("127.0.0.1", forward.local_port))
            .await
            .unwrap();
        let _source = cx.background_executor.spawn(async move {
            socket
                .write_all(&pattern(TRANSFER_BYTES))
                .await
                .expect("failed to write the source bytes");
            socket
                .close()
                .await
                .expect("failed to half close the source");
        });

        wait_for(cx, "the whole transfer to arrive", |_| {
            match received_rx.try_recv() {
                Ok(total) => {
                    assert_eq!(total, TRANSFER_BYTES, "the target received {total} bytes");
                    true
                }
                _ => false,
            }
        })
        .await;

        // Recorded by the store rather than by sampling, so it covers the whole
        // transfer and survives the tunnel being torn down.
        let peak = test
            .client_store
            .read_with(cx, |store, _| store.peak_in_flight_bytes());
        assert!(
            peak > 0 && peak <= PORT_TUNNEL_WINDOW_SIZE,
            "a {TRANSFER_BYTES} byte transfer buffered {peak} bytes at once, against a {PORT_TUNNEL_WINDOW_SIZE} byte window"
        );
    }

    #[gpui::test]
    async fn test_remote_connect_failure_reaches_the_local_side(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        let test = setup(cx, server_cx).await;

        let dead_remote_port = free_local_port().await;
        let forward = start_forward_to(&test, dead_remote_port, cx).await;

        let _socket = TcpStream::connect(("127.0.0.1", forward.local_port))
            .await
            .unwrap();

        let expected = forward.clone();
        wait_for(cx, "the remote connect error to surface", |cx| {
            test.client_store.read_with(cx, |store, _| {
                matches!(store.status(&expected), Some(PortForwardStatus::Failed(_)))
            })
        })
        .await;

        let status = test
            .client_store
            .read_with(cx, |store, _| store.status(&forward).cloned())
            .expect("the forward is still configured");
        let error = status.error().expect("a failed forward carries a reason");
        assert!(
            error.contains(&format!("127.0.0.1:{dead_remote_port}")),
            "the error should name the unreachable target, got {error:?}"
        );
        assert_eq!(
            test.server_store
                .read_with(server_cx, |store, _| store.tunnel_count()),
            0,
            "a tunnel that never connected must not be left registered on the remote side"
        );
    }

    #[gpui::test]
    async fn test_local_bind_failure_reports_the_reason(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        let test = setup(cx, server_cx).await;

        let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();
        let forward = forward(occupied_port, 1);

        test.client_store.update(cx, |store, cx| {
            store.set_forwards(vec![forward.clone()], cx)
        });

        let expected = forward.clone();
        wait_for(cx, "the bind failure to surface", |cx| {
            test.client_store.read_with(cx, |store, _| {
                matches!(store.status(&expected), Some(PortForwardStatus::Failed(_)))
            })
        })
        .await;

        let status = test
            .client_store
            .read_with(cx, |store, _| store.status(&forward).cloned())
            .expect("the forward is still configured");
        let error = status.error().expect("a failed forward carries a reason");
        assert!(
            error.contains(&format!("127.0.0.1:{occupied_port}")),
            "the error should name the address it could not bind, got {error:?}"
        );
        drop(occupied);
    }

    #[gpui::test]
    async fn test_closing_a_tunnel_cleans_up_both_ends(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        let test = setup(cx, server_cx).await;

        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_port = target.local_addr().unwrap().port();
        let _echo = cx.background_executor.spawn(async move {
            let (mut socket, _) = target.accept().await.unwrap();
            let mut buffer = vec![0u8; 1024];
            while let Ok(count) = socket.read(&mut buffer).await {
                if count == 0 {
                    break;
                }
                if socket.write_all(&buffer[..count]).await.is_err() {
                    break;
                }
            }
            socket.close().await.ok();
        });

        let forward = start_forward_to(&test, target_port, cx).await;

        let mut socket = TcpStream::connect(("127.0.0.1", forward.local_port))
            .await
            .unwrap();
        socket.write_all(b"hello").await.unwrap();

        wait_for(cx, "both ends to register the tunnel", |cx| {
            test.client_store
                .read_with(cx, |store, _| store.tunnel_count())
                == 1
        })
        .await;
        wait_for(cx, "the remote end to register the tunnel", |_| {
            test.server_store
                .read_with(server_cx, |store, _| store.tunnel_count())
                == 1
        })
        .await;

        drop(socket);

        wait_for(cx, "the local end to drop the tunnel", |cx| {
            test.client_store
                .read_with(cx, |store, _| store.tunnel_count())
                == 0
        })
        .await;
        wait_for(cx, "the remote end to drop the tunnel", |_| {
            test.server_store
                .read_with(server_cx, |store, _| store.tunnel_count())
                == 0
        })
        .await;
    }

    #[gpui::test]
    async fn test_externally_forwarded_ports_are_not_reported_as_active(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        let test = setup(cx, server_cx).await;
        let external = forward(free_local_port().await, 80);

        test.client_store.update(cx, |store, cx| {
            store.set_externally_forwarded(vec![external.clone()], cx);
        });

        let label = test.client_store.read_with(cx, |store, _| {
            store
                .status(&external)
                .map(|status| status.label().to_string())
        });
        assert_eq!(
            label.as_deref(),
            Some("External"),
            "ssh -L may have failed to bind the local port without Zed knowing, \
             so an externally established forward must not be reported with the \
             same label as a tunnel Zed bound and verified itself"
        );

        let error = test.client_store.read_with(cx, |store, _| {
            store
                .status(&external)
                .and_then(|status| status.error())
                .cloned()
        });
        assert_eq!(
            error, None,
            "an externally established forward has no failure of its own to report"
        );
    }
}
