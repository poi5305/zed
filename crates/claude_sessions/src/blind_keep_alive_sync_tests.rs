//! Blind tests for sharing keep-alive state across every Zed connected to one host.
//!
//! Written from the specification only: each `TestAppContext` stands for one Zed, and the
//! sources of both talk to one in-memory host that applies the compare-and-swap rule.

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

use anyhow::{Result, anyhow};
use collections::HashMap;
use gpui::{AppContext as _, Entity, Task, TestAppContext};

use crate::{
    ClaudeSessionStore, FileContents, KeepAliveState, SessionListing, SessionSource,
    keep_alive_registry,
    session_registry::{
        CacheTtl, ChannelStatus, HEARTBEAT_CUTOFF_MILLIS, HookInstallOutcome, KeepAliveRecord,
        KeepAliveWrite, SessionSummary, SlashCommand, SubagentSummary, TailProgress, TailState,
        TranscriptSpend, install_zed_hooks, normalize_whitespace, now_millis,
        read_channel_inbox_tail, read_events_tail, read_registrations, read_session_status,
        read_transcript_tail, visible_sessions, zed_hooks_installed,
    },
};

// Mirrors of the store's private intervals, taken from the specification.
const REGISTRY_POLL_INTERVAL: Duration = Duration::from_millis(1000);
const KEEP_ALIVE_POLL_INTERVAL: Duration = Duration::from_secs(30);
const KEEP_ALIVE_SYNC_INTERVAL: Duration = Duration::from_secs(10);

const SESSION_ID: &str = "blind-keep-alive-sync-session";
const SESSION_PID: u32 = 4171;
const PROCESS_START: &str = "Thu Sep 10 02:27:23 2026";
const MINUTE_MS: i64 = 60_000;

fn temporary_home(label: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let home_directory = std::env::temp_dir().join(format!(
        "claude-sessions-blind-keep-alive-{label}-{}-{unique}",
        std::process::id()
    ));
    let registry_directory = home_directory.join(".claude").join("sessions");
    std::fs::create_dir_all(&registry_directory).expect("creating the registry directory");
    let registration = format!(
        r#"{{"pid":{SESSION_PID},"sessionId":"{SESSION_ID}","cwd":"/tmp",
"procStart":"{PROCESS_START}","version":"2.1.267","kind":"interactive",
"messagingSocketPath":"/tmp/cc-socks/{SESSION_PID}.sock","name":"live"}}"#
    );
    std::fs::write(
        registry_directory.join(format!("{SESSION_PID}.json")),
        registration,
    )
    .expect("writing the registration");
    home_directory
}

/// The keep-alive records one host holds, shared by the sources of every Zed under test.
#[derive(Default)]
struct FakeHost {
    records: Mutex<HashMap<String, KeepAliveRecord>>,
    /// Every write attempt: (session id, expected revision, whether it was applied).
    writes: Mutex<Vec<(String, u64, bool)>>,
    /// When set, another Zed always writes first: the revision moves up by one just
    /// before every write is compared.
    always_beaten_to_it: AtomicBool,
}

impl FakeHost {
    fn read(&self, session_ids: Vec<String>) -> Vec<KeepAliveRecord> {
        let records = self.records.lock().expect("locking the host records");
        session_ids
            .iter()
            .filter_map(|session_id| records.get(session_id).cloned())
            .collect()
    }

    fn write(
        &self,
        session_id: String,
        expected_revision: u64,
        state_json: String,
    ) -> KeepAliveWrite {
        let mut records = self.records.lock().expect("locking the host records");
        if self.always_beaten_to_it.load(Ordering::SeqCst) {
            if let Some(record) = records.get_mut(&session_id) {
                record.revision += 1;
            }
        }
        let current = records.get(&session_id).cloned();
        let current_revision = current.as_ref().map_or(0, |record| record.revision);
        let applied = current_revision == expected_revision;
        self.writes.lock().expect("locking the write log").push((
            session_id.clone(),
            expected_revision,
            applied,
        ));
        if !applied {
            return KeepAliveWrite::Conflict(current);
        }
        let record = KeepAliveRecord {
            session_id: session_id.clone(),
            revision: current_revision + 1,
            state_json,
        };
        records.insert(session_id, record.clone());
        KeepAliveWrite::Applied(record)
    }

