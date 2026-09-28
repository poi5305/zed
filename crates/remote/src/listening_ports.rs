//! Discovery of the TCP ports that are listening on the machine this code runs
//! on. The parsers are deliberately free of IO so that every platform's format
//! can be tested from a captured sample, and the IO wrappers do nothing but
//! read a file or run a command and hand the text to a parser.
//!
//! The formats and the polling strategy follow VS Code's
//! `extHostTunnelService.ts`, which solves the same problem from inside the
//! remote server.

use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};

use crate::claude_sessions::RegisteredSession;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ListeningPort {
    pub host: String,
    pub port: u16,
}

/// A listening port together with the process whose socket it is, when the
/// platform's listing names one.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ListeningSocket {
    pub port: ListeningPort,
    pub process_id: Option<u32>,
}

/// The state `/proc/net/tcp` uses for `TCP_LISTEN`.
const TCP_LISTEN_STATE: &str = "0A";

/// `tx_queue:rx_queue` and `tr:tm->when` are each a single colon-joined column
/// in the rows but two names in the header, so dropping the second name of each
/// pair is what keeps the header and the rows aligned.
const PROC_NET_TCP_HEADER_ONLY_NAMES: [&str; 2] = ["rx_queue", "tm->when"];

/// Expands the little-endian hex address of a `/proc/net/tcp` row.
///
/// An IPv4 address is 8 hex characters holding the four bytes in reverse, and
/// an IPv6 address is four 8-character words whose bytes are reversed within
/// each half-word. Returns `None` for anything that is not one of those two
/// shapes, so that a truncated or corrupt row is skipped rather than guessed at.
pub fn parse_ip_address(hex: &str) -> Option<String> {
    if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }

    let mut result = String::new();
    if hex.len() == 8 {
        let mut index = hex.len();
        while index >= 2 {
            let byte = u8::from_str_radix(hex.get(index - 2..index)?, 16).ok()?;
            result.push_str(&byte.to_string());
            if index != 2 {
                result.push('.');
            }
            index -= 2;
        }
        return Some(result);
    }

    if !hex.len().is_multiple_of(8) || hex.is_empty() {
        return None;
    }

    for word_start in (0..hex.len()).step_by(8) {
        let word = hex.get(word_start..word_start + 8)?;
        for half in [1usize, 0] {
            let high = word.get(half * 4 + 2..half * 4 + 4)?;
            let low = word.get(half * 4..half * 4 + 2)?;
            let group = u16::from_str_radix(&format!("{high}{low}"), 16).ok()?;
            result.push_str(&format!("{group:x}"));
            // The last group of the last word is the only one not followed by a
            // separator; every other group, including the first of each word, is.
            let is_last = word_start + 8 == hex.len() && half == 0;
            if !is_last {
                result.push(':');
            }
        }
    }
    Some(result)
}

/// Parses one `/proc/net/tcp` or `/proc/net/tcp6` file. Rows that are truncated,
/// short of columns, or not hexadecimal are dropped individually so that a
/// single corrupt row cannot hide the rows after it.
pub fn parse_proc_net_tcp(contents: &str) -> Vec<ListeningPort> {
    parse_proc_net_tcp_with_inodes(contents)
        .into_iter()
        .map(|(port, _)| port)
        .collect()
}

/// Like [`parse_proc_net_tcp`], but also returns each row's socket inode, which
/// is what ties the socket to the process holding it. An inode of `0`, or a
/// row without the column, has no owner to find.
pub fn parse_proc_net_tcp_with_inodes(contents: &str) -> Vec<(ListeningPort, Option<u64>)> {
    let mut lines = contents.trim().lines();
    let Some(header) = lines.next() else {
        return Vec::new();
    };
    let names: Vec<&str> = header
        .split_whitespace()
        .filter(|name| !PROC_NET_TCP_HEADER_ONLY_NAMES.contains(name))
        .collect();
    let Some(state_column) = names.iter().position(|name| *name == "st") else {
        return Vec::new();
    };
    let Some(address_column) = names.iter().position(|name| *name == "local_address") else {
        return Vec::new();
    };
    let inode_column = names.iter().position(|name| *name == "inode");

    let mut ports = Vec::new();
    let mut seen = HashSet::default();
    for line in lines {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.get(state_column) != Some(&TCP_LISTEN_STATE) {
            continue;
        }
        let Some(address) = fields.get(address_column) else {
            continue;
        };
        let Some((ip, port)) = address.split_once(':') else {
            continue;
        };
        let Ok(port) = u16::from_str_radix(port, 16) else {
            continue;
        };
        let Some(host) = parse_ip_address(ip) else {
            continue;
        };
        if seen.insert((host.clone(), port)) {
            let inode = inode_column
                .and_then(|column| fields.get(column))
                .and_then(|inode| inode.parse::<u64>().ok())
                .filter(|inode| *inode != 0);
            ports.push((ListeningPort { host, port }, inode));
        }
    }
    ports
}

/// Reads the inode out of a `/proc/<pid>/fd/<n>` link target, which for a
/// socket is spelled `socket:[<inode>]`.
pub fn parse_socket_link_inode(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

/// Parses `lsof -nP -iTCP -sTCP:LISTEN`, whose rows end in
/// `TCP <address> (LISTEN)`.
pub fn parse_lsof_output(contents: &str) -> Vec<ListeningPort> {
    parse_lsof_output_with_pids(contents)
        .into_iter()
        .map(|socket| socket.port)
        .collect()
}

/// Like [`parse_lsof_output`], but keeps the PID from each row's second column.
pub fn parse_lsof_output_with_pids(contents: &str) -> Vec<ListeningSocket> {
    let mut ports = Vec::new();
    let mut seen = HashSet::default();
    for line in contents.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(state_index) = fields.iter().position(|field| *field == "(LISTEN)") else {
            continue;
        };
        let Some(address) = state_index
            .checked_sub(1)
            .and_then(|index| fields.get(index))
        else {
            continue;
        };
        let Some(port) = parse_host_and_port(address) else {
            continue;
        };
        let process_id = fields.get(1).and_then(|pid| pid.parse::<u32>().ok());
        if seen.insert(port.clone()) {
            ports.push(ListeningSocket { port, process_id });
        }
    }
    ports
}

/// Parses `netstat -ano`, whose TCP rows are
/// `TCP <local> <remote> LISTENING <pid>`.
pub fn parse_netstat_output(contents: &str) -> Vec<ListeningPort> {
    parse_netstat_output_with_pids(contents)
        .into_iter()
        .map(|socket| socket.port)
        .collect()
}

/// Like [`parse_netstat_output`], but keeps the PID from each row's last
/// column. PID 0 is the idle process, which owns nothing a user started.
pub fn parse_netstat_output_with_pids(contents: &str) -> Vec<ListeningSocket> {
    let mut ports = Vec::new();
    let mut seen = HashSet::default();
    for line in contents.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(protocol) = fields.first() else {
            continue;
        };
        if !protocol.eq_ignore_ascii_case("tcp") && !protocol.eq_ignore_ascii_case("tcp6") {
            continue;
        }
        if !fields
            .iter()
            .any(|field| field.eq_ignore_ascii_case("LISTENING"))
        {
            continue;
        }
        let Some(address) = fields.get(1) else {
            continue;
        };
        let Some(port) = parse_host_and_port(address) else {
            continue;
        };
        let process_id = fields
            .last()
            .and_then(|pid| pid.parse::<u32>().ok())
            .filter(|pid| *pid != 0);
        if seen.insert(port.clone()) {
            ports.push(ListeningSocket { port, process_id });
        }
    }
    ports
}

/// Splits the `host:port` form both `lsof` and `netstat` print. The host may be
/// bracketed (`[::1]:3000`) or the wildcard `*`, which means every interface.
fn parse_host_and_port(address: &str) -> Option<ListeningPort> {
    let (host, port) = address.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    if port == 0 {
        return None;
    }
    let host = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    let host = if host == "*" || host.is_empty() {
        "0.0.0.0"
    } else {
        host
    };
    Some(ListeningPort {
        host: host.to_string(),
        port,
    })
}

