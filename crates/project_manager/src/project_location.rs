use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use remote::{
    DockerConnectionOptions, RemoteConnectionOptions, SshConnectionOptions, WslConnectionOptions,
};

const SSH_SCHEME: &str = "ssh";
const WSL_SCHEME: &str = "wsl";
const DOCKER_SCHEME: &str = "docker";

/// Characters that have to be percent-encoded when a path is written back into
/// a `rootPath` URI. `/` is deliberately left alone: it separates the segments.
const PATH_ESCAPES: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'%')
    .add(b'#')
    .add(b'?')
    .add(b'"')
    .add(b'<')
    .add(b'>')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'|');

/// The authority also has to hide the delimiters that would otherwise end it.
const AUTHORITY_ESCAPES: &AsciiSet = &PATH_ESCAPES.add(b'/').add(b'@').add(b':');

/// Where the folders of a `ProjectEntry` live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectLocation {
    Local(Vec<PathBuf>),
    Remote {
        options: RemoteConnectionOptions,
        paths: Vec<PathBuf>,
    },
}

/// Resolves `root_path` plus the extra `paths` of a project entry into the
/// folders to open and, when they carry a `ssh://`, `wsl://` or `docker://`
/// scheme, the connection to open them through.
///
/// A path without a scheme is a path on this machine, which is what every
/// `projects.json` written before remote support contained.
pub fn parse_project_location(root_path: &str, paths: &[String]) -> Result<ProjectLocation> {
    let mut established: Option<(&str, Option<RemoteConnectionOptions>)> = None;
    let mut open_paths: Vec<PathBuf> = Vec::with_capacity(paths.len() + 1);

    for candidate in std::iter::once(root_path).chain(paths.iter().map(String::as_str)) {
        let candidate = candidate.trim();
        if candidate.is_empty() {
            continue;
        }

        let (connection, path) = parse_candidate(candidate)?;
        match &established {
            Some((first, expected)) if expected != &connection => bail!(
                "\"{first}\" is on {}, but \"{candidate}\" is on {}; every folder of a project must be in the same place",
                describe(expected),
                describe(&connection),
            ),
            _ => {}
        }
        if established.is_none() {
            established = Some((candidate, connection));
        }
        if !open_paths.contains(&path) {
            open_paths.push(path);
        }
    }

    match established {
        Some((_, Some(options))) => Ok(ProjectLocation::Remote {
            options,
            paths: open_paths,
        }),
        _ => Ok(ProjectLocation::Local(open_paths)),
    }
}

/// Renders an absolute remote path back into the URI form stored in
/// `projects.json`. `None` when the connection or the path cannot be written as
/// one, for instance a Windows-style path behind a remote connection.
pub fn remote_project_uri(options: &RemoteConnectionOptions, path: &Path) -> Option<String> {
    let path = path.to_str()?;
    if !path.starts_with('/') {
        return None;
    }
    let path = utf8_percent_encode(path, PATH_ESCAPES);

    Some(match options {
        RemoteConnectionOptions::Ssh(options) => {
            let mut uri = format!("{SSH_SCHEME}://");
            if let Some(username) = &options.username {
                uri.push_str(&utf8_percent_encode(username, AUTHORITY_ESCAPES).to_string());
                uri.push('@');
            }
            uri.push_str(&options.host.to_bracketed_string());
            if let Some(port) = options.port {
                uri.push_str(&format!(":{port}"));
            }
            uri.push_str(&path.to_string());
            uri
        }
        RemoteConnectionOptions::Wsl(options) => format!(
            "{WSL_SCHEME}://{}{path}",
            utf8_percent_encode(&options.distro_name, AUTHORITY_ESCAPES)
        ),
        RemoteConnectionOptions::Docker(options) => {
            // The container id is regenerated every time the container is
            // rebuilt, so the authority records the name, which devcontainer.json
            // keeps stable. A container reached by id alone has no name.
            let container = if options.name.is_empty() {
                &options.container_id
            } else {
                &options.name
            };
            format!(
                "{DOCKER_SCHEME}://{}{path}",
                utf8_percent_encode(container, AUTHORITY_ESCAPES)
            )
        }
        #[allow(unreachable_patterns)]
        _ => return None,
    })
}

