//! Deciding which ports should be offered for forwarding.
//!
//! The ports come from the *process* source of VS Code's remote explorer: the
//! remote server is asked which ports are listening
//! (`extHostTunnelService.ts`). This module holds the parts that are pure
//! decisions, so they can be tested without a socket or a clock.

use collections::HashSet;
use remote::ListeningPort;
use remote::listening_ports::PortOwner;
use settings::AutoForwardPortsContent;

/// What should happen when a port is detected. Same set of choices as
/// `devcontainer.json`'s `onAutoForward`, which is deserialized separately in
/// `dev_container`; the two are kept apart because that one describes a file
/// format and this one describes behaviour.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnAutoForward {
    /// Ask before exposing anything. This is the default on purpose: forwarding
    /// silently would put a service the user did not think about on their
    /// loopback interface.
    #[default]
    Notify,
    OpenBrowser,
    OpenBrowserOnce,
    OpenPreview,
    Silent,
    Ignore,
}

/// What the panel actually does about a detected port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoForwardAction {
    /// Offer the forward and wait for the user to accept it.
    Notify,
    /// Forward straight away, optionally showing the result.
    Forward { open_in_browser: bool },
    /// Leave the port alone.
    Ignore,
}

pub fn auto_forward_action(policy: OnAutoForward) -> AutoForwardAction {
    match policy {
        OnAutoForward::Notify => AutoForwardAction::Notify,
        OnAutoForward::Silent => AutoForwardAction::Forward {
            open_in_browser: false,
        },
        // Zed has no in-editor web preview, so a request for one is served by
        // the browser rather than refused.
        OnAutoForward::OpenBrowser
        | OnAutoForward::OpenBrowserOnce
        | OnAutoForward::OpenPreview => AutoForwardAction::Forward {
            open_in_browser: true,
        },
        OnAutoForward::Ignore => AutoForwardAction::Ignore,
    }
}

/// The policy a user's `remote.auto_forward_ports` setting asks for.
///
/// The setting deliberately offers fewer choices than [`OnAutoForward`], which
/// also has to represent what a `devcontainer.json` can ask for.
pub fn on_auto_forward(setting: AutoForwardPortsContent) -> OnAutoForward {
    match setting {
        AutoForwardPortsContent::Notify => OnAutoForward::Notify,
        AutoForwardPortsContent::Silent => OnAutoForward::Silent,
        AutoForwardPortsContent::OpenBrowser => OnAutoForward::OpenBrowser,
        AutoForwardPortsContent::Ignore => OnAutoForward::Ignore,
    }
}

/// Turns each scan of the remote host's listening ports into the set that
/// appeared since the previous scan.
///
/// The first scan only establishes the baseline: the ports that were already up
/// when the connection was made are not news, and announcing them would greet
/// every connection with a row of notifications.
#[derive(Default)]
pub struct ListeningPortTracker {
    known: HashSet<ListeningPort>,
    baseline_taken: bool,
}

impl ListeningPortTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn baseline_taken(&self) -> bool {
        self.baseline_taken
    }

    pub fn observe(&mut self, ports: Vec<ListeningPort>) -> Vec<ListeningPort> {
        let current: HashSet<ListeningPort> = ports.iter().cloned().collect();
        let mut appeared = Vec::new();
        if self.baseline_taken {
            for port in ports {
                if !self.known.contains(&port) {
                    appeared.push(port);
                }
            }
        }
        // A port that went away is forgotten, so that a server which is
        // restarted on the same port is announced again.
        self.known = current;
        self.baseline_taken = true;
        appeared
    }
}

/// A port the remote server reported, with who opened it. `owner` is `None`
/// only from a server too old to attribute ports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetectedPort {
    pub port: ListeningPort,
    pub owner: Option<PortOwner>,
    /// False when the server judged the port not worth a notification: one
    /// the operating system picked, or a browser's. An older server never
    /// says so.
    pub worth_offering: bool,
}

/// Of the ports that just appeared, the ones worth offering: those this
/// project's processes opened, less the ones the server marked as not worth
/// it. An old server says nothing about ownership, and its ports are offered
/// as they always were.
pub fn ports_to_offer(
    detected: Vec<DetectedPort>,
    appeared: &[ListeningPort],
) -> Vec<DetectedPort> {
    detected
        .into_iter()
        .filter(|detected| appeared.contains(&detected.port))
        .filter(|detected| detected.owner.as_ref().is_none_or(|owner| owner.in_project))
        .filter(|detected| detected.worth_offering)
        .collect()
}

