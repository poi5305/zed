use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use editor::{Editor, EditorElement, EditorEvent, EditorStyle};
use fs::Fs;
use futures::StreamExt as _;
use gpui::{
    AsyncApp, AsyncWindowContext, Entity, EventEmitter, FocusHandle, Focusable, FontStyle,
    PathPromptOptions, Pixels, Render, SharedString, Subscription, Task, TextStyle, WeakEntity,
    div, px, relative, rems,
};
use recent_projects::open_remote_project;
use remote::RemoteConnectionOptions;
use rope::Rope;
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::{Icon, ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    MultiWorkspace, OpenMode, OpenOptions, Workspace, create_and_open_local_file,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::{
    ImportMerge, ProjectEntry, ProjectGroup, ProjectLocation, ToggleFocus, filter_projects,
    group_projects, import_vscode_projects, load_projects, merge_imported_projects,
    project_entry_element_id, remote_project_uri, save_projects, vscode_project_files,
};

const PROJECT_MANAGER_PANEL_KEY: &str = "ProjectManagerPanel";
const FS_WATCH_LATENCY: Duration = Duration::from_millis(100);

/// What a `projects.json` that does not exist yet is created with.
const EMPTY_PROJECT_LIST: &str = "[]\n";

pub struct ProjectManagerPanel {
    workspace: WeakEntity<Workspace>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    filter_editor: Entity<Editor>,
    projects: Vec<ProjectEntry>,
    project_rows: Vec<ProjectRow>,
    load_error: Option<SharedString>,
    /// What the last import did, a line at a time. Kept apart from `load_error` because
    /// an import that reports something is not an import that failed.
    notice: Vec<SharedString>,
    position: DockPosition,
    reload_task: Task<()>,
    save_task: Task<()>,
    import_task: Task<()>,
    edit_task: Task<()>,
    _watch_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl ProjectManagerPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            ProjectManagerPanel::new(workspace, window, cx)
        })
    }

    pub fn new(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let workspace_handle = workspace.weak_handle();
        let fs = workspace.app_state().fs.clone();

        cx.new(|cx| {
            let filter_editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text("Filter projects…", window, cx);
                editor
            });

            let subscriptions = vec![cx.subscribe(
                &filter_editor,
                |_: &mut Self, _, event: &EditorEvent, cx| {
                    if matches!(event, EditorEvent::BufferEdited) {
                        cx.notify();
                    }
                },
            )];

            let watch_task = cx.spawn({
                let fs = fs.clone();
                async move |this: WeakEntity<Self>, cx| {
                    let path = paths::projects_file().clone();
                    let (mut events, _watcher) = fs.watch(&path, FS_WATCH_LATENCY).await;
                    while let Some(batch) = events.next().await {
                        if !batch.iter().any(|event| event.path == path) {
                            continue;
                        }
                        if this.update(cx, |this, cx| this.reload(cx)).is_err() {
                            break;
                        }
                    }
                }
            });

            let mut this = Self {
                workspace: workspace_handle,
                fs,
                focus_handle: cx.focus_handle(),
                filter_editor,
                projects: Vec::new(),
                project_rows: Vec::new(),
                load_error: None,
                notice: Vec::new(),
                position: DockPosition::Left,
                reload_task: Task::ready(()),
                save_task: Task::ready(()),
                import_task: Task::ready(()),
                edit_task: Task::ready(()),
                _watch_task: watch_task,
                _subscriptions: subscriptions,
            };
            this.reload(cx);
            this
        })
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.notice.clear();
        let fs = self.fs.clone();
        self.reload_task = cx.spawn(async move |this, cx| {
            let result = load_projects(&fs, paths::projects_file()).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(parsed) => {
                        this.set_projects(parsed.projects);
                        this.load_error = entry_errors_message(&parsed.errors);
                    }
                    Err(error) => {
                        this.load_error = Some(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn set_projects(&mut self, projects: Vec<ProjectEntry>) {
        self.project_rows = projects.iter().map(project_row).collect();
        self.projects = projects;
    }

    fn filter_query(&self, cx: &App) -> String {
        self.filter_editor.read(cx).text(cx)
    }

    fn open_project(
        &mut self,
        index: usize,
        new_window: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get(index) else {
            return;
        };
        let location = match project.location() {
            Ok(location) => location,
            Err(error) => {
                self.report_error(error, cx);
                return;
            }
        };

        match location {
            ProjectLocation::Local(open_paths) => {
                if open_paths.is_empty() {
                    return;
                }
                self.workspace
                    .update(cx, |workspace, cx| {
                        workspace
                            .open_workspace_for_paths(
                                open_mode_for_window(new_window),
                                open_paths,
                                window,
                                cx,
                            )
                            .detach_and_log_err(cx);
                    })
                    .log_err();
            }
            ProjectLocation::Remote { options, paths } => {
                if paths.is_empty() {
                    return;
                }
                self.open_remote(options, paths, new_window, window, cx);
            }
        }
    }

    fn open_remote(
        &mut self,
        options: RemoteConnectionOptions,
        paths: Vec<PathBuf>,
        new_window: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let app_state = workspace.read(cx).app_state().clone();
        // Without a requesting window `open_remote_project` opens a new one.
        let requesting_window = if new_window {
            None
        } else {
            window.window_handle().downcast::<MultiWorkspace>()
        };

        cx.spawn(async move |this, cx| {
            let opened = open_remote_project(
                options,
                paths,
                app_state,
                OpenOptions {
                    requesting_window,
                    ..Default::default()
                },
                cx,
            )
            .await;
            if let Err(error) = opened {
                this.update(cx, |this, cx| this.report_error(error, cx))
                    .log_err();
            }
        })
        .detach();
    }

    fn report_error(&mut self, error: anyhow::Error, cx: &mut Context<Self>) {
        self.load_error = Some(format!("{error:#}").into());
        cx.notify();
    }

    /// Opens `projects.json` in an editor.
    ///
    /// Through the same path Zed opens its own settings file by: the file is on this
    /// machine, and a window whose project is on a remote host has to be handed a local
    /// workspace to open it in. Opening it through the remote window asks the host for a
    /// path that only exists here, and asking `open_paths` for a local window is not
    /// enough either — a window holding both a local and a remote workspace counts as
    /// local while its active workspace is still the remote one, which is how the path
    /// reached the host again. `with_local_or_wsl_workspace`, which
    /// [`create_and_open_local_file`] goes through, is what actually switches workspaces.
    fn edit_projects_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

        let opened = workspace.update(cx, |_workspace, cx| {
            create_and_open_local_file(paths::projects_file().as_path(), window, cx, || {
                // An empty list rather than an empty file: the panel reads this back,
                // and a project file that is not a JSON array at all is an error it
                // would have to report.
                Rope::from(EMPTY_PROJECT_LIST)
            })
        });

        self.edit_task = cx.spawn(async move |this, cx| {
            // Said in the panel, not only in the log: a button that reports its failure
            // to a file the reader is not watching is a button that does nothing.
            if let Err(error) = opened.await {
                this.update(cx, |this, cx| this.report_error(error, cx))
                    .log_err();
            }
        });
    }

    /// Every visible worktree root of the current workspace, which is what "Save
    /// Current Project" records, together with the connection they are reached
    /// through when the workspace is remote.
    fn current_workspace_roots(
        &self,
        cx: &App,
    ) -> Option<(Option<RemoteConnectionOptions>, Vec<PathBuf>)> {
        let workspace = self.workspace.upgrade()?;
        let project = workspace.read(cx).project().read(cx);
        let roots: Vec<PathBuf> = project
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            .collect();
        if roots.is_empty() {
            return None;
        }
        let connection = project
            .remote_client()
            .and_then(|client| client.read(cx).remote_connection())
            .map(|connection| connection.connection_options());
        Some((connection, roots))
    }

    /// Imports the projects of the VS Code "Project Manager" extension, whose file
    /// format this panel's own `projects.json` follows.
    ///
    /// Read from the extension's own storage when one of the editors that runs it has
    /// written a file there, and asked for when none has: that directory is several
    /// levels deep inside an application support folder, so finding it is worth more than
    /// a file picker, while a reader who keeps an export of their own still gets one.
    pub fn import_from_vscode(&mut self, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        let candidates = vscode_project_files();

        // Its own task slot: dropping a `Task` cancels it, so sharing one with the reload
        // the projects.json watcher triggers would abort the import half-written.
        self.import_task = cx.spawn(async move |this, cx| {
            let source = match first_project_file(&fs, candidates).await {
                Some(source) => source,
                None => match ask_for_a_project_file(cx).await {
                    Ok(Some(source)) => source,
                    // A dialog the reader closed is not an error to report back to them.
                    Ok(None) => return,
                    Err(error) => {
                        this.update(cx, |this, cx| this.report_error(error, cx))
                            .log_err();
                        return;
                    }
                },
            };

            let imported = async {
                let contents = fs
                    .load(&source)
                    .await
                    .with_context(|| format!("reading {}", source.display()))?;
                let import = import_vscode_projects(&contents)?;

                let path = paths::projects_file().clone();
                let mut parsed = load_projects(&fs, &path).await?;
                let merge = merge_imported_projects(&mut parsed.projects, import.projects);
                if merge.added > 0 {
                    save_projects(&fs, &path, &parsed.projects).await?;
                }
                anyhow::Ok((parsed, merge, import.warnings))
            }
            .await;

            this.update(cx, |this, cx| {
                match imported {
                    Ok((parsed, merge, warnings)) => {
                        this.set_projects(parsed.projects);
                        this.load_error = entry_errors_message(&parsed.errors);
                        this.notice = import_notice(&source, merge, &warnings);
                    }
                    Err(error) => {
                        this.load_error = Some(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn save_current_project(&mut self, cx: &mut Context<Self>) {
        let Some((connection, roots)) = self.current_workspace_roots(cx) else {
            self.load_error = Some("No folder is open in this window.".into());
            cx.notify();
            return;
        };
        let entry = match project_entry_for_roots(connection.as_ref(), &roots) {
            Ok(entry) => entry,
            Err(error) => {
                self.load_error = Some(format!("{error:#}").into());
                cx.notify();
                return;
            }
        };

        let fs = self.fs.clone();
        // Saving must not share a task slot with `reload`: dropping a `Task`
        // cancels it, so a refresh, a panel activation or the projects.json
        // watcher landing first would abort the write with no error reported.
        self.save_task = cx.spawn(async move |this, cx| {
            let path = paths::projects_file().clone();
            let saved = async {
                let mut parsed = load_projects(&fs, &path).await?;
                if !parsed
                    .projects
                    .iter()
                    .any(|project| project.root_path == entry.root_path)
                {
                    parsed.projects.push(entry);
                    save_projects(&fs, &path, &parsed.projects).await?;
                }
                anyhow::Ok(parsed)
            }
            .await;

            this.update(cx, |this, cx| {
                match saved {
                    Ok(parsed) => {
                        this.set_projects(parsed.projects);
                        this.load_error = entry_errors_message(&parsed.errors);
                    }
                    Err(error) => {
                        this.load_error = Some(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn render_filter_input(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = ThemeSettings::get_global(cx);
        let text_style = TextStyle {
            color: cx.theme().colors().text,
            font_family: settings.ui_font.family.clone(),
            font_features: settings.ui_font.features.clone(),
            font_fallbacks: settings.ui_font.fallbacks.clone(),
            font_size: rems(0.875).into(),
            font_weight: settings.ui_font.weight,
            font_style: FontStyle::Normal,
            line_height: relative(1.3),
            ..Default::default()
        };

        EditorElement::new(
            &self.filter_editor,
            EditorStyle {
                local_player: cx.theme().players().local(),
                text: text_style,
                ..Default::default()
            },
        )
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_px()
            .child(
                IconButton::new("project-manager-edit", IconName::Pencil)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Edit Projects"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.edit_projects_file(window, cx);
                    })),
            )
            .child(
                IconButton::new("project-manager-save", IconName::FolderAdd)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Save Current Project"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.save_current_project(cx);
                    })),
            )
            .child(
                IconButton::new("project-manager-import", IconName::Download)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Import Projects from VS Code"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.import_from_vscode(cx);
                    })),
            )
            .child(
                IconButton::new("project-manager-refresh", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Refresh"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.reload(cx);
                    })),
            )
    }

    fn render_project(
        &self,
        group: &ProjectGroup,
        index: usize,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let project = self.projects.get(index)?;
        let row = self.project_rows.get(index)?;
        let name: SharedString = project.name.clone().into();
        let element_id = SharedString::from(project_entry_element_id(group, index));
        let this_window_id = SharedString::from(format!("{element_id}-this-window"));

        let icon = row.icon;
        let host = row.host.clone();
        let tooltip = row.tooltip.clone();
        let color = row.color;

        Some(
            ListItem::new(element_id.clone())
                .spacing(ListItemSpacing::Sparse)
                .start_slot(Icon::new(icon).size(IconSize::Small).color(color))
                .child(
                    h_flex()
                        .min_w_0()
                        .gap_1p5()
                        .child(Label::new(name).single_line().color(color))
                        .when_some(host, |this, host| {
                            this.child(
                                Label::new(host)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .single_line(),
                            )
                        }),
                )
                .tooltip(Tooltip::text(tooltip))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_project(index, CLICK_OPENS_A_NEW_WINDOW, window, cx);
                }))
                .end_slot(
                    IconButton::new(this_window_id, IconName::Replace)
                        .icon_size(IconSize::Small)
                        .visible_on_hover(element_id)
                        .tooltip(Tooltip::text("Open in This Window"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_project(index, false, window, cx);
                        })),
                )
                .into_any_element(),
        )
    }
}

/// What `render_project` needs to draw one row. `ProjectEntry::location` re-runs
/// the whole URI parse, so it is derived once per project-list change rather
/// than once per visible row per frame.
#[derive(Debug, PartialEq)]
struct ProjectRow {
    icon: IconName,
    host: Option<SharedString>,
    tooltip: SharedString,
    color: Color,
}

fn project_row(project: &ProjectEntry) -> ProjectRow {
    // A single unparsable entry must not take the whole panel down with it,
    // so it is rendered as an error row instead.
    match project.location() {
        Ok(ProjectLocation::Local(_)) => ProjectRow {
            icon: IconName::Folder,
            host: None,
            tooltip: SharedString::from(project.root_path.clone()),
            color: Color::Default,
        },
        Ok(ProjectLocation::Remote { options, .. }) => ProjectRow {
            icon: IconName::Server,
            host: Some(SharedString::from(options.display_name())),
            tooltip: SharedString::from(project.root_path.clone()),
            color: Color::Default,
        },
        Err(error) => ProjectRow {
            icon: IconName::Warning,
            host: None,
            tooltip: SharedString::from(format!("{error:#}")),
            color: Color::Error,
        },
    }
}

/// `Workspace::open_paths` adds the folders to the project this window already
/// shows; opening a saved project has to replace what the window shows instead,
/// which is what `OpenMode::Activate` does with the requesting window.
/// What clicking a project row does.
///
/// A window of its own: the reader is choosing another project, not another file, and
/// opening it over this window would take away the project they were working in — along
/// with its tabs, its terminals and its layout. The row keeps a button for replacing this
/// window, for the reader who meant that.
const CLICK_OPENS_A_NEW_WINDOW: bool = true;

fn open_mode_for_window(new_window: bool) -> OpenMode {
    if new_window {
        OpenMode::NewWindow
    } else {
        OpenMode::Activate
    }
}

/// The `projects.json` entry recording the folders currently open: the first
/// visible worktree root becomes `rootPath` and the rest become extra `paths`.
fn project_entry_for_roots(
    connection: Option<&RemoteConnectionOptions>,
    roots: &[PathBuf],
) -> Result<ProjectEntry> {
    let mut uris: Vec<String> = Vec::with_capacity(roots.len());
    for root in roots {
        let uri = match connection {
            Some(options) => remote_project_uri(options, root).with_context(|| {
                format!(
                    "Cannot record {} on {} as a project URI.",
                    root.display(),
                    options.display_name()
                )
            })?,
            None => root.to_string_lossy().to_string(),
        };
        if !uris.contains(&uri) {
            uris.push(uri);
        }
    }

    let mut uris = uris.into_iter();
    let root_path = uris.next().context("No folder is open in this window.")?;
    let name = roots
        .first()
        .and_then(|root| root.file_name())
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| root_path.clone());

    let mut entry = ProjectEntry::new(name, root_path);
    entry.paths = uris.collect();
    Ok(entry)
}

/// The first of `candidates` that is a file, or `None` when none of them is.
async fn first_project_file(fs: &Arc<dyn Fs>, candidates: Vec<PathBuf>) -> Option<PathBuf> {
    for candidate in candidates {
        if fs.is_file(&candidate).await {
            return Some(candidate);
        }
    }
    None
}

/// Asks the reader for a project file to import. `Ok(None)` when they closed the dialog
/// without choosing one, which is a decision rather than a failure.
async fn ask_for_a_project_file(cx: &mut AsyncApp) -> Result<Option<PathBuf>> {
    let receiver = cx.update(|cx| {
        cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        })
    });

    match receiver.await {
        Ok(Ok(paths)) => Ok(paths.and_then(|paths| paths.into_iter().next())),
        Ok(Err(error)) => Err(error).context("choosing a project file to import"),
        // The dialog went away with the window it belonged to.
        Err(_) => Ok(None),
    }
}