/// The scheme VS Code writes for a folder that is not on this machine. What follows it
/// is not a host but a remote *kind* and its argument, joined by a `+`.
const VSCODE_REMOTE_SCHEME: &str = "vscode-remote";
const VSCODE_SSH_REMOTE: &str = "ssh-remote+";
const VSCODE_WSL_REMOTE: &str = "wsl+";
const FILE_SCHEME: &str = "file";

/// A `rootPath` translated out of a VS Code project file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedPath {
    pub path: String,
    /// Something true of this translation that the reader has to be told, for a path that
    /// was imported anyway. A path that cannot be translated at all is an `Err` instead.
    pub warning: Option<String>,
}

/// Translates one `rootPath` of a VS Code "Project Manager" entry into the form this
/// panel stores.
///
/// The two formats are otherwise the same file, so the translation is only of the path:
///
/// * A plain path is what a local folder already looks like here.
/// * `vscode-remote://ssh-remote+<host>/<path>` becomes `ssh://<host>/<path>`. The host
///   is a name in the reader's SSH config, which is also how a Coder workspace is
///   reached — `coder config-ssh` writes one host per workspace — so those entries need
///   nothing beyond this.
/// * `vscode-remote://wsl+<distribution>/<path>` becomes `wsl://<distribution>/<path>`.
/// * `file:///<path>` is a local folder written as a URI, so it becomes the path again.
///
/// Every other remote VS Code can name — a dev container, an attached container, a
/// codespace, a tunnel — identifies its target by something that is not a host name, and
/// is reported rather than guessed at.
pub fn import_vscode_path(candidate: &str) -> Result<ImportedPath> {
    let candidate = candidate.trim();
    if candidate.is_empty() {
        bail!("it names no folder");
    }

    let Some((scheme, rest)) = split_scheme(candidate) else {
        return Ok(ImportedPath {
            path: candidate.to_string(),
            warning: None,
        });
    };

    match scheme.to_ascii_lowercase().as_str() {
        // Already stored in this panel's own form, which is what re-importing a file
        // that was exported from here produces.
        SSH_SCHEME | WSL_SCHEME | DOCKER_SCHEME => Ok(ImportedPath {
            path: candidate.to_string(),
            warning: None,
        }),
        FILE_SCHEME => {
            let path = decode(rest.trim_start_matches('/'))
                .with_context(|| format!("decoding the path of \"{candidate}\""))?;
            if path.is_empty() {
                bail!("\"{candidate}\" names no folder");
            }
            Ok(ImportedPath {
                path: format!("/{path}"),
                warning: None,
            })
        }
        VSCODE_REMOTE_SCHEME => import_vscode_remote(candidate, rest),
        other => bail!(
            "\"{candidate}\" uses the scheme \"{other}\", which is not a folder this panel can open"
        ),
    }
}

