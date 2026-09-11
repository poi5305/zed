mod forward_ports_button;
mod forward_ports_panel;
pub mod port_detection;
mod port_detector;

use collections::HashSet;
use std::collections::BTreeMap;
use std::fmt;

use gpui::{App, actions};
use remote::{DockerConnectionOptions, RemoteConnectionOptions};
use settings::{
    DevContainerConnection, RemoteSettingsContent, SshConnection, SshPortForwardOption,
    WslConnection,
};
use workspace::Workspace;

pub use forward_ports_button::ForwardPortsButton;
pub use forward_ports_panel::ForwardPortsPanel;

actions!(
    forward_ports,
    [
        /// Toggles focus on the forward ports panel.
        ToggleFocus
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<ForwardPortsPanel>(window, cx);
        });
    })
    .detach();
}

/// `ssh -L` binds `localhost` when the host part of a forward is left out, so
/// that is what an unset `local_host` / `remote_host` shows as.
pub const DEFAULT_FORWARD_HOST: &str = "localhost";

/// Which of the two ports of a forward an error is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortField {
    Local,
    Remote,
}

impl fmt::Display for PortField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PortField::Local => write!(formatter, "Local port"),
            PortField::Remote => write!(formatter, "Remote port"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortForwardError {
    MissingPort { field: PortField },
    InvalidPort { field: PortField, value: String },
    DuplicateLocalPort { local_port: u16, connection: String },
}

impl fmt::Display for PortForwardError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PortForwardError::MissingPort { field } => write!(formatter, "{field} is required."),
            PortForwardError::InvalidPort { field, value } => write!(
                formatter,
                "{field} must be a number between 1 and {}, got \"{value}\".",
                u16::MAX
            ),
            PortForwardError::DuplicateLocalPort {
                local_port,
                connection,
            } => write!(
                formatter,
                "Local port {local_port} is already forwarded on {connection}."
            ),
        }
    }
}

/// The raw text of the panel's inline add/edit form, before validation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PortForwardDraft {
    pub local_host: String,
    pub local_port: String,
    pub remote_host: String,
    pub remote_port: String,
}

impl PortForwardDraft {
    pub fn from_forward(forward: &SshPortForwardOption) -> Self {
        Self {
            local_host: forward.local_host.clone().unwrap_or_default(),
            local_port: forward.local_port.to_string(),
            remote_host: forward.remote_host.clone().unwrap_or_default(),
            remote_port: forward.remote_port.to_string(),
        }
    }
}

/// The fields that address a connection, whichever transport carries it. The
/// panel renders the merged settings but writes to the user settings file, and
/// the two lists need not line up by index, so edits are routed by this key
/// rather than by position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionKey {
    Ssh {
        host: String,
        username: Option<String>,
        port: Option<u16>,
    },
    Wsl {
        distro_name: String,
        user: Option<String>,
    },
    /// Keyed on the devcontainer.json project name rather than the container
    /// id, because `devcontainer up` mints a fresh id on every rebuild while
    /// the project name is stable.
    DevContainer { name: String },
}

impl ConnectionKey {
    /// A stable, transport-qualified string for element ids, so that a wsl
    /// distro and an ssh host of the same name do not collide.
    pub fn identifier(&self) -> String {
        match self {
            ConnectionKey::Ssh {
                host,
                username,
                port,
            } => format!(
                "ssh-{}-{}-{}",
                username.as_deref().unwrap_or_default(),
                host,
                port.map(|port| port.to_string()).unwrap_or_default()
            ),
            ConnectionKey::Wsl { distro_name, user } => format!(
                "wsl-{}-{}",
                user.as_deref().unwrap_or_default(),
                distro_name
            ),
            ConnectionKey::DevContainer { name } => format!("dev-container-{name}"),
        }
    }

    pub fn is_dev_container(&self) -> bool {
        matches!(self, ConnectionKey::DevContainer { .. })
    }
}

/// Implemented by every settings entry the panel can route an edit to, so that
/// [`find_connection`] works over all three lists.
pub trait ConnectionEntry {
    fn connection_key(&self) -> ConnectionKey;
}

impl ConnectionEntry for SshConnection {
    fn connection_key(&self) -> ConnectionKey {
        ConnectionKey::Ssh {
            host: self.host.clone(),
            username: self.username.clone(),
            port: self.port,
        }
    }
}

impl ConnectionEntry for WslConnection {
    fn connection_key(&self) -> ConnectionKey {
        ConnectionKey::Wsl {
            distro_name: self.distro_name.clone(),
            user: self.user.clone(),
        }
    }
}

impl ConnectionEntry for DevContainerConnection {
    fn connection_key(&self) -> ConnectionKey {
        ConnectionKey::DevContainer {
            name: self.name.clone(),
        }
    }
}

impl ConnectionEntry for DockerConnectionOptions {
    fn connection_key(&self) -> ConnectionKey {
        ConnectionKey::DevContainer {
            name: self.name.clone(),
        }
    }
}

/// One row of the panel, from whichever settings list it came out of.
#[derive(Clone, Debug, PartialEq)]
pub enum ForwardConnection {
    Ssh(SshConnection),
    Wsl(WslConnection),
    DevContainer(DevContainerConnection),
}

impl ForwardConnection {
    pub fn port_forwards(&self) -> Vec<SshPortForwardOption> {
        match self {
            ForwardConnection::Ssh(connection) => connection.port_forwards.clone(),
            ForwardConnection::Wsl(connection) => connection.port_forwards.clone(),
            ForwardConnection::DevContainer(connection) => connection.port_forwards.clone(),
        }
        .unwrap_or_default()
    }
}

impl ConnectionEntry for ForwardConnection {
    fn connection_key(&self) -> ConnectionKey {
        match self {
            ForwardConnection::Ssh(connection) => connection.connection_key(),
            ForwardConnection::Wsl(connection) => connection.connection_key(),
            ForwardConnection::DevContainer(connection) => connection.connection_key(),
        }
    }
}

pub fn find_connection<T: ConnectionEntry>(
    connections: &[T],
    key: &ConnectionKey,
) -> Option<usize> {
    connections
        .iter()
        .position(|connection| connection.connection_key() == *key)
}