/// `parse_ip_address` expands IPv6 addresses rather than compressing them, so
/// the expanded spelling of the loopback address has to be recognised too.
pub fn is_localhost(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "0:0:0:0:0:0:0:1")
}

pub fn is_all_interfaces(host: &str) -> bool {
    matches!(host, "0.0.0.0" | "::" | "0:0:0:0:0:0:0:0")
}

/// A port reachable from the remote host's own loopback is the only kind worth
/// offering to forward.
pub fn is_forwardable_host(host: &str) -> bool {
    is_localhost(host) || is_all_interfaces(host)
}

/// Reads the listening ports of the machine this is running on.
pub async fn scan_listening_ports() -> Result<Vec<ListeningPort>> {
    Ok(scan_listening_sockets()
        .await?
        .into_iter()
        .map(|socket| socket.port)
        .collect())
}

/// Reads the listening ports of the machine this is running on, with the
/// process that owns each one where it can be seen.
pub async fn scan_listening_sockets() -> Result<Vec<ListeningSocket>> {
    let mut sockets = platform_scan().await?;
    sockets.retain(|socket| is_forwardable_host(&socket.port.host));
    sockets.sort();
    sockets.dedup_by(|later, earlier| later.port == earlier.port);
    Ok(sockets)
}

/// Merges the optional `/proc/net/tcp` and `tcp6` reads.
///
/// `NotFound` means that file is absent (no IPv6, or the container hid it),
/// which is not a failed scan when the other file was actually read, an
/// empty table included. Any other error can hide listeners, so an empty
/// result is still a failure, as is every file missing.
#[cfg(any(target_os = "linux", test))]
fn merge_optional_reads<T>(
    reads: impl IntoIterator<Item = (&'static str, std::io::Result<Vec<T>>)>,
) -> Result<Vec<T>> {
    let mut ports = Vec::new();
    let mut any_success = false;
    let mut missing_path = None;
    let mut failed_read = None;
    for (path, read) in reads {
        match read {
            Ok(rows) => {
                any_success = true;
                ports.extend(rows);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing_path = Some(path);
            }
            Err(error) => failed_read = Some((path, error)),
        }
    }
    if ports.is_empty()
        && let Some((path, error)) = failed_read
    {
        return Err(error).with_context(|| format!("could not read {path}"));
    }
    if !any_success && let Some(path) = missing_path {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no listening-port table could be read",
        ))
        .with_context(|| format!("could not read {path}"));
    }
    Ok(ports)
}

#[cfg(target_os = "linux")]
async fn platform_scan() -> Result<Vec<ListeningSocket>> {
    // Both files are optional: a kernel built without IPv6 has no `tcp6`, and
    // a container may hide either, which is not a reason to report nothing.
    let ports = merge_optional_reads([
        (
            "/proc/net/tcp",
            std::fs::read_to_string("/proc/net/tcp")
                .map(|contents| parse_proc_net_tcp_with_inodes(&contents)),
        ),
        (
            "/proc/net/tcp6",
            std::fs::read_to_string("/proc/net/tcp6")
                .map(|contents| parse_proc_net_tcp_with_inodes(&contents)),
        ),
    ])?;

    let inodes: HashSet<u64> = ports.iter().filter_map(|(_, inode)| *inode).collect();
    let owners = socket_inode_owners(&inodes);
    Ok(ports
        .into_iter()
        .map(|(port, inode)| ListeningSocket {
            process_id: inode.and_then(|inode| owners.get(&inode).copied()),
            port,
        })
        .collect())
}

/// Finds which process holds each socket by reading every `/proc/<pid>/fd`
/// link. Another user's descriptors cannot be read without privileges, and a
/// process can exit mid-walk, so each unreadable entry is skipped on its own and
/// its sockets are simply left without an owner.
#[cfg(target_os = "linux")]
fn socket_inode_owners(inodes: &HashSet<u64>) -> HashMap<u64, u32> {
    let mut owners = HashMap::default();
    if inodes.is_empty() {
        return owners;
    }
    let Ok(processes) = std::fs::read_dir("/proc") else {
        return owners;
    };
    for process in processes.flatten() {
        let Some(process_id) = process
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(descriptors) = std::fs::read_dir(process.path().join("fd")) else {
            continue;
        };
        for descriptor in descriptors.flatten() {
            let Ok(target) = std::fs::read_link(descriptor.path()) else {
                continue;
            };
            let Some(inode) = target.to_str().and_then(parse_socket_link_inode) else {
                continue;
            };
            if inodes.contains(&inode) {
                owners.entry(inode).or_insert(process_id);
            }
        }
        if owners.len() == inodes.len() {
            break;
        }
    }
    owners
}

#[cfg(any(target_os = "macos", target_os = "freebsd"))]
async fn platform_scan() -> Result<Vec<ListeningSocket>> {
    let output = util::command::new_command("lsof")
        .args(["-nP", "-iTCP", "-sTCP:LISTEN"])
        .output()
        .await
        .context("could not run lsof")?;
    // `lsof` exits non-zero when some file descriptors could not be inspected,
    // which is the normal case for an unprivileged process, so the exit status
    // is not a reason to discard the rows it did print.
    Ok(parse_lsof_output_with_pids(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

#[cfg(target_os = "windows")]
async fn platform_scan() -> Result<Vec<ListeningSocket>> {
    let output = util::command::new_command("netstat")
        .arg("-ano")
        .output()
        .await
        .context("could not run netstat")?;
    Ok(parse_netstat_output_with_pids(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "windows"
)))]
async fn platform_scan() -> Result<Vec<ListeningSocket>> {
    anyhow::bail!("listening port detection is not implemented for this platform")
}

/// What is known about one process when attributing a port to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessDetails {
    pub parent_process_id: Option<u32>,
    pub name: String,
    pub working_directory: Option<PathBuf>,
}

/// Who opened a listening port, and whether that makes it this project's.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct PortOwner {
    pub process_id: Option<u32>,
    pub process_name: Option<String>,
    pub in_project: bool,
    pub claude_session_id: Option<String>,
    pub claude_session_name: Option<String>,
}

/// Far deeper than any real process tree, so the bound only ever stops a
/// parent chain that loops.
pub const MAX_PARENT_HOPS: usize = 64;

/// A directory as stored, plus its canonical path when that spelling differs.
///
/// Process working directories from sysinfo and `/proc/<pid>/cwd` are already
/// resolved. Worktree roots and Claude registration directories are the paths
/// the user opened, which may still go through a symlink (`/tmp` to
/// `/private/tmp`, `/home` to `/data/home`). A match on either spelling counts.
/// Canonicalize failing leaves only the raw path.
struct ResolvedDirectory {
    raw: PathBuf,
    canonical: Option<PathBuf>,
}

fn resolved_directory(path: &Path) -> ResolvedDirectory {
    let canonical = std::fs::canonicalize(path)
        .ok()
        .filter(|canonical| canonical != path);
    ResolvedDirectory {
        raw: path.to_path_buf(),
        canonical,
    }
}

fn path_is_inside(path: &Path, root: &ResolvedDirectory) -> bool {
    path.starts_with(&root.raw)
        || root
            .canonical
            .as_ref()
            .is_some_and(|canonical| path.starts_with(canonical))
}

fn directory_is_inside(directory: &Path, roots: &[ResolvedDirectory]) -> bool {
    roots.iter().any(|root| path_is_inside(directory, root))
}

/// Registration directories are not pre-resolved, so both the stored path and
/// its canonical path are compared. Roots are resolved once by the caller.
fn registered_directory_is_inside(directory: &Path, roots: &[ResolvedDirectory]) -> bool {
    if directory_is_inside(directory, roots) {
        return true;
    }
    std::fs::canonicalize(directory)
        .ok()
        .is_some_and(|canonical| directory_is_inside(&canonical, roots))
}

