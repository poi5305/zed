use anyhow::{Context as _, bail};
use fs::Fs;
use futures::{FutureExt, StreamExt as _, channel::mpsc, future::Shared};
use language::Buffer;
use remote::RemoteClient;
use rpc::proto::{self, REMOTE_SERVER_PROJECT_ID};
use std::{
    collections::VecDeque,
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::Arc,
};
use task::{Shell, shell_to_proto};
use util::{ResultExt, command::new_command};
use worktree::Worktree;

use collections::HashMap;
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Task, WeakEntity};
use settings::Settings as _;

use crate::{
    project_settings::{DirenvSettings, ProjectSettings},
    worktree_store::WorktreeStore,
};

pub struct ProjectEnvironment {
    cli_environment: Option<HashMap<String, String>>,
    local_environments: HashMap<(Shell, Arc<Path>), Shared<Task<Option<HashMap<String, String>>>>>,
    remote_environments: HashMap<(Shell, Arc<Path>), Shared<Task<Option<HashMap<String, String>>>>>,
    environment_error_messages: VecDeque<String>,
    environment_error_messages_tx: mpsc::UnboundedSender<String>,
    worktree_store: WeakEntity<WorktreeStore>,
    remote_client: Option<WeakEntity<RemoteClient>>,
    is_remote_project: bool,
    _tasks: Vec<Task<()>>,
}

pub enum ProjectEnvironmentEvent {
    ErrorsUpdated,
}

impl EventEmitter<ProjectEnvironmentEvent> for ProjectEnvironment {}

impl ProjectEnvironment {
    pub fn new(
        cli_environment: Option<HashMap<String, String>>,
        worktree_store: WeakEntity<WorktreeStore>,
        remote_client: Option<WeakEntity<RemoteClient>>,
        is_remote_project: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let (tx, mut rx) = mpsc::unbounded();
        let task = cx.spawn(async move |this, cx| {
            while let Some(message) = rx.next().await {
                this.update(cx, |this, cx| {
                    this.environment_error_messages.push_back(message);
                    cx.emit(ProjectEnvironmentEvent::ErrorsUpdated);
                })
                .ok();
            }
        });
        Self {
            cli_environment,
            local_environments: Default::default(),
            remote_environments: Default::default(),
            environment_error_messages: Default::default(),
            environment_error_messages_tx: tx,
            worktree_store,
            remote_client,
            is_remote_project,
            _tasks: vec![task],
        }
    }

    /// Returns the inherited CLI environment, if this project was opened from the Zed CLI.
    pub(crate) fn get_cli_environment(&self) -> Option<HashMap<String, String>> {
        if cfg!(any(test, feature = "test-support")) {
            return Some(HashMap::default());
        }
        if let Some(mut env) = self.cli_environment.clone() {
            set_origin_marker(&mut env, EnvironmentOrigin::Cli);
            Some(env)
        } else {
            None
        }
    }

    pub fn buffer_environment(
        &mut self,
        buffer: &Entity<Buffer>,
        worktree_store: &Entity<WorktreeStore>,
        cx: &mut Context<Self>,
    ) -> Shared<Task<Option<HashMap<String, String>>>> {
        if let Some(cli_environment) = self.get_cli_environment() {
            log::debug!("using project environment variables from CLI");
            return Task::ready(Some(cli_environment)).shared();
        }

        let Some(worktree) = buffer
            .read(cx)
            .file()
            .map(|f| f.worktree_id(cx))
            .and_then(|worktree_id| worktree_store.read(cx).worktree_for_id(worktree_id, cx))
        else {
            return Task::ready(None).shared();
        };
        self.worktree_environment(worktree, cx)
    }