/// The key for the connection this window is open on, when that transport is
/// one the panel can address.
pub fn connection_key_for_options(options: &RemoteConnectionOptions) -> Option<ConnectionKey> {
    match options {
        RemoteConnectionOptions::Ssh(options) => Some(ConnectionKey::Ssh {
            host: options.host.to_string(),
            username: options.username.clone(),
            port: options.port,
        }),
        RemoteConnectionOptions::Wsl(options) => Some(ConnectionKey::Wsl {
            distro_name: options.distro_name.clone(),
            user: options.user.clone(),
        }),
        RemoteConnectionOptions::Docker(options) => Some(options.connection_key()),
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

/// The name the panel shows for a connection: for ssh its nickname when it has
/// one, otherwise the `user@host:port` triple that identifies it; for the other
/// transports the distro or dev container project name.
pub fn connection_label(connection: &ForwardConnection) -> String {
    match connection {
        ForwardConnection::Ssh(connection) => ssh_connection_label(connection),
        ForwardConnection::Wsl(connection) => {
            match connection
                .user
                .as_deref()
                .filter(|user| !user.trim().is_empty())
            {
                Some(user) => format!("{user}@{}", connection.distro_name),
                None => connection.distro_name.clone(),
            }
        }
        ForwardConnection::DevContainer(connection) => connection.name.clone(),
    }
}

fn ssh_connection_label(connection: &SshConnection) -> String {
    if let Some(nickname) = connection
        .nickname
        .as_deref()
        .map(str::trim)
        .filter(|nickname| !nickname.is_empty())
    {
        return nickname.to_string();
    }

    let mut label = String::new();
    if let Some(username) = connection
        .username
        .as_deref()
        .filter(|username| !username.is_empty())
    {
        label.push_str(username);
        label.push('@');
    }
    label.push_str(&connection.host);
    if let Some(port) = connection.port {
        label.push(':');
        label.push_str(&port.to_string());
    }
    label
}

/// Whether an edit for `key` can be written to the user settings file.
///
/// An ssh or wsl key carries everything its entry needs — the host, the user and
/// the port — so one can be written from the key alone, and a connection opened
/// from a URI or from an ssh config host is as editable as one that was typed
/// into the settings file. This is what lets the panel record a forward for the
/// connection this window is actually on, which is the one whose ports are being
/// detected.
///
/// A dev container is the exception: its entry needs the running container's id
/// and user, which the key does not carry, so one can only be written while this
/// window holds that container's connection.
pub fn connection_is_editable(
    user_settings: Option<&RemoteSettingsContent>,
    key: &ConnectionKey,
    dev_container_options: Option<&DockerConnectionOptions>,
) -> bool {
    match key {
        ConnectionKey::Ssh { .. } | ConnectionKey::Wsl { .. } => true,
        ConnectionKey::DevContainer { .. } => {
            let entry_exists = user_settings.is_some_and(|remote| {
                remote
                    .dev_container_connections
                    .as_ref()
                    .is_some_and(|connections| find_connection(connections, key).is_some())
            });
            entry_exists
                || dev_container_options.is_some_and(|options| options.connection_key() == *key)
        }
    }
}

/// How a connection reads in the panel when it has no settings entry to take a
/// nickname from, built from the key alone.
pub fn connection_label_for_key(key: &ConnectionKey) -> String {
    match key {
        ConnectionKey::Ssh {
            host,
            username,
            port,
        } => connection_label(&ForwardConnection::Ssh(SshConnection {
            host: host.clone(),
            username: username.clone(),
            port: *port,
            ..SshConnection::default()
        })),
        ConnectionKey::Wsl { distro_name, user } => {
            connection_label(&ForwardConnection::Wsl(WslConnection {
                distro_name: distro_name.clone(),
                user: user.clone(),
                ..WslConnection::default()
            }))
        }
        ConnectionKey::DevContainer { name } => name.clone(),
    }
}

/// Whether the panel offers an Add button for a row.
///
/// Only dev containers can be refused, and only when they have no entry yet:
/// creating one needs the running container's details, which are available
/// only while this window is open on it. Once the entry exists it is an
/// ordinary settings entry like any other, so no live connection is needed.
/// Ssh and wsl rows are always offered; an entry that turns out not to live in
/// the user settings file is reported when the form is saved.
pub fn can_add_forward(
    user_settings: Option<&RemoteSettingsContent>,
    key: &ConnectionKey,
    dev_container_options: Option<&DockerConnectionOptions>,
) -> bool {
    !key.is_dev_container() || connection_is_editable(user_settings, key, dev_container_options)
}

/// The forward list an edit for `key` belongs in, inside the settings file
/// being written.
///
/// Nothing else in the tree writes `remote.dev_container_connections`, so the
/// entry for a dev container has to be created here on the first edit. That
/// needs the live [`DockerConnectionOptions`] to fill in real container
/// details, which is why a dev container that this window is not connected to
/// yields `None` rather than a half-empty entry.
pub fn port_forwards_for_key_mut<'a>(
    remote: &'a mut RemoteSettingsContent,
    key: &ConnectionKey,
    dev_container_options: Option<&DockerConnectionOptions>,
) -> Option<&'a mut Vec<SshPortForwardOption>> {
    match key {
        // Written from the key when the file has no entry for this connection yet:
        // a forward recorded against no connection is a forward that never runs,
        // and the key holds every field the entry addresses it by.
        ConnectionKey::Ssh {
            host,
            username,
            port,
        } => {
            let connections = remote.ssh_connections.get_or_insert_with(Vec::new);
            let index = match find_connection(connections, key) {
                Some(index) => index,
                None => {
                    connections.push(SshConnection {
                        host: host.clone(),
                        username: username.clone(),
                        port: *port,
                        ..SshConnection::default()
                    });
                    connections.len().saturating_sub(1)
                }
            };
            Some(
                connections
                    .get_mut(index)?
                    .port_forwards
                    .get_or_insert_with(Vec::new),
            )
        }
        ConnectionKey::Wsl { distro_name, user } => {
            let connections = remote.wsl_connections.get_or_insert_with(Vec::new);
            let index = match find_connection(connections, key) {
                Some(index) => index,
                None => {
                    connections.push(WslConnection {
                        distro_name: distro_name.clone(),
                        user: user.clone(),
                        ..WslConnection::default()
                    });
                    connections.len().saturating_sub(1)
                }
            };
            Some(
                connections
                    .get_mut(index)?
                    .port_forwards
                    .get_or_insert_with(Vec::new),
            )
        }
        ConnectionKey::DevContainer { .. } => {
            let existing = find_connection(
                remote.dev_container_connections.as_deref().unwrap_or(&[]),
                key,
            );
            let connections = match existing {
                Some(_) => remote.dev_container_connections.as_mut()?,
                None => {
                    // Options belonging to a different container would create
                    // an entry under the wrong name, which the lookup below
                    // would then miss, leaving a stray entry behind.
                    let options =
                        dev_container_options.filter(|options| options.connection_key() == *key)?;
                    let connections = remote
                        .dev_container_connections
                        .get_or_insert_with(Vec::new);
                    connections.push(DevContainerConnection {
                        name: options.name.clone(),
                        remote_user: options.remote_user.clone(),
                        container_id: options.container_id.clone(),
                        use_podman: options.use_podman,
                        extension_ids: Vec::new(),
                        remote_env: BTreeMap::new(),
                        port_forwards: None,
                    });
                    connections
                }
            };
            let index = find_connection(connections, key)?;
            Some(
                connections
                    .get_mut(index)?
                    .port_forwards
                    .get_or_insert_with(Vec::new),
            )
        }
    }
}