/// How many lines of an import's warnings are shown. Enough for every entry of a real
/// export that needed one, and short of burying the panel under a list as long as the
/// file itself when a reader imports one written for another machine.
const SHOWN_IMPORT_WARNINGS: usize = 6;

/// What an import did, as lines for the panel: what it read and how much of it was new,
/// then what it could not bring across.
fn import_notice(source: &Path, merge: ImportMerge, warnings: &[String]) -> Vec<SharedString> {
    let mut lines = vec![SharedString::from(format!(
        "Imported {} of {} projects from {}",
        merge.added,
        merge.added + merge.skipped,
        source.display()
    ))];
    lines.extend(
        warnings
            .iter()
            .take(SHOWN_IMPORT_WARNINGS)
            .map(|warning| SharedString::from(warning.clone())),
    );
    let hidden = warnings.len().saturating_sub(SHOWN_IMPORT_WARNINGS);
    if hidden > 0 {
        lines.push(SharedString::from(format!("…and {hidden} more")));
    }
    lines
}

fn entry_errors_message(errors: &[String]) -> Option<SharedString> {
    if errors.is_empty() {
        return None;
    }
    Some(errors.join("\n").into())
}

impl Render for ProjectManagerPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.filter_query(cx);
        let matches = filter_projects(&self.projects, &query);
        let groups = group_projects(&self.projects, &matches);

        v_flex()
            .key_context("ProjectManagerPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(
                h_flex()
                    .p_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(div().flex_1().px_1().child(self.render_filter_input(cx)))
                    .child(self.render_toolbar(cx)),
            )
            .when_some(self.load_error.clone(), |this, error| {
                this.child(div().p_2().child(Label::new(error).color(Color::Error)))
            })
            .when(!self.notice.is_empty(), |this| {
                this.child(v_flex().px_2().py_1().gap_0p5().children(
                    self.notice.iter().enumerate().map(|(line, text)| {
                        // The first line is what the import did; the ones under it are
                        // what it could not do, which is the part worth the warning
                        // colour.
                        Label::new(text.clone())
                            .size(LabelSize::Small)
                            .color(if line == 0 {
                                Color::Muted
                            } else {
                                Color::Warning
                            })
                    }),
                ))
            })
            .child(
                v_flex()
                    .id("project-manager-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .when(groups.is_empty(), |this| {
                        this.child(
                            div().p_2().child(
                                Label::new(if self.projects.is_empty() {
                                    "No saved projects."
                                } else {
                                    "No matching projects."
                                })
                                .color(Color::Muted),
                            ),
                        )
                    })
                    .children(groups.into_iter().map(|group| {
                        let heading = group.tag.clone().unwrap_or_else(|| "Untagged".to_string());
                        let rows: Vec<AnyElement> = group
                            .entry_indices
                            .iter()
                            .filter_map(|&index| self.render_project(&group, index, cx))
                            .collect();
                        v_flex()
                            .child(
                                div().px_2().pt_2().pb_1().child(
                                    Label::new(heading)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                            )
                            .children(rows)
                    })),
            )
    }
}