/// Set on every remote terminal a Zed client spawns, to that client's
/// connection identifier, so that the remote server can tell which window's
/// terminals a listening process descends from.
pub const REMOTE_CONNECTION_ID_ENV_VAR: &str = "ZED_REMOTE_CONNECTION_ID";

/// Reads [`REMOTE_CONNECTION_ID_ENV_VAR`] out of a NUL-separated environment
/// block, as `/proc/<pid>/environ` spells it.
pub fn connection_marker_in_environ(environ: &[u8]) -> Option<String> {
    let prefix = format!("{REMOTE_CONNECTION_ID_ENV_VAR}=");
    environ.split(|byte| *byte == 0).find_map(|entry| {
        entry
            .strip_prefix(prefix.as_bytes())
            .and_then(|value| std::str::from_utf8(value).ok())
            .map(str::to_string)
    })
}

/// The connection that asked for the scan, and how to read the
/// [`REMOTE_CONNECTION_ID_ENV_VAR`] of a process. The lookup returns `None`
/// when the variable is unset or the environment cannot be read.
pub struct ConnectionMarker<'a> {
    pub connection_id: &'a str,
    pub marker_of: &'a mut dyn FnMut(u32) -> Option<String>,
}

struct PreparedAttribution<'a> {
    roots: Vec<ResolvedDirectory>,
    registrations: &'a [RegisteredSession],
    session_inside_project: Vec<bool>,
}

/// Whether a process or one of its ancestors was spawned by a terminal of the
/// connection in `marker`. Each process's environment is read at most once
/// per scan, since sockets usually share most of their parent chain.
fn descends_from_marked_terminal(
    process_id: u32,
    processes: &HashMap<u32, ProcessDetails>,
    marker: &mut ConnectionMarker,
    marked: &mut HashMap<u32, bool>,
) -> bool {
    let mut visited = HashSet::default();
    let mut current = Some(process_id);
    while let Some(process_id) = current {
        if visited.len() >= MAX_PARENT_HOPS || !visited.insert(process_id) {
            return false;
        }
        let is_marked = *marked.entry(process_id).or_insert_with(|| {
            (marker.marker_of)(process_id).as_deref() == Some(marker.connection_id)
        });
        if is_marked {
            return true;
        }
        current = processes
            .get(&process_id)
            .and_then(|process| process.parent_process_id);
    }
    false
}

impl<'a> PreparedAttribution<'a> {
    fn prepare(registrations: &'a [RegisteredSession], worktree_roots: &[PathBuf]) -> Self {
        let roots = worktree_roots
            .iter()
            .map(|root| resolved_directory(root))
            .collect::<Vec<_>>();
        let session_inside_project = registrations
            .iter()
            .map(|session| registered_directory_is_inside(&session.working_directory, &roots))
            .collect();
        Self {
            roots,
            registrations,
            session_inside_project,
        }
    }

    /// With a `marker`, a port is the project's only when it descends from one
    /// of the asking connection's terminals or from a Claude session inside a
    /// worktree; the owner's own working directory no longer counts, because
    /// anything started from inside the project directory would pass it.
    /// Without one (an older client), the working directory still counts.
    fn attribute(
        &self,
        process_id: Option<u32>,
        processes: &HashMap<u32, ProcessDetails>,
        marker: Option<(&mut ConnectionMarker, &mut HashMap<u32, bool>)>,
    ) -> PortOwner {
        let Some(process_id) = process_id else {
            return PortOwner::default();
        };
        let process = processes.get(&process_id);
        let session_index = nearest_session_index(process_id, processes, self.registrations);
        let session = session_index.and_then(|index| self.registrations.get(index));
        let session_inside = session_index
            .and_then(|index| self.session_inside_project.get(index).copied())
            .unwrap_or(false);
        let in_project = session_inside
            || match marker {
                Some((marker, marked)) => {
                    descends_from_marked_terminal(process_id, processes, marker, marked)
                }
                None => process
                    .and_then(|process| process.working_directory.as_deref())
                    .is_some_and(|directory| directory_is_inside(directory, &self.roots)),
            };

        PortOwner {
            process_id: Some(process_id),
            process_name: process.map(|process| process.name.clone()),
            in_project,
            claude_session_id: session.map(|session| session.session_id.clone()),
            claude_session_name: session.and_then(|session| session.name.clone()),
        }
    }
}

/// Decides who owns a port and whether it belongs to the project whose
/// worktrees are `worktree_roots`: either the owning process runs inside one
/// of them, or it descends from a Claude Code session that does. A port whose
/// owner cannot be seen and has no such ancestor is not the project's.
pub fn attribute_port_owner(
    process_id: Option<u32>,
    processes: &HashMap<u32, ProcessDetails>,
    registrations: &[RegisteredSession],
    worktree_roots: &[PathBuf],
) -> PortOwner {
    PreparedAttribution::prepare(registrations, worktree_roots).attribute(
        process_id,
        processes,
        None,
    )
}

/// Attributes every socket from one scan. Worktree roots and registration
/// directories are canonicalized once here, on the caller's thread, which the
/// remote server keeps off the session thread. `marker` is the asking
/// connection's, when the client sent one.
pub fn attribute_listening_sockets(
    sockets: Vec<ListeningSocket>,
    processes: &HashMap<u32, ProcessDetails>,
    registrations: &[RegisteredSession],
    worktree_roots: &[PathBuf],
    mut marker: Option<ConnectionMarker>,
) -> Vec<(ListeningPort, PortOwner)> {
    let prepared = PreparedAttribution::prepare(registrations, worktree_roots);
    let mut marked = HashMap::default();
    sockets
        .into_iter()
        .map(|socket| {
            let owner = prepared.attribute(
                socket.process_id,
                processes,
                marker.as_mut().map(|marker| (marker, &mut marked)),
            );
            (socket.port, owner)
        })
        .collect()
}

/// Linux's default `net.ipv4.ip_local_port_range`.
pub const LINUX_DEFAULT_EPHEMERAL_PORTS: RangeInclusive<u16> = 32768..=60999;

/// The IANA dynamic range, which macOS and Windows use for ephemeral ports.
pub const IANA_EPHEMERAL_PORTS: RangeInclusive<u16> = 49152..=65535;

/// Parses `/proc/sys/net/ipv4/ip_local_port_range`, two whitespace-separated
/// numbers.
pub fn parse_ip_local_port_range(contents: &str) -> Option<RangeInclusive<u16>> {
    let mut numbers = contents.split_whitespace().map(str::parse::<u16>);
    let low = numbers.next()?.ok()?;
    let high = numbers.next()?.ok()?;
    (low <= high).then_some(low..=high)
}

/// The range the operating system picks from when a program binds port 0.
pub fn ephemeral_port_range() -> RangeInclusive<u16> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
            .ok()
            .and_then(|contents| parse_ip_local_port_range(&contents))
            .unwrap_or(LINUX_DEFAULT_EPHEMERAL_PORTS)
    }
    #[cfg(not(target_os = "linux"))]
    {
        IANA_EPHEMERAL_PORTS
    }
}

/// Browser process names, compared whole and without case. A browser's
/// listening socket is its remote-debugging endpoint (an MCP driving Chrome
/// opens one per page), never a server the user wants to open. Whole names
/// rather than prefixes, so a dev tool whose name starts with "chrom" is not
/// caught. Linux reports at most 15 bytes of a name, hence the truncated forms.
const BROWSER_PROCESS_NAMES: &[&str] = &[
    "chrome",
    "chromium",
    "chromium-browser",
    "chromium-browse",
    "chrome-headless-shell",
    "chrome-headless",
    "headless_shell",
    "chrome_crashpad_handler",
    "chrome_crashpad",
    "google chrome",
    "google-chrome",
    "msedge",
    "microsoft edge",
    "firefox",
    "firefox-bin",
    "firefox-esr",
];

