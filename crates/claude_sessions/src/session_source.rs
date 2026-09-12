//! Where the panel's view of the Claude Code sessions comes from.
//!
//! Everything the panel needs from the machine the sessions run on goes through this
//! trait: listing the sessions, following a transcript, reading a persisted output file
//! back, and typing into a session's pane. A remote project's sessions live on the far
//! end of a connection, and a second implementation of these four methods is the whole
//! difference between reading them and reading this machine's.
//!
//! Every method hands back a [`Task`] and does its IO on the background executor, so a
//! `ps` invocation or a multi-megabyte read cannot stall the frame the panel is drawing.

use std::{
    fs,
    io::Read as _,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use gpui::{BackgroundExecutor, Task};
use rpc::{AnyProtoClient, proto};

use crate::session_registry::{
    self, PaneKey, PendingQuestion, Question, QuestionOption, RegisteredSession, SessionSummary,
    SlashCommand, SlashCommandScope, SubagentMeta, SubagentSummary, TailProgress, TailState,
    TranscriptSpend, read_subagent_transcript_tail, read_transcript_tail,
};

/// What the machine a session runs on can say about questions.
///
/// `hook_installed` is carried beside the question because their meanings differ: a
/// machine that records nothing has no question to report whether or not one is waiting,
/// and a reader told only `None` would draw a session that is blocked on its user as one
/// with nothing to answer.
pub struct QuestionState {
    pub question: Option<PendingQuestion>,
    pub hook_installed: bool,
    /// What the session is saying right now, which the transcript will not hold until the
    /// turn it belongs to is over.
    pub live_message: Option<String>,
}

/// What the user asked to send to a session. `Escape` carries no text because it is an
/// interrupt, not a message.
pub enum SessionInput {
    Text(String),
    Escape,
    /// Answers a prompt the CLI has drawn in the pane. See [`PaneKey`].
    Key(PaneKey),
}

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
}

