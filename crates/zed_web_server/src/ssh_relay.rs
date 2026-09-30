use std::{borrow::Cow, collections::HashMap, sync::Arc};

use axum::{
    extract::{
        Query, State, WebSocketUpgrade,
        ws::{CloseFrame, Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures::{
    SinkExt as _, StreamExt as _,
    channel::{mpsc, oneshot},
    stream::SplitSink,
};
use remote::protocol::{DEFAULT_MAX_FRAME_LEN, EnvelopeFramer, encode_frame};
use rpc::proto::Envelope;
use serde_json::json;

use crate::{
    ssh_host::HostCommand,
    ssh_rpc::{ChannelRefusal, SshRpc},
};

/// Far above one 1 MiB chunk plus framing, far below the `/rpc` limit, so a peer that ignores
/// the chunking rule fails fast instead of buffering a huge message.
pub const RELAY_MAX_MESSAGE_SIZE: usize = 2 * 1024 * 1024;
pub const RELAY_CHUNK_SIZE: usize = 1024 * 1024;

pub const CLOSE_NORMAL: u16 = 1000;
pub const CLOSE_UNSUPPORTED_DATA: u16 = 1003;
pub const CLOSE_INTERNAL_ERROR: u16 = 1011;
pub const CLOSE_INVALID_TOKEN: u16 = 4400;
pub const CLOSE_ALREADY_ATTACHED: u16 = 4409;

/// RFC 6455 caps a close frame's reason at 123 bytes.
const MAX_CLOSE_REASON_LEN: usize = 123;

pub async fn channel(
    State(ssh): State<Arc<SshRpc>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if let Err(error) = ssh.enabled_host() {
        return (StatusCode::FORBIDDEN, error.to_string()).into_response();
    }
    if !crate::auth::same_origin(&headers) {
        return (StatusCode::FORBIDDEN, "cross-origin WebSocket rejected").into_response();
    }
    // A missing parameter is answered like a wrong one, with the close code the browser reads.
    let channel_id = query.get("channel_id").cloned().unwrap_or_default();
    let token = query.get("token").cloned().unwrap_or_default();
    upgrade
        .max_message_size(RELAY_MAX_MESSAGE_SIZE)
        .max_frame_size(RELAY_MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| relay(socket, ssh, channel_id, token))
        .into_response()
}

async fn relay(socket: WebSocket, ssh: Arc<SshRpc>, channel_id: String, token: String) {
    let (mut sink, stream) = socket.split();
    let claim = match ssh.claim_channel(&channel_id, &token) {
        Ok(claim) => claim,
        Err(ChannelRefusal::InvalidToken) => {
            close(&mut sink, CLOSE_INVALID_TOKEN, "invalid channel token").await;
            return;
        }
        Err(ChannelRefusal::AlreadyAttached) => {
            close(
                &mut sink,
                CLOSE_ALREADY_ATTACHED,
                "channel already attached",
            )
            .await;
            return;
        }
    };

    let (incoming_tx, incoming_rx) = mpsc::unbounded();
    let (outgoing_tx, outgoing_rx) = mpsc::unbounded();
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let (exit_tx, exit_rx) = oneshot::channel();
    let started = ssh.enabled_host().and_then(|host| {
        host.send(HostCommand::StartProxy {
            host_id: claim.host_id,
            identifier: claim.identifier,
            reconnect: claim.reconnect,
            incoming_tx,
            outgoing_rx,
            cancel: cancel_rx,
            exit: exit_tx,
        })
    });
    match started {
        Ok(()) => pump(sink, stream, incoming_rx, outgoing_tx, cancel_tx, exit_rx).await,
        Err(error) => close(&mut sink, CLOSE_INTERNAL_ERROR, &format!("{error:#}")).await,
    }
    ssh.finish_channel(&channel_id);
}

async fn pump(
    mut sink: SplitSink<WebSocket, Message>,
    mut stream: futures::stream::SplitStream<WebSocket>,
    mut incoming_rx: mpsc::UnboundedReceiver<Envelope>,
    outgoing_tx: mpsc::UnboundedSender<Envelope>,
    cancel_tx: oneshot::Sender<()>,
    mut exit_rx: oneshot::Receiver<anyhow::Result<i32>>,
) {
    let mut framer = EnvelopeFramer::new(DEFAULT_MAX_FRAME_LEN);
    let mut proxy_output_done = false;
    let failure = loop {
        tokio::select! {
            biased;
            envelope = incoming_rx.next(), if !proxy_output_done => match envelope {
                Some(envelope) => {
                    if let Err(error) = send_envelope(&mut sink, &envelope).await {
                        break Some(error);
                    }
                }
                None => proxy_output_done = true,
            },
            exit = &mut exit_rx => {
                // What the proxy wrote before it exited still belongs to the browser.
                while let Ok(envelope) = incoming_rx.try_recv() {
                    if let Err(error) = send_envelope(&mut sink, &envelope).await {
                        tracing::debug!(?error, "the browser left before the proxy's last output");
                        return;
                    }
                }
                match exit {
                    Ok(Ok(exit_code)) => {
                        let reason = json!({"exit_code": exit_code}).to_string();
                        close(&mut sink, CLOSE_NORMAL, &reason).await;
                    }
                    Ok(Err(error)) => {
                        close(&mut sink, CLOSE_INTERNAL_ERROR, &format!("{error:#}")).await;
                    }
                    Err(_) => {
                        close(&mut sink, CLOSE_INTERNAL_ERROR, "the ssh host dropped the proxy").await;
                    }
                }
                return;
            }
            message = stream.next() => match message {
                Some(Ok(Message::Binary(bytes))) => match framer.push(&bytes) {
                    Ok(envelopes) => {
                        for envelope in envelopes {
                            if outgoing_tx.unbounded_send(envelope).is_err() {
                                // The proxy stopped reading; its exit is on the way.
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        close(&mut sink, CLOSE_INTERNAL_ERROR, &format!("{error:#}")).await;
                        break None;
                    }
                },
                Some(Ok(Message::Text(_))) => {
                    close(&mut sink, CLOSE_UNSUPPORTED_DATA, "text frames are not accepted").await;
                    break None;
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) | None => break None,
                Some(Err(error)) => break Some(error.into()),
            }
        }
    };
    if let Some(error) = failure {
        tracing::debug!(?error, "ssh relay WebSocket ended");
    }
    // Dropping the proxy's io task is what kills the ssh proxy process.
    if cancel_tx.send(()).is_err() {
        tracing::debug!("the proxy had already exited when the relay closed");
    }
}

async fn send_envelope(
    sink: &mut SplitSink<WebSocket, Message>,
    envelope: &Envelope,
) -> anyhow::Result<()> {
    let mut frame = Vec::new();
    encode_frame(envelope, &mut frame)?;
    for chunk in frame.chunks(RELAY_CHUNK_SIZE) {
        sink.feed(Message::Binary(chunk.to_vec())).await?;
    }
    sink.flush().await?;
    Ok(())
}

async fn close(sink: &mut SplitSink<WebSocket, Message>, code: u16, reason: &str) {
    let frame = CloseFrame {
        code,
        reason: Cow::Owned(truncate_close_reason(reason).to_owned()),
    };
    if let Err(error) = sink.send(Message::Close(Some(frame))).await {
        tracing::debug!(?error, code, "could not send the relay's close frame");
    }
}

fn truncate_close_reason(reason: &str) -> &str {
    if reason.len() <= MAX_CLOSE_REASON_LEN {
        return reason;
    }
    let mut end = MAX_CLOSE_REASON_LEN;
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    &reason[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ssh_host::{BundleSummary, ConnectedHost, HostId, SshHostHandle},
        ssh_rpc::SshConfig,
    };
    use anyhow::{Context as _, Result};
    use axum::{Router, routing::get};
    use futures::channel::mpsc::UnboundedReceiver;
    use std::{net::SocketAddr, time::Duration};
    use tokio_tungstenite::{
        MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message as ClientMessage,
    };

    type Client = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

    struct FakeProxy {
        identifier: String,
        reconnect: bool,
        incoming_tx: mpsc::UnboundedSender<Envelope>,
        outgoing_rx: mpsc::UnboundedReceiver<Envelope>,
        cancel: oneshot::Receiver<()>,
        exit: oneshot::Sender<anyhow::Result<i32>>,
    }

    struct Harness {
        address: SocketAddr,
        ssh: Arc<SshRpc>,
        proxies: tokio::sync::mpsc::UnboundedReceiver<FakeProxy>,
    }

    fn connected_host() -> ConnectedHost {
        ConnectedHost {
            host_id: HostId("h-1".to_string()),
            platform: remote::RemotePlatform {
                os: remote::RemoteOs::Linux,
                arch: remote::RemoteArch::X86_64,
            },
            os_version: None,
            path_style: util::paths::PathStyle::Unix,
            shell: "/bin/sh".to_string(),
            default_system_shell: "/bin/sh".to_string(),
            recipe: remote::SshCommandRecipe {
                ssh_options: Vec::new(),
                destination: "devbox".to_string(),
                env: Default::default(),
                shell: "/bin/sh".to_string(),
                is_windows: false,
                path_style: remote::RecipePathStyle::Unix,
            },
        }
    }

    /// Stands in for the gpui thread: every connect succeeds, and every StartProxy is handed
    /// to the test as a `FakeProxy` it drives by hand.
    fn run_fake_host(
        mut commands: UnboundedReceiver<HostCommand>,
        proxies: tokio::sync::mpsc::UnboundedSender<FakeProxy>,
    ) {
        tokio::spawn(async move {
            while let Some(command) = commands.next().await {
                match command {
                    HostCommand::Connect { reply, .. } => {
                        if reply.send(Ok(connected_host())).is_err() {
                            panic!("the connect stopped waiting");
                        }
                    }
                    HostCommand::StartProxy {
                        identifier,
                        reconnect,
                        incoming_tx,
                        outgoing_rx,
                        cancel,
                        exit,
                        ..
                    } => {
                        if proxies
                            .send(FakeProxy {
                                identifier,
                                reconnect,
                                incoming_tx,
                                outgoing_rx,
                                cancel,
                                exit,
                            })
                            .is_err()
                        {
                            panic!("the test stopped taking proxies");
                        }
                    }
                    HostCommand::Release { .. } => {}
                }
            }
        });
    }

    async fn harness() -> Result<Harness> {
        let (host, commands) = SshHostHandle::channel(Some(BundleSummary {
            commit: "abc123".to_string(),
            platforms: vec!["linux-x86_64".to_string()],
        }));
        let (proxies_tx, proxies) = tokio::sync::mpsc::unbounded_channel();
        run_fake_host(commands, proxies_tx);
        let ssh = SshRpc::new(Some(host), SshConfig::default());
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let app = Router::new()
            .route("/remote/channel", get(channel))
            .with_state(ssh.clone());
        let server = axum::Server::from_tcp(listener)?.serve(app.into_make_service());
        tokio::spawn(async move {
            if let Err(error) = server.await {
                panic!("test server failed: {error}");
            }
        });
        Ok(Harness {
            address,
            ssh,
            proxies,
        })
    }

    impl Harness {
        async fn open_channel(&self) -> Result<(String, String)> {
            let (outgoing, _received) = tokio::sync::mpsc::unbounded_channel();
            let tab = self.ssh.connection(outgoing);
            let connected = tab
                .dispatch(
                    "RemoteSsh::connect",
                    json!({
                        "connect_id": "c-1",
                        "options": {"host": {"Hostname": "devbox"}, "upload_binary_over_ssh": false},
                    }),
                )
                .await?;
            let channel = tab
                .dispatch(
                    "RemoteSsh::open_channel",
                    json!({
                        "handle_id": connected["handle_id"],
                        "identifier": "web-dev-workspace-12",
                        "reconnect": false,
                    }),
                )
                .await?;
            Ok((
                channel["channel_id"]
                    .as_str()
                    .context("channel id")?
                    .to_string(),
                channel["token"].as_str().context("token")?.to_string(),
            ))
        }

        async fn attach(&self, channel_id: &str, token: &str) -> Result<Client> {
            let url = format!(
                "ws://{}/remote/channel?channel_id={channel_id}&token={token}",
                self.address
            );
            let (client, _response) = connect_async(url).await?;
            Ok(client)
        }

        async fn next_proxy(&mut self) -> Result<FakeProxy> {
            within(self.proxies.recv())
                .await?
                .context("no StartProxy reached the host")
        }
    }

    async fn within<T>(future: impl std::future::Future<Output = T>) -> Result<T> {
        tokio::time::timeout(Duration::from_secs(20), future)
            .await
            .context("timed out")
    }

    /// Reads until the server's close frame, returning its code and reason.
    async fn close_frame(client: &mut Client) -> Result<(u16, String)> {
        loop {
            match within(client.next()).await? {
                Some(Ok(ClientMessage::Close(Some(frame)))) => {
                    return Ok((u16::from(frame.code), frame.reason.into_owned()));
                }
                Some(Ok(ClientMessage::Close(None))) => anyhow::bail!("close frame without a code"),
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error.into()),
                None => anyhow::bail!("the socket ended without a close frame"),
            }
        }
    }

    fn big_envelope(len: usize) -> Envelope {
        Envelope {
            id: 42,
            payload: Some(rpc::proto::envelope::Payload::Error(rpc::proto::Error {
                message: "x".repeat(len),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_wrong_token_is_closed_with_4400() -> Result<()> {
        let harness = harness().await?;
        let (channel_id, _token) = harness.open_channel().await?;
        let mut client = harness.attach(&channel_id, &"0".repeat(64)).await?;
        assert_eq!(
            close_frame(&mut client).await?,
            (4400, "invalid channel token".to_string())
        );

        let mut client = harness.attach("ch-unknown", "").await?;
        assert_eq!(close_frame(&mut client).await?.0, 4400);
        Ok(())
    }

    #[tokio::test]
    async fn a_second_attach_is_closed_with_4409() -> Result<()> {
        let mut harness = harness().await?;
        let (channel_id, token) = harness.open_channel().await?;
        let _first = harness.attach(&channel_id, &token).await?;
        let proxy = harness.next_proxy().await?;
        assert_eq!(proxy.identifier, "web-dev-workspace-12");
        assert!(!proxy.reconnect);

        let mut second = harness.attach(&channel_id, &token).await?;
        assert_eq!(
            close_frame(&mut second).await?,
            (4409, "channel already attached".to_string())
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_proxy_exit_closes_with_1000_and_its_code() -> Result<()> {
        let mut harness = harness().await?;
        let (channel_id, token) = harness.open_channel().await?;
        let mut client = harness.attach(&channel_id, &token).await?;
        let proxy = harness.next_proxy().await?;

        // Written just before the exit; it must still arrive ahead of the close frame.
        let last_words = big_envelope(10);
        proxy
            .incoming_tx
            .unbounded_send(last_words.clone())
            .context("proxy output")?;
        if proxy.exit.send(Ok(90)).is_err() {
            anyhow::bail!("the relay stopped waiting for the exit");
        }

        let mut framer = EnvelopeFramer::new(DEFAULT_MAX_FRAME_LEN);
        let mut received = Vec::new();
        let close = loop {
            match within(client.next()).await? {
                Some(Ok(ClientMessage::Binary(bytes))) => received.extend(framer.push(&bytes)?),
                Some(Ok(ClientMessage::Close(Some(frame)))) => {
                    break (u16::from(frame.code), frame.reason.into_owned());
                }
                other => anyhow::bail!("unexpected {other:?}"),
            }
        };
        assert_eq!(received, vec![last_words]);
        assert_eq!(close, (1000, r#"{"exit_code":90}"#.to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn a_client_close_cancels_the_proxy() -> Result<()> {
        let mut harness = harness().await?;
        let (channel_id, token) = harness.open_channel().await?;
        let mut client = harness.attach(&channel_id, &token).await?;
        let proxy = harness.next_proxy().await?;
        client.close(None).await?;
        assert_eq!(within(proxy.cancel).await?, Ok(()));
        Ok(())
    }

    #[tokio::test]
    async fn a_text_frame_is_closed_with_1003_and_cancels_the_proxy() -> Result<()> {
        let mut harness = harness().await?;
        let (channel_id, token) = harness.open_channel().await?;
        let mut client = harness.attach(&channel_id, &token).await?;
        let proxy = harness.next_proxy().await?;
        client
            .send(ClientMessage::Text("hello".to_string()))
            .await?;
        assert_eq!(close_frame(&mut client).await?.0, 1003);
        assert_eq!(within(proxy.cancel).await?, Ok(()));
        Ok(())
    }

    #[tokio::test]
    async fn a_five_mebibyte_envelope_sent_one_byte_at_a_time_arrives_whole() -> Result<()> {
        let mut harness = harness().await?;
        let (channel_id, token) = harness.open_channel().await?;
        let mut client = harness.attach(&channel_id, &token).await?;
        let mut proxy = harness.next_proxy().await?;

        let envelope = big_envelope(5 << 20);
        let mut frame = Vec::new();
        encode_frame(&envelope, &mut frame)?;
        let started = std::time::Instant::now();
        let sender = async {
            for (index, byte) in frame.iter().enumerate() {
                client.feed(ClientMessage::Binary(vec![*byte])).await?;
                if index % 4096 == 0 {
                    client.flush().await?;
                }
            }
            client.flush().await?;
            anyhow::Ok(client)
        };
        let receiver = async {
            let received = proxy
                .outgoing_rx
                .next()
                .await
                .context("no envelope reached the proxy")?;
            anyhow::Ok((received, proxy))
        };
        let (client, (received, proxy)) = tokio::time::timeout(
            Duration::from_secs(600),
            futures::future::try_join(sender, receiver),
        )
        .await
        .context("5 MiB one byte at a time did not finish")??;
        eprintln!("5 MiB in 1-byte messages took {:?}", started.elapsed());
        assert!(received == envelope, "the proxy got a different envelope");

        // And back: the relay chunks the proxy's output, the browser reassembles it.
        proxy
            .incoming_tx
            .unbounded_send(envelope.clone())
            .context("proxy output")?;
        let mut client = client;
        let mut framer = EnvelopeFramer::new(DEFAULT_MAX_FRAME_LEN);
        let mut chunks = 0;
        let echoed = loop {
            match within(client.next()).await? {
                Some(Ok(ClientMessage::Binary(bytes))) => {
                    assert!(bytes.len() <= RELAY_CHUNK_SIZE);
                    chunks += 1;
                    let mut envelopes = framer.push(&bytes)?;
                    if let Some(envelope) = envelopes.pop() {
                        break envelope;
                    }
                }
                other => anyhow::bail!("unexpected {other:?}"),
            }
        };
        assert!(echoed == envelope, "the browser got a different envelope");
        assert_eq!(chunks, 6, "5 MiB plus framing is six 1 MiB chunks");
        Ok(())
    }

    #[tokio::test]
    async fn the_channel_route_is_forbidden_without_ssh() -> Result<()> {
        let ssh = SshRpc::new(None, SshConfig::default());
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let app = Router::new()
            .route("/remote/channel", get(channel))
            .with_state(ssh);
        let server = axum::Server::from_tcp(listener)?.serve(app.into_make_service());
        tokio::spawn(async move {
            if let Err(error) = server.await {
                panic!("test server failed: {error}");
            }
        });
        let result = connect_async(format!(
            "ws://{address}/remote/channel?channel_id=x&token=y"
        ))
        .await;
        match result {
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                assert_eq!(response.status().as_u16(), 403);
            }
            other => anyhow::bail!("expected a 403, got {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn close_reasons_fit_a_close_frame() {
        let long = "é".repeat(100);
        let truncated = truncate_close_reason(&long);
        assert!(truncated.len() <= MAX_CLOSE_REASON_LEN);
        assert!(long.starts_with(truncated));
        assert_eq!(truncate_close_reason("short"), "short");
    }
}