    fn put(&self, revision: u64, state: &KeepAliveState) {
        self.records
            .lock()
            .expect("locking the host records")
            .insert(
                SESSION_ID.to_string(),
                KeepAliveRecord {
                    session_id: SESSION_ID.to_string(),
                    revision,
                    state_json: serde_json::to_string(state).expect("serializing the state"),
                },
            );
    }

    fn state(&self) -> Option<(u64, KeepAliveState)> {
        let records = self.records.lock().expect("locking the host records");
        records.get(SESSION_ID).map(|record| {
            (
                record.revision,
                serde_json::from_str(&record.state_json).expect("the host holds a valid state"),
            )
        })
    }

    fn writes(&self) -> Vec<(String, u64, bool)> {
        self.writes.lock().expect("locking the write log").clone()
    }
}

/// What one machine's sessions look like: a single live session whose newest answer the
/// test moves, plus counters for the keep-alive side effects.
struct Machine {
    home_directory: PathBuf,
    last_answer_at_ms: Mutex<Option<i64>>,
    sends: AtomicU32,
    compacts: AtomicU32,
}

impl Machine {
    fn new(home_directory: PathBuf, last_answer_at_ms: i64) -> Arc<Self> {
        Arc::new(Self {
            home_directory,
            last_answer_at_ms: Mutex::new(Some(last_answer_at_ms)),
            sends: AtomicU32::new(0),
            compacts: AtomicU32::new(0),
        })
    }

    fn set_last_answer(&self, last_answer_at_ms: i64) {
        *self
            .last_answer_at_ms
            .lock()
            .expect("locking the answer time") = Some(last_answer_at_ms);
    }

    fn sends(&self) -> u32 {
        self.sends.load(Ordering::SeqCst)
    }

    fn compacts(&self) -> u32 {
        self.compacts.load(Ordering::SeqCst)
    }

    fn list_sessions(&self, project_root: Option<PathBuf>) -> Task<Result<SessionListing>> {
        let registry_directory = self.home_directory.join(".claude").join("sessions");
        let process_start_of_pid = |process_id: u32| {
            (process_id == SESSION_PID).then(|| normalize_whitespace(PROCESS_START))
        };
        let spend = TranscriptSpend {
            context_tokens: 1_000,
            total_cost_usd: None,
            last_answer_at_ms: *self
                .last_answer_at_ms
                .lock()
                .expect("locking the answer time"),
            cache_ttl: CacheTtl::OneHour,
        };
        Task::ready(
            read_registrations(&registry_directory).map(|registrations| SessionListing {
                sessions: visible_sessions(
                    registrations,
                    project_root.as_deref(),
                    now_millis(),
                    HEARTBEAT_CUTOFF_MILLIS,
                    &process_start_of_pid,
                )
                .into_iter()
                .map(|session| SessionSummary {
                    transcript_path: None,
                    session,
                    spend: Some(spend),
                })
                .collect(),
                home_directory: self.home_directory.clone(),
                liveness_unavailable_reason: None,
            }),
        )
    }
}

