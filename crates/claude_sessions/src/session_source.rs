//! Where the panel's view of the Claude Code sessions comes from.
//!
//! Everything the panel needs from the machine the sessions run on goes through this
//! trait: listing the sessions, following a transcript, reading a persisted output file
//! back, and talking to a session through the Zed channel. A remote project's sessions
//! live on the far end of a connection, and a second implementation of these methods is
//! the whole difference between reading them and reading this machine's.
//!
//! Every method hands back a [`Task`] and does its IO on the background executor, so a
//! `ps` invocation or a multi-megabyte read cannot stall the frame the panel is drawing.

use std::{
    fs,
    io::Read as _,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use collections::HashMap;
use gpui::{BackgroundExecutor, Task};
use rpc::{AnyProtoClient, proto};
#[cfg(target_family = "wasm")]
use serde::Deserialize;
#[cfg(target_family = "wasm")]
use serde_json::json;
#[cfg(target_family = "wasm")]
use std::sync::OnceLock;
#[cfg(any(test, target_family = "wasm"))]
use util::ResultExt as _;

use crate::session_registry::{
    self, AgentListing, CacheTtl, ChannelStatus, HookInstallOutcome, KeepAliveRecord,
    KeepAliveWrite, RegisteredSession, SessionSummary, SlashCommand, SlashCommandScope,
    SubagentMeta, SubagentSummary, TAIL_FIRST_READ_WINDOW_BYTES, TailProgress, TailState,
    TranscriptSpend, channel_answer_permission, channel_interrupt, channel_send_message,
    channel_status, now_millis, read_channel_inbox_tail_within, read_events_tail_within,
    read_session_status, read_subagent_transcript_tail, read_subagent_transcript_tail_within,
    read_transcript_tail, read_transcript_tail_within,
};

/// The prefix of a file that was read, and whether the file went on past it.
pub struct FileContents {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

/// What one scan reports: the sessions, and the home directory of the machine they were
/// scanned on.
///
/// The home directory travels with the sessions because every path in this listing — and
/// every path in the transcripts it leads to — was written by that machine. A boundary
/// under the home directory can only be checked against the home directory of the machine
/// that produced the path, which for a remote project is not the one Zed runs under.
pub struct SessionListing {
    pub sessions: Vec<SessionSummary>,
    pub home_directory: PathBuf,
    pub liveness_unavailable_reason: Option<String>,
}

pub trait SessionSource: Send + Sync + 'static {
    fn list_sessions(&self, project_root: Option<PathBuf>) -> Task<Result<SessionListing>>;

    fn tail_transcript(&self, session_id: String, state: TailState) -> Task<Result<TailProgress>>;

    /// Like [`Self::tail_transcript`], with a first read that delivers only the lines in
    /// the last `window` bytes of the file and reports how much it skipped. A source whose
    /// far end has no way to report that reads the whole file instead.
    fn tail_transcript_within(
        &self,
        session_id: String,
        state: TailState,
        _window: u64,
    ) -> Task<Result<TailProgress>> {
        self.tail_transcript(session_id, state)
    }

    /// Every subagent conversation the session has spawned.
    fn list_subagents(&self, session_id: String) -> Task<Result<Vec<SubagentSummary>>>;

    /// The same, for every listed session at once, keyed by session id.
    ///
    /// Separate from [`Self::list_subagents`] rather than a loop over it, because the
    /// panel draws the agents of every session it lists: locating one session's directory
    /// means searching the whole projects directory, and repeating that per session on
    /// every poll is the cost this avoids.
    ///
    /// Every id asked for appears in the result, mapping to an empty list when that
    /// session has spawned nothing, so a caller never has to tell "none" from "unknown".
    fn list_subagents_for_sessions(
        &self,
        session_ids: Vec<String>,
    ) -> Task<Result<HashMap<String, Vec<SubagentSummary>>>>;

    fn tail_events(&self, session_id: String, state: TailState) -> Task<Result<TailProgress>>;

    fn read_status(&self, session_id: String) -> Task<Result<Option<String>>>;

    fn install_hooks(&self) -> Task<Result<HookInstallOutcome>>;

    fn uninstall_hooks(&self) -> Task<Result<()>> {
        Task::ready(Err(anyhow::anyhow!("uninstalling hooks is not available")))
    }

    fn hooks_installed(&self) -> Task<Result<bool>>;

    /// The slash commands a session in this project answers to.
    fn list_slash_commands(&self, project_root: Option<PathBuf>)
    -> Task<Result<Vec<SlashCommand>>>;

    /// The paths under `directory` that a partly typed `@` names, relative to it.
    ///
    /// Walked on the machine the session runs on: the working directory is that
    /// machine's, and a path this side could reach is not one the session can read.
    fn list_session_files(&self, directory: PathBuf, query: String) -> Task<Result<Vec<String>>>;

    /// Writes a file pasted into the message box where the session can read it, and
    /// reports the path to give it. Only the name's last component is taken from here;
    /// where it goes is the host's to decide.
    fn write_session_file(&self, name: String, contents: Vec<u8>) -> Task<Result<String>>;

    /// Follows one subagent's conversation. The three ids name the file rather than a
    /// path doing it, because only the machine the agent ran on can turn them into one,
    /// and it has to answer for the boundary they cross on every read.
    fn tail_subagent(
        &self,
        session_id: String,
        agent_id: String,
        workflow_run_id: Option<String>,
        state: TailState,
    ) -> Task<Result<TailProgress>>;

    /// Like [`Self::tail_subagent`], bounded the way [`Self::tail_transcript_within`] is.
    fn tail_subagent_within(
        &self,
        session_id: String,
        agent_id: String,
        workflow_run_id: Option<String>,
        state: TailState,
        _window: u64,
    ) -> Task<Result<TailProgress>> {
        self.tail_subagent(session_id, agent_id, workflow_run_id, state)
    }

    /// Reads at most `max_bytes` of `path`, reporting whether the file continued past
    /// them.
    fn read_file(&self, path: PathBuf, max_bytes: u64) -> Task<Result<FileContents>>;

    /// Reads at most `max_bytes` of a file the session named as a `SendUserFile`
    /// attachment. Separate from [`Self::read_file`] because the two are allowed to read
    /// different things: an attachment is judged against the session's own working
    /// directory, and the machine the session runs on is the one that decides, from that
    /// session's registration rather than from anything sent here.
    fn read_attachment(
        &self,
        session_id: String,
        path: PathBuf,
        max_bytes: u64,
    ) -> Task<Result<FileContents>>;

    fn channel_status(&self, claude_pid: u32) -> Task<Result<ChannelStatus>>;

    fn channel_send_message(&self, claude_pid: u32, content: String) -> Task<Result<String>>;

    fn channel_interrupt(&self, claude_pid: u32, reason: String) -> Task<Result<String>>;

    fn channel_answer_permission(
        &self,
        claude_pid: u32,
        request_id: String,
        allow: bool,
    ) -> Task<Result<String>>;

    fn tail_channel_inbox(&self, claude_pid: u32, state: TailState) -> Task<Result<TailProgress>>;

    /// Whether this source reads a machine other than the one Zed is running on.
    /// A sent file that exists under the same path locally is still that other
    /// machine's file, so opening one always fetches rather than using the local name.
    fn is_remote(&self) -> bool {
        false
    }

    /// `claude agents --json` on the machine the sessions run on.
    fn list_agents(&self) -> Task<Result<Vec<AgentListing>>> {
        Task::ready(Ok(Vec::new()))
    }

    /// `claude --bg --resume <session_id>` in `cwd`.
    fn resume_session_in_background(
        &self,
        _session_id: String,
        _cwd: PathBuf,
    ) -> Task<Result<String>> {
        Task::ready(Err(anyhow::anyhow!(
            "resuming a session in the background is not available"
        )))
    }

    /// `claude respawn <id>` or `claude stop <id>` in `cwd`.
    fn run_claude_agent_command(&self, _args: Vec<String>, _cwd: PathBuf) -> Task<Result<String>> {
        Task::ready(Err(anyhow::anyhow!(
            "this Claude agent command is not available"
        )))
    }

    /// The keep-alive records the machine the sessions run on holds for `session_ids`, which
    /// every Zed connected to it shares; only the sessions that have one are returned. An error
    /// means that machine cannot share them (an older remote server), and keep-alive is then
    /// this Zed's alone.
    fn read_keep_alive(&self, _session_ids: Vec<String>) -> Task<Result<Vec<KeepAliveRecord>>> {
        Task::ready(Err(anyhow::anyhow!("sharing keep-alive is not available")))
    }

    /// Replaces the shared keep-alive record of `session_id` if it is still at
    /// `expected_revision`.
    fn write_keep_alive(
        &self,
        _session_id: String,
        _expected_revision: u64,
        _state_json: String,
    ) -> Task<Result<KeepAliveWrite>> {
        Task::ready(Err(anyhow::anyhow!("sharing keep-alive is not available")))
    }

    /// Types `/compact` into the tmux pane the session runs in.
    fn compact_session(&self, _session_id: String) -> Task<Result<()>> {
        Task::ready(Err(anyhow::anyhow!(
            "compacting a session is not available"
        )))
    }

    /// Like [`Self::list_session_files`], naming the session whose working directory
    /// bounds the walk. Sources that do not know a session still answer from `directory`.
    fn list_session_files_in(
        &self,
        _session_id: String,
        directory: PathBuf,
        query: String,
    ) -> Task<Result<Vec<String>>> {
        self.list_session_files(directory, query)
    }

    /// Like [`Self::list_slash_commands`], naming the session `project_root` must sit in.
    fn list_slash_commands_for(
        &self,
        project_root: Option<PathBuf>,
        _session_id: Option<String>,
    ) -> Task<Result<Vec<SlashCommand>>> {
        self.list_slash_commands(project_root)
    }
}

/// The sessions running on the machine Zed itself is running on.
pub struct LocalSource {
    home_directory: PathBuf,
    executor: BackgroundExecutor,
}

impl LocalSource {
    pub fn new(executor: BackgroundExecutor) -> Self {
        Self {
            home_directory: paths::home_dir().clone(),
            executor,
        }
    }
}

impl SessionSource for LocalSource {
    fn list_sessions(&self, project_root: Option<PathBuf>) -> Task<Result<SessionListing>> {
        let home_directory = self.home_directory.clone();
        self.executor.spawn(async move {
            let sessions =
                session_registry::list_sessions(&home_directory, project_root.as_deref()).await?;
            Ok(SessionListing {
                sessions,
                home_directory,
                liveness_unavailable_reason: session_registry::liveness_unavailable_reason()
                    .map(str::to_string),
            })
        })
    }

    fn tail_transcript(&self, session_id: String, state: TailState) -> Task<Result<TailProgress>> {
        let home_directory = self.home_directory.clone();
        self.executor
            .spawn(async move { read_transcript_tail(&home_directory, &session_id, state) })
    }

    fn tail_transcript_within(
        &self,
        session_id: String,
        state: TailState,
        window: u64,
    ) -> Task<Result<TailProgress>> {
        let home_directory = self.home_directory.clone();
        self.executor.spawn(async move {
            read_transcript_tail_within(&home_directory, &session_id, state, window)
        })
    }

    fn list_subagents(&self, session_id: String) -> Task<Result<Vec<SubagentSummary>>> {
        let home_directory = self.home_directory.clone();
        self.executor.spawn(async move {
            session_registry::list_subagents(&home_directory, &session_id).await
        })
    }

    fn list_subagents_for_sessions(
        &self,
        session_ids: Vec<String>,
    ) -> Task<Result<HashMap<String, Vec<SubagentSummary>>>> {
        let home_directory = self.home_directory.clone();
        self.executor.spawn(async move {
            session_registry::list_subagents_for_sessions(&home_directory, &session_ids).await
        })
    }

    fn tail_events(&self, session_id: String, state: TailState) -> Task<Result<TailProgress>> {
        let home_directory = self.home_directory.clone();
        self.executor.spawn(async move {
            read_events_tail_within(
                &home_directory,
                &session_id,
                state,
                TAIL_FIRST_READ_WINDOW_BYTES,
            )
        })
    }

    fn read_status(&self, session_id: String) -> Task<Result<Option<String>>> {
        let home_directory = self.home_directory.clone();
        self.executor
            .spawn(async move { read_session_status(&home_directory, &session_id) })
    }

    fn install_hooks(&self) -> Task<Result<HookInstallOutcome>> {
        let home_directory = self.home_directory.clone();
        self.executor
            .spawn(async move { session_registry::install_zed_hooks(&home_directory) })
    }

    fn uninstall_hooks(&self) -> Task<Result<()>> {
        let home_directory = self.home_directory.clone();
        self.executor
            .spawn(async move { session_registry::uninstall_zed_hooks(&home_directory) })
    }

    fn hooks_installed(&self) -> Task<Result<bool>> {
        let home_directory = self.home_directory.clone();
        self.executor
            .spawn(async move { Ok(session_registry::zed_hooks_installed(&home_directory)) })
    }

    fn list_slash_commands(
        &self,
        project_root: Option<PathBuf>,
    ) -> Task<Result<Vec<SlashCommand>>> {
        let home_directory = self.home_directory.clone();
        self.executor.spawn(async move {
            Ok(session_registry::list_slash_commands(
                &home_directory,
                project_root.as_deref(),
            ))
        })
    }

    fn list_session_files(&self, directory: PathBuf, query: String) -> Task<Result<Vec<String>>> {
        self.executor
            .spawn(async move { Ok(session_registry::list_files_under(&directory, &query)) })
    }

    fn write_session_file(&self, name: String, contents: Vec<u8>) -> Task<Result<String>> {
        let home_directory = self.home_directory.clone();
        self.executor.spawn(async move {
            session_registry::write_pasted_file(&home_directory, &name, &contents)
        })
    }

    fn tail_subagent(
        &self,
        session_id: String,
        agent_id: String,
        workflow_run_id: Option<String>,
        state: TailState,
    ) -> Task<Result<TailProgress>> {
        let home_directory = self.home_directory.clone();
        self.executor.spawn(async move {
            read_subagent_transcript_tail(
                &home_directory,
                &session_id,
                &agent_id,
                workflow_run_id.as_deref(),
                state,
            )
        })
    }

    fn tail_subagent_within(
        &self,
        session_id: String,
        agent_id: String,
        workflow_run_id: Option<String>,
        state: TailState,
        window: u64,
    ) -> Task<Result<TailProgress>> {
        let home_directory = self.home_directory.clone();
        self.executor.spawn(async move {
            read_subagent_transcript_tail_within(
                &home_directory,
                &session_id,
                &agent_id,
                workflow_run_id.as_deref(),
                state,
                window,
            )
        })
    }

    fn read_file(&self, path: PathBuf, max_bytes: u64) -> Task<Result<FileContents>> {
        self.executor
            .spawn(async move { read_file_prefix(&path, max_bytes) })
    }

    fn read_attachment(
        &self,
        session_id: String,
        path: PathBuf,
        max_bytes: u64,
    ) -> Task<Result<FileContents>> {
        let home_directory = self.home_directory.clone();
        self.executor.spawn(async move {
            // Checked here even though the sessions are this machine's, so that the rule
            // is applied on the machine the file is on however the session was reached.
            let working_directory =
                session_registry::session_working_directory(&home_directory, &session_id)
                    .with_context(|| format!("no session is registered as {session_id}"))?;
            anyhow::ensure!(
                session_registry::attachment_is_readable(&path, &working_directory),
                "{} is outside the working directory of session {session_id}",
                path.display(),
            );
            read_file_prefix(&path, max_bytes)
        })
    }

    fn channel_status(&self, claude_pid: u32) -> Task<Result<ChannelStatus>> {
        let home_directory = self.home_directory.clone();
        self.executor
            .spawn(async move { Ok(channel_status(&home_directory, claude_pid, now_millis())) })
    }

    fn channel_send_message(&self, claude_pid: u32, content: String) -> Task<Result<String>> {
        let home_directory = self.home_directory.clone();
        self.executor
            .spawn(async move { channel_send_message(&home_directory, claude_pid, &content) })
    }

    fn channel_interrupt(&self, claude_pid: u32, reason: String) -> Task<Result<String>> {
        let home_directory = self.home_directory.clone();
        self.executor
            .spawn(async move { channel_interrupt(&home_directory, claude_pid, &reason) })
    }

    fn channel_answer_permission(
        &self,
        claude_pid: u32,
        request_id: String,
        allow: bool,
    ) -> Task<Result<String>> {
        let home_directory = self.home_directory.clone();
        self.executor.spawn(async move {
            channel_answer_permission(&home_directory, claude_pid, &request_id, allow)
        })
    }

    fn tail_channel_inbox(&self, claude_pid: u32, state: TailState) -> Task<Result<TailProgress>> {
        let home_directory = self.home_directory.clone();
        self.executor.spawn(async move {
            read_channel_inbox_tail_within(
                &home_directory,
                claude_pid,
                state,
                TAIL_FIRST_READ_WINDOW_BYTES,
            )
        })
    }

    fn list_agents(&self) -> Task<Result<Vec<AgentListing>>> {
        let executor = self.executor.clone();
        self.executor
            .spawn(async move { session_registry::list_claude_agents(&executor).await })
    }

    fn resume_session_in_background(
        &self,
        session_id: String,
        cwd: PathBuf,
    ) -> Task<Result<String>> {
        let executor = self.executor.clone();
        self.executor.spawn(async move {
            session_registry::resume_session_in_background(&session_id, &cwd, &executor).await
        })
    }

    fn read_keep_alive(&self, session_ids: Vec<String>) -> Task<Result<Vec<KeepAliveRecord>>> {
        let home_directory = self.home_directory.clone();
        self.executor.spawn(async move {
            session_registry::read_keep_alive_records(&home_directory, &session_ids)
        })
    }

    fn write_keep_alive(
        &self,
        session_id: String,
        expected_revision: u64,
        state_json: String,
    ) -> Task<Result<KeepAliveWrite>> {
        let home_directory = self.home_directory.clone();
        self.executor.spawn(async move {
            session_registry::write_keep_alive_record(
                &home_directory,
                &session_id,
                expected_revision,
                &state_json,
            )
        })
    }

    fn compact_session(&self, session_id: String) -> Task<Result<()>> {
        let home_directory = self.home_directory.clone();
        let executor = self.executor.clone();
        self.executor.spawn(async move {
            session_registry::compact_session(&home_directory, &session_id, &executor).await
        })
    }

    fn run_claude_agent_command(&self, args: Vec<String>, cwd: PathBuf) -> Task<Result<String>> {
        let executor = self.executor.clone();
        self.executor.spawn(async move {
            session_registry::run_claude_agent_command(&args, &cwd, &executor).await
        })
    }

    fn list_session_files_in(
        &self,
        session_id: String,
        directory: PathBuf,
        query: String,
    ) -> Task<Result<Vec<String>>> {
        let home_directory = self.home_directory.clone();
        self.executor.spawn(async move {
            let directory = session_registry::session_files_directory(
                &home_directory,
                &session_id,
                &directory,
            )?;
            Ok(session_registry::list_files_under(&directory, &query))
        })
    }

    fn list_slash_commands_for(
        &self,
        project_root: Option<PathBuf>,
        session_id: Option<String>,
    ) -> Task<Result<Vec<SlashCommand>>> {
        let home_directory = self.home_directory.clone();
        self.executor.spawn(async move {
            let project_root = session_registry::slash_command_project_root(
                &home_directory,
                session_id.as_deref(),
                project_root.as_deref(),
            )?;
            Ok(session_registry::list_slash_commands(
                &home_directory,
                project_root.as_deref(),
            ))
        })
    }
}

/// The sessions running on the machine a remote project is opened from.
///
/// Every method is one request to the remote server, which answers it by calling the
/// same functions [`LocalSource`] calls directly. Nothing is decided twice: only the
/// machine the processes live on can look at them, so this side does no more than ask
/// and translate the answer.
pub struct RemoteSource {
    client: AnyProtoClient,
    executor: BackgroundExecutor,
}

impl RemoteSource {
    pub fn new(client: AnyProtoClient, executor: BackgroundExecutor) -> Self {
        Self { client, executor }
    }
}

impl SessionSource for RemoteSource {
    fn list_sessions(&self, project_root: Option<PathBuf>) -> Task<Result<SessionListing>> {
        let request = self.client.request(proto::ListClaudeSessions {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            project_root: project_root.as_deref().map(path_to_wire),
        });

        self.executor
            .spawn(async move { Ok(session_listing_from_proto(request.await?)) })
    }

    fn tail_transcript(&self, session_id: String, state: TailState) -> Task<Result<TailProgress>> {
        let request = self.client.request(proto::TailClaudeTranscript {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            session_id,
            path: state.path.as_deref().map(path_to_wire),
            offset: state.offset,
            pending: state.pending,
            // The main conversation is what an absent agent id asks for; a subagent is
            // asked for by `tail_subagent`.
            agent_id: None,
            workflow_run_id: None,
        });

        self.executor
            .spawn(async move { Ok(tail_progress_from_proto(request.await?)) })
    }

    fn list_subagents(&self, session_id: String) -> Task<Result<Vec<SubagentSummary>>> {
        let request = self.client.request(proto::ListClaudeSubagents {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            session_id,
        });

        self.executor.spawn(async move {
            Ok(request
                .await?
                .subagents
                .into_iter()
                .map(subagent_summary_from_proto)
                .collect())
        })
    }

    /// One request for every listed session's agents, so the host walks
    /// `~/.claude/projects` once rather than once per session.
    fn list_subagents_for_sessions(
        &self,
        session_ids: Vec<String>,
    ) -> Task<Result<HashMap<String, Vec<SubagentSummary>>>> {
        let request = self.client.request(proto::ListClaudeSubagentsForSessions {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            session_ids: session_ids.clone(),
        });

        self.executor.spawn(async move {
            let response = request.await?;
            let mut subagents_by_session = session_ids
                .into_iter()
                .map(|session_id| (session_id, Vec::new()))
                .collect::<HashMap<_, _>>();
            for of_session in response.by_session {
                subagents_by_session.insert(
                    of_session.session_id,
                    of_session
                        .subagents
                        .into_iter()
                        .map(subagent_summary_from_proto)
                        .collect(),
                );
            }
            Ok(subagents_by_session)
        })
    }

    fn tail_events(&self, session_id: String, state: TailState) -> Task<Result<TailProgress>> {
        let request = self.client.request(proto::TailClaudeEvents {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            session_id,
            offset: state.offset,
            pending: state.pending,
        });

        self.executor
            .spawn(async move { Ok(events_progress_from_proto(request.await?)) })
    }

    fn read_status(&self, session_id: String) -> Task<Result<Option<String>>> {
        let request = self.client.request(proto::ReadClaudeStatus {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            session_id,
        });

        self.executor
            .spawn(async move { Ok(request.await?.status_json) })
    }

    fn install_hooks(&self) -> Task<Result<HookInstallOutcome>> {
        let request = self.client.request(proto::InstallClaudeHooks {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
        });

        self.executor
            .spawn(async move { Ok(hook_install_outcome_from_proto(request.await?)) })
    }

    fn uninstall_hooks(&self) -> Task<Result<()>> {
        let request = self.client.request(proto::UninstallClaudeHooks {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
        });

        self.executor.spawn(async move {
            request.await?;
            Ok(())
        })
    }

    fn hooks_installed(&self) -> Task<Result<bool>> {
        let request = self.client.request(proto::ClaudeHooksInstalled {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
        });

        self.executor
            .spawn(async move { Ok(request.await?.installed) })
    }

    fn list_slash_commands(
        &self,
        project_root: Option<PathBuf>,
    ) -> Task<Result<Vec<SlashCommand>>> {
        let request = self.client.request(proto::ListClaudeSlashCommands {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            project_root: project_root.map(|root| root.to_string_lossy().into_owned()),
            session_id: None,
        });

        self.executor.spawn(async move {
            Ok(request
                .await?
                .commands
                .into_iter()
                .map(|command| SlashCommand {
                    name: command.name,
                    description: command.description,
                    argument_hint: command.argument_hint,
                    scope: match command.scope {
                        1 => SlashCommandScope::Project,
                        2 => SlashCommandScope::User,
                        // A scope this version has never seen says nothing about where
                        // the command came from, which is what `Builtin` says.
                        _ => SlashCommandScope::Builtin,
                    },
                })
                .collect())
        })
    }

    fn list_session_files(&self, directory: PathBuf, query: String) -> Task<Result<Vec<String>>> {
        self.list_session_files_in(String::new(), directory, query)
    }

    fn list_session_files_in(
        &self,
        session_id: String,
        directory: PathBuf,
        query: String,
    ) -> Task<Result<Vec<String>>> {
        let request = self.client.request(proto::ListClaudeSessionFiles {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            directory: directory.to_string_lossy().into_owned(),
            query,
            session_id,
        });

        self.executor.spawn(async move { Ok(request.await?.paths) })
    }

    fn write_session_file(&self, name: String, contents: Vec<u8>) -> Task<Result<String>> {
        let request = self.client.request(proto::WriteClaudeSessionFile {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            name,
            contents,
        });

        self.executor.spawn(async move { Ok(request.await?.path) })
    }

    fn tail_subagent(
        &self,
        session_id: String,
        agent_id: String,
        workflow_run_id: Option<String>,
        state: TailState,
    ) -> Task<Result<TailProgress>> {
        let request = self.client.request(proto::TailClaudeTranscript {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            session_id,
            // No path is sent at all: the far end names the agent's transcript from the
            // ids, and a path from this side is not evidence of anything it should open.
            path: None,
            offset: state.offset,
            pending: state.pending,
            agent_id: Some(agent_id),
            workflow_run_id,
        });

        self.executor
            .spawn(async move { Ok(tail_progress_from_proto(request.await?)) })
    }

    fn read_attachment(
        &self,
        session_id: String,
        path: PathBuf,
        max_bytes: u64,
    ) -> Task<Result<FileContents>> {
        let request = self.client.request(proto::ReadClaudeFile {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            path: path_to_wire(&path),
            max_bytes,
            session_id: Some(session_id),
        });

        self.executor.spawn(async move {
            let response = request.await?;
            Ok(FileContents {
                bytes: response.contents,
                truncated: response.truncated,
            })
        })
    }

    fn read_file(&self, path: PathBuf, max_bytes: u64) -> Task<Result<FileContents>> {
        let request = self.client.request(proto::ReadClaudeFile {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            path: path_to_wire(&path),
            max_bytes,
            session_id: None,
        });

        self.executor.spawn(async move {
            let response = request.await?;
            Ok(FileContents {
                bytes: response.contents,
                truncated: response.truncated,
            })
        })
    }

    fn channel_status(&self, claude_pid: u32) -> Task<Result<ChannelStatus>> {
        let request = self.client.request(proto::ClaudeChannelStatus {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            claude_pid,
        });
        self.executor.spawn(async move {
            let response = request.await?;
            Ok(ChannelStatus {
                live: response.live,
                heartbeat_at_ms: response.heartbeat_at_ms,
                server_pid: response.server_pid,
                features: response.features,
            })
        })
    }

    fn channel_send_message(&self, claude_pid: u32, content: String) -> Task<Result<String>> {
        let request = self.client.request(proto::ClaudeChannelSend {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            claude_pid,
            content,
        });
        self.executor
            .spawn(async move { Ok(request.await?.outbox_file) })
    }

    fn channel_interrupt(&self, claude_pid: u32, reason: String) -> Task<Result<String>> {
        let request = self.client.request(proto::ClaudeChannelInterrupt {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            claude_pid,
            reason,
        });
        self.executor
            .spawn(async move { Ok(request.await?.outbox_file) })
    }

    fn is_remote(&self) -> bool {
        true
    }

    fn read_keep_alive(&self, session_ids: Vec<String>) -> Task<Result<Vec<KeepAliveRecord>>> {
        let request = self.client.request(proto::ReadClaudeKeepAlive {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            session_ids,
        });
        self.executor.spawn(async move {
            Ok(request
                .await?
                .records
                .into_iter()
                .map(keep_alive_record_from_proto)
                .collect())
        })
    }

    fn write_keep_alive(
        &self,
        session_id: String,
        expected_revision: u64,
        state_json: String,
    ) -> Task<Result<KeepAliveWrite>> {
        let request = self.client.request(proto::WriteClaudeKeepAlive {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            session_id,
            expected_revision,
            state_json,
        });
        self.executor.spawn(async move {
            let response = request.await?;
            let record = response.record.map(keep_alive_record_from_proto);
            if response.applied {
                Ok(KeepAliveWrite::Applied(
                    record.context("an applied keep-alive write named no record")?,
                ))
            } else {
                Ok(KeepAliveWrite::Conflict(record))
            }
        })
    }

    fn compact_session(&self, session_id: String) -> Task<Result<()>> {
        let request = self.client.request(proto::CompactClaudeSession {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            session_id,
        });
        self.executor.spawn(async move {
            request.await?;
            Ok(())
        })
    }

    fn channel_answer_permission(
        &self,
        claude_pid: u32,
        request_id: String,
        allow: bool,
    ) -> Task<Result<String>> {
        let request = self.client.request(proto::ClaudeChannelAnswerPermission {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            claude_pid,
            request_id,
            allow,
        });
        self.executor
            .spawn(async move { Ok(request.await?.outbox_file) })
    }

    fn tail_channel_inbox(&self, claude_pid: u32, state: TailState) -> Task<Result<TailProgress>> {
        let request = self.client.request(proto::TailClaudeChannelInbox {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            claude_pid,
            offset: state.offset,
            pending: state.pending,
        });
        self.executor
            .spawn(async move { Ok(channel_inbox_progress_from_proto(request.await?)) })
    }

    fn list_agents(&self) -> Task<Result<Vec<AgentListing>>> {
        let request = self.client.request(proto::ListClaudeAgents {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
        });
        self.executor.spawn(async move {
            Ok(request
                .await?
                .agents
                .into_iter()
                .map(agent_listing_from_proto)
                .collect())
        })
    }

    fn resume_session_in_background(
        &self,
        session_id: String,
        cwd: PathBuf,
    ) -> Task<Result<String>> {
        let request = self.client.request(proto::ResumeClaudeSession {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            session_id,
            cwd: cwd.to_string_lossy().into_owned(),
        });
        self.executor
            .spawn(async move { Ok(request.await?.output) })
    }

    fn run_claude_agent_command(&self, args: Vec<String>, cwd: PathBuf) -> Task<Result<String>> {
        let request = self.client.request(proto::RunClaudeAgentCommand {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            args,
            cwd: cwd.to_string_lossy().into_owned(),
        });
        self.executor
            .spawn(async move { Ok(request.await?.output) })
    }

    fn list_slash_commands_for(
        &self,
        project_root: Option<PathBuf>,
        session_id: Option<String>,
    ) -> Task<Result<Vec<SlashCommand>>> {
        let request = self.client.request(proto::ListClaudeSlashCommands {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            project_root: project_root.map(|root| root.to_string_lossy().into_owned()),
            session_id,
        });

        self.executor.spawn(async move {
            Ok(request
                .await?
                .commands
                .into_iter()
                .map(|command| SlashCommand {
                    name: command.name,
                    description: command.description,
                    argument_hint: command.argument_hint,
                    scope: match command.scope {
                        1 => SlashCommandScope::Project,
                        2 => SlashCommandScope::User,
                        _ => SlashCommandScope::Builtin,
                    },
                })
                .collect())
        })
    }
}

#[cfg(target_family = "wasm")]
static REMOTE_CLIENT: OnceLock<smol::RpcClient> = OnceLock::new();

/// Joins one listing per session on the current task.
///
/// Nested `executor.spawn` then `await` on a single-threaded dispatcher is the
/// `scoped` / `now_or_never` hang: the children are only polled by the thread
/// that is blocked waiting for them.
#[cfg(any(test, target_family = "wasm"))]
async fn collect_subagents_by_session<F, Fut>(
    session_ids: Vec<String>,
    mut list_one: F,
) -> HashMap<String, Vec<SubagentSummary>>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<Vec<SubagentSummary>>>,
{
    let mut subagents_by_session = HashMap::default();
    for session_id in session_ids {
        // One session the far end cannot list must not hide every other
        // session's agents: the dock draws them all, and an empty list for the
        // one that failed is the same answer the local scan gives for a
        // directory it could not read.
        let subagents = match list_one(session_id.clone()).await.log_err() {
            Some(subagents) => subagents,
            None => Vec::new(),
        };
        subagents_by_session.insert(session_id, subagents);
    }
    subagents_by_session
}

/// Store the browser RPC client so wasm session I/O can run on the host.
///
/// Same shape as `smol::set_remote_client` / `terminal::set_remote_client`.
#[cfg(target_family = "wasm")]
pub fn set_remote_client(client: smol::RpcClient) {
    if REMOTE_CLIENT.set(client).is_err() {
        // Already installed by the workspace bootstrap; a second call is not a failure.
    }
}

#[cfg(target_family = "wasm")]
fn wasm_rpc_client() -> Result<smol::RpcClient> {
    REMOTE_CLIENT
        .get()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("claude_sessions remote RPC client is not initialized"))
}

