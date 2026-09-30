mod project_location;
mod project_manager_button;
mod project_manager_panel;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use fs::Fs;
use gpui::{App, Context, Window, actions};
use recent_projects::open_remote_project;
use remote::RemoteConnectionOptions;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use util::ResultExt as _;
use workspace::notifications::NotifyTaskExt as _;
use workspace::{OpenMode, OpenOptions, Workspace};

pub use project_location::{
    ImportedPath, LaunchLocation, PUBLIC_KEY_HINT, ProjectLocation, SshCapabilities,
    import_vscode_path, launch_location, parse_project_location, remote_project_uri,
    remote_tab_paths,
};
pub use project_manager_button::ProjectManagerButton;
pub use project_manager_panel::ProjectManagerPanel;

actions!(
    project_manager,
    [
        /// Toggles focus on the project manager panel.
        ToggleFocus,
        /// Imports the projects of the VS Code "Project Manager" extension.
        ImportFromVsCode
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<ProjectManagerPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &ImportFromVsCode, _window, cx| {
            let Some(panel) = workspace.panel::<ProjectManagerPanel>(cx) else {
                return;
            };
            panel.update(cx, |panel, cx| panel.import_from_vscode(cx));
        });
    })
    .detach();
}

/// A single entry of the `projects.json` file, matching the format used by the
/// VS Code "Project Manager" extension.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectEntry {
    pub name: String,
    pub root_path: String,
    /// Additional folders opened alongside `root_path` as a multi-root workspace.
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

impl ProjectEntry {
    pub fn new(name: impl Into<String>, root_path: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            root_path: root_path.into(),
            paths: Vec::new(),
            tags: Vec::new(),
            enabled: true,
        }
    }

    /// Where this entry's folders live: on this machine, or behind a remote
    /// connection when they carry a `ssh://`, `wsl://` or `docker://` scheme.
    pub fn location(&self) -> Result<ProjectLocation> {
        parse_project_location(&self.root_path, &self.paths)
    }

    fn matches_query(&self, lowercase_query: &str) -> bool {
        if lowercase_query.is_empty() {
            return true;
        }
        self.name.to_lowercase().contains(lowercase_query)
            || self
                .tags
                .iter()
                .any(|tag| tag.to_lowercase().contains(lowercase_query))
    }
}

/// A tag heading plus the indices (into the source slice) of the projects under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectGroup {
    /// `None` is the group of projects that carry no tag at all.
    pub tag: Option<String>,
    pub entry_indices: Vec<usize>,
}

/// The tag stands in for the group in element ids; `None` needs a name of its own.
const UNTAGGED_GROUP_ID: &str = "untagged";

/// The GPUI element id of one project row. A project carrying several tags is
/// rendered once under each of them, so the id has to name the group as well or
/// the rows collide and share hover state and click handling.
pub fn project_entry_element_id(group: &ProjectGroup, index: usize) -> String {
    let tag = group.tag.as_deref().unwrap_or(UNTAGGED_GROUP_ID);
    format!("project-manager-entry-{tag}-{index}")
}

/// Indices of the enabled projects whose name or tags match `query`, ordered by name.
///
/// Ordered here rather than in the file: `projects.json` holds projects in the order they
/// were added, and an import writes them in the order of the file it read, neither of
/// which is an order a reader can look a project up in. Compared without case, so that
/// `alpha-notes` and `Alpha-notes` sit next to each other instead of in two blocks.
pub fn filter_projects(projects: &[ProjectEntry], query: &str) -> Vec<usize> {
    let lowercase_query = query.trim().to_lowercase();
    // Sorted as (name, index) pairs rather than by indexing back into `projects`, and the
    // index in the key is what settles two projects of the same name: the one the file
    // holds first stays first.
    let mut matches: Vec<(String, usize)> = projects
        .iter()
        .enumerate()
        .filter(|(_, project)| project.enabled && project.matches_query(&lowercase_query))
        .map(|(index, project)| (project.name.trim().to_lowercase(), index))
        .collect();
    matches.sort();
    matches.into_iter().map(|(_, index)| index).collect()
}

/// Groups the given projects by tag, sorted alphabetically, with the untagged
/// group last. A project with several tags is listed under each of them.
pub fn group_projects(projects: &[ProjectEntry], entry_indices: &[usize]) -> Vec<ProjectGroup> {
    let mut tagged: Vec<ProjectGroup> = Vec::new();
    let mut untagged: Vec<usize> = Vec::new();

    for &index in entry_indices {
        let Some(project) = projects.get(index) else {
            continue;
        };
        let mut tags: Vec<&String> = project.tags.iter().filter(|tag| !tag.is_empty()).collect();
        tags.sort();
        tags.dedup();
        if tags.is_empty() {
            untagged.push(index);
            continue;
        }
        for tag in tags {
            match tagged
                .iter_mut()
                .find(|group| group.tag.as_ref().is_some_and(|group_tag| group_tag == tag))
            {
                Some(group) => group.entry_indices.push(index),
                None => tagged.push(ProjectGroup {
                    tag: Some(tag.clone()),
                    entry_indices: vec![index],
                }),
            }
        }
    }

    tagged.sort_by(|left, right| left.tag.cmp(&right.tag));
    if !untagged.is_empty() {
        tagged.push(ProjectGroup {
            tag: None,
            entry_indices: untagged,
        });
    }
    tagged
}

