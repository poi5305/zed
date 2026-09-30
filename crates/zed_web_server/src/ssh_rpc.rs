use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{
        Arc, Mutex as StdMutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail};
use askpass::EncryptedPassword;
use axum::extract::ws::Message;
use futures::{
    FutureExt as _, StreamExt as _,
    channel::{mpsc, oneshot},
};
use gpui::{AsyncApp, Task};
use rand::RngCore as _;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc as tokio_mpsc;

use crate::ssh_host::{ConnectedHost, HandleId, HostCommand, HostId, SshHostHandle};

/// How long a handle outlives the `/rpc` connection that owns it, waiting for the browser to
/// reconnect and `RemoteSsh::attach_handles` it.
pub const HANDLE_REAP_DELAY: Duration = Duration::from_secs(30);
/// How long a `RemoteSsh::open_channel` token waits for its `/remote/channel` WebSocket.
pub const CHANNEL_TOKEN_LIFETIME: Duration = Duration::from_secs(30);

pub const DISABLED_REASON: &str = "ssh is disabled on this server by ZED_WEB_ALLOW_SSH or ssh.enabled in .zed/web.json; remove that setting or start zed-web-server with --allow-ssh";

pub fn handles(method: &str) -> bool {
    method.starts_with("RemoteSsh::")
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SshConfig {
    /// Why ssh is refused although the server was started with it, naming what to fix.
    pub disabled_reason: Option<String>,
    /// `.zed/web.json` `ssh.allowed_hosts`; `None` allows every host.
    pub allowed_hosts: Option<Vec<String>>,
    /// Whether the server process has an ssh-agent (`SSH_AUTH_SOCK`) for key auth.
    pub ssh_agent: bool,
}

impl SshConfig {
    /// Reads the `ssh` block of `.zed/web.json`. A malformed block turns ssh off rather than
    /// guessing which hosts were meant.
    pub fn from_web_json(config: &Value, ssh_agent: bool) -> Self {
        let mut ssh_config = Self {
            ssh_agent,
            ..Self::default()
        };
        let Some(ssh) = config.get("ssh") else {
            return ssh_config;
        };
        // `--allow-ssh` and the env var never read the file, so this is the only place a
        // malformed block can still fail closed.
        let Some(allowed_hosts) = ssh.as_object().map(|ssh| ssh.get("allowed_hosts")) else {
            ssh_config.disabled_reason =
                Some("ssh is disabled: .zed/web.json ssh must be an object".to_string());
            return ssh_config;
        };
        let Some(allowed_hosts) = allowed_hosts else {
            return ssh_config;
        };
        let hosts = allowed_hosts.as_array().and_then(|hosts| {
            hosts
                .iter()
                .map(|host| host.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
        });
        match hosts {
            Some(hosts) => ssh_config.allowed_hosts = Some(hosts),
            None => {
                ssh_config.disabled_reason = Some(
                    "ssh is disabled: .zed/web.json ssh.allowed_hosts must be a list of host names"
                        .to_string(),
                )
            }
        }
        ssh_config
    }
}

/// Everything the gpui side's connect reports back to the tokio side while it runs.
pub enum DelegateEvent {
    Status(Option<String>),
    Prompt {
        prompt: String,
        reply: oneshot::Sender<EncryptedPassword>,
        cancellation: oneshot::Receiver<()>,
    },
}

/// The `RemoteClientDelegate` for connects the browser asked for. It runs on the gpui thread,
/// so it only forwards: the tokio side owns the `/rpc` connection the prompt must reach.
pub struct WebSshDelegate {
    events: Option<mpsc::UnboundedSender<DelegateEvent>>,
}

impl WebSshDelegate {
    pub fn new(events: mpsc::UnboundedSender<DelegateEvent>) -> Self {
        Self {
            events: Some(events),
        }
    }

    /// For work that happens after the connect that owned the prompts has returned.
    pub fn detached() -> Self {
        Self { events: None }
    }

    /// Dropping `reply` unanswered is how askpass learns the prompt was refused.
    pub fn request_password(
        &self,
        prompt: String,
        reply: oneshot::Sender<EncryptedPassword>,
        cancellation: oneshot::Receiver<()>,
    ) {
        let Some(events) = &self.events else {
            tracing::warn!(%prompt, "ssh asked for input after its connect finished; refusing");
            return;
        };
        if events
            .unbounded_send(DelegateEvent::Prompt {
                prompt,
                reply,
                cancellation,
            })
            .is_err()
        {
            tracing::debug!("ssh asked for input after the browser stopped waiting");
        }
    }

    pub fn report_status(&self, status: Option<&str>) {
        let Some(events) = &self.events else {
            return;
        };
        if events
            .unbounded_send(DelegateEvent::Status(status.map(str::to_owned)))
            .is_err()
        {
            tracing::debug!(?status, "ssh status after the browser stopped waiting");
        }
    }
}

impl remote::RemoteClientDelegate for WebSshDelegate {
    fn ask_password(
        &self,
        prompt: String,
        tx: oneshot::Sender<EncryptedPassword>,
        cancellation: oneshot::Receiver<()>,
        _cx: &mut AsyncApp,
    ) {
        self.request_password(prompt, tx, cancellation);
    }

    fn get_download_url(
        &self,
        _platform: remote::RemotePlatform,
        _release_channel: release_channel::ReleaseChannel,
        _version: Option<semver::Version>,
        _cx: &mut AsyncApp,
    ) -> Task<Result<Option<String>>> {
        Task::ready(Ok(None))
    }

    fn download_server_binary_locally(
        &self,
        platform: remote::RemotePlatform,
        _release_channel: release_channel::ReleaseChannel,
        _version: Option<semver::Version>,
        _cx: &mut AsyncApp,
    ) -> Task<Result<std::path::PathBuf>> {
        Task::ready(Err(anyhow!(
            "no bundled remote_server for {}-{}: remote_server binaries are provisioned by \
             zed-web-server's bundle; see RemoteSsh::capabilities",
            platform.os.as_str(),
            platform.arch.as_str()
        )))
    }

    fn set_status(&self, status: Option<&str>, _cx: &mut AsyncApp) {
        self.report_status(status);
    }
}

/// Server-wide ssh state: the host, the gates, and the handles and channels every `/rpc`
/// connection and `/remote/channel` WebSocket share.
pub struct SshRpc {
    host: Option<SshHostHandle>,
    config: SshConfig,
    next_connection_id: AtomicU64,
    shared: StdMutex<Shared>,
}

#[derive(Default)]
struct Shared {
    handles: HashMap<HandleId, HandleEntry>,
    channels: HashMap<String, ChannelEntry>,
}

struct HandleEntry {
    host_id: HostId,
    owner: u64,
}

struct ChannelEntry {
    token: String,
    host_id: HostId,
    identifier: String,
    reconnect: bool,
    expires_at: tokio::time::Instant,
    attached: bool,
}

/// What a `/remote/channel` WebSocket may start once its token checks out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelClaim {
    pub host_id: HostId,
    pub identifier: String,
    pub reconnect: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelRefusal {
    InvalidToken,
    AlreadyAttached,
}

impl SshRpc {
    /// `host` is `None` when the server runs without ssh (no gpui thread).
    pub fn new(host: Option<SshHostHandle>, config: SshConfig) -> Arc<Self> {
        Arc::new(Self {
            host,
            config,
            next_connection_id: AtomicU64::new(1),
            shared: StdMutex::new(Shared::default()),
        })
    }

    /// One per `/rpc` WebSocket. Call `SshConnection::close` when that socket closes.
    pub fn connection(
        self: &Arc<Self>,
        outgoing: tokio_mpsc::UnboundedSender<Message>,
    ) -> SshConnection {
        SshConnection {
            inner: Arc::new(ConnectionInner {
                rpc: self.clone(),
                id: self.next_connection_id.fetch_add(1, Ordering::Relaxed),
                state: StdMutex::new(ConnectionState {
                    outgoing: Some(outgoing),
                    ..ConnectionState::default()
                }),
            }),
        }
    }

    pub fn enabled_host(&self) -> Result<&SshHostHandle> {
        let Some(host) = &self.host else {
            bail!(DISABLED_REASON);
        };
        if let Some(reason) = &self.config.disabled_reason {
            bail!("{reason}");
        }
        Ok(host)
    }

    pub fn capabilities(&self) -> Value {
        let reason = self.enabled_host().err().map(|error| error.to_string());
        let bundle = self.host.as_ref().and_then(SshHostHandle::bundle);
        json!({
            "enabled": reason.is_none(),
            "reason": reason,
            "commit": bundle.map(|bundle| bundle.commit.clone()),
            "platforms": bundle.map(|bundle| bundle.platforms.clone()).unwrap_or_default(),
            "ssh_agent": self.config.ssh_agent,
        })
    }

    pub fn claim_channel(
        &self,
        channel_id: &str,
        token: &str,
    ) -> Result<ChannelClaim, ChannelRefusal> {
        let mut shared = self.shared();
        let Some(channel) = shared.channels.get_mut(channel_id) else {
            return Err(ChannelRefusal::InvalidToken);
        };
        if !crate::auth::token_matches(token, &channel.token) {
            return Err(ChannelRefusal::InvalidToken);
        }
        if channel.attached {
            return Err(ChannelRefusal::AlreadyAttached);
        }
        if tokio::time::Instant::now() >= channel.expires_at {
            shared.channels.remove(channel_id);
            return Err(ChannelRefusal::InvalidToken);
        }
        channel.attached = true;
        Ok(ChannelClaim {
            host_id: channel.host_id.clone(),
            identifier: channel.identifier.clone(),
            reconnect: channel.reconnect,
        })
    }

    /// Forgets an attached channel once its proxy is gone; its token was already spent.
    pub fn finish_channel(&self, channel_id: &str) {
        self.shared().channels.remove(channel_id);
    }

    fn shared(&self) -> MutexGuard<'_, Shared> {
        self.shared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn release_handle(&self, handle_id: &HandleId) {
        let Some(host) = &self.host else {
            return;
        };
        if let Err(error) = host.send(HostCommand::Release {
            handle_id: handle_id.clone(),
        }) {
            tracing::debug!(?error, "could not release an ssh handle");
        }
    }

    fn reap_handles(&self, owner: u64) {
        let reaped = {
            let mut shared = self.shared();
            let reaped = shared
                .handles
                .iter()
                .filter(|(_, handle)| handle.owner == owner)
                .map(|(handle_id, _)| handle_id.clone())
                .collect::<Vec<_>>();
            for handle_id in &reaped {
                shared.handles.remove(handle_id);
            }
            reaped
        };
        for handle_id in &reaped {
            self.release_handle(handle_id);
        }
        if !reaped.is_empty() {
            tracing::info!(
                count = reaped.len(),
                "released ssh handles nobody reattached"
            );
        }
    }
}

/// The ssh side of one `/rpc` WebSocket: its in-flight connects and their prompts, which
/// must never reach another tab (the session's notifications are broadcast to all of them).
#[derive(Clone)]
pub struct SshConnection {
    inner: Arc<ConnectionInner>,
}

struct ConnectionInner {
    rpc: Arc<SshRpc>,
    id: u64,
    state: StdMutex<ConnectionState>,
}

#[derive(Default)]
struct ConnectionState {
    /// Taken on close: the `/rpc` writer only finishes once every sender is gone, and a prompt
    /// watcher may outlive the socket by a moment.
    outgoing: Option<tokio_mpsc::UnboundedSender<Message>>,
    connects: HashMap<String, oneshot::Sender<()>>,
    prompts: HashMap<String, PendingPrompt>,
    closed: bool,
}

struct PendingPrompt {
    connect_id: String,
    reply: oneshot::Sender<EncryptedPassword>,
}

#[derive(Deserialize)]
struct ConnectParams {
    connect_id: String,
    options: remote::SshConnectionOptions,
    #[serde(default)]
    client_commit: Option<String>,
}

#[derive(Deserialize)]
struct ConnectIdParams {
    connect_id: String,
}

#[derive(Deserialize)]
struct OpenChannelParams {
    handle_id: String,
    identifier: String,
    #[serde(default)]
    reconnect: bool,
}

#[derive(Deserialize)]
struct HandleIdParams {
    handle_id: String,
}

#[derive(Deserialize)]
struct HandleIdsParams {
    handle_ids: Vec<String>,
}

impl SshConnection {
    /// Answers one `RemoteSsh::*` request. `RemoteSsh::connect` resolves only when the connect
    /// does, which can take as long as the user takes to type a password, so callers must not
    /// await it on the loop that reads the socket.
    pub async fn dispatch(&self, method: &str, params: Value) -> Result<Value> {
        match method {
            "RemoteSsh::capabilities" => Ok(self.inner.rpc.capabilities()),
            "RemoteSsh::connect" => self.connect(params).await,
            "RemoteSsh::answer_prompt" => self.answer_prompt(params),
            "RemoteSsh::cancel_connect" => self.cancel_connect(params),
            "RemoteSsh::open_channel" => self.open_channel(params),
            "RemoteSsh::release" => self.release(params),
            "RemoteSsh::attach_handles" => self.attach_handles(params),
            _ => bail!("unknown method: {method}"),
        }
    }

    /// Cancels this connection's connects and prompts now, and releases its handles unless
    /// another connection attaches them within `HANDLE_REAP_DELAY`.
    pub fn close(&self) {
        let (connects, prompts) = {
            let mut state = self.state();
            state.closed = true;
            state.outgoing = None;
            (
                std::mem::take(&mut state.connects),
                std::mem::take(&mut state.prompts),
            )
        };
        for (_, cancel) in connects {
            if cancel.send(()).is_err() {
                tracing::debug!("an ssh connect finished while its connection closed");
            }
        }
        drop(prompts);
        let rpc = self.inner.rpc.clone();
        let owner = self.inner.id;
        tokio::spawn(async move {
            tokio::time::sleep(HANDLE_REAP_DELAY).await;
            rpc.reap_handles(owner);
        });
    }

    fn state(&self) -> MutexGuard<'_, ConnectionState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn notify(&self, method: String, params: Value) {
        let Some(outgoing) = self.state().outgoing.clone() else {
            return;
        };
        let message = json!({"method": method, "params": params}).to_string();
        if outgoing.send(Message::Text(message)).is_err() {
            tracing::debug!(%method, "the rpc connection closed before an ssh notification");
        }
    }

    async fn connect(&self, params: Value) -> Result<Value> {
        let host = self.inner.rpc.enabled_host()?.clone();
        // Without a manifest no provider is installed, and the desktop flow that would run
        // instead cannot deploy a remote_server that a web build can use.
        anyhow::ensure!(
            host.bundle().is_some(),
            "this Zed Web build has no remote server bundle, so it cannot open ssh projects. \
             Run web/build.sh to build one"
        );
        let params: ConnectParams =
            serde_json::from_value(params).context("invalid RemoteSsh::connect params")?;
        validate_connect_id(&params.connect_id)?;
        check_connect_options(
            &params.options,
            self.inner.rpc.config.allowed_hosts.as_deref(),
        )?;
        check_commit(
            params.client_commit.as_deref(),
            host.bundle().map(|bundle| bundle.commit.as_str()),
        )?;

        let connect_id = params.connect_id;
        let (cancel_tx, cancel_rx) = oneshot::channel();
        {
            let mut state = self.state();
            if state.closed {
                bail!("the rpc connection is closing");
            }
            if state.connects.contains_key(&connect_id) {
                bail!("connect {connect_id} is already in progress");
            }
            state.connects.insert(connect_id.clone(), cancel_tx);
        }

        let result = self
            .run_connect(&host, &connect_id, params.options, cancel_rx)
            .await;

        {
            let mut state = self.state();
            state.connects.remove(&connect_id);
            state
                .prompts
                .retain(|_, prompt| prompt.connect_id != connect_id);
        }
        self.notify(
            format!("RemoteSsh::status:{connect_id}"),
            json!({"status": null}),
        );
        result
    }

    async fn run_connect(
        &self,
        host: &SshHostHandle,
        connect_id: &str,
        options: remote::SshConnectionOptions,
        mut cancel: oneshot::Receiver<()>,
    ) -> Result<Value> {
        let handle_id = HandleId(format!("k-{}", random_hex(16)));
        let (events_tx, events) = mpsc::unbounded();
        let mut events = Some(events);
        let (host_cancel, host_cancel_rx) = oneshot::channel();
        let (reply_tx, mut reply) = oneshot::channel();
        host.send(HostCommand::Connect {
            options,
            handle_id: handle_id.clone(),
            delegate: Arc::new(WebSshDelegate::new(events_tx)),
            cancel: host_cancel_rx,
            reply: reply_tx,
        })?;

        let connected = loop {
            let next_event = async {
                match events.as_mut() {
                    Some(events) => events.next().await,
                    None => futures::future::pending().await,
                }
            };
            futures::select_biased! {
                connected = reply => {
                    break connected.map_err(|_| anyhow!("the ssh host dropped the connect"))??;
                }
                event = next_event.fuse() => match event {
                    Some(DelegateEvent::Status(status)) => self.notify(
                        format!("RemoteSsh::status:{connect_id}"),
                        json!({"status": status}),
                    ),
                    Some(DelegateEvent::Prompt { prompt, reply: password_reply, cancellation }) => {
                        self.add_prompt(connect_id, prompt, password_reply, cancellation)
                    }
                    None => {
                        events = None;
                    }
                },
                _ = cancel => {
                    if host_cancel.send(()).is_err() {
                        tracing::debug!("the ssh host finished the connect being cancelled");
                    }
                    // The host may have connected just before the cancel reached it; its reply
                    // is then already queued, and nobody else would release that handle.
                    reply.close();
                    if let Ok(Some(Ok(_))) = reply.try_recv() {
                        self.inner.rpc.release_handle(&handle_id);
                    }
                    bail!("connect {connect_id} was cancelled");
                }
            }
        };

        self.inner.rpc.shared().handles.insert(
            handle_id.clone(),
            HandleEntry {
                host_id: connected.host_id.clone(),
                owner: self.inner.id,
            },
        );
        connect_result(&connected, &handle_id)
    }

    fn add_prompt(
        &self,
        connect_id: &str,
        prompt: String,
        reply: oneshot::Sender<EncryptedPassword>,
        cancellation: oneshot::Receiver<()>,
    ) {
        let prompt_id = format!("p-{}", random_hex(16));
        {
            let mut state = self.state();
            if state.closed || !state.connects.contains_key(connect_id) {
                return;
            }
            state.prompts.insert(
                prompt_id.clone(),
                PendingPrompt {
                    connect_id: connect_id.to_owned(),
                    reply,
                },
            );
        }
        tracing::info!(%connect_id, %prompt, "ssh is asking the browser for input");
        self.notify(
            format!("RemoteSsh::prompt:{connect_id}"),
            json!({"prompt_id": prompt_id, "prompt": prompt}),
        );

        let connection = self.clone();
        let connect_id = connect_id.to_owned();
        tokio::spawn(async move {
            // Resolves when askpass stops waiting: answered, timed out, or the connect ended.
            cancellation.await.ok();
            let still_pending = connection.state().prompts.remove(&prompt_id).is_some();
            if still_pending {
                connection.notify(
                    format!("RemoteSsh::prompt_cancelled:{connect_id}"),
                    json!({"prompt_id": prompt_id}),
                );
            }
        });
    }

    fn answer_prompt(&self, params: Value) -> Result<Value> {
        // Not deserialized into a struct: serde's type errors quote the offending value, and
        // this one is a password.
        let Some(prompt_id) = params.get("prompt_id").and_then(Value::as_str) else {
            bail!("invalid RemoteSsh::answer_prompt params: prompt_id must be a string");
        };
        let response = match params.get("response") {
            None | Some(Value::Null) => None,
            Some(Value::String(response)) => Some(response.as_str()),
            Some(_) => {
                bail!("invalid RemoteSsh::answer_prompt params: response must be a string or null")
            }
        };
        let prompt = self
            .state()
            .prompts
            .remove(prompt_id)
            .ok_or_else(|| anyhow!("unknown or expired prompt"))?;
        let Some(response) = response else {
            drop(prompt);
            return Ok(Value::Null);
        };
        let password = EncryptedPassword::try_from(response).context("the answer is too long")?;
        prompt
            .reply
            .send(password)
            .map_err(|_| anyhow!("unknown or expired prompt"))?;
        Ok(Value::Null)
    }

    fn cancel_connect(&self, params: Value) -> Result<Value> {
        let params: ConnectIdParams =
            serde_json::from_value(params).context("invalid RemoteSsh::cancel_connect params")?;
        let cancel = {
            let mut state = self.state();
            state
                .prompts
                .retain(|_, prompt| prompt.connect_id != params.connect_id);
            state.connects.remove(&params.connect_id)
        };
        if let Some(cancel) = cancel
            && cancel.send(()).is_err()
        {
            tracing::debug!("the ssh connect finished before its cancel");
        }
        Ok(Value::Null)
    }

    fn open_channel(&self, params: Value) -> Result<Value> {
        self.inner.rpc.enabled_host()?;
        let params: OpenChannelParams =
            serde_json::from_value(params).context("invalid RemoteSsh::open_channel params")?;
        validate_identifier(&params.identifier)?;
        let channel_id = format!("ch-{}", random_hex(16));
        let token = random_hex(32);
        {
            let mut shared = self.inner.rpc.shared();
            let host_id = shared
                .handles
                .get(&HandleId(params.handle_id.clone()))
                .map(|handle| handle.host_id.clone())
                .ok_or_else(|| anyhow!("unknown ssh handle {}", params.handle_id))?;
            shared.channels.insert(
                channel_id.clone(),
                ChannelEntry {
                    token: token.clone(),
                    host_id,
                    identifier: params.identifier,
                    reconnect: params.reconnect,
                    expires_at: tokio::time::Instant::now() + CHANNEL_TOKEN_LIFETIME,
                    attached: false,
                },
            );
        }
        let rpc = self.inner.rpc.clone();
        let expiring = channel_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(CHANNEL_TOKEN_LIFETIME).await;
            let mut shared = rpc.shared();
            if shared
                .channels
                .get(&expiring)
                .is_some_and(|channel| !channel.attached)
            {
                shared.channels.remove(&expiring);
            }
        });
        Ok(json!({"channel_id": channel_id, "token": token}))
    }

    fn release(&self, params: Value) -> Result<Value> {
        let params: HandleIdParams =
            serde_json::from_value(params).context("invalid RemoteSsh::release params")?;
        let handle_id = HandleId(params.handle_id);
        let released = self.inner.rpc.shared().handles.remove(&handle_id);
        if released.is_some() {
            self.inner.rpc.release_handle(&handle_id);
        }
        Ok(Value::Null)
    }

    fn attach_handles(&self, params: Value) -> Result<Value> {
        let params: HandleIdsParams =
            serde_json::from_value(params).context("invalid RemoteSsh::attach_handles params")?;
        let mut shared = self.inner.rpc.shared();
        let mut attached = Vec::new();
        let mut missing = Vec::new();
        for handle_id in params.handle_ids {
            match shared.handles.get_mut(&HandleId(handle_id.clone())) {
                Some(handle) => {
                    handle.owner = self.inner.id;
                    attached.push(handle_id);
                }
                None => missing.push(handle_id),
            }
        }
        Ok(json!({"attached": attached, "missing": missing}))
    }
}