/// The sessions running on the machine the browser is talking to.
///
/// Every method is one request (or a handful that LocalSource also issues as
/// separate function calls) to the web server, which answers by calling the
/// same functions [`LocalSource`] calls directly. Nothing is decided twice.
#[cfg(target_family = "wasm")]
pub struct WebSource {
    executor: BackgroundExecutor,
}

#[cfg(target_family = "wasm")]
impl WebSource {
    pub fn new(executor: BackgroundExecutor) -> Self {
        Self { executor }
    }
}

#[cfg(target_family = "wasm")]
impl WebSource {
    fn call<R: serde::de::DeserializeOwned + Send + 'static>(
        &self,
        method: &'static str,
        params: serde_json::Value,
    ) -> Task<Result<R>> {
        let client = match wasm_rpc_client() {
            Ok(client) => client,
            Err(error) => return Task::ready(Err(error)),
        };
        self.executor
            .spawn(async move { client.call::<_, R>(method, &params).await })
    }

    fn tail(&self, method: &'static str, params: serde_json::Value) -> Task<Result<TailProgress>> {
        let response = self.call::<TailProgressJson>(method, params);
        self.executor
            .spawn(async move { tail_progress_from_json(response.await?) })
    }

    fn read_claude_file(
        &self,
        path: PathBuf,
        max_bytes: u64,
        session_id: Option<String>,
    ) -> Task<Result<FileContents>> {
        let response = self.call::<ReadFileJson>(
            "ClaudeSessions::read_file",
            json!({
                "path": path_to_wire(&path),
                "max_bytes": max_bytes,
                "session_id": session_id,
            }),
        );
        self.executor.spawn(async move {
            let response = response.await?;
            Ok(FileContents {
                bytes: decode_bytes(&response.contents)?,
                truncated: response.truncated,
            })
        })
    }

    fn outbox_file(&self, method: &'static str, params: serde_json::Value) -> Task<Result<String>> {
        let response = self.call::<OutboxFileJson>(method, params);
        self.executor
            .spawn(async move { Ok(response.await?.outbox_file) })
    }
}