pub trait SessionSource: Send + Sync + 'static {
    fn list_sessions(&self, project_root: Option<PathBuf>) -> Task<Result<SessionListing>>;

    fn tail_transcript(&self, session_id: String, state: TailState) -> Task<Result<TailProgress>>;

    /// Every subagent conversation the session has spawned.
    fn list_subagents(&self, session_id: String) -> Task<Result<Vec<SubagentSummary>>>;

    /// The question the session is waiting on, and whether the machine it runs on records
    /// questions at all. A question that is waiting is written nowhere the conversation
    /// can be read from, so a machine without the hook has nothing to answer with and the
    /// reader is told so rather than shown an empty panel.
    fn pending_question(&self, session_id: String) -> Task<Result<QuestionState>>;

    /// Installs the hook that records waiting questions on the machine the sessions run
    /// on, reporting where the settings that were there were copied to. Installing twice
    /// changes nothing.
    fn install_question_hook(&self) -> Task<Result<Option<PathBuf>>>;

    /// The slash commands a session in this project answers to.
    fn list_slash_commands(&self, project_root: Option<PathBuf>)
    -> Task<Result<Vec<SlashCommand>>>;

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

    fn send_input(&self, pane_target: String, input: SessionInput) -> Task<Result<()>>;

    /// The visible contents of the session's tmux pane, which is where Claude Code draws
    /// what it never writes to the transcript.
    fn capture_pane(&self, pane_target: String) -> Task<Result<String>>;
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

    fn pending_question(&self, session_id: String) -> Task<Result<QuestionState>> {
        let home_directory = self.home_directory.clone();
        self.executor.spawn(async move {
            Ok(QuestionState {
                question: session_registry::read_pending_question(&home_directory, &session_id)?,
                hook_installed: session_registry::question_hook_is_installed(&home_directory),
                live_message: session_registry::read_live_message(&home_directory, &session_id)?,
            })
        })
    }

    fn install_question_hook(&self) -> Task<Result<Option<PathBuf>>> {
        let home_directory = self.home_directory.clone();
        self.executor
            .spawn(async move { session_registry::install_question_hook(&home_directory) })
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

    fn send_input(&self, pane_target: String, input: SessionInput) -> Task<Result<()>> {
        self.executor.spawn(async move {
            match input {
                SessionInput::Text(text) => session_registry::send_text(&pane_target, &text).await,
                SessionInput::Escape => session_registry::send_escape(&pane_target).await,
                SessionInput::Key(key) => session_registry::send_key(&pane_target, key).await,
            }
        })
    }

    fn capture_pane(&self, pane_target: String) -> Task<Result<String>> {
        self.executor
            .spawn(async move { session_registry::capture_pane(&pane_target).await })
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

    fn pending_question(&self, session_id: String) -> Task<Result<QuestionState>> {
        let request = self.client.request(proto::GetClaudePendingQuestion {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            session_id,
        });

        self.executor.spawn(async move {
            let response = request.await?;
            Ok(QuestionState {
                question: pending_question_from_proto(&response),
                hook_installed: response.hook_installed,
                live_message: response.live_message.clone(),
            })
        })
    }

    fn install_question_hook(&self) -> Task<Result<Option<PathBuf>>> {
        let request = self.client.request(proto::InstallClaudeQuestionHook {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
        });

        self.executor
            .spawn(async move { Ok(request.await?.backup_path.map(PathBuf::from)) })
    }

    fn list_slash_commands(
        &self,
        project_root: Option<PathBuf>,
    ) -> Task<Result<Vec<SlashCommand>>> {
        let request = self.client.request(proto::ListClaudeSlashCommands {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            project_root: project_root.map(|root| root.to_string_lossy().into_owned()),
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

    fn read_file(&self, path: PathBuf, max_bytes: u64) -> Task<Result<FileContents>> {
        let request = self.client.request(proto::ReadClaudeFile {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            path: path_to_wire(&path),
            max_bytes,
        });

        self.executor.spawn(async move {
            let response = request.await?;
            Ok(FileContents {
                bytes: response.contents,
                truncated: response.truncated,
            })
        })
    }

    fn capture_pane(&self, pane_target: String) -> Task<Result<String>> {
        let request = self.client.request(proto::CaptureClaudePane {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            pane_target,
        });

        self.executor
            .spawn(async move { Ok(request.await?.contents) })
    }

    fn send_input(&self, pane_target: String, input: SessionInput) -> Task<Result<()>> {
        let request = self.client.request(proto::SendClaudeInput {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            pane_target,
            input: Some(match input {
                SessionInput::Text(text) => proto::send_claude_input::Input::Text(text),
                // The variant is the whole message; the boolean it carries only exists
                // because a protobuf `oneof` arm must have a type.
                SessionInput::Escape => proto::send_claude_input::Input::Escape(true),
                SessionInput::Key(key) => {
                    proto::send_claude_input::Input::Key(key.tmux_name().to_string())
                }
            }),
        });

        self.executor.spawn(async move {
            request.await?;
            Ok(())
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
    }
}

/// Rebuilds a listed session from what the far end sent.
///
/// Three fields of [`RegisteredSession`] are deliberately absent from the wire message.
/// `process_start` and `kind` exist to decide whether a registration is live and
/// interactive, and that decision has already been made on the machine the process runs
/// on — this side has no pid table to check it against, so a second answer here could
/// only be a worse one. `bridge_session_id` names a resource on that machine that
/// nothing here can reach. Sending them would widen what the protocol exposes to no
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
            bridge_session_id: None,
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
    }
}

/// The question a response carries, or `None` when it carries none.
///
/// A response naming no call is not a question this side can draw: whether a question is
/// still waiting is answered by looking for the tool result that answers its id, and
/// without one there is nothing to look for. A question with no options left is dropped
/// for the same reason the far end drops it — there would be nothing to pick.
fn pending_question_from_proto(
    response: &proto::GetClaudePendingQuestionResponse,
) -> Option<PendingQuestion> {
    let tool_use_id = response
        .tool_use_id
        .as_deref()
        .filter(|id| !id.is_empty())?
        .to_string();

    let questions: Vec<Question> = response
        .questions
        .iter()
        .filter(|question| !question.options.is_empty())
        .map(|question| Question {
            header: question.header.clone(),
            question: question.question.clone(),
            options: question
                .options
                .iter()
                .map(|option| QuestionOption {
                    label: option.label.clone(),
                    description: option.description.clone(),
                })
                .collect(),
            multi_select: question.multi_select,
        })
        .collect();

    (!questions.is_empty()).then_some(PendingQuestion {
        tool_use_id,
        questions,
    })
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
            working_directory: "/home/andy/project".to_string(),
            version: "2.1.267".to_string(),
            name: Some("alpha".to_string()),
            status: Some("busy".to_string()),
            updated_at: Some(1_759_000_000_000),
            tmux_target: Some("main:@3.%7".to_string()),
            transcript_path: Some("/home/andy/.claude/projects/p/abc-123.jsonl".to_string()),
            context_tokens: 0,
            total_cost_usd: None,
        }
    }

    #[test]
    fn a_listed_session_crosses_the_wire_field_for_field() {
        let summary = session_summary_from_proto(wire_session());

        assert_eq!(summary.session.process_id, 4321);
        assert_eq!(summary.session.session_id, "abc-123");
        assert_eq!(
            summary.session.working_directory,
            PathBuf::from("/home/andy/project")
        );
        assert_eq!(summary.session.version, "2.1.267");
        assert_eq!(summary.session.name.as_deref(), Some("alpha"));
        assert_eq!(summary.session.status.as_deref(), Some("busy"));
        assert_eq!(summary.session.updated_at, Some(1_759_000_000_000));
        assert_eq!(summary.session.tmux_target.as_deref(), Some("main:@3.%7"));
        assert_eq!(
            summary.transcript_path,
            Some(PathBuf::from("/home/andy/.claude/projects/p/abc-123.jsonl"))
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
        assert_eq!(summary.session.bridge_session_id, None);
    }

    #[test]
    fn the_optional_fields_of_a_listed_session_stay_absent() {
        let summary = session_summary_from_proto(proto::ClaudeSession {
            name: None,
            status: None,
            updated_at: None,
            tmux_target: None,
            transcript_path: None,
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
        // The fields that are always present still arrive.
        assert_eq!(summary.session.process_id, 4321);
        assert_eq!(summary.session.session_id, "abc-123");
    }

    #[test]
    fn a_listing_carries_the_home_directory_of_the_machine_it_was_scanned_on() {
        let listing = session_listing_from_proto(proto::ListClaudeSessionsResponse {
            sessions: vec![wire_session()],
            home_directory: "/home/deploy".to_string(),
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
        });

        assert_eq!(
            listing.home_directory,
            PathBuf::new(),
            "an unset home directory must stay empty rather than become this machine's"
        );
        assert!(listing.sessions.is_empty());
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
                "/home/andy/.claude/projects/p/session/subagents/workflows/wf_b529a29d-562/agent-af090e203ec41bc73.jsonl"
                    .to_string(),
            ),
            size: 4096,
            workflow_agent_finished: Some(false),
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
                "/home/andy/.claude/projects/p/session/subagents/workflows/wf_b529a29d-562/agent-af090e203ec41bc73.jsonl"
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
            path: Some("/home/andy/.claude/projects/p/abc-123.jsonl".to_string()),
            start_offset: 1024,
            offset: 2048,
            pending: b"{\"type\":\"user\"".to_vec(),
            lines: vec!["{\"type\":\"assistant\"}".to_string()],
            restarted: true,
        });

        assert_eq!(
            progress.path,
            Some(PathBuf::from("/home/andy/.claude/projects/p/abc-123.jsonl"))
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