/// Whether a port the project opened should still be offered: not when the
/// operating system chose it (a program that binds port 0 is talking to
/// itself, not serving anything a person would open) and not when a browser
/// holds it.
pub fn is_worth_offering(
    port: u16,
    process_name: Option<&str>,
    ephemeral_ports: &RangeInclusive<u16>,
) -> bool {
    if ephemeral_ports.contains(&port) {
        return false;
    }
    !process_name.is_some_and(|name| {
        BROWSER_PROCESS_NAMES
            .iter()
            .any(|browser| name.eq_ignore_ascii_case(browser))
    })
}

fn nearest_session_index(
    process_id: u32,
    processes: &HashMap<u32, ProcessDetails>,
    registrations: &[RegisteredSession],
) -> Option<usize> {
    let mut visited = HashSet::default();
    let mut current = Some(process_id);
    while let Some(process_id) = current {
        if visited.len() >= MAX_PARENT_HOPS || !visited.insert(process_id) {
            return None;
        }
        if let Some(index) = registrations
            .iter()
            .position(|session| session.process_id == process_id)
        {
            return Some(index);
        }
        current = processes
            .get(&process_id)
            .and_then(|process| process.parent_process_id);
    }
    None
}

/// The floor VS Code puts under the scan interval.
pub const MINIMUM_SCAN_INTERVAL: Duration = Duration::from_millis(2000);

/// How many times slower than the scan itself the polling loop should be, so
/// that a slow machine is not spent scanning.
pub const SCAN_INTERVAL_MULTIPLIER: u32 = 20;

/// VS Code's `if (scanCount++ > 3)`: the first four scans are warm-up and are
/// left out of the average because they are the slow ones.
pub const SCANS_EXCLUDED_FROM_AVERAGE: usize = 4;

/// The cumulative average of how long a scan takes, which is what sets the
/// interval between scans.
#[derive(Debug, Default)]
pub struct ScanTimings {
    scans: usize,
    samples: u32,
    average_millis: f64,
}

impl ScanTimings {
    pub fn record(&mut self, duration: Duration) {
        self.scans += 1;
        if self.scans <= SCANS_EXCLUDED_FROM_AVERAGE {
            return;
        }
        self.samples += 1;
        let value = duration.as_secs_f64() * 1000.0;
        self.average_millis += (value - self.average_millis) / f64::from(self.samples);
    }

    pub fn average_millis(&self) -> f64 {
        self.average_millis
    }

    pub fn next_delay(&self) -> Duration {
        let scaled = self.average_millis * f64::from(SCAN_INTERVAL_MULTIPLIER);
        if scaled <= MINIMUM_SCAN_INTERVAL.as_secs_f64() * 1000.0 {
            return MINIMUM_SCAN_INTERVAL;
        }
        Duration::from_millis(scaled as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row of `/proc/net/tcp`, with the columns the real file has.
    fn proc_net_tcp_header() -> &'static str {
        "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode"
    }

    #[test]
    fn test_parse_ip_address_reverses_ipv4_bytes() {
        assert_eq!(parse_ip_address("0100007F").as_deref(), Some("127.0.0.1"));
        assert_eq!(parse_ip_address("00000000").as_deref(), Some("0.0.0.0"));
        assert_eq!(parse_ip_address("0101A8C0").as_deref(), Some("192.168.1.1"));
        assert_eq!(parse_ip_address("0F02000A").as_deref(), Some("10.0.2.15"));
    }

    #[test]
    fn test_parse_ip_address_expands_ipv6_words() {
        assert_eq!(
            parse_ip_address("00000000000000000000000001000000").as_deref(),
            Some("0:0:0:0:0:0:0:1"),
            "the loopback address is expanded rather than compressed to ::1"
        );
        assert_eq!(
            parse_ip_address("00000000000000000000000000000000").as_deref(),
            Some("0:0:0:0:0:0:0:0")
        );
        assert_eq!(
            parse_ip_address("0000000000000000FFFF00000100007F").as_deref(),
            Some("0:0:0:0:0:ffff:7f00:1"),
            "an IPv4-mapped address keeps the mapped bytes in network order"
        );
    }

    #[test]
    fn test_parse_ip_address_rejects_malformed_input() {
        for hex in [
            "",
            "010000",
            "0100007",
            "0100007G",
            "zzzzzzzz",
            "0100007F00",
        ] {
            assert_eq!(parse_ip_address(hex), None, "{hex:?} is not an address");
        }
    }

    #[test]
    fn test_parse_proc_net_tcp_keeps_only_listening_rows() {
        let contents = format!(
            "{}\n{}\n{}\n{}\n",
            proc_net_tcp_header(),
            "   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345 1 0000000000000000 100 0 0 10 0",
            "   1: 0100007F:0BB8 0100007F:C1B4 01 00000000:00000000 00:00000000 00000000  1000        0 12346 1 0000000000000000 20 4 30 10 -1",
            "   2: 00000000:1F91 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12347 1 0000000000000000 100 0 0 10 0",
        );

        let ports = parse_proc_net_tcp(&contents);

        assert_eq!(
            ports,
            vec![
                ListeningPort {
                    host: "127.0.0.1".to_string(),
                    port: 8080,
                },
                ListeningPort {
                    host: "0.0.0.0".to_string(),
                    port: 8081,
                },
            ],
            "0100007F:1F90 is 127.0.0.1:8080 and the ESTABLISHED row (st 01) is dropped"
        );
    }

    #[test]
    fn test_parse_proc_net_tcp6_expands_ipv6_addresses() {
        let contents = format!(
            "{}\n{}\n",
            proc_net_tcp_header(),
            "   0: 00000000000000000000000001000000:0FA0 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 22222 1 0000000000000000 100 0 0 10 0",
        );

        assert_eq!(
            parse_proc_net_tcp(&contents),
            vec![ListeningPort {
                host: "0:0:0:0:0:0:0:1".to_string(),
                port: 4000,
            }],
            "0FA0 is 4000 in hexadecimal"
        );
    }

    #[test]
    fn test_parse_proc_net_tcp_deduplicates_repeated_addresses() {
        let row = "   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345 1 0000000000000000 100 0 0 10 0";
        let contents = format!("{}\n{row}\n{row}\n", proc_net_tcp_header());

        assert_eq!(
            parse_proc_net_tcp(&contents),
            vec![ListeningPort {
                host: "127.0.0.1".to_string(),
                port: 8080,
            }]
        );
    }

    #[test]
    fn test_parse_proc_net_tcp_skips_malformed_rows_without_losing_later_rows() {
        let contents = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            proc_net_tcp_header(),
            // Truncated mid-row: the state column is missing entirely.
            "   0: 0100007F:1F90",
            // The address has no colon at all.
            "   1: 0100007F1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1 1 0 100 0 0 10 0",
            // Non-hexadecimal characters in the address.
            "   2: ZZZZZZZZ:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 2 1 0 100 0 0 10 0",
            // Non-hexadecimal characters in the port.
            "   3: 0100007F:ZZZZ 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 3 1 0 100 0 0 10 0",
            // An address of a length that is neither IPv4 nor IPv6.
            "   4: 0100007FAB:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4 1 0 100 0 0 10 0",
            // Empty line in the middle of the table.
            "",
            // The row that must still be found after all of the above.
            "   5: 0101A8C0:0050 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 5 1 0 100 0 0 10 0",
        );

        assert_eq!(
            parse_proc_net_tcp(&contents),
            vec![ListeningPort {
                host: "192.168.1.1".to_string(),
                port: 80,
            }],
            "every malformed row is skipped and the well formed row after them is still parsed"
        );
    }