#[cfg(target_family = "wasm")]
impl SessionSource for WebSource {
    fn list_sessions(&self, project_root: Option<PathBuf>) -> Task<Result<SessionListing>> {
        let response = self.call::<ListSessionsJson>(
            "ClaudeSessions::list_sessions",
            json!({ "project_root": project_root.as_deref().map(path_to_wire) }),
        );
        self.executor
            .spawn(async move { Ok(session_listing_from_json(response.await?)) })
    }

    fn tail_transcript(&self, session_id: String, state: TailState) -> Task<Result<TailProgress>> {
        self.tail(
            "ClaudeSessions::read_transcript_tail",
            json!({
                "session_id": session_id,
                "path": state.path.as_deref().map(path_to_wire),
                "offset": state.offset,
                "pending": encode_bytes(&state.pending),
            }),
        )
    }

    fn tail_transcript_within(
        &self,
        session_id: String,
        state: TailState,
        window: u64,
    ) -> Task<Result<TailProgress>> {
        self.tail(
            "ClaudeSessions::read_transcript_tail",
            json!({
                "session_id": session_id,
                "path": state.path.as_deref().map(path_to_wire),
                "offset": state.offset,
                "pending": encode_bytes(&state.pending),
                "window": window,
            }),
        )
    }

    fn list_subagents(&self, session_id: String) -> Task<Result<Vec<SubagentSummary>>> {
        let response = self.call::<ListSubagentsJson>(
            "ClaudeSessions::list_subagents",
            json!({ "session_id": session_id }),
        );
        self.executor.spawn(async move {
            Ok(response
                .await?
                .subagents
                .into_iter()
                .map(subagent_summary_from_json)
                .collect())
        })
    }

