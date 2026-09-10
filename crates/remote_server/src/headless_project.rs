use anyhow::{Context as _, Result, anyhow};
use client::ProjectId;
use collections::HashMap;
use collections::HashSet;
use gpui::TasksIncluded;
use language::File;
use lsp::LanguageServerId;

use extension::ExtensionHostProxy;
use extension_host::headless_host::HeadlessExtensionStore;
use fs::Fs;
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, PromptLevel, TaskExt};
use http_client::HttpClient;
use language::{Buffer, BufferEvent, LanguageRegistry, proto::serialize_operation};
use node_runtime::NodeRuntime;
use project::{
    AgentRegistryStore, LspStore, LspStoreEvent, ManifestTree, PrettierStore, ProjectEnvironment,
    ProjectPath, ToolchainStore, WorktreeId,
    agent_server_store::AgentServerStore,
    buffer_store::{BufferStore, BufferStoreEvent},
    context_server_store::ContextServerStore,
    debugger::{breakpoint_store::BreakpointStore, dap_store::DapStore},
    git_store::GitStore,
    image_store::ImageId,
    lsp_store::log_store::{
        self, GlobalLogStore, LanguageServerKind, LanguageServerLogKey, LogKind,
    },
    project_settings::SettingsObserver,
    search::SearchQuery,
    task_store::TaskStore,
    trusted_worktrees::{PathTrust, RemoteHostLocation, TrustedWorktrees},
    worktree_store::{WorktreeIdCounter, WorktreeStore},
};
use remote::PortForwardStore;
use rpc::{
    AnyProtoClient, TypedEnvelope,
    proto::{self, REMOTE_SERVER_PEER_ID, REMOTE_SERVER_PROJECT_ID},
};
use smol::process::Child;

use settings::initial_server_settings_content;
use std::{
    ffi::OsStr,
    num::NonZeroU64,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};
use sysinfo::{ProcessRefreshKind, RefreshKind, System, UpdateKind};
use util::{ResultExt, paths::PathStyle, rel_path::RelPath};
use worktree::Worktree;

pub struct HeadlessProject {
    pub fs: Arc<dyn Fs>,
    pub session: AnyProtoClient,
    pub worktree_store: Entity<WorktreeStore>,
    pub buffer_store: Entity<BufferStore>,
    pub lsp_store: Entity<LspStore>,
    pub task_store: Entity<TaskStore>,
    pub dap_store: Entity<DapStore>,
    pub breakpoint_store: Entity<BreakpointStore>,
    pub agent_server_store: Entity<AgentServerStore>,
    pub context_server_store: Entity<ContextServerStore>,
    pub settings_observer: Entity<SettingsObserver>,
    pub next_entry_id: Arc<AtomicUsize>,
    pub languages: Arc<LanguageRegistry>,
    pub extensions: Entity<HeadlessExtensionStore>,
    pub git_store: Entity<GitStore>,
    pub environment: Entity<ProjectEnvironment>,
    pub profiling_collector: gpui::ProfilingCollector,
    // Used mostly to keep alive the toolchain store for RPC handlers.
    // Local variant is used within LSP store, but that's a separate entity.
    pub _toolchain_store: Entity<ToolchainStore>,
    pub kernels: HashMap<String, Child>,
    pub port_forwards: Entity<PortForwardStore>,
}

pub struct HeadlessAppState {
    pub session: AnyProtoClient,
    pub fs: Arc<dyn Fs>,
    pub http_client: Arc<dyn HttpClient>,
    pub node_runtime: NodeRuntime,
    pub languages: Arc<LanguageRegistry>,
    pub extension_host_proxy: Arc<ExtensionHostProxy>,
    pub startup_time: Instant,
}

impl HeadlessProject {
    pub fn init(cx: &mut App) {
        settings::init(cx);
        log_store::init(true, cx);
    }