    #[test]
    fn test_parse_proc_net_tcp_handles_empty_and_header_only_input() {
        assert_eq!(parse_proc_net_tcp(""), Vec::new());
        assert_eq!(parse_proc_net_tcp("   \n  \n"), Vec::new());
        assert_eq!(parse_proc_net_tcp(proc_net_tcp_header()), Vec::new());
        assert_eq!(
            parse_proc_net_tcp("not a header at all\n   0: 0100007F:1F90 x 0A"),
            Vec::new(),
            "without the expected column names no row can be located"
        );
    }

    #[test]
    fn test_parse_lsof_output_reads_listening_rows() {
        let contents = "\
COMMAND     PID USER   FD   TYPE             DEVICE SIZE/OFF NODE NAME
node      12345 user   23u  IPv4 0x1234567890abcdef      0t0  TCP 127.0.0.1:3000 (LISTEN)
node      12345 user   24u  IPv6 0xabcdef1234567890      0t0  TCP [::1]:3000 (LISTEN)
python3   99999 user    3u  IPv4 0x000000000000beef      0t0  TCP *:8000 (LISTEN)
ssh         777 user    5u  IPv4 0x000000000000cafe      0t0  TCP 127.0.0.1:52000->127.0.0.1:22 (ESTABLISHED)
";

        assert_eq!(
            parse_lsof_output(contents),
            vec![
                ListeningPort {
                    host: "127.0.0.1".to_string(),
                    port: 3000,
                },
                ListeningPort {
                    host: "::1".to_string(),
                    port: 3000,
                },
                ListeningPort {
                    host: "0.0.0.0".to_string(),
                    port: 8000,
                },
            ],
            "the brackets of an IPv6 literal are dropped, * means every interface, \
             and the ESTABLISHED row is not a listener"
        );
    }

    #[test]
    fn test_parse_lsof_output_skips_malformed_rows_without_losing_later_rows() {
        let contents = "\
COMMAND     PID USER   FD   TYPE             DEVICE SIZE/OFF NODE NAME
(LISTEN)
node      12345 user   23u  IPv4 0x1 0t0  TCP 127.0.0.1:notaport (LISTEN)
node      12345 user   23u  IPv4 0x1 0t0  TCP 127.0.0.1:99999 (LISTEN)
node      12345 user   23u  IPv4 0x1 0t0  TCP nocolonhere (LISTEN)

node      12345 user   23u  IPv4 0x1 0t0  TCP 127.0.0.1:4321 (LISTEN)
";

        assert_eq!(
            parse_lsof_output(contents),
            vec![ListeningPort {
                host: "127.0.0.1".to_string(),
                port: 4321,
            }],
            "a row with (LISTEN) as its first field, an unparsable port, a port above \
             65535 and a missing colon are all skipped without hiding the last row"
        );
    }

    #[test]
    fn test_parse_netstat_output_reads_listening_rows() {
        let contents = "\r
Active Connections\r
\r
  Proto  Local Address          Foreign Address        State           PID\r
  TCP    127.0.0.1:3000         0.0.0.0:0              LISTENING       1234\r
  TCP    0.0.0.0:445            0.0.0.0:0              LISTENING       4\r
  TCP    [::1]:3000             [::]:0                 LISTENING       1234\r
  TCP    127.0.0.1:52000        127.0.0.1:22           ESTABLISHED     5678\r
  UDP    0.0.0.0:5353           *:*                                    999\r
";

        assert_eq!(
            parse_netstat_output(contents),
            vec![
                ListeningPort {
                    host: "127.0.0.1".to_string(),
                    port: 3000,
                },
                ListeningPort {
                    host: "0.0.0.0".to_string(),
                    port: 445,
                },
                ListeningPort {
                    host: "::1".to_string(),
                    port: 3000,
                },
            ],
            "only the LISTENING TCP rows are kept, and UDP has no state column at all"
        );
    }

    #[test]
    fn test_parse_netstat_output_skips_malformed_rows_without_losing_later_rows() {
        let contents = "\
  Proto  Local Address          Foreign Address        State           PID
  TCP
  TCP    notanaddress           0.0.0.0:0              LISTENING       1
  TCP    127.0.0.1:70000        0.0.0.0:0              LISTENING       2
  TCP    127.0.0.1:0            0.0.0.0:0              LISTENING       3

  TCP    127.0.0.1:9000         0.0.0.0:0              LISTENING       4
";

        assert_eq!(
            parse_netstat_output(contents),
            vec![ListeningPort {
                host: "127.0.0.1".to_string(),
                port: 9000,
            }],
            "a bare protocol row, an address with no port, a port above 65535 and \
             port 0 are skipped without hiding the last row"
        );
    }

    #[test]
    fn test_forwardable_hosts_cover_the_expanded_ipv6_spellings() {
        for host in ["localhost", "127.0.0.1", "::1", "0:0:0:0:0:0:0:1"] {
            assert!(is_localhost(host), "{host} is loopback");
            assert!(is_forwardable_host(host));
        }
        for host in ["0.0.0.0", "::", "0:0:0:0:0:0:0:0"] {
            assert!(is_all_interfaces(host), "{host} is every interface");
            assert!(is_forwardable_host(host));
        }
        for host in ["192.168.1.1", "10.0.2.15", "0:0:0:0:0:ffff:7f00:1"] {
            assert!(
                !is_forwardable_host(host),
                "{host} is not reachable through the remote loopback"
            );
        }
    }

    #[test]
    fn test_parse_proc_net_tcp_with_inodes_reads_the_inode_column() {
        let contents = format!(
            "{}\n{}\n{}\n",
            proc_net_tcp_header(),
            "   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345 1 0000000000000000 100 0 0 10 0",
            "   1: 00000000:1F91 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 0 1 0000000000000000 100 0 0 10 0",
        );

        assert_eq!(
            parse_proc_net_tcp_with_inodes(&contents),
            vec![
                (
                    ListeningPort {
                        host: "127.0.0.1".to_string(),
                        port: 8080,
                    },
                    Some(12345)
                ),
                (
                    ListeningPort {
                        host: "0.0.0.0".to_string(),
                        port: 8081,
                    },
                    None
                ),
            ],
            "the inode is the column named inode once rx_queue and tm->when are dropped, \
             and inode 0 means no process holds the socket"
        );
    }

    #[test]
    fn test_parse_socket_link_inode() {
        assert_eq!(parse_socket_link_inode("socket:[12345]"), Some(12345));
        for link in [
            "pipe:[12345]",
            "socket:[]",
            "socket:[12x]",
            "socket:12345",
            "/dev/null",
            "anon_inode:[eventfd]",
        ] {
            assert_eq!(
                parse_socket_link_inode(link),
                None,
                "{link:?} is not a socket"
            );
        }
    }

    #[test]
    fn test_parse_lsof_output_with_pids_keeps_the_pid_column() {
        let contents = "\
COMMAND     PID USER   FD   TYPE             DEVICE SIZE/OFF NODE NAME
node      12345 user   23u  IPv4 0x1234567890abcdef      0t0  TCP 127.0.0.1:3000 (LISTEN)
python3   99999 user    3u  IPv4 0x000000000000beef      0t0  TCP *:8000 (LISTEN)
";

        assert_eq!(
            parse_lsof_output_with_pids(contents),
            vec![
                ListeningSocket {
                    port: ListeningPort {
                        host: "127.0.0.1".to_string(),
                        port: 3000,
                    },
                    process_id: Some(12345),
                },
                ListeningSocket {
                    port: ListeningPort {
                        host: "0.0.0.0".to_string(),
                        port: 8000,
                    },
                    process_id: Some(99999),
                },
            ]
        );
    }