    /// One request per session rather than one that names them all: the far end already
    /// answers [`ClaudeSessions::list_subagents`].
    fn list_subagents_for_sessions(
        &self,
        session_ids: Vec<String>,
    ) -> Task<Result<HashMap<String, Vec<SubagentSummary>>>> {
        let client = match wasm_rpc_client() {
            Ok(client) => client,
            Err(error) => return Task::ready(Err(error)),
        };
        self.executor.spawn(async move {
            let listed = collect_subagents_by_session(session_ids, |session_id| {
                let client = client.clone();
                async move {
                    let response = client
                        .call::<_, ListSubagentsJson>(
                            "ClaudeSessions::list_subagents",
                            &json!({ "session_id": session_id }),
                        )
                        .await?;
                    Ok(response
                        .subagents
                        .into_iter()
                        .map(subagent_summary_from_json)
                        .collect())
                }
            })
            .await;
            Ok(listed)
        })
    }

    fn tail_events(&self, session_id: String, state: TailState) -> Task<Result<TailProgress>> {
        self.tail(
            "ClaudeSessions::read_events_tail",
            json!({
                "session_id": session_id,
                "offset": state.offset,
                "pending": encode_bytes(&state.pending),
                "window": TAIL_FIRST_READ_WINDOW_BYTES,
            }),
        )
    }

