use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow};
use futures::{
    FutureExt as _, StreamExt as _,
    channel::{mpsc, oneshot},
};
use gpui::{App, AsyncApp};
use rand::RngCore as _;
use release_channel::{AppCommitSha, AppVersion};
use remote::{RemoteConnection, RemoteConnectionOptions};
use util::ResultExt as _;

/// How long a host with no handles keeps its ControlMaster. The browser's most common
/// disconnect is its WebSocket, not ssh, so a reconnect inside this window skips auth.
const HOST_GRACE: Duration = Duration::from_secs(30);

pub type BundledServerProvider = Arc<
    dyn Fn(remote::RemotePlatform) -> Option<Result<remote::BundledRemoteServer>> + Send + Sync,
>;

/// What a remote_server bundle hands the ssh host: the provider `ensure_server_binary`
/// consults, and the manifest facts `RemoteSsh::capabilities` reports.
#[derive(Clone)]
pub struct RemoteServerBundle {
    pub commit: String,
    /// `RemoteOs::as_str()-RemoteArch::as_str()`, e.g. `linux-x86_64`.
    pub platforms: Vec<String>,
    pub provider: BundledServerProvider,
}

/// The single hook point for the bundled remote_server (WP7): return the bundle read from
/// its manifest here. `None` leaves remote's own binary resolution in charge.
pub fn remote_server_bundle() -> Option<RemoteServerBundle> {
    use crate::remote_server_bundle::RemoteServerBundle as Manifest;
    let directory = Manifest::default_directory().log_err()?;
    let manifest = match Manifest::load(&directory) {
        Ok(manifest) => Arc::new(manifest),
        Err(error) => {
            tracing::warn!("ssh connections will not deploy a bundled remote server: {error:#}");
            return None;
        }
    };
    let commit = manifest.commit().to_string();
    let platforms = manifest
        .platforms()
        .into_iter()
        .map(|platform| format!("{}-{}", platform.os.as_str(), platform.arch.as_str()))
        .collect();
    Some(RemoteServerBundle {
        commit,
        platforms,
        provider: bundle_provider(manifest),
    })
}

fn bundle_provider(
    manifest: Arc<crate::remote_server_bundle::RemoteServerBundle>,
) -> BundledServerProvider {
    // Never `None`: a bundle that cannot serve a platform must fail the connect with the
    // reason, because the desktop flow it would fall back to cannot deploy a web build.
    Arc::new(move |platform| {
        Some(manifest.server_for(platform).inspect_err(|error| {
            tracing::error!("no bundled remote server for this host: {error:#}");
        }))
    })
}

