use crate::{
    RemoteArch, RemoteClientDelegate, RemoteOs, RemotePlatform,
    remote_client::{CommandTemplate, Interactive, RemoteConnection, RemoteConnectionOptions},
    transport::{parse_platform, parse_shell},
};
use anyhow::{Context as _, Result, anyhow};
use async_trait::async_trait;
use collections::HashMap;
use futures::{
    AsyncReadExt as _,
    channel::mpsc::{Sender, UnboundedReceiver, UnboundedSender},
};
#[cfg(not(target_family = "wasm"))]
use futures::{FutureExt as _, select_biased};
use gpui::{App, AppContext as _, AsyncApp, Task};
use parking_lot::Mutex;
use paths::remote_server_dir_relative;
use release_channel::{AppVersion, ReleaseChannel};
use rpc::proto::Envelope;
use semver::Version;
pub use settings::SshPortForwardOption;
use smol::fs;
use std::{
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tempfile::TempDir;
use util::command::{Child, Stdio};
use util::{
    paths::{PathStyle, RemotePathBuf},
    rel_path::RelPath,
    shell::ShellKind,
};
use web_time::Instant;

/// How long to wait for SSH to connect when no askpass prompt has opened.
const SSH_CONNECTION_PROMPT_TIMEOUT: Duration = Duration::from_secs(17);

pub(crate) struct SshRemoteConnection {
    socket: SshSocket,
    master_process: Mutex<Option<MasterProcess>>,
    /// Whether `kill()` has been called. Separate from `master_process` because
    /// reused ControlMaster sessions start with `master_process` as `None`.
    killed: AtomicBool,
    remote_binary_path: Option<Arc<RelPath>>,
    ssh_platform: RemotePlatform,
    ssh_os_version: Option<String>,
    ssh_path_style: PathStyle,
    ssh_shell: String,
    ssh_shell_kind: ShellKind,
    ssh_default_system_shell: String,
    _temp_dir: TempDir,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SshConnectionHost {
    IpAddr(IpAddr),
    Hostname(String),
}

impl SshConnectionHost {
    pub fn to_bracketed_string(&self) -> String {
        match self {
            Self::IpAddr(IpAddr::V4(ip)) => ip.to_string(),
            Self::IpAddr(IpAddr::V6(ip)) => format!("[{}]", ip),
            Self::Hostname(hostname) => hostname.clone(),
        }
    }

    pub fn to_string(&self) -> String {
        match self {
            Self::IpAddr(ip) => ip.to_string(),
            Self::Hostname(hostname) => hostname.clone(),
        }
    }
}

impl From<&str> for SshConnectionHost {
    fn from(value: &str) -> Self {
        if let Ok(address) = value.parse() {
            Self::IpAddr(address)
        } else {
            Self::Hostname(value.to_string())
        }
    }
}

impl From<String> for SshConnectionHost {
    fn from(value: String) -> Self {
        if let Ok(address) = value.parse() {
            Self::IpAddr(address)
        } else {
            Self::Hostname(value)
        }
    }
}

impl Default for SshConnectionHost {
    fn default() -> Self {
        Self::Hostname(Default::default())
    }
}

fn bracket_ipv6(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{}]", host)
    } else {
        host.to_string()
    }
}

