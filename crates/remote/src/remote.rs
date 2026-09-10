pub mod claude_sessions;
pub mod json_log;
pub mod listening_ports;
pub mod port_forward;
pub mod protocol;
pub mod proxy;
pub mod remote_client;
pub mod remote_identity;
pub mod tmux_sessions;
mod transport;

pub use listening_ports::{ListeningPort, ScanTimings, is_forwardable_host, scan_listening_ports};
pub use port_forward::{
    PORT_TUNNEL_CHUNK_SIZE, PORT_TUNNEL_WINDOW_SIZE, PortForwardStatus, PortForwardStore,
    TunnelFlowControl,
};
#[cfg(target_os = "windows")]
pub use remote_client::OpenWslPath;
pub use remote_client::{
    CommandTemplate, ConnectionIdentifier, ConnectionState, Interactive, RemoteArch, RemoteClient,
    RemoteClientDelegate, RemoteClientEvent, RemoteConnection, RemoteConnectionOptions, RemoteOs,
    RemotePlatform, connect, has_active_connection,
};
pub use remote_identity::{
    RemoteConnectionIdentity, remote_connection_identity, same_remote_connection_identity,
};
pub use transport::docker::DockerConnectionOptions;
pub use transport::ssh::{SshConnectionOptions, SshPortForwardOption};
pub use transport::wsl::WslConnectionOptions;
#[cfg(target_os = "windows")]
pub use transport::wsl::wsl_path_to_windows_path;

#[cfg(any(test, feature = "test-support"))]
pub use transport::mock::{
    MockConnection, MockConnectionOptions, MockConnectionRegistry, MockDelegate,
};