    pub fn new(
        HeadlessAppState {
            session,
            fs,
            http_client,
            node_runtime,
            languages,
            extension_host_proxy: proxy,
            startup_time,
        }: HeadlessAppState,
        init_worktree_trust: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        debug_adapter_extension::init(proxy.clone(), cx);
        languages::init(languages.clone(), fs.clone(), node_runtime.clone(), cx);

        let worktree_store = cx.new(|cx| {
            let mut store = WorktreeStore::local(true, fs.clone(), WorktreeIdCounter::get(cx));
            store.shared(REMOTE_SERVER_PROJECT_ID, session.clone(), cx);
            store
        });

        if init_worktree_trust {
            project::trusted_worktrees::track_worktree_trust(
                worktree_store.clone(),
                None::<RemoteHostLocation>,
                Some((session.clone(), ProjectId(REMOTE_SERVER_PROJECT_ID))),
                None,
                cx,
            );
        }

        let environment =
            cx.new(|cx| ProjectEnvironment::new(None, worktree_store.downgrade(), None, true, cx));
        let manifest_tree = ManifestTree::new(worktree_store.clone(), cx);
        let toolchain_store = cx.new(|cx| {
            ToolchainStore::local(
                languages.clone(),
                worktree_store.clone(),
                environment.clone(),
                manifest_tree.clone(),
                cx,
            )
        });

        let buffer_store = cx.new(|cx| {
            let mut buffer_store = BufferStore::local(worktree_store.clone(), cx);
            buffer_store.shared(REMOTE_SERVER_PROJECT_ID, session.clone(), cx);
            buffer_store
        });

        let breakpoint_store = cx.new(|_| {
            let mut breakpoint_store =
                BreakpointStore::local(worktree_store.clone(), buffer_store.clone());
            breakpoint_store.shared(REMOTE_SERVER_PROJECT_ID, session.clone());

            breakpoint_store
        });

        let dap_store = cx.new(|cx| {
            let mut dap_store = DapStore::new_local(
                http_client.clone(),
                node_runtime.clone(),
                fs.clone(),
                environment.clone(),
                toolchain_store.read(cx).as_language_toolchain_store(),
                worktree_store.clone(),
                breakpoint_store.clone(),
                true,
                cx,
            );
            dap_store.shared(REMOTE_SERVER_PROJECT_ID, session.clone(), cx);
            dap_store
        });

        let git_store = cx.new(|cx| {
            let mut store = GitStore::local(
                &worktree_store,
                buffer_store.clone(),
                environment.clone(),
                fs.clone(),
                cx,
            );
            store.shared(REMOTE_SERVER_PROJECT_ID, session.clone(), cx);
            store
        });

        let prettier_store = cx.new(|cx| {
            PrettierStore::new(
                node_runtime.clone(),
                fs.clone(),
                languages.clone(),
                worktree_store.clone(),
                cx,
            )
        });

        let task_store = cx.new(|cx| {
            let mut task_store = TaskStore::local(
                buffer_store.downgrade(),
                worktree_store.clone(),
                toolchain_store.read(cx).as_language_toolchain_store(),
                environment.clone(),
                git_store.clone(),
                cx,
            );
            task_store.shared(REMOTE_SERVER_PROJECT_ID, session.clone(), cx);
            task_store
        });
        let settings_observer = cx.new(|cx| {
            let mut observer = SettingsObserver::new_local(
                fs.clone(),
                worktree_store.clone(),
                task_store.clone(),
                true,
                cx,
            );
            observer.shared(REMOTE_SERVER_PROJECT_ID, session.clone(), cx);
            observer
        });

        let lsp_store = cx.new(|cx| {
            let mut lsp_store = LspStore::new_local(
                buffer_store.clone(),
                worktree_store.clone(),
                prettier_store.clone(),
                toolchain_store
                    .read(cx)
                    .as_local_store()
                    .expect("Toolchain store to be local")
                    .clone(),
                environment.clone(),
                manifest_tree,
                languages.clone(),
                http_client.clone(),
                fs.clone(),
                cx,
            );
            lsp_store.shared(REMOTE_SERVER_PROJECT_ID, session.clone(), cx);
            lsp_store
        });

        AgentRegistryStore::init_global(cx, fs.clone(), http_client.clone());

        let agent_server_store = cx.new(|cx| {
            let mut agent_server_store = AgentServerStore::local(
                node_runtime.clone(),
                fs.clone(),
                environment.clone(),
                http_client.clone(),
                cx,
            );
            agent_server_store.shared(REMOTE_SERVER_PROJECT_ID, session.clone(), cx);
            agent_server_store
        });

        let context_server_store = cx.new(|cx| {
            let mut context_server_store =
                ContextServerStore::local(worktree_store.clone(), None, true, cx);
            context_server_store.shared(REMOTE_SERVER_PROJECT_ID, session.clone());
            context_server_store
        });

        cx.subscribe(&lsp_store, Self::on_lsp_store_event).detach();
        language_extension::init(
            language_extension::LspAccess::ViaLspStore(lsp_store.downgrade()),
            proxy.clone(),
            languages.clone(),
        );

        cx.subscribe(&buffer_store, |_this, _buffer_store, event, cx| {
            if let BufferStoreEvent::BufferAdded(buffer) = event {
                cx.subscribe(buffer, Self::on_buffer_event).detach();
            }
        })
        .detach();

        let extensions = HeadlessExtensionStore::new(
            fs.clone(),
            http_client.clone(),
            paths::remote_extensions_dir().to_path_buf(),
            proxy,
            node_runtime,
            cx,
        );

        // local_machine -> ssh handlers
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &worktree_store);
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &buffer_store);
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &cx.entity());
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &lsp_store);
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &task_store);
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &toolchain_store);
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &dap_store);
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &breakpoint_store);
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &settings_observer);
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &git_store);
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &agent_server_store);
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &context_server_store);

        let port_forwards =
            cx.new(|_| PortForwardStore::new(REMOTE_SERVER_PROJECT_ID, session.clone()));
        session.subscribe_to_entity(REMOTE_SERVER_PROJECT_ID, &port_forwards);
        PortForwardStore::init_remote(&session);

        session.add_request_handler(cx.weak_entity(), Self::handle_list_remote_directory);
        session.add_request_handler(cx.weak_entity(), Self::handle_get_path_metadata);
        session.add_request_handler(cx.weak_entity(), Self::handle_shutdown_remote_server);
        session.add_request_handler(cx.weak_entity(), Self::handle_ping);
        session.add_request_handler(cx.weak_entity(), Self::handle_get_processes);
        session.add_request_handler(cx.weak_entity(), Self::handle_get_listening_ports);
        session.add_request_handler(cx.weak_entity(), Self::handle_list_tmux_sessions);
        session.add_request_handler(cx.weak_entity(), Self::handle_list_claude_sessions);
        session.add_request_handler(cx.weak_entity(), Self::handle_list_claude_subagents);
        session.add_request_handler(cx.weak_entity(), Self::handle_tail_claude_transcript);
        session.add_request_handler(cx.weak_entity(), Self::handle_read_claude_file);
        session.add_request_handler(cx.weak_entity(), Self::handle_send_claude_input);
        session.add_request_handler(cx.weak_entity(), Self::handle_get_remote_profiling_data);

        session.add_entity_request_handler(Self::handle_add_worktree);
        session.add_request_handler(cx.weak_entity(), Self::handle_remove_worktree);

        session.add_entity_request_handler(Self::handle_open_buffer_by_path);
        session.add_entity_request_handler(Self::handle_open_new_buffer);
        session.add_entity_request_handler(Self::handle_find_search_candidates);
        session.add_entity_request_handler(Self::handle_open_server_settings);
        session.add_entity_request_handler(Self::handle_get_directory_environment);
        session.add_entity_message_handler(Self::handle_toggle_lsp_logs);
        session.add_entity_request_handler(Self::handle_open_image_by_path);
        session.add_entity_request_handler(Self::handle_trust_worktrees);
        session.add_entity_request_handler(Self::handle_restrict_worktrees);
        session.add_entity_request_handler(Self::handle_download_file_by_path);

        session.add_entity_message_handler(Self::handle_find_search_candidates_cancel);
        session.add_entity_request_handler(BufferStore::handle_update_buffer);
        session.add_entity_message_handler(BufferStore::handle_close_buffer);

        session.add_request_handler(
            extensions.downgrade(),
            HeadlessExtensionStore::handle_sync_extensions,
        );
        session.add_request_handler(
            extensions.downgrade(),
            HeadlessExtensionStore::handle_install_extension,
        );

        session.add_request_handler(cx.weak_entity(), Self::handle_spawn_kernel);
        session.add_request_handler(cx.weak_entity(), Self::handle_kill_kernel);

        BufferStore::init(&session);
        WorktreeStore::init(&session);
        SettingsObserver::init(&session);
        LspStore::init(&session);
        TaskStore::init(Some(&session));
        ToolchainStore::init(&session);
        DapStore::init(&session, cx);
        // todo(debugger): Re init breakpoint store when we set it up for collab
        BreakpointStore::init(&session);
        GitStore::init(&session);
        AgentServerStore::init_headless(&session);
        ContextServerStore::init_headless(&session);

        HeadlessProject {
            next_entry_id: Default::default(),
            session,
            settings_observer,
            fs,
            worktree_store,
            buffer_store,
            lsp_store,
            task_store,
            dap_store,
            breakpoint_store,
            agent_server_store,
            context_server_store,
            languages,
            extensions,
            git_store,
            environment,
            profiling_collector: gpui::ProfilingCollector::new(startup_time),
            _toolchain_store: toolchain_store,
            kernels: Default::default(),
            port_forwards,
        }
    }

    fn on_buffer_event(
        &mut self,
        buffer: Entity<Buffer>,
        event: &BufferEvent,
        cx: &mut Context<Self>,
    ) {
        if let BufferEvent::Operation {
            operation,
            is_local: true,
        } = event
        {
            cx.background_spawn(self.session.request(proto::UpdateBuffer {
                project_id: REMOTE_SERVER_PROJECT_ID,
                buffer_id: buffer.read(cx).remote_id().to_proto(),
                operations: vec![serialize_operation(operation)],
            }))
            .detach()
        }
    }

    fn on_lsp_store_event(
        &mut self,
        lsp_store: Entity<LspStore>,
        event: &LspStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            LspStoreEvent::LanguageServerAdded(id, name, worktree_id) => {
                let log_store = cx
                    .try_global::<GlobalLogStore>()
                    .map(|lsp_logs| lsp_logs.0.clone());
                if let Some(log_store) = log_store {
                    log_store.update(cx, |log_store, cx| {
                        log_store.add_language_server(
                            LanguageServerKind::LocalSsh {
                                lsp_store: self.lsp_store.downgrade(),
                            },
                            *id,
                            Some(name.clone()),
                            *worktree_id,
                            lsp_store.read(cx).language_server_for_id(*id),
                            cx,
                        );
                    });
                }
            }
            LspStoreEvent::SupplementaryLanguageServerAdded(id, name) => {
                let log_store = cx
                    .try_global::<GlobalLogStore>()
                    .map(|lsp_logs| lsp_logs.0.clone());
                if let Some(log_store) = log_store {
                    log_store.update(cx, |log_store, cx| {
                        log_store.add_language_server(
                            LanguageServerKind::LocalSsh {
                                lsp_store: self.lsp_store.downgrade(),
                            },
                            *id,
                            Some(name.clone()),
                            None,
                            lsp_store.read(cx).language_server_for_id(*id),
                            cx,
                        );
                    });
                }
            }
            LspStoreEvent::LanguageServerRemoved(id)
            | LspStoreEvent::SupplementaryLanguageServerRemoved(id) => {
                let log_store = cx
                    .try_global::<GlobalLogStore>()
                    .map(|lsp_logs| lsp_logs.0.clone());
                if let Some(log_store) = log_store {
                    let server_key = LanguageServerLogKey::new(
                        LanguageServerKind::LocalSsh {
                            lsp_store: self.lsp_store.downgrade(),
                        },
                        *id,
                    );
                    log_store.update(cx, |log_store, cx| {
                        log_store.remove_language_server(&server_key, cx);
                    });
                }
                self.session
                    .send(proto::UpdateLanguageServer {
                        project_id: REMOTE_SERVER_PROJECT_ID,
                        server_name: None,
                        language_server_id: id.to_proto(),
                        variant: Some(proto::update_language_server::Variant::Removed(
                            proto::ServerRemoved {},
                        )),
                    })
                    .log_err();
            }
            LspStoreEvent::LanguageServerUpdate {
                language_server_id,
                name,
                message,
            } => {
                self.session
                    .send(proto::UpdateLanguageServer {
                        project_id: REMOTE_SERVER_PROJECT_ID,
                        server_name: name.as_ref().map(|name| name.to_string()),
                        language_server_id: language_server_id.to_proto(),
                        variant: Some(message.clone()),
                    })
                    .log_err();
            }
            LspStoreEvent::Notification(message) => {
                self.session
                    .send(proto::Toast {
                        project_id: REMOTE_SERVER_PROJECT_ID,
                        notification_id: "lsp".to_string(),
                        message: message.clone(),
                    })
                    .log_err();
            }
            LspStoreEvent::LanguageServerShowDocument(show_document_request) => {
                let request = self
                    .session
                    .request(proto::LanguageServerShowDocumentRequest {
                        project_id: REMOTE_SERVER_PROJECT_ID,
                        uri: show_document_request.uri.as_str().to_owned(),
                        external: show_document_request.external,
                        take_focus: show_document_request.take_focus,
                        selection_start: show_document_request.selection.map(|selection| {
                            proto::PointUtf16 {
                                row: selection.start.line,
                                column: selection.start.character,
                            }
                        }),
                        selection_end: show_document_request.selection.map(|selection| {
                            proto::PointUtf16 {
                                row: selection.end.line,
                                column: selection.end.character,
                            }
                        }),
                    });
                let show_document_request = show_document_request.clone();
                cx.background_spawn(async move {
                    show_document_request.respond(request.await.is_ok());
                })
                .detach();
            }
            LspStoreEvent::LanguageServerPrompt(prompt) => {
                let request = self.session.request(proto::LanguageServerPromptRequest {
                    project_id: REMOTE_SERVER_PROJECT_ID,
                    actions: prompt
                        .actions
                        .iter()
                        .map(|action| action.title.to_string())
                        .collect(),
                    level: Some(prompt_to_proto(prompt)),
                    lsp_name: prompt.lsp_name.clone(),
                    message: prompt.message.clone(),
                });
                let prompt = prompt.clone();
                cx.background_spawn(async move {
                    let response = request.await?;
                    if let Some(action_response) = response.action_response {
                        prompt.respond(action_response as usize).await;
                    }
                    anyhow::Ok(())
                })
                .detach();
            }
            _ => {}
        }
    }

    pub async fn handle_add_worktree(
        this: Entity<Self>,
        message: TypedEnvelope<proto::AddWorktree>,
        mut cx: AsyncApp,
    ) -> Result<proto::AddWorktreeResponse> {
        use client::ErrorCodeExt;
        let fs = this.read_with(&cx, |this, _| this.fs.clone());
        let path = PathBuf::from(shellexpand::tilde(&message.payload.path).to_string());

        let canonicalized = match fs.canonicalize(&path).await {
            Ok(path) => path,
            Err(e) => {
                let mut parent = path
                    .parent()
                    .ok_or(e)
                    .with_context(|| format!("{path:?} does not exist"))?;
                if parent == Path::new("") {
                    parent = util::paths::home_dir();
                }
                let parent = fs.canonicalize(parent).await.map_err(|_| {
                    anyhow!(
                        proto::ErrorCode::DevServerProjectPathDoesNotExist
                            .with_tag("path", path.to_string_lossy().as_ref())
                    )
                })?;
                if let Some(file_name) = path.file_name() {
                    parent.join(file_name)
                } else {
                    parent
                }
            }
        };
        let next_worktree_id = this
            .update(&mut cx, |this, cx| {
                this.worktree_store
                    .update(cx, |worktree_store, _| worktree_store.next_worktree_id())
            })
            .await?;
        let worktree = this
            .read_with(&cx.clone(), |this, _| {
                Worktree::local(
                    Arc::from(canonicalized.as_path()),
                    message.payload.visible,
                    this.fs.clone(),
                    this.next_entry_id.clone(),
                    true,
                    next_worktree_id,
                    &mut cx,
                )
            })
            .await?;

        let response = this.read_with(&cx, |_, cx| {
            let worktree = worktree.read(cx);
            proto::AddWorktreeResponse {
                worktree_id: worktree.id().to_proto(),
                canonicalized_path: canonicalized.to_string_lossy().into_owned(),
                root_repo_common_dir: worktree
                    .root_repo_common_dir()
                    .map(|p| p.to_string_lossy().into_owned()),
                root_repo_is_linked_worktree: worktree.root_repo_is_linked_worktree(),
            }
        });

        // We spawn this asynchronously, so that we can send the response back
        // *before* `worktree_store.add()` can send out UpdateProject requests
        // to the client about the new worktree.
        //
        // That lets the client manage the reference/handles of the newly-added
        // worktree, before getting interrupted by an UpdateProject request.
        //
        // This fixes the problem of the client sending the AddWorktree request,
        // headless project sending out a project update, client receiving it
        // and immediately dropping the reference of the new client, causing it
        // to be dropped on the headless project, and the client only then
        // receiving a response to AddWorktree.
        cx.spawn(async move |cx| {
            this.update(cx, |this, cx| {
                this.worktree_store.update(cx, |worktree_store, cx| {
                    worktree_store.add(&worktree, cx);
                });
            });
        })
        .detach();

        Ok(response)
    }

    pub async fn handle_remove_worktree(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::RemoveWorktree>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let worktree_id = WorktreeId::from_proto(envelope.payload.worktree_id);
        this.update(&mut cx, |this, cx| {
            this.worktree_store.update(cx, |worktree_store, cx| {
                worktree_store.remove_worktree(worktree_id, cx);
            });
        });
        Ok(proto::Ack {})
    }

    pub async fn handle_open_buffer_by_path(
        this: Entity<Self>,
        message: TypedEnvelope<proto::OpenBufferByPath>,
        mut cx: AsyncApp,
    ) -> Result<proto::OpenBufferResponse> {
        let worktree_id = WorktreeId::from_proto(message.payload.worktree_id);
        let path = RelPath::from_unix_str(&message.payload.path)?.into();
        let (buffer_store, buffer) = this.update(&mut cx, |this, cx| {
            let buffer_store = this.buffer_store.clone();
            let buffer = this.buffer_store.update(cx, |buffer_store, cx| {
                buffer_store.open_buffer(ProjectPath { worktree_id, path }, cx)
            });
            (buffer_store, buffer)
        });

        let buffer = buffer.await?;
        let buffer_id = buffer.read_with(&cx, |b, _| b.remote_id());
        buffer_store.update(&mut cx, |buffer_store, cx| {
            buffer_store
                .create_buffer_for_peer(&buffer, REMOTE_SERVER_PEER_ID, cx)
                .detach_and_log_err(cx);
        });

        Ok(proto::OpenBufferResponse {
            buffer_id: buffer_id.to_proto(),
        })
    }

    pub async fn handle_open_image_by_path(
        this: Entity<Self>,
        message: TypedEnvelope<proto::OpenImageByPath>,
        mut cx: AsyncApp,
    ) -> Result<proto::OpenImageResponse> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let worktree_id = WorktreeId::from_proto(message.payload.worktree_id);
        let path = RelPath::from_unix_str(&message.payload.path)?;
        let project_id = message.payload.project_id;
        use proto::create_image_for_peer::Variant;

        let (worktree_store, session) = this.read_with(&cx, |this, _| {
            (this.worktree_store.clone(), this.session.clone())
        });

        let worktree = worktree_store
            .read_with(&cx, |store, cx| store.worktree_for_id(worktree_id, cx))
            .context("worktree not found")?;

        let load_task = worktree.update(&mut cx, |worktree, cx| {
            worktree.load_binary_file(path.as_ref(), cx)
        });

        let loaded_file = load_task.await?;
        let content = loaded_file.content;
        let file = loaded_file.file;

        let proto_file = worktree.read_with(&cx, |_worktree, cx| file.to_proto(cx));
        let image_id =
            ImageId::from(NonZeroU64::new(NEXT_ID.fetch_add(1, Ordering::Relaxed)).unwrap());

        let format = image::guess_format(&content)
            .map(|f| format!("{:?}", f).to_lowercase())
            .unwrap_or_else(|_| "unknown".to_string());

        let state = proto::ImageState {
            id: image_id.to_proto(),
            file: Some(proto_file),
            content_size: content.len() as u64,
            format,
        };

        session.send(proto::CreateImageForPeer {
            project_id,
            peer_id: Some(REMOTE_SERVER_PEER_ID),
            variant: Some(Variant::State(state)),
        })?;

        const CHUNK_SIZE: usize = 1024 * 1024; // 1MB chunks
        for chunk in content.chunks(CHUNK_SIZE) {
            session.send(proto::CreateImageForPeer {
                project_id,
                peer_id: Some(REMOTE_SERVER_PEER_ID),
                variant: Some(Variant::Chunk(proto::ImageChunk {
                    image_id: image_id.to_proto(),
                    data: chunk.to_vec(),
                })),
            })?;
        }

        Ok(proto::OpenImageResponse {
            image_id: image_id.to_proto(),
        })
    }

    pub async fn handle_trust_worktrees(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::TrustWorktrees>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let trusted_worktrees = cx
            .update(|cx| TrustedWorktrees::try_get_global(cx))
            .context("missing trusted worktrees")?;
        let worktree_store = this.read_with(&cx, |project, _| project.worktree_store.clone());
        trusted_worktrees.update(&mut cx, |trusted_worktrees, cx| {
            trusted_worktrees.trust(
                &worktree_store,
                envelope
                    .payload
                    .trusted_paths
                    .into_iter()
                    .filter_map(PathTrust::from_proto)
                    .collect(),
                cx,
            );
        });
        Ok(proto::Ack {})
    }

    pub async fn handle_restrict_worktrees(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::RestrictWorktrees>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let trusted_worktrees = cx
            .update(|cx| TrustedWorktrees::try_get_global(cx))
            .context("missing trusted worktrees")?;
        let worktree_store = this.read_with(&cx, |project, _| project.worktree_store.downgrade());
        trusted_worktrees.update(&mut cx, |trusted_worktrees, cx| {
            let restricted_paths = envelope
                .payload
                .worktree_ids
                .into_iter()
                .map(WorktreeId::from_proto)
                .map(PathTrust::Worktree)
                .collect::<HashSet<_>>();
            trusted_worktrees.restrict(worktree_store, restricted_paths, cx);
        });
        Ok(proto::Ack {})
    }

    pub async fn handle_download_file_by_path(
        this: Entity<Self>,
        message: TypedEnvelope<proto::DownloadFileByPath>,
        mut cx: AsyncApp,
    ) -> Result<proto::DownloadFileResponse> {
        log::debug!(
            "handle_download_file_by_path: received request: {:?}",
            message.payload
        );

        let worktree_id = WorktreeId::from_proto(message.payload.worktree_id);
        let path = RelPath::from_unix_str(&message.payload.path)?;
        let project_id = message.payload.project_id;
        let file_id = message.payload.file_id;
        log::debug!(
            "handle_download_file_by_path: worktree_id={:?}, path={:?}, file_id={}",
            worktree_id,
            path,
            file_id
        );
        use proto::create_file_for_peer::Variant;

        let (worktree_store, session): (Entity<WorktreeStore>, AnyProtoClient) = this
            .read_with(&cx, |this, _| {
                (this.worktree_store.clone(), this.session.clone())
            });

        let worktree = worktree_store
            .read_with(&cx, |store, cx| store.worktree_for_id(worktree_id, cx))
            .context("worktree not found")?;

        let download_task = worktree.update(&mut cx, |worktree: &mut Worktree, cx| {
            worktree.load_binary_file(path.as_ref(), cx)
        });

        let downloaded_file = download_task.await?;
        let content = downloaded_file.content;
        let file = downloaded_file.file;
        log::debug!(
            "handle_download_file_by_path: file loaded, content_size={}",
            content.len()
        );

        let proto_file = worktree.read_with(&cx, |_worktree: &Worktree, cx| file.to_proto(cx));
        log::debug!(
            "handle_download_file_by_path: using client-provided file_id={}",
            file_id
        );

        let state = proto::FileState {
            id: file_id,
            file: Some(proto_file),
            content_size: content.len() as u64,
        };

        log::debug!("handle_download_file_by_path: sending State message");
        session.send(proto::CreateFileForPeer {
            project_id,
            peer_id: Some(REMOTE_SERVER_PEER_ID),
            variant: Some(Variant::State(state)),
        })?;

        const CHUNK_SIZE: usize = 1024 * 1024; // 1MB chunks
        let num_chunks = content.len().div_ceil(CHUNK_SIZE);
        log::debug!(
            "handle_download_file_by_path: sending {} chunks",
            num_chunks
        );
        for (i, chunk) in content.chunks(CHUNK_SIZE).enumerate() {
            log::trace!(
                "handle_download_file_by_path: sending chunk {}/{}, size={}",
                i + 1,
                num_chunks,
                chunk.len()
            );
            session.send(proto::CreateFileForPeer {
                project_id,
                peer_id: Some(REMOTE_SERVER_PEER_ID),
                variant: Some(Variant::Chunk(proto::FileChunk {
                    file_id,
                    data: chunk.to_vec(),
                })),
            })?;
        }

        log::debug!(
            "handle_download_file_by_path: returning file_id={}",
            file_id
        );
        Ok(proto::DownloadFileResponse { file_id })
    }

    pub async fn handle_open_new_buffer(
        this: Entity<Self>,
        _message: TypedEnvelope<proto::OpenNewBuffer>,
        mut cx: AsyncApp,
    ) -> Result<proto::OpenBufferResponse> {
        let (buffer_store, buffer) = this.update(&mut cx, |this, cx| {
            let buffer_store = this.buffer_store.clone();
            let buffer = this.buffer_store.update(cx, |buffer_store, cx| {
                buffer_store.create_buffer(None, true, cx)
            });
            (buffer_store, buffer)
        });

        let buffer = buffer.await?;
        let buffer_id = buffer.read_with(&cx, |b, _| b.remote_id());
        buffer_store.update(&mut cx, |buffer_store, cx| {
            buffer_store
                .create_buffer_for_peer(&buffer, REMOTE_SERVER_PEER_ID, cx)
                .detach_and_log_err(cx);
        });

        Ok(proto::OpenBufferResponse {
            buffer_id: buffer_id.to_proto(),
        })
    }

    async fn handle_toggle_lsp_logs(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::ToggleLspLogs>,
        cx: AsyncApp,
    ) -> Result<()> {
        let server_id = LanguageServerId::from_proto(envelope.payload.server_id);
        let lsp_store = this.read_with(&cx, |this, _| this.lsp_store.downgrade());
        cx.update(|cx| {
            let log_store = cx
                .try_global::<GlobalLogStore>()
                .map(|global_log_store| global_log_store.0.clone())
                .context("lsp logs store is missing")?;
            let toggled_log_kind =
                match proto::toggle_lsp_logs::LogType::try_from(envelope.payload.log_type)
                    .ok()
                    .context("invalid log type")?
                {
                    proto::toggle_lsp_logs::LogType::Log => LogKind::Logs,
                    proto::toggle_lsp_logs::LogType::Trace => LogKind::Trace,
                    proto::toggle_lsp_logs::LogType::Rpc => LogKind::Rpc,
                };
            let server_key =
                LanguageServerLogKey::new(LanguageServerKind::LocalSsh { lsp_store }, server_id);
            log_store.update(cx, |log_store, _| {
                log_store.toggle_lsp_logs(&server_key, envelope.payload.enabled, toggled_log_kind);
            });
            anyhow::Ok(())
        })?;

        Ok(())
    }

    async fn handle_open_server_settings(
        this: Entity<Self>,
        _: TypedEnvelope<proto::OpenServerSettings>,
        mut cx: AsyncApp,
    ) -> Result<proto::OpenBufferResponse> {
        let settings_path = paths::settings_file();
        let (worktree, path) = this
            .update(&mut cx, |this, cx| {
                this.worktree_store.update(cx, |worktree_store, cx| {
                    worktree_store.find_or_create_worktree(settings_path, false, cx)
                })
            })
            .await?;

        let (buffer, buffer_store) = this.update(&mut cx, |this, cx| {
            let buffer = this.buffer_store.update(cx, |buffer_store, cx| {
                buffer_store.open_buffer(
                    ProjectPath {
                        worktree_id: worktree.read(cx).id(),
                        path,
                    },
                    cx,
                )
            });

            (buffer, this.buffer_store.clone())
        });

        let buffer = buffer.await?;

        let buffer_id = cx.update(|cx| {
            if buffer.read(cx).is_empty() {
                buffer.update(cx, |buffer, cx| {
                    buffer.edit([(0..0, initial_server_settings_content())], None, cx)
                });
            }

            let buffer_id = buffer.read(cx).remote_id();

            buffer_store.update(cx, |buffer_store, cx| {
                buffer_store
                    .create_buffer_for_peer(&buffer, REMOTE_SERVER_PEER_ID, cx)
                    .detach_and_log_err(cx);
            });

            buffer_id
        });

        Ok(proto::OpenBufferResponse {
            buffer_id: buffer_id.to_proto(),
        })
    }

    async fn handle_spawn_kernel(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SpawnKernel>,
        cx: AsyncApp,
    ) -> Result<proto::SpawnKernelResponse> {
        let fs = this.update(&mut cx.clone(), |this, _| this.fs.clone());

        let mut ports = Vec::new();
        for _ in 0..5 {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            let port = listener.local_addr()?.port();
            ports.push(port);
        }

        let connection_info = serde_json::json!({
            "shell_port": ports[0],
            "iopub_port": ports[1],
            "stdin_port": ports[2],
            "control_port": ports[3],
            "hb_port": ports[4],
            "ip": "127.0.0.1",
            "key": uuid::Uuid::new_v4().to_string(),
            "transport": "tcp",
            "signature_scheme": "hmac-sha256",
            "kernel_name": envelope.payload.kernel_name,
        });

        let connection_file_content = serde_json::to_string_pretty(&connection_info)?;
        let kernel_id = uuid::Uuid::new_v4().to_string();

        let connection_file_path = std::env::temp_dir().join(format!("kernel-{}.json", kernel_id));
        fs.save(
            &connection_file_path,
            &connection_file_content.as_str().into(),
            language::LineEnding::Unix,
        )
        .await?;

        let working_directory = if envelope.payload.working_directory.is_empty() {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        } else {
            Some(envelope.payload.working_directory)
        };

        // Spawn kernel (Assuming python for now, or we'd need to parse kernelspec logic here or pass the command)

        // Spawn kernel
        let spawn_kernel = |binary: &str, args: &[String]| {
            let mut command = smol::process::Command::new(binary);

            if !args.is_empty() {
                for arg in args {
                    if arg == "{connection_file}" {
                        command.arg(&connection_file_path);
                    } else {
                        command.arg(arg);
                    }
                }
            } else {
                command
                    .arg("-m")
                    .arg("ipykernel_launcher")
                    .arg("-f")
                    .arg(&connection_file_path);
            }

            // This ensures subprocesses spawned from the kernel use the correct Python environment
            let python_bin_dir = std::path::Path::new(binary).parent();
            if let Some(bin_dir) = python_bin_dir {
                if let Some(path_var) = std::env::var_os("PATH") {
                    let mut paths = std::env::split_paths(&path_var).collect::<Vec<_>>();
                    paths.insert(0, bin_dir.to_path_buf());
                    if let Ok(new_path) = std::env::join_paths(paths) {
                        command.env("PATH", new_path);
                    }
                }

                if let Some(venv_root) = bin_dir.parent() {
                    command.env("VIRTUAL_ENV", venv_root.to_string_lossy().to_string());
                }
            }

            if let Some(wd) = &working_directory {
                command.current_dir(wd);
            }
            command.spawn()
        };

        // We need to manage the child process lifecycle
        let child = if !envelope.payload.command.is_empty() {
            spawn_kernel(&envelope.payload.command, &envelope.payload.args).context(format!(
                "failed to spawn kernel process (command: {})",
                envelope.payload.command
            ))?
        } else if let Some(venv_python) = working_directory
            .as_ref()
            .and_then(|wd| find_venv_python(wd))
        {
            let path_str = venv_python.to_string_lossy().to_string();
            spawn_kernel(&path_str, &[]).context(format!(
                "failed to spawn kernel process (venv: {})",
                path_str
            ))?
        } else {
            spawn_kernel("python3", &[])
                .or_else(|_| spawn_kernel("python", &[]))
                .context("failed to spawn kernel process (tried python3 and python)")?
        };

        this.update(&mut cx.clone(), |this, _cx| {
            this.kernels.insert(kernel_id.clone(), child);
        });

        Ok(proto::SpawnKernelResponse {
            kernel_id,
            connection_file: connection_file_content,
        })
    }

    async fn handle_kill_kernel(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::KillKernel>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let kernel_id = envelope.payload.kernel_id;
        let child = this.update(&mut cx, |this, _| this.kernels.remove(&kernel_id));
        if let Some(mut child) = child {
            child.kill().log_err();
        }
        Ok(proto::Ack {})
    }

    async fn handle_find_search_candidates(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::FindSearchCandidates>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        use futures::stream::StreamExt as _;

        let peer_id = envelope.original_sender_id.unwrap_or(envelope.sender_id);
        let message = envelope.payload;
        let query = SearchQuery::from_proto(
            message.query.context("missing query field")?,
            PathStyle::local(),
        )?;

        let project_id = message.project_id;
        let buffer_store = this.read_with(&cx, |this, _| this.buffer_store.clone());
        let handle = message.handle;
        let _buffer_store = buffer_store.clone();
        let client = this.read_with(&cx, |this, _| this.session.clone());
        let task = cx.spawn(async move |cx| {
            let results = this.update(cx, |this, cx| {
                project::Search::local(
                    this.fs.clone(),
                    this.buffer_store.clone(),
                    this.worktree_store.clone(),
                    message.limit as _,
                    cx,
                )
                .into_handle(query, cx)
                .matching_buffers(cx)
            });
            let (batcher, batches) =
                project::project_search::AdaptiveBatcher::new(cx.background_executor());
            let mut new_matches = Box::pin(results.rx);

            let sender_task = cx.background_executor().spawn({
                let client = client.clone();
                async move {
                    let mut batches = std::pin::pin!(batches);
                    while let Some(buffer_ids) = batches.next().await {
                        client
                            .request(proto::FindSearchCandidatesChunk {
                                handle,
                                peer_id: Some(peer_id),
                                project_id,
                                variant: Some(
                                    proto::find_search_candidates_chunk::Variant::Matches(
                                        proto::FindSearchCandidatesMatches { buffer_ids },
                                    ),
                                ),
                            })
                            .await?;
                    }
                    anyhow::Ok(())
                }
            });

            while let Some((buffer, _)) = new_matches.next().await {
                let _ = buffer_store
                    .update(cx, |this, cx| {
                        this.create_buffer_for_peer(&buffer, REMOTE_SERVER_PEER_ID, cx)
                    })
                    .await;
                let buffer_id = buffer.read_with(cx, |this, _| this.remote_id().to_proto());
                batcher.push(buffer_id).await;
            }
            batcher.flush().await;

            sender_task.await?;

            client
                .request(proto::FindSearchCandidatesChunk {
                    handle,
                    peer_id: Some(peer_id),
                    project_id,
                    variant: Some(proto::find_search_candidates_chunk::Variant::Done(
                        proto::FindSearchCandidatesDone {},
                    )),
                })
                .await?;
            anyhow::Ok(())
        });
        _buffer_store.update(&mut cx, |this, _| {
            this.register_ongoing_project_search((peer_id, handle), task);
        });

        Ok(proto::Ack {})
    }

    // Goes from client to host.
    async fn handle_find_search_candidates_cancel(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::FindSearchCandidatesCancelled>,
        mut cx: AsyncApp,
    ) -> Result<()> {
        let buffer_store = this.read_with(&mut cx, |this, _| this.buffer_store.clone());
        BufferStore::handle_find_search_candidates_cancel(buffer_store, envelope, cx).await
    }

    async fn handle_list_remote_directory(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::ListRemoteDirectory>,
        cx: AsyncApp,
    ) -> Result<proto::ListRemoteDirectoryResponse> {
        use smol::stream::StreamExt;
        let fs = cx.read_entity(&this, |this, _| this.fs.clone());
        let expanded = PathBuf::from(shellexpand::tilde(&envelope.payload.path).to_string());
        let check_info = envelope
            .payload
            .config
            .as_ref()
            .is_some_and(|config| config.is_dir);

        let mut entries = Vec::new();
        let mut entry_info = Vec::new();
        let mut response = fs.read_dir(&expanded).await?;
        while let Some(path) = response.next().await {
            let path = path?;
            if let Some(file_name) = path.file_name() {
                entries.push(file_name.to_string_lossy().into_owned());
                if check_info {
                    let is_dir = fs.is_dir(&path).await;
                    entry_info.push(proto::EntryInfo { is_dir });
                }
            }
        }
        Ok(proto::ListRemoteDirectoryResponse {
            entries,
            entry_info,
        })
    }

    async fn handle_get_path_metadata(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GetPathMetadata>,
        cx: AsyncApp,
    ) -> Result<proto::GetPathMetadataResponse> {
        let fs = cx.read_entity(&this, |this, _| this.fs.clone());
        let expanded = PathBuf::from(shellexpand::tilde(&envelope.payload.path).to_string());

        let metadata = fs.metadata(&expanded).await?;
        let is_dir = metadata.map(|metadata| metadata.is_dir).unwrap_or(false);

        Ok(proto::GetPathMetadataResponse {
            exists: metadata.is_some(),
            is_dir,
            path: expanded.to_string_lossy().into_owned(),
        })
    }

    async fn handle_shutdown_remote_server(
        _this: Entity<Self>,
        _envelope: TypedEnvelope<proto::ShutdownRemoteServer>,
        cx: AsyncApp,
    ) -> Result<proto::Ack> {
        cx.spawn(async move |cx| {
            cx.update(|cx| {
                // TODO: This is a hack, because in a headless project, shutdown isn't executed
                // when calling quit, but it should be.
                cx.shutdown();
                cx.quit();
            })
        })
        .detach();

        Ok(proto::Ack {})
    }

    pub async fn handle_ping(
        _this: Entity<Self>,
        _envelope: TypedEnvelope<proto::Ping>,
        _cx: AsyncApp,
    ) -> Result<proto::Ack> {
        log::debug!("Received ping from client");
        Ok(proto::Ack {})
    }

    async fn handle_get_processes(
        _this: Entity<Self>,
        _envelope: TypedEnvelope<proto::GetProcesses>,
        _cx: AsyncApp,
    ) -> Result<proto::GetProcessesResponse> {
        let mut processes = Vec::new();
        let refresh_kind = RefreshKind::nothing().with_processes(
            ProcessRefreshKind::nothing()
                .without_tasks()
                .with_cmd(UpdateKind::Always),
        );

        for process in System::new_with_specifics(refresh_kind)
            .processes()
            .values()
        {
            let name = process.name().to_string_lossy().into_owned();
            let command = process
                .cmd()
                .iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect::<Vec<_>>();

            processes.push(proto::ProcessInfo {
                pid: process.pid().as_u32(),
                name,
                command,
            });
        }

        processes.sort_by_key(|p| p.name.clone());

        Ok(proto::GetProcessesResponse { processes })
    }

    async fn handle_get_listening_ports(
        _this: Entity<Self>,
        _envelope: TypedEnvelope<proto::GetListeningPorts>,
        cx: AsyncApp,
    ) -> Result<proto::GetListeningPortsResponse> {
        // Scanning reads files on Linux but shells out on the other platforms,
        // so it is kept off the thread that serves the rest of the session.
        let ports = cx
            .background_spawn(async move { remote::listening_ports::scan_listening_ports().await })
            .await?;

        Ok(proto::GetListeningPortsResponse {
            ports: ports
                .into_iter()
                .map(|port| proto::ListeningPort {
                    host: port.host,
                    port: u32::from(port.port),
                })
                .collect(),
        })
    }

    async fn handle_list_tmux_sessions(
        _this: Entity<Self>,
        _envelope: TypedEnvelope<proto::ListTmuxSessions>,
        cx: AsyncApp,
    ) -> Result<proto::ListTmuxSessionsResponse> {
        // Listing shells out twice, so it is kept off the thread that serves
        // the rest of the session.
        let listing = cx
            .background_spawn(async move { remote::tmux_sessions::list_tmux_sessions().await })
            .await?;

        Ok(proto::ListTmuxSessionsResponse {
            tmux_available: listing.tmux_available,
            sessions: listing
                .sessions
                .into_iter()
                .map(|session| proto::TmuxSession {
                    name: session.name,
                    attached: session.attached,
                    window_count: session.window_count,
                    windows: session
                        .windows
                        .into_iter()
                        .map(|window| proto::TmuxWindow {
                            index: window.index,
                            name: window.name,
                            active: window.active,
                        })
                        .collect(),
                })
                .collect(),
        })
    }

    async fn handle_list_claude_sessions(
        _this: Entity<Self>,
        envelope: TypedEnvelope<proto::ListClaudeSessions>,
        cx: AsyncApp,
    ) -> Result<proto::ListClaudeSessionsResponse> {
        let home_directory = paths::home_dir().to_path_buf();
        let project_root = envelope.payload.project_root.map(PathBuf::from);

        let session_summaries = cx
            .background_spawn({
                let home_directory = home_directory.clone();
                async move {
                    remote::claude_sessions::list_sessions(&home_directory, project_root.as_deref())
                        .await
                }
            })
            .await?;

        Ok(proto::ListClaudeSessionsResponse {
            sessions: session_summaries
                .into_iter()
                .map(|summary| proto::ClaudeSession {
                    process_id: summary.session.process_id,
                    session_id: summary.session.session_id,
                    working_directory: summary
                        .session
                        .working_directory
                        .to_string_lossy()
                        .into_owned(),
                    version: summary.session.version,
                    name: summary.session.name,
                    status: summary.session.status,
                    updated_at: summary.session.updated_at,
                    tmux_target: summary.session.tmux_target,
                    transcript_path: summary
                        .transcript_path
                        .map(|transcript_path| transcript_path.to_string_lossy().into_owned()),
                })
                .collect(),
            home_directory: home_directory.to_string_lossy().into_owned(),
        })
    }

    async fn handle_list_claude_subagents(
        _this: Entity<Self>,
        envelope: TypedEnvelope<proto::ListClaudeSubagents>,
        cx: AsyncApp,
    ) -> Result<proto::ListClaudeSubagentsResponse> {
        let home_directory = paths::home_dir().to_path_buf();
        let session_id = envelope.payload.session_id;

        let subagents = cx
            .background_spawn({
                let home_directory = home_directory.clone();
                async move {
                    remote::claude_sessions::list_subagents(&home_directory, &session_id).await
                }
            })
            .await?;

        Ok(proto::ListClaudeSubagentsResponse {
            subagents: subagents
                .into_iter()
                .map(|summary| proto::ClaudeSubagent {
                    agent_id: summary.agent_id,
                    workflow_run_id: summary.workflow_run_id,
                    agent_type: summary.meta.agent_type,
                    description: summary.meta.description,
                    tool_use_id: summary.meta.tool_use_id,
                    spawn_depth: summary.meta.spawn_depth,
                    model: summary.meta.model,
                    workflow_phase: summary.meta.workflow_phase,
                    transcript_path: Some(summary.transcript_path.to_string_lossy().into_owned()),
                    size: summary.size,
                })
                .collect(),
        })
    }

    async fn handle_tail_claude_transcript(
        _this: Entity<Self>,
        envelope: TypedEnvelope<proto::TailClaudeTranscript>,
        cx: AsyncApp,
    ) -> Result<proto::TailClaudeTranscriptResponse> {
        let home_directory = paths::home_dir().to_path_buf();
        let request = envelope.payload;
        let path = tail_target(&request, &home_directory)?;
        let tail_state = remote::claude_sessions::TailState {
            path,
            offset: request.offset,
            pending: request.pending,
        };

        let progress = cx
            .background_spawn(async move {
                remote::claude_sessions::read_transcript_tail(
                    &home_directory,
                    &request.session_id,
                    tail_state,
                )
            })
            .await?;

        Ok(proto::TailClaudeTranscriptResponse {
            path: progress
                .path
                .map(|path| path.to_string_lossy().into_owned()),
            start_offset: progress.start_offset,
            offset: progress.offset,
            pending: progress.pending,
            lines: progress.lines,
            restarted: progress.restarted,
        })
    }

    async fn handle_read_claude_file(
        _this: Entity<Self>,
        envelope: TypedEnvelope<proto::ReadClaudeFile>,
        cx: AsyncApp,
    ) -> Result<proto::ReadClaudeFileResponse> {
        let home_directory = paths::home_dir().to_path_buf();
        let request = envelope.payload;
        let file_path = PathBuf::from(request.path);

        validate_claude_file_path(&file_path, &home_directory)?;

        // An upper bound of 4 MiB prevents unbounded memory allocation if a client requests
        // an excessively large file. Because protobuf defaults unset numeric fields to 0,
        // max_bytes == 0 is interpreted as requesting the default maximum limit.
        const MAXIMUM_READ_BYTES: u64 = 4 * 1024 * 1024;
        let byte_limit = if request.max_bytes == 0 {
            MAXIMUM_READ_BYTES
        } else {
            request.max_bytes.min(MAXIMUM_READ_BYTES)
        };

        let (contents, truncated) = cx
            .background_spawn(async move {
                use std::io::Read as _;
                let mut file = std::fs::File::open(&file_path)?;
                let mut read_buffer = Vec::new();
                file.by_ref()
                    .take(byte_limit + 1)
                    .read_to_end(&mut read_buffer)?;
                let is_truncated = read_buffer.len() as u64 > byte_limit;
                if is_truncated {
                    read_buffer.truncate(byte_limit as usize);
                }
                Ok::<_, anyhow::Error>((read_buffer, is_truncated))
            })
            .await?;

        Ok(proto::ReadClaudeFileResponse {
            contents,
            truncated,
        })
    }

    async fn handle_send_claude_input(
        _this: Entity<Self>,
        envelope: TypedEnvelope<proto::SendClaudeInput>,
        cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let request = envelope.payload;
        // The pane target must be sanitized before passing it to tmux to prevent command injection.
        let Some(sanitized_pane_target) =
            remote::claude_sessions::pane_target(&request.pane_target)
        else {
            anyhow::bail!("invalid tmux pane target: {:?}", request.pane_target);
        };

        cx.background_spawn(async move {
            match request.input {
                Some(proto::send_claude_input::Input::Text(text_to_send)) => {
                    remote::claude_sessions::send_text(&sanitized_pane_target, &text_to_send)
                        .await?;
                }
                Some(proto::send_claude_input::Input::Escape(_)) => {
                    remote::claude_sessions::send_escape(&sanitized_pane_target).await?;
                }
                None => anyhow::bail!("no input provided in SendClaudeInput"),
            }
            Ok::<_, anyhow::Error>(())
        })
        .await?;

        Ok(proto::Ack {})
    }

    async fn handle_get_remote_profiling_data(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GetRemoteProfilingData>,
        cx: AsyncApp,
    ) -> Result<proto::GetRemoteProfilingDataResponse> {
        let foreground_only = envelope.payload.foreground_only;

        let (deltas, now_nanos) = cx.update(|cx| {
            let timings = if foreground_only {
                vec![gpui::profiler::get_current_thread_timings(
                    TasksIncluded::OnlyCompleted,
                )]
            } else {
                gpui::profiler::get_all_timings(TasksIncluded::OnlyCompleted)
            };
            this.update(cx, |this, _cx| {
                let deltas = this.profiling_collector.collect_unseen(timings);
                let now_nanos = Instant::now()
                    .duration_since(this.profiling_collector.startup_time())
                    .as_nanos() as u64;
                (deltas, now_nanos)
            })
        });

        let threads = deltas
            .into_iter()
            .map(|delta| proto::RemoteProfilingThread {
                thread_name: delta.thread_name,
                thread_id: delta.thread_id,
                timings: delta
                    .new_timings
                    .into_iter()
                    .map(|t| proto::RemoteProfilingTiming {
                        location: Some(proto::RemoteProfilingLocation {
                            file: t.location.file.to_string(),
                            line: t.location.line,
                            column: t.location.column,
                        }),
                        start_nanos: t.start as u64,
                        duration_nanos: t.duration as u64,
                    })
                    .collect(),
            })
            .collect();

        Ok(proto::GetRemoteProfilingDataResponse { threads, now_nanos })
    }

    async fn handle_get_directory_environment(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GetDirectoryEnvironment>,
        mut cx: AsyncApp,
    ) -> Result<proto::DirectoryEnvironment> {
        let shell = task::shell_from_proto(envelope.payload.shell.context("missing shell")?)?;
        let directory = PathBuf::from(envelope.payload.directory);
        let environment = this
            .update(&mut cx, |this, cx| {
                this.environment.update(cx, |environment, cx| {
                    environment.local_directory_environment(&shell, directory.into(), cx)
                })
            })
            .await
            .context("failed to get directory environment")?
            .into_iter()
            .collect();
        Ok(proto::DirectoryEnvironment { environment })
    }
}