fn import_vscode_remote(candidate: &str, rest: &str) -> Result<ImportedPath> {
    let (authority, path) = match rest.find('/') {
        Some(separator) => (&rest[..separator], &rest[separator..]),
        None => (rest, ""),
    };
    if path.is_empty() {
        bail!("\"{candidate}\" names a host but no folder on it");
    }
    let authority =
        decode(authority).with_context(|| format!("decoding the remote of \"{candidate}\""))?;

    // VS Code writes a folder on a Windows host as `/C:/Users/...`: a drive letter behind
    // a leading separator. The leading separator is what makes it a path in a URI at all,
    // and dropping it would leave a path this panel rejects as relative, so it is kept
    // and said out loud instead.
    let warning = windows_drive_path(path).map(|drive| {
        format!(
            "\"{candidate}\" is on drive {drive} of a Windows host, and is imported as \
             \"{path}\"; open it once to check that the host reads it that way"
        )
    });

    if let Some(host) = authority.strip_prefix(VSCODE_SSH_REMOTE) {
        if host.is_empty() {
            bail!("\"{candidate}\" does not name a host");
        }
        return Ok(ImportedPath {
            path: format!(
                "{SSH_SCHEME}://{}{path}",
                utf8_percent_encode(host, AUTHORITY_ESCAPES)
            ),
            warning,
        });
    }
    if let Some(distribution) = authority.strip_prefix(VSCODE_WSL_REMOTE) {
        if distribution.is_empty() {
            bail!("\"{candidate}\" does not name a WSL distribution");
        }
        return Ok(ImportedPath {
            path: format!(
                "{WSL_SCHEME}://{}{path}",
                utf8_percent_encode(distribution, AUTHORITY_ESCAPES)
            ),
            warning,
        });
    }

    let kind = authority
        .split_once('+')
        .map_or(authority.as_str(), |(kind, _)| kind);
    bail!(
        "\"{candidate}\" is a \"{kind}\" remote, which names its target by something other \
         than a host; open it once from Zed and save the project from there"
    )
}

/// The drive letter of `/C:/Users/andy`, or `None` for a path that is not one.
fn windows_drive_path(path: &str) -> Option<char> {
    let mut characters = path.strip_prefix('/')?.chars();
    let drive = characters.next()?;
    if !drive.is_ascii_alphabetic() || characters.next()? != ':' {
        return None;
    }
    Some(drive.to_ascii_uppercase())
}

fn parse_candidate(candidate: &str) -> Result<(Option<RemoteConnectionOptions>, PathBuf)> {
    let Some((scheme, rest)) = split_scheme(candidate) else {
        return Ok((None, PathBuf::from(shellexpand::tilde(candidate).as_ref())));
    };
    let scheme = scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), SSH_SCHEME | WSL_SCHEME | DOCKER_SCHEME) {
        bail!(
            "\"{candidate}\" uses the unsupported scheme \"{scheme}\"; expected {SSH_SCHEME}, {WSL_SCHEME}, {DOCKER_SCHEME}, or a path without a scheme"
        );
    }

    let (authority, path) = match rest.find('/') {
        Some(separator) => (&rest[..separator], &rest[separator..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        bail!("\"{candidate}\" does not name a host");
    }
    let path = decode(path).with_context(|| format!("decoding the path of \"{candidate}\""))?;
    if !path.starts_with('/') {
        bail!(
            "the path of \"{candidate}\" is not absolute; write it as \"{scheme}://{authority}/absolute/path\""
        );
    }

    let options = match scheme.as_str() {
        SSH_SCHEME => RemoteConnectionOptions::Ssh(parse_ssh_authority(authority, candidate)?),
        WSL_SCHEME => RemoteConnectionOptions::Wsl(WslConnectionOptions {
            distro_name: decode(authority)
                .with_context(|| format!("decoding the distribution of \"{candidate}\""))?,
            user: None,
        }),
        _ => {
            let container = decode(authority)
                .with_context(|| format!("decoding the container of \"{candidate}\""))?;
            // A URI carries one identifier, and `docker exec` accepts either a
            // name or an id, so it goes into both fields: `name` is what the
            // panel displays, `container_id` is what the connection execs into.
            RemoteConnectionOptions::Docker(DockerConnectionOptions {
                name: container.clone(),
                container_id: container,
                ..Default::default()
            })
        }
    };
    Ok((Some(options), PathBuf::from(path)))
}

/// Splits `scheme://rest`, or `None` when `candidate` is a plain path. A single
/// leading character is never treated as a scheme so that the Windows drive
/// letter of `C:\Users\andy\proj` cannot be mistaken for one.
fn split_scheme(candidate: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = candidate.split_once("://")?;
    if scheme.len() < 2 {
        return None;
    }
    let mut characters = scheme.chars();
    if !characters.next()?.is_ascii_alphabetic() {
        return None;
    }
    if !characters
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.'))
    {
        return None;
    }
    Some((scheme, rest))
}

fn parse_ssh_authority(authority: &str, candidate: &str) -> Result<SshConnectionOptions> {
    let (username, host_and_port) = match authority.rsplit_once('@') {
        Some((username, host_and_port)) => {
            let username = decode(username)
                .with_context(|| format!("decoding the user of \"{candidate}\""))?;
            if username.is_empty() {
                bail!(
                    "\"{candidate}\" has an empty user name; write it as \"{SSH_SCHEME}://{host_and_port}/absolute/path\" when there is no user"
                );
            }
            (Some(username), host_and_port)
        }
        None => (None, authority),
    };

    let (host, port) = match host_and_port.strip_prefix('[') {
        Some(bracketed) => {
            let (host, rest) = bracketed
                .split_once(']')
                .with_context(|| format!("\"{candidate}\" has an unterminated IPv6 host"))?;
            match rest.strip_prefix(':') {
                Some(port) => (host, Some(port)),
                None if rest.is_empty() => (host, None),
                None => bail!("\"{candidate}\" has trailing text \"{rest}\" after its host"),
            }
        }
        None => match host_and_port.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (host_and_port, None),
        },
    };
    let host = decode(host).with_context(|| format!("decoding the host of \"{candidate}\""))?;
    // RFC 3986 makes the host of a URI case-insensitive, so `HOST.COM` and
    // `host.com` have to resolve to one connection rather than two.
    let host = host.to_ascii_lowercase();
    if host.is_empty() {
        bail!("\"{candidate}\" does not name a host");
    }

    // An omitted port stays `None` so that ssh picks it up from `~/.ssh/config`
    // instead of us hardcoding 22.
    let port = match port {
        Some(port) => {
            let number = port
                .parse::<u16>()
                .with_context(|| format!("\"{candidate}\" has the invalid port \"{port}\""))?;
            if number == 0 {
                bail!(
                    "\"{candidate}\" has the invalid port \"0\"; port 0 is reserved and cannot be connected to"
                );
            }
            Some(number)
        }
        None => None,
    };

    Ok(SshConnectionOptions {
        host: host.into(),
        username,
        port,
        ..Default::default()
    })
}