fn connect_result(connected: &ConnectedHost, handle_id: &HandleId) -> Result<Value> {
    let path_style = match connected.path_style {
        util::paths::PathStyle::Unix => "unix",
        util::paths::PathStyle::Windows => "windows",
    };
    Ok(json!({
        "host_id": connected.host_id.0,
        "handle_id": handle_id.0,
        "platform": {
            "os": connected.platform.os.as_str(),
            "arch": connected.platform.arch.as_str(),
        },
        "os_version": connected.os_version,
        "path_style": path_style,
        "shell": connected.shell,
        "default_system_shell": connected.default_system_shell,
        "recipe": serde_json::to_value(&connected.recipe)?,
    }))
}

fn random_hex(byte_count: usize) -> String {
    let mut bytes = vec![0_u8; byte_count];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn is_name_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_' || character == '-'
}

/// It becomes part of notification method names, so it must not carry a `:` or whitespace.
fn validate_connect_id(connect_id: &str) -> Result<()> {
    anyhow::ensure!(
        (1..=64).contains(&connect_id.len()) && connect_id.chars().all(is_name_character),
        "connect_id must be 1 to 64 characters of A-Z, a-z, 0-9, _ or -"
    );
    Ok(())
}

/// It becomes a socket directory name on the remote host.
pub(crate) fn validate_identifier(identifier: &str) -> Result<()> {
    anyhow::ensure!(
        (1..=40).contains(&identifier.len()) && identifier.chars().all(is_name_character),
        "identifier {identifier:?} must match ^[A-Za-z0-9_-]{{1,40}}$"
    );
    Ok(())
}