    #[test]
    fn test_parse_netstat_output_with_pids_keeps_the_last_column() {
        let contents = "\
  Proto  Local Address          Foreign Address        State           PID\r
  TCP    127.0.0.1:3000         0.0.0.0:0              LISTENING       1234\r
  TCP    0.0.0.0:135            0.0.0.0:0              LISTENING       0\r
";

        assert_eq!(
            parse_netstat_output_with_pids(contents),
            vec![
                ListeningSocket {
                    port: ListeningPort {
                        host: "127.0.0.1".to_string(),
                        port: 3000,
                    },
                    process_id: Some(1234),
                },
                ListeningSocket {
                    port: ListeningPort {
                        host: "0.0.0.0".to_string(),
                        port: 135,
                    },
                    process_id: None,
                },
            ],
            "the trailing carriage return is not part of the PID, and PID 0 owns nothing"
        );
    }

    fn process(parent: Option<u32>, name: &str, working_directory: Option<&str>) -> ProcessDetails {
        ProcessDetails {
            parent_process_id: parent,
            name: name.to_string(),
            working_directory: working_directory.map(PathBuf::from),
        }
    }

    fn registration(
        process_id: u32,
        session_id: &str,
        name: Option<&str>,
        cwd: &str,
    ) -> RegisteredSession {
        RegisteredSession {
            process_id,
            session_id: session_id.to_string(),
            working_directory: PathBuf::from(cwd),
            process_start: String::new(),
            version: String::new(),
            kind: String::new(),
            name: name.map(str::to_string),
            status: None,
            updated_at: None,
            tmux_target: None,
            bridge_session_id: None,
        }
    }

    fn roots() -> Vec<PathBuf> {
        vec![PathBuf::from("/work/app")]
    }

    #[test]
    fn test_attribute_port_owner_process_inside_a_worktree_is_in_project() {
        let processes = HashMap::from_iter([
            (1, process(None, "init", Some("/"))),
            (100, process(Some(1), "node", Some("/work/app/frontend"))),
        ]);

        assert_eq!(
            attribute_port_owner(Some(100), &processes, &[], &roots()),
            PortOwner {
                process_id: Some(100),
                process_name: Some("node".to_string()),
                in_project: true,
                claude_session_id: None,
                claude_session_name: None,
            }
        );
    }

    #[test]
    fn test_attribute_port_owner_claude_ancestor_inside_a_worktree_is_in_project() {
        let processes = HashMap::from_iter([
            (1, process(None, "init", Some("/"))),
            (50, process(Some(1), "claude", Some("/work/app"))),
            (60, process(Some(50), "zsh", Some("/tmp"))),
            (70, process(Some(60), "python3", Some("/tmp/scratch"))),
        ]);
        let registrations = [registration(
            50,
            "session-a",
            Some("fix-login"),
            "/work/app",
        )];

        assert_eq!(
            attribute_port_owner(Some(70), &processes, &registrations, &roots()),
            PortOwner {
                process_id: Some(70),
                process_name: Some("python3".to_string()),
                in_project: true,
                claude_session_id: Some("session-a".to_string()),
                claude_session_name: Some("fix-login".to_string()),
            },
            "the process runs outside the project, but the Claude session it descends from does not"
        );
    }

    #[test]
    fn test_attribute_port_owner_neighbouring_claude_session_is_named_but_not_in_project() {
        let processes = HashMap::from_iter([
            (1, process(None, "init", Some("/"))),
            (50, process(Some(1), "claude", Some("/work/app"))),
            (80, process(Some(1), "claude", Some("/work/other"))),
            (90, process(Some(80), "node", Some("/work/other/web"))),
        ]);
        let registrations = [
            registration(50, "session-a", Some("fix-login"), "/work/app"),
            registration(80, "session-b", None, "/work/other"),
        ];

        assert_eq!(
            attribute_port_owner(Some(90), &processes, &registrations, &roots()),
            PortOwner {
                process_id: Some(90),
                process_name: Some("node".to_string()),
                in_project: false,
                claude_session_id: Some("session-b".to_string()),
                claude_session_name: None,
            }
        );
    }

    #[test]
    fn test_attribute_port_owner_unknown_owner_is_not_in_project() {
        let processes = HashMap::from_iter([(1, process(None, "init", Some("/")))]);
        assert_eq!(
            attribute_port_owner(None, &processes, &[], &roots()),
            PortOwner::default(),
            "a socket with no visible owner is nobody's"
        );
        assert_eq!(
            attribute_port_owner(Some(4242), &processes, &[], &roots()),
            PortOwner {
                process_id: Some(4242),
                ..PortOwner::default()
            },
            "a PID the process table cannot see has no name or directory to go by"
        );

        let unreadable = HashMap::from_iter([(300, process(Some(1), "postgres", None))]);
        assert!(
            !attribute_port_owner(Some(300), &unreadable, &[], &roots()).in_project,
            "a process whose working directory cannot be read is not in the project"
        );
    }

    #[test]
    fn test_attribute_port_owner_survives_a_parent_cycle() {
        let processes = HashMap::from_iter([
            (10, process(Some(20), "a", Some("/elsewhere"))),
            (20, process(Some(10), "b", Some("/elsewhere"))),
        ]);
        let registrations = [registration(999, "session-z", None, "/work/app")];

        assert_eq!(
            attribute_port_owner(Some(10), &processes, &registrations, &roots()),
            PortOwner {
                process_id: Some(10),
                process_name: Some("a".to_string()),
                in_project: false,
                claude_session_id: None,
                claude_session_name: None,
            }
        );

        let self_parent = HashMap::from_iter([(7, process(Some(7), "loop", None))]);
        assert!(!attribute_port_owner(Some(7), &self_parent, &registrations, &roots()).in_project);
    }

    #[test]
    fn test_attribute_port_owner_stops_after_the_hop_limit() {
        // A chain one longer than the limit, with the Claude session at its top.
        let chain_length = MAX_PARENT_HOPS as u32 + 1;
        let processes: HashMap<u32, ProcessDetails> = (1..=chain_length)
            .map(|process_id| {
                let parent = (process_id > 1).then(|| process_id - 1);
                (process_id, process(parent, "sh", None))
            })
            .collect();
        let registrations = [registration(1, "session-top", None, "/work/app")];

        assert_eq!(
            attribute_port_owner(Some(chain_length), &processes, &registrations, &roots())
                .claude_session_id,
            None
        );
        assert_eq!(
            attribute_port_owner(Some(chain_length - 1), &processes, &registrations, &roots())
                .claude_session_id
                .as_deref(),
            Some("session-top"),
            "a chain of exactly {MAX_PARENT_HOPS} processes still reaches its top"
        );
    }

    fn socket(port: u16, process_id: u32) -> ListeningSocket {
        ListeningSocket {
            port: ListeningPort {
                host: "127.0.0.1".to_string(),
                port,
            },
            process_id: Some(process_id),
        }
    }

    /// Attributes one socket as the connection `connection_id` would, with
    /// `environment` standing in for each process's marker variable, and
    /// returns whether it is in the project plus every PID whose environment
    /// was read.
    fn attribute_for_connection(
        owner: u32,
        processes: &HashMap<u32, ProcessDetails>,
        registrations: &[RegisteredSession],
        connection_id: Option<&str>,
        environment: &HashMap<u32, &str>,
    ) -> (bool, Vec<u32>) {
        let mut lookups = Vec::new();
        let mut marker_of = |process_id: u32| {
            lookups.push(process_id);
            environment.get(&process_id).map(|value| value.to_string())
        };
        let marker = connection_id.map(|connection_id| ConnectionMarker {
            connection_id,
            marker_of: &mut marker_of,
        });
        let attributed = attribute_listening_sockets(
            vec![socket(3000, owner)],
            processes,
            registrations,
            &roots(),
            marker,
        );
        let in_project = attributed
            .first()
            .is_some_and(|(_, owner)| owner.in_project);
        (in_project, lookups)
    }

