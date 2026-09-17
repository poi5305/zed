//! Browser half of the terminal: protocol with `zed_web_server` `Terminal::*` RPCs.
//!
//! The encode/decode helpers are compiled on every target so native tests can
//! pin the wire format against the server handler in `terminal_rpc.rs` without
//! a wasm runtime. The live RPC loop is wasm-only.

use std::borrow::Cow;

use futures::channel::mpsc::UnboundedSender;
use serde_json::{Value, json};

/// Reads a terminal id from JSON. The server sends and requires a JSON number
/// (`Value::as_u64`); some transports stringify numbers, which is still the
/// same id and must not be rejected.
pub(crate) fn parse_term_id(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64().or_else(|| {
            number
                .as_i64()
                .and_then(|signed| u64::try_from(signed).ok())
        }),
        Value::String(text) => {
            let text = text.trim();
            if text.is_empty() {
                None
            } else {
                text.parse().ok()
            }
        }
        _ => None,
    }
}

pub(crate) fn data_method(notification_id: &str) -> String {
    format!("Terminal::data:{notification_id}")
}

pub(crate) fn exit_method(notification_id: &str) -> String {
    format!("Terminal::exit:{notification_id}")
}

pub(crate) fn encode_write_params(term_id: u64, bytes: &[u8]) -> Value {
    json!({
        "term_id": term_id,
        "data": encode_bytes(bytes),
    })
}

pub(crate) fn encode_resize_params(term_id: u64, rows: u16, cols: u16) -> Value {
    json!({
        "term_id": term_id,
        "rows": rows,
        "cols": cols,
    })
}

pub(crate) fn encode_close_params(term_id: u64) -> Value {
    json!({ "term_id": term_id })
}

pub(crate) fn encode_bind_params(term_id: u64, resume_key: &str, notification_id: &str) -> Value {
    json!({
        "term_id": term_id,
        "resume_key": resume_key,
        "notification_id": notification_id,
    })
}

pub(crate) fn encode_attach_params(term_id: u64, notification_id: &str) -> Value {
    json!({
        "term_id": term_id,
        "notification_id": notification_id,
    })
}

pub(crate) fn encode_open_params(
    rows: u16,
    cols: u16,
    program: Option<&str>,
    args: &[String],
    working_directory: Option<&str>,
    env: &[(String, String)],
    notification_id: &str,
    compact_prompt: bool,
) -> Value {
    let mut env_object = serde_json::Map::new();
    for (key, value) in env {
        env_object.insert(key.clone(), Value::String(value.clone()));
    }
    let mut params = json!({
        "rows": rows,
        "cols": cols,
        "shell": {
            "program": program.unwrap_or(""),
            "args": args,
        },
        "env": env_object,
        "notification_id": notification_id,
        "compact_prompt": compact_prompt,
    });
    if let Some(directory) = working_directory {
        params["working_directory"] = json!(directory);
    }
    params
}

pub(crate) fn encode_bytes(bytes: &[u8]) -> String {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    STANDARD.encode(bytes)
}