/// What could be read out of `projects.json`: the entries that parsed, plus one
/// message for each entry that did not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedProjects {
    pub projects: Vec<ProjectEntry>,
    pub errors: Vec<String>,
}

/// Parses the contents of `projects.json`. An empty (or whitespace-only) file is
/// treated as an empty project list, and a file that is not a JSON array at all
/// is reported so the UI can surface it instead of silently showing nothing.
/// A single entry that cannot be read is reported on its own and leaves the
/// entries around it usable, matching how the panel renders one unparsable
/// project as a warning row rather than as an empty panel.
pub fn parse_projects(contents: &str) -> Result<ParsedProjects> {
    if contents.trim().is_empty() {
        return Ok(ParsedProjects::default());
    }
    let entries: Vec<serde_json_lenient::Value> =
        serde_json_lenient::from_str(contents).context("parsing projects.json")?;

    let mut parsed = ParsedProjects {
        projects: Vec::with_capacity(entries.len()),
        errors: Vec::new(),
    };
    for (position, entry) in entries.into_iter().enumerate() {
        match serde_json_lenient::from_value::<ProjectEntry>(entry) {
            Ok(project) => parsed.projects.push(project),
            Err(error) => parsed
                .errors
                .push(format!("projects.json entry {}: {error}", position + 1)),
        }
    }
    Ok(parsed)
}

/// The extension whose file format this panel's `projects.json` follows. Its storage
/// directory is named after the extension's id, and every editor built on VS Code keeps
/// it in the same place under its own application directory.
const VSCODE_PROJECT_MANAGER_STORAGE: &str = "alefragnani.project-manager";

/// The editors that run the extension. Each keeps its own copy, and a reader who moved
/// from one to another has projects in the one they left.
const VSCODE_FLAVORS: [&str; 5] = ["Code", "Code - Insiders", "Cursor", "VSCodium", "Windsurf"];

/// Where the VS Code "Project Manager" extension keeps its projects, newest flavor first.
///
/// Every path is returned whether or not it exists; the caller reads the ones that do.
/// The operating system whose VS Code layout should be read.
///
/// `cfg!` answers for the machine this was compiled for. On the web that is
/// wasm32-unknown-unknown, whose `target_os` is `unknown`, so every `cfg!` below would
/// fall through to the Linux layout while the files live on whatever the server runs.
/// The server reports its own `std::env::consts::OS` over `Home::dirs`, and the web
/// entry point seeds it here before anything reads it.
#[cfg(target_family = "wasm")]
static SERVER_OS: std::sync::OnceLock<String> = std::sync::OnceLock::new();

#[cfg(target_family = "wasm")]
pub fn set_server_os(os: impl Into<String>) {
    let _ = SERVER_OS.set(os.into());
}

/// How the browser opens another project: a browser tab holds a single GPUI window, so
/// the "new window" a click asks for has to be a new tab, and only the web entry point
/// can build that tab's URL and call `window.open`.
#[cfg(target_family = "wasm")]
static OPEN_IN_NEW_TAB: std::sync::OnceLock<fn(&[PathBuf]) -> anyhow::Result<()>> =
    std::sync::OnceLock::new();

#[cfg(target_family = "wasm")]
pub fn set_open_in_new_tab(open: fn(&[PathBuf]) -> anyhow::Result<()>) {
    if OPEN_IN_NEW_TAB.set(open).is_err() {
        log::warn!("project manager: the new-tab opener was already set");
    }
}

/// Asks the server what its ssh support is. Only the web entry point holds the connection
/// to that server, so it is handed in rather than reached from here.
#[cfg(target_family = "wasm")]
static SSH_CAPABILITIES_LOADER: std::sync::OnceLock<
    fn() -> futures::future::LocalBoxFuture<'static, anyhow::Result<SshCapabilities>>,
> = std::sync::OnceLock::new();