/// The source methods that do not differ between a host that shares keep-alive and one
/// that does not; each source type holds its machine in a `machine` field.
macro_rules! delegate_to_machine {
    () => {
        fn list_sessions(&self, project_root: Option<PathBuf>) -> Task<Result<SessionListing>> {
            self.machine.list_sessions(project_root)
        }

        fn tail_transcript(
            &self,
            session_id: String,
            state: TailState,
        ) -> Task<Result<TailProgress>> {
            Task::ready(read_transcript_tail(
                &self.machine.home_directory,
                &session_id,
                state,
            ))
        }

        fn list_subagents(&self, _session_id: String) -> Task<Result<Vec<SubagentSummary>>> {
            Task::ready(Ok(Vec::new()))
        }

        fn list_subagents_for_sessions(
            &self,
            session_ids: Vec<String>,
        ) -> Task<Result<HashMap<String, Vec<SubagentSummary>>>> {
            Task::ready(Ok(session_ids
                .into_iter()
                .map(|session_id| (session_id, Vec::new()))
                .collect()))
        }

        fn tail_events(&self, session_id: String, state: TailState) -> Task<Result<TailProgress>> {
            Task::ready(read_events_tail(
                &self.machine.home_directory,
                &session_id,
                state,
            ))
        }

        fn read_status(&self, session_id: String) -> Task<Result<Option<String>>> {
            Task::ready(read_session_status(
                &self.machine.home_directory,
                &session_id,
            ))
        }

        fn install_hooks(&self) -> Task<Result<HookInstallOutcome>> {
            Task::ready(install_zed_hooks(&self.machine.home_directory))
        }

        fn hooks_installed(&self) -> Task<Result<bool>> {
            Task::ready(Ok(zed_hooks_installed(&self.machine.home_directory)))
        }

        fn list_slash_commands(
            &self,
            _project_root: Option<PathBuf>,
        ) -> Task<Result<Vec<SlashCommand>>> {
            Task::ready(Ok(Vec::new()))
        }

        fn list_session_files(
            &self,
            _directory: PathBuf,
            _query: String,
        ) -> Task<Result<Vec<String>>> {
            Task::ready(Ok(Vec::new()))
        }

        fn write_session_file(&self, _name: String, _contents: Vec<u8>) -> Task<Result<String>> {
            Task::ready(Err(anyhow!("nothing is written in these tests")))
        }

        fn tail_subagent(
            &self,
            _session_id: String,
            _agent_id: String,
            _workflow_run_id: Option<String>,
            _state: TailState,
        ) -> Task<Result<TailProgress>> {
            Task::ready(Err(anyhow!("no subagents in these tests")))
        }

        fn read_file(&self, _path: PathBuf, _max_bytes: u64) -> Task<Result<FileContents>> {
            Task::ready(Err(anyhow!("no files are read in these tests")))
        }

        fn read_attachment(
            &self,
            _session_id: String,
            _path: PathBuf,
            _max_bytes: u64,
        ) -> Task<Result<FileContents>> {
            Task::ready(Err(anyhow!("no attachments in these tests")))
        }

        fn channel_status(&self, _claude_pid: u32) -> Task<Result<ChannelStatus>> {
            Task::ready(Ok(ChannelStatus {
                live: true,
                heartbeat_at_ms: Some(now_millis()),
                server_pid: None,
                features: Vec::new(),
            }))
        }

        fn channel_send_message(&self, _claude_pid: u32, _content: String) -> Task<Result<String>> {
            self.machine.sends.fetch_add(1, Ordering::SeqCst);
            Task::ready(Ok("sent".to_string()))
        }

        fn channel_interrupt(&self, _claude_pid: u32, _reason: String) -> Task<Result<String>> {
            Task::ready(Err(anyhow!("no interrupts in these tests")))
        }

        fn channel_answer_permission(
            &self,
            _claude_pid: u32,
            _request_id: String,
            _allow: bool,
        ) -> Task<Result<String>> {
            Task::ready(Err(anyhow!("no permissions in these tests")))
        }

        fn tail_channel_inbox(
            &self,
            claude_pid: u32,
            state: TailState,
        ) -> Task<Result<TailProgress>> {
            Task::ready(read_channel_inbox_tail(
                &self.machine.home_directory,
                claude_pid,
                state,
            ))
        }
    };
}

/// A source on a host that shares keep-alive records.
struct SharedSource {
    machine: Arc<Machine>,
    host: Arc<FakeHost>,
}

impl SessionSource for SharedSource {
    delegate_to_machine!();