fn decode(value: &str) -> Result<String> {
    Ok(percent_decode_str(value).decode_utf8()?.into_owned())
}

fn describe(connection: &Option<RemoteConnectionOptions>) -> String {
    match connection {
        None => "this machine".to_string(),
        Some(RemoteConnectionOptions::Ssh(options)) => {
            let mut description = String::from("ssh ");
            if let Some(username) = &options.username {
                description.push_str(username);
                description.push('@');
            }
            description.push_str(&options.host.to_bracketed_string());
            if let Some(port) = options.port {
                description.push_str(&format!(":{port}"));
            }
            description
        }
        Some(options) => format!("{} ({})", options.host(), options.connection_type()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(root_path: &str, paths: &[&str]) -> Result<ProjectLocation> {
        let paths: Vec<String> = paths.iter().map(|path| path.to_string()).collect();
        parse_project_location(root_path, &paths)
    }

    fn ssh_options(location: &ProjectLocation) -> &SshConnectionOptions {
        match location {
            ProjectLocation::Remote {
                options: RemoteConnectionOptions::Ssh(options),
                ..
            } => options,
            other => panic!("expected an ssh location, got {other:?}"),
        }
    }

    #[test]
    fn test_paths_without_a_scheme_stay_local() {
        assert_eq!(
            parse("/abs/path", &["/abs/other"]).expect("plain paths are local"),
            ProjectLocation::Local(vec![
                PathBuf::from("/abs/path"),
                PathBuf::from("/abs/other"),
            ]),
            "a projects.json written before remote support keeps working"
        );

        assert_eq!(
            parse("/root", &["  ", "/extra", "/root"]).expect("plain paths are local"),
            ProjectLocation::Local(vec![PathBuf::from("/root"), PathBuf::from("/extra")]),
            "blanks and duplicates are dropped, root first"
        );

        assert_eq!(
            parse("   ", &[]).expect("a blank entry is not an error"),
            ProjectLocation::Local(Vec::new())
        );

        let home = shellexpand::tilde("~").to_string();
        assert_eq!(
            parse("~/proj", &[]).expect("a tilde path is local"),
            ProjectLocation::Local(vec![PathBuf::from(format!("{home}/proj"))]),
            "`~` is expanded for local paths only"
        );
    }

    #[test]
    fn test_ssh_uri_with_user_and_port() {
        let location = parse("ssh://user@host:2222/home/a/p", &[]).expect("a full ssh uri");
        let options = ssh_options(&location);

        assert_eq!(options.username.as_deref(), Some("user"));
        assert_eq!(options.host.to_string(), "host");
        assert_eq!(options.port, Some(2222));
        assert_eq!(
            location,
            ProjectLocation::Remote {
                options: RemoteConnectionOptions::Ssh(options.clone()),
                paths: vec![PathBuf::from("/home/a/p")],
            }
        );
    }

    #[test]
    fn test_ssh_uri_without_user_or_port_leaves_the_port_unset() {
        let location = parse("ssh://host/path", &[]).expect("a bare ssh uri");
        let options = ssh_options(&location);

        assert_eq!(options.username, None);
        assert_eq!(options.host.to_string(), "host");
        assert_eq!(
            options.port, None,
            "an omitted port must stay None so ssh reads it from ~/.ssh/config, not be filled in as 22"
        );
        assert_eq!(
            location,
            ProjectLocation::Remote {
                options: RemoteConnectionOptions::Ssh(options.clone()),
                paths: vec![PathBuf::from("/path")],
            }
        );
    }

    #[test]
    fn test_wsl_and_docker_uris() {
        assert_eq!(
            parse("wsl://Ubuntu/home/a/p", &[]).expect("a wsl uri"),
            ProjectLocation::Remote {
                options: RemoteConnectionOptions::Wsl(WslConnectionOptions {
                    distro_name: "Ubuntu".to_string(),
                    user: None,
                }),
                paths: vec![PathBuf::from("/home/a/p")],
            }
        );

        assert_eq!(
            parse("docker://abc123/workspaces/app", &[]).expect("a docker uri"),
            ProjectLocation::Remote {
                options: RemoteConnectionOptions::Docker(DockerConnectionOptions {
                    name: "abc123".to_string(),
                    container_id: "abc123".to_string(),
                    ..Default::default()
                }),
                paths: vec![PathBuf::from("/workspaces/app")],
            }
        );
    }

    #[test]
    fn test_docker_uris_carry_the_container_name_not_its_id() {
        let options = RemoteConnectionOptions::Docker(DockerConnectionOptions {
            name: "my-app".to_string(),
            container_id: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_string(),
            ..Default::default()
        });
        assert_eq!(
            remote_project_uri(&options, Path::new("/workspaces/app")).as_deref(),
            Some("docker://my-app/workspaces/app"),
            "a rebuilt container gets a new id, so the uri has to record the stable name"
        );
    }

    #[test]
    fn test_docker_uri_parses_into_a_usable_exec_target() {
        let location = parse("docker://my-app/workspaces/app", &[]).expect("a docker uri");
        let ProjectLocation::Remote {
            options: RemoteConnectionOptions::Docker(options),
            ..
        } = &location
        else {
            panic!("expected a docker location, got {location:?}");
        };
        assert_eq!(
            options.name, "my-app",
            "the authority names the container, which is what the panel displays"
        );
        assert!(
            !options.container_id.is_empty(),
            "an empty container_id makes `docker exec` fail, so a saved project would no longer open"
        );
        assert_eq!(
            options.container_id, "my-app",
            "`docker exec` takes a name or an id, so the single identifier a uri carries has to fill the exec target too; leaving it empty makes `docker exec` fail and the saved project unopenable"
        );
    }

    #[test]
    fn test_docker_uri_falls_back_to_the_container_id_when_there_is_no_name() {
        let options = RemoteConnectionOptions::Docker(DockerConnectionOptions {
            name: String::new(),
            container_id: "abc123".to_string(),
            ..Default::default()
        });
        assert_eq!(
            remote_project_uri(&options, Path::new("/workspaces/app")).as_deref(),
            Some("docker://abc123/workspaces/app"),
            "a container reached by id alone must still be recordable"
        );
    }

    #[test]
    fn test_mixing_local_and_remote_paths_is_rejected() {
        let error = parse("ssh://host/remote", &["/local"])
            .expect_err("a project cannot straddle this machine and a remote host");
        let message = format!("{error:#}");

        assert!(
            message.contains("ssh://host/remote"),
            "the error must name the first folder, got: {message}"
        );
        assert!(
            message.contains("/local"),
            "the error must name the folder that disagrees, got: {message}"
        );
        assert!(
            message.contains("this machine"),
            "the error must say where the offending folder is, got: {message}"
        );
    }

    #[test]
    fn test_two_different_hosts_in_one_entry_are_rejected() {
        let error =
            parse("ssh://one/p", &["ssh://two/p"]).expect_err("a project cannot span two hosts");
        let message = format!("{error:#}");

        assert!(
            message.contains("ssh://one/p") && message.contains("ssh://two/p"),
            "the error must name both folders, got: {message}"
        );

        let error = parse("ssh://host/p", &["ssh://host:2222/p"])
            .expect_err("the same host on another port is another connection");
        assert!(
            format!("{error:#}").contains("ssh://host:2222/p"),
            "the error must name the folder that disagrees, got: {error:#}"
        );
    }

    #[test]
    fn test_relative_remote_paths_are_rejected() {
        for uri in ["ssh://host", "wsl://Ubuntu", "docker://abc123"] {
            let error = parse(uri, &[]).expect_err("a remote uri without a path is not usable");
            let message = format!("{error:#}");
            assert!(
                message.contains(uri) && message.contains("absolute"),
                "the error must name the uri and say the path must be absolute, got: {message}"
            );
        }

        let error =
            parse("ssh://host/p", &["ssh://host"]).expect_err("the extra folders are checked too");
        assert!(format!("{error:#}").contains("absolute"), "got: {error:#}");
    }

    #[test]
    fn test_percent_encoded_paths_are_decoded() {
        assert_eq!(
            parse("ssh://host/home/a/My%20Project", &[]).expect("a percent-encoded path"),
            ProjectLocation::Remote {
                options: RemoteConnectionOptions::Ssh(SshConnectionOptions {
                    host: "host".to_string().into(),
                    ..Default::default()
                }),
                paths: vec![PathBuf::from("/home/a/My Project")],
            }
        );

        assert_eq!(
            parse("wsl://Ubuntu%2024.04/home/a%20b", &[]).expect("a percent-encoded distro"),
            ProjectLocation::Remote {
                options: RemoteConnectionOptions::Wsl(WslConnectionOptions {
                    distro_name: "Ubuntu 24.04".to_string(),
                    user: None,
                }),
                paths: vec![PathBuf::from("/home/a b")],
            }
        );

        let location = parse("ssh://user%40example.com@host/p", &[]).expect("an encoded user");
        assert_eq!(
            ssh_options(&location).username.as_deref(),
            Some("user@example.com")
        );
    }

    #[test]
    fn test_windows_drive_letters_are_not_schemes() {
        assert_eq!(
            parse(r"C:\Users\andy\proj", &[]).expect("a Windows path is a local path"),
            ProjectLocation::Local(vec![PathBuf::from(r"C:\Users\andy\proj")]),
            "the `C:` of a Windows drive must never be read as a URI scheme"
        );

        assert_eq!(
            parse(r"C://Users/andy/proj", &[]).expect("a Windows path with forward slashes"),
            ProjectLocation::Local(vec![PathBuf::from(r"C://Users/andy/proj")]),
            "a one-character prefix is a drive letter, not a scheme, even before `://`"
        );

        assert_eq!(
            parse(r"C:\Users\andy\proj", &[r"D:\other"])
                .expect("two Windows drives are both local"),
            ProjectLocation::Local(vec![
                PathBuf::from(r"C:\Users\andy\proj"),
                PathBuf::from(r"D:\other"),
            ]),
            "two drive letters do not count as two different hosts"
        );
    }

    #[test]
    fn test_unsupported_schemes_are_reported() {
        let error = parse("vscode-remote://ssh-remote+host/p", &[])
            .expect_err("unknown scheme is an error");
        let message = format!("{error:#}");
        assert!(
            message.contains("vscode-remote") && message.contains("unsupported scheme"),
            "got: {message}"
        );
    }

    #[test]
    fn test_remote_project_uri_round_trips() {
        let cases = [
            "ssh://user@host:2222/home/a/p",
            "ssh://host/home/a/My%20Project",
            "wsl://Ubuntu/home/a/p",
            "docker://abc123/workspaces/app",
        ];

        for uri in cases {
            let location = parse(uri, &[]).unwrap_or_else(|error| panic!("parsing {uri}: {error}"));
            let ProjectLocation::Remote { options, paths } = &location else {
                panic!("{uri} should be remote, got {location:?}");
            };
            let rendered = remote_project_uri(options, &paths[0])
                .unwrap_or_else(|| panic!("{uri} should render back to a uri"));
            assert_eq!(rendered, uri, "{uri} must survive a round trip");
        }

        assert_eq!(
            remote_project_uri(
                &RemoteConnectionOptions::Ssh(SshConnectionOptions {
                    host: "::1".to_string().into(),
                    port: Some(22),
                    ..Default::default()
                }),
                Path::new("/home/a")
            )
            .as_deref(),
            Some("ssh://[::1]:22/home/a"),
            "an IPv6 host is bracketed so the port stays readable"
        );

        assert_eq!(
            remote_project_uri(
                &RemoteConnectionOptions::Wsl(WslConnectionOptions {
                    distro_name: "Ubuntu".to_string(),
                    user: None,
                }),
                Path::new(r"C:\proj")
            ),
            None,
            "a path that is not absolute in the URI sense cannot be recorded"
        );
    }

    #[test]
    fn test_ssh_hosts_are_compared_without_case() {
        let location = parse("ssh://host.com/a", &["ssh://HOST.COM/b"]).expect(
            "RFC 3986 makes the host case-insensitive, so both folders share one connection",
        );
        let options = ssh_options(&location);
        assert_eq!(
            options.host.to_string(),
            "host.com",
            "the host is stored in its canonical lowercase form"
        );
        assert_eq!(
            location,
            ProjectLocation::Remote {
                options: RemoteConnectionOptions::Ssh(options.clone()),
                paths: vec![PathBuf::from("/a"), PathBuf::from("/b")],
            }
        );

        let location = parse("ssh://[FE80::1]/p", &[]).expect("an uppercase IPv6 host");
        assert_eq!(ssh_options(&location).host.to_string(), "fe80::1");
    }

    #[test]
    fn test_ssh_host_is_percent_decoded() {
        let location = parse("ssh://ho%73t/p", &[]).expect("a percent-encoded host");
        assert_eq!(
            ssh_options(&location).host.to_string(),
            "host",
            "the host is decoded like the user, the path, the distro and the container are"
        );
    }

    #[test]
    fn test_ssh_port_zero_is_rejected() {
        let error = parse("ssh://host:0/p", &[])
            .expect_err("port 0 is reserved and cannot be connected to");
        let message = format!("{error:#}");
        assert!(
            message.contains("ssh://host:0/p"),
            "the error must name the uri, got: {message}"
        );
    }

    #[test]
    fn test_ssh_empty_user_is_rejected() {
        let error = parse("ssh://@host/p", &[]).expect_err("an empty user name is not a user name");
        let message = format!("{error:#}");
        assert!(
            message.contains("ssh://@host/p"),
            "the error must name the uri, got: {message}"
        );
    }

    #[test]
    fn test_ssh_ports_at_the_edges() {
        for (uri, why) in [
            ("ssh://host:65536/p", "65536 does not fit in a port"),
            ("ssh://host:-1/p", "a port cannot be negative"),
            ("ssh://host:ssh/p", "a service name is not a port number"),
            ("ssh://host:/p", "an empty port is not a port"),
        ] {
            let error = parse(uri, &[]).expect_err(why);
            assert!(
                format!("{error:#}").contains(uri),
                "the error for {uri} must name it, got: {error:#}"
            );
        }

        assert_eq!(
            ssh_options(&parse("ssh://host:65535/p", &[]).expect("65535 is the last valid port"))
                .port,
            Some(65535)
        );
        assert_eq!(
            ssh_options(&parse("ssh://host:1/p", &[]).expect("1 is the first valid port")).port,
            Some(1)
        );
    }

    #[test]
    fn test_malformed_ssh_authorities() {
        for (uri, why) in [
            ("ssh:///p", "there is no host between the slashes"),
            ("ssh://user@/p", "a user without a host is not a connection"),
        ] {
            let error = parse(uri, &[]).expect_err(why);
            assert!(
                format!("{error:#}").contains("does not name a host"),
                "the error for {uri} must say the host is missing, got: {error:#}"
            );
        }

        let location = parse("ssh://a%40b@c/p", &[]).expect("an encoded @ in the user name");
        assert_eq!(ssh_options(&location).username.as_deref(), Some("a@b"));

        let location =
            parse("ssh://a@b@c/p", &[]).expect("the last @ separates the user from the host");
        let options = ssh_options(&location);
        assert_eq!(options.username.as_deref(), Some("a@b"));
        assert_eq!(options.host.to_string(), "c");

        let error = parse("ssh://[::1/p", &[]).expect_err("the bracket is never closed");
        assert!(
            format!("{error:#}").contains("unterminated IPv6 host"),
            "got: {error:#}"
        );
    }

    #[test]
    fn test_ipv6_hosts_are_parsed() {
        let location = parse("ssh://[::1]:22/home/a", &[]).expect("a bracketed IPv6 host and port");
        let options = ssh_options(&location);
        assert_eq!(options.host.to_string(), "::1");
        assert_eq!(options.port, Some(22));
        assert_eq!(
            location,
            ProjectLocation::Remote {
                options: RemoteConnectionOptions::Ssh(options.clone()),
                paths: vec![PathBuf::from("/home/a")],
            }
        );
        assert_eq!(
            remote_project_uri(
                &RemoteConnectionOptions::Ssh(options.clone()),
                Path::new("/home/a")
            )
            .as_deref(),
            Some("ssh://[::1]:22/home/a"),
            "the parsed form renders back to the uri it came from"
        );

        let location = parse("ssh://[fe80::1]/home/a", &[]).expect("an IPv6 host without a port");
        let options = ssh_options(&location);
        assert_eq!(options.host.to_string(), "fe80::1");
        assert_eq!(options.port, None);
    }

    #[test]
    fn test_paths_that_reach_the_scheme_guard_stay_local() {
        for path in [
            "1c://server/share",
            "my_scheme://host/p",
            r"\\?\C:\Users\andy\proj",
            r"\\server\share\proj",
        ] {
            assert_eq!(
                parse(path, &[]).unwrap_or_else(|error| panic!("parsing {path}: {error:#}")),
                ProjectLocation::Local(vec![PathBuf::from(path)]),
                "{path} is a path on this machine, not a uri"
            );
        }
    }
}