    pub fn worktree_environment(
        &mut self,
        worktree: Entity<Worktree>,
        cx: &mut App,
    ) -> Shared<Task<Option<HashMap<String, String>>>> {
        if let Some(cli_environment) = self.get_cli_environment() {
            log::debug!("using project environment variables from CLI");
            return Task::ready(Some(cli_environment)).shared();
        }

        let worktree = worktree.read(cx);
        let mut abs_path = worktree.abs_path();
        if worktree.is_single_file() {
            let Some(parent) = abs_path.parent() else {
                return Task::ready(None).shared();
            };
            abs_path = parent.into();
        }

        let remote_client = self.remote_client.as_ref().and_then(|it| it.upgrade());
        match remote_client {
            Some(remote_client) => remote_client.clone().read(cx).shell().map(|shell| {
                self.remote_directory_environment(
                    &Shell::Program(shell),
                    abs_path,
                    remote_client,
                    cx,
                )
            }),
            None if self.is_remote_project => {
                Some(self.local_directory_environment(&Shell::System, abs_path, cx))
            }
            None => Some(self.local_directory_environment(&Shell::System, abs_path, cx)),
        }
        .unwrap_or_else(|| Task::ready(None).shared())
    }

    pub fn directory_environment(
        &mut self,
        abs_path: Arc<Path>,
        cx: &mut App,
    ) -> Shared<Task<Option<HashMap<String, String>>>> {
        let remote_client = self.remote_client.as_ref().and_then(|it| it.upgrade());
        match remote_client {
            Some(remote_client) => remote_client.clone().read(cx).shell().map(|shell| {
                self.remote_directory_environment(
                    &Shell::Program(shell),
                    abs_path,
                    remote_client,
                    cx,
                )
            }),
            None if self.is_remote_project => {
                Some(self.local_directory_environment(&Shell::System, abs_path, cx))
            }
            None => self
                .worktree_store
                .read_with(cx, |worktree_store, cx| {
                    worktree_store.find_worktree(&abs_path, cx)
                })
                .ok()
                .map(|_| self.local_directory_environment(&Shell::System, abs_path, cx)),
        }
        .unwrap_or_else(|| Task::ready(None).shared())
    }

    /// Returns the project environment using the default worktree path.
    /// This ensures that project-specific environment variables (e.g. from `.envrc`)
    /// are loaded from the project directory rather than the home directory.
    pub fn default_environment(
        &mut self,
        cx: &mut App,
    ) -> Shared<Task<Option<HashMap<String, String>>>> {
        let abs_path = self
            .worktree_store
            .read_with(cx, |worktree_store, cx| {
                crate::Project::default_visible_worktree_paths(worktree_store, cx)
                    .into_iter()
                    .next()
            })
            .ok()
            .flatten()
            .map(|path| Arc::<Path>::from(path))
            .unwrap_or_else(|| paths::home_dir().as_path().into());
        self.local_directory_environment(&Shell::System, abs_path, cx)
    }

    /// Returns the project environment, if possible.
    /// If the project was opened from the CLI, then the inherited CLI environment is returned.
    /// If it wasn't opened from the CLI, and an absolute path is given, then a shell is spawned in
    /// that directory, to get environment variables as if the user has `cd`'d there.
    pub fn local_directory_environment(
        &mut self,
        shell: &Shell,
        abs_path: Arc<Path>,
        cx: &mut App,
    ) -> Shared<Task<Option<HashMap<String, String>>>> {
        if let Some(cli_environment) = self.get_cli_environment() {
            log::debug!("using project environment variables from CLI");
            return Task::ready(Some(cli_environment)).shared();
        }

        self.local_environments
            .entry((shell.clone(), abs_path.clone()))
            .or_insert_with(|| {
                let load_direnv = ProjectSettings::get_global(cx).load_direnv.clone();
                let shell = shell.clone();
                let tx = self.environment_error_messages_tx.clone();
                let fs = self
                    .worktree_store
                    .read_with(cx, |worktree_store, _| worktree_store.fs())
                    .ok()
                    .flatten();
                cx.spawn(async move |cx| {
                    let mut shell_env = match cx
                        .background_spawn(load_directory_shell_environment(
                            shell,
                            abs_path.clone(),
                            load_direnv,
                            tx,
                            fs,
                        ))
                        .await
                    {
                        Ok(shell_env) => Some(shell_env),
                        Err(e) => {
                            log::error!(
                                "Failed to load shell environment for directory {abs_path:?}: {e:#}"
                            );
                            None
                        }
                    };

                    if let Some(shell_env) = shell_env.as_mut() {
                        let path = shell_env
                            .get("PATH")
                            .map(|path| path.as_str())
                            .unwrap_or_default();
                        log::debug!(
                            "using project environment variables shell launched in {:?}. PATH={:?}",
                            abs_path,
                            path
                        );

                        set_origin_marker(shell_env, EnvironmentOrigin::WorktreeShell);
                    }

                    shell_env
                })
                .shared()
            })
            .clone()
    }