    #[test]
    fn test_attribute_with_marker_a_process_under_this_connections_terminal_is_in_project() {
        let processes = HashMap::from_iter([
            (1, process(None, "sshd", Some("/"))),
            (10, process(Some(1), "zsh", Some("/home/me"))),
            (20, process(Some(10), "npm", Some("/tmp/elsewhere"))),
            (30, process(Some(20), "node", Some("/tmp/elsewhere"))),
        ]);
        // Only the shell carries the marker, as if `node` had been started
        // with a cleared environment.
        let environment = HashMap::from_iter([(10, "workspace-7")]);

        assert!(
            attribute_for_connection(30, &processes, &[], Some("workspace-7"), &environment).0,
            "node descends from this connection's terminal, wherever it runs"
        );
        assert!(
            !attribute_for_connection(30, &processes, &[], Some("workspace-8"), &environment).0,
            "another window's terminal does not make the port this window's"
        );
    }

    #[test]
    fn test_attribute_with_marker_ignores_a_working_directory_inside_a_worktree() {
        let processes = HashMap::from_iter([
            (1, process(None, "init", Some("/"))),
            (100, process(Some(1), "workerd", Some("/work/app/frontend"))),
        ]);
        let environment = HashMap::default();

        assert!(
            !attribute_for_connection(100, &processes, &[], Some("workspace-7"), &environment).0,
            "a client that sends its connection id no longer trusts the working directory"
        );
        assert_eq!(
            attribute_for_connection(100, &processes, &[], None, &environment),
            (true, Vec::new()),
            "an older client that sends no id keeps the working-directory rule, and no environment is read"
        );
    }

    #[test]
    fn test_attribute_with_marker_a_claude_session_inside_a_worktree_still_counts() {
        let processes = HashMap::from_iter([
            (1, process(None, "tmux", Some("/"))),
            (50, process(Some(1), "claude", Some("/work/app"))),
            (70, process(Some(50), "node", Some("/tmp/scratch"))),
            (80, process(None, "claude", Some("/work/other"))),
            (90, process(Some(80), "node", Some("/work/app"))),
        ]);
        let registrations = [
            registration(50, "session-a", Some("web-tools"), "/work/app"),
            registration(80, "session-b", None, "/work/other"),
        ];
        let environment = HashMap::default();

        assert!(
            attribute_for_connection(
                70,
                &processes,
                &registrations,
                Some("workspace-7"),
                &environment
            )
            .0,
            "a Claude session in a worktree needs no terminal marker"
        );
        assert!(
            !attribute_for_connection(
                90,
                &processes,
                &registrations,
                Some("workspace-7"),
                &environment
            )
            .0,
            "a session outside the project does not count, even though the process runs inside it"
        );
    }

    #[test]
    fn test_attribute_with_marker_an_unreadable_environment_is_unmarked() {
        let processes = HashMap::from_iter([
            (1, process(None, "init", Some("/"))),
            (10, process(Some(1), "zsh", Some("/work/app"))),
            (20, process(Some(10), "node", Some("/work/app"))),
        ]);
        let unreadable = HashMap::default();
        let (in_project, lookups) =
            attribute_for_connection(20, &processes, &[], Some("workspace-7"), &unreadable);
        assert_eq!(
            (in_project, lookups),
            (false, vec![20, 10, 1]),
            "every ancestor is tried, and none that cannot be read counts as marked"
        );

        let empty_marker = HashMap::from_iter([(10, "")]);
        assert!(
            !attribute_for_connection(20, &processes, &[], Some("workspace-7"), &empty_marker).0
        );
    }

    #[test]
    fn test_attribute_with_marker_survives_a_parent_cycle_and_reads_each_process_once() {
        let processes = HashMap::from_iter([
            (10, process(Some(20), "a", Some("/work/app"))),
            (20, process(Some(10), "b", Some("/work/app"))),
            (7, process(Some(7), "loop", None)),
        ]);
        let environment = HashMap::default();

        assert_eq!(
            attribute_for_connection(10, &processes, &[], Some("workspace-7"), &environment),
            (false, vec![10, 20])
        );
        assert_eq!(
            attribute_for_connection(7, &processes, &[], Some("workspace-7"), &environment),
            (false, vec![7])
        );

        let chain_length = MAX_PARENT_HOPS as u32 + 5;
        let chain: HashMap<u32, ProcessDetails> = (1..=chain_length)
            .map(|process_id| {
                let parent = (process_id > 1).then(|| process_id - 1);
                (process_id, process(parent, "sh", None))
            })
            .collect();
        let marked_top = HashMap::from_iter([(1, "workspace-7")]);
        let (in_project, lookups) =
            attribute_for_connection(chain_length, &chain, &[], Some("workspace-7"), &marked_top);
        assert_eq!(
            (in_project, lookups.len()),
            (false, MAX_PARENT_HOPS),
            "the walk stops at the hop limit rather than reading the whole chain"
        );
    }

    #[test]
    fn test_attribute_with_marker_reads_a_shared_ancestor_once_per_scan() {
        let processes = HashMap::from_iter([
            (1, process(None, "init", Some("/"))),
            (10, process(Some(1), "zsh", Some("/"))),
            (20, process(Some(10), "node", Some("/"))),
            (30, process(Some(10), "node", Some("/"))),
        ]);
        let mut lookups = Vec::new();
        let mut marker_of = |process_id: u32| {
            lookups.push(process_id);
            None
        };
        let attributed = attribute_listening_sockets(
            vec![socket(3000, 20), socket(5173, 30)],
            &processes,
            &[],
            &roots(),
            Some(ConnectionMarker {
                connection_id: "workspace-7",
                marker_of: &mut marker_of,
            }),
        );
        assert_eq!(attributed.len(), 2);
        assert_eq!(lookups, vec![20, 10, 1, 30]);
    }

    #[test]
    fn test_connection_marker_in_environ() {
        assert_eq!(
            connection_marker_in_environ(
                b"PATH=/bin\0ZED_REMOTE_CONNECTION_ID_X=wrong\0\xff\xfe\0ZED_REMOTE_CONNECTION_ID=workspace-7=a\0"
            )
            .as_deref(),
            Some("workspace-7=a"),
            "a longer name sharing the prefix and a non-UTF-8 entry are skipped"
        );
        assert_eq!(
            connection_marker_in_environ(b"ZED_REMOTE_CONNECTION_ID=workspace-7").as_deref(),
            Some("workspace-7"),
            "the last entry need not end in NUL"
        );
        assert_eq!(connection_marker_in_environ(b"PATH=/bin\0"), None);
        assert_eq!(connection_marker_in_environ(b""), None);
    }

    #[test]
    fn test_is_worth_offering_skips_ephemeral_ports_and_browsers() {
        let ephemeral = LINUX_DEFAULT_EPHEMERAL_PORTS;
        for port in [3000, 5173, 8080, 8977, 9229] {
            assert!(
                is_worth_offering(port, Some("node"), &ephemeral),
                "a dev server on {port} is offered"
            );
        }
        assert!(is_worth_offering(8977, Some("MainThread"), &ephemeral));
        assert!(is_worth_offering(32767, None, &ephemeral));
        assert!(is_worth_offering(60999 + 1, Some("node"), &ephemeral));
        for port in [32768, 33675, 39205, 44427, 60999] {
            assert!(
                !is_worth_offering(port, Some("node"), &ephemeral),
                "{port} is one the kernel hands out for port 0"
            );
        }
        for browser in [
            "chrome",
            "Chrome",
            "chromium-browse",
            "Google Chrome",
            "chrome_crashpad",
            "firefox",
            "msedge",
        ] {
            assert!(
                !is_worth_offering(9641, Some(browser), &ephemeral),
                "{browser} holds a debugging port"
            );
        }
        for not_a_browser in ["chromatic", "chrome-devtools-mcp", "node"] {
            assert!(
                is_worth_offering(9641, Some(not_a_browser), &ephemeral),
                "{not_a_browser} is not a browser"
            );
        }
        assert!(!is_worth_offering(50000, Some("node"), &IANA_EPHEMERAL_PORTS));
        assert!(is_worth_offering(40000, Some("node"), &IANA_EPHEMERAL_PORTS));
    }