    fn read_keep_alive(&self, session_ids: Vec<String>) -> Task<Result<Vec<KeepAliveRecord>>> {
        Task::ready(Ok(self.host.read(session_ids)))
    }

    fn write_keep_alive(
        &self,
        session_id: String,
        expected_revision: u64,
        state_json: String,
    ) -> Task<Result<KeepAliveWrite>> {
        Task::ready(Ok(self.host.write(
            session_id,
            expected_revision,
            state_json,
        )))
    }

    fn compact_session(&self, _session_id: String) -> Task<Result<()>> {
        self.machine.compacts.fetch_add(1, Ordering::SeqCst);
        Task::ready(Ok(()))
    }
}

/// A source on an older host: the keep-alive methods keep the trait's default errors.
struct UnsharedSource {
    machine: Arc<Machine>,
}

impl SessionSource for UnsharedSource {
    delegate_to_machine!();
}

/// One Zed: settings, a store on `source`, and its first scan applied.
fn start_zed(
    source: Arc<dyn SessionSource>,
    cx: &mut TestAppContext,
) -> Entity<ClaudeSessionStore> {
    cx.update(|cx| {
        let settings_store = settings::SettingsStore::test(cx);
        cx.set_global(settings_store);
    });
    let store = cx.new(|cx| ClaudeSessionStore::new(source, None, cx));
    cx.executor().advance_clock(REGISTRY_POLL_INTERVAL);
    cx.run_until_parked();
    store
}

fn state(cx: &mut TestAppContext) -> KeepAliveState {
    cx.update(|cx| keep_alive_registry(cx).read(cx).state(SESSION_ID))
}

fn error(cx: &mut TestAppContext) -> Option<String> {
    cx.update(|cx| {
        keep_alive_registry(cx)
            .read(cx)
            .error(SESSION_ID)
            .map(|error| error.to_string())
    })
}

fn toggle_shared(cx: &mut TestAppContext) {
    cx.update(|cx| {
        keep_alive_registry(cx).update(cx, |registry, cx| registry.toggle_shared(SESSION_ID, cx));
    });
}

fn advance(cx: &mut TestAppContext, duration: Duration) {
    cx.executor().advance_clock(duration);
    cx.run_until_parked();
}

fn advance_both(cx_a: &mut TestAppContext, cx_b: &mut TestAppContext, duration: Duration) {
    cx_a.executor().advance_clock(duration);
    cx_a.run_until_parked();
    cx_b.run_until_parked();
}

/// Spec items 1–3: a mode turned on in one Zed reaches the other through the host, and
/// turning it off there reaches the first.
#[gpui::test]
async fn a_mode_changed_in_one_zed_is_followed_by_the_other(
    cx_a: &mut TestAppContext,
    cx_b: &mut TestAppContext,
) {
    let host = Arc::new(FakeHost::default());
    let home_directory = temporary_home("follow");
    // Not due, so no ping changes the state while the modes travel.
    let machine_a = Machine::new(home_directory.clone(), now_millis() - 10 * MINUTE_MS);
    let machine_b = Machine::new(home_directory, now_millis() - 10 * MINUTE_MS);
    let _store_a = start_zed(
        Arc::new(SharedSource {
            machine: machine_a,
            host: host.clone(),
        }),
        cx_a,
    );
    let _store_b = start_zed(
        Arc::new(SharedSource {
            machine: machine_b,
            host: host.clone(),
        }),
        cx_b,
    );

    toggle_shared(cx_a);
    cx_a.run_until_parked();
    cx_b.run_until_parked();
    let on_host = host.state();
    assert!(
        on_host.as_ref().is_some_and(|(_, state)| state.enabled),
        "toggle_shared in A must write an enabled record to the host; host holds {on_host:?}, \
         expected enabled == true"
    );

    advance_both(
        cx_a,
        cx_b,
        KEEP_ALIVE_SYNC_INTERVAL + Duration::from_secs(1),
    );
    let state_b = state(cx_b);
    assert!(
        state_b.enabled && !state_b.compact,
        "after one sync B must follow A into warm; got enabled={} compact={} ({state_b:?}), \
         expected enabled=true compact=false",
        state_b.enabled,
        state_b.compact
    );

    toggle_shared(cx_b);
    toggle_shared(cx_b);
    cx_a.run_until_parked();
    cx_b.run_until_parked();
    let state_b = state(cx_b);
    assert!(
        !state_b.enabled && !state_b.compact,
        "two toggles in B must go warm -> warm+compact -> off; got enabled={} compact={}, \
         expected enabled=false compact=false",
        state_b.enabled,
        state_b.compact
    );
    let on_host = host.state();
    assert!(
        on_host.as_ref().is_some_and(|(_, state)| !state.enabled),
        "B's off must reach the host; host holds {on_host:?}, expected enabled == false"
    );

    advance_both(
        cx_a,
        cx_b,
        KEEP_ALIVE_SYNC_INTERVAL + Duration::from_secs(1),
    );
    let state_a = state(cx_a);
    assert!(
        !state_a.enabled,
        "after one sync A must follow B to off; got enabled={} ({state_a:?}), expected false",
        state_a.enabled
    );
    assert_eq!(error(cx_a), None, "A must report no keep-alive error");
    assert_eq!(error(cx_b), None, "B must report no keep-alive error");
}