#[cfg(target_family = "wasm")]
pub fn set_ssh_capabilities_loader(
    load: fn() -> futures::future::LocalBoxFuture<'static, anyhow::Result<SshCapabilities>>,
) {
    if SSH_CAPABILITIES_LOADER.set(load).is_err() {
        log::warn!("project manager: the ssh capabilities loader was already set");
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn load_ssh_capabilities() -> anyhow::Result<SshCapabilities> {
    let load = SSH_CAPABILITIES_LOADER
        .get()
        .context("this build cannot ask the server about its ssh support")?;
    load().await
}

#[cfg(target_family = "wasm")]
pub(crate) fn open_in_new_tab(paths: &[PathBuf]) -> anyhow::Result<()> {
    let open = OPEN_IN_NEW_TAB
        .get()
        .context("this build cannot open a project in a new browser tab")?;
    open(paths)
}

fn host_os() -> &'static str {
    #[cfg(target_family = "wasm")]
    {
        SERVER_OS.get().map(String::as_str).unwrap_or("linux")
    }
    #[cfg(not(target_family = "wasm"))]
    {
        std::env::consts::OS
    }
}

pub fn vscode_project_files() -> Vec<PathBuf> {
    let home = paths::home_dir();
    let host_os = host_os();
    VSCODE_FLAVORS
        .iter()
        .map(|flavor| {
            let application = if host_os == "macos" {
                home.join("Library/Application Support").join(flavor)
            } else if host_os == "windows" {
                std::env::var("APPDATA")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| home.join("AppData/Roaming"))
                    .join(flavor)
            } else {
                home.join(".config").join(flavor)
            };
            application
                .join("User/globalStorage")
                .join(VSCODE_PROJECT_MANAGER_STORAGE)
                .join("projects.json")
        })
        .collect()
}

/// What reading a VS Code "Project Manager" file produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VsCodeImport {
    pub projects: Vec<ProjectEntry>,
    /// One line for each entry that was left out, and for each one that was imported with
    /// something worth saying about it.
    pub warnings: Vec<String>,
}

/// Reads a VS Code "Project Manager" file into entries this panel can store.
///
/// The two formats are the same file — this panel's `projects.json` follows that
/// extension's — so it is read with the same parser and only the paths are translated.
/// An entry whose folder cannot be translated is left out and reported rather than
/// failing the file: importing twenty-seven of twenty-eight projects, with a line saying
/// which one was dropped and why, is worth more than importing none of them.
pub fn import_vscode_projects(contents: &str) -> Result<VsCodeImport> {
    let parsed = parse_projects(contents).context("reading the VS Code project file")?;
    let mut import = VsCodeImport {
        projects: Vec::with_capacity(parsed.projects.len()),
        warnings: parsed.errors,
    };

    for project in parsed.projects {
        match import_project(project) {
            Ok((project, warnings)) => {
                import.projects.push(project);
                import.warnings.extend(warnings);
            }
            Err(error) => import.warnings.push(format!("{error:#}")),
        }
    }
    Ok(import)
}

/// One imported entry and whatever has to be said about the paths in it.
fn import_project(project: ProjectEntry) -> Result<(ProjectEntry, Vec<String>)> {
    let named = |error: anyhow::Error| error.context(format!("skipped \"{}\"", project.name));

    let mut warnings = Vec::new();
    let root = import_vscode_path(&project.root_path).map_err(named)?;
    let mut paths = Vec::with_capacity(project.paths.len());
    for path in &project.paths {
        let imported = import_vscode_path(path).map_err(named)?;
        warnings.extend(imported.warning);
        paths.push(imported.path);
    }
    warnings.extend(root.warning);

    Ok((
        ProjectEntry {
            name: project.name,
            root_path: root.path,
            paths,
            tags: project.tags,
            enabled: project.enabled,
        },
        warnings,
    ))
}

/// How many of an import's entries were new.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImportMerge {
    pub added: usize,
    /// Entries whose name is already in the list.
    pub skipped: usize,
}

/// Adds the imported projects that are not in `existing` already.
///
/// Matched on name, because that is what the reader calls a project and what they would
/// otherwise see twice in the panel. An entry already there is left exactly as it is: its
/// tags and its path may have been edited here since it was first imported, and running
/// the import again is not a reason to undo that.
pub fn merge_imported_projects(
    existing: &mut Vec<ProjectEntry>,
    imported: Vec<ProjectEntry>,
) -> ImportMerge {
    let mut merge = ImportMerge::default();
    for project in imported {
        let is_new = !existing
            .iter()
            .any(|held| held.name.trim().eq_ignore_ascii_case(project.name.trim()));
        if is_new {
            existing.push(project);
            merge.added += 1;
        } else {
            merge.skipped += 1;
        }
    }
    merge
}

/// The enabled project that `folder` on the machine behind `connection` (this machine
/// when `None`) belongs to: the one with a folder that is `folder` itself or, failing
/// that, the deepest folder containing it, so that a shell sitting in a subdirectory of a
/// project still finds it.
pub fn project_for_folder(
    projects: &[ProjectEntry],
    connection: Option<&RemoteConnectionOptions>,
    folder: &Path,
) -> Option<usize> {
    deepest_project_for_folder(projects, connection, folder, &HashMap::default())
}

