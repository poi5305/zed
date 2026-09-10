use std::time::Instant;

use gpui::{AsyncApp, Context, EventEmitter, SharedString, Task, WeakEntity};
use remote::{ListeningPort, ScanTimings};
use rpc::{AnyProtoClient, proto};

use crate::port_detection::ListeningPortTracker;

/// A remote server that does not answer the request at all - an older build,
/// or a platform where scanning is not implemented - would otherwise be polled
/// for the whole life of the connection.
pub const MAX_CONSECUTIVE_FAILURES: usize = 3;

pub enum PortDetectorEvent {
    /// Ports that were not listening on the previous scan. The scan made when
    /// the connection opens is the baseline and never produces this.
    PortsAppeared(Vec<ListeningPort>),
    /// Scanning has been abandoned. Nothing will be detected until a new
    /// detector is built, so whoever owns this one has to say so and offer a
    /// way to start again.
    Stopped { reason: SharedString },
}

/// Polls the remote server for the ports listening on it.
pub struct PortDetector {
    project_id: u64,
    client: AnyProtoClient,
    tracker: ListeningPortTracker,
    timings: ScanTimings,
    _scan_task: Task<()>,
}

impl EventEmitter<PortDetectorEvent> for PortDetector {}

impl PortDetector {
    pub fn new(project_id: u64, client: AnyProtoClient, cx: &mut Context<Self>) -> Self {
        Self {
            project_id,
            client,
            tracker: ListeningPortTracker::new(),
            timings: ScanTimings::default(),
            _scan_task: cx.spawn(async move |this, cx| Self::scan_loop(this, cx).await),
        }
    }

    async fn scan_loop(this: WeakEntity<Self>, cx: &mut AsyncApp) {
        let mut consecutive_failures = 0usize;
        loop {
            let Ok((project_id, client)) =
                this.read_with(cx, |this, _| (this.project_id, this.client.clone()))
            else {
                return;
            };

            let started_at = Instant::now();
            let response = client
                .request(proto::GetListeningPorts { project_id })
                .await;
            let elapsed = started_at.elapsed();

            match response {
                Ok(response) => {
                    consecutive_failures = 0;
                    let ports = response
                        .ports
                        .into_iter()
                        .filter_map(|port| {
                            Some(ListeningPort {
                                host: port.host,
                                port: u16::try_from(port.port).ok()?,
                            })
                        })
                        .collect();
                    let updated = this.update(cx, |this, cx| {
                        this.timings.record(elapsed);
                        let appeared = this.tracker.observe(ports);
                        if !appeared.is_empty() {
                            cx.emit(PortDetectorEvent::PortsAppeared(appeared));
                        }
                    });
                    if updated.is_err() {
                        return;
                    }
                }
                Err(error) => {
                    consecutive_failures += 1;
                    log::warn!("port detection: could not list remote ports: {error:#}");
                    if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                        let reason = SharedString::from(format!(
                            "Port detection stopped after {consecutive_failures} \
                             failed scans: {error:#}"
                        ));
                        log::warn!("port detection: {reason}");
                        this.update(cx, |_, cx| {
                            cx.emit(PortDetectorEvent::Stopped { reason });
                        })
                        .ok();
                        return;
                    }
                }
            }

            let Ok(delay) = this.read_with(cx, |this, _| this.timings.next_delay()) else {
                return;
            };
            cx.background_executor().timer(delay).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::SeqCst},
    };

    use anyhow::Result;
    use futures::{FutureExt as _, future::BoxFuture};
    use gpui::{AppContext as _, TestAppContext};
    use parking_lot::Mutex;
    use remote::listening_ports::MINIMUM_SCAN_INTERVAL;
    use rpc::{ProtoClient, ProtoMessageHandlerSet, proto::Envelope};

    use super::*;

    #[derive(Default)]
    struct AlwaysFailingProtoClient {
        requests: Arc<AtomicUsize>,
        handlers: Mutex<ProtoMessageHandlerSet>,
    }

    impl ProtoClient for AlwaysFailingProtoClient {
        fn request(
            &self,
            _envelope: Envelope,
            _request_type: &'static str,
        ) -> BoxFuture<'static, Result<Envelope>> {
            self.requests.fetch_add(1, SeqCst);
            async { anyhow::bail!("the remote server is not answering") }.boxed()
        }

        fn send(&self, _envelope: Envelope, _message_type: &'static str) -> Result<()> {
            anyhow::bail!("this client only answers requests")
        }

        fn send_response(&self, _envelope: Envelope, _message_type: &'static str) -> Result<()> {
            anyhow::bail!("this client only answers requests")
        }

        fn message_handler_set(&self) -> &Mutex<ProtoMessageHandlerSet> {
            &self.handlers
        }

        fn is_via_collab(&self) -> bool {
            false
        }

        fn has_wsl_interop(&self) -> bool {
            false
        }
    }

    #[gpui::test]
    async fn test_the_detector_says_so_when_it_gives_up(cx: &mut TestAppContext) {
        let requests = Arc::new(AtomicUsize::new(0));
        let client = AnyProtoClient::new(Arc::new(AlwaysFailingProtoClient {
            requests: requests.clone(),
            handlers: Mutex::default(),
        }));
        let detector = cx.new(|cx| PortDetector::new(1, client, cx));

        let stopped_reasons = Arc::new(Mutex::new(Vec::<SharedString>::new()));
        let subscription = cx.update(|cx| {
            cx.subscribe(&detector, {
                let stopped_reasons = stopped_reasons.clone();
                move |_, event: &PortDetectorEvent, _| {
                    if let PortDetectorEvent::Stopped { reason } = event {
                        stopped_reasons.lock().push(reason.clone());
                    }
                }
            })
        });

        cx.run_until_parked();
        for _ in 0..MAX_CONSECUTIVE_FAILURES {
            cx.executor().advance_clock(MINIMUM_SCAN_INTERVAL);
            cx.run_until_parked();
        }

        assert_eq!(
            requests.load(SeqCst),
            MAX_CONSECUTIVE_FAILURES,
            "the loop should stop asking after {MAX_CONSECUTIVE_FAILURES} failures"
        );
        let reasons = stopped_reasons.lock().clone();
        assert_eq!(
            reasons.len(),
            1,
            "expected one Stopped event after {MAX_CONSECUTIVE_FAILURES} failed scans, got {reasons:?}"
        );
        assert!(
            reasons[0].contains(&MAX_CONSECUTIVE_FAILURES.to_string())
                && reasons[0].contains("not answering"),
            "the reason must name how many scans failed and why, got {:?}",
            reasons[0]
        );

        // A detector that has given up must stay quiet rather than keep the
        // timer alive.
        cx.executor().advance_clock(MINIMUM_SCAN_INTERVAL * 10);
        cx.run_until_parked();
        assert_eq!(
            requests.load(SeqCst),
            MAX_CONSECUTIVE_FAILURES,
            "no further scans should have been made"
        );
        assert_eq!(stopped_reasons.lock().len(), 1);

        drop(subscription);
    }
}
