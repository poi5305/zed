//! Deciding which ports should be offered for forwarding.
//!
//! The ports come from the *process* source of VS Code's remote explorer: the
//! remote server is asked which ports are listening
//! (`extHostTunnelService.ts`). This module holds the parts that are pure
//! decisions, so they can be tested without a socket or a clock.

use collections::HashSet;
use remote::ListeningPort;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn listening(host: &str, port: u16) -> ListeningPort {
        ListeningPort {
            host: host.to_string(),
            port,
        }
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