    #[test]
    fn test_parse_ip_local_port_range() {
        assert_eq!(parse_ip_local_port_range("32768\t60999\n"), Some(32768..=60999));
        assert_eq!(parse_ip_local_port_range("1024 65535"), Some(1024..=65535));
        assert_eq!(parse_ip_local_port_range("60999 32768"), None);
        assert_eq!(parse_ip_local_port_range("32768"), None);
        assert_eq!(parse_ip_local_port_range("a b"), None);
        assert_eq!(parse_ip_local_port_range(""), None);
    }

    #[test]
    fn test_attribute_port_owner_matches_worktree_roots_by_path_component() {
        let processes = HashMap::from_iter([
            (100, process(None, "node", Some("/work/app2"))),
            (101, process(None, "node", Some("/work/app"))),
        ]);
        let registrations = [registration(200, "session-c", None, "/work/app2/sub")];
        let child_of_session = HashMap::from_iter([
            (200, process(None, "claude", Some("/work/app2/sub"))),
            (201, process(Some(200), "node", Some("/tmp"))),
        ]);

        assert!(
            !attribute_port_owner(Some(100), &processes, &[], &roots()).in_project,
            "/work/app2 shares a string prefix with /work/app but is not inside it"
        );
        assert!(
            attribute_port_owner(Some(101), &processes, &[], &roots()).in_project,
            "the worktree root itself is inside the project"
        );
        assert!(
            !attribute_port_owner(Some(201), &child_of_session, &registrations, &roots())
                .in_project,
            "a Claude session in /work/app2 is not inside /work/app either"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_attribute_port_owner_matches_a_worktree_opened_through_a_symlink() {
        use std::os::unix::fs::symlink;

        struct TemporaryDirectory(std::path::PathBuf);
        impl Drop for TemporaryDirectory {
            fn drop(&mut self) {
                if let Err(error) = std::fs::remove_dir_all(&self.0) {
                    eprintln!(
                        "failed to remove temporary directory {}: {error}",
                        self.0.display()
                    );
                }
            }
        }

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let temporary = TemporaryDirectory(std::env::temp_dir().join(format!(
            "zed-port-owner-symlink-{}-{unique}",
            std::process::id()
        )));
        let real_application = temporary.0.join("real").join("app");
        let real_sibling = temporary.0.join("real").join("app2");
        let frontend = real_application.join("frontend");
        let sibling_frontend = real_sibling.join("frontend");
        let link_application = temporary.0.join("links").join("app");
        std::fs::create_dir_all(&frontend).unwrap();
        std::fs::create_dir_all(&sibling_frontend).unwrap();
        std::fs::create_dir_all(link_application.parent().unwrap()).unwrap();
        symlink(&real_application, &link_application).unwrap();

        let resolved_application = std::fs::canonicalize(&real_application).unwrap();
        let resolved_frontend = std::fs::canonicalize(&frontend).unwrap();
        let resolved_sibling = std::fs::canonicalize(&sibling_frontend).unwrap();
        let link_directory = link_application.to_str().unwrap();
        let resolved_frontend_directory = resolved_frontend.to_str().unwrap();
        let resolved_sibling_directory = resolved_sibling.to_str().unwrap();
        let outside_directory = temporary.0.join("outside");
        let outside = outside_directory.to_str().unwrap();

        let inside_processes = HashMap::from_iter([(
            100,
            process(None, "node", Some(resolved_frontend_directory)),
        )]);
        let sibling_processes =
            HashMap::from_iter([(101, process(None, "node", Some(resolved_sibling_directory)))]);
        let session_processes = HashMap::from_iter([
            (50, process(None, "claude", Some(outside))),
            (70, process(Some(50), "python3", Some(outside))),
        ]);
        let registrations = [registration(
            50,
            "session-a",
            Some("fix-login"),
            link_directory,
        )];

        let process_owner = attribute_port_owner(
            Some(100),
            &inside_processes,
            &[],
            std::slice::from_ref(&link_application),
        );
        let sibling_owner = attribute_port_owner(
            Some(101),
            &sibling_processes,
            &[],
            std::slice::from_ref(&link_application),
        );
        let session_owner = attribute_port_owner(
            Some(70),
            &session_processes,
            &registrations,
            std::slice::from_ref(&resolved_application),
        );

        assert_eq!(
            (
                process_owner.in_project,
                sibling_owner.in_project,
                session_owner.in_project,
                session_owner.claude_session_id.as_deref(),
            ),
            (true, false, true, Some("session-a")),
            "process {process_owner:?}\nsibling {sibling_owner:?}\nsession {session_owner:?}\nlink {}\nresolved {}",
            link_application.display(),
            resolved_application.display(),
        );
    }

    #[test]
    fn test_a_missing_proc_net_table_does_not_fail_when_the_other_was_read() {
        let missing = || std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let denied = || std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");

        let actual = merge_optional_reads([
            ("/proc/net/tcp", Ok(Vec::<u16>::new())),
            ("/proc/net/tcp6", Err(missing())),
        ]);
        assert_eq!(
            actual
                .as_ref()
                .map(Vec::len)
                .map_err(|error| error.to_string()),
            Ok(0),
            "not found is an absent optional table; an empty successful read is a real answer"
        );

        let denied_and_empty = merge_optional_reads([
            ("/proc/net/tcp", Err(denied())),
            ("/proc/net/tcp6", Ok(Vec::<u16>::new())),
        ]);
        assert!(
            denied_and_empty.is_err(),
            "a permission error on one table with no rows from the other must still fail the scan, got {denied_and_empty:?}"
        );

        let both_missing: Result<Vec<u16>> = merge_optional_reads([
            ("/proc/net/tcp", Err(missing())),
            ("/proc/net/tcp6", Err(missing())),
        ]);
        assert!(
            both_missing.is_err(),
            "when neither table can be read the scan must fail, got {both_missing:?}"
        );

        let kept = merge_optional_reads([
            ("/proc/net/tcp", Ok(vec![7u16])),
            ("/proc/net/tcp6", Err(missing())),
        ]);
        assert_eq!(
            kept.as_ref()
                .map(Vec::len)
                .map_err(|error| error.to_string()),
            Ok(1),
            "rows from the table that could be read are kept when the other is absent, got {kept:?}"
        );
    }

    #[test]
    fn test_scan_timings_ignores_the_warm_up_scans() {
        let mut timings = ScanTimings::default();
        for _ in 0..SCANS_EXCLUDED_FROM_AVERAGE {
            timings.record(Duration::from_millis(10_000));
        }
        assert_eq!(
            timings.average_millis(),
            0.0,
            "the first {SCANS_EXCLUDED_FROM_AVERAGE} scans are warm-up and do not count"
        );
        assert_eq!(timings.next_delay(), MINIMUM_SCAN_INTERVAL);

        timings.record(Duration::from_millis(100));
        assert_eq!(timings.average_millis(), 100.0);
        timings.record(Duration::from_millis(300));
        assert_eq!(
            timings.average_millis(),
            200.0,
            "the average is cumulative over the scans that count"
        );
    }

    #[test]
    fn test_scan_timings_delay_is_twenty_times_the_average_but_never_below_the_floor() {
        let mut timings = ScanTimings::default();
        for _ in 0..SCANS_EXCLUDED_FROM_AVERAGE {
            timings.record(Duration::from_millis(1));
        }

        timings.record(Duration::from_millis(50));
        assert_eq!(
            timings.next_delay(),
            MINIMUM_SCAN_INTERVAL,
            "50 ms * 20 is 1000 ms, which is below the 2000 ms floor"
        );

        timings.record(Duration::from_millis(550));
        assert_eq!(timings.average_millis(), 300.0);
        assert_eq!(
            timings.next_delay(),
            Duration::from_millis(6000),
            "300 ms * 20 is 6000 ms, which is above the floor"
        );
    }
}