/// Spec item 4: when both Zeds have keep-alive on and a ping is due, only the one whose
/// claim the host applied sends it.
#[gpui::test]
async fn two_zeds_with_keep_alive_on_send_one_ping(
    cx_a: &mut TestAppContext,
    cx_b: &mut TestAppContext,
) {
    let host = Arc::new(FakeHost::default());
    let home_directory = temporary_home("one-ping");
    let answer_at = now_millis() - 57 * MINUTE_MS;
    let machine_a = Machine::new(home_directory.clone(), answer_at);
    let machine_b = Machine::new(home_directory, answer_at);
    let _store_a = start_zed(
        Arc::new(SharedSource {
            machine: machine_a.clone(),
            host: host.clone(),
        }),
        cx_a,
    );
    let _store_b = start_zed(
        Arc::new(SharedSource {
            machine: machine_b.clone(),
            host: host.clone(),
        }),
        cx_b,
    );

    toggle_shared(cx_a);
    cx_a.run_until_parked();
    cx_b.run_until_parked();
    advance_both(
        cx_a,
        cx_b,
        KEEP_ALIVE_SYNC_INTERVAL + Duration::from_secs(1),
    );
    let state_a = state(cx_a);
    let state_b = state(cx_b);
    assert!(
        state_a.enabled && state_b.enabled,
        "both Zeds must have keep-alive on before the ping is due; got A enabled={}, \
         B enabled={}, expected both true",
        state_a.enabled,
        state_b.enabled
    );

    for _ in 0..4 {
        advance_both(cx_a, cx_b, KEEP_ALIVE_POLL_INTERVAL);
    }
    let sends_a = machine_a.sends();
    let sends_b = machine_b.sends();
    assert_eq!(
        sends_a + sends_b,
        1,
        "one due period must be pinged once across both Zeds; got A={sends_a} B={sends_b} \
         (total {}), expected total 1",
        sends_a + sends_b
    );
    let on_host = host.state();
    assert!(
        on_host
            .as_ref()
            .is_some_and(|(_, state)| state.pings_sent == 1 && state.ping_outstanding),
        "the host must record the one claimed ping; host holds {on_host:?}, expected \
         pings_sent == 1 and ping_outstanding == true"
    );
}