fn prompt_to_proto(
    prompt: &project::LanguageServerPromptRequest,
) -> proto::language_server_prompt_request::Level {
    match prompt.level {
        PromptLevel::Info => proto::language_server_prompt_request::Level::Info(
            proto::language_server_prompt_request::Info {},
        ),
        PromptLevel::Warning => proto::language_server_prompt_request::Level::Warning(
            proto::language_server_prompt_request::Warning {},
        ),
        PromptLevel::Critical => proto::language_server_prompt_request::Level::Critical(
            proto::language_server_prompt_request::Critical {},
        ),
    }
}

fn find_venv_python(working_directory: &str) -> Option<std::path::PathBuf> {
    let wd = std::path::Path::new(working_directory);
    for dir_name in &[".venv", "venv", ".env", "env"] {
        let venv_dir = wd.join(dir_name);
        let has_pyvenv_cfg = venv_dir.join("pyvenv.cfg").is_file();
        let has_activate = venv_dir.join("bin").join("activate").is_file();
        if has_pyvenv_cfg || has_activate {
            let python = venv_dir.join("bin").join("python");
            if python.is_file() {
                return Some(python);
            }
            let python3 = venv_dir.join("bin").join("python3");
            if python3.is_file() {
                return Some(python3);
            }
        }
    }
    None
}