pub(crate) fn check_commit(
    client_commit: Option<&str>,
    bundled_commit: Option<&str>,
) -> Result<()> {
    let client_commit = client_commit
        .map(str::trim)
        .filter(|commit| !commit.is_empty());
    let bundled_commit = bundled_commit
        .map(str::trim)
        .filter(|commit| !commit.is_empty());
    if let (Some(client_commit), Some(bundled_commit)) = (client_commit, bundled_commit)
        && client_commit != bundled_commit
    {
        bail!(
            "remote_server commit mismatch: browser {client_commit}, bundled {bundled_commit}; \
             reload the page"
        );
    }
    Ok(())
}

pub(crate) fn check_connect_options(
    options: &remote::SshConnectionOptions,
    allowed_hosts: Option<&[String]>,
) -> Result<()> {
    anyhow::ensure!(
        options.password.is_none(),
        "options.password is not accepted; the password is asked for when ssh needs it"
    );
    let host = options.host.to_string();
    // ssh reads a destination that starts with `-` as an option, e.g. `-oProxyCommand=…`.
    anyhow::ensure!(
        is_plain_argument(&host) && !host.contains('@'),
        "options.host {host:?} is not a host name"
    );
    if let Some(username) = &options.username {
        anyhow::ensure!(
            is_plain_argument(username),
            "options.username {username:?} is not a user name"
        );
    }
    // With an allowlist, the arguments must not be able to name a different destination than
    // the one that was checked.
    check_ssh_args_inner(
        options.args.as_deref().unwrap_or_default(),
        allowed_hosts.is_some(),
    )?;
    if let Some(allowed_hosts) = allowed_hosts
        && !host_allowed(&host, allowed_hosts)
    {
        bail!("host {host} is not in .zed/web.json ssh.allowed_hosts");
    }
    Ok(())
}