/// An empty host means "unset", and an IPv6 literal is stored bare because the
/// brackets `-L` needs are added when the ssh command line is built.
pub fn normalize_host(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let unbracketed = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(trimmed)
        .trim();
    if unbracketed.is_empty() {
        None
    } else {
        Some(unbracketed.to_string())
    }
}

/// `ssh -L` binds localhost when the local host of a forward is left out, so an
/// unset host is the loopback address rather than "any address".
pub const DEFAULT_LOCAL_BIND_HOST: &str = "127.0.0.1";

/// Whether two forwards on the same local port would fight over the same
/// socket. A wildcard covers every address on its port, so it overlaps with
/// anything; otherwise only the same address overlaps, which is why
/// `127.0.0.1:8080` and `[::1]:8080` can both be configured.
pub fn bind_addresses_overlap(left: Option<&str>, right: Option<&str>) -> bool {
    fn canonical(host: Option<&str>) -> String {
        let host = host
            .map(str::trim)
            .filter(|host| !host.is_empty())
            .unwrap_or(DEFAULT_LOCAL_BIND_HOST)
            .to_ascii_lowercase();
        if host == "localhost" {
            DEFAULT_LOCAL_BIND_HOST.to_string()
        } else {
            host
        }
    }

    fn is_wildcard(host: &str) -> bool {
        matches!(host, "0.0.0.0" | "::" | "*")
    }

    let left = canonical(left);
    let right = canonical(right);
    is_wildcard(&left) || is_wildcard(&right) || left == right
}

/// Whether one of `configured` already claims the socket a forward with no
/// local host would bind on `port`.
pub fn local_port_is_configured(configured: &[SshPortForwardOption], port: u16) -> bool {
    configured.iter().any(|forward| {
        forward.local_port == port && bind_addresses_overlap(forward.local_host.as_deref(), None)
    })
}

/// How many port numbers above a detected port are examined before the search
/// gives up. Bounded so that a machine with a long run of busy ports cannot
/// turn one detection into thousands of bind attempts.
pub const LOCAL_PORT_SEARCH_LIMIT: u16 = 100;

/// Picks the local port a detected remote port should be published on: the
/// detected port itself when it is free, otherwise the first port above it that
/// is neither already configured on this connection nor held by something else
/// on this machine.
///
/// The bind probe is a parameter rather than a socket call so that the search
/// can be tested, and because the real probe has to run off the foreground
/// thread. Returns `None` when [`LOCAL_PORT_SEARCH_LIMIT`] port numbers have
/// been examined without finding one, or when the search runs off the end of
/// the port range.
pub fn choose_local_port(
    detected_port: u16,
    configured: &[SshPortForwardOption],
    mut can_bind: impl FnMut(u16) -> bool,
) -> Option<u16> {
    (0..LOCAL_PORT_SEARCH_LIMIT)
        .map_while(|offset| detected_port.checked_add(offset))
        .find(|candidate| !local_port_is_configured(configured, *candidate) && can_bind(*candidate))
}

/// The forwards the panel opens tunnels for.
///
/// Two kinds are left out, for opposite reasons: one the transport already carries
/// (`ssh -L` bound it when the connection was made, so binding it again would only
/// collide with the ssh process), and one the reader disconnected by hand.
pub fn tunnelled_forwards(
    configured: &[SshPortForwardOption],
    established_at_connect: &[SshPortForwardOption],
    disconnected: &HashSet<SshPortForwardOption>,
) -> Vec<SshPortForwardOption> {
    configured
        .iter()
        .filter(|forward| !established_at_connect.contains(forward))
        .filter(|forward| !disconnected.contains(*forward))
        .cloned()
        .collect()
}

pub fn parse_port(field: PortField, text: &str) -> Result<u16, PortForwardError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(PortForwardError::MissingPort { field });
    }
    let invalid = || PortForwardError::InvalidPort {
        field,
        value: trimmed.to_string(),
    };
    let value: u64 = trimmed.parse().map_err(|_| invalid())?;
    if value < 1 || value > u16::MAX as u64 {
        return Err(invalid());
    }
    Ok(value as u16)
}

/// Validates the form against the forwards already configured on the same
/// connection. `original` is the entry being edited, which is excluded from the
/// duplicate check so that saving it unchanged is not rejected.
pub fn validate_port_forward(
    draft: &PortForwardDraft,
    connection: &str,
    existing: &[SshPortForwardOption],
    original: Option<&SshPortForwardOption>,
) -> Result<SshPortForwardOption, PortForwardError> {
    let local_port = parse_port(PortField::Local, &draft.local_port)?;
    let remote_port = parse_port(PortField::Remote, &draft.remote_port)?;
    let local_host = normalize_host(&draft.local_host);

    if existing.iter().any(|forward| {
        Some(forward) != original
            && forward.local_port == local_port
            && bind_addresses_overlap(forward.local_host.as_deref(), local_host.as_deref())
    }) {
        return Err(PortForwardError::DuplicateLocalPort {
            local_port,
            connection: connection.to_string(),
        });
    }

    Ok(SshPortForwardOption {
        local_host,
        local_port,
        remote_host: normalize_host(&draft.remote_host),
        remote_port,
    })
}