/// Spec item 5: after two answered pings in warm+compact, the next poll compacts once
/// instead of pinging, and keep-alive is off from then on.
#[gpui::test]
async fn warm_and_compact_compacts_once_after_two_answered_pings(cx: &mut TestAppContext) {
    let host = Arc::new(FakeHost::default());
    let now = now_millis();
    let machine = Machine::new(temporary_home("compact"), now - 59 * MINUTE_MS);
    let _store = start_zed(
        Arc::new(SharedSource {
            machine: machine.clone(),
            host,
        }),
        cx,
    );

    toggle_shared(cx);
    toggle_shared(cx);
    cx.run_until_parked();
    let initial = state(cx);
    assert!(
        initial.enabled && initial.compact,
        "two toggles from off must reach warm+compact; got enabled={} compact={}, expected \
         enabled=true compact=true",
        initial.enabled,
        initial.compact
    );

    let replies = [now - 58 * MINUTE_MS, now - 57 * MINUTE_MS];
    for (index, reply_at) in replies.iter().enumerate() {
        let expected_pings = index as u32 + 1;
        advance(cx, KEEP_ALIVE_POLL_INTERVAL);
        assert_eq!(
            machine.sends(),
            expected_pings,
            "ping number {expected_pings} must have been sent; got {} sends, expected \
             {expected_pings}",
            machine.sends()
        );
        let after_ping = state(cx);
        assert_eq!(
            after_ping.pings_sent, expected_pings,
            "pings_sent after ping {expected_pings}: got {}, expected {expected_pings} \
             ({after_ping:?})",
            after_ping.pings_sent
        );
        assert_eq!(
            machine.compacts(),
            0,
            "nothing may be compacted before two pings are answered; got {} compacts \
             after ping {expected_pings}, expected 0",
            machine.compacts()
        );
        machine.set_last_answer(*reply_at);
    }

    advance(cx, KEEP_ALIVE_POLL_INTERVAL);
    assert_eq!(
        machine.compacts(),
        1,
        "the poll after two answered pings must compact once; got {} compacts, expected 1",
        machine.compacts()
    );
    assert_eq!(
        machine.sends(),
        2,
        "the compacting poll must not also ping; got {} sends, expected 2",
        machine.sends()
    );
    let after_compact = state(cx);
    assert!(
        !after_compact.enabled && !after_compact.compact,
        "compacting must turn keep-alive off; got enabled={} compact={} ({after_compact:?}), \
         expected enabled=false compact=false",
        after_compact.enabled,
        after_compact.compact
    );

    for _ in 0..6 {
        advance(cx, KEEP_ALIVE_POLL_INTERVAL);
    }
    advance(cx, KEEP_ALIVE_SYNC_INTERVAL + Duration::from_secs(1));
    assert_eq!(
        (machine.sends(), machine.compacts()),
        (2, 1),
        "once off, nothing more is pinged or compacted; got (sends, compacts) = ({}, {}), \
         expected (2, 1)",
        machine.sends(),
        machine.compacts()
    );
    let final_state = state(cx);
    assert!(
        !final_state.enabled && !final_state.compact,
        "keep-alive must stay off; got enabled={} compact={}, expected false/false",
        final_state.enabled,
        final_state.compact
    );
}

/// Spec item 4, fallback: a host that cannot share keep-alive leaves the decision to this
/// Zed, which still pings.
#[gpui::test]
async fn a_host_that_cannot_share_still_gets_pinged(cx: &mut TestAppContext) {
    let machine = Machine::new(temporary_home("unshared"), now_millis() - 57 * MINUTE_MS);
    let _store = start_zed(
        Arc::new(UnsharedSource {
            machine: machine.clone(),
        }),
        cx,
    );

    cx.update(|cx| {
        keep_alive_registry(cx).update(cx, |registry, _| registry.toggle(SESSION_ID));
    });
    advance(cx, KEEP_ALIVE_SYNC_INTERVAL + Duration::from_secs(1));
    let after_sync = state(cx);
    assert!(
        after_sync.enabled,
        "a failed shared read must not turn local keep-alive off; got enabled={}, expected true",
        after_sync.enabled
    );

    advance(cx, KEEP_ALIVE_POLL_INTERVAL);
    assert_eq!(
        machine.sends(),
        1,
        "without sharing the due ping is still sent; got {} sends, expected 1",
        machine.sends()
    );
}