impl Focusable for ProjectManagerPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ProjectManagerPanel {}

impl Panel for ProjectManagerPanel {
    fn persistent_name() -> &'static str {
        "ProjectManagerPanel"
    }

    fn panel_key() -> &'static str {
        PROJECT_MANAGER_PANEL_KEY
    }

    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.filter_editor.focus_handle(cx)
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(240.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::FolderSearch)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Project Manager")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        8
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.reload(cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::Project;
    use remote::SshConnectionOptions;
    use serde_json::json;
    use util::path;
    use workspace::{AppState, MultiWorkspace};

    fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
        cx.update(|cx| {
            let app_state = AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            app_state
        })
    }

    #[gpui::test]
    async fn test_saving_a_project_survives_a_concurrent_reload(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/root"), json!({ "a.txt": "" }))
            .await;

        let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            ProjectManagerPanel::new(workspace, window, cx)
        });
        cx.run_until_parked();

        panel.update(cx, |panel, cx| {
            panel.save_current_project(cx);
            // Refresh, panel activation and the projects.json watcher all call
            // `reload` and can do so before the save has written the file.
            panel.reload(cx);
        });
        cx.run_until_parked();

        let saved = load_projects(&app_state.fs, paths::projects_file())
            .await
            .expect("projects.json must be readable after saving");
        let recorded: Vec<&str> = saved
            .projects
            .iter()
            .map(|project| project.root_path.as_str())
            .collect();
        assert!(
            recorded.contains(&path!("/root")),
            "a reload must not cancel the save: expected projects.json to contain {:?}, got {:?}",
            path!("/root"),
            recorded
        );
    }

    #[test]
    fn test_opening_in_this_window_replaces_what_it_shows() {
        assert_eq!(
            open_mode_for_window(false),
            OpenMode::Activate,
            "opening a project in this window must switch the window to it, not merge its folders into the project already open"
        );
        assert_eq!(open_mode_for_window(true), OpenMode::NewWindow);
    }

    /// Clicking a project must not take away the window the reader was working in. The
    /// default is asserted here because it is a single `bool` at the click site, and
    /// flipping it back would otherwise be a silent change of behaviour.
    #[test]
    fn clicking_a_project_opens_a_window_of_its_own() {
        assert_eq!(
            open_mode_for_window(CLICK_OPENS_A_NEW_WINDOW),
            OpenMode::NewWindow
        );
    }

    #[test]
    fn test_project_rows_carry_what_each_entry_resolves_to() {
        let row = project_row(&ProjectEntry::new("api", "/work/api"));
        assert_eq!(row.icon, IconName::Folder);
        assert_eq!(row.host, None);
        assert_eq!(row.tooltip, SharedString::from("/work/api"));
        assert_eq!(row.color, Color::Default);

        let row = project_row(&ProjectEntry::new("api", "ssh://host/srv/api"));
        assert_eq!(row.icon, IconName::Server);
        assert_eq!(row.host.as_deref(), Some("host"));
        assert_eq!(row.tooltip, SharedString::from("ssh://host/srv/api"));
        assert_eq!(row.color, Color::Default);

        let row = project_row(&ProjectEntry::new("api", "vscode-remote://host/p"));
        assert_eq!(
            row.icon,
            IconName::Warning,
            "an entry that cannot be parsed still renders as a warning row"
        );
        assert_eq!(row.host, None);
        assert_eq!(row.color, Color::Error);
        assert!(
            row.tooltip.contains("unsupported scheme"),
            "the row keeps showing the parse error, got: {}",
            row.tooltip
        );
    }

    #[test]
    fn test_saving_a_multi_root_workspace_records_every_folder() {
        let entry = project_entry_for_roots(
            None,
            &[
                PathBuf::from("/work/api"),
                PathBuf::from("/work/web"),
                PathBuf::from("/work/api"),
            ],
        )
        .expect("a local multi-root workspace is recordable");

        assert_eq!(entry.name, "api");
        assert_eq!(entry.root_path, "/work/api");
        assert_eq!(
            entry.paths,
            vec!["/work/web".to_string()],
            "every folder after the first is kept as an extra path"
        );

        let options = RemoteConnectionOptions::Ssh(SshConnectionOptions {
            host: "host".to_string().into(),
            ..Default::default()
        });
        let entry = project_entry_for_roots(
            Some(&options),
            &[PathBuf::from("/srv/api"), PathBuf::from("/srv/web")],
        )
        .expect("a remote multi-root workspace is recordable");
        assert_eq!(entry.root_path, "ssh://host/srv/api");
        assert_eq!(entry.paths, vec!["ssh://host/srv/web".to_string()]);

        project_entry_for_roots(None, &[])
            .expect_err("a window with no folder open has nothing to record");
    }
}