// The path a tail is allowed to follow, which is only ever the transcript of the session
// the request names.
//
// A client sends back the path it is already following so that a replaced file can be
// noticed, but a path out of a request is not evidence of anything: the tail reads
// whatever path it is handed, so without this it would answer with the contents of any
// file the remote server can read, session key files included. A path that does not
// belong to this session is dropped rather than refused, because the session's real
// transcript is then located the same way the first request located it.
fn transcript_path_to_follow(
    path: Option<&str>,
    session_id: &str,
    home_directory: &Path,
) -> Option<PathBuf> {
    let path = PathBuf::from(path?);
    let projects_directory = home_directory.join(".claude").join("projects");
    let expected_file_name = format!("{session_id}.jsonl");

    let names_this_sessions_transcript = path
        .file_name()
        .is_some_and(|file_name| file_name == OsStr::new(&expected_file_name));
    if !path.is_absolute()
        || !path.starts_with(&projects_directory)
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        || !names_this_sessions_transcript
    {
        return None;
    }

    Some(path)
}

// When an agent_id is provided, completely ignore any client-provided path.
// A path out of a request is not evidence of anything: the main session transcript
// tail accepts a client-provided path solely to detect if the file was replaced,
// whereas each subagent transcript path is resolved directly by the server via
// directory lookup at the cost of a single read_dir, so there is no reason to
// trust or follow a client-supplied path.
fn tail_target(
    request: &proto::TailClaudeTranscript,
    home_directory: &Path,
) -> Result<Option<PathBuf>> {
    if let Some(agent_id) = request.agent_id.as_deref() {
        let path = remote::claude_sessions::subagent_transcript_path(
            home_directory,
            &request.session_id,
            agent_id,
            request.workflow_run_id.as_deref(),
        )
        .ok_or_else(|| {
            anyhow!(
                "transcript for subagent {:?} in session {:?} not found",
                agent_id,
                request.session_id
            )
        })?;
        Ok(Some(path))
    } else {
        Ok(transcript_path_to_follow(
            request.path.as_deref(),
            &request.session_id,
            home_directory,
        ))
    }
}