/// Spec item 3 (revised): a click whose write is rejected is laid over the host's record
/// and written again at the host's revision.
#[gpui::test]
async fn a_click_that_conflicts_is_laid_over_the_hosts_record(cx: &mut TestAppContext) {
    let host = Arc::new(FakeHost::default());
    let answer_at = now_millis() - 10 * MINUTE_MS;
    let machine = Machine::new(temporary_home("conflict"), answer_at);
    let _store = start_zed(
        Arc::new(SharedSource {
            machine,
            host: host.clone(),
        }),
        cx,
    );

    // Another Zed writes the record after this one's first scan and before its next sync,
    // so this Zed still knows the session at revision 0.
    let hosts_state = KeepAliveState {
        enabled: false,
        compact: false,
        last_real_answer_ms: Some(answer_at),
        last_seen_answer_ms: Some(answer_at),
        last_ping_ms: Some(12_345),
        pings_sent: 2,
        ping_outstanding: false,
    };
    host.put(5, &hosts_state);

    toggle_shared(cx);
    cx.run_until_parked();

    let writes = host.writes();
    assert_eq!(
        writes,
        vec![
            (SESSION_ID.to_string(), 0, false),
            (SESSION_ID.to_string(), 5, true)
        ],
        "the click must be rejected at revision 0 and then applied at revision 5; got \
         {writes:?}"
    );
    let (revision, on_host) = host.state().expect("the host must hold a record");
    assert_eq!(revision, 6, "the host revision must be 6; got {revision}");
    assert!(
        on_host.enabled && !on_host.compact,
        "the host must hold warm; got enabled={} compact={}, expected enabled=true \
         compact=false",
        on_host.enabled,
        on_host.compact
    );
    assert_eq!(
        on_host.last_seen_answer_ms,
        Some(answer_at),
        "last_seen_answer_ms must keep the host's value; got {:?}, expected {:?}",
        on_host.last_seen_answer_ms,
        Some(answer_at)
    );
    assert_eq!(
        on_host.pings_sent, 0,
        "turning on from off must reset pings_sent; got {}, expected 0",
        on_host.pings_sent
    );
    let local = state(cx);
    assert_eq!(
        local, on_host,
        "this Zed must hold what the host holds; got {local:?}, expected {on_host:?}"
    );
}

/// Spec item 3 (revised): a click gives up after the fourth conflict and takes the host's
/// record.
#[gpui::test]
async fn a_click_that_keeps_losing_gives_up_after_four_writes(cx: &mut TestAppContext) {
    let host = Arc::new(FakeHost::default());
    let answer_at = now_millis() - 10 * MINUTE_MS;
    let machine = Machine::new(temporary_home("conflict-loop"), answer_at);
    let _store = start_zed(
        Arc::new(SharedSource {
            machine,
            host: host.clone(),
        }),
        cx,
    );

    let hosts_state = KeepAliveState {
        enabled: false,
        compact: false,
        last_real_answer_ms: Some(answer_at),
        last_seen_answer_ms: Some(answer_at),
        last_ping_ms: Some(12_345),
        pings_sent: 2,
        ping_outstanding: false,
    };
    host.put(5, &hosts_state);
    host.always_beaten_to_it.store(true, Ordering::SeqCst);

    toggle_shared(cx);
    cx.run_until_parked();

    let writes = host.writes();
    assert_eq!(
        writes.len(),
        4,
        "a click must write once and retry three times; got {} writes ({writes:?}), \
         expected 4",
        writes.len()
    );
    assert!(
        writes.iter().all(|(_, _, applied)| !*applied),
        "every write was beaten to it; got {writes:?}, expected none applied"
    );
    let (revision, on_host) = host.state().expect("the host must hold a record");
    assert!(
        !on_host.enabled,
        "the host record must stay as the others left it; got {on_host:?}"
    );
    let local = state(cx);
    assert_eq!(
        local, on_host,
        "after giving up this Zed must hold the host's last record (revision {revision}); \
         got {local:?}, expected {on_host:?}"
    );
}
