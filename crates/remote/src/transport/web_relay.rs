//! The browser's ssh transport: zed_web_server owns the ssh ControlMaster and the proxy process,
//! and this relays the proxy's stdio over a binary WebSocket (`/remote/channel`).

use std::{
    path::PathBuf,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow};
use askpass::{EncryptedPassword, IKnowWhatIAmDoingAndIHaveReadTheDocs};
use async_trait::async_trait;
use collections::HashMap;
use futures::{
    FutureExt as _, StreamExt as _,
    channel::{
        mpsc::{self, Sender, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    future::LocalBoxFuture,
    stream::FuturesUnordered,
};
use gpui::{App, AsyncApp, BackgroundExecutor, FutureExt as _, Task};
use rpc::proto::Envelope;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use util::paths::{PathStyle, RemotePathBuf};

use crate::{
    RemoteClientDelegate, RemoteConnection, RemoteConnectionOptions, RemotePlatform,
    SshConnectionOptions, build_ssh_command,
    protocol::{DEFAULT_MAX_FRAME_LEN, EnvelopeFramer, encode_frame},
    remote_client::{CommandTemplate, Interactive},
    web_relay_core::{
        ConnectResponse, exit_code_from_close, next_in_flight, parse_connect_response,
        prefixed_identifier,
    },
};

/// `wasm_rpc::call` has no timeout of its own, and `RemoteClient::reconnect` awaits these.
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
/// The relay route refuses messages over 2 MiB; frames are split below that.
const RELAY_CHUNK_SIZE: usize = 1024 * 1024;

static WEB_RPC_CLIENT: OnceLock<smol::RpcClient> = OnceLock::new();

pub fn set_web_rpc_client(client: smol::RpcClient) {
    if WEB_RPC_CLIENT.set(client).is_err() {
        log::warn!("remote web RPC client already installed");
    }
}

fn web_rpc_client() -> Result<smol::RpcClient> {
    WEB_RPC_CLIENT.get().cloned().context(
        "ssh from the browser needs the web RPC client; \
         zed_web_workspace's init_app_state installs it with remote::set_web_rpc_client",
    )
}

pub(crate) struct WebRelayConnection {
    client: smol::RpcClient,
    executor: BackgroundExecutor,
    options: SshConnectionOptions,
    response: ConnectResponse,
    killed: Arc<AtomicBool>,
    /// Dropping it stops the loop that reattaches the handle after a `/rpc` reconnect.
    _stop_reattaching: oneshot::Sender<()>,
}

enum ConnectEvent {
    Status(Option<String>),
    Prompt { prompt_id: String, prompt: String },
    PromptCancelled { prompt_id: String },
}

fn notification_methods(connect_id: &str) -> [String; 3] {
    [
        format!("RemoteSsh::status:{connect_id}"),
        format!("RemoteSsh::prompt:{connect_id}"),
        format!("RemoteSsh::prompt_cancelled:{connect_id}"),
    ]
}

/// Lives for exactly as long as a `RemoteSsh::connect` is awaited. Dropping the connect future
/// (the connection modal was cancelled) must reach the server, which is otherwise still
/// connecting or waiting on a prompt nobody will answer.
struct PendingConnect {
    client: smol::RpcClient,
    executor: BackgroundExecutor,
    connect_id: String,
    finished: bool,
}

impl Drop for PendingConnect {
    fn drop(&mut self) {
        // `on_notification` has no way to remove a handler, so the per-connect names are
        // parked on a no-op instead.
        for method in notification_methods(&self.connect_id) {
            self.client.on_notification(&method, |_| {});
        }
        if self.finished {
            return;
        }
        let client = self.client.clone();
        let executor = self.executor.clone();
        let params = json!({"connect_id": self.connect_id});
        self.executor
            .spawn(async move {
                if let Err(error) = call_with_timeout::<Value>(
                    &client,
                    "RemoteSsh::cancel_connect",
                    &params,
                    &executor,
                )
                .await
                {
                    log::warn!("could not cancel the ssh connect: {error:#}");
                }
            })
            .detach();
    }
}

async fn call_with_timeout<R: DeserializeOwned>(
    client: &smol::RpcClient,
    method: &str,
    params: &Value,
    executor: &BackgroundExecutor,
) -> Result<R> {
    client
        .call::<_, R>(method, params)
        .with_timeout(RPC_TIMEOUT, executor)
        .await
        .map_err(|_| {
            anyhow!(
                "{method} got no answer in {} seconds",
                RPC_TIMEOUT.as_secs()
            )
        })?
}

fn release_in_background(client: &smol::RpcClient, executor: &BackgroundExecutor, handle_id: &str) {
    let client = client.clone();
    let task_executor = executor.clone();
    let params = json!({"handle_id": handle_id});
    executor
        .spawn(async move {
            if let Err(error) =
                call_with_timeout::<Value>(&client, "RemoteSsh::release", &params, &task_executor)
                    .await
            {
                log::warn!("could not release the ssh connection: {error:#}");
            }
        })
        .detach();
}

#[derive(Deserialize)]
struct OpenedChannel {
    channel_id: String,
    token: String,
}

#[derive(Deserialize)]
struct AttachedHandles {
    missing: Vec<String>,
}

impl WebRelayConnection {
    pub(crate) async fn new(
        options: SshConnectionOptions,
        delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<Self> {
        let client = web_rpc_client()?;
        let executor = cx.background_executor().clone();
        let connect_id = format!("c-{}", uuid::Uuid::new_v4().simple());

        let (events_tx, mut events) = mpsc::unbounded::<ConnectEvent>();
        let [status_method, prompt_method, prompt_cancelled_method] =
            notification_methods(&connect_id);
        // Handlers run inside the socket's onmessage with no AsyncApp, so they only forward.
        client.on_notification(&status_method, {
            let events_tx = events_tx.clone();
            move |params| {
                let status = params
                    .get("status")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                events_tx.unbounded_send(ConnectEvent::Status(status)).ok();
            }
        });
        client.on_notification(&prompt_method, {
            let events_tx = events_tx.clone();
            move |params| {
                let prompt_id = params.get("prompt_id").and_then(Value::as_str);
                let prompt = params.get("prompt").and_then(Value::as_str);
                let (Some(prompt_id), Some(prompt)) = (prompt_id, prompt) else {
                    log::error!("malformed RemoteSsh::prompt notification");
                    return;
                };
                events_tx
                    .unbounded_send(ConnectEvent::Prompt {
                        prompt_id: prompt_id.to_owned(),
                        prompt: prompt.to_owned(),
                    })
                    .ok();
            }
        });
        client.on_notification(&prompt_cancelled_method, move |params| {
            let Some(prompt_id) = params.get("prompt_id").and_then(Value::as_str) else {
                log::error!("malformed RemoteSsh::prompt_cancelled notification");
                return;
            };
            events_tx
                .unbounded_send(ConnectEvent::PromptCancelled {
                    prompt_id: prompt_id.to_owned(),
                })
                .ok();
        });
        let mut pending_connect = PendingConnect {
            client: client.clone(),
            executor: executor.clone(),
            connect_id: connect_id.clone(),
            finished: false,
        };

        // Scoped so the connect future's borrow of `client` ends before `client` moves.
        let result = {
            // The server refuses a password in the options. The delegate was built with that same
            // password as its known answer, so it still answers ssh's prompt without asking.
            let mut wire_options = options.clone();
            wire_options.password = None;
            let connect_params = json!({
                "connect_id": connect_id,
                "options": wire_options,
                "client_commit": option_env!("ZED_COMMIT_SHA"),
            });
            // No timeout: it includes however long the user takes to answer a prompt.
            let connect = client
                .call::<_, Value>("RemoteSsh::connect", &connect_params)
                .fuse();
            futures::pin_mut!(connect);

            let mut server_cancellations: HashMap<String, oneshot::Sender<()>> = HashMap::default();
            let mut prompts: FuturesUnordered<
                LocalBoxFuture<'static, Option<(String, Option<EncryptedPassword>)>>,
            > = FuturesUnordered::new();
            let mut answers: FuturesUnordered<LocalBoxFuture<'static, ()>> =
                FuturesUnordered::new();

            loop {
                futures::select_biased! {
                    result = connect => break result,
                    event = events.select_next_some() => match event {
                        ConnectEvent::Status(status) => delegate.set_status(status.as_deref(), cx),
                        ConnectEvent::Prompt { prompt_id, prompt } => {
                            let (password_tx, password_rx) = oneshot::channel();
                            let (modal_cancel_tx, modal_cancel_rx) = oneshot::channel::<()>();
                            let (server_cancel_tx, server_cancel_rx) = oneshot::channel::<()>();
                            server_cancellations.insert(prompt_id.clone(), server_cancel_tx);
                            delegate.ask_password(prompt, password_tx, modal_cancel_rx, cx);
                            prompts.push(
                                async move {
                                    let answered = futures::select_biased! {
                                        _ = server_cancel_rx.fuse() => None,
                                        password = password_rx.fuse() => Some((prompt_id, password.ok())),
                                    };
                                    // Held until now because dropping it is what clears the modal.
                                    drop(modal_cancel_tx);
                                    answered
                                }
                                .boxed_local(),
                            );
                        }
                        ConnectEvent::PromptCancelled { prompt_id } => {
                            server_cancellations.remove(&prompt_id);
                        }
                    },
                    answered = next_in_flight(&mut prompts).fuse() => {
                        let Some((prompt_id, password)) = answered else {
                            continue;
                        };
                        server_cancellations.remove(&prompt_id);
                        // A dropped sender is the user cancelling, which the server takes as null.
                        let response = match password.map(|password| {
                            password.decrypt(IKnowWhatIAmDoingAndIHaveReadTheDocs)
                        }) {
                            Some(Ok(response)) => Some(response),
                            Some(Err(error)) => {
                                log::error!("could not read the ssh prompt answer: {error:#}");
                                None
                            }
                            None => None,
                        };
                        let client = client.clone();
                        let executor = executor.clone();
                        let params = json!({"prompt_id": prompt_id, "response": response});
                        answers.push(
                            async move {
                                if let Err(error) = call_with_timeout::<Value>(
                                    &client,
                                    "RemoteSsh::answer_prompt",
                                    &params,
                                    &executor,
                                )
                                .await
                                {
                                    log::warn!("the ssh prompt answer was not delivered: {error:#}");
                                }
                            }
                            .boxed_local(),
                        );
                    }
                    () = next_in_flight(&mut answers).fuse() => {}
                }
            }
        };
        pending_connect.finished = true;
        drop(pending_connect);

        let value = result.context("could not connect over ssh through zed_web_server")?;
        let response = match parse_connect_response(&value) {
            Ok(response) => response,
            Err(error) => {
                // The server already holds a connection for this answer; nothing else would
                // ever release it.
                if let Some(handle_id) = value.get("handle_id").and_then(Value::as_str) {
                    release_in_background(&client, &executor, handle_id);
                }
                return Err(error);
            }
        };

        let killed = Arc::new(AtomicBool::new(false));
        let (stop_reattaching, stop_reattaching_rx) = oneshot::channel();
        Self::reattach_after_reconnects(
            client.clone(),
            executor.clone(),
            response.handle_id.clone(),
            killed.clone(),
            stop_reattaching_rx,
            cx,
        );

        Ok(Self {
            client,
            executor,
            options,
            response,
            killed,
            _stop_reattaching: stop_reattaching,
        })
    }

    /// The server releases a handle 30 seconds after the `/rpc` connection that owns it
    /// closes, unless a new connection claims it.
    fn reattach_after_reconnects(
        client: smol::RpcClient,
        executor: BackgroundExecutor,
        handle_id: String,
        killed: Arc<AtomicBool>,
        stop: oneshot::Receiver<()>,
        cx: &mut AsyncApp,
    ) {
        let mut reconnects = client.subscribe_reconnect();
        cx.spawn(async move |_| {
            let mut stop = stop.fuse();
            loop {
                futures::select_biased! {
                    _ = stop => return,
                    generation = reconnects.next() => {
                        if generation.is_none() || killed.load(Ordering::Acquire) {
                            return;
                        }
                    }
                }
                let params = json!({"handle_ids": [handle_id]});
                match call_with_timeout::<AttachedHandles>(
                    &client,
                    "RemoteSsh::attach_handles",
                    &params,
                    &executor,
                )
                .await
                {
                    Ok(attached) if attached.missing.contains(&handle_id) => {
                        // The server let the ssh connection go while the browser was away.
                        // Marked killed so the connection pool makes a new one on reconnect.
                        log::warn!("zed_web_server no longer holds ssh connection {handle_id}");
                        killed.store(true, Ordering::Release);
                        return;
                    }
                    Ok(_) => {}
                    Err(error) => log::warn!("could not reattach the ssh connection: {error:#}"),
                }
            }
        })
        .detach();
    }
}

impl Drop for WebRelayConnection {
    fn drop(&mut self) {
        // A desktop connection's ControlMaster dies with it; the server's does not unless told.
        if !self.killed.swap(true, Ordering::AcqRel) {
            release_in_background(&self.client, &self.executor, &self.response.handle_id);
        }
    }
}

#[async_trait(?Send)]
impl RemoteConnection for WebRelayConnection {
    fn start_proxy(
        &self,
        unique_identifier: String,
        reconnect: bool,
        incoming_tx: UnboundedSender<Envelope>,
        mut outgoing_rx: UnboundedReceiver<Envelope>,
        mut connection_activity_tx: Sender<()>,
        delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Task<Result<i32>> {
        delegate.set_status(Some("Starting proxy"), cx);
        let client = self.client.clone();
        let executor = self.executor.clone();
        let params = json!({
            "handle_id": self.response.handle_id,
            "identifier": prefixed_identifier(&unique_identifier),
            "reconnect": reconnect,
        });
        cx.spawn(async move |_| {
            let channel = call_with_timeout::<OpenedChannel>(
                &client,
                "RemoteSsh::open_channel",
                &params,
                &executor,
            )
            .await?;
            let url = format!(
                "/remote/channel?channel_id={}&token={}",
                urlencoding::encode(&channel.channel_id),
                urlencoding::encode(&channel.token)
            );
            let smol::ByteChannel {
                sender,
                mut receiver,
                closed,
            } = client.open_byte_channel(&url)?;

            // Dropping `sender` (this future finishing, or the whole task being dropped) closes
            // the socket with 1000, which makes the server cancel the proxy.
            let outgoing = async move {
                let mut frame = Vec::new();
                while let Some(envelope) = outgoing_rx.next().await {
                    frame.clear();
                    encode_frame(&envelope, &mut frame)?;
                    for chunk in frame.chunks(RELAY_CHUNK_SIZE) {
                        if sender.unbounded_send(chunk.to_vec()).is_err() {
                            // The socket is gone; its close code, read below, says why.
                            return anyhow::Ok(());
                        }
                    }
                }
                anyhow::Ok(())
            }
            .fuse();
            let incoming = async move {
                let mut framer = EnvelopeFramer::new(DEFAULT_MAX_FRAME_LEN);
                while let Some(bytes) = receiver.next().await {
                    for envelope in framer.push(&bytes)? {
                        connection_activity_tx.try_send(()).ok();
                        incoming_tx.unbounded_send(envelope).ok();
                    }
                }
                anyhow::Ok(())
            }
            .fuse();
            futures::pin_mut!(outgoing, incoming);

            // `receiver` ends only once the socket has closed, after every message before it.
            loop {
                futures::select_biased! {
                    result = incoming => {
                        result.context("reading from the ssh relay")?;
                        break;
                    }
                    result = outgoing => result.context("writing to the ssh relay")?,
                }
            }
            let close = closed
                .await
                .map_err(|_| anyhow!("relay disconnected: the ssh relay reported no close"))?;
            exit_code_from_close(close.code, &close.reason)
        })
    }

    fn upload_directory(
        &self,
        _src_path: PathBuf,
        _dest_path: RemotePathBuf,
        _cx: &App,
    ) -> Task<Result<()>> {
        Task::ready(Err(anyhow!(
            "upload_directory is not supported by the web relay yet"
        )))
    }

    async fn kill(&self) -> Result<()> {
        self.killed.store(true, Ordering::Release);
        call_with_timeout::<Value>(
            &self.client,
            "RemoteSsh::release",
            &json!({"handle_id": self.response.handle_id}),
            &self.executor,
        )
        .await?;
        Ok(())
    }

    fn has_been_killed(&self) -> bool {
        self.killed.load(Ordering::Acquire)
    }

    fn build_command(
        &self,
        program: Option<String>,
        args: &[String],
        env: &HashMap<String, String>,
        working_dir: Option<String>,
        port_forward: Option<(u16, String, u16)>,
        interactive: Interactive,
    ) -> Result<CommandTemplate> {
        build_ssh_command(
            &self.response.recipe,
            program,
            args,
            env,
            working_dir,
            port_forward,
            interactive,
        )
    }

    fn build_forward_ports_command(
        &self,
        _forwards: Vec<(u16, String, u16)>,
    ) -> Result<CommandTemplate> {
        Err(anyhow!(
            "port forwarding is not available from the browser; see docs/web-zed-plan.md §6.5"
        ))
    }

    fn connection_options(&self) -> RemoteConnectionOptions {
        RemoteConnectionOptions::Ssh(self.options.clone())
    }

    fn path_style(&self) -> PathStyle {
        self.response.path_style
    }

    fn remote_platform(&self) -> RemotePlatform {
        self.response.platform
    }

    fn remote_os_version(&self) -> Option<String> {
        self.response.os_version.clone()
    }

    fn shell(&self) -> String {
        self.response.shell.clone()
    }

    fn default_system_shell(&self) -> String {
        self.response.default_system_shell.clone()
    }

    fn has_wsl_interop(&self) -> bool {
        false
    }
}