// The files this protocol may read back, which are the tool outputs Claude Code persists
// beside a transcript, under `~/.claude/projects`.
//
// The boundary is the projects directory rather than the whole of `~/.claude` because
// `~/.claude/sessions` holds the `<pid>.<sha256>.key` credential for each session's
// messaging socket, and a path out of a request is not evidence of anything: a boundary
// at `~/.claude` would answer a request for a key file with its contents. Requiring an
// absolute path with no parent traversal component ("..") is what makes the prefix check
// mean what it says.
fn validate_claude_file_path(path: &Path, home_directory: &Path) -> Result<()> {
    if !path.is_absolute() {
        anyhow::bail!("path must be absolute: {}", path.display());
    }
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        anyhow::bail!("path must not contain '..' components: {}", path.display());
    }
    let allowed_directory = home_directory.join(".claude").join("projects");
    if !path.starts_with(&allowed_directory) {
        anyhow::bail!(
            "path {} is outside the allowed directory {}",
            path.display(),
            allowed_directory.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn test_validate_claude_file_path() {
        let home_directory = PathBuf::from("/Users/testuser");

        // Positive case: absolute path within <home>/.claude/
        let valid_path = home_directory.join(".claude/projects/x/tool-results/a.txt");
        assert!(validate_claude_file_path(&valid_path, &home_directory).is_ok());

        // Negative case: relative path
        let relative_path = Path::new("projects/x/tool-results/a.txt");
        assert!(validate_claude_file_path(relative_path, &home_directory).is_err());

        // Negative case: relative path with leading .claude
        let relative_claude_path = Path::new(".claude/projects/x/tool-results/a.txt");
        assert!(validate_claude_file_path(relative_claude_path, &home_directory).is_err());

        // Negative case: path containing ".." component
        let parent_directory_path = home_directory.join(".claude/../.ssh/id_rsa");
        assert!(validate_claude_file_path(&parent_directory_path, &home_directory).is_err());

        let internal_traversal_path = home_directory.join(".claude/projects/x/../../id_rsa");
        assert!(validate_claude_file_path(&internal_traversal_path, &home_directory).is_err());

        // Negative case: path within user home but outside .claude
        let ssh_key_path = home_directory.join(".ssh/id_rsa");
        assert!(validate_claude_file_path(&ssh_key_path, &home_directory).is_err());

        // Negative case: system file outside user home
        let password_file_path = Path::new("/etc/passwd");
        assert!(validate_claude_file_path(password_file_path, &home_directory).is_err());
    }

    #[test]
    fn test_a_tail_only_follows_the_transcript_of_the_session_it_names() {
        let home_directory = PathBuf::from("/Users/testuser");
        let session = "095bcff6-b9a8-4584-a3c6-861f16c9a807";
        let transcript = home_directory
            .join(".claude/projects/-Users-testuser-work")
            .join(format!("{session}.jsonl"));

        assert_eq!(
            transcript_path_to_follow(transcript.to_str(), session, &home_directory),
            Some(transcript.clone()),
            "the session's own transcript is what a tail is for"
        );
        assert_eq!(
            transcript_path_to_follow(None, session, &home_directory),
            None,
            "a request with no path leaves the transcript to be located from the session id"
        );

        for rejected in [
            home_directory.join(".claude/sessions/17694.a66fc5e9.key"),
            home_directory.join(".ssh/id_rsa"),
            PathBuf::from("/etc/passwd"),
            home_directory.join(".claude/projects/x/../../.ssh/id_rsa"),
            home_directory.join(".claude/projects/x/another-session.jsonl"),
            // Right name, no traversal, wrong directory: only the prefix check rejects
            // these two, and the first of them is a session key file's neighbour.
            home_directory
                .join(".claude/sessions")
                .join(format!("{session}.jsonl")),
            PathBuf::from("/tmp").join(format!("{session}.jsonl")),
            // Only the parent-directory check stands between this and a key file: it is
            // under the projects directory lexically and it does carry the expected name.
            home_directory
                .join(".claude/projects/x/../../.claude/sessions")
                .join(format!("{session}.jsonl")),
            PathBuf::from(".claude/projects/x").join(format!("{session}.jsonl")),
        ] {
            assert_eq!(
                transcript_path_to_follow(rejected.to_str(), session, &home_directory),
                None,
                "a tail must not be talked into reading {}",
                rejected.display()
            );
        }
    }

    #[test]
    fn test_pane_target_sanitization() {
        assert_eq!(
            remote::claude_sessions::pane_target("%3"),
            Some("%3".to_string())
        );
        assert_eq!(
            remote::claude_sessions::pane_target("session:@0.%42"),
            Some("%42".to_string())
        );
        assert_eq!(remote::claude_sessions::pane_target("invalid"), None);
        assert_eq!(remote::claude_sessions::pane_target("%"), None);
        assert_eq!(remote::claude_sessions::pane_target("%3; malicious"), None);
    }

    #[test]
    fn a_read_is_confined_to_the_directory_the_client_may_offer() {
        let home_directory = PathBuf::from("/Users/testuser");

        // Every path the panel can ask for: `persisted_output_is_loadable` only offers
        // one under `<home>/.claude/projects`, so nothing a client legitimately sends is
        // refused by a boundary drawn at the same directory.
        for offered in [
            home_directory.join(".claude/projects/-Users-testuser-work/tool-results/a.txt"),
            home_directory.join(".claude/projects/-Users-testuser-work/memory/notes.md"),
            home_directory.join(".claude/projects/-Users-testuser-work/095bcff6-b9a8-4584.jsonl"),
            home_directory.join(".claude/projects"),
        ] {
            assert!(
                validate_claude_file_path(&offered, &home_directory).is_ok(),
                "a file the panel offers to read must stay readable: {}",
                offered.display()
            );
        }

        // The session key file holds the credential for a session's messaging socket,
        // and nothing in this protocol has any reason to read it.
        for refused in [
            home_directory.join(".claude/sessions/17694.a66fc5e9.key"),
            home_directory.join(".claude/sessions/17694.a66fc5e9.json"),
            home_directory.join(".claude/.credentials.json"),
            home_directory.join(".claude/todos/4e2e3600.json"),
        ] {
            let outcome = validate_claude_file_path(&refused, &home_directory);
            assert!(
                outcome.is_err(),
                "reading {} must be refused, but validation answered {:?}",
                refused.display(),
                outcome
            );
        }
    }

    #[test]
    fn test_tail_target_when_agent_id_is_none_uses_client_path() {
        let home_directory = PathBuf::from("/Users/testuser");
        let session_id = "095bcff6-b9a8-4584-a3c6-861f16c9a807";
        let valid_main_transcript = home_directory
            .join(".claude/projects/-Users-testuser-work")
            .join(format!("{session_id}.jsonl"));

        let request_with_path = proto::TailClaudeTranscript {
            project_id: 1,
            session_id: session_id.to_string(),
            path: Some(valid_main_transcript.to_str().unwrap().to_string()),
            offset: 0,
            pending: Vec::new(),
            agent_id: None,
            workflow_run_id: None,
        };
        assert_eq!(
            tail_target(&request_with_path, &home_directory).unwrap(),
            Some(valid_main_transcript)
        );

        let request_without_path = proto::TailClaudeTranscript {
            project_id: 1,
            session_id: session_id.to_string(),
            path: None,
            offset: 0,
            pending: Vec::new(),
            agent_id: None,
            workflow_run_id: None,
        };
        assert_eq!(
            tail_target(&request_without_path, &home_directory).unwrap(),
            None
        );

        let request_with_invalid_path = proto::TailClaudeTranscript {
            project_id: 1,
            session_id: session_id.to_string(),
            path: Some("/etc/passwd".to_string()),
            offset: 0,
            pending: Vec::new(),
            agent_id: None,
            workflow_run_id: None,
        };
        assert_eq!(
            tail_target(&request_with_invalid_path, &home_directory).unwrap(),
            None
        );
    }

    #[test]
    fn test_tail_target_when_agent_id_is_some_ignores_client_path() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let home_directory = temporary_directory.path();
        let session_id = "095bcff6-b9a8-4584-a3c6-861f16c9a807";
        let agent_id = "a1b2c3d4";

        let valid_main_transcript = home_directory
            .join(".claude/projects/-Users-testuser-work")
            .join(format!("{session_id}.jsonl"));

        // Flat subagent layout: .claude/projects/<project>/<session_id>/subagents/agent-<agent_id>.jsonl
        let subagent_directory = home_directory
            .join(".claude/projects/-Users-testuser-work")
            .join(session_id)
            .join("subagents");
        std::fs::create_dir_all(&subagent_directory).unwrap();
        let subagent_transcript = subagent_directory.join(format!("agent-{agent_id}.jsonl"));
        std::fs::write(&subagent_transcript, b"{}").unwrap();

        // When agent_id is present, client-provided path must be ignored even if it is a valid main transcript path
        let request = proto::TailClaudeTranscript {
            project_id: 1,
            session_id: session_id.to_string(),
            path: Some(valid_main_transcript.to_str().unwrap().to_string()),
            offset: 0,
            pending: Vec::new(),
            agent_id: Some(agent_id.to_string()),
            workflow_run_id: None,
        };

        let result = tail_target(&request, home_directory).unwrap();
        assert_eq!(
            result,
            Some(subagent_transcript),
            "when agent_id is present, client path must be ignored in favor of the subagent transcript"
        );

        // Workflow run layout: .claude/projects/<project>/<session_id>/subagents/workflows/<run_id>/agent-<agent_id>.jsonl
        let workflow_run_id = "wf-run-123";
        let workflow_directory = subagent_directory.join("workflows").join(workflow_run_id);
        std::fs::create_dir_all(&workflow_directory).unwrap();
        let workflow_transcript = workflow_directory.join("agent-wf-agent.jsonl");
        std::fs::write(&workflow_transcript, b"{}").unwrap();

        let workflow_request = proto::TailClaudeTranscript {
            project_id: 1,
            session_id: session_id.to_string(),
            path: Some(valid_main_transcript.to_str().unwrap().to_string()),
            offset: 0,
            pending: Vec::new(),
            agent_id: Some("wf-agent".to_string()),
            workflow_run_id: Some(workflow_run_id.to_string()),
        };

        let workflow_result = tail_target(&workflow_request, home_directory).unwrap();
        assert_eq!(
            workflow_result,
            Some(workflow_transcript),
            "when agent_id and workflow_run_id are present, client path must be ignored in favor of the workflow subagent transcript"
        );
    }

    #[test]
    fn test_tail_target_when_agent_id_cannot_be_resolved_returns_error() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let home_directory = temporary_directory.path();
        let session_id = "095bcff6-b9a8-4584-a3c6-861f16c9a807";

        let valid_main_transcript = home_directory
            .join(".claude/projects/-Users-testuser-work")
            .join(format!("{session_id}.jsonl"));

        let request_nonexistent_agent = proto::TailClaudeTranscript {
            project_id: 1,
            session_id: session_id.to_string(),
            path: Some(valid_main_transcript.to_str().unwrap().to_string()),
            offset: 0,
            pending: Vec::new(),
            agent_id: Some("nonexistent".to_string()),
            workflow_run_id: None,
        };

        let result = tail_target(&request_nonexistent_agent, home_directory);
        assert!(
            result.is_err(),
            "resolving a nonexistent agent must return an error even if client passed a valid path"
        );

        let request_invalid_agent_id = proto::TailClaudeTranscript {
            project_id: 1,
            session_id: session_id.to_string(),
            path: Some(valid_main_transcript.to_str().unwrap().to_string()),
            offset: 0,
            pending: Vec::new(),
            agent_id: Some("../escape".to_string()),
            workflow_run_id: None,
        };

        let result = tail_target(&request_invalid_agent_id, home_directory);
        assert!(
            result.is_err(),
            "an invalid agent_id that fails path validation must return an error"
        );
    }
}