fn is_plain_argument(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
}

/// The allowlists `SshConnectionOptions::parse_command_line` applies to what a user types.
const ALLOWED_FLAGS: &[&str] = &[
    "-4", "-6", "-A", "-a", "-C", "-K", "-k", "-X", "-x", "-Y", "-y",
];
/// `-I` is absent on purpose: it loads a PKCS#11 library into ssh on this server.
const ALLOWED_WITH_VALUE: &[&str] = &[
    "-B", "-b", "-c", "-D", "-F", "-i", "-J", "-l", "-m", "-o", "-P", "-p", "-R", "-w",
];
/// ssh_config keywords that make ssh run a command on this server. `KnownHostsCommand` is not
/// in the spec's list of three but executes a local command exactly like `LocalCommand`.
/// The providers and `XAuthLocation` (run by `-X`/`-Y`) load or exec a file on this server just
/// the same. ssh keeps the first value it sees and zed appends its own `ControlMaster` and
/// `ControlPath` after the browser's arguments, so the browser's would win and could ride
/// another host's master.
const REFUSED_OPTIONS: &[&str] = &[
    "proxycommand",
    "localcommand",
    "permitlocalcommand",
    "knownhostscommand",
    "pkcs11provider",
    "securitykeyprovider",
    "xauthlocation",
    "controlpath",
    "controlmaster",
];
/// Refused only when `ssh.allowed_hosts` is set, because they change where ssh connects.
const DESTINATION_OPTIONS: &[&str] = &["hostname"];

#[cfg(test)]
pub(crate) fn check_ssh_args(args: &[String]) -> Result<()> {
    check_ssh_args_inner(args, false)
}

fn check_ssh_args_inner(args: &[String], pin_destination: bool) -> Result<()> {
    let mut remaining = args.iter();
    while let Some(argument) = remaining.next() {
        if ALLOWED_FLAGS.contains(&argument.as_str()) {
            continue;
        }
        if argument.starts_with("-I") {
            bail!(
                "options.args: -I loads a library on this server and is not accepted; \
                 put PKCS11Provider in the server's ~/.ssh/config instead"
            );
        }
        let Some(flag) = ALLOWED_WITH_VALUE
            .iter()
            .find(|flag| argument.starts_with(**flag))
        else {
            bail!("options.args: unsupported ssh argument {argument:?}");
        };
        let value = if argument.as_str() == *flag {
            remaining
                .next()
                .ok_or_else(|| anyhow!("options.args: {flag} needs a value"))?
                .as_str()
        } else {
            &argument[flag.len()..]
        };
        match *flag {
            "-o" => check_ssh_option(value, pin_destination)?,
            "-J" => check_jump_hosts(value)?,
            "-F" if pin_destination => bail!(
                "options.args: -F can change the destination and is not accepted while \
                 .zed/web.json ssh.allowed_hosts is set"
            ),
            _ => {}
        }
    }
    Ok(())
}

/// ssh separates an option's keyword from its value with `=` or whitespace, compares keywords
/// case-insensitively, and unquotes them; anything but a bare keyword is refused so that no
/// spelling of a refused keyword gets through.
fn check_ssh_option(option: &str, pin_destination: bool) -> Result<()> {
    let option = option.trim_start();
    let keyword_len = option
        .find(|character: char| !character.is_ascii_alphanumeric())
        .unwrap_or(option.len());
    let (keyword, rest) = option.split_at(keyword_len);
    let separated = rest.is_empty()
        || rest.starts_with('=')
        || rest.starts_with(|character: char| character.is_whitespace());
    anyhow::ensure!(
        !keyword.is_empty() && separated,
        "options.args: unsupported ssh option {option:?}"
    );
    if REFUSED_OPTIONS.contains(&keyword.to_ascii_lowercase().as_str()) {
        bail!(
            "options.args: -o {keyword} runs a command on this server or takes over zed's connection and is \
             not accepted; put it in the server's ~/.ssh/config instead"
        );
    }
    let keyword = keyword.to_ascii_lowercase();
    if pin_destination && DESTINATION_OPTIONS.contains(&keyword.as_str()) {
        bail!(
            "options.args: -o {keyword} can change the destination and is not accepted while \
             .zed/web.json ssh.allowed_hosts is set"
        );
    }
    if keyword == "proxyjump" {
        let value = rest
            .trim_start_matches(|character: char| character == '=' || character.is_whitespace());
        check_jump_hosts(value)?;
    }
    Ok(())
}

/// ssh expands a jump host into a nested ssh command line, where one that starts with `-` is
/// an option on OpenSSH before 8.4.
fn check_jump_hosts(jump_hosts: &str) -> Result<()> {
    anyhow::ensure!(
        !jump_hosts.trim().is_empty(),
        "options.args: jump host list is empty"
    );
    for jump_host in jump_hosts.split(',') {
        let jump_host = jump_host.trim().trim_matches(['"', '\'']);
        anyhow::ensure!(
            !jump_host.is_empty()
                && !jump_host.starts_with('-')
                && !jump_host
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_control()),
            "options.args: jump host {jump_host:?} is not a host name"
        );
    }
    Ok(())
}