pub(crate) fn decode_bytes(data: &str) -> Result<Vec<u8>, String> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    STANDARD.decode(data).map_err(|error| error.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenResponse {
    pub term_id: u64,
    pub resumed: bool,
    pub history: Vec<u8>,
    pub exit_status: Option<i32>,
}

pub(crate) fn parse_open_response(value: &Value) -> Result<OpenResponse, String> {
    let term_id = value
        .get("term_id")
        .and_then(parse_term_id)
        .ok_or_else(|| "missing terminal id".to_string())?;
    let resumed = value
        .get("resumed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let history = match value.get("history") {
        None => Vec::new(),
        Some(Value::Null) => Vec::new(),
        Some(Value::String(data)) => decode_bytes(data)?,
        Some(_) => return Err("history is not a string".to_string()),
    };
    Ok(OpenResponse {
        term_id,
        resumed,
        history,
        exit_status: parse_optional_i32(value.get("exit_status"), "exit_status")?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DataNotification {
    pub term_id: u64,
    pub bytes: Vec<u8>,
}

/// A malformed or oversized data frame must yield `Err` for that frame only.
/// The caller keeps the session readable.
pub(crate) fn parse_data_notification(value: &Value) -> Result<DataNotification, String> {
    let term_id = value
        .get("term_id")
        .and_then(parse_term_id)
        .ok_or_else(|| "missing terminal id".to_string())?;
    let data = value
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing data".to_string())?;
    Ok(DataNotification {
        term_id,
        bytes: decode_bytes(data)?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExitNotification {
    pub term_id: u64,
    pub status: Option<i32>,
}

pub(crate) fn parse_exit_notification(value: &Value) -> Result<ExitNotification, String> {
    let term_id = value
        .get("term_id")
        .and_then(parse_term_id)
        .ok_or_else(|| "missing terminal id".to_string())?;
    Ok(ExitNotification {
        term_id,
        status: parse_optional_i32(value.get("status"), "status")?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttachResponse {
    pub attached: bool,
    pub exit_status: Option<i32>,
}

pub(crate) fn parse_attach_response(value: &Value) -> Result<AttachResponse, String> {
    let attached = value
        .get("attached")
        .and_then(Value::as_bool)
        .ok_or_else(|| "missing attached".to_string())?;
    Ok(AttachResponse {
        attached,
        exit_status: parse_optional_i32(value.get("exit_status"), "exit_status")?,
    })
}

fn parse_optional_i32(value: Option<&Value>, field: &str) -> Result<Option<i32>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_i64()
            .and_then(|signed| i32::try_from(signed).ok())
            .or_else(|| {
                number
                    .as_u64()
                    .and_then(|unsigned| i32::try_from(unsigned).ok())
            })
            .map(Some)
            .ok_or_else(|| format!("{field} is not a number")),
        Some(_) => Err(format!("{field} is not a number")),
    }
}

/// Client-side data stream. A single bad frame must not make later frames
/// unreachable.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct RemotePtyStream {
    pub output: Vec<u8>,
    pub exited: bool,
    pub receiving: bool,
}

#[cfg(test)]
impl RemotePtyStream {
    pub fn new() -> Self {
        Self {
            output: Vec::new(),
            exited: false,
            receiving: true,
        }
    }

    pub fn on_data(&mut self, value: &Value) -> Result<(), String> {
        if !self.receiving {
            return Err("stream closed".to_string());
        }
        match parse_data_notification(value) {
            Ok(notification) => {
                self.output.extend_from_slice(&notification.bytes);
                Ok(())
            }
            Err(_) => Ok(()),
        }
    }

    pub fn on_exit(&mut self, value: &Value) -> Result<(), String> {
        let _notification = parse_exit_notification(value)?;
        self.exited = true;
        Ok(())
    }

    pub fn on_connection_dropped(&mut self) {
        self.receiving = false;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PtyCommand {
    Write(Vec<u8>),
    Resize { rows: u16, cols: u16 },
    Shutdown,
}

/// Exit condition for the write/resize/close pump. A dropped connection is not
/// by itself an exit: reconnect may still attach. Shutdown and a closed
/// command channel are the only exits.
pub(crate) fn pump_should_continue(command_channel_open: bool, saw_shutdown: bool) -> bool {
    command_channel_open && !saw_shutdown
}

pub(super) fn u16_dimension(count: usize) -> u16 {
    u16::try_from(count).unwrap_or(u16::MAX).max(1)
}

pub(crate) struct RemotePtyHandle {
    commands: UnboundedSender<PtyCommand>,
    stop: UnboundedSender<()>,
}

impl RemotePtyHandle {
    pub(crate) fn write(&self, input: impl Into<Cow<'static, [u8]>>) {
        if let Err(error) = self
            .commands
            .unbounded_send(PtyCommand::Write(input.into().into_owned()))
        {
            log::debug!("remote pty write dropped: {error}");
        }
    }

    pub(crate) fn resize(&self, bounds: crate::TerminalBounds) {
        let rows = u16_dimension(bounds.num_lines());
        let cols = u16_dimension(bounds.num_columns());
        if let Err(error) = self
            .commands
            .unbounded_send(PtyCommand::Resize { rows, cols })
        {
            log::debug!("remote pty resize dropped: {error}");
        }
    }

    pub(crate) fn shutdown(&self) {
        if let Err(error) = self.stop.unbounded_send(()) {
            log::debug!("remote pty stop dropped: {error}");
        }
        if let Err(error) = self.commands.unbounded_send(PtyCommand::Shutdown) {
            log::debug!("remote pty shutdown dropped: {error}");
        }
    }
}

#[cfg(target_family = "wasm")]
mod wasm {
    use std::{
        path::PathBuf,
        process::ExitStatus,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
    };

    use anyhow::{Context as _, Result, anyhow};
    use futures::{StreamExt as _, channel::mpsc, select_biased};
    use gpui::BackgroundExecutor;
    use parking_lot::Mutex;
    use serde_json::Value;
    use vte::ansi::{Processor, StdSyncHandler};

    use super::{
        PtyCommand, RemotePtyHandle, data_method, encode_attach_params, encode_bind_params,
        encode_close_params, encode_open_params, encode_resize_params, encode_write_params,
        exit_method, parse_attach_response, parse_data_notification, parse_exit_notification,
        parse_open_response, pump_should_continue, u16_dimension,
    };
    use crate::{PtyEvent, TerminalBackendEvent, alacritty::AlacrittyTermLock};

    static NEXT_NOTIFICATION: AtomicU64 = AtomicU64::new(1);

    pub(crate) async fn spawn_remote_pty(
        client: smol::RpcClient,
        shell_program: Option<String>,
        shell_args: Vec<String>,
        working_directory: Option<PathBuf>,
        env: impl IntoIterator<Item = (String, String)>,
        bounds: crate::TerminalBounds,
        term: Arc<AlacrittyTermLock>,
        events_tx: futures::channel::mpsc::UnboundedSender<PtyEvent>,
        executor: &BackgroundExecutor,
    ) -> Result<RemotePtyHandle> {
        let notification_id = format!(
            "browser-pty-{}",
            NEXT_NOTIFICATION.fetch_add(1, Ordering::SeqCst)
        );
        let env = env.into_iter().collect::<Vec<_>>();
        let working_directory = working_directory.map(|path| path.to_string_lossy().into_owned());
        let rows = u16_dimension(bounds.num_lines());
        let cols = u16_dimension(bounds.num_columns());
        let open_params = encode_open_params(
            rows,
            cols,
            shell_program.as_deref(),
            &shell_args,
            working_directory.as_deref(),
            &env,
            &notification_id,
            false,
        );

        let shutdown = Arc::new(AtomicBool::new(false));
        let expected_term_id = Arc::new(AtomicU64::new(0));
        let processor = Arc::new(Mutex::new(Processor::<StdSyncHandler>::new()));

        let data_handler_term = term.clone();
        let data_handler_events = events_tx.clone();
        let data_handler_processor = processor.clone();
        let data_handler_shutdown = shutdown.clone();
        let data_handler_term_id = expected_term_id.clone();
        client.on_notification(&data_method(&notification_id), move |params| {
            if data_handler_shutdown.load(Ordering::SeqCst) {
                return;
            }
            match parse_data_notification(&params) {
                Ok(notification) => {
                    let expected = data_handler_term_id.load(Ordering::SeqCst);
                    if expected != 0 && notification.term_id != expected {
                        return;
                    }
                    {
                        let mut processor = data_handler_processor.lock();
                        let mut term = data_handler_term.lock();
                        processor.advance(&mut *term, &notification.bytes);
                    }
                    if let Err(error) = data_handler_events
                        .unbounded_send(PtyEvent::Event(TerminalBackendEvent::Wakeup))
                    {
                        log::debug!("remote pty wakeup dropped: {error}");
                    }
                }
                Err(error) => {
                    log::warn!("skipping malformed Terminal::data notification: {error}");
                }
            }
        });

        let exit_handler_events = events_tx.clone();
        let exit_handler_shutdown = shutdown.clone();
        let exit_handler_term_id = expected_term_id.clone();
        client.on_notification(&exit_method(&notification_id), move |params| {
            match parse_exit_notification(&params) {
                Ok(notification) => {
                    let expected = exit_handler_term_id.load(Ordering::SeqCst);
                    if expected != 0 && notification.term_id != expected {
                        return;
                    }
                    if let Err(error) = exit_handler_events
                        .unbounded_send(PtyEvent::Event(backend_exit(notification.status)))
                    {
                        log::debug!("remote pty exit event dropped: {error}");
                    }
                }
                Err(error) => {
                    if exit_handler_shutdown.load(Ordering::SeqCst) {
                        return;
                    }
                    log::warn!("skipping malformed Terminal::exit notification: {error}");
                }
            }
        });

        let opened_value: Value = client
            .call("Terminal::open", &open_params)
            .await
            .context("Terminal::open")?;
        let opened = parse_open_response(&opened_value).map_err(|error| anyhow!("{error}"))?;
        expected_term_id.store(opened.term_id, Ordering::SeqCst);

        if opened.resumed && !opened.history.is_empty() {
            {
                let mut history_processor = processor.lock();
                let mut term = term.lock();
                history_processor.advance(&mut *term, &opened.history);
            }
            if let Err(error) =
                events_tx.unbounded_send(PtyEvent::Event(TerminalBackendEvent::Wakeup))
            {
                log::debug!("remote pty history wakeup dropped: {error}");
            }
        }
        if let Some(status) = opened.exit_status {
            if let Err(error) =
                events_tx.unbounded_send(PtyEvent::Event(backend_exit(Some(status))))
            {
                log::debug!("remote pty resumed exit dropped: {error}");
            }
        }

        let resume_key = format!("browser-pty-{}", opened.term_id);
        if let Err(error) = client
            .call_void(
                "Terminal::bind",
                &encode_bind_params(opened.term_id, &resume_key, &notification_id),
            )
            .await
        {
            log::error!("Terminal::bind failed: {error:#}");
        }

        let (commands_tx, commands_rx) = mpsc::unbounded();
        let (stop_tx, stop_rx) = mpsc::unbounded();
        spawn_write_pump(
            client.clone(),
            opened.term_id,
            commands_rx,
            shutdown.clone(),
            executor,
        );
        spawn_reconnect_loop(
            client,
            opened.term_id,
            notification_id,
            events_tx,
            shutdown,
            stop_rx,
            executor,
        );

        Ok(RemotePtyHandle {
            commands: commands_tx,
            stop: stop_tx,
        })
    }

    fn spawn_write_pump(
        client: smol::RpcClient,
        term_id: u64,
        mut commands: mpsc::UnboundedReceiver<PtyCommand>,
        shutdown: Arc<AtomicBool>,
        executor: &BackgroundExecutor,
    ) {
        executor
            .spawn(async move {
                while let Some(command) = commands.next().await {
                    if !pump_should_continue(true, shutdown.load(Ordering::SeqCst))
                        && !matches!(command, PtyCommand::Shutdown)
                    {
                        break;
                    }
                    match command {
                        PtyCommand::Write(bytes) => {
                            if let Err(error) = client
                                .call_void("Terminal::write", &encode_write_params(term_id, &bytes))
                                .await
                            {
                                log::error!("Terminal::write failed: {error:#}");
                            }
                        }
                        PtyCommand::Resize { rows, cols } => {
                            if let Err(error) = client
                                .call_void(
                                    "Terminal::resize",
                                    &encode_resize_params(term_id, rows, cols),
                                )
                                .await
                            {
                                log::error!("Terminal::resize failed: {error:#}");
                            }
                        }
                        PtyCommand::Shutdown => {
                            shutdown.store(true, Ordering::SeqCst);
                            if let Err(error) = client
                                .call_void("Terminal::close", &encode_close_params(term_id))
                                .await
                            {
                                log::error!("Terminal::close failed: {error:#}");
                            }
                            break;
                        }
                    }
                }
                if !shutdown.swap(true, Ordering::SeqCst) {
                    if let Err(error) = client
                        .call_void("Terminal::close", &encode_close_params(term_id))
                        .await
                    {
                        log::error!("Terminal::close failed: {error:#}");
                    }
                }
            })
            .detach();
    }

    fn spawn_reconnect_loop(
        client: smol::RpcClient,
        term_id: u64,
        notification_id: String,
        events_tx: futures::channel::mpsc::UnboundedSender<PtyEvent>,
        shutdown: Arc<AtomicBool>,
        mut stop_rx: mpsc::UnboundedReceiver<()>,
        executor: &BackgroundExecutor,
    ) {
        let mut reconnects = client.subscribe_reconnect().fuse();
        executor
            .spawn(async move {
                loop {
                    select_biased! {
                        stop = stop_rx.next() => {
                            let _stopped = stop;
                            break;
                        }
                        reconnect = reconnects.next() => {
                            let Some(_) = reconnect else {
                                break;
                            };
                            if shutdown.load(Ordering::SeqCst) {
                                break;
                            }
                            match client
                                .call::<_, Value>(
                                    "Terminal::attach",
                                    &encode_attach_params(term_id, &notification_id),
                                )
                                .await
                            {
                                Ok(value) => match parse_attach_response(&value) {
                                    Ok(response) if !response.attached => {
                                        if let Err(error) = events_tx.unbounded_send(PtyEvent::Event(
                                            backend_exit(response.exit_status),
                                        )) {
                                            log::debug!(
                                                "remote pty missing-on-attach event dropped: {error}"
                                            );
                                        }
                                        shutdown.store(true, Ordering::SeqCst);
                                        break;
                                    }
                                    Ok(response) => {
                                        if let Some(status) = response.exit_status
                                            && let Err(error) = events_tx.unbounded_send(
                                                PtyEvent::Event(backend_exit(Some(status))),
                                            )
                                        {
                                            log::debug!(
                                                "remote pty attach exit event dropped: {error}"
                                            );
                                        }
                                    }
                                    Err(error) => {
                                        log::error!("Terminal::attach response: {error}");
                                    }
                                },
                                Err(error) => {
                                    log::error!("Terminal::attach failed: {error:#}");
                                }
                            }
                        }
                    }
                }
            })
            .detach();
    }

    fn backend_exit(status: Option<i32>) -> TerminalBackendEvent {
        match status {
            Some(code) => {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    TerminalBackendEvent::ChildExit(ExitStatus::from_raw(code.wrapping_shl(8)))
                }
                #[cfg(not(unix))]
                {
                    if code == 0 {
                        TerminalBackendEvent::ChildExit(ExitStatus::default())
                    } else {
                        TerminalBackendEvent::Exit
                    }
                }
            }
            None => TerminalBackendEvent::Exit,
        }
    }
}

#[cfg(target_family = "wasm")]
pub(crate) use wasm::spawn_remote_pty;

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;
    use serde_json::json;

    #[test]
    fn parse_term_id_accepts_server_json_number() {
        assert_eq!(
            parse_term_id(&json!(1)),
            Some(1),
            "server Terminal::* payloads send term_id as a JSON number"
        );
    }

    #[test]
    fn parse_term_id_accepts_stringified_number() {
        assert_eq!(
            parse_term_id(&json!("1")),
            Some(1),
            "a transport that stringifies numbers still carries the same id"
        );
    }

    #[test]
    fn parse_term_id_rejects_non_numeric_string() {
        assert_eq!(parse_term_id(&json!("term-1")), None);
    }

    #[test]
    fn write_params_send_term_id_as_number_not_string() {
        let params = encode_write_params(1, b"hi");
        assert_eq!(
            params.get("term_id"),
            Some(&json!(1)),
            "server terminal_id() uses Value::as_u64; a string id is 'missing terminal id'"
        );
        assert!(params["term_id"].as_str().is_none());
        assert_eq!(params["data"], json!(encode_bytes(b"hi")));
    }

    #[test]
    fn write_params_allow_empty_payload() {
        let params = encode_write_params(7, b"");
        assert_eq!(params["term_id"], json!(7));
        assert_eq!(params["data"], json!(""));
    }

    #[test]
    fn notification_methods_match_server_bind_shape() {
        let notification_id = "persisted-workspace:1:terminal:9";
        assert_eq!(
            data_method(notification_id),
            "Terminal::data:persisted-workspace:1:terminal:9"
        );
        assert_eq!(
            exit_method(notification_id),
            "Terminal::exit:persisted-workspace:1:terminal:9"
        );
    }

    #[test]
    fn parse_open_response_reads_fresh_open_from_server() {
        let parsed =
            parse_open_response(&json!({"term_id": 1, "resumed": false})).expect("fresh open");
        assert_eq!(
            parsed,
            OpenResponse {
                term_id: 1,
                resumed: false,
                history: Vec::new(),
                exit_status: None,
            }
        );
    }

    #[test]
    fn parse_open_response_ignores_unknown_fields() {
        let parsed = parse_open_response(&json!({
            "term_id": 3,
            "resumed": false,
            "extra": true
        }))
        .expect("unknown fields are not a rejection");
        assert_eq!(parsed.term_id, 3);
    }

    #[test]
    fn parse_open_response_replays_resume_history() {
        let history = encode_bytes(b"persistent-terminal-marker");
        let parsed = parse_open_response(&json!({
            "term_id": 1,
            "resumed": true,
            "history": history,
            "exit_status": null
        }))
        .expect("resume");
        assert!(parsed.resumed);
        assert_eq!(parsed.history, b"persistent-terminal-marker");
        assert_eq!(parsed.exit_status, None);
    }

    #[test]
    fn parse_exit_notification_accepts_null_status() {
        let parsed = parse_exit_notification(&json!({"term_id": 2, "status": null}))
            .expect("null status is a legitimate wait() miss");
        assert_eq!(parsed.term_id, 2);
        assert_eq!(parsed.status, None);
    }

    #[test]
    fn parse_exit_notification_accepts_zero() {
        let parsed = parse_exit_notification(&json!({"term_id": 2, "status": 0})).expect("exit 0");
        assert_eq!(parsed.status, Some(0));
    }

    #[test]
    fn parse_attach_response_missing_terminal_is_not_an_error() {
        let parsed = parse_attach_response(&json!({"attached": false}))
            .expect("server returns attached:false for a missing id");
        assert!(!parsed.attached);
        assert_eq!(parsed.exit_status, None);
    }

    #[test]
    fn malformed_data_frame_does_not_drop_the_rest_of_the_session() {
        let mut stream = RemotePtyStream::new();
        let bad = json!({"term_id": 1, "data": "***not-base64***"});
        let good = json!({"term_id": 1, "data": encode_bytes(b"still-here")});

        stream
            .on_data(&bad)
            .expect("a malformed frame is skipped, not fatal");
        stream
            .on_data(&good)
            .expect("session must still be readable after a malformed frame");
        assert_eq!(
            stream.output, b"still-here",
            "good bytes after a bad frame must still land"
        );
        assert!(stream.receiving);
    }

    #[test]
    fn immediate_exit_marks_exited_without_spinning() {
        let mut stream = RemotePtyStream::new();
        stream
            .on_exit(&json!({"term_id": 4, "status": 0}))
            .expect("immediate exit");
        assert!(stream.exited);
        assert!(
            pump_should_continue(true, false),
            "child exit is not the write-pump exit; the panel still owns the sender"
        );
    }

    #[test]
    fn dropped_connection_is_a_reachable_stop_for_receiving() {
        let mut stream = RemotePtyStream::new();
        stream.on_connection_dropped();
        assert!(!stream.receiving);
    }

    #[test]
    fn pump_stops_on_shutdown_and_on_closed_channel() {
        assert!(
            !pump_should_continue(true, true),
            "Shutdown must stop the pump"
        );
        assert!(
            !pump_should_continue(false, false),
            "a dropped PtySender (closed channel) must stop the pump"
        );
        assert!(
            pump_should_continue(true, false),
            "a live channel with no shutdown must keep pumping"
        );
    }

    #[test]
    fn encode_open_params_keeps_empty_program_so_server_substitutes_shell() {
        let params = encode_open_params(24, 80, None, &[], None, &[], "browser-pty-1", false);
        assert_eq!(params["shell"]["program"], json!(""));
        assert_eq!(params["shell"]["args"], json!([]));
        assert_eq!(params["rows"], json!(24));
        assert_eq!(params["cols"], json!(80));
        assert_eq!(params["notification_id"], json!("browser-pty-1"));
        assert!(params.get("working_directory").is_none());
    }

    #[test]
    fn encode_bind_params_keep_colon_notification_id() {
        let params = encode_bind_params(
            1,
            "workspace:1:terminal:9",
            "persisted-workspace:1:terminal:9",
        );
        assert_eq!(params["term_id"], json!(1));
        assert!(params["term_id"].as_str().is_none());
        assert_eq!(params["resume_key"], json!("workspace:1:terminal:9"));
        assert_eq!(
            params["notification_id"],
            json!("persisted-workspace:1:terminal:9")
        );
    }

    #[test]
    fn parse_data_notification_ignores_unknown_fields() {
        let parsed = parse_data_notification(&json!({
            "term_id": 1,
            "data": encode_bytes(b"ok"),
            "extra": {"nested": true}
        }))
        .expect("unknown fields are not a rejection");
        assert_eq!(parsed.bytes, b"ok");
    }

    #[test]
    fn encode_resize_close_and_attach_send_numeric_term_id() {
        let resize = encode_resize_params(9, 24, 80);
        assert_eq!(resize["term_id"], json!(9));
        assert!(resize["term_id"].as_str().is_none());
        assert_eq!(resize["rows"], json!(24));
        assert_eq!(resize["cols"], json!(80));

        let close = encode_close_params(9);
        assert_eq!(close["term_id"], json!(9));
        assert!(close["term_id"].as_str().is_none());

        let attach = encode_attach_params(9, "browser-pty-1");
        assert_eq!(attach["term_id"], json!(9));
        assert!(attach["term_id"].as_str().is_none());
        assert_eq!(attach["notification_id"], json!("browser-pty-1"));
    }

    #[test]
    fn handle_write_queues_bytes_without_rpc() {
        let (commands, mut commands_rx) = futures::channel::mpsc::unbounded();
        let (stop, _stop_rx) = futures::channel::mpsc::unbounded();
        let handle = RemotePtyHandle { commands, stop };
        handle.write(&b"abc"[..]);
        let received = futures_lite::future::block_on(commands_rx.next());
        assert_eq!(received, Some(PtyCommand::Write(b"abc".to_vec())));
    }

    #[test]
    fn handle_resize_and_shutdown_queue_commands() {
        let (commands, mut commands_rx) = futures::channel::mpsc::unbounded();
        let (stop, mut stop_rx) = futures::channel::mpsc::unbounded();
        let handle = RemotePtyHandle { commands, stop };
        handle.resize(crate::TerminalBounds::default());
        handle.shutdown();
        let resize = futures_lite::future::block_on(commands_rx.next());
        let shutdown = futures_lite::future::block_on(commands_rx.next());
        let stopped = futures_lite::future::block_on(stop_rx.next());
        assert!(matches!(
            resize,
            Some(PtyCommand::Resize { rows: _, cols: _ })
        ));
        assert_eq!(shutdown, Some(PtyCommand::Shutdown));
        assert_eq!(stopped, Some(()));
    }
}