// Quote paths for sftp batch parsing and double backslashes for POSIX glob();
// Win32-OpenSSH accepts the same encoding.
fn escape_sftp_path(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len() + 2);
    escaped.push('"');
    for character in path.chars() {
        if character == '"' || character == '\\' {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped.push('"');
    escaped
}

fn sftp_put_command(source_path: &str, destination_path: &str) -> String {
    let source = escape_sftp_path(source_path);
    let destination = escape_sftp_path(destination_path);
    format!("put {source} {destination}\n")
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SshConnectionOptions {
    pub host: SshConnectionHost,
    pub username: Option<String>,
    pub port: Option<u16>,
    pub password: Option<String>,
    pub args: Option<Vec<String>>,
    pub port_forwards: Option<Vec<SshPortForwardOption>>,
    pub connection_timeout: Option<u16>,

    pub nickname: Option<String>,
    pub upload_binary_over_ssh: bool,
}

impl From<settings::SshConnection> for SshConnectionOptions {
    fn from(val: settings::SshConnection) -> Self {
        SshConnectionOptions {
            host: val.host.to_string().into(),
            username: val.username,
            port: val.port,
            password: None,
            args: Some(val.args),
            nickname: val.nickname,
            upload_binary_over_ssh: val.upload_binary_over_ssh.unwrap_or_default(),
            port_forwards: val.port_forwards,
            connection_timeout: val.connection_timeout,
        }
    }
}

struct SshSocket {
    connection_options: SshConnectionOptions,
    #[cfg(not(windows))]
    socket_path: std::path::PathBuf,
    /// Extra environment variables needed for the ssh process
    envs: HashMap<String, String>,
    #[cfg(windows)]
    _proxy: askpass::PasswordProxy,
}

struct MasterProcess {
    process: Child,
}

#[cfg(not(windows))]
impl MasterProcess {
    pub fn new(
        askpass_script_path: &std::ffi::OsStr,
        additional_args: Vec<String>,
        socket_path: &std::path::Path,
        destination: &str,
    ) -> Result<Self> {
        let args = [
            "-N",
            "-o",
            "ControlPersist=no",
            "-o",
            "ControlMaster=yes",
            "-o",
        ];

        let mut master_process = util::command::new_command("ssh");
        master_process
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env("SSH_ASKPASS", askpass_script_path)
            .args(additional_args)
            .args(args);

        master_process.arg(format!("ControlPath={}", socket_path.display()));

        let process = master_process.arg(&destination).spawn()?;

        Ok(MasterProcess { process })
    }

    pub async fn wait_connected(&mut self) -> Result<()> {
        let Some(mut stdout) = self.process.stdout.take() else {
            anyhow::bail!("ssh process stdout capture failed");
        };

        let mut output = Vec::new();
        stdout.read_to_end(&mut output).await?;
        Ok(())
    }
}

#[cfg(windows)]
impl MasterProcess {
    const CONNECTION_ESTABLISHED_MAGIC: &str = "ZED_SSH_CONNECTION_ESTABLISHED";

    pub fn new(
        askpass_script_path: &std::ffi::OsStr,
        askpass_socket_path: &std::ffi::OsStr,
        additional_args: Vec<String>,
        destination: &str,
    ) -> Result<Self> {
        // On Windows, `ControlMaster` and `ControlPath` are not supported:
        // https://github.com/PowerShell/Win32-OpenSSH/issues/405
        // https://github.com/PowerShell/Win32-OpenSSH/wiki/Project-Scope
        //
        // Using an ugly workaround to detect connection establishment
        // -N doesn't work with JumpHosts as windows openssh never closes stdin in that case
        let args = [
            "-t",
            &format!("echo '{}'; exec $0", Self::CONNECTION_ESTABLISHED_MAGIC),
        ];

        let mut master_process = util::command::new_command("ssh");
        master_process
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env("SSH_ASKPASS", askpass_script_path)
            .env("ZED_ASKPASS_SOCKET", askpass_socket_path)
            .args(additional_args)
            .arg(destination)
            .args(args);

        let process = master_process.spawn()?;

        Ok(MasterProcess { process })
    }

    pub async fn wait_connected(&mut self) -> Result<()> {
        use smol::io::AsyncBufReadExt;

        let Some(stdout) = self.process.stdout.take() else {
            anyhow::bail!("ssh process stdout capture failed");
        };

        let mut reader = smol::io::BufReader::new(stdout);

        let mut line = String::new();

        loop {
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                anyhow::bail!("ssh process exited before connection established");
            }

            if line.contains(Self::CONNECTION_ESTABLISHED_MAGIC) {
                return Ok(());
            }
        }
    }
}

/// How long a master that closed its stdout gets to exit before it counts as connected.
#[cfg(not(target_family = "wasm"))]
const MASTER_EXIT_GRACE: Duration = Duration::from_millis(200);

/// The master closes its stdout both once it has authenticated and when it exits, and a
/// master that exits right after (a wrong password) is often not reaped yet when stdout
/// reaches EOF, so its status is awaited for `grace` instead of only polled.
#[cfg(not(target_family = "wasm"))]
async fn master_exit_status(
    process: &mut Child,
    grace: impl std::future::Future<Output = ()>,
) -> Result<Option<std::process::ExitStatus>> {
    select_biased! {
        status = process.status().fuse() => Ok(Some(status?)),
        _ = grace.fuse() => Ok(None),
    }
}

impl AsRef<Child> for MasterProcess {
    fn as_ref(&self) -> &Child {
        &self.process
    }
}

impl AsMut<Child> for MasterProcess {
    fn as_mut(&mut self) -> &mut Child {
        &mut self.process
    }
}

#[async_trait(?Send)]
impl RemoteConnection for SshRemoteConnection {
    async fn kill(&self) -> Result<()> {
        self.killed.store(true, Ordering::Release);
        let Some(mut process) = self.master_process.lock().take() else {
            log::debug!("no master process to kill (external ControlMaster session)");
            return Ok(());
        };
        process.as_mut().kill().ok();
        process.as_mut().status().await?;
        Ok(())
    }

    fn has_been_killed(&self) -> bool {
        self.killed.load(Ordering::Acquire)
    }

    fn connection_options(&self) -> RemoteConnectionOptions {
        RemoteConnectionOptions::Ssh(self.socket.connection_options.clone())
    }

    fn shell(&self) -> String {
        self.ssh_shell.clone()
    }

    fn default_system_shell(&self) -> String {
        self.ssh_default_system_shell.clone()
    }

    fn build_command(
        &self,
        input_program: Option<String>,
        input_args: &[String],
        input_env: &HashMap<String, String>,
        working_dir: Option<String>,
        port_forward: Option<(u16, String, u16)>,
        interactive: Interactive,
    ) -> Result<CommandTemplate> {
        build_ssh_command(
            &self.recipe(),
            input_program,
            input_args,
            input_env,
            working_dir,
            port_forward,
            interactive,
        )
    }

    fn command_recipe(&self) -> Option<SshCommandRecipe> {
        Some(self.recipe())
    }

    fn build_forward_ports_command(
        &self,
        forwards: Vec<(u16, String, u16)>,
    ) -> Result<CommandTemplate> {
        let Self { socket, .. } = self;
        let mut args = socket.ssh_command_options();
        args.push("-N".into());
        for (local_port, host, remote_port) in forwards {
            args.push("-L".into());
            args.push(format!(
                "{}:{}:{}",
                local_port,
                bracket_ipv6(&host),
                remote_port
            ));
        }
        args.push(socket.connection_options.ssh_destination());
        Ok(CommandTemplate {
            program: "ssh".into(),
            args,
            env: Default::default(),
        })
    }

    fn upload_directory(
        &self,
        src_path: PathBuf,
        dest_path: RemotePathBuf,
        cx: &App,
    ) -> Task<Result<()>> {
        let dest_path_str = dest_path.to_string();
        let src_path_display = src_path.display().to_string();

        let mut sftp_command = self.build_sftp_command();
        sftp_command.kill_on_drop(true);
        let mut scp_command =
            self.build_scp_command(&src_path, &dest_path_str, Some(&["-C", "-r"]));
        scp_command.kill_on_drop(true);

        cx.background_spawn(async move {
            // We will try SFTP first, and if that fails, we will fall back to SCP.
            // If SCP fails also, we give up and return an error.
            // The reason we allow a fallback from SFTP to SCP is that if the user has to specify a password,
            // depending on the implementation of SSH stack, SFTP may disable interactive password prompts in batch mode.
            // This is for example the case on Windows as evidenced by this implementation snippet:
            // https://github.com/PowerShell/openssh-portable/blob/b8c08ef9da9450a94a9c5ef717d96a7bd83f3332/sshconnect2.c#L417
            if Self::is_sftp_available().await {
                log::debug!("using SFTP for directory upload");
                let mut child = sftp_command.spawn()?;
                if let Some(mut stdin) = child.stdin.take() {
                    use futures::AsyncWriteExt;
                    let sftp_batch = format!("put -r \"{src_path_display}\" \"{dest_path_str}\"\n");
                    stdin.write_all(sftp_batch.as_bytes()).await?;
                    stdin.flush().await?;
                }

                let output = child.output().await?;
                if output.status.success() {
                    return Ok(());
                }

                let stderr = String::from_utf8_lossy(&output.stderr);
                log::debug!("failed to upload directory via SFTP {src_path_display} -> {dest_path_str}: {stderr}");
            }

            log::debug!("using SCP for directory upload");
            let output = scp_command.output().await?;

            if output.status.success() {
                return Ok(());
            }

            let stderr = String::from_utf8_lossy(&output.stderr);
            log::debug!("failed to upload directory via SCP {src_path_display} -> {dest_path_str}: {stderr}");

            anyhow::bail!(
                "failed to upload directory via SFTP/SCP {} -> {}: {}",
                src_path_display,
                dest_path_str,
                stderr,
            );
        })
    }

    fn start_proxy(
        &self,
        unique_identifier: String,
        reconnect: bool,
        incoming_tx: UnboundedSender<Envelope>,
        outgoing_rx: UnboundedReceiver<Envelope>,
        connection_activity_tx: Sender<()>,
        delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Task<Result<i32>> {
        const VARS: [&str; 3] = ["RUST_LOG", "RUST_BACKTRACE", "ZED_GENERATE_MINIDUMPS"];
        delegate.set_status(Some("Starting proxy"), cx);

        let Some(remote_binary_path) = self.remote_binary_path.clone() else {
            return Task::ready(Err(anyhow!("Remote binary path not set")));
        };

        let mut ssh_command = if self.ssh_platform.os.is_windows() {
            // TODO: Set the `VARS` environment variables, we do not have `env` on windows
            // so this needs a different approach
            let mut proxy_args = vec![];
            proxy_args.push("proxy".to_owned());
            proxy_args.push("--identifier".to_owned());
            proxy_args.push(unique_identifier);

            if reconnect {
                proxy_args.push("--reconnect".to_owned());
            }
            self.socket.ssh_command(
                self.ssh_shell_kind,
                &remote_binary_path.display(self.path_style()),
                &proxy_args,
                false,
            )
        } else {
            let mut proxy_args = vec![];
            for env_var in VARS {
                if let Some(value) = std::env::var(env_var).ok() {
                    proxy_args.push(format!("{env_var}={value}"));
                }
            }
            proxy_args.push(remote_binary_path.display(self.path_style()).into_owned());
            proxy_args.push("proxy".to_owned());
            proxy_args.push("--identifier".to_owned());
            proxy_args.push(unique_identifier);

            if reconnect {
                proxy_args.push("--reconnect".to_owned());
            }
            self.socket
                .ssh_command(self.ssh_shell_kind, "env", &proxy_args, false)
        };

        let ssh_proxy_process = match ssh_command
            // IMPORTANT: we kill this process when we drop the task that uses it.
            .kill_on_drop(true)
            .spawn()
        {
            Ok(process) => process,
            Err(error) => {
                return Task::ready(Err(
                    anyhow::Error::new(error).context("failed to spawn remote server")
                ));
            }
        };

        super::handle_rpc_messages_over_child_process_stdio(
            ssh_proxy_process,
            incoming_tx,
            outgoing_rx,
            connection_activity_tx,
            cx,
        )
    }

    fn path_style(&self) -> PathStyle {
        self.ssh_path_style
    }

    fn remote_platform(&self) -> RemotePlatform {
        self.ssh_platform
    }

    fn remote_os_version(&self) -> Option<String> {
        self.ssh_os_version.clone()
    }

    fn has_wsl_interop(&self) -> bool {
        false
    }
}

/// Check if the user already has an active SSH ControlMaster session for the
/// given destination. See: https://github.com/zed-industries/zed/issues/45271
#[cfg(not(windows))]
async fn find_existing_control_master(
    destination: &str,
    additional_args: &[String],
) -> Option<PathBuf> {
    // Use `ssh -G` to resolve the user's effective SSH config for this host.
    // This expands ControlPath tokens (%h, %p, %r, %C, etc.) into actual paths.
    let output = match util::command::new_command("ssh")
        .args(additional_args)
        .arg("-G")
        .arg(destination)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
    {
        Ok(output) => output,
        Err(e) => {
            log::debug!("failed to run ssh -G: {e}");
            return None;
        }
    };

    if !output.status.success() {
        log::debug!("ssh -G failed for {destination}, skipping ControlMaster reuse");
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let control_path = stdout.lines().find_map(|line| {
        let path = line.strip_prefix("controlpath ")?.trim();
        if path == "none" || path.is_empty() {
            None
        } else {
            Some(PathBuf::from(path))
        }
    })?;

    // Verify the master is actually alive by sending a control command.
    let check = match util::command::new_command("ssh")
        .args(additional_args)
        .args(["-O", "check"])
        .arg("-o")
        .arg(format!("ControlPath={}", control_path.display()))
        .arg(destination)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
    {
        Ok(output) => output,
        Err(e) => {
            log::debug!("failed to run ssh -O check: {e}");
            return None;
        }
    };

    if check.status.success() {
        log::info!(
            "reusing existing SSH ControlMaster at {}",
            control_path.display()
        );
        Some(control_path)
    } else {
        log::debug!(
            "ControlMaster socket at {} is not alive, creating new connection",
            control_path.display()
        );
        None
    }
}

impl SshRemoteConnection {
    /// Not on wasm: the browser reaches ssh through `transport::web_relay`, and the binary
    /// deployment this leads to calls `std::process::id()`, which panics there.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) async fn new(
        connection_options: SshConnectionOptions,
        delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<Self> {
        use askpass::AskPassResult;

        let destination = connection_options.ssh_destination();

        let temp_dir = tempfile::Builder::new()
            .prefix("zed-ssh-session")
            .tempdir()?;

        // On non-Windows, check if the user already has an active ControlMaster
        // session for this host. If so, reuse it instead of prompting for auth.
        #[cfg(not(windows))]
        let reused_socket =
            find_existing_control_master(&destination, &connection_options.additional_args()).await;

        #[cfg(not(windows))]
        let (socket, master_process_option) = if let Some(reused_path) = reused_socket {
            delegate.set_status(Some("Connecting (reusing session)"), cx);
            log::info!("reusing existing ControlMaster, skipping authentication");
            let socket = SshSocket::new(connection_options, reused_path).await?;
            (socket, None)
        } else {
            let askpass_delegate = askpass::AskPassDelegate::new_with_cancellation(cx, {
                let delegate = delegate.clone();
                move |prompt, tx, cancellation, cx| {
                    delegate.ask_password(prompt, tx, cancellation, cx)
                }
            });

            let mut askpass =
                askpass::AskPassSession::new(cx.background_executor().clone(), askpass_delegate)
                    .await?;

            delegate.set_status(Some("Connecting"), cx);

            // Start the master SSH process, which does not do anything except
            // for establish the connection and keep it open, allowing other ssh
            // commands to reuse it via a control socket.
            let socket_path = temp_dir.path().join("ssh.sock");
            let mut master_process = MasterProcess::new(
                askpass.script_path().as_ref(),
                connection_options.additional_args(),
                &socket_path,
                &destination,
            )?;

            let result = select_biased! {
                result = askpass.run(Some(SSH_CONNECTION_PROMPT_TIMEOUT)).fuse() => {
                    match result {
                        AskPassResult::CancelledByUser => {
                            master_process.as_mut().kill().ok();
                            anyhow::bail!("SSH connection canceled")
                        }
                        AskPassResult::Timedout => {
                            anyhow::bail!("connecting to host timed out")
                        }
                    }
                }
                _ = master_process.wait_connected().fuse() => {
                    anyhow::Ok(())
                }
            };

            if let Err(e) = result {
                return Err(e.context("Failed to connect to host"));
            }

            if master_exit_status(
                master_process.as_mut(),
                cx.background_executor().timer(MASTER_EXIT_GRACE),
            )
            .await?
            .is_some()
            {
                let mut output = Vec::new();
                let mut stderr = master_process.as_mut().stderr.take().unwrap();
                stderr.read_to_end(&mut output).await?;

                let error_message = format!(
                    "failed to connect: {}",
                    String::from_utf8_lossy(&output).trim()
                );
                anyhow::bail!(error_message);
            }

            let socket = SshSocket::new(connection_options, socket_path).await?;
            drop(askpass);
            (socket, Some(master_process))
        };

        #[cfg(windows)]
        let (socket, master_process_option) = {
            let askpass_delegate = askpass::AskPassDelegate::new_with_cancellation(cx, {
                let delegate = delegate.clone();
                move |prompt, tx, cancellation, cx| {
                    delegate.ask_password(prompt, tx, cancellation, cx)
                }
            });

            let mut askpass =
                askpass::AskPassSession::new(cx.background_executor().clone(), askpass_delegate)
                    .await?;

            delegate.set_status(Some("Connecting"), cx);

            let mut master_process = MasterProcess::new(
                askpass.script_path().as_ref(),
                askpass.socket_path().as_ref(),
                connection_options.additional_args(),
                &destination,
            )?;

            let result = select_biased! {
                result = askpass.run(Some(SSH_CONNECTION_PROMPT_TIMEOUT)).fuse() => {
                    match result {
                        AskPassResult::CancelledByUser => {
                            master_process.as_mut().kill().ok();
                            anyhow::bail!("SSH connection canceled")
                        }
                        AskPassResult::Timedout => {
                            anyhow::bail!("connecting to host timed out")
                        }
                    }
                }
                _ = master_process.wait_connected().fuse() => {
                    anyhow::Ok(())
                }
            };

            if let Err(e) = result {
                return Err(e.context("Failed to connect to host"));
            }

            if master_exit_status(
                master_process.as_mut(),
                cx.background_executor().timer(MASTER_EXIT_GRACE),
            )
            .await?
            .is_some()
            {
                let mut output = Vec::new();
                let mut stderr = master_process.as_mut().stderr.take().unwrap();
                stderr.read_to_end(&mut output).await?;

                let error_message = format!(
                    "failed to connect: {}",
                    String::from_utf8_lossy(&output).trim()
                );
                anyhow::bail!(error_message);
            }

            let socket = SshSocket::new(
                connection_options,
                askpass
                    .get_password()
                    .or_else(|| askpass::EncryptedPassword::try_from("").ok())
                    .context("Failed to fetch askpass password")?,
                cx.background_executor().clone(),
            )
            .await?;
            drop(askpass);

            (socket, Some(master_process))
        };

        let is_windows = socket.probe_is_windows().await;
        log::info!("Remote is windows: {}", is_windows);

        let ssh_shell = socket.shell(is_windows).await;
        log::info!("Remote shell discovered: {}", ssh_shell);

        let ssh_shell_kind = ShellKind::new(&ssh_shell, is_windows);
        let ssh_platform = socket.platform(ssh_shell_kind, is_windows).await?;
        log::info!("Remote platform discovered: {:?}", ssh_platform);

        let ssh_os_version = socket.os_version(ssh_platform.os, ssh_shell_kind).await;
        log::info!("Remote OS version discovered: {:?}", ssh_os_version);

        let (ssh_path_style, ssh_default_system_shell) = match ssh_platform.os {
            RemoteOs::Windows => (PathStyle::Windows, ssh_shell.clone()),
            _ => (PathStyle::Unix, String::from("/bin/sh")),
        };

        let mut this = Self {
            socket,
            master_process: Mutex::new(master_process_option),
            killed: AtomicBool::new(false),
            _temp_dir: temp_dir,
            remote_binary_path: None,
            ssh_path_style,
            ssh_platform,
            ssh_os_version,
            ssh_shell,
            ssh_shell_kind,
            ssh_default_system_shell,
        };

        let (release_channel, version) =
            cx.update(|cx| (ReleaseChannel::global(cx), AppVersion::global(cx)));
        this.remote_binary_path = Some(
            this.ensure_server_binary(&delegate, release_channel, version, cx)
                .await?,
        );

        Ok(this)
    }

    fn recipe(&self) -> SshCommandRecipe {
        SshCommandRecipe {
            ssh_options: self.socket.ssh_command_options(),
            destination: self.socket.connection_options.ssh_destination(),
            env: self.socket.envs.clone(),
            shell: self.ssh_shell.clone(),
            is_windows: self.ssh_platform.os.is_windows(),
            path_style: self.ssh_path_style.into(),
        }
    }

    async fn ensure_server_binary(
        &self,
        delegate: &Arc<dyn RemoteClientDelegate>,
        release_channel: ReleaseChannel,
        version: Version,
        cx: &mut AsyncApp,
    ) -> Result<Arc<RelPath>> {
        #[cfg(not(target_family = "wasm"))]
        if let Some(bundle) = super::bundled_remote_server(self.ssh_platform) {
            return self
                .ensure_bundled_server_binary(bundle, delegate, cx)
                .await;
        }

        let version_str = match release_channel {
            ReleaseChannel::Dev => "build".to_string(),
            _ => version.to_string(),
        };
        let binary_name = format!(
            "zed-remote-server-{}-{}{}",
            release_channel.dev_name(),
            version_str,
            if self.ssh_platform.os.is_windows() {
                ".exe"
            } else {
                ""
            }
        );
        let dst_path =
            paths::remote_server_dir_relative().join(RelPath::from_unix_str(&binary_name).unwrap());

        let binary_exists_on_server = self
            .socket
            .run_command(
                self.ssh_shell_kind,
                &dst_path.display(self.path_style()),
                &["version"],
                true,
            )
            .await
            .is_ok();

        #[cfg(any(debug_assertions, feature = "build-remote-server-binary"))]
        if let Some(remote_server_path) = super::build_remote_server_from_source(
            &self.ssh_platform,
            delegate.as_ref(),
            binary_exists_on_server,
            cx,
        )
        .await?
        {
            let tmp_path = paths::remote_server_dir_relative().join(
                RelPath::from_unix_str(&format!(
                    "download-{}-{}",
                    std::process::id(),
                    remote_server_path.file_name().unwrap().to_string_lossy()
                ))
                .unwrap(),
            );
            self.upload_local_server_binary(&remote_server_path, &tmp_path, delegate, cx)
                .await?;
            self.extract_server_binary(&dst_path, &tmp_path, delegate, cx)
                .await?;
            return Ok(dst_path.into());
        }

        if binary_exists_on_server {
            return Ok(dst_path.into());
        }

        let wanted_version = cx.update(|cx| match release_channel {
            ReleaseChannel::Nightly => Ok(None),
            ReleaseChannel::Dev => {
                anyhow::bail!(
                    "ZED_BUILD_REMOTE_SERVER is not set and no remote server exists at ({:?})",
                    dst_path
                )
            }
            _ => Ok(Some(AppVersion::global(cx))),
        })?;

        let tmp_path_compressed = remote_server_dir_relative().join(
            RelPath::from_unix_str(&format!(
                "{}-download-{}.{}",
                binary_name,
                std::process::id(),
                if self.ssh_platform.os.is_windows() {
                    "zip"
                } else {
                    "gz"
                }
            ))
            .unwrap(),
        );
        if !self.socket.connection_options.upload_binary_over_ssh
            && let Some(url) = delegate
                .get_download_url(
                    self.ssh_platform,
                    release_channel,
                    wanted_version.clone(),
                    cx,
                )
                .await?
        {
            match self
                .download_binary_on_server(&url, &tmp_path_compressed, delegate, cx)
                .await
            {
                Ok(_) => {
                    self.extract_server_binary(&dst_path, &tmp_path_compressed, delegate, cx)
                        .await
                        .context("extracting server binary")?;
                    return Ok(dst_path.into());
                }
                Err(e) => {
                    log::error!(
                        "Failed to download binary on server, attempting to download locally and then upload it the server: {e:#}",
                    )
                }
            }
        }

        let src_path = delegate
            .download_server_binary_locally(
                self.ssh_platform,
                release_channel,
                wanted_version.clone(),
                cx,
            )
            .await
            .context("downloading server binary locally")?;
        self.upload_local_server_binary(&src_path, &tmp_path_compressed, delegate, cx)
            .await
            .context("uploading server binary")?;
        self.extract_server_binary(&dst_path, &tmp_path_compressed, delegate, cx)
            .await
            .context("extracting server binary")?;
        Ok(dst_path.into())
    }

    #[cfg(not(target_family = "wasm"))]
    async fn ensure_bundled_server_binary(
        &self,
        bundle: Result<super::BundledRemoteServer>,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<Arc<RelPath>> {
        let bundle = bundle?;
        let binary_name = bundled_server_binary_name(&bundle.content_id, self.ssh_platform.os)?;
        let dst_path = remote_server_dir_relative().join(RelPath::from_unix_str(&binary_name)?);
        let dst_display = dst_path.display(self.path_style()).into_owned();

        let installed_version = self
            .socket
            .run_command(self.ssh_shell_kind, &dst_display, &["version"], true)
            .await;
        if let Ok(output) = &installed_version
            && bundled_version_matches(output, &bundle.version)
        {
            return Ok(dst_path.into());
        }

        // The pid alone is not enough: one web server can run two connects to the same host at
        // once, and both would upload to the same temporary file.
        let tmp_name = format!(
            "{binary_name}-upload-{}-{:016x}",
            std::process::id(),
            upload_nonce()
        );
        let tmp_path = remote_server_dir_relative().join(RelPath::from_unix_str(&tmp_name)?);
        self.upload_local_server_binary(&bundle.path, &tmp_path, delegate, cx)
            .await
            .context("uploading bundled server binary")?;
        self.extract_server_binary(&dst_path, &tmp_path, delegate, cx)
            .await
            .context("installing bundled server binary")?;

        let uploaded_version = self
            .socket
            .run_command(self.ssh_shell_kind, &dst_display, &["version"], true)
            .await
            .context("running the uploaded remote server binary")?;
        anyhow::ensure!(
            bundled_version_matches(&uploaded_version, &bundle.version),
            "the uploaded remote server reports version {:?}, expected {:?}",
            uploaded_version.trim(),
            bundle.version
        );
        Ok(dst_path.into())
    }

    async fn download_binary_on_server(
        &self,
        url: &str,
        tmp_path: &RelPath,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        if let Some(parent) = tmp_path.parent() {
            let res = self
                .socket
                .run_command(
                    self.ssh_shell_kind,
                    "mkdir",
                    &["-p", parent.display(self.path_style()).as_ref()],
                    true,
                )
                .await;
            if !self.ssh_platform.os.is_windows() {
                // mkdir fails on windows if the path already exists ...
                res?;
            }
        }

        delegate.set_status(Some("Downloading remote development server on host"), cx);

        let connection_timeout = self
            .socket
            .connection_options
            .connection_timeout
            .unwrap_or(10)
            .to_string();

        match self
            .socket
            .run_command(
                self.ssh_shell_kind,
                "curl",
                &[
                    "-f",
                    "-L",
                    "--connect-timeout",
                    &connection_timeout,
                    url,
                    "-o",
                    &tmp_path.display(self.path_style()),
                ],
                true,
            )
            .await
        {
            Ok(_) => {}
            Err(e) => {
                if self
                    .socket
                    .run_command(self.ssh_shell_kind, "which", &["curl"], true)
                    .await
                    .is_ok()
                {
                    return Err(e);
                }

                log::info!("curl is not available, trying wget");
                match self
                    .socket
                    .run_command(
                        self.ssh_shell_kind,
                        "wget",
                        &[
                            "--connect-timeout",
                            &connection_timeout,
                            "--tries",
                            "1",
                            url,
                            "-O",
                            &tmp_path.display(self.path_style()),
                        ],
                        true,
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(e) => {
                        if self
                            .socket
                            .run_command(self.ssh_shell_kind, "which", &["wget"], true)
                            .await
                            .is_ok()
                        {
                            return Err(e);
                        } else {
                            anyhow::bail!("Neither curl nor wget is available");
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn upload_local_server_binary(
        &self,
        src_path: &Path,
        tmp_path: &RelPath,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        if let Some(parent) = tmp_path.parent() {
            let res = self
                .socket
                .run_command(
                    self.ssh_shell_kind,
                    "mkdir",
                    &["-p", parent.display(self.path_style()).as_ref()],
                    true,
                )
                .await;
            if !self.ssh_platform.os.is_windows() {
                // mkdir fails on windows if the path already exists ...
                res?;
            }
        }

        let src_stat = fs::metadata(&src_path)
            .await
            .with_context(|| format!("failed to get metadata for {:?}", src_path))?;
        let size = src_stat.len();

        let t0 = Instant::now();
        delegate.set_status(Some("Uploading remote development server"), cx);
        log::info!(
            "uploading remote development server to {:?} ({}kb)",
            tmp_path,
            size / 1024
        );
        self.upload_file(src_path, tmp_path)
            .await
            .context("failed to upload server binary")?;
        log::info!("uploaded remote development server in {:?}", t0.elapsed());
        Ok(())
    }

    async fn extract_server_binary(
        &self,
        dst_path: &RelPath,
        tmp_path: &RelPath,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        delegate.set_status(Some("Extracting remote development server"), cx);

        if self.ssh_platform.os.is_windows() {
            self.extract_server_binary_windows(dst_path, tmp_path).await
        } else {
            self.extract_server_binary_posix(dst_path, tmp_path).await
        }
    }

    async fn extract_server_binary_posix(
        &self,
        dst_path: &RelPath,
        tmp_path: &RelPath,
    ) -> Result<()> {
        let shell_kind = ShellKind::Posix;
        let server_mode = 0o755;
        let orig_tmp_path = tmp_path.display(self.path_style());
        let server_mode = format!("{:o}", server_mode);
        let server_mode = shell_kind
            .try_quote(&server_mode)
            .context("shell quoting")?;
        let dst_path = dst_path.display(self.path_style());
        let dst_path = shell_kind.try_quote(&dst_path).context("shell quoting")?;
        let script = if let Some(tmp_path) = orig_tmp_path.strip_suffix(".gz") {
            let orig_tmp_path = shell_kind
                .try_quote(&orig_tmp_path)
                .context("shell quoting")?;
            let tmp_path = shell_kind.try_quote(&tmp_path).context("shell quoting")?;
            format!(
                "gunzip -f {orig_tmp_path} && chmod {server_mode} {tmp_path} && mv {tmp_path} {dst_path}",
            )
        } else {
            let orig_tmp_path = shell_kind
                .try_quote(&orig_tmp_path)
                .context("shell quoting")?;
            format!("chmod {server_mode} {orig_tmp_path} && mv {orig_tmp_path} {dst_path}",)
        };
        let args = shell_kind.args_for_shell(false, script.to_string());
        self.socket
            .run_command(self.ssh_shell_kind, "sh", &args, true)
            .await?;
        Ok(())
    }

    async fn extract_server_binary_windows(
        &self,
        dst_path: &RelPath,
        tmp_path: &RelPath,
    ) -> Result<()> {
        let shell_kind = ShellKind::Pwsh;
        let orig_tmp_path = tmp_path.display(self.path_style());
        let dst_path = dst_path.display(self.path_style());
        let dst_path = shell_kind.try_quote(&dst_path).context("shell quoting")?;

        let script = if let Some(tmp_path) = orig_tmp_path.strip_suffix(".zip") {
            let orig_tmp_path = shell_kind
                .try_quote(&orig_tmp_path)
                .context("shell quoting")?;
            let tmp_path = shell_kind.try_quote(tmp_path).context("shell quoting")?;
            let tmp_exe_path = format!("{tmp_path}\\remote_server.exe");
            let tmp_exe_path = shell_kind
                .try_quote(&tmp_exe_path)
                .context("shell quoting")?;
            format!(
                "Expand-Archive -Force -Path {orig_tmp_path} -DestinationPath {tmp_path} -ErrorAction Stop; Move-Item -Force {tmp_exe_path} {dst_path}; Remove-Item -Force {tmp_path} -Recurse; Remove-Item -Force {orig_tmp_path}",
            )
        } else {
            let orig_tmp_path = shell_kind
                .try_quote(&orig_tmp_path)
                .context("shell quoting")?;
            format!("Move-Item -Force {orig_tmp_path} {dst_path}")
        };

        let args = shell_kind.args_for_shell(false, script);
        self.socket
            .run_command(self.ssh_shell_kind, "powershell", &args, true)
            .await?;
        Ok(())
    }

    fn build_scp_command(
        &self,
        src_path: &Path,
        dest_path_str: &str,
        additional_args: Option<&[&str]>,
    ) -> util::command::Command {
        /// These arguments exist for `ssh` but don't exist / don't have the same semantic for `scp`.
        const SSH_DENY_ARGS_FOR_SCP: &[&str] = &["-X", "-Y"];

        let mut command = util::command::new_command("scp");
        self.socket
            .ssh_options(&mut command, false, Some(SSH_DENY_ARGS_FOR_SCP))
            .args(
                self.socket
                    .connection_options
                    .port
                    .map(|port| vec!["-P".to_string(), port.to_string()])
                    .unwrap_or_default(),
            );
        if let Some(args) = additional_args {
            command.args(args);
        }
        command.arg(src_path).arg(format!(
            "{}:{}",
            self.socket.connection_options.scp_destination(),
            dest_path_str
        ));
        command
    }

    fn build_sftp_command(&self) -> util::command::Command {
        // these arguments exist for "ssh" but don't exist / don't have the same semantic for "sftp"
        const SSH_DENY_ARGS_FOR_SFTP: &[&str] = &["-X", "-Y"];

        let mut command = util::command::new_command("sftp");
        self.socket
            .ssh_options(&mut command, false, Some(SSH_DENY_ARGS_FOR_SFTP))
            .args(
                self.socket
                    .connection_options
                    .port
                    .map(|port| vec!["-P".to_string(), port.to_string()])
                    .unwrap_or_default(),
            );
        command.arg("-b").arg("-");
        command.arg(self.socket.connection_options.scp_destination());
        command.stdin(Stdio::piped());
        command
    }

    async fn upload_file(&self, src_path: &Path, dest_path: &RelPath) -> Result<()> {
        log::debug!("uploading file {:?} to {:?}", src_path, dest_path);

        let src_path_display = src_path.display().to_string();
        let dest_path_str = dest_path.display(self.path_style());

        // We will try SFTP first, and if that fails, we will fall back to SCP.
        // If SCP fails also, we give up and return an error.
        // The reason we allow a fallback from SFTP to SCP is that if the user has to specify a password,
        // depending on the implementation of SSH stack, SFTP may disable interactive password prompts in batch mode.
        // This is for example the case on Windows as evidenced by this implementation snippet:
        // https://github.com/PowerShell/openssh-portable/blob/b8c08ef9da9450a94a9c5ef717d96a7bd83f3332/sshconnect2.c#L417
        if Self::is_sftp_available().await {
            log::debug!("using SFTP for file upload");
            let mut command = self.build_sftp_command();
            let sftp_batch = sftp_put_command(&src_path_display, &dest_path_str);

            let mut child = command.spawn()?;
            if let Some(mut stdin) = child.stdin.take() {
                use futures::AsyncWriteExt;
                stdin.write_all(sftp_batch.as_bytes()).await?;
                stdin.flush().await?;
            }

            let output = child.output().await?;
            if output.status.success() {
                return Ok(());
            }

            let stderr = String::from_utf8_lossy(&output.stderr);
            log::debug!(
                "failed to upload file via SFTP {src_path_display} -> {dest_path_str}: {stderr}"
            );
        }

        log::debug!("using SCP for file upload");
        let mut command = self.build_scp_command(src_path, &dest_path_str, None);
        let output = command.output().await?;

        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        log::debug!(
            "failed to upload file via SCP {src_path_display} -> {dest_path_str}: {stderr}",
        );
        anyhow::bail!(
            "failed to upload file via STFP/SCP {} -> {}: {}",
            src_path_display,
            dest_path_str,
            stderr,
        );
    }

    async fn is_sftp_available() -> bool {
        which::which("sftp").is_ok()
    }
}

impl SshSocket {
    #[cfg(not(windows))]
    async fn new(options: SshConnectionOptions, socket_path: PathBuf) -> Result<Self> {
        Ok(Self {
            connection_options: options,
            envs: HashMap::default(),
            socket_path,
        })
    }

    #[cfg(windows)]
    async fn new(
        options: SshConnectionOptions,
        password: askpass::EncryptedPassword,
        executor: gpui::BackgroundExecutor,
    ) -> Result<Self> {
        let mut envs = HashMap::default();
        let get_password =
            move |_| Task::ready(std::ops::ControlFlow::Continue(Ok(password.clone())));

        let _proxy = askpass::PasswordProxy::new(Box::new(get_password), executor).await?;
        envs.insert("SSH_ASKPASS_REQUIRE".into(), "force".into());
        envs.insert(
            "SSH_ASKPASS".into(),
            _proxy.script_path().as_ref().display().to_string(),
        );
        envs.insert(
            "ZED_ASKPASS_SOCKET".into(),
            _proxy.socket_path().as_ref().display().to_string(),
        );

        Ok(Self {
            connection_options: options,
            envs,
            _proxy,
        })
    }

    // :WARNING: ssh unquotes arguments when executing on the remote :WARNING:
    // e.g. $ ssh host sh -c 'ls -l' is equivalent to $ ssh host sh -c ls -l
    // and passes -l as an argument to sh, not to ls.
    // Furthermore, some setups (e.g. Coder) will change directory when SSH'ing
    // into a machine. You must use `cd` to get back to $HOME.
    // You need to do it like this: $ ssh host "cd; sh -c 'ls -l /tmp'"
    fn ssh_command(
        &self,
        shell_kind: ShellKind,
        program: &str,
        args: &[impl AsRef<str>],
        allow_pseudo_tty: bool,
    ) -> util::command::Command {
        let mut command = util::command::new_command("ssh");
        let program = shell_kind.prepend_command_prefix(program);
        let mut to_run = shell_kind
            .try_quote_prefix_aware(&program)
            .expect("shell quoting")
            .into_owned();
        for arg in args {
            // We're trying to work with: sh, bash, zsh, fish, tcsh, ...?
            debug_assert!(
                !arg.as_ref().contains('\n'),
                "multiline arguments do not work in all shells"
            );
            to_run.push(' ');
            to_run.push_str(&shell_kind.try_quote(arg.as_ref()).expect("shell quoting"));
        }
        let to_run = if shell_kind == ShellKind::Cmd {
            to_run // 'cd' prints the current directory in CMD
        } else {
            let separator = shell_kind.sequential_commands_separator();
            format!("cd{separator} {to_run}")
        };
        self.ssh_options(&mut command, true, None)
            .arg(self.connection_options.ssh_destination());
        if !allow_pseudo_tty {
            command.arg("-T");
        }
        command.arg(to_run);
        log::debug!("ssh {:?}", command);
        command
    }

    async fn run_command(
        &self,
        shell_kind: ShellKind,
        program: &str,
        args: &[impl AsRef<str>],
        allow_pseudo_tty: bool,
    ) -> Result<String> {
        let mut command = self.ssh_command(shell_kind, program, args, allow_pseudo_tty);
        let output = command.output().await?;
        log::debug!("{:?}: {:?}", command, output);
        anyhow::ensure!(
            output.status.success(),
            "failed to run command {command:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    fn ssh_options<'a>(
        &self,
        command: &'a mut util::command::Command,
        include_port_forwards: bool,
        deny_args: Option<&[&str]>,
    ) -> &'a mut util::command::Command {
        let mut args = if include_port_forwards {
            self.connection_options.additional_args()
        } else {
            self.connection_options.additional_args_for_scp()
        };

        // draining all arguments that are explicitly denied
        if let Some(deny_args) = deny_args {
            args.retain(|x| !deny_args.contains(&x.as_str()));
        }

        let cmd = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .args(args);

        if cfg!(windows) {
            cmd.envs(self.envs.clone());
        }
        #[cfg(not(windows))]
        {
            cmd.args(["-o", "ControlMaster=no", "-o"])
                .arg(format!("ControlPath={}", self.socket_path.display()));
        }
        cmd
    }

    // Returns the SSH command-line options (without the destination) for building commands.
    // On Linux, this includes the ControlPath option to reuse the existing connection.
    // Note: The destination must be added separately after all options to ensure proper
    // SSH command structure: ssh [options] destination [command]
    fn ssh_command_options(&self) -> Vec<String> {
        let arguments = self.connection_options.additional_args();
        #[cfg(not(windows))]
        let arguments = {
            let mut args = arguments;
            args.extend(vec![
                "-o".to_string(),
                "ControlMaster=no".to_string(),
                "-o".to_string(),
                format!("ControlPath={}", self.socket_path.display()),
            ]);
            args
        };
        arguments
    }

    async fn platform(&self, shell: ShellKind, is_windows: bool) -> Result<RemotePlatform> {
        if is_windows {
            self.platform_windows(shell).await
        } else {
            self.platform_posix(shell).await
        }
    }

    async fn platform_posix(&self, shell: ShellKind) -> Result<RemotePlatform> {
        let output = self
            .run_command(shell, "uname", &["-sm"], false)
            .await
            .context("Failed to run 'uname -sm' to determine platform")?;
        parse_platform(&output)
    }

    /// Best-effort detection of the remote OS version. Failures are logged and
    /// result in `None` rather than failing the connection, since this is only
    /// used for telemetry.
    async fn os_version(&self, os: RemoteOs, shell: ShellKind) -> Option<String> {
        let (program, args) = super::os_version_command(os);
        match self.run_command(shell, program, args, false).await {
            Ok(output) => super::parse_os_version(os, &output),
            Err(error) => {
                log::warn!("Failed to determine remote OS version: {error:#}");
                None
            }
        }
    }

    async fn platform_windows(&self, shell: ShellKind) -> Result<RemotePlatform> {
        let output = self
            .run_command(
                shell,
                "cmd.exe",
                &["/c", "echo", "%PROCESSOR_ARCHITECTURE%"],
                false,
            )
            .await
            .context(
                "Failed to run 'echo %PROCESSOR_ARCHITECTURE%' to determine Windows architecture",
            )?;

        Ok(RemotePlatform {
            os: RemoteOs::Windows,
            arch: match output.trim() {
                "AMD64" => RemoteArch::X86_64,
                "ARM64" => RemoteArch::Aarch64,
                arch => anyhow::bail!(
                    "Prebuilt remote servers are not yet available for windows-{arch}. See https://zed.dev/docs/remote-development"
                ),
            },
        })
    }

    /// Probes whether the remote host is running Windows.
    ///
    /// This is done by attempting to run a simple Windows-specific command.
    /// If it succeeds and returns Windows-like output, we assume it's Windows.
    async fn probe_is_windows(&self) -> bool {
        match self
            .run_command(ShellKind::Cmd, "cmd.exe", &["/c", "ver"], false)
            .await
        {
            // Windows 'ver' command outputs something like "Microsoft Windows [Version 10.0.19045.5011]"
            Ok(output) => output.trim().contains("indows"),
            Err(_) => false,
        }
    }

    async fn shell(&self, is_windows: bool) -> String {
        if is_windows {
            self.shell_windows().await
        } else {
            self.shell_posix().await
        }
    }

    async fn shell_posix(&self) -> String {
        const DEFAULT_SHELL: &str = "sh";
        match self
            .run_command(ShellKind::Posix, "sh", &["-c", "echo $SHELL"], false)
            .await
        {
            Ok(output) => parse_shell(&output, DEFAULT_SHELL),
            Err(e) => {
                log::error!("Failed to detect remote shell: {e}");
                DEFAULT_SHELL.to_owned()
            }
        }
    }

    async fn shell_windows(&self) -> String {
        const DEFAULT_SHELL: &str = "cmd.exe";

        // We detect the shell used by the SSH session by running the following command in PowerShell:
        // (Get-CimInstance Win32_Process -Filter "ProcessId = $((Get-CimInstance Win32_Process -Filter ProcessId=$PID).ParentProcessId)").Name
        // This prints the name of PowerShell's parent process (which will be the shell that SSH launched).
        // We pass it as a Base64 encoded string since we don't yet know how to correctly quote that command.
        // (We'd need to know what the shell is to do that...)
        match self
            .run_command(
                ShellKind::Cmd,
                "powershell",
                &[
                    "-E",
                    "KABHAGUAdAAtAEMAaQBtAEkAbgBzAHQAYQBuAGMAZQAgAFcAaQBuADMAMgBfAFAAcgBvAGMAZQBzAHMAIAAtAEYAaQBsAHQAZQByACAAIgBQAHIAbwBjAGUAcwBzAEkAZAAgAD0AIAAkACgAKABHAGUAdAAtAEMAaQBtAEkAbgBzAHQAYQBuAGMAZQAgAFcAaQBuADMAMgBfAFAAcgBvAGMAZQBzAHMAIAAtAEYAaQBsAHQAZQByACAAUAByAG8AYwBlAHMAcwBJAGQAPQAkAFAASQBEACkALgBQAGEAcgBlAG4AdABQAHIAbwBjAGUAcwBzAEkAZAApACIAKQAuAE4AYQBtAGUA",
                ],
                false,
            )
            .await
        {
            Ok(output) => parse_shell(&output, DEFAULT_SHELL),
            Err(e) => {
                log::error!("Failed to detect remote shell: {e}");
                DEFAULT_SHELL.to_owned()
            }
        }
    }
}

fn parse_port_number(port_str: &str) -> Result<u16> {
    port_str
        .parse()
        .with_context(|| format!("parsing port number: {port_str}"))
}

fn split_port_forward_tokens(spec: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut chars = spec.chars().peekable();

    while chars.peek().is_some() {
        if chars.peek() == Some(&'[') {
            chars.next();
            let mut bracket_content = String::new();
            loop {
                match chars.next() {
                    Some(']') => break,
                    Some(ch) => bracket_content.push(ch),
                    None => anyhow::bail!("Unmatched '[' in port forward spec: {spec}"),
                }
            }
            tokens.push(bracket_content);
            if chars.peek() == Some(&':') {
                chars.next();
            }
        } else {
            let mut token = String::new();
            for ch in chars.by_ref() {
                if ch == ':' {
                    break;
                }
                token.push(ch);
            }
            tokens.push(token);
        }
    }

    Ok(tokens)
}

fn parse_port_forward_spec(spec: &str) -> Result<SshPortForwardOption> {
    let tokens = if spec.contains('[') {
        split_port_forward_tokens(spec)?
    } else {
        spec.split(':').map(String::from).collect()
    };

    match tokens.len() {
        4 => {
            let local_port = parse_port_number(&tokens[1])?;
            let remote_port = parse_port_number(&tokens[3])?;

            Ok(SshPortForwardOption {
                local_host: Some(tokens[0].clone()),
                local_port,
                remote_host: Some(tokens[2].clone()),
                remote_port,
            })
        }
        3 => {
            let local_port = parse_port_number(&tokens[0])?;
            let remote_port = parse_port_number(&tokens[2])?;

            Ok(SshPortForwardOption {
                local_host: None,
                local_port,
                remote_host: Some(tokens[1].clone()),
                remote_port,
            })
        }
        _ => anyhow::bail!("Invalid port forward format: {spec}"),
    }
}

impl SshConnectionOptions {
    pub fn parse_command_line(input: &str) -> Result<Self> {
        let input = input.trim_start_matches("ssh ");
        let mut hostname: Option<String> = None;
        let mut username: Option<String> = None;
        let mut port: Option<u16> = None;
        let mut args = Vec::new();
        let mut port_forwards: Vec<SshPortForwardOption> = Vec::new();

        // disallowed: -E, -e, -F, -f, -G, -g, -M, -N, -n, -O, -q, -S, -s, -T, -t, -V, -v, -W
        const ALLOWED_OPTS: &[&str] = &[
            "-4", "-6", "-A", "-a", "-C", "-K", "-k", "-X", "-x", "-Y", "-y",
        ];
        const ALLOWED_ARGS: &[&str] = &[
            "-B", "-b", "-c", "-D", "-F", "-I", "-i", "-J", "-l", "-m", "-o", "-P", "-p", "-R",
            "-w",
        ];

        let mut tokens = ShellKind::Posix
            .split(input)
            .context("invalid input")?
            .into_iter();

        'outer: while let Some(arg) = tokens.next() {
            if ALLOWED_OPTS.contains(&(&arg as &str)) {
                args.push(arg.to_string());
                continue;
            }
            if arg == "-p" {
                port = tokens.next().and_then(|arg| arg.parse().ok());
                continue;
            } else if let Some(p) = arg.strip_prefix("-p") {
                port = p.parse().ok();
                continue;
            }
            if arg == "-l" {
                username = tokens.next();
                continue;
            } else if let Some(l) = arg.strip_prefix("-l") {
                username = Some(l.to_string());
                continue;
            }
            if arg == "-L" || arg.starts_with("-L") {
                let forward_spec = if arg == "-L" {
                    tokens.next()
                } else {
                    Some(arg.strip_prefix("-L").unwrap().to_string())
                };

                if let Some(spec) = forward_spec {
                    port_forwards.push(parse_port_forward_spec(&spec)?);
                } else {
                    anyhow::bail!("Missing port forward format");
                }
            }

            for a in ALLOWED_ARGS {
                if arg == *a {
                    args.push(arg);
                    if let Some(next) = tokens.next() {
                        args.push(next);
                    }
                    continue 'outer;
                } else if arg.starts_with(a) {
                    args.push(arg);
                    continue 'outer;
                }
            }
            if arg.starts_with("-") || hostname.is_some() {
                anyhow::bail!("unsupported argument: {:?}", arg);
            }
            let mut input = &arg as &str;
            // Destination might be: username1@username2@ip2@ip1
            if let Some((u, rest)) = input.rsplit_once('@') {
                input = rest;
                username = Some(u.to_string());
            }

            // Handle port parsing, accounting for IPv6 addresses
            // IPv6 addresses can be: 2001:db8::1 or [2001:db8::1]:22
            if input.starts_with('[') {
                if let Some((rest, p)) = input.rsplit_once("]:") {
                    input = rest.strip_prefix('[').unwrap_or(rest);
                    port = p.parse().ok();
                } else if input.ends_with(']') {
                    input = input.strip_prefix('[').unwrap_or(input);
                    input = input.strip_suffix(']').unwrap_or(input);
                }
            } else if let Some((rest, p)) = input.rsplit_once(':')
                && !rest.contains(":")
            {
                input = rest;
                port = p.parse().ok();
            }

            hostname = Some(input.to_string())
        }

        let Some(hostname) = hostname else {
            anyhow::bail!("missing hostname");
        };

        let port_forwards = match port_forwards.len() {
            0 => None,
            _ => Some(port_forwards),
        };

        Ok(Self {
            host: hostname.into(),
            username,
            port,
            port_forwards,
            args: Some(args),
            password: None,
            nickname: None,
            upload_binary_over_ssh: false,
            connection_timeout: None,
        })
    }

    pub fn ssh_destination(&self) -> String {
        let mut result = String::default();
        if let Some(username) = &self.username {
            // Username might be: username1@username2@ip2
            let username = urlencoding::encode(username);
            result.push_str(&username);
            result.push('@');
        }

        result.push_str(&self.host.to_string());
        result
    }

    pub fn additional_args_for_scp(&self) -> Vec<String> {
        self.args.iter().flatten().cloned().collect::<Vec<String>>()
    }

    pub fn additional_args(&self) -> Vec<String> {
        let mut args = self.additional_args_for_scp();

        if let Some(timeout) = self.connection_timeout {
            args.extend(["-o".to_string(), format!("ConnectTimeout={}", timeout)]);
        }

        if let Some(port) = self.port {
            args.push("-p".to_string());
            args.push(port.to_string());
        }

        if let Some(forwards) = &self.port_forwards {
            args.extend(forwards.iter().map(|pf| {
                let local_host = match &pf.local_host {
                    Some(host) => host,
                    None => "localhost",
                };
                let remote_host = match &pf.remote_host {
                    Some(host) => host,
                    None => "localhost",
                };

                format!(
                    "-L{}:{}:{}:{}",
                    bracket_ipv6(local_host),
                    pf.local_port,
                    bracket_ipv6(remote_host),
                    pf.remote_port
                )
            }));
        }

        args
    }

    fn scp_destination(&self) -> String {
        if let Some(username) = &self.username {
            format!("{}@{}", username, self.host.to_bracketed_string())
        } else {
            self.host.to_string()
        }
    }

    pub fn connection_string(&self) -> String {
        let host = if let Some(port) = &self.port {
            format!("{}:{}", self.host.to_bracketed_string(), port)
        } else {
            self.host.to_string()
        };

        if let Some(username) = &self.username {
            format!("{}@{}", username, host)
        } else {
            host
        }
    }
}

/// The pieces `build_command` uses, for a transport that must rebuild commands elsewhere
/// (the browser cannot reach the ControlMaster's connection object, only its facts).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SshCommandRecipe {
    pub ssh_options: Vec<String>,
    pub destination: String,
    pub env: HashMap<String, String>,
    pub shell: String,
    pub is_windows: bool,
    pub path_style: RecipePathStyle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecipePathStyle {
    Unix,
    Windows,
}

impl From<PathStyle> for RecipePathStyle {
    fn from(path_style: PathStyle) -> Self {
        match path_style {
            PathStyle::Unix => Self::Unix,
            PathStyle::Windows => Self::Windows,
        }
    }
}

impl From<RecipePathStyle> for PathStyle {
    fn from(path_style: RecipePathStyle) -> Self {
        match path_style {
            RecipePathStyle::Unix => Self::Unix,
            RecipePathStyle::Windows => Self::Windows,
        }
    }
}

pub fn build_ssh_command(
    recipe: &SshCommandRecipe,
    program: Option<String>,
    args: &[String],
    env: &HashMap<String, String>,
    working_dir: Option<String>,
    port_forward: Option<(u16, String, u16)>,
    interactive: Interactive,
) -> Result<CommandTemplate> {
    // Recomputed rather than carried in the recipe: `SshRemoteConnection::new` derives it the
    // same way, and the platform's `is_windows` always agrees with the probe that fed it.
    let shell_kind = ShellKind::new(&recipe.shell, recipe.is_windows);
    let build = if recipe.is_windows {
        build_command_windows
    } else {
        build_command_posix
    };
    build(
        program,
        args,
        env,
        working_dir,
        port_forward,
        recipe.env.clone(),
        recipe.path_style.into(),
        &recipe.shell,
        shell_kind,
        recipe.ssh_options.clone(),
        &recipe.destination,
        interactive,
    )
}

#[cfg(not(target_family = "wasm"))]
fn bundled_server_binary_name(content_id: &str, os: RemoteOs) -> Result<String> {
    const CONTENT_ID_PREFIX_LEN: usize = 16;
    anyhow::ensure!(
        content_id.len() >= CONTENT_ID_PREFIX_LEN
            && content_id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "the bundled remote server content id {content_id:?} is not a hex digest"
    );
    let content_prefix = &content_id[..CONTENT_ID_PREFIX_LEN];
    let extension = if os.is_windows() { ".exe" } else { "" };
    Ok(format!("zed-remote-server-web-{content_prefix}{extension}"))
}

#[cfg(not(target_family = "wasm"))]
fn bundled_version_matches(version_output: &str, expected_version: &str) -> bool {
    // Same reason `parse_platform` reads the last line: a login shell may print noise first.
    version_output
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .is_some_and(|line| line == expected_version)
}

#[cfg(not(target_family = "wasm"))]
fn upload_nonce() -> u64 {
    use std::hash::{BuildHasher as _, Hasher as _};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

fn build_command_posix(
    input_program: Option<String>,
    input_args: &[String],
    input_env: &HashMap<String, String>,
    working_dir: Option<String>,
    port_forward: Option<(u16, String, u16)>,
    ssh_env: HashMap<String, String>,
    ssh_path_style: PathStyle,
    ssh_shell: &str,
    ssh_shell_kind: ShellKind,
    ssh_options: Vec<String>,
    ssh_destination: &str,
    interactive: Interactive,
) -> Result<CommandTemplate> {
    use std::fmt::Write as _;

    let mut exec = String::new();
    if let Some(working_dir) = working_dir {
        let working_dir = RemotePathBuf::new(working_dir, ssh_path_style).to_string();

        // For paths starting with ~/, we need $HOME to expand, but the remainder
        // must be properly quoted to prevent command injection.
        // Pattern: cd "$HOME"/'quoted/remainder' - $HOME expands, rest is single-quoted
        const TILDE_PREFIX: &str = "~/";
        if working_dir.starts_with(TILDE_PREFIX) {
            let remainder = working_dir.trim_start_matches(TILDE_PREFIX);
            if remainder.is_empty() {
                write!(
                    exec,
                    "cd \"$HOME\" {} ",
                    ssh_shell_kind.sequential_and_commands_separator()
                )?;
            } else {
                let quoted_remainder = ssh_shell_kind
                    .try_quote(remainder)
                    .context("shell quoting")?;
                write!(
                    exec,
                    "cd \"$HOME\"/{quoted_remainder} {} ",
                    ssh_shell_kind.sequential_and_commands_separator()
                )?;
            }
        } else {
            let quoted_dir = ssh_shell_kind
                .try_quote(&working_dir)
                .context("shell quoting")?;
            write!(
                exec,
                "cd {quoted_dir} {} ",
                ssh_shell_kind.sequential_and_commands_separator()
            )?;
        }
    } else {
        write!(
            exec,
            "cd {} ",
            ssh_shell_kind.sequential_and_commands_separator()
        )?;
    };
    write!(exec, "exec env ")?;

    for (k, v) in input_env.iter() {
        let assignment = format!("{k}={v}");
        let assignment = ssh_shell_kind
            .try_quote(&assignment)
            .context("shell quoting")?;
        write!(exec, "{assignment} ")?;
    }

    if let Some(input_program) = input_program {
        write!(
            exec,
            "{}",
            ssh_shell_kind
                .try_quote_prefix_aware(&input_program)
                .context("shell quoting")?
        )?;
        for arg in input_args {
            let arg = ssh_shell_kind.try_quote(&arg).context("shell quoting")?;
            write!(exec, " {arg}")?;
        }
    } else {
        write!(exec, "{ssh_shell} -l")?;
    };

    let mut args = Vec::new();
    args.extend(ssh_options);

    if let Some((local_port, host, remote_port)) = port_forward {
        args.push("-L".into());
        args.push(format!(
            "{}:{}:{}",
            local_port,
            bracket_ipv6(&host),
            remote_port
        ));
    }

    // LogLevel=ERROR suppresses the "Connection to ... closed." message while
    // preserving SSH errors.
    args.extend(["-o".into(), "LogLevel=ERROR".into()]);
    match interactive {
        // -t forces pseudo-TTY allocation (for interactive use)
        Interactive::Yes => args.push("-t".into()),
        // -T disables pseudo-TTY allocation (for non-interactive piped stdio)
        Interactive::No => args.push("-T".into()),
    }
    // The destination must come after all options but before the command
    args.push(ssh_destination.into());
    args.push(exec);

    Ok(CommandTemplate {
        program: "ssh".into(),
        args,
        env: ssh_env,
    })
}

fn build_command_windows(
    input_program: Option<String>,
    input_args: &[String],
    input_env: &HashMap<String, String>,
    working_dir: Option<String>,
    port_forward: Option<(u16, String, u16)>,
    ssh_env: HashMap<String, String>,
    ssh_path_style: PathStyle,
    ssh_shell: &str,
    _ssh_shell_kind: ShellKind,
    ssh_options: Vec<String>,
    ssh_destination: &str,
    interactive: Interactive,
) -> Result<CommandTemplate> {
    use base64::Engine as _;
    use std::fmt::Write as _;

    let mut exec = String::new();
    let shell_kind = ShellKind::PowerShell;

    if let Some(working_dir) = working_dir {
        let working_dir = RemotePathBuf::new(working_dir, ssh_path_style).to_string();

        write!(
            exec,
            "Set-Location -Path {} {} ",
            shell_kind
                .try_quote(&working_dir)
                .context("shell quoting")?,
            shell_kind.sequential_and_commands_separator()
        )?;
    }

    // Windows OpenSSH has an 8K character limit for command lines. The full
    // environment blows past it, so the only variable forwarded is the one port
    // attribution matches: this window's connection id.
    if let Some(connection_id) = input_env.get(crate::listening_ports::REMOTE_CONNECTION_ID_ENV_VAR)
    {
        let quoted = shell_kind
            .try_quote(connection_id)
            .context("shell quoting")?;
        write!(
            exec,
            "$env:{}={} {} ",
            crate::listening_ports::REMOTE_CONNECTION_ID_ENV_VAR,
            quoted,
            shell_kind.sequential_and_commands_separator()
        )?;
    }

    if let Some(input_program) = input_program {
        write!(
            exec,
            "{}",
            shell_kind
                .try_quote_prefix_aware(&shell_kind.prepend_command_prefix(&input_program))
                .context("shell quoting")?
        )?;
        for arg in input_args {
            let arg = shell_kind.try_quote(arg).context("shell quoting")?;
            write!(exec, " {arg}")?;
        }
    } else {
        // Launch an interactive shell session
        write!(exec, "{ssh_shell}")?;
    };

    let mut args = Vec::new();
    args.extend(ssh_options);

    if let Some((local_port, host, remote_port)) = port_forward {
        args.push("-L".into());
        args.push(format!(
            "{}:{}:{}",
            local_port,
            bracket_ipv6(&host),
            remote_port
        ));
    }

    // LogLevel=ERROR suppresses the "Connection to ... closed." message while
    // preserving SSH errors.
    args.extend(["-o".into(), "LogLevel=ERROR".into()]);
    match interactive {
        // -t forces pseudo-TTY allocation (for interactive use)
        Interactive::Yes => args.push("-t".into()),
        // -T disables pseudo-TTY allocation (for non-interactive piped stdio)
        Interactive::No => args.push("-T".into()),
    }

    // The destination must come after all options but before the command
    args.push(ssh_destination.into());

    // Windows OpenSSH server incorrectly escapes the command string when the PTY is used.
    // The simplest way to work around this is to use a base64 encoded command, which doesn't require escaping.
    let utf16_bytes: Vec<u16> = exec.encode_utf16().collect();
    let byte_slice: Vec<u8> = utf16_bytes.iter().flat_map(|&u| u.to_le_bytes()).collect();
    let base64_encoded = base64::engine::general_purpose::STANDARD.encode(&byte_slice);

    args.push(format!("powershell.exe -E {}", base64_encoded));

    Ok(CommandTemplate {
        program: "ssh".into(),
        args,
        env: ssh_env,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_command() -> Result<()> {
        let mut input_env = HashMap::default();
        input_env.insert("INPUT_VA".to_string(), "val".to_string());
        let mut env = HashMap::default();
        env.insert("SSH_VAR".to_string(), "ssh-val".to_string());

        // Test non-interactive command (interactive=false should use -T)
        let command = build_command_posix(
            Some("remote_program".to_string()),
            &["arg1".to_string(), "arg2".to_string()],
            &input_env,
            Some("~/work".to_string()),
            None,
            env.clone(),
            PathStyle::Unix,
            "/bin/bash",
            ShellKind::Posix,
            vec!["-o".to_string(), "ControlMaster=auto".to_string()],
            "user@host",
            Interactive::No,
        )?;
        assert_eq!(command.program, "ssh");
        // Should contain -T for non-interactive
        assert!(command.args.iter().any(|arg| arg == "-T"));
        assert!(!command.args.iter().any(|arg| arg == "-t"));

        // Test interactive command (interactive=true should use -t)
        let command = build_command_posix(
            Some("remote_program".to_string()),
            &["arg1".to_string(), "arg2".to_string()],
            &input_env,
            Some("~/work".to_string()),
            None,
            env.clone(),
            PathStyle::Unix,
            "/bin/fish",
            ShellKind::Fish,
            vec!["-p".to_string(), "2222".to_string()],
            "user@host",
            Interactive::Yes,
        )?;

        assert_eq!(command.program, "ssh");
        assert_eq!(
            command.args.iter().map(String::as_str).collect::<Vec<_>>(),
            [
                "-p",
                "2222",
                "-o",
                "LogLevel=ERROR",
                "-t",
                "user@host",
                "cd \"$HOME\"/work && exec env 'INPUT_VA=val' remote_program arg1 arg2"
            ]
        );
        assert_eq!(command.env, env);

        let mut input_env = HashMap::default();
        input_env.insert("INPUT_VA".to_string(), "val".to_string());
        let mut env = HashMap::default();
        env.insert("SSH_VAR".to_string(), "ssh-val".to_string());

        let command = build_command_posix(
            None,
            &[],
            &input_env,
            None,
            Some((1, "foo".to_owned(), 2)),
            env.clone(),
            PathStyle::Unix,
            "/bin/fish",
            ShellKind::Fish,
            vec!["-p".to_string(), "2222".to_string()],
            "user@host",
            Interactive::Yes,
        )?;

        assert_eq!(command.program, "ssh");
        assert_eq!(
            command.args.iter().map(String::as_str).collect::<Vec<_>>(),
            [
                "-p",
                "2222",
                "-L",
                "1:foo:2",
                "-o",
                "LogLevel=ERROR",
                "-t",
                "user@host",
                "cd && exec env 'INPUT_VA=val' /bin/fish -l"
            ]
        );
        assert_eq!(command.env, env);

        Ok(())
    }

    #[test]
    fn test_build_command_quotes_env_assignment() -> Result<()> {
        let mut input_env = HashMap::default();
        input_env.insert("ZED$(echo foo)".to_string(), "value".to_string());

        let command = build_command_posix(
            Some("remote_program".to_string()),
            &[],
            &input_env,
            None,
            None,
            HashMap::default(),
            PathStyle::Unix,
            "/bin/bash",
            ShellKind::Posix,
            vec![],
            "user@host",
            Interactive::No,
        )?;

        let remote_command = command
            .args
            .last()
            .context("missing remote command argument")?;
        assert!(
            remote_command.contains("exec env 'ZED$(echo foo)=value' remote_program"),
            "expected env assignment to be quoted, got: {remote_command}"
        );

        Ok(())
    }

    /// Windows OpenSSH cannot carry the whole environment (the command line is
    /// capped near 8K), but this window's connection id is a single short
    /// assignment and is what port attribution matches on.
    #[test]
    fn test_windows_remote_shell_receives_zed_remote_connection_id() -> Result<()> {
        let mut input_env = HashMap::default();
        input_env.insert("PATH".to_string(), "x".repeat(7_000));
        input_env.insert(
            crate::listening_ports::REMOTE_CONNECTION_ID_ENV_VAR.to_string(),
            "workspace-7".to_string(),
        );

        let command = build_command_windows(
            None,
            &[],
            &input_env,
            None,
            None,
            HashMap::default(),
            PathStyle::Windows,
            "powershell.exe",
            ShellKind::PowerShell,
            Vec::new(),
            "user@host",
            Interactive::Yes,
        )?;

        let script = decode_powershell_encoded_command(&command)?;
        assert_eq!(
            script, "$env:ZED_REMOTE_CONNECTION_ID=workspace-7 ; powershell.exe",
            "windows remote shell must inherit this window's connection id without copying the rest of the environment"
        );
        Ok(())
    }

    fn decode_powershell_encoded_command(command: &CommandTemplate) -> Result<String> {
        use base64::Engine as _;

        let argument = command
            .args
            .last()
            .context("missing remote command argument")?;
        let encoded = argument
            .strip_prefix("powershell.exe -E ")
            .context("remote command is not a powershell -E payload")?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .context("powershell command is not base64")?;
        if !bytes.len().is_multiple_of(2) {
            anyhow::bail!("powershell command byte length {} is odd", bytes.len());
        }
        let units = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        String::from_utf16(&units).context("powershell command is not utf-16")
    }

    #[test]
    fn test_sftp_put_command_quotes_paths() {
        assert_eq!(
            sftp_put_command(
                "/tmp/Zed Repro/remote_server",
                ".zed_server/downloaded server",
            ),
            "put \"/tmp/Zed Repro/remote_server\" \".zed_server/downloaded server\"\n"
        );
    }

    #[test]
    fn test_sftp_put_command_escapes_quotes_in_paths() {
        assert_eq!(
            sftp_put_command(
                r#"/tmp/Zed "Nightly"/remote_server"#,
                ".zed_server/remote_server",
            ),
            "put \"/tmp/Zed \\\"Nightly\\\"/remote_server\" \".zed_server/remote_server\"\n"
        );
    }

    #[test]
    fn test_sftp_put_command_doubles_trailing_destination_backslash_before_closing_quote() {
        assert_eq!(
            sftp_put_command("/tmp/remote_server", r"C:\zed server\"),
            "put \"/tmp/remote_server\" \"C:\\\\zed server\\\\\"\n"
        );
    }

    #[test]
    fn test_sftp_put_command_doubles_source_backslashes_for_posix_glob() {
        assert_eq!(
            sftp_put_command(
                r"/tmp/zed\server/remote_server",
                ".zed_server/remote_server",
            ),
            "put \"/tmp/zed\\\\server/remote_server\" \".zed_server/remote_server\"\n"
        );
    }

    #[test]
    fn test_sftp_put_command_doubles_windows_source_backslashes_on_all_platforms() {
        assert_eq!(
            sftp_put_command(
                r"C:\Users\Smit\Zed Repro\remote_server",
                ".zed_server/remote_server",
            ),
            "put \"C:\\\\Users\\\\Smit\\\\Zed Repro\\\\remote_server\" \".zed_server/remote_server\"\n"
        );
    }

    #[test]
    fn scp_args_exclude_port_forward_flags() {
        let options = SshConnectionOptions {
            host: "example.com".into(),
            args: Some(vec![
                "-p".to_string(),
                "2222".to_string(),
                "-o".to_string(),
                "StrictHostKeyChecking=no".to_string(),
            ]),
            port_forwards: Some(vec![SshPortForwardOption {
                local_host: Some("127.0.0.1".to_string()),
                local_port: 8080,
                remote_host: Some("127.0.0.1".to_string()),
                remote_port: 80,
            }]),
            ..Default::default()
        };

        let ssh_args = options.additional_args();
        assert!(
            ssh_args.iter().any(|arg| arg.starts_with("-L")),
            "expected ssh args to include port-forward: {ssh_args:?}"
        );

        let scp_args = options.additional_args_for_scp();
        assert_eq!(
            scp_args,
            vec![
                "-p".to_string(),
                "2222".to_string(),
                "-o".to_string(),
                "StrictHostKeyChecking=no".to_string(),
            ]
        );
    }

    #[test]
    fn test_host_parsing() -> Result<()> {
        let opts = SshConnectionOptions::parse_command_line("user@2001:db8::1")?;
        assert_eq!(opts.host, "2001:db8::1".into());
        assert_eq!(opts.username, Some("user".to_string()));
        assert_eq!(opts.port, None);

        let opts = SshConnectionOptions::parse_command_line("user@[2001:db8::1]:2222")?;
        assert_eq!(opts.host, "2001:db8::1".into());
        assert_eq!(opts.username, Some("user".to_string()));
        assert_eq!(opts.port, Some(2222));

        let opts = SshConnectionOptions::parse_command_line("user@[2001:db8::1]")?;
        assert_eq!(opts.host, "2001:db8::1".into());
        assert_eq!(opts.username, Some("user".to_string()));
        assert_eq!(opts.port, None);

        let opts = SshConnectionOptions::parse_command_line("2001:db8::1")?;
        assert_eq!(opts.host, "2001:db8::1".into());
        assert_eq!(opts.username, None);
        assert_eq!(opts.port, None);

        let opts = SshConnectionOptions::parse_command_line("[2001:db8::1]:2222")?;
        assert_eq!(opts.host, "2001:db8::1".into());
        assert_eq!(opts.username, None);
        assert_eq!(opts.port, Some(2222));

        let opts = SshConnectionOptions::parse_command_line("user@example.com:2222")?;
        assert_eq!(opts.host, "example.com".into());
        assert_eq!(opts.username, Some("user".to_string()));
        assert_eq!(opts.port, Some(2222));

        let opts = SshConnectionOptions::parse_command_line("user@192.168.1.1:2222")?;
        assert_eq!(opts.host, "192.168.1.1".into());
        assert_eq!(opts.username, Some("user".to_string()));
        assert_eq!(opts.port, Some(2222));

        Ok(())
    }

    #[test]
    fn test_parse_port_forward_spec_ipv6() -> Result<()> {
        let pf = parse_port_forward_spec("[::1]:8080:[::1]:80")?;
        assert_eq!(pf.local_host, Some("::1".to_string()));
        assert_eq!(pf.local_port, 8080);
        assert_eq!(pf.remote_host, Some("::1".to_string()));
        assert_eq!(pf.remote_port, 80);

        let pf = parse_port_forward_spec("8080:[::1]:80")?;
        assert_eq!(pf.local_host, None);
        assert_eq!(pf.local_port, 8080);
        assert_eq!(pf.remote_host, Some("::1".to_string()));
        assert_eq!(pf.remote_port, 80);

        let pf = parse_port_forward_spec("[2001:db8::1]:3000:[fe80::1]:4000")?;
        assert_eq!(pf.local_host, Some("2001:db8::1".to_string()));
        assert_eq!(pf.local_port, 3000);
        assert_eq!(pf.remote_host, Some("fe80::1".to_string()));
        assert_eq!(pf.remote_port, 4000);

        let pf = parse_port_forward_spec("127.0.0.1:8080:localhost:80")?;
        assert_eq!(pf.local_host, Some("127.0.0.1".to_string()));
        assert_eq!(pf.local_port, 8080);
        assert_eq!(pf.remote_host, Some("localhost".to_string()));
        assert_eq!(pf.remote_port, 80);

        Ok(())
    }

    #[test]
    fn test_port_forward_ipv6_formatting() {
        let options = SshConnectionOptions {
            host: "example.com".into(),
            port_forwards: Some(vec![SshPortForwardOption {
                local_host: Some("::1".to_string()),
                local_port: 8080,
                remote_host: Some("::1".to_string()),
                remote_port: 80,
            }]),
            ..Default::default()
        };

        let args = options.additional_args();
        assert!(
            args.iter().any(|arg| arg == "-L[::1]:8080:[::1]:80"),
            "expected bracketed IPv6 in -L flag: {args:?}"
        );
    }

    #[test]
    fn test_build_command_with_ipv6_port_forward() -> Result<()> {
        let command = build_command_posix(
            None,
            &[],
            &HashMap::default(),
            None,
            Some((8080, "::1".to_owned(), 80)),
            HashMap::default(),
            PathStyle::Unix,
            "/bin/bash",
            ShellKind::Posix,
            vec![],
            "user@host",
            Interactive::No,
        )?;

        assert!(
            command.args.iter().any(|arg| arg == "8080:[::1]:80"),
            "expected bracketed IPv6 in port forward arg: {:?}",
            command.args
        );

        Ok(())
    }

    #[cfg(not(windows))]
    fn test_connection(
        os: RemoteOs,
        shell: &str,
        path_style: PathStyle,
    ) -> Result<SshRemoteConnection> {
        let temp_dir = tempfile::tempdir()?;
        let socket = SshSocket {
            connection_options: SshConnectionOptions {
                host: "example.com".into(),
                username: Some("user".to_string()),
                port: Some(2222),
                args: Some(vec!["-i".to_string(), "/keys/id_ed25519".to_string()]),
                ..Default::default()
            },
            socket_path: temp_dir.path().join("ssh.sock"),
            envs: HashMap::default(),
        };
        Ok(SshRemoteConnection {
            socket,
            master_process: Mutex::new(None),
            killed: AtomicBool::new(false),
            remote_binary_path: None,
            ssh_platform: RemotePlatform {
                os,
                arch: RemoteArch::X86_64,
            },
            ssh_os_version: None,
            ssh_path_style: path_style,
            ssh_shell: shell.to_string(),
            ssh_shell_kind: ShellKind::new(shell, os.is_windows()),
            ssh_default_system_shell: shell.to_string(),
            _temp_dir: temp_dir,
        })
    }

    /// What `SshRemoteConnection::build_command` computed before it went through the recipe.
    #[cfg(not(windows))]
    fn build_command_from_connection_fields(
        connection: &SshRemoteConnection,
        program: Option<String>,
        args: &[String],
        env: &HashMap<String, String>,
        working_dir: Option<String>,
        port_forward: Option<(u16, String, u16)>,
        interactive: Interactive,
    ) -> Result<CommandTemplate> {
        let build = if connection.ssh_platform.os.is_windows() {
            build_command_windows
        } else {
            build_command_posix
        };
        build(
            program,
            args,
            env,
            working_dir,
            port_forward,
            connection.socket.envs.clone(),
            connection.ssh_path_style,
            &connection.ssh_shell,
            connection.ssh_shell_kind,
            connection.socket.ssh_command_options(),
            &connection.socket.connection_options.ssh_destination(),
            interactive,
        )
    }

    #[cfg(not(windows))]
    fn assert_same_command(actual: &CommandTemplate, expected: &CommandTemplate) {
        assert_eq!(actual.program, expected.program);
        assert_eq!(actual.args, expected.args);
        assert_eq!(actual.env, expected.env);
    }

    #[cfg(not(windows))]
    fn assert_recipe_rebuilds_connection_commands(connection: &SshRemoteConnection) -> Result<()> {
        let recipe = connection
            .command_recipe()
            .context("an ssh connection must expose its recipe")?;
        // The browser only ever sees the recipe after it crossed the wire as JSON.
        let recipe: SshCommandRecipe = serde_json::from_str(&serde_json::to_string(&recipe)?)?;

        let mut env = HashMap::default();
        env.insert("INPUT_VAR".to_string(), "value with space".to_string());
        env.insert(
            crate::listening_ports::REMOTE_CONNECTION_ID_ENV_VAR.to_string(),
            "connection-7".to_string(),
        );
        let cases: Vec<(
            Option<String>,
            Vec<String>,
            Option<String>,
            Option<(u16, String, u16)>,
            Interactive,
        )> = vec![
            (None, vec![], None, None, Interactive::Yes),
            (
                Some("cargo".to_string()),
                vec!["test".to_string(), "it's quoted".to_string()],
                Some("~/work/project".to_string()),
                None,
                Interactive::No,
            ),
            (
                Some("/usr/bin/env".to_string()),
                vec![],
                Some("/srv/app".to_string()),
                Some((8080, "::1".to_string(), 80)),
                Interactive::Yes,
            ),
        ];
        for (program, args, working_dir, port_forward, interactive) in cases {
            let expected = build_command_from_connection_fields(
                connection,
                program.clone(),
                &args,
                &env,
                working_dir.clone(),
                port_forward.clone(),
                interactive,
            )?;
            let from_connection = connection.build_command(
                program.clone(),
                &args,
                &env,
                working_dir.clone(),
                port_forward.clone(),
                interactive,
            )?;
            let from_recipe = build_ssh_command(
                &recipe,
                program,
                &args,
                &env,
                working_dir,
                port_forward,
                interactive,
            )?;
            assert_same_command(&from_connection, &expected);
            assert_same_command(&from_recipe, &expected);
        }
        Ok(())
    }

    #[cfg(not(windows))]
    #[test]
    fn test_posix_recipe_builds_the_same_command_as_the_connection() -> Result<()> {
        let connection = test_connection(RemoteOs::Linux, "/bin/zsh", PathStyle::Unix)?;
        assert_recipe_rebuilds_connection_commands(&connection)?;

        let command = connection.build_command(
            None,
            &[],
            &HashMap::default(),
            None,
            None,
            Interactive::Yes,
        )?;
        let control_path = format!("ControlPath={}", connection.socket.socket_path.display());
        assert_eq!(
            command.args.iter().map(String::as_str).collect::<Vec<_>>(),
            [
                "-i",
                "/keys/id_ed25519",
                "-p",
                "2222",
                "-o",
                "ControlMaster=no",
                "-o",
                control_path.as_str(),
                "-o",
                "LogLevel=ERROR",
                "-t",
                "user@example.com",
                "cd && exec env /bin/zsh -l",
            ]
        );
        Ok(())
    }

    #[cfg(not(windows))]
    #[test]
    fn test_windows_recipe_builds_the_same_command_as_the_connection() -> Result<()> {
        let connection = test_connection(RemoteOs::Windows, "powershell.exe", PathStyle::Windows)?;
        assert_recipe_rebuilds_connection_commands(&connection)?;

        let recipe = connection
            .command_recipe()
            .context("an ssh connection must expose its recipe")?;
        assert!(recipe.is_windows);
        assert_eq!(recipe.path_style, RecipePathStyle::Windows);
        Ok(())
    }

    #[cfg(not(windows))]
    #[test]
    fn test_recipe_round_trips_through_json() -> Result<()> {
        let connection = test_connection(RemoteOs::MacOs, "/bin/bash", PathStyle::Unix)?;
        let mut recipe = connection
            .command_recipe()
            .context("an ssh connection must expose its recipe")?;
        recipe
            .env
            .insert("SSH_ASKPASS".to_string(), "/tmp/askpass.sh".to_string());

        let json = serde_json::to_value(&recipe)?;
        assert_eq!(json["path_style"], "unix");
        assert_eq!(json["destination"], "user@example.com");
        assert_eq!(json["is_windows"], false);
        let round_tripped: SshCommandRecipe = serde_json::from_value(json)?;
        assert_eq!(round_tripped, recipe);

        let windows: RecipePathStyle = serde_json::from_str("\"windows\"")?;
        assert_eq!(PathStyle::from(windows), PathStyle::Windows);
        Ok(())
    }

    #[test]
    fn test_no_bundled_server_without_a_provider() {
        // No test in this crate registers a provider, so the desktop path is what runs.
        for os in [RemoteOs::Linux, RemoteOs::MacOs, RemoteOs::Windows] {
            for arch in [RemoteArch::X86_64, RemoteArch::Aarch64] {
                assert!(
                    crate::transport::bundled_remote_server(RemotePlatform { os, arch }).is_none()
                );
            }
        }
    }

    #[test]
    fn test_bundled_version_uses_the_last_non_empty_line() {
        // A login shell can print a banner before the command's own output.
        assert!(bundled_version_matches("abc123\n", "abc123"));
        assert!(bundled_version_matches(
            "Welcome to devbox\nlast login: yesterday\nabc123\n\n",
            "abc123"
        ));
        assert!(bundled_version_matches("  abc123  \r\n", "abc123"));
        assert!(!bundled_version_matches("abc123\nnoise after\n", "abc123"));
        assert!(!bundled_version_matches("abc1234\n", "abc123"));
        assert!(!bundled_version_matches("", "abc123"));
        assert!(!bundled_version_matches("\n  \n", "abc123"));
    }

    #[test]
    fn test_bundled_server_file_name_is_decided_by_content() -> Result<()> {
        // Two builds of one commit differ in content, so the name must differ with it.
        let first = "0123456789abcdef".to_string() + &"0".repeat(48);
        let second = "0123456789abcdef".to_string() + &"f".repeat(48);
        assert_eq!(
            bundled_server_binary_name(&first, RemoteOs::Linux)?,
            "zed-remote-server-web-0123456789abcdef"
        );
        assert_eq!(
            bundled_server_binary_name(&first, RemoteOs::Windows)?,
            "zed-remote-server-web-0123456789abcdef.exe"
        );
        // Only the first 16 hex characters take part.
        assert_eq!(
            bundled_server_binary_name(&second, RemoteOs::MacOs)?,
            bundled_server_binary_name(&first, RemoteOs::MacOs)?
        );
        let other = "fedcba9876543210".to_string() + &"0".repeat(48);
        assert_ne!(
            bundled_server_binary_name(&other, RemoteOs::Linux)?,
            bundled_server_binary_name(&first, RemoteOs::Linux)?
        );
        // A real sha256 is 64 hex characters and must not be refused.
        let real = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(
            bundled_server_binary_name(real, RemoteOs::Linux)?,
            "zed-remote-server-web-e3b0c44298fc1c14"
        );
        assert!(bundled_server_binary_name("", RemoteOs::Linux).is_err());
        assert!(bundled_server_binary_name("0123456789abcde", RemoteOs::Linux).is_err());
        assert!(bundled_server_binary_name("../../evil0123456789", RemoteOs::Linux).is_err());
        Ok(())
    }

    #[gpui::test]
    async fn test_bundle_error_fails_the_install_without_a_desktop_fallback(
        cx: &mut gpui::TestAppContext,
    ) {
        // gpui::test drops a returned Result, so failures here must panic.
        let connection = match test_connection(RemoteOs::Linux, "/bin/bash", PathStyle::Unix) {
            Ok(connection) => connection,
            Err(error) => panic!("building the test connection failed: {error:#}"),
        };
        let delegate: Arc<dyn RemoteClientDelegate> =
            Arc::new(crate::transport::mock::MockDelegate);
        let mut async_cx = cx.to_async();

        // The connection's ssh socket does not exist, and MockDelegate panics if it is asked
        // to download anything, so only an early return can produce this exact error.
        let result = connection
            .ensure_bundled_server_binary(
                Err(anyhow::anyhow!(
                    "no remote server for linux-x86_64: run import-remote-server.sh"
                )),
                &delegate,
                &mut async_cx,
            )
            .await;
        match result {
            Ok(path) => panic!("a bundle error must fail the install, got {path:?}"),
            Err(error) => assert_eq!(
                format!("{error:#}"),
                "no remote server for linux-x86_64: run import-remote-server.sh"
            ),
        }
    }

    #[test]
    fn test_upload_nonces_differ() {
        assert_ne!(upload_nonce(), upload_nonce());
    }

    #[cfg(not(windows))]
    fn spawn_fake_master(script: &str) -> Result<Child> {
        Ok(util::command::new_command("sh")
            .arg("-c")
            .arg(script)
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?)
    }

    // A plain thread rather than a gpui timer: the fake master is a real process, which a
    // test dispatcher's virtual clock knows nothing about.
    #[cfg(not(windows))]
    fn elapsed_after(duration: Duration) -> impl std::future::Future<Output = ()> {
        let (sender, receiver) = futures::channel::oneshot::channel::<()>();
        std::thread::spawn(move || {
            std::thread::sleep(duration);
            sender.send(()).ok();
        });
        receiver.map(|_| ())
    }

    #[cfg(not(windows))]
    async fn read_stdout_to_end(process: &mut Child) -> Result<()> {
        let mut stdout = process.stdout.take().context("fake master has no stdout")?;
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).await?;
        Ok(())
    }

    // The sleep between closing stdout and exiting widens the window a real master that
    // rejected a password leaves between its stdout EOF and its reaping.
    #[cfg(not(windows))]
    #[test]
    fn test_master_exit_status_sees_a_master_that_exits_after_closing_stdout() -> Result<()> {
        smol::block_on(async {
            let mut process = spawn_fake_master("exec 1>&-; sleep 0.1; exit 7")?;
            read_stdout_to_end(&mut process).await?;
            let status =
                master_exit_status(&mut process, elapsed_after(Duration::from_secs(10))).await?;
            assert_eq!(status.and_then(|status| status.code()), Some(7));
            Ok(())
        })
    }

    #[cfg(not(windows))]
    #[test]
    fn test_master_exit_status_keeps_a_live_master_connected() -> Result<()> {
        smol::block_on(async {
            let mut process = spawn_fake_master("exec 1>&-; exec sleep 30")?;
            read_stdout_to_end(&mut process).await?;
            let started = Instant::now();
            let status = master_exit_status(&mut process, elapsed_after(MASTER_EXIT_GRACE)).await?;
            assert_eq!(status, None);
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "a live master must be reported after the grace period, took {:?}",
                started.elapsed()
            );
            assert_eq!(
                process.try_status()?,
                None,
                "the master must be left running"
            );
            Ok(())
        })
    }
}
