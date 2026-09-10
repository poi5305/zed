//! Discovery of the TCP ports that are listening on the machine this code runs
//! on. The parsers are deliberately free of IO so that every platform's format
//! can be tested from a captured sample, and the IO wrappers do nothing but
//! read a file or run a command and hand the text to a parser.
//!
//! The formats and the polling strategy follow VS Code's
//! `extHostTunnelService.ts`, which solves the same problem from inside the
//! remote server.

use std::time::Duration;

use anyhow::{Context as _, Result};
use collections::HashSet;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ListeningPort {
    pub host: String,
    pub port: u16,
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
            ports.push(ListeningPort { host, port });
        }
    }
    ports
}

/// Parses `lsof -nP -iTCP -sTCP:LISTEN`, whose rows end in
/// `TCP <address> (LISTEN)`.
pub fn parse_lsof_output(contents: &str) -> Vec<ListeningPort> {
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
        if seen.insert(port.clone()) {
            ports.push(port);
        }
    }
    ports
}

/// Parses `netstat -ano`, whose TCP rows are
/// `TCP <local> <remote> LISTENING <pid>`.
pub fn parse_netstat_output(contents: &str) -> Vec<ListeningPort> {
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
        if seen.insert(port.clone()) {
            ports.push(port);
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
    let mut ports = platform_scan().await?;
    ports.retain(|port| is_forwardable_host(&port.host));
    ports.sort();
    ports.dedup();
    Ok(ports)
}

#[cfg(target_os = "linux")]
async fn platform_scan() -> Result<Vec<ListeningPort>> {
    // Both files are optional: a kernel built without IPv6 has no `tcp6`, and
    // a container may hide either, which is not a reason to report nothing.
    let mut ports = Vec::new();
    let mut last_error = None;
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        match std::fs::read_to_string(path) {
            Ok(contents) => ports.extend(parse_proc_net_tcp(&contents)),
            Err(error) => last_error = Some((path, error)),
        }
    }
    if ports.is_empty()
        && let Some((path, error)) = last_error
    {
        return Err(error).with_context(|| format!("could not read {path}"));
    }
    Ok(ports)
}

#[cfg(any(target_os = "macos", target_os = "freebsd"))]
async fn platform_scan() -> Result<Vec<ListeningPort>> {
    let output = util::command::new_command("lsof")
        .args(["-nP", "-iTCP", "-sTCP:LISTEN"])
        .output()
        .await
        .context("could not run lsof")?;
    // `lsof` exits non-zero when some file descriptors could not be inspected,
    // which is the normal case for an unprivileged process, so the exit status
    // is not a reason to discard the rows it did print.
    Ok(parse_lsof_output(&String::from_utf8_lossy(&output.stdout)))
}

#[cfg(target_os = "windows")]
async fn platform_scan() -> Result<Vec<ListeningPort>> {
    let output = util::command::new_command("netstat")
        .arg("-ano")
        .output()
        .await
        .context("could not run netstat")?;
    Ok(parse_netstat_output(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "windows"
)))]
async fn platform_scan() -> Result<Vec<ListeningPort>> {
    anyhow::bail!("listening port detection is not implemented for this platform")
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
node      12345 andy   23u  IPv4 0x1234567890abcdef      0t0  TCP 127.0.0.1:3000 (LISTEN)
node      12345 andy   24u  IPv6 0xabcdef1234567890      0t0  TCP [::1]:3000 (LISTEN)
python3   99999 andy    3u  IPv4 0x000000000000beef      0t0  TCP *:8000 (LISTEN)
ssh         777 andy    5u  IPv4 0x000000000000cafe      0t0  TCP 127.0.0.1:52000->127.0.0.1:22 (ESTABLISHED)
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
node      12345 andy   23u  IPv4 0x1 0t0  TCP 127.0.0.1:notaport (LISTEN)
node      12345 andy   23u  IPv4 0x1 0t0  TCP 127.0.0.1:99999 (LISTEN)
node      12345 andy   23u  IPv4 0x1 0t0  TCP nocolonhere (LISTEN)

node      12345 andy   23u  IPv4 0x1 0t0  TCP 127.0.0.1:4321 (LISTEN)
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
