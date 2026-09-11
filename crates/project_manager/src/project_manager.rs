mod project_location;
mod project_manager_button;
mod project_manager_panel;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use fs::Fs;
use gpui::{App, actions};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use workspace::Workspace;

pub use project_location::{
    ImportedPath, ProjectLocation, import_vscode_path, parse_project_location, remote_project_uri,
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
/// `andy-stocktw` and `Andy-stocktw` sit next to each other instead of in two blocks.
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
pub fn vscode_project_files() -> Vec<PathBuf> {
    let home = paths::home_dir();
    VSCODE_FLAVORS
        .iter()
        .map(|flavor| {
            let application = if cfg!(target_os = "macos") {
                home.join("Library/Application Support").join(flavor)
            } else if cfg!(target_os = "windows") {
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

    /// The shape of a real export: local folders as plain paths, remote ones behind
    /// `vscode-remote://ssh-remote+<host>`, one of them a Coder workspace and one of them
    /// a folder on a Windows host.
    #[test]
    fn a_vscode_export_imports_its_hosts_as_ssh() {
        let import = import_vscode_projects(
            r#"[
                {"name":"CDB-ewimg","rootPath":"/Users/andy/go/src/github.com/CreatorDB/ewimg",
                 "paths":[],"tags":[],"enabled":true,"profile":""},
                {"name":"XR-robotmon (DB2)",
                 "rootPath":"vscode-remote://ssh-remote+192.168.100.252/mnt/data/andy/robotmon",
                 "paths":[],"tags":[],"enabled":true,"profile":""},
                {"name":"CDB-agency (coder)",
                 "rootPath":"vscode-remote://ssh-remote+coder-vscode.coder.elggum.com--poi5305--andy.main/home/coder/dashboard",
                 "paths":[],"tags":[],"enabled":true,"profile":""},
                {"name":"Ubuntu","rootPath":"vscode-remote://wsl+Ubuntu-22.04/home/andy/work",
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
                "/Users/andy/go/src/github.com/CreatorDB/ewimg",
                "ssh://192.168.100.252/mnt/data/andy/robotmon",
                // A Coder workspace is reached through the host `coder config-ssh`
                // writes, so it needs no handling of its own.
                "ssh://coder-vscode.coder.elggum.com--poi5305--andy.main/home/coder/dashboard",
                "wsl://Ubuntu-22.04/home/andy/work",
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
                {"name":"Kept","rootPath":"/Users/andy/kept","paths":[],"tags":[],"enabled":true}
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
            name: "CDB-ewimg".into(),
            root_path: "/somewhere/else".into(),
            paths: Vec::new(),
            tags: vec!["work".into()],
            enabled: true,
        }];
        let imported = vec![
            ProjectEntry::new("cdb-ewimg", "/Users/andy/ewimg"),
            ProjectEntry::new("R-robotmon", "/Users/andy/robotmon"),
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
        assert_eq!(existing[1].name, "R-robotmon");
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
            ProjectEntry::new("Andy-stocktw", "/stocktw"),
            ProjectEntry::new("cdb-agency", "/agency"),
            ProjectEntry::new("CDB-ewimg", "/ewimg"),
        ];

        let listed: Vec<&str> = filter_projects(&projects, "")
            .into_iter()
            .filter_map(|index| projects.get(index))
            .map(|project| project.name.as_str())
            .collect();

        assert_eq!(
            listed,
            vec!["Andy-stocktw", "cdb-agency", "CDB-ewimg", "zed-fc"],
            "ordered by name without case, so that the two CDB projects are together"
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