    pub fn remote_directory_environment(
        &mut self,
        shell: &Shell,
        abs_path: Arc<Path>,
        remote_client: Entity<RemoteClient>,
        cx: &mut App,
    ) -> Shared<Task<Option<HashMap<String, String>>>> {
        if cfg!(any(test, feature = "test-support")) {
            return Task::ready(Some(HashMap::default())).shared();
        }

        self.remote_environments
            .entry((shell.clone(), abs_path.clone()))
            .or_insert_with(|| {
                let response =
                    remote_client
                        .read(cx)
                        .proto_client()
                        .request(proto::GetDirectoryEnvironment {
                            project_id: REMOTE_SERVER_PROJECT_ID,
                            shell: Some(shell_to_proto(shell.clone())),
                            directory: abs_path.to_string_lossy().to_string(),
                        });
                cx.background_spawn(async move {
                    let environment = response.await.log_err()?;
                    Some(environment.environment.into_iter().collect())
                })
                .shared()
            })
            .clone()
    }

    pub fn peek_environment_error(&self) -> Option<&String> {
        self.environment_error_messages.front()
    }

    pub fn pop_environment_error(&mut self) -> Option<String> {
        self.environment_error_messages.pop_front()
    }
}

fn set_origin_marker(env: &mut HashMap<String, String>, origin: EnvironmentOrigin) {
    env.insert(ZED_ENVIRONMENT_ORIGIN_MARKER.to_string(), origin.into());
}

const ZED_ENVIRONMENT_ORIGIN_MARKER: &str = "ZED_ENVIRONMENT";

enum EnvironmentOrigin {
    Cli,
    WorktreeShell,
}

impl From<EnvironmentOrigin> for String {
    fn from(val: EnvironmentOrigin) -> Self {
        match val {
            EnvironmentOrigin::Cli => "cli".into(),
            EnvironmentOrigin::WorktreeShell => "worktree-shell".into(),
        }
    }
}

async fn path_is_directory(abs_path: &Path, fs: Option<&dyn Fs>) -> anyhow::Result<bool> {
    #[cfg(target_family = "wasm")]
    {
        let Some(fs) = fs else {
            anyhow::bail!(
                "std::fs::Metadata cannot be synthesized on WASM; use RemoteFs / Fs::is_dir instead"
            );
        };
        return directory_from_fs_metadata(abs_path, fs).await;
    }

    #[cfg(not(target_family = "wasm"))]
    {
        if cfg!(any(test, feature = "test-support"))
            && let Some(fs) = fs
        {
            return directory_from_fs_metadata(abs_path, fs).await;
        }
        Ok(smol::fs::metadata(abs_path).await?.is_dir())
    }
}

async fn directory_from_fs_metadata(abs_path: &Path, fs: &dyn Fs) -> anyhow::Result<bool> {
    match fs.metadata(abs_path).await? {
        Some(metadata) => Ok(metadata.is_dir),
        None => anyhow::bail!("path does not exist"),
    }
}