    fn read_status(&self, session_id: String) -> Task<Result<Option<String>>> {
        self.call(
            "ClaudeSessions::read_session_status",
            json!({ "session_id": session_id }),
        )
    }

    fn install_hooks(&self) -> Task<Result<HookInstallOutcome>> {
        let response =
            self.call::<InstallHooksJson>("ClaudeSessions::install_zed_hooks", json!({}));
        self.executor.spawn(async move {
            let response = response.await?;
            Ok(match response.outcome.as_str() {
                "installed" => HookInstallOutcome::Installed {
                    backup_path: response.backup_path.map(PathBuf::from),
                },
                "scripts_refreshed" => HookInstallOutcome::ScriptsRefreshed,
                _ => HookInstallOutcome::AlreadyCurrent,
            })
        })
    }

    fn uninstall_hooks(&self) -> Task<Result<()>> {
        let response =
            self.call::<serde_json::Value>("ClaudeSessions::uninstall_zed_hooks", json!({}));
        self.executor.spawn(async move {
            response.await?;
            Ok(())
        })
    }

    fn hooks_installed(&self) -> Task<Result<bool>> {
        self.call("ClaudeSessions::zed_hooks_installed", json!({}))
    }

    fn list_slash_commands(
        &self,
        project_root: Option<PathBuf>,
    ) -> Task<Result<Vec<SlashCommand>>> {
        self.list_slash_commands_for(project_root, None)
    }

    fn list_slash_commands_for(
        &self,
        project_root: Option<PathBuf>,
        session_id: Option<String>,
    ) -> Task<Result<Vec<SlashCommand>>> {
        let response = self.call::<ListSlashCommandsJson>(
            "ClaudeSessions::list_slash_commands",
            json!({
                "project_root": project_root.map(|root| root.to_string_lossy().into_owned()),
                "session_id": session_id,
            }),
        );
        self.executor.spawn(async move {
            Ok(response
                .await?
                .commands
                .into_iter()
                .map(slash_command_from_json)
                .collect())
        })
    }

    fn list_session_files(&self, directory: PathBuf, query: String) -> Task<Result<Vec<String>>> {
        self.list_session_files_in(String::new(), directory, query)
    }

    fn list_session_files_in(
        &self,
        session_id: String,
        directory: PathBuf,
        query: String,
    ) -> Task<Result<Vec<String>>> {
        let response = self.call::<ListSessionFilesJson>(
            "ClaudeSessions::list_files_under",
            json!({
                "session_id": session_id,
                "directory": directory.to_string_lossy(),
                "query": query,
            }),
        );
        self.executor
            .spawn(async move { Ok(response.await?.paths) })
    }

    fn write_session_file(&self, name: String, contents: Vec<u8>) -> Task<Result<String>> {
        let response = self.call::<WriteSessionFileJson>(
            "ClaudeSessions::write_pasted_file",
            json!({
                "name": name,
                "contents": encode_bytes(&contents),
            }),
        );
        self.executor.spawn(async move { Ok(response.await?.path) })
    }

    fn tail_subagent(
        &self,
        session_id: String,
        agent_id: String,
        workflow_run_id: Option<String>,
        state: TailState,
    ) -> Task<Result<TailProgress>> {
        self.tail(
            "ClaudeSessions::read_subagent_transcript_tail",
            json!({
                "session_id": session_id,
                "agent_id": agent_id,
                "workflow_run_id": workflow_run_id,
                "offset": state.offset,
                "pending": encode_bytes(&state.pending),
            }),
        )
    }

    fn tail_subagent_within(
        &self,
        session_id: String,
        agent_id: String,
        workflow_run_id: Option<String>,
        state: TailState,
        window: u64,
    ) -> Task<Result<TailProgress>> {
        self.tail(
            "ClaudeSessions::read_subagent_transcript_tail",
            json!({
                "session_id": session_id,
                "agent_id": agent_id,
                "workflow_run_id": workflow_run_id,
                "offset": state.offset,
                "pending": encode_bytes(&state.pending),
                "window": window,
            }),
        )
    }

    fn read_file(&self, path: PathBuf, max_bytes: u64) -> Task<Result<FileContents>> {
        self.read_claude_file(path, max_bytes, None)
    }

    fn read_attachment(
        &self,
        session_id: String,
        path: PathBuf,
        max_bytes: u64,
    ) -> Task<Result<FileContents>> {
        self.read_claude_file(path, max_bytes, Some(session_id))
    }

    fn channel_status(&self, claude_pid: u32) -> Task<Result<ChannelStatus>> {
        let response = self.call::<ChannelStatusJson>(
            "ClaudeSessions::channel_status",
            json!({ "claude_pid": claude_pid }),
        );
        self.executor.spawn(async move {
            let response = response.await?;
            Ok(ChannelStatus {
                live: response.live,
                heartbeat_at_ms: response.heartbeat_at_ms,
                server_pid: response.server_pid,
                features: response.features,
            })
        })
    }

    fn channel_send_message(&self, claude_pid: u32, content: String) -> Task<Result<String>> {
        self.outbox_file(
            "ClaudeSessions::channel_send_message",
            json!({ "claude_pid": claude_pid, "content": content }),
        )
    }

    fn channel_interrupt(&self, claude_pid: u32, reason: String) -> Task<Result<String>> {
        self.outbox_file(
            "ClaudeSessions::channel_interrupt",
            json!({ "claude_pid": claude_pid, "reason": reason }),
        )
    }

    fn channel_answer_permission(
        &self,
        claude_pid: u32,
        request_id: String,
        allow: bool,
    ) -> Task<Result<String>> {
        self.outbox_file(
            "ClaudeSessions::channel_answer_permission",
            json!({
                "claude_pid": claude_pid,
                "request_id": request_id,
                "allow": allow,
            }),
        )
    }

    fn tail_channel_inbox(&self, claude_pid: u32, state: TailState) -> Task<Result<TailProgress>> {
        self.tail(
            "ClaudeSessions::read_channel_inbox_tail",
            json!({
                "claude_pid": claude_pid,
                "offset": state.offset,
                "pending": encode_bytes(&state.pending),
                "window": TAIL_FIRST_READ_WINDOW_BYTES,
            }),
        )
    }

    // The browser has no filesystem of its own: every path a session names is the
    // server's, so a sent file is always fetched rather than opened by its local name.
    fn is_remote(&self) -> bool {
        true
    }

    fn read_keep_alive(&self, session_ids: Vec<String>) -> Task<Result<Vec<KeepAliveRecord>>> {
        let response = self.call::<ReadKeepAliveJson>(
            "ClaudeSessions::read_keep_alive",
            json!({ "session_ids": session_ids }),
        );
        self.executor.spawn(async move {
            Ok(response
                .await?
                .records
                .into_iter()
                .map(KeepAliveRecord::from)
                .collect())
        })
    }

    fn write_keep_alive(
        &self,
        session_id: String,
        expected_revision: u64,
        state_json: String,
    ) -> Task<Result<KeepAliveWrite>> {
        let response = self.call::<WriteKeepAliveJson>(
            "ClaudeSessions::write_keep_alive",
            json!({
                "session_id": session_id,
                "expected_revision": expected_revision,
                "state_json": state_json,
            }),
        );
        self.executor.spawn(async move {
            let response = response.await?;
            let record = response.record.map(KeepAliveRecord::from);
            if response.applied {
                Ok(KeepAliveWrite::Applied(
                    record.context("an applied keep-alive write named no record")?,
                ))
            } else {
                Ok(KeepAliveWrite::Conflict(record))
            }
        })
    }