/// Replaces `original` with `replacement`, or appends when this is a new entry
/// (or when `original` is no longer present, e.g. the file changed underneath).
pub fn apply_port_forward_edit(
    forwards: &mut Vec<SshPortForwardOption>,
    original: Option<&SshPortForwardOption>,
    replacement: SshPortForwardOption,
) {
    let position =
        original.and_then(|original| forwards.iter().position(|forward| forward == original));
    match position {
        Some(position) => forwards[position] = replacement,
        None => forwards.push(replacement),
    }
}

pub fn remove_port_forward(
    forwards: &mut Vec<SshPortForwardOption>,
    forward: &SshPortForwardOption,
) -> bool {
    match forwards.iter().position(|candidate| candidate == forward) {
        Some(position) => {
            forwards.remove(position);
            true
        }
        None => false,
    }
}

fn format_endpoint(host: Option<&str>, port: u16) -> String {
    let host = host.unwrap_or(DEFAULT_FORWARD_HOST);
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// The two ends of a forward: the socket this machine binds, then what the host
/// connects it to — the same way round as `ssh -L`.
///
/// Returned apart rather than as one string because which end is which is drawn with
/// an icon: `localhost:3000 → localhost:3000` is the common case, and nothing in the
/// text of it says which `localhost` is this machine.
pub fn port_forward_endpoints(forward: &SshPortForwardOption) -> (String, String) {
    (
        format_endpoint(forward.local_host.as_deref(), forward.local_port),
        format_endpoint(forward.remote_host.as_deref(), forward.remote_port),
    )
}

/// The same two ends in words, for the tooltip that says what the icons mean.
pub fn describe_port_forward(forward: &SshPortForwardOption) -> String {
    let (local, remote) = port_forward_endpoints(forward);
    format!("{local} on this machine → {remote} on the remote host")
}

#[cfg(test)]
mod tests {
    use super::*;
    use remote::{SshConnectionOptions, WslConnectionOptions};

    fn forward(
        local_host: Option<&str>,
        local_port: u16,
        remote_host: Option<&str>,
        remote_port: u16,
    ) -> SshPortForwardOption {
        SshPortForwardOption {
            local_host: local_host.map(str::to_string),
            local_port,
            remote_host: remote_host.map(str::to_string),
            remote_port,
        }
    }

    fn draft(
        local_host: &str,
        local_port: &str,
        remote_host: &str,
        remote_port: &str,
    ) -> PortForwardDraft {
        PortForwardDraft {
            local_host: local_host.to_string(),
            local_port: local_port.to_string(),
            remote_host: remote_host.to_string(),
            remote_port: remote_port.to_string(),
        }
    }

    /// A forward is left out of the tunnels for two opposite reasons, and confusing
    /// them means either a port that never opens or two things binding one socket.
    #[test]
    fn test_only_the_forwards_this_panel_owns_are_tunnelled() {
        let carried_by_ssh = forward(None, 5432, None, 5432);
        let disconnected_by_hand = forward(None, 8080, None, 80);
        let running = forward(None, 3000, None, 3000);
        let configured = [
            carried_by_ssh.clone(),
            disconnected_by_hand.clone(),
            running.clone(),
        ];

        assert_eq!(
            tunnelled_forwards(
                &configured,
                &[carried_by_ssh],
                &HashSet::from_iter([disconnected_by_hand])
            ),
            vec![running.clone()],
            "one is already bound by the ssh process and one the reader closed"
        );

        assert_eq!(
            tunnelled_forwards(&configured, &[], &HashSet::default()).len(),
            3,
            "with nothing to exclude, every configured forward is opened"
        );
    }

    #[test]
    fn test_parse_port_accepts_the_whole_valid_range() {
        assert_eq!(parse_port(PortField::Local, "1"), Ok(1));
        assert_eq!(parse_port(PortField::Local, "65535"), Ok(65535));
        assert_eq!(
            parse_port(PortField::Remote, "  8080\t"),
            Ok(8080),
            "surrounding whitespace is trimmed rather than rejected"
        );
    }

    #[test]
    fn test_parse_port_rejects_out_of_range_and_non_numeric_input() {
        assert_eq!(
            parse_port(PortField::Local, ""),
            Err(PortForwardError::MissingPort {
                field: PortField::Local
            })
        );
        assert_eq!(
            parse_port(PortField::Remote, "   "),
            Err(PortForwardError::MissingPort {
                field: PortField::Remote
            })
        );

        for value in [
            "0",
            "65536",
            "70000",
            "99999999999999999999",
            "-1",
            "8080a",
            "abc",
            "80.5",
        ] {
            assert_eq!(
                parse_port(PortField::Local, value),
                Err(PortForwardError::InvalidPort {
                    field: PortField::Local,
                    value: value.to_string()
                }),
                "{value:?} is not a valid port"
            );
        }
    }

    #[test]
    fn test_port_error_messages_name_the_field_and_the_bad_value() {
        assert_eq!(
            PortForwardError::MissingPort {
                field: PortField::Local
            }
            .to_string(),
            "Local port is required."
        );
        assert_eq!(
            PortForwardError::InvalidPort {
                field: PortField::Remote,
                value: "70000".to_string()
            }
            .to_string(),
            "Remote port must be a number between 1 and 65535, got \"70000\"."
        );
        assert_eq!(
            PortForwardError::DuplicateLocalPort {
                local_port: 8080,
                connection: "dev-box".to_string()
            }
            .to_string(),
            "Local port 8080 is already forwarded on dev-box."
        );
    }

    #[test]
    fn test_normalize_host_treats_blank_as_unset_and_stores_ipv6_bare() {
        assert_eq!(normalize_host(""), None);
        assert_eq!(normalize_host("   \t"), None);
        assert_eq!(normalize_host("[]"), None);
        assert_eq!(normalize_host(" 127.0.0.1 "), Some("127.0.0.1".to_string()));
        assert_eq!(
            normalize_host("[::1]"),
            Some("::1".to_string()),
            "the brackets -L needs are added when the ssh command is built"
        );
        assert_eq!(
            normalize_host("fe80::1%en0"),
            Some("fe80::1%en0".to_string())
        );
    }

    #[test]
    fn test_validate_port_forward_builds_the_settings_entry() {
        assert_eq!(
            validate_port_forward(&draft("", "8080", "", "80"), "dev-box", &[], None),
            Ok(forward(None, 8080, None, 80))
        );
        assert_eq!(
            validate_port_forward(
                &draft("127.0.0.1", "8080", "0.0.0.0", "80"),
                "dev-box",
                &[],
                None
            ),
            Ok(forward(Some("127.0.0.1"), 8080, Some("0.0.0.0"), 80))
        );
    }

    #[test]
    fn test_validate_port_forward_accepts_ipv6_hosts() {
        assert_eq!(
            validate_port_forward(&draft("[::1]", "8080", "::1", "80"), "dev-box", &[], None),
            Ok(forward(Some("::1"), 8080, Some("::1"), 80)),
            "both bracketed and bare IPv6 literals are accepted and stored bare"
        );
    }

    #[test]
    fn test_validate_port_forward_rejects_a_duplicate_local_port() {
        let existing = vec![forward(None, 8080, None, 80)];
        assert_eq!(
            validate_port_forward(&draft("", "8080", "", "3000"), "dev-box", &existing, None),
            Err(PortForwardError::DuplicateLocalPort {
                local_port: 8080,
                connection: "dev-box".to_string()
            })
        );
        assert_eq!(
            validate_port_forward(&draft("", "8081", "", "3000"), "dev-box", &existing, None),
            Ok(forward(None, 8081, None, 3000)),
            "a free local port on the same connection is still accepted"
        );
    }

    #[test]
    fn test_bind_addresses_overlap_only_when_they_share_a_socket() {
        assert!(
            bind_addresses_overlap(None, Some("127.0.0.1")),
            "an unset local host is the loopback address ssh -L would bind"
        );
        assert!(
            !bind_addresses_overlap(Some("127.0.0.1"), Some("::1")),
            "IPv4 and IPv6 loopback are different bind addresses"
        );
        assert!(bind_addresses_overlap(Some("0.0.0.0"), Some("127.0.0.1")));
        assert!(bind_addresses_overlap(Some("::"), Some("::1")));
        assert!(bind_addresses_overlap(Some("0.0.0.0"), None));
        assert!(bind_addresses_overlap(Some("*"), Some("192.168.0.2")));
        assert!(bind_addresses_overlap(Some(" 127.0.0.1 "), None));
        assert!(
            bind_addresses_overlap(Some("localhost"), None),
            "an explicit localhost is the same socket as leaving the host unset"
        );
        assert!(!bind_addresses_overlap(
            Some("192.168.0.2"),
            Some("192.168.0.3")
        ));
    }

    #[test]
    fn test_validate_port_forward_allows_the_same_port_on_a_different_address() {
        let existing = vec![forward(Some("127.0.0.1"), 8080, None, 80)];
        assert_eq!(
            validate_port_forward(
                &draft("[::1]", "8080", "", "3000"),
                "dev-box",
                &existing,
                None
            ),
            Ok(forward(Some("::1"), 8080, None, 3000)),
            "127.0.0.1:8080 and [::1]:8080 are different bind addresses"
        );
    }

    #[test]
    fn test_validate_port_forward_rejects_a_port_taken_by_a_wildcard_bind() {
        let existing = vec![forward(Some("0.0.0.0"), 8080, None, 80)];
        assert_eq!(
            validate_port_forward(
                &draft("127.0.0.1", "8080", "", "3000"),
                "dev-box",
                &existing,
                None
            ),
            Err(PortForwardError::DuplicateLocalPort {
                local_port: 8080,
                connection: "dev-box".to_string()
            }),
            "a wildcard bind already owns every address on that port"
        );
    }

    #[test]
    fn test_validate_port_forward_lets_the_edited_entry_keep_its_own_port() {
        let existing = vec![forward(None, 8080, None, 80), forward(None, 9090, None, 90)];

        assert_eq!(
            validate_port_forward(
                &draft("", "8080", "", "8000"),
                "dev-box",
                &existing,
                Some(&existing[0])
            ),
            Ok(forward(None, 8080, None, 8000)),
            "keeping the local port while editing the same entry is not a duplicate"
        );

        assert_eq!(
            validate_port_forward(
                &draft("", "9090", "", "8000"),
                "dev-box",
                &existing,
                Some(&existing[0])
            ),
            Err(PortForwardError::DuplicateLocalPort {
                local_port: 9090,
                connection: "dev-box".to_string()
            }),
            "taking another entry's local port is still a duplicate"
        );
    }

    #[test]
    fn test_apply_port_forward_edit_replaces_in_place_or_appends() {
        let mut forwards = vec![forward(None, 8080, None, 80), forward(None, 9090, None, 90)];

        let original = forwards[0].clone();
        apply_port_forward_edit(
            &mut forwards,
            Some(&original),
            forward(None, 8080, None, 8000),
        );
        assert_eq!(
            forwards,
            vec![
                forward(None, 8080, None, 8000),
                forward(None, 9090, None, 90)
            ],
            "the edited entry keeps its position"
        );

        apply_port_forward_edit(&mut forwards, None, forward(None, 7070, None, 70));
        assert_eq!(forwards.len(), 3);
        assert_eq!(forwards[2], forward(None, 7070, None, 70));

        apply_port_forward_edit(
            &mut forwards,
            Some(&forward(None, 1234, None, 1234)),
            forward(None, 6060, None, 60),
        );
        assert_eq!(
            forwards.len(),
            4,
            "an entry that is no longer present is appended instead of dropped"
        );
        assert_eq!(forwards[3], forward(None, 6060, None, 60));
    }

    #[test]
    fn test_remove_port_forward_reports_whether_it_matched() {
        let mut forwards = vec![forward(None, 8080, None, 80), forward(None, 9090, None, 90)];

        assert!(remove_port_forward(
            &mut forwards,
            &forward(None, 8080, None, 80)
        ));
        assert_eq!(forwards, vec![forward(None, 9090, None, 90)]);

        assert!(
            !remove_port_forward(&mut forwards, &forward(None, 8080, None, 80)),
            "removing an entry that is already gone leaves the list alone"
        );
        assert_eq!(forwards, vec![forward(None, 9090, None, 90)]);
    }

    #[test]
    fn test_describe_port_forward_fills_in_localhost_and_brackets_ipv6() {
        assert_eq!(
            port_forward_endpoints(&forward(None, 8080, None, 80)),
            ("localhost:8080".to_string(), "localhost:80".to_string()),
            "an unset host on either side is the loopback each end defaults to"
        );
        assert_eq!(
            port_forward_endpoints(&forward(Some("127.0.0.1"), 8080, Some("10.0.0.5"), 80)),
            ("127.0.0.1:8080".to_string(), "10.0.0.5:80".to_string())
        );
        assert_eq!(
            port_forward_endpoints(&forward(Some("::1"), 8080, Some("::1"), 80)),
            ("[::1]:8080".to_string(), "[::1]:80".to_string()),
            "an IPv6 host is bracketed so the port is not read as part of it"
        );
        assert_eq!(
            describe_port_forward(&forward(None, 8080, None, 80)),
            "localhost:8080 on this machine → localhost:80 on the remote host",
            "the words are what the tooltip says the two icons mean"
        );
    }

    fn connection(host: &str, username: Option<&str>, port: Option<u16>) -> SshConnection {
        SshConnection {
            host: host.to_string(),
            username: username.map(str::to_string),
            port,
            ..SshConnection::default()
        }
    }

    #[test]
    fn test_connection_label_prefers_the_nickname() {
        let mut named = connection("example.com", Some("andy"), Some(2222));
        assert_eq!(
            connection_label(&ForwardConnection::Ssh(named.clone())),
            "andy@example.com:2222"
        );

        named.nickname = Some("  ".to_string());
        assert_eq!(
            connection_label(&ForwardConnection::Ssh(named.clone())),
            "andy@example.com:2222",
            "a blank nickname is not a name"
        );

        named.nickname = Some("dev-box".to_string());
        assert_eq!(connection_label(&ForwardConnection::Ssh(named)), "dev-box");

        assert_eq!(
            connection_label(&ForwardConnection::Ssh(connection(
                "example.com",
                None,
                None
            ))),
            "example.com"
        );
    }

    #[test]
    fn test_find_connection_matches_host_username_and_port() {
        let connections = vec![
            connection("example.com", Some("andy"), Some(2222)),
            connection("example.com", Some("root"), Some(2222)),
            connection("example.com", Some("andy"), None),
        ];

        assert_eq!(
            find_connection(&connections, &connections[1].connection_key()),
            Some(1),
            "a different username is a different connection"
        );
        assert_eq!(
            find_connection(&connections, &connections[2].connection_key()),
            Some(2),
            "an unset port is a different connection from an explicit one"
        );
        assert_eq!(
            find_connection(
                &connections,
                &connection("other.com", Some("andy"), Some(2222)).connection_key()
            ),
            None
        );
    }

    fn wsl_connection(distro_name: &str, user: Option<&str>) -> WslConnection {
        WslConnection {
            distro_name: distro_name.to_string(),
            user: user.map(str::to_string),
            ..WslConnection::default()
        }
    }

    fn dev_container_connection(name: &str, container_id: &str) -> DevContainerConnection {
        DevContainerConnection {
            name: name.to_string(),
            container_id: container_id.to_string(),
            remote_user: "vscode".to_string(),
            ..DevContainerConnection::default()
        }
    }

    fn docker_options(name: &str, container_id: &str) -> DockerConnectionOptions {
        DockerConnectionOptions {
            name: name.to_string(),
            container_id: container_id.to_string(),
            remote_user: "vscode".to_string(),
            upload_binary_over_docker_exec: false,
            use_podman: false,
            remote_env: BTreeMap::default(),
        }
    }

    #[test]
    fn test_connection_key_is_built_from_every_transport() {
        let ssh = connection_key_for_options(&RemoteConnectionOptions::Ssh(SshConnectionOptions {
            host: "example.com".into(),
            username: Some("andy".to_string()),
            port: Some(2222),
            ..SshConnectionOptions::default()
        }));
        assert_eq!(
            ssh,
            Some(ConnectionKey::Ssh {
                host: "example.com".to_string(),
                username: Some("andy".to_string()),
                port: Some(2222),
            })
        );

        let wsl = connection_key_for_options(&RemoteConnectionOptions::Wsl(WslConnectionOptions {
            distro_name: "Ubuntu".to_string(),
            user: Some("andy".to_string()),
        }));
        assert_eq!(
            wsl,
            Some(ConnectionKey::Wsl {
                distro_name: "Ubuntu".to_string(),
                user: Some("andy".to_string()),
            })
        );

        let dev_container = connection_key_for_options(&RemoteConnectionOptions::Docker(
            docker_options("web", "abc123"),
        ));
        assert_eq!(
            dev_container,
            Some(ConnectionKey::DevContainer {
                name: "web".to_string(),
            })
        );

        assert_ne!(ssh, wsl);
        assert_ne!(wsl, dev_container);
        assert_ne!(ssh, dev_container);
    }

    #[test]
    fn test_dev_container_key_survives_a_container_rebuild() {
        let before = connection_key_for_options(&RemoteConnectionOptions::Docker(docker_options(
            "web", "abc123",
        )));
        let after = connection_key_for_options(&RemoteConnectionOptions::Docker(docker_options(
            "web", "def456",
        )));
        assert_eq!(
            before, after,
            "`devcontainer up` hands out a fresh container id on every rebuild, \
             so keying on it would orphan the user's forwards"
        );

        let other_project = connection_key_for_options(&RemoteConnectionOptions::Docker(
            docker_options("api", "abc123"),
        ));
        assert_ne!(before, other_project);
    }

    #[test]
    fn test_connection_label_for_each_transport() {
        assert_eq!(
            connection_label(&ForwardConnection::Ssh(connection(
                "example.com",
                Some("andy"),
                Some(2222)
            ))),
            "andy@example.com:2222"
        );
        assert_eq!(
            connection_label(&ForwardConnection::Wsl(wsl_connection("Ubuntu", None))),
            "Ubuntu"
        );
        assert_eq!(
            connection_label(&ForwardConnection::Wsl(wsl_connection(
                "Ubuntu",
                Some("andy")
            ))),
            "andy@Ubuntu"
        );
        assert_eq!(
            connection_label(&ForwardConnection::DevContainer(dev_container_connection(
                "web", "abc123"
            ))),
            "web"
        );
    }

    #[test]
    fn test_find_connection_locates_entries_in_every_list() {
        let ssh_connections = vec![
            connection("example.com", Some("andy"), Some(2222)),
            connection("other.com", None, None),
        ];
        let wsl_connections = vec![
            wsl_connection("Ubuntu", None),
            wsl_connection("Ubuntu", Some("andy")),
        ];
        let dev_containers = vec![
            dev_container_connection("api", "aaa"),
            dev_container_connection("web", "bbb"),
        ];

        assert_eq!(
            find_connection(&ssh_connections, &ssh_connections[1].connection_key()),
            Some(1)
        );
        assert_eq!(
            find_connection(&wsl_connections, &wsl_connections[1].connection_key()),
            Some(1),
            "a wsl entry with a user is a different connection from one without"
        );
        assert_eq!(
            find_connection(&dev_containers, &dev_containers[1].connection_key()),
            Some(1)
        );
        assert_eq!(
            find_connection(
                &dev_containers,
                &ConnectionKey::DevContainer {
                    name: "missing".to_string(),
                }
            ),
            None
        );
        assert_eq!(
            find_connection(
                &ssh_connections,
                &ConnectionKey::Wsl {
                    distro_name: "example.com".to_string(),
                    user: Some("andy".to_string()),
                }
            ),
            None,
            "a wsl distro must never resolve against an ssh host of the same name"
        );
    }

    #[test]
    fn test_port_forwards_for_key_mut_routes_to_the_matching_list() {
        let mut remote = RemoteSettingsContent {
            ssh_connections: Some(vec![connection("example.com", Some("andy"), Some(2222))]),
            wsl_connections: Some(vec![wsl_connection("Ubuntu", None)]),
            dev_container_connections: Some(vec![dev_container_connection("web", "abc123")]),
            ..RemoteSettingsContent::default()
        };

        let ssh_key = ConnectionKey::Ssh {
            host: "example.com".to_string(),
            username: Some("andy".to_string()),
            port: Some(2222),
        };
        port_forwards_for_key_mut(&mut remote, &ssh_key, None)
            .expect("the ssh entry exists")
            .push(SshPortForwardOption {
                local_host: None,
                local_port: 8080,
                remote_host: None,
                remote_port: 80,
            });

        let wsl_key = ConnectionKey::Wsl {
            distro_name: "Ubuntu".to_string(),
            user: None,
        };
        port_forwards_for_key_mut(&mut remote, &wsl_key, None)
            .expect("the wsl entry exists")
            .push(SshPortForwardOption {
                local_host: None,
                local_port: 3000,
                remote_host: None,
                remote_port: 3000,
            });

        assert_eq!(
            remote.ssh_connections.as_ref().and_then(|connections| {
                connections
                    .first()
                    .and_then(|connection| connection.port_forwards.clone())
            }),
            Some(vec![SshPortForwardOption {
                local_host: None,
                local_port: 8080,
                remote_host: None,
                remote_port: 80,
            }])
        );
        assert_eq!(
            remote.wsl_connections.as_ref().and_then(|connections| {
                connections
                    .first()
                    .and_then(|connection| connection.port_forwards.clone())
            }),
            Some(vec![SshPortForwardOption {
                local_host: None,
                local_port: 3000,
                remote_host: None,
                remote_port: 3000,
            }])
        );
    }

    #[test]
    fn test_port_forwards_for_key_mut_creates_a_dev_container_entry_from_live_options() {
        let mut remote = RemoteSettingsContent::default();
        let key = ConnectionKey::DevContainer {
            name: "web".to_string(),
        };
        let options = docker_options("web", "abc123");

        assert!(
            port_forwards_for_key_mut(&mut remote, &key, None).is_none(),
            "without live connection options there is nothing to fill a new entry with"
        );
        assert_eq!(
            remote.dev_container_connections, None,
            "a refused write must not leave an empty list behind"
        );

        port_forwards_for_key_mut(&mut remote, &key, Some(&options))
            .expect("the entry is created from the live options")
            .push(SshPortForwardOption {
                local_host: None,
                local_port: 5173,
                remote_host: None,
                remote_port: 5173,
            });

        assert_eq!(
            remote.dev_container_connections,
            Some(vec![DevContainerConnection {
                name: "web".to_string(),
                container_id: "abc123".to_string(),
                remote_user: "vscode".to_string(),
                use_podman: false,
                extension_ids: Vec::new(),
                remote_env: BTreeMap::default(),
                port_forwards: Some(vec![SshPortForwardOption {
                    local_host: None,
                    local_port: 5173,
                    remote_host: None,
                    remote_port: 5173,
                }]),
            }]),
            "the created entry must carry the real container details, not placeholders"
        );

        port_forwards_for_key_mut(&mut remote, &key, Some(&options))
            .expect("the entry now exists")
            .push(SshPortForwardOption {
                local_host: None,
                local_port: 8080,
                remote_host: None,
                remote_port: 80,
            });
        assert_eq!(
            remote
                .dev_container_connections
                .as_ref()
                .map(|connections| connections.len()),
            Some(1),
            "a second forward must reuse the entry rather than duplicate it"
        );
    }

    #[test]
    fn test_an_existing_dev_container_can_be_added_to_without_a_live_connection() {
        let remote = RemoteSettingsContent {
            dev_container_connections: Some(vec![dev_container_connection("web", "abc123")]),
            ..RemoteSettingsContent::default()
        };
        let key = ConnectionKey::DevContainer {
            name: "web".to_string(),
        };

        assert!(
            can_add_forward(Some(&remote), &key, None),
            "the entry is already in the user settings file, so no live container details are needed to add another forward to it"
        );
        assert!(
            can_add_forward(Some(&remote), &key, Some(&docker_options("api", "aaa"))),
            "being connected to a different dev container must not take the Add button away from this one"
        );
        assert!(
            connection_is_editable(Some(&remote), &key, None),
            "add and edit must agree: an existing entry is editable either way"
        );

        let without_entry = ConnectionKey::DevContainer {
            name: "api".to_string(),
        };
        assert!(
            !can_add_forward(Some(&remote), &without_entry, None),
            "a dev container with no entry and no live connection still has nothing to record"
        );
        assert!(
            can_add_forward(
                Some(&remote),
                &without_entry,
                Some(&docker_options("api", "aaa"))
            ),
            "the live connection supplies the details a first entry needs"
        );
    }

    #[test]
    fn test_ssh_and_wsl_rows_always_offer_add() {
        let remote = RemoteSettingsContent::default();
        let ssh_key = ConnectionKey::Ssh {
            host: "example.com".to_string(),
            username: None,
            port: None,
        };
        let wsl_key = ConnectionKey::Wsl {
            distro_name: "Ubuntu".to_string(),
            user: None,
        };

        assert!(
            can_add_forward(Some(&remote), &ssh_key, None),
            "an ssh row offers Add whether or not the file has an entry for it yet"
        );
        assert!(can_add_forward(Some(&remote), &wsl_key, None));
        // The rule this used to assert was the opposite: Add was offered and the save
        // then refused, because the entry did not exist. The entry is now written from
        // the key, which is what makes a connection opened from a URI or an ssh config
        // host forwardable at all.
        assert!(
            connection_is_editable(Some(&remote), &ssh_key, None),
            "the key carries the host, the user and the port, so the entry can be written"
        );
        assert!(connection_is_editable(Some(&remote), &wsl_key, None));
    }

    /// A connection opened from a URI or an ssh config host has no settings entry, and
    /// refusing to write one left the reader with a notification offering a forward, a
    /// click that did nothing, and a panel that did not mention the host they were on.
    #[test]
    fn test_a_forward_for_a_connection_with_no_entry_writes_one() {
        let mut remote = RemoteSettingsContent::default();
        let key = ConnectionKey::Ssh {
            host: "coder-vscode.coder.elggum.com--poi5305--andy.main".to_string(),
            username: None,
            port: None,
        };

        port_forwards_for_key_mut(&mut remote, &key, None)
            .unwrap_or_else(|| panic!("an ssh entry is written from the key"))
            .push(forward(None, 3000, None, 3000));

        let written = remote
            .ssh_connections
            .as_deref()
            .unwrap_or_else(|| panic!("expected the ssh_connections list to exist"));
        assert_eq!(written.len(), 1);
        assert_eq!(
            written
                .first()
                .map(|connection| connection.connection_key()),
            Some(key.clone()),
            "the entry addresses the same connection the forward was recorded for"
        );
        assert_eq!(
            written
                .first()
                .and_then(|connection| connection.port_forwards.clone()),
            Some(vec![forward(None, 3000, None, 3000)])
        );

        // A second forward for the same connection joins the entry that is now there
        // rather than adding a duplicate one beside it.
        port_forwards_for_key_mut(&mut remote, &key, None)
            .unwrap_or_else(|| panic!("the entry is found the second time"))
            .push(forward(None, 8080, None, 80));
        assert_eq!(
            remote.ssh_connections.as_ref().map(Vec::len),
            Some(1),
            "one connection, two forwards"
        );
        assert_eq!(
            remote
                .ssh_connections
                .as_ref()
                .and_then(|connections| connections.first())
                .and_then(|connection| connection.port_forwards.as_ref())
                .map(Vec::len),
            Some(2)
        );
    }

    /// The row for a connection with no entry has no nickname to take a label from.
    #[test]
    fn test_a_connection_with_no_entry_is_still_named_in_the_panel() {
        assert_eq!(
            connection_label_for_key(&ConnectionKey::Ssh {
                host: "example.com".to_string(),
                username: Some("andy".to_string()),
                port: Some(2222),
            }),
            "andy@example.com:2222"
        );
        assert_eq!(
            connection_label_for_key(&ConnectionKey::Wsl {
                distro_name: "Ubuntu-22.04".to_string(),
                user: None,
            }),
            "Ubuntu-22.04"
        );
    }

    #[test]
    fn test_port_forwards_for_key_mut_refuses_options_from_another_dev_container() {
        let mut remote = RemoteSettingsContent::default();
        let key = ConnectionKey::DevContainer {
            name: "web".to_string(),
        };

        assert!(
            port_forwards_for_key_mut(&mut remote, &key, Some(&docker_options("api", "aaa")))
                .is_none(),
            "the connected container is not the one being edited, so there is nothing to create the entry from"
        );
        assert_eq!(
            remote.dev_container_connections, None,
            "a refused write must not leave an entry under the wrong name behind"
        );
    }

    #[test]
    fn test_choose_local_port_keeps_the_detected_port_when_it_is_free() {
        let mut probed = Vec::new();
        assert_eq!(
            choose_local_port(3000, &[], |candidate| {
                probed.push(candidate);
                true
            }),
            Some(3000),
            "a detected port nothing is holding is published under its own number"
        );
        assert_eq!(probed, vec![3000], "no port above it is even probed");
    }

    #[test]
    fn test_choose_local_port_skips_ports_the_connection_already_forwards() {
        let configured = [
            forward(None, 3000, None, 3000),
            forward(Some("0.0.0.0"), 3001, None, 9001),
        ];
        let mut probed = Vec::new();
        assert_eq!(
            choose_local_port(3000, &configured, |candidate| {
                probed.push(candidate);
                true
            }),
            Some(3002),
            "3000 is taken by an existing forward and 3001 by a wildcard bind that covers it"
        );
        assert_eq!(
            probed,
            vec![3002],
            "a port that is already configured is skipped without being probed"
        );
    }

    #[test]
    fn test_choose_local_port_walks_up_past_ports_that_are_in_use() {
        let mut probed = Vec::new();
        assert_eq!(
            choose_local_port(8080, &[], |candidate| {
                probed.push(candidate);
                candidate >= 8083
            }),
            Some(8083)
        );
        assert_eq!(probed, vec![8080, 8081, 8082, 8083]);
    }

    #[test]
    fn test_choose_local_port_gives_up_at_the_search_limit() {
        let mut probed = Vec::new();
        assert_eq!(
            choose_local_port(4000, &[], |candidate| {
                probed.push(candidate);
                false
            }),
            None,
            "nothing in the searched range could be bound"
        );
        assert_eq!(
            probed.len(),
            usize::from(LOCAL_PORT_SEARCH_LIMIT),
            "the search examines exactly {LOCAL_PORT_SEARCH_LIMIT} port numbers"
        );
        assert_eq!(probed.first().copied(), Some(4000));
        assert_eq!(
            probed.last().copied(),
            Some(4000 + LOCAL_PORT_SEARCH_LIMIT - 1)
        );
    }

    #[test]
    fn test_choose_local_port_does_not_reject_the_inputs_the_limits_could_misfire_on() {
        assert_eq!(
            choose_local_port(8080, &[forward(Some("::1"), 8080, None, 8080)], |_| true),
            Some(8080),
            "a forward bound to a different address does not claim this socket"
        );
        assert_eq!(
            choose_local_port(65535, &[], |candidate| candidate == 65535),
            Some(65535),
            "the last port is a legal choice"
        );

        let mut probed = Vec::new();
        assert_eq!(
            choose_local_port(65530, &[], |candidate| {
                probed.push(candidate);
                false
            }),
            None,
            "a search that runs out of ports reports failure"
        );
        assert_eq!(
            probed,
            vec![65530, 65531, 65532, 65533, 65534, 65535],
            "the search stops at the end of the range instead of wrapping to a low port"
        );
    }
}
