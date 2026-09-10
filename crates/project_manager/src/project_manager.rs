mod project_location;
mod project_manager_button;
mod project_manager_panel;

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use fs::Fs;
use gpui::{App, actions};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use workspace::Workspace;

pub use project_location::{ProjectLocation, parse_project_location, remote_project_uri};
pub use project_manager_button::ProjectManagerButton;
pub use project_manager_panel::ProjectManagerPanel;

actions!(
    project_manager,
    [
        /// Toggles focus on the project manager panel.
        ToggleFocus
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<ProjectManagerPanel>(window, cx);
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

/// Indices of the enabled projects whose name or tags match `query`.
pub fn filter_projects(projects: &[ProjectEntry], query: &str) -> Vec<usize> {
    let lowercase_query = query.trim().to_lowercase();
    projects
        .iter()
        .enumerate()
        .filter(|(_, project)| project.enabled && project.matches_query(&lowercase_query))
        .map(|(index, _)| index)
        .collect()
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
            vec![0, 1, 2],
            "the disabled project is never listed"
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