async fn resolve_shell_environment_directory(
    abs_path: Arc<Path>,
    fs: Option<Arc<dyn Fs>>,
    tx: mpsc::UnboundedSender<String>,
) -> anyhow::Result<Arc<Path>> {
    let is_directory = path_is_directory(&abs_path, fs.as_deref())
        .await
        .with_context(|| {
            tx.unbounded_send(format!("Failed to open {}", abs_path.display()))
                .ok();
            format!("stat {abs_path:?}")
        })?;

    if is_directory {
        Ok(abs_path)
    } else {
        Ok(abs_path
            .parent()
            .with_context(|| {
                tx.unbounded_send(format!("Failed to open {}", abs_path.display()))
                    .ok();
                format!("getting parent of {abs_path:?}")
            })?
            .into())
    }
}

#[cfg(feature = "test-support")]
pub async fn resolve_shell_environment_directory_for_tests(
    abs_path: Arc<Path>,
    fs: Option<Arc<dyn Fs>>,
) -> anyhow::Result<Arc<Path>> {
    let (tx, _rx) = mpsc::unbounded();
    resolve_shell_environment_directory(abs_path, fs, tx).await
}

/// Locate a host binary via PATH.
///
/// On wasm32-unknown-unknown `std::env::split_paths` panics (`unsupported`),
/// and `which` uses it. Callers must pass `path_lookup_unsupported` rather
/// than calling `which` themselves.
pub(crate) fn lookup_system_binary(
    program: &str,
    search_paths: Option<&OsStr>,
    cwd: &Path,
) -> Option<PathBuf> {
    lookup_system_binary_impl(program, search_paths, cwd, cfg!(target_family = "wasm"))
}

/// Compiled on every target so native tests can pin the wasm disposition
/// without a wasm runtime.
pub(crate) fn lookup_system_binary_impl(
    program: &str,
    search_paths: Option<&OsStr>,
    cwd: &Path,
    path_lookup_unsupported: bool,
) -> Option<PathBuf> {
    if path_lookup_unsupported {
        return None;
    }
    match search_paths {
        Some(paths) => which::which_in(program, Some(paths), cwd).ok(),
        None => which::which(program).ok(),
    }
}

/// PATH lookup used by LSP/DAP `which()` after the shell environment is loaded.
///
/// On wasm `which::which_in` calls `std::env::split_paths` and panics. Returning
/// "not found" would skip a host rust-analyzer / rustup. Returning a host
/// absolute path would be a local lookup we cannot do. The bare program name
/// lets `Process::output` resolve PATH on the host, matching `direnv_spawn_path`.
pub(crate) fn lookup_adapter_binary(
    program: &OsStr,
    search_paths: Option<&OsStr>,
    cwd: &Path,
    path_lookup_unsupported: bool,
) -> Option<PathBuf> {
    if program.is_empty() {
        return None;
    }
    if path_lookup_unsupported {
        return Some(PathBuf::from(program));
    }
    match search_paths {
        Some(paths) => which::which_in(program, Some(paths), cwd).ok(),
        None => which::which(program).ok(),
    }
}

/// Program to spawn for `direnv export json`.
///
/// When PATH lookup is unsupported, spawn by name so `Process::output` can
/// resolve PATH on the host. Returning "not found" would drop a real host
/// direnv.
pub(crate) fn direnv_spawn_path(path_lookup_unsupported: bool) -> Option<PathBuf> {
    match lookup_system_binary_impl("direnv", None, Path::new("."), path_lookup_unsupported) {
        Some(path) => Some(path),
        None if path_lookup_unsupported => Some(PathBuf::from("direnv")),
        None => None,
    }
}

#[cfg(feature = "test-support")]
pub fn lookup_system_binary_impl_for_tests(
    program: &str,
    search_paths: Option<&OsStr>,
    cwd: &Path,
    path_lookup_unsupported: bool,
) -> Option<PathBuf> {
    lookup_system_binary_impl(program, search_paths, cwd, path_lookup_unsupported)
}