/// The text of the notification offering a detected port.
pub fn detected_port_message(port: u16, owner: Option<&PortOwner>) -> String {
    let mut details = Vec::new();
    if let Some(owner) = owner {
        if let Some(process_name) = &owner.process_name {
            details.push(process_name.clone());
        }
        if let Some(session) = owner
            .claude_session_name
            .as_ref()
            .or(owner.claude_session_id.as_ref())
        {
            details.push(format!("Claude session \"{session}\""));
        }
    }
    if details.is_empty() {
        format!("Port {port} is now listening on the remote host.")
    } else {
        format!(
            "Port {port} is now listening on the remote host ({}).",
            details.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listening(host: &str, port: u16) -> ListeningPort {
        ListeningPort {
            host: host.to_string(),
            port,
        }
    }

    fn owned(in_project: bool) -> PortOwner {
        PortOwner {
            process_id: Some(100),
            process_name: Some("node".to_string()),
            in_project,
            claude_session_id: None,
            claude_session_name: None,
        }
    }

    fn detected(port: u16, owner: Option<PortOwner>) -> DetectedPort {
        DetectedPort {
            port: listening("127.0.0.1", port),
            owner,
            worth_offering: true,
        }
    }

    #[test]
    fn test_ports_to_offer_skips_ports_the_server_says_are_not_worth_offering() {
        let appeared = vec![
            listening("127.0.0.1", 8977),
            listening("127.0.0.1", 9641),
            listening("127.0.0.1", 39205),
        ];
        let browser = DetectedPort {
            worth_offering: false,
            ..detected(9641, Some(owned(true)))
        };
        let ephemeral = DetectedPort {
            worth_offering: false,
            ..detected(39205, Some(owned(true)))
        };
        assert_eq!(
            ports_to_offer(
                vec![detected(8977, Some(owned(true))), browser, ephemeral],
                &appeared
            ),
            vec![detected(8977, Some(owned(true)))],
            "a project's dev server is offered; its browser and OS-assigned ports are not"
        );
    }

    #[test]
    fn test_ports_to_offer_keeps_only_this_projects_ports() {
        let mut tracker = ListeningPortTracker::new();
        let baseline = [detected(22, Some(owned(false)))];
        tracker.observe(baseline.iter().map(|port| port.port.clone()).collect());

        let scan = vec![
            detected(22, Some(owned(false))),
            detected(3000, Some(owned(true))),
            detected(4000, Some(owned(false))),
        ];
        let appeared = tracker.observe(scan.iter().map(|port| port.port.clone()).collect());
        assert_eq!(
            ports_to_offer(scan.clone(), &appeared),
            vec![detected(3000, Some(owned(true)))],
            "a port opened outside the project is not offered, and the baseline port is not new"
        );

        assert_eq!(
            tracker.observe(scan.iter().map(|port| port.port.clone()).collect()),
            Vec::new(),
            "the suppressed port is still tracked, so it does not come back on the next scan"
        );
    }

    #[test]
    fn test_ports_to_offer_offers_every_port_from_an_old_server() {
        let appeared = vec![listening("127.0.0.1", 3000), listening("127.0.0.1", 4000)];
        assert_eq!(
            ports_to_offer(vec![detected(3000, None), detected(4000, None)], &appeared),
            vec![detected(3000, None), detected(4000, None)]
        );
    }

    #[test]
    fn test_detected_port_message_names_the_owner() {
        assert_eq!(
            detected_port_message(3000, None),
            "Port 3000 is now listening on the remote host.",
            "an old server's ports keep the old text"
        );
        assert_eq!(
            detected_port_message(3000, Some(&PortOwner::default())),
            "Port 3000 is now listening on the remote host."
        );
        assert_eq!(
            detected_port_message(3000, Some(&owned(true))),
            "Port 3000 is now listening on the remote host (node)."
        );
        assert_eq!(
            detected_port_message(
                3000,
                Some(&PortOwner {
                    claude_session_id: Some("0f3a".to_string()),
                    claude_session_name: Some("fix-login".to_string()),
                    ..owned(true)
                })
            ),
            "Port 3000 is now listening on the remote host (node, Claude session \"fix-login\")."
        );
        assert_eq!(
            detected_port_message(
                3000,
                Some(&PortOwner {
                    process_name: None,
                    claude_session_id: Some("0f3a".to_string()),
                    ..owned(true)
                })
            ),
            "Port 3000 is now listening on the remote host (Claude session \"0f3a\").",
            "an unnamed session is called by its id"
        );
    }

    #[test]
    fn test_listening_port_tracker_treats_the_first_scan_as_the_baseline() {
        let mut tracker = ListeningPortTracker::new();
        assert!(!tracker.baseline_taken());

        assert_eq!(
            tracker.observe(vec![listening("127.0.0.1", 22), listening("0.0.0.0", 5432)]),
            Vec::new(),
            "ports that were already up when the connection was made are not new"
        );
        assert!(tracker.baseline_taken());

        assert_eq!(
            tracker.observe(vec![
                listening("127.0.0.1", 22),
                listening("0.0.0.0", 5432),
                listening("127.0.0.1", 3000),
            ]),
            vec![listening("127.0.0.1", 3000)]
        );
        assert_eq!(
            tracker.observe(vec![
                listening("127.0.0.1", 22),
                listening("0.0.0.0", 5432),
                listening("127.0.0.1", 3000),
            ]),
            Vec::new(),
            "an unchanged scan reports nothing"
        );
    }

    #[test]
    fn test_listening_port_tracker_reports_a_restarted_server_again() {
        let mut tracker = ListeningPortTracker::new();
        tracker.observe(vec![listening("127.0.0.1", 22)]);
        assert_eq!(
            tracker.observe(vec![
                listening("127.0.0.1", 22),
                listening("127.0.0.1", 3000)
            ]),
            vec![listening("127.0.0.1", 3000)]
        );
        assert_eq!(
            tracker.observe(vec![listening("127.0.0.1", 22)]),
            Vec::new(),
            "a port going away is not an appearance"
        );
        assert_eq!(
            tracker.observe(vec![
                listening("127.0.0.1", 22),
                listening("127.0.0.1", 3000)
            ]),
            vec![listening("127.0.0.1", 3000)],
            "the same port coming back is announced again"
        );
    }

    #[test]
    fn test_auto_forward_action_never_forwards_silently_by_default() {
        assert_eq!(
            auto_forward_action(OnAutoForward::default()),
            AutoForwardAction::Notify,
            "the default must ask before putting a remote service on the local machine"
        );
        assert_eq!(
            auto_forward_action(OnAutoForward::Silent),
            AutoForwardAction::Forward {
                open_in_browser: false
            }
        );
        for policy in [
            OnAutoForward::OpenBrowser,
            OnAutoForward::OpenBrowserOnce,
            OnAutoForward::OpenPreview,
        ] {
            assert_eq!(
                auto_forward_action(policy),
                AutoForwardAction::Forward {
                    open_in_browser: true
                },
                "{policy:?} forwards and shows the result"
            );
        }
        assert_eq!(
            auto_forward_action(OnAutoForward::Ignore),
            AutoForwardAction::Ignore
        );
    }

    #[test]
    fn test_the_setting_maps_onto_a_policy_and_defaults_to_notify() {
        assert_eq!(
            on_auto_forward(AutoForwardPortsContent::default()),
            OnAutoForward::Notify,
            "an unset setting must not start forwarding on its own"
        );
        assert_eq!(
            auto_forward_action(on_auto_forward(AutoForwardPortsContent::default())),
            AutoForwardAction::Notify
        );

        for (setting, expected) in [
            (AutoForwardPortsContent::Notify, OnAutoForward::Notify),
            (AutoForwardPortsContent::Silent, OnAutoForward::Silent),
            (
                AutoForwardPortsContent::OpenBrowser,
                OnAutoForward::OpenBrowser,
            ),
            (AutoForwardPortsContent::Ignore, OnAutoForward::Ignore),
        ] {
            assert_eq!(
                on_auto_forward(setting),
                expected,
                "{setting:?} asks for {expected:?}"
            );
        }
    }
}