/// [`project_for_folder`], with `resolved_folders` giving the symlink-free form of local
/// project folders: tmux reports a working directory already resolved, so a project saved
/// through a symlink only contains it in its resolved form.
fn deepest_project_for_folder(
    projects: &[ProjectEntry],
    connection: Option<&RemoteConnectionOptions>,
    folder: &Path,
    resolved_folders: &HashMap<PathBuf, PathBuf>,
) -> Option<usize> {
    let wanted_machine = connection.map(machine_key);
    let mut best: Option<(usize, usize)> = None;
    for (index, project) in projects.iter().enumerate() {
        if !project.enabled {
            continue;
        }
        let Ok(location) = project.location() else {
            continue;
        };
        let (machine, project_folders) = match location {
            ProjectLocation::Local(folders) => (None, folders),
            ProjectLocation::Remote { options, paths } => (Some(machine_key(&options)), paths),
        };
        if machine != wanted_machine {
            continue;
        }
        for project_folder in &project_folders {
            let resolved_folder = resolved_folders.get(project_folder);
            for candidate in std::iter::once(project_folder).chain(resolved_folder) {
                if !folder.starts_with(candidate) {
                    continue;
                }
                let depth = candidate.components().count();
                if best.is_none_or(|(_, best_depth)| depth > best_depth) {
                    best = Some((index, depth));
                }
            }
        }
    }
    best.map(|(index, _)| index)
}

/// What two connections share when they reach the same machine: the authority
/// `projects.json` would record for them, as reading it back normalizes it. A live
/// connection keeps its host as typed while one read from `projects.json` has it
/// lowercased, so comparing the written form alone would tell them apart. A connection
/// that cannot be written as a URI matches nothing but itself through its debug form.
fn machine_key(options: &RemoteConnectionOptions) -> String {
    let root = Path::new("/");
    let Some(uri) = remote_project_uri(options, root) else {
        return format!("{options:?}");
    };
    match parse_project_location(&uri, &[]) {
        Ok(ProjectLocation::Remote { options, .. }) => {
            remote_project_uri(&options, root).unwrap_or(uri)
        }
        _ => uri,
    }
}

/// What `open_folder_in_new_window` opens for `folder`: the saved project it belongs to,
/// or `folder` alone on the machine behind `connection`.
pub async fn location_for_folder(
    fs: &Arc<dyn Fs>,
    projects: &[ProjectEntry],
    connection: Option<RemoteConnectionOptions>,
    folder: PathBuf,
) -> ProjectLocation {
    let mut resolved_folders = HashMap::default();
    if connection.is_none() {
        for project in projects.iter().filter(|project| project.enabled) {
            let Ok(ProjectLocation::Local(project_folders)) = project.location() else {
                continue;
            };
            for project_folder in project_folders {
                // A saved folder that no longer exists cannot hold the working directory,
                // so failing to resolve it leaves only its written form to match.
                if let Ok(resolved_folder) = fs.canonicalize(&project_folder).await
                    && resolved_folder != project_folder
                {
                    resolved_folders.insert(project_folder, resolved_folder);
                }
            }
        }
    }

    let location =
        deepest_project_for_folder(projects, connection.as_ref(), &folder, &resolved_folders)
            .and_then(|index| projects.get(index))
            .and_then(|project| project.location().log_err());
    match (location, connection) {
        // The project is on the machine this window is connected to, and the live
        // connection carries what `projects.json` cannot: ssh args, nickname, port
        // forwards, a Podman container.
        (Some(ProjectLocation::Remote { paths, .. }), Some(options)) => {
            ProjectLocation::Remote { options, paths }
        }
        (Some(location), _) => location,
        (None, None) => ProjectLocation::Local(vec![folder]),
        (None, Some(options)) => ProjectLocation::Remote {
            options,
            paths: vec![folder],
        },
    }
}

/// Opens, in a window of its own, the saved project that `folder` belongs to, or `folder`
/// alone when no saved project holds it. `folder` is on the machine the workspace's
/// project is on, and so is what gets opened.
pub fn open_folder_in_new_window(
    workspace: &mut Workspace,
    folder: PathBuf,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let app_state = workspace.app_state().clone();
    let fs = app_state.fs.clone();
    let connection = workspace
        .project()
        .read(cx)
        .remote_client()
        .and_then(|client| client.read(cx).remote_connection())
        .map(|connection| connection.connection_options());

    let workspace_handle = workspace.weak_handle();
    cx.spawn_in(window, async move |workspace, cx| {
        let projects = match load_projects(&fs, paths::projects_file()).await {
            Ok(parsed) => parsed.projects,
            Err(error) => {
                log::warn!(
                    "reading projects.json to open {}: {error:#}",
                    folder.display()
                );
                Vec::new()
            }
        };
        let location = location_for_folder(&fs, &projects, connection, folder).await;

        match location {
            ProjectLocation::Local(open_paths) => {
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        workspace.open_workspace_for_paths(
                            OpenMode::NewWindow,
                            open_paths,
                            window,
                            cx,
                        )
                    })?
                    .await?;
            }
            ProjectLocation::Remote { options, paths } => {
                // Without a requesting window `open_remote_project` opens a new one.
                open_remote_project(options, paths, app_state, OpenOptions::default(), cx).await?;
            }
        }
        anyhow::Ok(())
    })
    .detach_and_notify_err(workspace_handle, window, cx);
}

pub fn serialize_projects(projects: &[ProjectEntry]) -> Result<String> {
    let mut contents =
        serde_json::to_string_pretty(projects).context("serializing projects.json")?;
    contents.push('\n');
    Ok(contents)
}