#[cfg(feature = "test-support")]
pub fn direnv_spawn_path_for_tests(path_lookup_unsupported: bool) -> Option<PathBuf> {
    direnv_spawn_path(path_lookup_unsupported)
}

#[cfg(feature = "test-support")]
pub fn lookup_adapter_binary_for_tests(
    program: &OsStr,
    search_paths: Option<&OsStr>,
    cwd: &Path,
    path_lookup_unsupported: bool,
) -> Option<PathBuf> {
    lookup_adapter_binary(program, search_paths, cwd, path_lookup_unsupported)
}

async fn load_directory_shell_environment(
    shell: Shell,
    abs_path: Arc<Path>,
    load_direnv: DirenvSettings,
    tx: mpsc::UnboundedSender<String>,
    fs: Option<Arc<dyn Fs>>,
) -> anyhow::Result<HashMap<String, String>> {
    if let DirenvSettings::Disabled = load_direnv {
        return Ok(HashMap::default());
    }

    let dir = resolve_shell_environment_directory(abs_path.clone(), fs, tx.clone()).await?;

    let (shell, args) = shell.program_and_args();
    let mut envs = util::shell_env::capture(shell.clone(), args, abs_path)
        .await
        .with_context(|| {
            tx.unbounded_send("Failed to load environment variables".into())
                .ok();
            format!("capturing shell environment with {shell:?}")
        })?;

    if cfg!(target_os = "windows")
        && let Some(path) = envs.remove("Path")
    {
        // windows env vars are case-insensitive, so normalize the path var
        // so we can just assume `PATH` in other places
        envs.insert("PATH".into(), path);
    }
    // If the user selects `Direct` for direnv, it would set an environment
    // variable that later uses to know that it should not run the hook.
    // We would include in `.envs` call so it is okay to run the hook
    // even if direnv direct mode is enabled.
    let direnv_environment = match load_direnv {
        DirenvSettings::ShellHook => None,
        DirenvSettings::Disabled => bail!("direnv integration is disabled"),
        // Note: direnv is not available on Windows, so we skip direnv processing
        // and just return the shell environment
        DirenvSettings::Direct if cfg!(target_os = "windows") => None,
        DirenvSettings::Direct => load_direnv_environment(&envs, &dir)
            .await
            .with_context(|| {
                tx.unbounded_send("Failed to load direnv environment".into())
                    .ok();
                "load direnv environment"
            })
            .log_err(),
    };
    if let Some(direnv_environment) = direnv_environment {
        for (key, value) in direnv_environment {
            if let Some(value) = value {
                envs.insert(key, value);
            } else {
                envs.remove(&key);
            }
        }
    }

    Ok(envs)
}

async fn load_direnv_environment(
    env: &HashMap<String, String>,
    dir: &Path,
) -> anyhow::Result<HashMap<String, Option<String>>> {
    let Some(direnv_path) = direnv_spawn_path(cfg!(target_family = "wasm")) else {
        return Ok(HashMap::default());
    };

    let args = &["export", "json"];
    let direnv_output = match new_command(&direnv_path)
        .args(args)
        .envs(env)
        .env("TERM", "dumb")
        .current_dir(dir)
        .output()
        .await
    {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HashMap::default());
        }
        result => result.context("running direnv")?,
    };

    if !direnv_output.status.success() {
        bail!(
            "Loading direnv environment failed ({}), stderr: {}",
            direnv_output.status,
            String::from_utf8_lossy(&direnv_output.stderr)
        );
    }

    let output = String::from_utf8_lossy(&direnv_output.stdout);
    if output.is_empty() {
        // direnv outputs nothing when it has no changes to apply to environment variables
        return Ok(HashMap::default());
    }

    serde_json::from_str(&output).context("parsing direnv json")
}