/// Matches `SshConnectionOptions.host.to_string()` against `.zed/web.json` `ssh.allowed_hosts`:
/// exact names case-insensitively, IP addresses by value, and `*.suffix` for subdomains only.
pub(crate) fn host_allowed(host: &str, allowed_hosts: &[String]) -> bool {
    let unbracketed_host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let host_address = unbracketed_host.parse::<IpAddr>().ok();
    allowed_hosts.iter().any(|pattern| {
        let pattern = pattern.trim();
        if let Some(suffix) = pattern.strip_prefix("*.") {
            return host_address.is_none()
                && !suffix.is_empty()
                && unbracketed_host.len() > suffix.len() + 1
                && unbracketed_host
                    .to_ascii_lowercase()
                    .ends_with(&format!(".{}", suffix.to_ascii_lowercase()));
        }
        let unbracketed_pattern = pattern
            .strip_prefix('[')
            .and_then(|pattern| pattern.strip_suffix(']'))
            .unwrap_or(pattern);
        match (host_address, unbracketed_pattern.parse::<IpAddr>()) {
            (Some(host_address), Ok(pattern_address)) => host_address == pattern_address,
            _ => unbracketed_host.eq_ignore_ascii_case(unbracketed_pattern),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh_host::BundleSummary;
    use futures::channel::mpsc::UnboundedReceiver;

    const COMMIT: &str = "a1b2c3d4e5f6a7b8c9d0";

    fn enabled_rpc(config: SshConfig) -> (Arc<SshRpc>, UnboundedReceiver<HostCommand>) {
        let (host, commands) = SshHostHandle::channel(Some(BundleSummary {
            commit: COMMIT.to_string(),
            platforms: vec!["linux-x86_64".to_string()],
        }));
        (SshRpc::new(Some(host), config), commands)
    }

    fn tab(rpc: &Arc<SshRpc>) -> (SshConnection, tokio_mpsc::UnboundedReceiver<Message>) {
        let (outgoing, received) = tokio_mpsc::unbounded_channel();
        (rpc.connection(outgoing), received)
    }

    fn connect_params(connect_id: &str, host: &str) -> Value {
        json!({
            "connect_id": connect_id,
            "options": {
                "host": {"Hostname": host}, "username": "andy", "port": 22, "password": null,
                "args": ["-o", "ServerAliveInterval=30"], "port_forwards": null,
                "connection_timeout": null, "nickname": null, "upload_binary_over_ssh": false
            },
            "client_commit": COMMIT,
        })
    }

    fn connected_host(host_id: &str) -> ConnectedHost {
        ConnectedHost {
            host_id: HostId(host_id.to_string()),
            platform: remote::RemotePlatform {
                os: remote::RemoteOs::Linux,
                arch: remote::RemoteArch::X86_64,
            },
            os_version: Some("ubuntu 24.04".to_string()),
            path_style: util::paths::PathStyle::Unix,
            shell: "/bin/bash".to_string(),
            default_system_shell: "/bin/sh".to_string(),
            recipe: remote::SshCommandRecipe {
                ssh_options: vec![
                    "-o".to_string(),
                    "ControlMaster=no".to_string(),
                    "-o".to_string(),
                    "ControlPath=/tmp/zed-ssh-sessionAbC/ssh.sock".to_string(),
                ],
                destination: "andy@devbox".to_string(),
                env: Default::default(),
                shell: "/bin/bash".to_string(),
                is_windows: false,
                path_style: remote::RecipePathStyle::Unix,
            },
        }
    }

    struct ConnectRequest {
        options: remote::SshConnectionOptions,
        delegate: Arc<WebSshDelegate>,
        cancel: oneshot::Receiver<()>,
        reply: oneshot::Sender<Result<ConnectedHost>>,
    }

    async fn next_connect(commands: &mut UnboundedReceiver<HostCommand>) -> ConnectRequest {
        match commands.next().await {
            Some(HostCommand::Connect {
                options,
                delegate,
                cancel,
                reply,
                ..
            }) => ConnectRequest {
                options,
                delegate,
                cancel,
                reply,
            },
            Some(_) => panic!("expected a Connect command"),
            None => panic!("the host channel closed"),
        }
    }

    async fn next_message(received: &mut tokio_mpsc::UnboundedReceiver<Message>) -> Value {
        match received.recv().await {
            Some(Message::Text(text)) => serde_json::from_str(&text).expect("json notification"),
            other => panic!("expected a text notification, got {other:?}"),
        }
    }

    fn pending_messages(received: &mut tokio_mpsc::UnboundedReceiver<Message>) -> Vec<Value> {
        let mut messages = Vec::new();
        while let Ok(Message::Text(text)) = received.try_recv() {
            messages.push(serde_json::from_str(&text).expect("json notification"));
        }
        messages
    }

    async fn connect_ok(
        connection: &SshConnection,
        commands: &mut UnboundedReceiver<HostCommand>,
        connect_id: &str,
        host_id: &str,
    ) -> Value {
        let dispatch = tokio::spawn({
            let connection = connection.clone();
            let params = connect_params(connect_id, "devbox");
            async move { connection.dispatch("RemoteSsh::connect", params).await }
        });
        let request = next_connect(commands).await;
        assert!(request.reply.send(Ok(connected_host(host_id))).is_ok());
        dispatch
            .await
            .expect("connect task")
            .expect("connect succeeds")
    }

    fn error_text(result: Result<Value>) -> String {
        match result {
            Ok(value) => panic!("expected an error, got {value}"),
            Err(error) => format!("{error:#}"),
        }
    }

    #[tokio::test]
    async fn a_server_without_ssh_names_the_switch() {
        let rpc = SshRpc::new(None, SshConfig::default());
        let (connection, _received) = tab(&rpc);
        let capabilities = connection
            .dispatch("RemoteSsh::capabilities", json!({}))
            .await
            .expect("capabilities");
        assert_eq!(capabilities["enabled"], false);
        assert_eq!(capabilities["reason"], DISABLED_REASON);
        assert_eq!(capabilities["commit"], Value::Null);
        assert_eq!(capabilities["platforms"], json!([]));
        assert_eq!(capabilities["ssh_agent"], false);

        let error = error_text(
            connection
                .dispatch("RemoteSsh::connect", connect_params("c-1", "devbox"))
                .await,
        );
        assert!(error.contains("--allow-ssh"), "{error}");
    }

    #[tokio::test]
    async fn capabilities_report_the_bundle_and_the_agent() {
        let (rpc, _commands) = enabled_rpc(SshConfig {
            ssh_agent: true,
            ..SshConfig::default()
        });
        let capabilities = rpc.capabilities();
        assert_eq!(
            capabilities,
            json!({
                "enabled": true, "reason": null, "commit": COMMIT,
                "platforms": ["linux-x86_64"], "ssh_agent": true,
            })
        );
    }

    #[tokio::test]
    async fn connect_without_a_manifest_names_the_build_script() {
        // ssh is switched on, but this build shipped no remote_server manifest.
        let (host, mut commands) = SshHostHandle::channel(None);
        let rpc = SshRpc::new(Some(host), SshConfig::default());
        let (connection, _received) = tab(&rpc);

        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            connection.dispatch("RemoteSsh::connect", connect_params("c-1", "devbox")),
        )
        .await;
        let error = match outcome {
            Ok(result) => error_text(result),
            Err(_) => panic!(
                "connect did not refuse: still waiting after 2s (host received a command: {})",
                commands.try_recv().is_ok()
            ),
        };
        assert!(error.contains("web/build.sh"), "{error}");
        assert!(
            commands.try_recv().is_err(),
            "a refused connect must not reach the host"
        );
    }

    #[tokio::test]
    async fn connect_is_allowed_with_restrict_paths_on() {
        let config = SshConfig::from_web_json(
            &json!({"restrict_paths": true, "ssh": {"enabled": true}}),
            false,
        );
        assert_eq!(config, SshConfig::default());
        let (rpc, mut commands) = enabled_rpc(config);
        assert_eq!(rpc.capabilities()["enabled"], true);
        let (connection, _received) = tab(&rpc);

        let result = connect_ok(&connection, &mut commands, "c-1", "h-1").await;
        assert_eq!(result["host_id"], "h-1");
        assert!(
            result["handle_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("k-") && id.len() == 34)
        );
        assert_eq!(result["platform"], json!({"os": "linux", "arch": "x86_64"}));
        assert_eq!(result["os_version"], "ubuntu 24.04");
        assert_eq!(result["path_style"], "unix");
        assert_eq!(result["shell"], "/bin/bash");
        assert_eq!(result["default_system_shell"], "/bin/sh");
        assert_eq!(result["recipe"]["destination"], "andy@devbox");
        assert_eq!(result["recipe"]["path_style"], "unix");
        assert_eq!(result["recipe"]["is_windows"], false);
    }

    #[tokio::test]
    async fn refused_connects_never_reach_the_host() {
        let (rpc, mut commands) = enabled_rpc(SshConfig {
            allowed_hosts: Some(vec!["*.corp.example".to_string()]),
            ..SshConfig::default()
        });
        let (connection, _received) = tab(&rpc);

        let mut with_password = connect_params("c-1", "build.corp.example");
        with_password["options"]["password"] = json!("hunter2");
        let error = error_text(
            connection
                .dispatch("RemoteSsh::connect", with_password)
                .await,
        );
        assert_eq!(
            error,
            "options.password is not accepted; the password is asked for when ssh needs it"
        );

        let error = error_text(
            connection
                .dispatch("RemoteSsh::connect", connect_params("c-2", "devbox"))
                .await,
        );
        assert_eq!(
            error,
            "host devbox is not in .zed/web.json ssh.allowed_hosts"
        );

        let mut proxy_command = connect_params("c-3", "build.corp.example");
        proxy_command["options"]["args"] = json!(["-o", "ProxyCommand=nc %h %p"]);
        let error = error_text(
            connection
                .dispatch("RemoteSsh::connect", proxy_command)
                .await,
        );
        assert!(error.contains("ProxyCommand"), "{error}");

        let mut stale_browser = connect_params("c-4", "build.corp.example");
        stale_browser["client_commit"] = json!("c3d4e5f6");
        let error = error_text(
            connection
                .dispatch("RemoteSsh::connect", stale_browser)
                .await,
        );
        assert_eq!(
            error,
            format!(
                "remote_server commit mismatch: browser c3d4e5f6, bundled {COMMIT}; reload the page"
            )
        );

        assert!(
            commands.try_recv().is_err(),
            "no command was sent to the host"
        );
    }

    #[tokio::test]
    async fn a_prompt_reaches_only_the_tab_that_connected() {
        let (rpc, mut commands) = enabled_rpc(SshConfig::default());
        let (first_tab, mut first_received) = tab(&rpc);
        let (second_tab, mut second_received) = tab(&rpc);

        let dispatch = tokio::spawn({
            let first_tab = first_tab.clone();
            async move {
                first_tab
                    .dispatch("RemoteSsh::connect", connect_params("c-1", "devbox"))
                    .await
            }
        });
        let request = next_connect(&mut commands).await;
        assert_eq!(request.options.host.to_string(), "devbox");
        request.delegate.report_status(Some("Connecting"));
        let (password_tx, password_rx) = oneshot::channel();
        let (_cancellation_tx, cancellation_rx) = oneshot::channel();
        request.delegate.request_password(
            "andy@devbox's password: ".to_string(),
            password_tx,
            cancellation_rx,
        );

        let status = next_message(&mut first_received).await;
        assert_eq!(
            status,
            json!({"method": "RemoteSsh::status:c-1", "params": {"status": "Connecting"}})
        );
        let prompt = next_message(&mut first_received).await;
        assert_eq!(prompt["method"], "RemoteSsh::prompt:c-1");
        assert_eq!(prompt["params"]["prompt"], "andy@devbox's password: ");
        let prompt_id = prompt["params"]["prompt_id"]
            .as_str()
            .expect("prompt id")
            .to_string();
        assert!(
            prompt_id.starts_with("p-") && prompt_id.len() == 34,
            "{prompt_id}"
        );
        assert!(
            pending_messages(&mut second_received).is_empty(),
            "the other tab saw the prompt"
        );

        let error = error_text(
            second_tab
                .dispatch(
                    "RemoteSsh::answer_prompt",
                    json!({"prompt_id": prompt_id, "response": "stolen"}),
                )
                .await,
        );
        assert_eq!(error, "unknown or expired prompt");

        first_tab
            .dispatch(
                "RemoteSsh::answer_prompt",
                json!({"prompt_id": prompt_id, "response": "hunter2"}),
            )
            .await
            .expect("answer");
        assert!(password_rx.await.is_ok(), "the password reached askpass");

        assert!(request.reply.send(Ok(connected_host("h-1"))).is_ok());
        let result = dispatch.await.expect("connect task").expect("connect");
        assert_eq!(result["host_id"], "h-1");
        assert_eq!(
            next_message(&mut first_received).await,
            json!({"method": "RemoteSsh::status:c-1", "params": {"status": null}})
        );
        assert!(pending_messages(&mut second_received).is_empty());
    }

    #[tokio::test]
    async fn a_null_answer_refuses_the_prompt_and_unknown_ids_are_errors() {
        let (rpc, mut commands) = enabled_rpc(SshConfig::default());
        let (connection, mut received) = tab(&rpc);
        let _dispatch = tokio::spawn({
            let connection = connection.clone();
            async move {
                connection
                    .dispatch("RemoteSsh::connect", connect_params("c-1", "devbox"))
                    .await
            }
        });
        let request = next_connect(&mut commands).await;
        let (password_tx, password_rx) = oneshot::channel();
        let (_cancellation_tx, cancellation_rx) = oneshot::channel();
        request
            .delegate
            .request_password("password:".to_string(), password_tx, cancellation_rx);
        let prompt = next_message(&mut received).await;
        let prompt_id = prompt["params"]["prompt_id"].clone();

        let error = error_text(
            connection
                .dispatch(
                    "RemoteSsh::answer_prompt",
                    json!({"prompt_id": "p-00000000000000000000000000000000", "response": "x"}),
                )
                .await,
        );
        assert_eq!(error, "unknown or expired prompt");

        connection
            .dispatch(
                "RemoteSsh::answer_prompt",
                json!({"prompt_id": prompt_id, "response": null}),
            )
            .await
            .expect("cancel the prompt");
        assert!(
            password_rx.await.is_err(),
            "askpass sees the prompt refused"
        );

        let error = error_text(
            connection
                .dispatch(
                    "RemoteSsh::answer_prompt",
                    json!({"prompt_id": prompt_id, "response": "late"}),
                )
                .await,
        );
        assert_eq!(error, "unknown or expired prompt");
    }

    #[tokio::test]
    async fn a_prompt_askpass_gave_up_on_is_withdrawn() {
        let (rpc, mut commands) = enabled_rpc(SshConfig::default());
        let (connection, mut received) = tab(&rpc);
        let _dispatch = tokio::spawn({
            let connection = connection.clone();
            async move {
                connection
                    .dispatch("RemoteSsh::connect", connect_params("c-1", "devbox"))
                    .await
            }
        });
        let request = next_connect(&mut commands).await;
        let (password_tx, _password_rx) = oneshot::channel();
        let (cancellation_tx, cancellation_rx) = oneshot::channel::<()>();
        request
            .delegate
            .request_password("password:".to_string(), password_tx, cancellation_rx);
        let prompt = next_message(&mut received).await;
        drop(cancellation_tx);
        assert_eq!(
            next_message(&mut received).await,
            json!({
                "method": "RemoteSsh::prompt_cancelled:c-1",
                "params": {"prompt_id": prompt["params"]["prompt_id"]},
            })
        );
    }

    #[tokio::test]
    async fn cancel_connect_abandons_the_host_connect() {
        let (rpc, mut commands) = enabled_rpc(SshConfig::default());
        let (connection, _received) = tab(&rpc);
        let dispatch = tokio::spawn({
            let connection = connection.clone();
            async move {
                connection
                    .dispatch("RemoteSsh::connect", connect_params("c-1", "devbox"))
                    .await
            }
        });
        let request = next_connect(&mut commands).await;
        connection
            .dispatch("RemoteSsh::cancel_connect", json!({"connect_id": "c-1"}))
            .await
            .expect("cancel");
        let error = error_text(dispatch.await.expect("connect task"));
        assert_eq!(error, "connect c-1 was cancelled");
        assert!(request.cancel.await.is_ok(), "the host was told to stop");
    }

    #[tokio::test]
    async fn closing_the_rpc_connection_cancels_its_connects() {
        let (rpc, mut commands) = enabled_rpc(SshConfig::default());
        let (connection, _received) = tab(&rpc);
        let dispatch = tokio::spawn({
            let connection = connection.clone();
            async move {
                connection
                    .dispatch("RemoteSsh::connect", connect_params("c-1", "devbox"))
                    .await
            }
        });
        let request = next_connect(&mut commands).await;
        connection.close();
        let error = error_text(dispatch.await.expect("connect task"));
        assert_eq!(error, "connect c-1 was cancelled");
        assert!(request.cancel.await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn handles_of_a_closed_connection_are_released_after_the_delay() {
        let (rpc, mut commands) = enabled_rpc(SshConfig::default());
        let (first_tab, _first_received) = tab(&rpc);
        let kept = connect_ok(&first_tab, &mut commands, "c-1", "h-1").await["handle_id"].clone();
        let reaped = connect_ok(&first_tab, &mut commands, "c-2", "h-1").await["handle_id"].clone();

        first_tab.close();
        tokio::time::sleep(HANDLE_REAP_DELAY - Duration::from_secs(1)).await;
        assert!(commands.try_recv().is_err(), "released before the delay");

        let (second_tab, _second_received) = tab(&rpc);
        let attached = second_tab
            .dispatch(
                "RemoteSsh::attach_handles",
                json!({"handle_ids": [kept, "k-missing"]}),
            )
            .await
            .expect("attach");
        assert_eq!(
            attached,
            json!({"attached": [kept], "missing": ["k-missing"]})
        );

        tokio::time::sleep(Duration::from_secs(2)).await;
        match commands.try_recv() {
            Ok(HostCommand::Release { handle_id }) => assert_eq!(json!(handle_id.0), reaped),
            _ => panic!("the unattached handle was not released"),
        }
        assert!(
            commands.try_recv().is_err(),
            "the reattached handle must stay"
        );

        second_tab
            .dispatch("RemoteSsh::release", json!({"handle_id": kept}))
            .await
            .expect("release");
        match commands.try_recv() {
            Ok(HostCommand::Release { handle_id }) => assert_eq!(json!(handle_id.0), kept),
            _ => panic!("release did not reach the host"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_channel_token_is_single_use_and_expires() {
        let (rpc, mut commands) = enabled_rpc(SshConfig::default());
        let (connection, _received) = tab(&rpc);
        let handle_id =
            connect_ok(&connection, &mut commands, "c-1", "h-1").await["handle_id"].clone();

        for identifier in ["", "a/b", "../escape", &"x".repeat(41)] {
            let error = error_text(
                connection
                    .dispatch(
                        "RemoteSsh::open_channel",
                        json!({"handle_id": handle_id, "identifier": identifier, "reconnect": false}),
                    )
                    .await,
            );
            assert!(
                error.contains("must match ^[A-Za-z0-9_-]{1,40}$"),
                "{identifier:?}: {error}"
            );
        }
        let error = error_text(
            connection
                .dispatch(
                    "RemoteSsh::open_channel",
                    json!({"handle_id": "k-unknown", "identifier": "web-dev-workspace-1"}),
                )
                .await,
        );
        assert_eq!(error, "unknown ssh handle k-unknown");

        let open = |identifier: String| {
            let connection = connection.clone();
            let handle_id = handle_id.clone();
            async move {
                let channel = connection
                    .dispatch(
                        "RemoteSsh::open_channel",
                        json!({"handle_id": handle_id, "identifier": identifier, "reconnect": true}),
                    )
                    .await
                    .expect("open channel");
                (
                    channel["channel_id"]
                        .as_str()
                        .expect("channel id")
                        .to_string(),
                    channel["token"].as_str().expect("token").to_string(),
                )
            }
        };
        let (channel_id, token) = open("x".repeat(40)).await;
        assert!(
            channel_id.starts_with("ch-") && channel_id.len() == 35,
            "{channel_id}"
        );
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|character| character.is_ascii_hexdigit()));

        assert_eq!(
            rpc.claim_channel(&channel_id, &"0".repeat(64)),
            Err(ChannelRefusal::InvalidToken)
        );
        let claim = rpc.claim_channel(&channel_id, &token).expect("first claim");
        assert_eq!(claim.host_id, HostId("h-1".to_string()));
        assert_eq!(claim.identifier, "x".repeat(40));
        assert!(claim.reconnect);
        assert_eq!(
            rpc.claim_channel(&channel_id, &token),
            Err(ChannelRefusal::AlreadyAttached)
        );
        rpc.finish_channel(&channel_id);
        assert_eq!(
            rpc.claim_channel(&channel_id, &token),
            Err(ChannelRefusal::InvalidToken)
        );

        let (expiring_id, expiring_token) = open("web-dev-workspace-1".to_string()).await;
        tokio::time::sleep(CHANNEL_TOKEN_LIFETIME).await;
        assert_eq!(
            rpc.claim_channel(&expiring_id, &expiring_token),
            Err(ChannelRefusal::InvalidToken)
        );
    }

    #[test]
    fn allowed_hosts_match_exact_names_suffixes_and_addresses() {
        let allowed = [
            "devbox".to_string(),
            "*.corp.example".to_string(),
            "2001:db8::1".to_string(),
            "[fe80::2]".to_string(),
            "10.0.0.5".to_string(),
        ];
        for host in [
            "devbox",
            "DevBox",
            "build.corp.example",
            "a.b.CORP.example",
            "2001:db8::1",
            "2001:0db8:0:0:0:0:0:1",
            "fe80::2",
            "10.0.0.5",
        ] {
            assert!(host_allowed(host, &allowed), "{host} should be allowed");
        }
        for host in [
            "devbox2",
            "corp.example",
            "evilcorp.example",
            "build.corp.example.evil",
            "2001:db8::2",
            "10.0.0.50",
            "",
        ] {
            assert!(!host_allowed(host, &allowed), "{host} should be refused");
        }
        assert!(!host_allowed("devbox", &[]));
    }

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn ssh_args_keep_the_parse_command_line_allowlist() -> Result<()> {
        for args in [
            arguments(&[]),
            arguments(&["-o", "ServerAliveInterval=30"]),
            arguments(&["-oServerAliveInterval=30"]),
            arguments(&["-o", "ServerAliveInterval 30"]),
            arguments(&["-i", "/home/andy/.ssh/id_ed25519", "-A", "-C"]),
            arguments(&["-J", "jump.corp.example", "-p", "2222", "-4"]),
            arguments(&["-o", "ProxyJump=jump.corp.example"]),
            arguments(&["-F", "/home/andy/.ssh/config"]),
        ] {
            check_ssh_args(&args).with_context(|| format!("{args:?}"))?;
        }
        Ok(())
    }

    #[test]
    fn ssh_args_that_run_commands_on_the_server_are_refused() {
        for args in [
            arguments(&["-o", "ProxyCommand=nc %h %p"]),
            arguments(&["-oProxyCommand=nc %h %p"]),
            arguments(&["-o", "proxycommand nc %h %p"]),
            arguments(&["-o", "  PROXYCOMMAND = nc"]),
            arguments(&["-o", "LocalCommand=touch /tmp/owned"]),
            arguments(&["-o", "PermitLocalCommand=yes"]),
            arguments(&["-o", "KnownHostsCommand=/bin/sh -c id"]),
            arguments(&["-o", "\"ProxyCommand\"=nc"]),
            arguments(&["-o"]),
            arguments(&["-N"]),
            arguments(&["-E", "/tmp/log"]),
            arguments(&["--", "devbox"]),
            arguments(&["devbox"]),
        ] {
            assert!(check_ssh_args(&args).is_err(), "{args:?} should be refused");
        }
    }

    #[test]
    fn destinations_that_look_like_options_are_refused() {
        let mut options = remote::SshConnectionOptions {
            host: "-oProxyCommand=touch /tmp/owned".into(),
            ..Default::default()
        };
        assert!(check_connect_options(&options, None).is_err());
        options.host = "devbox".into();
        check_connect_options(&options, None).expect("plain host");
        options.username = Some("-oProxyCommand=x".to_string());
        assert!(check_connect_options(&options, None).is_err());
        options.username = Some("andy".to_string());
        options.host = "".into();
        assert!(check_connect_options(&options, None).is_err());
    }

    #[test]
    fn commits_are_compared_only_when_both_are_known() {
        assert!(check_commit(Some("abc"), Some("abc")).is_ok());
        assert!(check_commit(None, Some("abc")).is_ok());
        assert!(check_commit(Some("abc"), None).is_ok());
        assert!(check_commit(Some(""), Some("abc")).is_ok());
        assert!(check_commit(Some("abc"), Some("def")).is_err());
    }

    #[test]
    fn a_malformed_allowed_hosts_list_disables_ssh() {
        let config = SshConfig::from_web_json(&json!({"ssh": {"allowed_hosts": "devbox"}}), true);
        assert!(
            config
                .disabled_reason
                .is_some_and(|reason| reason.contains("allowed_hosts"))
        );
        let config = SshConfig::from_web_json(&json!({"ssh": {"allowed_hosts": ["a", 1]}}), true);
        assert!(config.disabled_reason.is_some());
        let config = SshConfig::from_web_json(&json!({"ssh": {"allowed_hosts": ["a"]}}), true);
        assert_eq!(config.allowed_hosts, Some(vec!["a".to_string()]));
        assert!(config.ssh_agent);
    }

    #[test]
    fn connect_ids_are_safe_in_notification_names() {
        assert!(validate_connect_id("c-0123456789abcdef0123456789abcdef").is_ok());
        assert!(validate_connect_id("").is_err());
        assert!(validate_connect_id("c:1").is_err());
        assert!(validate_connect_id(&"c".repeat(65)).is_err());
    }
}

#[cfg(test)]
mod review_round_one_tests {
    use super::*;

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn options_with(host: &str, args: &[&str]) -> remote::SshConnectionOptions {
        remote::SshConnectionOptions {
            host: host.into(),
            args: Some(arguments(args)),
            ..Default::default()
        }
    }

    #[test]
    fn a_web_json_ssh_block_that_is_not_an_object_disables_ssh() {
        for config in [
            json!({"ssh": "devbox"}),
            json!({"ssh": true}),
            json!({"ssh": ["devbox"]}),
            json!({"ssh": null}),
        ] {
            let ssh_config = SshConfig::from_web_json(&config, false);
            assert!(
                ssh_config.disabled_reason.is_some(),
                "{config} left ssh enabled with allowed_hosts {:?}",
                ssh_config.allowed_hosts
            );
        }
        for config in [
            Value::Null,
            json!({}),
            json!({"restrict_paths": true}),
            json!({"ssh": {}}),
            json!({"ssh": {"enabled": true}}),
        ] {
            assert_eq!(
                SshConfig::from_web_json(&config, false),
                SshConfig::default(),
                "{config} must stay enabled with every host allowed"
            );
        }
    }

    #[test]
    fn args_cannot_repoint_the_destination_or_the_control_socket() {
        let allowed = vec!["devbox".to_string()];
        for args in [
            &["-o", "HostName=evil.example"][..],
            &["-oHostName=evil.example"],
            &["-o", "hostname evil.example"],
            &["-o", "  HOSTNAME = evil.example"],
            &["-F", "/srv/root/evil.conf"],
            &["-F/srv/root/evil.conf"],
        ] {
            let result = check_connect_options(&options_with("devbox", args), Some(&allowed));
            assert!(
                result.is_err(),
                "{args:?} passed with allowed_hosts and would reach another host"
            );
        }
        for args in [
            &["-o", "ControlPath=/home/u/.ssh/cm-prod"][..],
            &["-oControlPath=/home/u/.ssh/cm-prod"],
            &["-o", "ControlMaster=auto"],
            &["-o", "controlpath /x"],
        ] {
            assert!(
                check_ssh_args(&arguments(args)).is_err(),
                "{args:?} is placed before zed's own ControlPath and wins"
            );
        }

        // What must not be killed: without an allowlist HostName and -F stay usable, and the
        // other multiplexing keywords and per-host settings are harmless.
        for args in [
            &["-o", "HostName=other.example"][..],
            &["-F", "/home/andy/.ssh/config"],
        ] {
            check_connect_options(&options_with("devbox", args), None).unwrap_or_else(|error| {
                panic!("{args:?} refused without allowed_hosts: {error:#}")
            });
        }
        for args in [
            &["-o", "ControlPersist=10m"][..],
            &["-o", "User=andy"],
            &["-o", "Port=22", "-o", "ServerAliveInterval=30"],
            &[
                "-i",
                "/home/andy/.ssh/id_ed25519",
                "-J",
                "jump.corp.example",
            ],
        ] {
            check_connect_options(&options_with("devbox", args), Some(&allowed))
                .unwrap_or_else(|error| panic!("{args:?} refused: {error:#}"));
        }
    }

    #[test]
    fn args_that_load_or_run_local_programs_are_refused() {
        for args in [
            &["-I", "/srv/root/evil.so"][..],
            &["-I/srv/root/evil.so"],
            &["-o", "PKCS11Provider=/srv/root/evil.so"],
            &["-o", "pkcs11provider /srv/root/evil.so"],
            &["-o", "SecurityKeyProvider=/srv/root/evil.so"],
            &["-o", "XAuthLocation=/srv/root/evil.sh", "-X"],
            &["-oXAuthLocation=/srv/root/evil.sh", "-Y"],
        ] {
            assert!(
                check_ssh_args(&arguments(args)).is_err(),
                "{args:?} should be refused"
            );
        }
        for args in [
            &["-i", "/home/andy/.ssh/id_ed25519"][..],
            &["-o", "IdentityFile=/home/andy/.ssh/id_ed25519"],
            &["-o", "IdentitiesOnly=yes", "-X"],
            &["-o", "ForwardX11=yes", "-Y"],
        ] {
            check_ssh_args(&arguments(args))
                .unwrap_or_else(|error| panic!("{args:?} refused: {error:#}"));
        }
    }

    #[test]
    fn a_jump_destination_that_looks_like_an_option_is_refused() {
        for args in [
            &["-J", "-oProxyCommand=touch /tmp/x"][..],
            &["-J-oProxyCommand=touch /tmp/x"],
            &["-o", "ProxyJump=-oProxyCommand=touch /tmp/x"],
            &["-o", "ProxyJump = -oProxyCommand=x"],
            &["-o", "proxyjump good.corp.example,-oProxyCommand=x"],
            &["-J", "good.corp.example,-x"],
        ] {
            assert!(
                check_ssh_args(&arguments(args)).is_err(),
                "{args:?} should be refused"
            );
        }
        for args in [
            &["-J", "andy@jump.corp.example:2222"][..],
            &["-J", "[::1]:22"],
            &["-J", "first,second.corp.example"],
            &["-o", "ProxyJump=jump.corp.example"],
            &["-o", "ProxyJump=none"],
            &["-J", "user-name@jump-host.example"],
        ] {
            check_ssh_args(&arguments(args))
                .unwrap_or_else(|error| panic!("{args:?} refused: {error:#}"));
        }
    }

    #[tokio::test]
    async fn an_answer_of_the_wrong_type_is_not_echoed_back() {
        let rpc = SshRpc::new(None, SshConfig::default());
        let (outgoing, _received) = tokio_mpsc::unbounded_channel();
        let connection = rpc.connection(outgoing);
        for response in [json!(12345678), json!(true), json!(1.5)] {
            let error = match connection
                .dispatch(
                    "RemoteSsh::answer_prompt",
                    json!({"prompt_id": "p-1", "response": response}),
                )
                .await
            {
                Ok(value) => panic!("expected an error, got {value}"),
                Err(error) => format!("{error:#}"),
            };
            assert!(
                !error.contains(&response.to_string()),
                "the error repeats the answer {response}: {error}"
            );
        }
        for params in [
            json!({"prompt_id": "p-1", "response": "hunter2"}),
            json!({"prompt_id": "p-1", "response": null}),
        ] {
            let error = match connection
                .dispatch("RemoteSsh::answer_prompt", params)
                .await
            {
                Ok(value) => panic!("expected an error, got {value}"),
                Err(error) => format!("{error:#}"),
            };
            assert_eq!(error, "unknown or expired prompt");
        }
    }
}

#[cfg(test)]
mod review_round_two_security_tests {
    use super::*;
    use crate::ssh_host::BundleSummary;

    const COMMIT: &str = "a1b2c3d4e5f6a7b8c9d0";

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn connected_host(host_id: &str) -> ConnectedHost {
        ConnectedHost {
            host_id: HostId(host_id.to_string()),
            platform: remote::RemotePlatform {
                os: remote::RemoteOs::Linux,
                arch: remote::RemoteArch::X86_64,
            },
            os_version: Some("ubuntu 24.04".to_string()),
            path_style: util::paths::PathStyle::Unix,
            shell: "/bin/bash".to_string(),
            default_system_shell: "/bin/sh".to_string(),
            recipe: remote::SshCommandRecipe {
                ssh_options: vec![
                    "-o".to_string(),
                    "ControlMaster=no".to_string(),
                    "-o".to_string(),
                    "ControlPath=/tmp/zed-ssh-sessionAbC/ssh.sock".to_string(),
                ],
                destination: "andy@devbox".to_string(),
                env: Default::default(),
                shell: "/bin/bash".to_string(),
                is_windows: false,
                path_style: remote::RecipePathStyle::Unix,
            },
        }
    }

    fn connect_params(connect_id: &str, host: &str) -> Value {
        json!({
            "connect_id": connect_id,
            "options": {
                "host": {"Hostname": host}, "username": "andy", "port": 22, "password": null,
                "args": ["-o", "ServerAliveInterval=30"], "port_forwards": null,
                "connection_timeout": null, "nickname": null, "upload_binary_over_ssh": false
            },
            "client_commit": COMMIT,
        })
    }

    #[test]
    fn events_stream_closure_does_not_busy_spin_while_waiting_for_reply() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        runtime.block_on(async {
            let (host, mut commands) = SshHostHandle::channel(Some(BundleSummary {
                commit: COMMIT.to_string(),
                platforms: vec!["linux-x86_64".to_string()],
            }));
            let rpc = SshRpc::new(Some(host), SshConfig::default());
            let (outgoing, _received) = tokio_mpsc::unbounded_channel();
            let connection = rpc.connection(outgoing);

            let connect_task = tokio::spawn(async move {
                connection
                    .dispatch(
                        "RemoteSsh::connect",
                        connect_params("c-spin-test", "devbox"),
                    )
                    .await
            });

            let command = commands.next().await.expect("host command");
            let (delegate, reply) = match command {
                HostCommand::Connect {
                    delegate, reply, ..
                } => (delegate, reply),
                _ => panic!("expected connect"),
            };

            // Dropping the delegate drops `events_tx`.
            drop(delegate);

            let timer_task = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                reply
                    .send(Ok(connected_host("h-spin-test")))
                    .expect("send reply");
            });

            let result = tokio::time::timeout(Duration::from_millis(300), connect_task)
                .await
                .expect("timed out: run_connect busy-spun on closed events stream without yielding")
                .expect("task join")
                .expect("connect ok");
            assert_eq!(result["host_id"], "h-spin-test");
            timer_task.await.expect("timer join");
        });
    }

    #[test]
    fn allowed_hosts_accepts_bracketed_ipv6_destinations() {
        let allowed = vec![
            "2001:db8::1".to_string(),
            "[fe80::2]".to_string(),
            "*.corp.example".to_string(),
        ];
        assert!(
            host_allowed("[2001:db8::1]", &allowed),
            "expected [2001:db8::1] to be allowed"
        );
        assert!(
            host_allowed("[2001:0db8:0:0:0:0:0:1]", &allowed),
            "expected [2001:0db8:0:0:0:0:0:1] to match 2001:db8::1"
        );
        assert!(
            host_allowed("[fe80::2]", &allowed),
            "expected [fe80::2] to match [fe80::2]"
        );
        assert!(
            !host_allowed("[2001:db8::2]", &allowed),
            "expected [2001:db8::2] to be refused"
        );
    }

    #[test]
    fn jump_hosts_rejects_empty_and_whitespace_arguments() {
        for empty_or_whitespace in ["", "  ", "host1,,host2", "jump.example -oProxyCommand=x"] {
            let result = check_ssh_args(&arguments(&["-J", empty_or_whitespace]));
            assert!(
                result.is_err(),
                "expected -J {empty_or_whitespace:?} to be rejected, got {result:?}"
            );
        }
    }

    #[test]
    fn host_with_at_sign_is_rejected_as_invalid_hostname() {
        let options = remote::SshConnectionOptions {
            host: "user@devbox".into(),
            ..Default::default()
        };
        let result = check_connect_options(&options, None);
        assert!(
            result.is_err(),
            "expected host with '@' to be rejected, got {result:?}"
        );
    }
}