    fn compact_session(&self, session_id: String) -> Task<Result<()>> {
        let response = self.call::<serde_json::Value>(
            "ClaudeSessions::compact_session",
            json!({ "session_id": session_id }),
        );
        self.executor.spawn(async move {
            response.await?;
            Ok(())
        })
    }
}

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
struct ListSessionsJson {
    sessions: Vec<ClaudeSessionJson>,
    home_directory: String,
    liveness_unavailable_reason: Option<String>,
}

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
struct ClaudeSessionJson {
    process_id: u32,
    session_id: String,
    working_directory: String,
    version: String,
    name: Option<String>,
    status: Option<String>,
    updated_at: Option<i64>,
    started_at: Option<i64>,
    tmux_target: Option<String>,
    transcript_path: Option<String>,
    context_tokens: u64,
    total_cost_usd: Option<f64>,
    bridge_session_id: Option<String>,
    last_answer_at_ms: Option<i64>,
    cache_ttl: u32,
}

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
struct KeepAliveRecordJson {
    session_id: String,
    revision: u64,
    state_json: String,
}

#[cfg(target_family = "wasm")]
impl From<KeepAliveRecordJson> for KeepAliveRecord {
    fn from(record: KeepAliveRecordJson) -> Self {
        Self {
            session_id: record.session_id,
            revision: record.revision,
            state_json: record.state_json,
        }
    }
}

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
struct ReadKeepAliveJson {
    records: Vec<KeepAliveRecordJson>,
}

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
struct WriteKeepAliveJson {
    applied: bool,
    record: Option<KeepAliveRecordJson>,
}

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
struct ListSubagentsJson {
    subagents: Vec<ClaudeSubagentJson>,
}

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
struct ClaudeSubagentJson {
    agent_id: String,
    workflow_run_id: Option<String>,
    agent_type: String,
    description: Option<String>,
    tool_use_id: Option<String>,
    spawn_depth: u32,
    model: Option<String>,
    workflow_phase: Option<String>,
    transcript_path: Option<String>,
    size: u64,
    workflow_agent_finished: Option<bool>,
    task_agent_finished: Option<bool>,
    request_shape: Option<String>,
}

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
struct TailProgressJson {
    path: Option<String>,
    start_offset: u64,
    offset: u64,
    pending: String,
    lines: Vec<String>,
    restarted: bool,
    /// Absent from a server that predates the windowed first read, which always sent the
    /// whole file.
    #[serde(default)]
    skipped_bytes: u64,
}

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
struct InstallHooksJson {
    outcome: String,
    backup_path: Option<String>,
}

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
struct ChannelStatusJson {
    live: bool,
    heartbeat_at_ms: Option<i64>,
    server_pid: Option<u32>,
    features: Vec<String>,
}

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
struct OutboxFileJson {
    outbox_file: String,
}

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
struct ListSlashCommandsJson {
    commands: Vec<SlashCommandJson>,
}

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
struct SlashCommandJson {
    name: String,
    description: Option<String>,
    argument_hint: Option<String>,
    scope: u32,
}

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
struct ListSessionFilesJson {
    paths: Vec<String>,
}

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
struct WriteSessionFileJson {
    path: String,
}

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
struct ReadFileJson {
    contents: String,
    truncated: bool,
}

#[cfg(target_family = "wasm")]
fn encode_bytes(bytes: &[u8]) -> String {
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    BASE64.encode(bytes)
}

#[cfg(target_family = "wasm")]
fn decode_bytes(encoded: &str) -> Result<Vec<u8>> {
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    BASE64.decode(encoded).context("decoding base64 bytes")
}

#[cfg(target_family = "wasm")]
fn session_listing_from_json(response: ListSessionsJson) -> SessionListing {
    session_listing_from_proto(proto::ListClaudeSessionsResponse {
        sessions: response
            .sessions
            .into_iter()
            .map(|session| proto::ClaudeSession {
                process_id: session.process_id,
                session_id: session.session_id,
                working_directory: session.working_directory,
                version: session.version,
                name: session.name,
                status: session.status,
                updated_at: session.updated_at,
                tmux_target: session.tmux_target,
                transcript_path: session.transcript_path,
                context_tokens: session.context_tokens,
                total_cost_usd: session.total_cost_usd,
                bridge_session_id: session.bridge_session_id,
                last_answer_at_ms: session.last_answer_at_ms,
                cache_ttl: session.cache_ttl,
                started_at: session.started_at,
            })
            .collect(),
        home_directory: response.home_directory,
        liveness_unavailable_reason: response.liveness_unavailable_reason,
    })
}

#[cfg(target_family = "wasm")]
fn subagent_summary_from_json(subagent: ClaudeSubagentJson) -> SubagentSummary {
    subagent_summary_from_proto(proto::ClaudeSubagent {
        agent_id: subagent.agent_id,
        workflow_run_id: subagent.workflow_run_id,
        agent_type: subagent.agent_type,
        description: subagent.description,
        tool_use_id: subagent.tool_use_id,
        spawn_depth: subagent.spawn_depth,
        model: subagent.model,
        workflow_phase: subagent.workflow_phase,
        transcript_path: subagent.transcript_path,
        size: subagent.size,
        workflow_agent_finished: subagent.workflow_agent_finished,
        task_agent_finished: subagent.task_agent_finished,
        request_shape: subagent.request_shape,
    })
}

#[cfg(target_family = "wasm")]
fn tail_progress_from_json(response: TailProgressJson) -> Result<TailProgress> {
    let skipped_bytes = response.skipped_bytes;
    let mut progress = tail_progress_from_proto(proto::TailClaudeTranscriptResponse {
        path: response.path,
        start_offset: response.start_offset,
        offset: response.offset,
        pending: decode_bytes(&response.pending)?,
        lines: response.lines,
        restarted: response.restarted,
    });
    progress.skipped_bytes = skipped_bytes;
    Ok(progress)
}

#[cfg(target_family = "wasm")]
fn slash_command_from_json(command: SlashCommandJson) -> SlashCommand {
    SlashCommand {
        name: command.name,
        description: command.description,
        argument_hint: command.argument_hint,
        scope: match command.scope {
            1 => SlashCommandScope::Project,
            2 => SlashCommandScope::User,
            _ => SlashCommandScope::Builtin,
        },
    }
}