/// Registered once, before any connect, because remote reads the provider from a global.
pub fn install_bundled_remote_server_provider(provider: Option<BundledServerProvider>) {
    if let Some(provider) = provider {
        remote::set_bundled_remote_server_provider(move |platform| provider(platform));
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleSummary {
    pub commit: String,
    pub platforms: Vec<String>,
}

/// The tokio side's only way to reach the gpui thread: `AsyncApp` is not `Send`, so every
/// request crosses as a `HostCommand` and is answered through the channel it carries.
#[derive(Clone)]
pub struct SshHostHandle {
    commands: mpsc::UnboundedSender<HostCommand>,
    bundle: Option<Arc<BundleSummary>>,
}

impl SshHostHandle {
    /// The receiving end is what `install` runs on the gpui thread; tests stand in for it.
    pub fn channel(bundle: Option<BundleSummary>) -> (Self, mpsc::UnboundedReceiver<HostCommand>) {
        let (commands, receiver) = mpsc::unbounded();
        (
            Self {
                commands,
                bundle: bundle.map(Arc::new),
            },
            receiver,
        )
    }

    pub fn send(&self, command: HostCommand) -> Result<()> {
        self.commands
            .unbounded_send(command)
            .map_err(|_| anyhow!("the ssh host has shut down"))
    }

    pub fn bundle(&self) -> Option<&BundleSummary> {
        self.bundle.as_deref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HostId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HandleId(pub String);

pub enum HostCommand {
    /// Registers `handle_id` on the host in the same step that connects, so the host's grace
    /// timer can never fire between the connect and the handle that keeps it alive.
    Connect {
        options: remote::SshConnectionOptions,
        handle_id: HandleId,
        delegate: Arc<crate::ssh_rpc::WebSshDelegate>,
        /// Resolving (sent or dropped) abandons the connect.
        cancel: oneshot::Receiver<()>,
        reply: oneshot::Sender<Result<ConnectedHost>>,
    },
    StartProxy {
        host_id: HostId,
        identifier: String,
        reconnect: bool,
        incoming_tx: mpsc::UnboundedSender<rpc::proto::Envelope>,
        outgoing_rx: mpsc::UnboundedReceiver<rpc::proto::Envelope>,
        /// Resolving (sent or dropped) kills the proxy.
        cancel: oneshot::Receiver<()>,
        exit: oneshot::Sender<Result<i32>>,
    },
    Release {
        handle_id: HandleId,
    },
}

#[derive(Clone, Debug)]
pub struct ConnectedHost {
    pub host_id: HostId,
    pub platform: remote::RemotePlatform,
    pub os_version: Option<String>,
    pub path_style: util::paths::PathStyle,
    pub shell: String,
    pub default_system_shell: String,
    pub recipe: remote::SshCommandRecipe,
}

/// Runs gpui on the calling thread, which must be the main thread: macOS only runs a
/// headless gpui there. The web server runs on `runtime` until it exits, and then gpui quits.
pub fn run_with_gpui(runtime: tokio::runtime::Runtime) -> Result<()> {
    let bundle = remote_server_bundle();
    let summary = bundle.as_ref().map(|bundle| BundleSummary {
        commit: bundle.commit.clone(),
        platforms: bundle.platforms.clone(),
    });
    install_bundled_remote_server_provider(bundle.map(|bundle| bundle.provider));

    let app = gpui_platform::headless();
    let (host, commands_rx) = SshHostHandle::channel(summary);
    let server = runtime.spawn(crate::run_with_ssh_host(host));
    let tokio_handle = runtime.handle().clone();
    let server_result = Rc::new(RefCell::new(None::<Result<()>>));

    app.run({
        let server_result = server_result.clone();
        move |cx| {
            release_channel::init(web_app_version(), cx);
            gpui_tokio::init_from_handle(cx, tokio_handle);
            crate::claude_sessions_rpc::set_background_executor(cx.background_executor().clone());
            let pool = install(commands_rx, cx);
            cx.spawn(async move |cx| {
                let result = match server.await {
                    Ok(result) => result,
                    Err(join_error) => {
                        Err(anyhow!(join_error).context("the web server task failed"))
                    }
                };
                // The process may exit before anything is dropped, and a master outliving the
                // server would keep an authenticated session open to the remote host.
                let connections = pool.borrow_mut().drain();
                for connection in connections {
                    connection.kill().await.log_err();
                }
                // A headless macOS gpui quits through `NSApp terminate`, which exits the process
                // with status 0 before `run` returns, so a failure has to leave from here.
                #[cfg(target_os = "macos")]
                if let Err(error) = &result {
                    eprintln!("Error: {error:?}");
                    std::process::exit(1);
                }
                server_result.replace(Some(result));
                cx.update(|cx| cx.quit());
            })
            .detach();
        }
    });

    server_result.take().unwrap_or(Ok(()))
}

fn web_app_version() -> semver::Version {
    AppVersion::load(
        env!("CARGO_PKG_VERSION"),
        option_env!("ZED_BUILD_ID"),
        option_env!("ZED_COMMIT_SHA").map(|sha| AppCommitSha::new(sha.to_owned())),
    )
}

type SharedPool = Rc<RefCell<HostPool<Arc<dyn RemoteConnection>>>>;

pub(crate) fn install(
    mut commands: mpsc::UnboundedReceiver<HostCommand>,
    cx: &mut App,
) -> SharedPool {
    let pool: SharedPool = Rc::new(RefCell::new(HostPool::default()));
    cx.spawn({
        let pool = pool.clone();
        async move |cx: &mut AsyncApp| {
            while let Some(command) = commands.next().await {
                // One task per command: a Connect that waits on a password prompt in one tab
                // must not hold back another tab's commands.
                let pool = pool.clone();
                cx.spawn(async move |cx| handle_command(command, pool, cx).await)
                    .detach();
            }
        }
    })
    .detach();
    pool
}

async fn handle_command(command: HostCommand, pool: SharedPool, cx: &mut AsyncApp) {
    match command {
        HostCommand::Connect {
            options,
            handle_id,
            delegate,
            cancel,
            reply,
        } => {
            let connected = futures::select_biased! {
                _ = cancel.fuse() => Err(anyhow!("the connect was cancelled")),
                connected = connect(&options, &handle_id, delegate, &pool, cx).fuse() => connected,
            };
            let succeeded = connected.is_ok();
            if reply.send(connected).is_err() {
                tracing::debug!("the requester stopped waiting for the ssh connect");
                if succeeded {
                    release(&handle_id, &pool, cx);
                }
            }
        }
        HostCommand::StartProxy {
            host_id,
            identifier,
            reconnect,
            incoming_tx,
            outgoing_rx,
            cancel,
            exit,
        } => {
            let result = start_proxy(
                &host_id,
                identifier,
                reconnect,
                incoming_tx,
                outgoing_rx,
                cancel,
                &pool,
                cx,
            )
            .await;
            if exit.send(result).is_err() {
                tracing::debug!("the relay stopped waiting for the proxy's exit");
            }
        }
        HostCommand::Release { handle_id } => release(&handle_id, &pool, cx),
    }
}

async fn connect(
    options: &remote::SshConnectionOptions,
    handle_id: &HandleId,
    delegate: Arc<dyn remote::RemoteClientDelegate>,
    pool: &SharedPool,
    cx: &mut AsyncApp,
) -> Result<ConnectedHost> {
    // remote's own pool hands back a connection whose master may have died since; only a
    // `-O check` can tell, and a dead one must be killed so remote dials a fresh master.
    let existing = pool.borrow().find(options);
    if let Some((host_id, connection)) = existing {
        let alive = match connection.command_recipe() {
            Some(recipe) => control_master_alive(options, &recipe).await,
            None => false,
        };
        if !alive {
            tracing::info!(host = %options.host.to_string(), "ssh master is gone; reconnecting");
            let removed = pool.borrow_mut().remove(&host_id);
            if let Some(connection) = removed {
                connection.kill().await.log_err();
            }
        }
    }

    let connection =
        remote::connect(RemoteConnectionOptions::Ssh(options.clone()), delegate, cx).await?;
    let recipe = connection
        .command_recipe()
        .context("the ssh connection did not describe its commands")?;
    let host_id = pool.borrow_mut().add_handle(
        options,
        connection.clone(),
        |left, right| Arc::ptr_eq(left, right),
        handle_id.clone(),
        new_host_id,
    );
    Ok(ConnectedHost {
        host_id,
        platform: connection.remote_platform(),
        os_version: connection.remote_os_version(),
        path_style: connection.path_style(),
        shell: connection.shell(),
        default_system_shell: connection.default_system_shell(),
        recipe,
    })
}

async fn start_proxy(
    host_id: &HostId,
    identifier: String,
    reconnect: bool,
    incoming_tx: mpsc::UnboundedSender<rpc::proto::Envelope>,
    outgoing_rx: mpsc::UnboundedReceiver<rpc::proto::Envelope>,
    cancel: oneshot::Receiver<()>,
    pool: &SharedPool,
    cx: &mut AsyncApp,
) -> Result<i32> {
    let connection = pool
        .borrow()
        .connection(host_id)
        .with_context(|| format!("unknown ssh host {}", host_id.0))?;
    // The browser's RemoteClient runs the heartbeat end to end, so activity here has no reader.
    let (activity_tx, _activity_rx) = mpsc::channel(1);
    let io_task = connection.start_proxy(
        identifier,
        reconnect,
        incoming_tx,
        outgoing_rx,
        activity_tx,
        Arc::new(crate::ssh_rpc::WebSshDelegate::detached()),
        cx,
    );
    futures::select_biased! {
        exit = io_task.fuse() => exit,
        // Dropping `io_task` kills the ssh proxy process (`kill_on_drop`).
        _ = cancel.fuse() => Err(anyhow!("the relay closed the channel")),
    }
}

fn release(handle_id: &HandleId, pool: &SharedPool, cx: &mut AsyncApp) {
    let Some((host_id, idle_generation)) = pool.borrow_mut().release(handle_id) else {
        return;
    };
    let pool = pool.clone();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(HOST_GRACE).await;
        let connection = pool.borrow_mut().remove_if_idle(&host_id, idle_generation);
        if let Some(connection) = connection {
            tracing::info!(host_id = %host_id.0, "closing idle ssh master");
            connection.kill().await.log_err();
        }
    })
    .detach();
}

async fn control_master_alive(
    options: &remote::SshConnectionOptions,
    recipe: &remote::SshCommandRecipe,
) -> bool {
    let Some(control_path) = control_path(recipe) else {
        return false;
    };
    let status = util::command::new_command("ssh")
        .args(options.additional_args())
        .args(["-O", "check"])
        .arg("-o")
        .arg(format!("ControlPath={control_path}"))
        .arg(options.ssh_destination())
        .stdin(util::command::Stdio::null())
        .stdout(util::command::Stdio::null())
        .stderr(util::command::Stdio::null())
        .output()
        .await;
    match status {
        Ok(output) => output.status.success(),
        Err(error) => {
            tracing::debug!(?error, "failed to run ssh -O check");
            false
        }
    }
}

fn control_path(recipe: &remote::SshCommandRecipe) -> Option<&str> {
    recipe
        .ssh_options
        .iter()
        .rev()
        .find_map(|option| option.strip_prefix("ControlPath="))
}

fn new_host_id() -> HostId {
    let mut bytes = [0_u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    HostId(format!("h-{}", hex::encode(bytes)))
}

/// Bookkeeping for the gpui-side hosts, generic over the connection so it can be tested
/// without a real ssh session.
pub(crate) struct HostPool<Connection> {
    hosts: HashMap<HostId, HostEntry<Connection>>,
    handle_hosts: HashMap<HandleId, HostId>,
}

struct HostEntry<Connection> {
    options: remote::SshConnectionOptions,
    connection: Connection,
    handles: HashSet<HandleId>,
    /// Bumped whenever the host gains a handle or goes idle, so a grace timer started for an
    /// earlier idle period cannot close a host that has since been used again.
    idle_generation: u64,
}

impl<Connection> Default for HostPool<Connection> {
    fn default() -> Self {
        Self {
            hosts: HashMap::new(),
            handle_hosts: HashMap::new(),
        }
    }
}

impl<Connection: Clone> HostPool<Connection> {
    pub(crate) fn find(
        &self,
        options: &remote::SshConnectionOptions,
    ) -> Option<(HostId, Connection)> {
        self.hosts
            .iter()
            .find(|(_, entry)| &entry.options == options)
            .map(|(host_id, entry)| (host_id.clone(), entry.connection.clone()))
    }

    pub(crate) fn add_handle(
        &mut self,
        options: &remote::SshConnectionOptions,
        connection: Connection,
        same_connection: impl Fn(&Connection, &Connection) -> bool,
        handle_id: HandleId,
        new_host_id: impl FnOnce() -> HostId,
    ) -> HostId {
        let existing = self
            .hosts
            .iter()
            .find(|(_, entry)| same_connection(&entry.connection, &connection))
            .map(|(host_id, _)| host_id.clone());
        let host_id = existing.unwrap_or_else(|| {
            let host_id = new_host_id();
            self.hosts.insert(
                host_id.clone(),
                HostEntry {
                    options: options.clone(),
                    connection,
                    handles: HashSet::new(),
                    idle_generation: 0,
                },
            );
            host_id
        });
        if let Some(entry) = self.hosts.get_mut(&host_id) {
            entry.handles.insert(handle_id.clone());
            entry.idle_generation += 1;
        }
        self.handle_hosts.insert(handle_id, host_id.clone());
        host_id
    }

    /// Returns the host and its idle generation when this was the host's last handle.
    pub(crate) fn release(&mut self, handle_id: &HandleId) -> Option<(HostId, u64)> {
        let host_id = self.handle_hosts.remove(handle_id)?;
        let entry = self.hosts.get_mut(&host_id)?;
        entry.handles.remove(handle_id);
        if !entry.handles.is_empty() {
            return None;
        }
        entry.idle_generation += 1;
        Some((host_id, entry.idle_generation))
    }

    pub(crate) fn remove_if_idle(
        &mut self,
        host_id: &HostId,
        idle_generation: u64,
    ) -> Option<Connection> {
        let entry = self.hosts.get(host_id)?;
        if !entry.handles.is_empty() || entry.idle_generation != idle_generation {
            return None;
        }
        self.remove(host_id)
    }

    pub(crate) fn remove(&mut self, host_id: &HostId) -> Option<Connection> {
        let entry = self.hosts.remove(host_id)?;
        for handle_id in &entry.handles {
            self.handle_hosts.remove(handle_id);
        }
        Some(entry.connection)
    }

    pub(crate) fn connection(&self, host_id: &HostId) -> Option<Connection> {
        self.hosts
            .get(host_id)
            .map(|entry| entry.connection.clone())
    }

    pub(crate) fn drain(&mut self) -> Vec<Connection> {
        self.handle_hosts.clear();
        self.hosts
            .drain()
            .map(|(_, entry)| entry.connection)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(host: &str) -> remote::SshConnectionOptions {
        remote::SshConnectionOptions {
            host: host.into(),
            ..Default::default()
        }
    }

    fn handle(name: &str) -> HandleId {
        HandleId(name.to_string())
    }

    fn fixed_host_id(name: &'static str) -> impl FnOnce() -> HostId {
        move || HostId(name.to_string())
    }

    fn same(left: &Arc<u32>, right: &Arc<u32>) -> bool {
        Arc::ptr_eq(left, right)
    }

    #[test]
    fn the_same_connection_shares_one_host() {
        let mut pool = HostPool::default();
        let connection = Arc::new(1);
        let first = pool.add_handle(
            &options("devbox"),
            connection.clone(),
            same,
            handle("k-1"),
            fixed_host_id("h-1"),
        );
        let second = pool.add_handle(
            &options("devbox"),
            connection,
            same,
            handle("k-2"),
            fixed_host_id("h-2"),
        );
        assert_eq!(first, HostId("h-1".to_string()));
        assert_eq!(
            second, first,
            "a second tab on the same master reuses its host"
        );

        let other = pool.add_handle(
            &options("devbox"),
            Arc::new(1),
            same,
            handle("k-3"),
            fixed_host_id("h-3"),
        );
        assert_eq!(
            other,
            HostId("h-3".to_string()),
            "a different connection object is a new host"
        );
    }

    #[test]
    fn a_host_goes_idle_only_after_its_last_handle() {
        let mut pool = HostPool::default();
        let connection = Arc::new(1);
        let host_id = pool.add_handle(
            &options("devbox"),
            connection.clone(),
            same,
            handle("k-1"),
            fixed_host_id("h-1"),
        );
        pool.add_handle(
            &options("devbox"),
            connection.clone(),
            same,
            handle("k-2"),
            fixed_host_id("h-2"),
        );

        assert_eq!(pool.release(&handle("k-1")), None);
        let (idle_host, generation) = pool
            .release(&handle("k-2"))
            .expect("last handle makes the host idle");
        assert_eq!(idle_host, host_id);
        assert_eq!(
            pool.release(&handle("k-2")),
            None,
            "a released handle is forgotten"
        );

        let removed = pool
            .remove_if_idle(&host_id, generation)
            .expect("idle host is removed");
        assert!(Arc::ptr_eq(&removed, &connection));
        assert!(pool.connection(&host_id).is_none());
    }

    #[test]
    fn a_host_used_again_during_its_grace_survives_the_timer() {
        let mut pool = HostPool::default();
        let connection = Arc::new(1);
        let host_id = pool.add_handle(
            &options("devbox"),
            connection.clone(),
            same,
            handle("k-1"),
            fixed_host_id("h-1"),
        );
        let (_, stale_generation) = pool.release(&handle("k-1")).expect("idle");
        pool.add_handle(
            &options("devbox"),
            connection,
            same,
            handle("k-2"),
            fixed_host_id("h-2"),
        );
        assert!(pool.remove_if_idle(&host_id, stale_generation).is_none());

        let (_, generation) = pool.release(&handle("k-2")).expect("idle again");
        assert!(
            pool.remove_if_idle(&host_id, stale_generation).is_none(),
            "the first timer is stale"
        );
        assert!(pool.remove_if_idle(&host_id, generation).is_some());
    }

    #[test]
    fn a_dead_host_is_found_by_options_and_removed_with_its_handles() {
        let mut pool = HostPool::default();
        let host_id = pool.add_handle(
            &options("devbox"),
            Arc::new(1),
            same,
            handle("k-1"),
            fixed_host_id("h-1"),
        );
        assert!(pool.find(&options("other")).is_none());
        let (found, _) = pool.find(&options("devbox")).expect("found by options");
        assert_eq!(found, host_id);

        assert!(pool.remove(&host_id).is_some());
        assert_eq!(
            pool.release(&handle("k-1")),
            None,
            "its handles went with it"
        );
        assert!(pool.find(&options("devbox")).is_none());
    }

    fn linux_x86_64() -> remote::RemotePlatform {
        remote::RemotePlatform {
            os: remote::RemoteOs::Linux,
            arch: remote::RemoteArch::X86_64,
        }
    }

    fn manifest_with_linux_x86_64(
        directory: &std::path::Path,
        content: &[u8],
    ) -> Arc<crate::remote_server_bundle::RemoteServerBundle> {
        use sha2::Digest as _;
        let sha256 = hex::encode(sha2::Sha256::digest(content));
        std::fs::write(directory.join("zed-remote-server-linux-x86_64"), content)
            .expect("writing the bundled binary");
        let text = format!(
            r#"{{"commit": "abc123", "binaries": [{{"os": "linux", "arch": "x86_64",
                "file": "zed-remote-server-linux-x86_64", "sha256": "{sha256}"}}]}}"#
        );
        match crate::remote_server_bundle::RemoteServerBundle::parse(directory, &text) {
            Ok(manifest) => Arc::new(manifest),
            Err(error) => panic!("the test manifest must parse: {error:#}"),
        }
    }

    #[test]
    fn provider_fails_a_platform_the_manifest_lacks_instead_of_declining() {
        let directory = tempfile::tempdir().expect("tempdir");
        let provider = bundle_provider(manifest_with_linux_x86_64(directory.path(), b"linux"));
        let macos = remote::RemotePlatform {
            os: remote::RemoteOs::MacOs,
            arch: remote::RemoteArch::Aarch64,
        };
        // None would send ensure_server_binary down the desktop flow, which cannot work here.
        let answer = provider(macos);
        assert!(
            answer.is_some(),
            "a bundle that lacks the platform must answer Some(Err), not None"
        );
        let message = match answer {
            Some(Err(error)) => format!("{error:#}"),
            Some(Ok(server)) => panic!("macos-aarch64 is not bundled, got {server:?}"),
            None => panic!("unreachable: checked above"),
        };
        assert!(
            message.contains("web/scripts/import-remote-server.sh"),
            "{message}"
        );
        assert!(message.contains("macos-aarch64"), "{message}");
    }

    #[test]
    fn provider_fails_a_binary_whose_hash_changed() {
        let directory = tempfile::tempdir().expect("tempdir");
        let provider = bundle_provider(manifest_with_linux_x86_64(directory.path(), b"linux"));
        std::fs::write(
            directory.path().join("zed-remote-server-linux-x86_64"),
            b"tampered",
        )
        .expect("rewriting the bundled binary");
        let answer = provider(linux_x86_64());
        assert!(
            answer.is_some(),
            "a binary that no longer matches the manifest must answer Some(Err), not None"
        );
        let message = match answer {
            Some(Err(error)) => format!("{error:#}"),
            Some(Ok(server)) => panic!("the binary was tampered with, got {server:?}"),
            None => panic!("unreachable: checked above"),
        };
        assert!(message.contains("sha256"), "{message}");
        assert!(
            message.contains("web/scripts/import-remote-server.sh"),
            "{message}"
        );
    }

    #[test]
    fn provider_serves_a_bundled_platform() {
        let directory = tempfile::tempdir().expect("tempdir");
        let provider = bundle_provider(manifest_with_linux_x86_64(directory.path(), b"linux"));
        match provider(linux_x86_64()) {
            Some(Ok(server)) => {
                assert_eq!(server.version, "abc123");
                assert_eq!(server.content_id.len(), 64);
            }
            Some(Err(error)) => panic!("a bundled platform must be served: {error:#}"),
            None => panic!("a bundle must never decline"),
        }
    }

    #[test]
    fn control_path_comes_from_the_recipe() {
        let recipe = remote::SshCommandRecipe {
            ssh_options: vec![
                "-p".to_string(),
                "2222".to_string(),
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
        };
        assert_eq!(
            control_path(&recipe),
            Some("/tmp/zed-ssh-sessionAbC/ssh.sock")
        );
        let without = remote::SshCommandRecipe {
            ssh_options: Vec::new(),
            ..recipe
        };
        assert_eq!(control_path(&without), None);
    }
}
