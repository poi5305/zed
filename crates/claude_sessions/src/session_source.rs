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

use crate::session_registry::{
    self, AgentListing, ChannelStatus, HookInstallOutcome, RegisteredSession, SessionSummary,
    SlashCommand, SlashCommandScope, SubagentMeta, SubagentSummary, TailProgress, TailState,
    TranscriptSpend, channel_answer_permission, channel_interrupt, channel_send_message,
    channel_status, now_millis, read_channel_inbox_tail, read_events_tail, read_session_status,
    read_subagent_transcript_tail, read_transcript_tail,
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
        self.executor
            .spawn(async move { read_events_tail(&home_directory, &session_id, state) })
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
        self.executor
            .spawn(async move { read_channel_inbox_tail(&home_directory, claude_pid, state) })
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
        },
        transcript_path: session.transcript_path.map(PathBuf::from),
        // Zero context means the far end found no answer to read, which is the same
        // thing as having nothing to report.
        spend: (session.context_tokens > 0 || session.total_cost_usd.is_some()).then_some(
            TranscriptSpend {
                context_tokens: session.context_tokens,
                total_cost_usd: session.total_cost_usd,
            },
        ),
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
}