pub async fn load_projects(fs: &Arc<dyn Fs>, path: &Path) -> Result<ParsedProjects> {
    if !fs.is_file(path).await {
        return Ok(ParsedProjects::default());
    }
    let contents = fs
        .load(path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    parse_projects(&contents)
}

pub async fn save_projects(fs: &Arc<dyn Fs>, path: &Path, projects: &[ProjectEntry]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs.create_dir(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    fs.atomic_write(path.to_path_buf(), serialize_projects(projects)?)
        .await
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;

    fn folder_projects() -> Vec<ProjectEntry> {
        let mut disabled = ProjectEntry::new("disabled", "/work/zed/crates");
        disabled.enabled = false;
        vec![
            ProjectEntry::new("work", "/work"),
            ProjectEntry::new("zed", "/work/zed"),
            ProjectEntry::new("remote zed", "ssh://me@box/work/zed"),
            disabled,
            ProjectEntry::new("other box", "ssh://me@other/srv/app"),
        ]
    }

    fn ssh_connection(uri: &str) -> RemoteConnectionOptions {
        match parse_project_location(uri, &[]) {
            Ok(ProjectLocation::Remote { options, .. }) => options,
            other => panic!("{uri} is not a remote location: {other:?}"),
        }
    }

    #[test]
    fn a_folder_finds_the_deepest_enabled_project_on_its_own_machine() {
        let projects = folder_projects();
        let box_connection = ssh_connection("ssh://me@box/");

        let found = |connection: Option<&RemoteConnectionOptions>, folder: &str| {
            project_for_folder(&projects, connection, Path::new(folder))
                .and_then(|index| projects.get(index))
                .map(|project| project.name.as_str())
        };

        assert_eq!(found(None, "/work/zed"), Some("zed"));
        assert_eq!(
            found(None, "/work/zed/crates/gpui"),
            Some("zed"),
            "a subdirectory belongs to the deepest project containing it, and a disabled \
             project is skipped"
        );
        assert_eq!(found(None, "/work/other"), Some("work"));
        assert_eq!(
            found(None, "/workspace"),
            None,
            "a sibling whose name merely starts with a project's is not inside it"
        );
        assert_eq!(
            found(Some(&box_connection), "/work/zed/src"),
            Some("remote zed"),
            "on a remote machine only that machine's projects match"
        );
        assert_eq!(found(Some(&box_connection), "/srv/app"), None);
    }

    #[test]
    fn a_live_host_with_capitals_finds_the_projects_saved_for_it() {
        // Saving from a window connected to `MyBox` writes this URI, and reading it back
        // lowercases the host.
        let projects = vec![ProjectEntry::new("remote zed", "ssh://MyBox/work/zed")];
        let live = RemoteConnectionOptions::Ssh(remote::SshConnectionOptions {
            host: "MyBox".into(),
            ..Default::default()
        });

        assert_eq!(
            project_for_folder(&projects, Some(&live), Path::new("/work/zed/src")),
            Some(0),
            "the live host MyBox is the host projects.json records as ssh://MyBox"
        );
    }

    #[gpui::test]
    async fn a_matched_remote_project_opens_through_the_live_connection(
        cx: &mut gpui::TestAppContext,
    ) {
        let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());
        let projects = vec![ProjectEntry::new("remote zed", "ssh://box/work/zed")];
        let live = RemoteConnectionOptions::Ssh(remote::SshConnectionOptions {
            host: "box".into(),
            nickname: Some("dev box".to_string()),
            args: Some(vec!["-J".to_string(), "jump".to_string()]),
            ..Default::default()
        });

        assert_eq!(
            location_for_folder(
                &fs,
                &projects,
                Some(live.clone()),
                PathBuf::from("/work/zed/src")
            )
            .await,
            ProjectLocation::Remote {
                options: live,
                paths: vec![PathBuf::from("/work/zed")],
            },
            "the project is on the machine this window is connected to, so it opens through \
             the same connection settings rather than the bare authority in projects.json"
        );
    }

    #[gpui::test]
    async fn a_project_saved_through_a_symlink_holds_the_resolved_directory(
        cx: &mut gpui::TestAppContext,
    ) {
        let fake_fs = FakeFs::new(cx.executor());
        let fs: Arc<dyn Fs> = fake_fs.clone();
        fs.create_dir(Path::new("/Volumes/data/code/zed/crates"))
            .await
            .expect("creating the real project folder");
        fs.create_dir(Path::new("/home/me"))
            .await
            .expect("creating the home folder");
        fake_fs
            .insert_symlink("/home/me/code", PathBuf::from("/Volumes/data/code"))
            .await;
        let projects = vec![
            ProjectEntry::new("all code", "/Volumes/data/code"),
            ProjectEntry::new("zed", "/home/me/code/zed"),
        ];

        // tmux reports the directory resolved, never through the symlink.
        assert_eq!(
            location_for_folder(
                &fs,
                &projects,
                None,
                PathBuf::from("/Volumes/data/code/zed/crates")
            )
            .await,
            ProjectLocation::Local(vec![PathBuf::from("/home/me/code/zed")]),
            "/home/me/code/zed resolves to /Volumes/data/code/zed, the deepest project \
             holding the shell's directory"
        );
    }

    #[gpui::test]
    async fn normalizing_machines_and_folders_keeps_the_distinctions_that_matter(
        cx: &mut gpui::TestAppContext,
    ) {
        let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());
        let projects = vec![
            ProjectEntry::new("lowercase container", "docker://app/work"),
            ProjectEntry::new("gone", "/no/longer/here"),
        ];
        let capitalized_container =
            RemoteConnectionOptions::Docker(remote::DockerConnectionOptions {
                name: "App".to_string(),
                container_id: "App".to_string(),
                ..Default::default()
            });

        assert_eq!(
            project_for_folder(&projects, Some(&capitalized_container), Path::new("/work")),
            None,
            "container names are case-sensitive, so App is not the container app"
        );
        assert_eq!(
            location_for_folder(&fs, &projects, None, PathBuf::from("/no/longer/here/src")).await,
            ProjectLocation::Local(vec![PathBuf::from("/no/longer/here")]),
            "a saved folder that cannot be resolved still matches as written"
        );
    }

    /// The shape of a real export: local folders as plain paths, remote ones behind
    /// `vscode-remote://ssh-remote+<host>`, one of them a Coder workspace and one of them
    /// a folder on a Windows host.
    #[test]
    fn a_vscode_export_imports_its_hosts_as_ssh() {
        let import = import_vscode_projects(
            r#"[
                {"name":"BETA-images","rootPath":"/Users/user/go/src/github.com/example/images",
                 "paths":[],"tags":[],"enabled":true,"profile":""},
                {"name":"XR-scraper (DB2)",
                 "rootPath":"vscode-remote://ssh-remote+192.168.0.2/mnt/data/user/scraper",
                 "paths":[],"tags":[],"enabled":true,"profile":""},
                {"name":"BETA-dashboard (coder)",
                 "rootPath":"vscode-remote://ssh-remote+coder-vscode.coder.example.com--user--dev.main/home/coder/dashboard",
                 "paths":[],"tags":[],"enabled":true,"profile":""},
                {"name":"Ubuntu","rootPath":"vscode-remote://wsl+Ubuntu-22.04/home/user/work",
                 "paths":[],"tags":[],"enabled":true,"profile":""}
            ]"#,
        )
        .expect("a VS Code export is the same file shape");

        assert_eq!(
            import
                .projects
                .iter()
                .map(|project| project.root_path.as_str())
                .collect::<Vec<_>>(),
            vec![
                "/Users/user/go/src/github.com/example/images",
                "ssh://192.168.0.2/mnt/data/user/scraper",
                // A Coder workspace is reached through the host `coder config-ssh`
                // writes, so it needs no handling of its own.
                "ssh://coder-vscode.coder.example.com--user--dev.main/home/coder/dashboard",
                "wsl://Ubuntu-22.04/home/user/work",
            ]
        );
        assert!(
            import.warnings.is_empty(),
            "nothing about these needed saying: {:?}",
            import.warnings
        );
        assert!(
            import
                .projects
                .iter()
                .all(|project| project.location().is_ok()),
            "and every one of them parses back into a location to open"
        );
    }

    /// An entry that cannot be translated is left out with a line naming it, rather than
    /// failing the whole file: the reader has twenty-seven other projects in it.
    #[test]
    fn an_untranslatable_entry_is_reported_and_the_rest_imported() {
        let import = import_vscode_projects(
            r#"[
                {"name":"Container","rootPath":"vscode-remote://dev-container+7b2268/workspaces/app",
                 "paths":[],"tags":[],"enabled":true},
                {"name":"Kept","rootPath":"/Users/user/kept","paths":[],"tags":[],"enabled":true}
            ]"#,
        )
        .expect("the file itself is readable");

        assert_eq!(
            import
                .projects
                .iter()
                .map(|project| project.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Kept"]
        );
        assert_eq!(import.warnings.len(), 1, "{:?}", import.warnings);
        let warning = &import.warnings[0];
        assert!(
            warning.contains("Container") && warning.contains("dev-container"),
            "the line has to name the project and what was wrong with it: {warning}"
        );
    }

    /// A folder on a Windows host keeps the leading separator its URI needs, and the
    /// reader is told so rather than left to discover it when the folder does not open.
    #[test]
    fn a_windows_folder_behind_ssh_is_imported_with_a_warning() {
        let import = import_vscode_projects(
            r#"[{"name":"XR-xrpc (Win)",
                 "rootPath":"vscode-remote://ssh-remote+192.168.1.132/C:/Users/XR/workspace/xrpc",
                 "paths":[],"tags":[],"enabled":true}]"#,
        )
        .expect("readable");

        assert_eq!(
            import.projects[0].root_path,
            "ssh://192.168.1.132/C:/Users/XR/workspace/xrpc"
        );
        let warning = import
            .warnings
            .first()
            .unwrap_or_else(|| panic!("expected a warning, got {:?}", import.warnings));
        assert!(
            warning.contains("drive C"),
            "the warning has to say what is odd about it: {warning}"
        );
    }

    /// Running the import twice must not double the list, and must not undo edits made
    /// here to a project that was imported before.
    #[test]
    fn importing_twice_adds_only_what_is_new() {
        let mut existing = vec![ProjectEntry {
            name: "BETA-images".into(),
            root_path: "/somewhere/else".into(),
            paths: Vec::new(),
            tags: vec!["work".into()],
            enabled: true,
        }];
        let imported = vec![
            ProjectEntry::new("beta-images", "/Users/user/images"),
            ProjectEntry::new("R-scraper", "/Users/user/scraper"),
        ];

        let merge = merge_imported_projects(&mut existing, imported);

        assert_eq!(
            merge,
            ImportMerge {
                added: 1,
                skipped: 1
            }
        );
        assert_eq!(existing.len(), 2);
        assert_eq!(
            existing[0].root_path, "/somewhere/else",
            "the entry already held keeps the path it was edited to"
        );
        assert_eq!(
            existing[0].tags,
            vec!["work".to_string()],
            "and keeps its tags"
        );
        assert_eq!(existing[1].name, "R-scraper");
    }

    #[test]
    fn test_parse_projects_normal() {
        let projects = parse_projects(
            r#"[
                {
                    "name": "My Project",
                    "rootPath": "/abs/path",
                    "paths": ["/abs/other"],
                    "tags": ["work"],
                    "enabled": true
                },
                {
                    "name": "Minimal",
                    "rootPath": "/minimal"
                }
            ]"#,
        )
        .expect("valid projects.json should parse")
        .projects;

        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].name, "My Project");
        assert_eq!(projects[0].root_path, "/abs/path");
        assert_eq!(projects[0].paths, vec!["/abs/other".to_string()]);
        assert_eq!(projects[0].tags, vec!["work".to_string()]);
        assert!(projects[0].enabled);

        assert_eq!(projects[1].name, "Minimal");
        assert!(projects[1].paths.is_empty());
        assert!(projects[1].tags.is_empty());
        assert!(
            projects[1].enabled,
            "`enabled` should default to true when omitted"
        );
    }

    #[test]
    fn test_parse_projects_allows_comments_and_trailing_commas() {
        let projects = parse_projects(
            r#"[
                // the only project
                {
                    "name": "Commented",
                    "rootPath": "/c",
                },
            ]"#,
        )
        .expect("lenient parsing should accept comments and trailing commas")
        .projects;
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "Commented");
    }

    #[test]
    fn test_parse_projects_empty_contents() {
        assert_eq!(
            parse_projects("").expect("empty file is empty list"),
            ParsedProjects::default()
        );
        assert_eq!(
            parse_projects("   \n\t ").expect("blank file is empty list"),
            ParsedProjects::default()
        );
        assert_eq!(
            parse_projects("[]").expect("empty array"),
            ParsedProjects::default()
        );
    }

    #[test]
    fn test_parse_projects_malformed_returns_error() {
        let error = parse_projects("{ this is not projects.json }")
            .expect_err("malformed JSON must be reported, not swallowed");
        assert!(
            error.to_string().contains("projects.json"),
            "error should mention the file being parsed, got: {error}"
        );

        let parsed = parse_projects(r#"[{"rootPath": "/missing-name"}]"#)
            .expect("a well-formed array stays readable when one of its entries is not");
        assert!(
            parsed.projects.is_empty(),
            "the entry that is missing a name cannot become a project, got: {:?}",
            parsed.projects
        );
        assert_eq!(
            parsed.errors.len(),
            1,
            "the missing required field is reported instead of failing the whole file, got: {:?}",
            parsed.errors
        );
    }

    fn sample_projects() -> Vec<ProjectEntry> {
        let mut work = ProjectEntry::new("Zed", "/zed");
        work.tags = vec!["work".to_string(), "rust".to_string()];

        let mut personal = ProjectEntry::new("Dotfiles", "/dotfiles");
        personal.tags = vec!["personal".to_string()];

        let untagged = ProjectEntry::new("Scratch", "/scratch");

        let mut disabled = ProjectEntry::new("Archived", "/archived");
        disabled.tags = vec!["work".to_string()];
        disabled.enabled = false;

        vec![work, personal, untagged, disabled]
    }

    #[test]
    fn test_filter_projects_skips_disabled_and_matches_name_or_tag() {
        let projects = sample_projects();

        assert_eq!(
            filter_projects(&projects, ""),
            vec![1, 2, 0],
            "the disabled project is never listed, and the rest come out by name: \
             Dotfiles, Scratch, Zed"
        );
        assert_eq!(filter_projects(&projects, "dot"), vec![1]);
        assert_eq!(
            filter_projects(&projects, "RUST"),
            vec![0],
            "tag matching is case-insensitive"
        );
        assert_eq!(
            filter_projects(&projects, "work"),
            vec![0],
            "the disabled project matching the tag stays hidden"
        );
        assert_eq!(filter_projects(&projects, "nothing"), Vec::<usize>::new());
    }

    /// A list of twenty-odd projects in the order they happened to be added to the file
    /// is a list a reader has to scan; by name they can go straight to one.
    #[test]
    fn projects_are_listed_by_name_whatever_order_the_file_holds_them_in() {
        let projects = vec![
            ProjectEntry::new("zed-fc", "/zed-fc"),
            ProjectEntry::new("Alpha-notes", "/notes"),
            ProjectEntry::new("beta-dashboard", "/dashboard"),
            ProjectEntry::new("BETA-images", "/images"),
        ];

        let listed: Vec<&str> = filter_projects(&projects, "")
            .into_iter()
            .filter_map(|index| projects.get(index))
            .map(|project| project.name.as_str())
            .collect();

        assert_eq!(
            listed,
            vec!["Alpha-notes", "beta-dashboard", "BETA-images", "zed-fc"],
            "ordered by name without case, so that the two beta projects are together"
        );
    }

    #[test]
    fn test_group_projects_sorts_tags_and_puts_untagged_last() {
        let projects = sample_projects();
        let groups = group_projects(&projects, &filter_projects(&projects, ""));

        assert_eq!(
            groups,
            vec![
                ProjectGroup {
                    tag: Some("personal".to_string()),
                    entry_indices: vec![1],
                },
                ProjectGroup {
                    tag: Some("rust".to_string()),
                    entry_indices: vec![0],
                },
                ProjectGroup {
                    tag: Some("work".to_string()),
                    entry_indices: vec![0],
                },
                ProjectGroup {
                    tag: None,
                    entry_indices: vec![2],
                },
            ],
            "a multi-tag project appears under each of its tags"
        );
    }

    #[test]
    fn test_group_projects_without_any_tag() {
        let projects = vec![ProjectEntry::new("A", "/a"), ProjectEntry::new("B", "/b")];
        assert_eq!(
            group_projects(&projects, &filter_projects(&projects, "")),
            vec![ProjectGroup {
                tag: None,
                entry_indices: vec![0, 1],
            }]
        );
    }

    #[test]
    fn test_parse_projects_keeps_the_entries_it_can_read() {
        let parsed = parse_projects(
            r#"[
                {"name": "Good", "rootPath": "/good"},
                {"rootPath": "/missing-name"},
                {"name": "Wrong Type", "rootPath": 7},
                {"name": "Also Good", "rootPath": "/also-good"}
            ]"#,
        )
        .expect("a well-formed array is readable even when one of its entries is not");

        assert_eq!(
            parsed
                .projects
                .iter()
                .map(|project| project.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Good", "Also Good"],
            "one broken entry must not hide the entries around it"
        );
        assert_eq!(
            parsed.errors.len(),
            2,
            "both broken entries are reported, got: {:?}",
            parsed.errors
        );
        assert!(
            parsed.errors[0].contains('2'),
            "the report must say which entry is broken, got: {}",
            parsed.errors[0]
        );
    }

    #[test]
    fn test_element_ids_are_unique_across_tag_groups() {
        let projects = sample_projects();
        let groups = group_projects(&projects, &filter_projects(&projects, ""));

        let mut ids: Vec<String> = Vec::new();
        for group in &groups {
            for &index in &group.entry_indices {
                ids.push(project_entry_element_id(group, index));
            }
        }

        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            ids.len(),
            4,
            "three tagged rows plus one untagged row are rendered, got: {ids:?}"
        );
        assert_eq!(
            unique.len(),
            ids.len(),
            "a project listed under several tags is rendered once per tag, so each row needs its own element id, got: {ids:?}"
        );
    }

    #[gpui::test]
    async fn test_load_projects_missing_file(cx: &mut gpui::TestAppContext) {
        let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());
        let projects = load_projects(&fs, Path::new("/config/projects.json"))
            .await
            .expect("a missing projects.json is not an error");
        assert_eq!(projects, ParsedProjects::default());
    }

    #[gpui::test]
    async fn test_save_then_load_round_trip(cx: &mut gpui::TestAppContext) {
        let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());
        let path = Path::new("/config/projects.json");

        let mut project = ProjectEntry::new("Zed", "/zed");
        project.tags = vec!["work".to_string()];
        project.paths = vec!["/zed-docs".to_string()];
        let projects = vec![project];

        save_projects(&fs, path, &projects)
            .await
            .expect("writing projects.json");

        assert_eq!(
            load_projects(&fs, path)
                .await
                .expect("reading it back")
                .projects,
            projects
        );
    }

    #[gpui::test]
    async fn test_load_projects_malformed_file(cx: &mut gpui::TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/config", serde_json::json!({})).await;
        fs.atomic_write("/config/projects.json".into(), "{ nope".to_string())
            .await
            .expect("writing the malformed file");

        let fs: Arc<dyn Fs> = fs;
        load_projects(&fs, Path::new("/config/projects.json"))
            .await
            .expect_err("a malformed projects.json must surface an error");
    }
}