/// Paths cross the wire as strings, the way the remote server already sends them back.
/// A path that is not UTF-8 belongs to the other machine's filesystem, and this side
/// only ever echoes back one the server named.
fn path_to_wire(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The only session kind the registry scan lets through, on either machine.
const INTERACTIVE_SESSION_KIND: &str = "interactive";

/// Rebuilds a scan's result from what the far end sent.
///
/// The home directory arrives as part of the listing rather than being read locally,
/// because the paths in the listing were written under the far end's home and nothing on
/// this side can name it.
fn session_listing_from_proto(response: proto::ListClaudeSessionsResponse) -> SessionListing {
    SessionListing {
        sessions: response
            .sessions
            .into_iter()
            .map(session_summary_from_proto)
            .collect(),
        home_directory: PathBuf::from(response.home_directory),
        // The panel draws this as a row of its own, so a reason with nothing in it would
        // be a blank row saying nothing at all.
        liveness_unavailable_reason: response
            .liveness_unavailable_reason
            .filter(|reason| !reason.trim().is_empty()),
    }
}

fn keep_alive_record_from_proto(record: proto::ClaudeKeepAliveRecord) -> KeepAliveRecord {
    KeepAliveRecord {
        session_id: record.session_id,
        revision: record.revision,
        state_json: record.state_json,
    }
}

/// Rebuilds a listed session from what the far end sent.
///
/// Two fields of [`RegisteredSession`] are deliberately absent from the wire message.
/// `process_start` and `kind` exist to decide whether a registration is live and
/// interactive, and that decision has already been made on the machine the process runs
/// on — this side has no pid table to check it against, so a second answer here could
/// only be a worse one. Sending them would widen what the protocol exposes to no
/// reader, so they are filled with what the scan on the far end has already proven:
/// the kind it filtered for, and no process start of our own to claim.
fn session_summary_from_proto(session: proto::ClaudeSession) -> SessionSummary {
    SessionSummary {
        session: RegisteredSession {
            process_id: session.process_id,
            session_id: session.session_id,
            working_directory: PathBuf::from(session.working_directory),
            process_start: String::new(),
            version: session.version,
            kind: INTERACTIVE_SESSION_KIND.to_string(),
            name: session.name,
            status: session.status,
            updated_at: session.updated_at,
            tmux_target: session.tmux_target,
            bridge_session_id: session.bridge_session_id,
            started_at: session.started_at,
        },
        transcript_path: session.transcript_path.map(PathBuf::from),
        // Zero context means the far end found no answer to read. An answer time with
        // no token count is still an answer, which keep-alive has to see.
        spend: (session.context_tokens > 0
            || session.total_cost_usd.is_some()
            || session.last_answer_at_ms.is_some())
        .then_some(TranscriptSpend {
            context_tokens: session.context_tokens,
            total_cost_usd: session.total_cost_usd,
            last_answer_at_ms: session.last_answer_at_ms,
            cache_ttl: cache_ttl_from_code(session.cache_ttl),
        }),
    }
}

fn cache_ttl_from_code(code: u32) -> CacheTtl {
    match code {
        1 => CacheTtl::FiveMinutes,
        2 => CacheTtl::OneHour,
        _ => CacheTtl::Unknown,
    }
}

fn agent_listing_from_proto(agent: proto::ClaudeAgent) -> AgentListing {
    AgentListing {
        id: agent.id,
        kind: agent.kind,
        state: agent.state,
        status: agent.status,
        waiting_for: agent.waiting_for,
        process_id: agent.pid,
        session_id: agent.session_id,
        name: agent.name,
        working_directory: agent.cwd.map(PathBuf::from),
        started_at: agent.started_at,
    }
}

/// Rebuilds one listed subagent from what the far end sent.
///
/// The meta arrives field by field rather than as the sidecar's JSON, because the far end
/// has already parsed it and a second parse here could only disagree with the machine
/// that holds the file.
fn subagent_summary_from_proto(subagent: proto::ClaudeSubagent) -> SubagentSummary {
    SubagentSummary {
        agent_id: subagent.agent_id,
        workflow_run_id: subagent.workflow_run_id,
        meta: SubagentMeta {
            agent_type: subagent.agent_type,
            description: subagent.description,
            tool_use_id: subagent.tool_use_id,
            spawn_depth: subagent.spawn_depth,
            model: subagent.model,
            workflow_phase: subagent.workflow_phase,
            request_shape: subagent.request_shape,
        },
        // An empty path stands in for one the far end did not name. Nothing on this side
        // ever opens this path: a subagent's transcript is only ever read through
        // [`SessionSource::tail_subagent`], which names the file by the three ids on the
        // machine that holds it. The field is a label the panel may show, so a missing
        // one costs a label rather than a read.
        transcript_path: subagent
            .transcript_path
            .map(PathBuf::from)
            .unwrap_or_default(),
        size: subagent.size,
        workflow_agent_finished: subagent.workflow_agent_finished,
        task_agent_finished: subagent.task_agent_finished,
    }
}

fn events_progress_from_proto(response: proto::TailClaudeEventsResponse) -> TailProgress {
    TailProgress {
        path: response.path.map(PathBuf::from),
        start_offset: response.start_offset,
        offset: response.offset,
        pending: response.pending,
        lines: response.lines,
        restarted: response.restarted,
        // The remote server's reads are unwindowed, and its protocol has no field to
        // say otherwise.
        skipped_bytes: 0,
    }
}

fn channel_inbox_progress_from_proto(
    response: proto::TailClaudeChannelInboxResponse,
) -> TailProgress {
    TailProgress {
        path: response.path.map(PathBuf::from),
        start_offset: response.start_offset,
        offset: response.offset,
        pending: response.pending,
        lines: response.lines,
        restarted: response.restarted,
        // The remote server's reads are unwindowed, and its protocol has no field to
        // say otherwise.
        skipped_bytes: 0,
    }
}

fn hook_install_outcome_from_proto(
    response: proto::InstallClaudeHooksResponse,
) -> HookInstallOutcome {
    if response.outcome == proto::ClaudeHookInstallOutcome::ScriptsRefreshed as i32 {
        HookInstallOutcome::ScriptsRefreshed
    } else if response.outcome == proto::ClaudeHookInstallOutcome::Installed as i32 {
        HookInstallOutcome::Installed {
            backup_path: response.backup_path.map(PathBuf::from),
        }
    } else {
        HookInstallOutcome::AlreadyCurrent
    }
}

fn tail_progress_from_proto(response: proto::TailClaudeTranscriptResponse) -> TailProgress {
    TailProgress {
        path: response.path.map(PathBuf::from),
        start_offset: response.start_offset,
        offset: response.offset,
        pending: response.pending,
        lines: response.lines,
        restarted: response.restarted,
        // The remote server's reads are unwindowed, and its protocol has no field to
        // say otherwise.
        skipped_bytes: 0,
    }
}

pub(crate) fn read_file_prefix(path: &Path, max_bytes: u64) -> Result<FileContents> {
    let file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;

    // Read one byte past the cap so that hitting it can be distinguished from a file
    // that happens to be exactly that long.
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;

    let truncated = bytes.len() as u64 > max_bytes;
    bytes.truncate(usize::try_from(max_bytes).unwrap_or(usize::MAX));

    Ok(FileContents { bytes, truncated })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire_session() -> proto::ClaudeSession {
        proto::ClaudeSession {
            process_id: 4321,
            session_id: "abc-123".to_string(),
            working_directory: "/home/user/project".to_string(),
            version: "2.1.267".to_string(),
            name: Some("alpha".to_string()),
            status: Some("busy".to_string()),
            updated_at: Some(1_759_000_000_000),
            tmux_target: Some("main:@3.%7".to_string()),
            transcript_path: Some("/home/user/.claude/projects/p/abc-123.jsonl".to_string()),
            context_tokens: 0,
            total_cost_usd: None,
            bridge_session_id: Some("bridge-abc".to_string()),
            started_at: None,
            last_answer_at_ms: None,
            cache_ttl: 0,
        }
    }

    #[test]
    fn a_listed_session_crosses_the_wire_field_for_field() {
        let summary = session_summary_from_proto(wire_session());

        assert_eq!(summary.session.process_id, 4321);
        assert_eq!(summary.session.session_id, "abc-123");
        assert_eq!(
            summary.session.working_directory,
            PathBuf::from("/home/user/project")
        );
        assert_eq!(summary.session.version, "2.1.267");
        assert_eq!(summary.session.name.as_deref(), Some("alpha"));
        assert_eq!(summary.session.status.as_deref(), Some("busy"));
        assert_eq!(summary.session.updated_at, Some(1_759_000_000_000));
        assert_eq!(summary.session.tmux_target.as_deref(), Some("main:@3.%7"));
        assert_eq!(
            summary.transcript_path,
            Some(PathBuf::from("/home/user/.claude/projects/p/abc-123.jsonl"))
        );

        // The three fields the wire message does not carry: the far end already
        // filtered on them, so this side claims nothing of its own about them.
        assert_eq!(
            summary.session.process_start, "",
            "no process start may be invented for a pid on another machine"
        );
        assert_eq!(
            summary.session.kind, "interactive",
            "the far end lists interactive sessions only"
        );
        assert_eq!(
            summary.session.bridge_session_id.as_deref(),
            Some("bridge-abc")
        );
    }

    #[test]
    fn the_optional_fields_of_a_listed_session_stay_absent() {
        let summary = session_summary_from_proto(proto::ClaudeSession {
            name: None,
            status: None,
            updated_at: None,
            tmux_target: None,
            transcript_path: None,
            bridge_session_id: None,
            ..wire_session()
        });

        assert_eq!(summary.session.name, None);
        assert_eq!(summary.session.status, None);
        assert_eq!(summary.session.updated_at, None);
        assert_eq!(
            summary.session.tmux_target, None,
            "a session with no pane must not come out looking like one Zed can type into"
        );
        assert_eq!(summary.transcript_path, None);
        assert_eq!(summary.session.bridge_session_id, None);
        // The fields that are always present still arrive.
        assert_eq!(summary.session.process_id, 4321);
        assert_eq!(summary.session.session_id, "abc-123");
    }

    #[test]
    fn a_listed_session_keeps_the_answer_time_and_cache_ttl() {
        let summary = session_summary_from_proto(proto::ClaudeSession {
            context_tokens: 0,
            total_cost_usd: None,
            last_answer_at_ms: Some(1_700_000_000_000),
            cache_ttl: 2,
            ..wire_session()
        });
        let spend = summary
            .spend
            .expect("an answer time is something to report");
        assert_eq!(spend.last_answer_at_ms, Some(1_700_000_000_000));
        assert_eq!(spend.cache_ttl, CacheTtl::OneHour);
        assert_eq!(spend.context_tokens, 0);

        let five_minutes = session_summary_from_proto(proto::ClaudeSession {
            context_tokens: 8,
            cache_ttl: 1,
            ..wire_session()
        });
        assert_eq!(
            five_minutes.spend.expect("context").cache_ttl,
            CacheTtl::FiveMinutes
        );

        let unknown = session_summary_from_proto(proto::ClaudeSession {
            context_tokens: 8,
            cache_ttl: 9,
            ..wire_session()
        });
        assert_eq!(unknown.spend.expect("context").cache_ttl, CacheTtl::Unknown);

        let ttl_alone = session_summary_from_proto(proto::ClaudeSession {
            context_tokens: 0,
            total_cost_usd: None,
            last_answer_at_ms: None,
            cache_ttl: 2,
            ..wire_session()
        });
        assert_eq!(
            ttl_alone.spend, None,
            "a ttl alone is not an answer the scan found"
        );
    }

    #[test]
    fn a_listing_carries_the_home_directory_of_the_machine_it_was_scanned_on() {
        let listing = session_listing_from_proto(proto::ListClaudeSessionsResponse {
            sessions: vec![wire_session()],
            home_directory: "/home/deploy".to_string(),
            liveness_unavailable_reason: None,
        });

        assert_eq!(
            listing.home_directory,
            PathBuf::from("/home/deploy"),
            "the far end's home directory must arrive as sent, not be replaced by this machine's"
        );
        assert_eq!(listing.sessions.len(), 1);
        assert_eq!(listing.sessions[0].session.session_id, "abc-123");
    }

    #[test]
    fn a_listing_from_a_server_that_names_no_home_directory_names_none() {
        let listing = session_listing_from_proto(proto::ListClaudeSessionsResponse {
            sessions: Vec::new(),
            home_directory: String::new(),
            liveness_unavailable_reason: None,
        });

        assert_eq!(
            listing.home_directory,
            PathBuf::new(),
            "an unset home directory must stay empty rather than become this machine's"
        );
        assert!(listing.sessions.is_empty());
    }

    #[test]
    fn a_remote_liveness_unavailable_reason_surfaces_from_the_proto() {
        let listing = session_listing_from_proto(proto::ListClaudeSessionsResponse {
            sessions: Vec::new(),
            home_directory: "C:\\Users\\remote".to_string(),
            liveness_unavailable_reason: Some(
                "Claude session liveness checks are unavailable on this host.".to_string(),
            ),
        });

        assert_eq!(
            listing.liveness_unavailable_reason.as_deref(),
            Some("Claude session liveness checks are unavailable on this host."),
        );
    }

    /// A note with nothing in it is not a note: the panel draws whatever arrives here as
    /// a muted row under the session list, so an empty reason would be an empty row.
    #[test]
    fn a_liveness_unavailable_reason_with_nothing_in_it_is_no_reason() {
        for sent in ["", "   ", "\n"] {
            let listing = session_listing_from_proto(proto::ListClaudeSessionsResponse {
                sessions: Vec::new(),
                home_directory: "C:\\Users\\remote".to_string(),
                liveness_unavailable_reason: Some(sent.to_string()),
            });

            assert_eq!(
                listing.liveness_unavailable_reason, None,
                "{sent:?} says nothing and must arrive as no reason at all, not as a blank note"
            );
        }
    }

    fn wire_subagent() -> proto::ClaudeSubagent {
        proto::ClaudeSubagent {
            agent_id: "af090e203ec41bc73".to_string(),
            workflow_run_id: Some("wf_b529a29d-562".to_string()),
            agent_type: "workflow-subagent".to_string(),
            description: Some("R2:send-atomicity".to_string()),
            tool_use_id: Some("toolu_01BEgVRSnksoz6YAyUWEEdeQ".to_string()),
            spawn_depth: 2,
            model: Some("opus".to_string()),
            workflow_phase: Some("Wave 5".to_string()),
            transcript_path: Some(
                "/home/user/.claude/projects/p/session/subagents/workflows/wf_b529a29d-562/agent-af090e203ec41bc73.jsonl"
                    .to_string(),
            ),
            size: 4096,
            workflow_agent_finished: Some(false),
            request_shape: Some("background".to_string()),
            task_agent_finished: Some(true),
        }
    }

    #[test]
    fn a_listed_subagent_crosses_the_wire_field_for_field() {
        let summary = subagent_summary_from_proto(wire_subagent());

        assert_eq!(summary.agent_id, "af090e203ec41bc73");
        assert_eq!(summary.workflow_run_id.as_deref(), Some("wf_b529a29d-562"));
        assert_eq!(summary.meta.agent_type, "workflow-subagent");
        assert_eq!(
            summary.meta.description.as_deref(),
            Some("R2:send-atomicity")
        );
        assert_eq!(
            summary.meta.tool_use_id.as_deref(),
            Some("toolu_01BEgVRSnksoz6YAyUWEEdeQ")
        );
        assert_eq!(summary.meta.spawn_depth, 2);
        assert_eq!(summary.meta.model.as_deref(), Some("opus"));
        assert_eq!(
            summary.meta.workflow_phase.as_deref(),
            Some("Wave 5"),
            "the phase is what pairs a workflow's agents with the wave that spawned them"
        );
        assert_eq!(
            summary.workflow_agent_finished,
            Some(false),
            "only the machine the run happened on reads its journal, so what it found has \
             to survive the crossing rather than be worked out again on this side"
        );
        assert_eq!(
            summary.transcript_path,
            PathBuf::from(
                "/home/user/.claude/projects/p/session/subagents/workflows/wf_b529a29d-562/agent-af090e203ec41bc73.jsonl"
            )
        );
        assert_eq!(summary.size, 4096);
    }

    /// An agent spawned by the `Task` tool belongs to no workflow run, and one whose
    /// transcript has not been flushed yet is still an agent to list.
    #[test]
    fn a_subagent_the_far_end_named_no_transcript_for_keeps_an_empty_path() {
        let summary = subagent_summary_from_proto(proto::ClaudeSubagent {
            workflow_run_id: None,
            description: None,
            tool_use_id: None,
            model: None,
            workflow_phase: None,
            transcript_path: None,
            size: 0,
            ..wire_subagent()
        });

        assert_eq!(
            summary.transcript_path,
            PathBuf::new(),
            "a path the far end did not name must stay empty rather than become a path \
             on this machine"
        );
        assert_eq!(
            summary.workflow_run_id, None,
            "an agent with no run id must not be looked for in a run's directory"
        );
        assert_eq!(summary.meta.description, None);
        assert_eq!(summary.meta.tool_use_id, None);
        assert_eq!(summary.meta.model, None);
        assert_eq!(summary.meta.workflow_phase, None);
        assert_eq!(summary.size, 0);
        // The fields that are always present still arrive.
        assert_eq!(summary.agent_id, "af090e203ec41bc73");
        assert_eq!(summary.meta.agent_type, "workflow-subagent");
        assert_eq!(summary.meta.spawn_depth, 2);
    }

    #[test]
    fn tail_progress_crosses_the_wire_field_for_field() {
        let progress = tail_progress_from_proto(proto::TailClaudeTranscriptResponse {
            path: Some("/home/user/.claude/projects/p/abc-123.jsonl".to_string()),
            start_offset: 1024,
            offset: 2048,
            pending: b"{\"type\":\"user\"".to_vec(),
            lines: vec!["{\"type\":\"assistant\"}".to_string()],
            restarted: true,
        });

        assert_eq!(
            progress.path,
            Some(PathBuf::from("/home/user/.claude/projects/p/abc-123.jsonl"))
        );
        assert_eq!(progress.start_offset, 1024);
        assert_eq!(progress.offset, 2048);
        assert_eq!(progress.pending, b"{\"type\":\"user\"".to_vec());
        assert_eq!(progress.lines, vec!["{\"type\":\"assistant\"}".to_string()]);
        assert!(progress.restarted);
    }

    #[test]
    fn a_tail_of_a_session_with_no_transcript_yet_reports_no_path() {
        let progress = tail_progress_from_proto(proto::TailClaudeTranscriptResponse {
            path: None,
            start_offset: 0,
            offset: 0,
            pending: Vec::new(),
            lines: Vec::new(),
            restarted: false,
        });

        assert_eq!(
            progress.path, None,
            "an absent path must stay absent rather than become an empty one"
        );
        assert_eq!(progress.start_offset, 0);
        assert_eq!(progress.offset, 0);
        assert!(progress.pending.is_empty());
        assert!(progress.lines.is_empty());
        assert!(!progress.restarted);
    }

    #[test]
    fn collect_subagents_by_session_includes_every_asked_for_id() {
        let listed = smol::block_on(collect_subagents_by_session(
            vec!["a".into(), "b".into()],
            |_session_id| async { Ok(Vec::new()) },
        ));

        assert_eq!(
            listed.len(),
            2,
            "every id asked for must appear, mapping to an empty list when that session spawned nothing"
        );
        assert!(
            listed
                .get("a")
                .is_some_and(|subagents| subagents.is_empty())
        );
        assert!(
            listed
                .get("b")
                .is_some_and(|subagents| subagents.is_empty())
        );
    }

    #[test]
    fn collect_subagents_by_session_keeps_a_session_whose_listing_failed() {
        let listed = smol::block_on(collect_subagents_by_session(
            vec!["ok".into(), "fail".into()],
            |session_id| async move {
                if session_id == "fail" {
                    anyhow::bail!("unreachable host");
                }
                Ok(Vec::new())
            },
        ));

        assert!(
            listed
                .get("fail")
                .is_some_and(|subagents| subagents.is_empty()),
            "one session the far end cannot list must not hide every other session's agents"
        );
        assert!(listed.contains_key("ok"));
    }

    #[test]
    fn awaiting_a_oneshot_whose_sender_is_never_polled_stays_pending() {
        use std::future::Future as _;
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

        fn noop_waker() -> Waker {
            const VTABLE: RawWakerVTable =
                RawWakerVTable::new(|ptr| RawWaker::new(ptr, &VTABLE), |_| {}, |_| {}, |_| {});
            // SAFETY: the vtable is a no-op; the future is only polled to observe Pending.
            unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
        }

        let ready = std::rc::Rc::new(std::cell::Cell::new(false));
        struct WaitFlag(std::rc::Rc<std::cell::Cell<bool>>);
        impl Future for WaitFlag {
            type Output = ();
            fn poll(self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
                if self.0.get() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }
        }

        let mut parent = Box::pin(WaitFlag(ready.clone()));
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);
        for _ in 0..32 {
            assert_eq!(
                parent.as_mut().poll(&mut context),
                Poll::Pending,
                "a sibling that is never polled cannot make the parent Ready — the hang class of nested spawn+await"
            );
        }
        drop(ready);
    }
}
