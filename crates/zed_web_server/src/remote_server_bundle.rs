use std::{
    collections::HashSet,
    fs,
    io::Read as _,
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{Context as _, Result, anyhow, bail};
use remote::{BundledRemoteServer, RemoteArch, RemoteOs, RemotePlatform};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

/// The one place that decides where the bundle lives: next to the server binary, so a copied
/// `dist/bin` carries its remote servers with it. web/build.sh writes to `dist/bin/remote`.
pub const BUNDLE_DIRECTORY_NAME: &str = "remote";
pub const MANIFEST_FILE_NAME: &str = "manifest.json";
pub const IMPORT_SCRIPT: &str = "web/scripts/import-remote-server.sh";

const KNOWN_OPERATING_SYSTEMS: [RemoteOs; 3] =
    [RemoteOs::Linux, RemoteOs::MacOs, RemoteOs::Windows];
const KNOWN_ARCHITECTURES: [RemoteArch; 2] = [RemoteArch::X86_64, RemoteArch::Aarch64];

#[derive(Debug, Deserialize)]
struct ManifestFile {
    commit: String,
    binaries: Vec<ManifestBinary>,
}

#[derive(Debug, Deserialize)]
struct ManifestBinary {
    os: String,
    arch: String,
    file: String,
    sha256: String,
}

#[derive(Debug)]
struct BundleEntry {
    platform: RemotePlatform,
    path: PathBuf,
    sha256: String,
}

#[derive(Debug)]
pub struct RemoteServerBundle {
    commit: String,
    entries: Vec<BundleEntry>,
    // Hashing a remote server takes a noticeable moment, so a file is only hashed once it
    // is asked for, and a file that verified is not hashed again on every connection.
    verified: Mutex<HashSet<usize>>,
}

impl RemoteServerBundle {
    pub fn default_directory() -> Result<PathBuf> {
        let executable = std::env::current_exe().context("locating the zed-web-server binary")?;
        let parent = executable
            .parent()
            .ok_or_else(|| anyhow!("{} has no parent directory", executable.display()))?;
        Ok(parent.join(BUNDLE_DIRECTORY_NAME))
    }

    pub fn load(directory: &Path) -> Result<Self> {
        let manifest_path = directory.join(MANIFEST_FILE_NAME);
        let text = fs::read_to_string(&manifest_path).with_context(|| {
            format!(
                "reading {}: this Zed Web build has no remote server bundle. \
                 Run web/build.sh to build one, and {IMPORT_SCRIPT} to add other platforms",
                manifest_path.display()
            )
        })?;
        Self::parse(directory, &text)
            .with_context(|| format!("invalid remote server manifest {}", manifest_path.display()))
    }

    pub fn parse(directory: &Path, manifest_text: &str) -> Result<Self> {
        let manifest: ManifestFile =
            serde_json::from_str(manifest_text).context("parsing the manifest")?;

        let commit = manifest.commit.trim();
        if commit.is_empty() {
            bail!("the manifest's commit is empty");
        }

        let mut entries: Vec<BundleEntry> = Vec::with_capacity(manifest.binaries.len());
        for binary in manifest.binaries {
            let platform = parse_platform(&binary.os, &binary.arch)?;
            if entries
                .iter()
                .any(|entry| same_platform(entry.platform, platform))
            {
                bail!(
                    "the manifest lists {}-{} more than once",
                    binary.os,
                    binary.arch
                );
            }
            let sha256 = binary.sha256.trim().to_ascii_lowercase();
            if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                bail!(
                    "the sha256 of {}-{} is not 64 hex digits: {:?}",
                    binary.os,
                    binary.arch,
                    binary.sha256
                );
            }
            entries.push(BundleEntry {
                platform,
                path: directory.join(plain_file_name(&binary.file)?),
                sha256,
            });
        }

        Ok(Self {
            commit: commit.to_owned(),
            entries,
            verified: Mutex::new(HashSet::new()),
        })
    }

    pub fn commit(&self) -> &str {
        &self.commit
    }

    pub fn platforms(&self) -> Vec<RemotePlatform> {
        self.entries.iter().map(|entry| entry.platform).collect()
    }

    /// The file for `platform`, after checking it still hashes to what the manifest says.
    pub fn server_for(&self, platform: RemotePlatform) -> Result<BundledRemoteServer> {
        let Some((index, entry)) = self
            .entries
            .iter()
            .enumerate()
            .find(|(_, entry)| same_platform(entry.platform, platform))
        else {
            return Err(missing_platform_error(platform, &self.platforms()));
        };

        let already_verified = self
            .verified
            .lock()
            .map_err(|_| anyhow!("the remote server bundle's verification state is poisoned"))?
            .contains(&index);
        if !already_verified {
            let actual = sha256_of_file(&entry.path)?;
            if actual != entry.sha256 {
                bail!(
                    "{} has sha256 {actual}, but the manifest says {}. \
                     Rebuild with web/build.sh or re-import it with {IMPORT_SCRIPT}",
                    entry.path.display(),
                    entry.sha256
                );
            }
            self.verified
                .lock()
                .map_err(|_| anyhow!("the remote server bundle's verification state is poisoned"))?
                .insert(index);
        }

        Ok(BundledRemoteServer {
            path: entry.path.clone(),
            version: self.commit.clone(),
            content_id: entry.sha256.clone(),
        })
    }
}

pub fn missing_platform_error(
    platform: RemotePlatform,
    available: &[RemotePlatform],
) -> anyhow::Error {
    let available = if available.is_empty() {
        "none".to_owned()
    } else {
        available
            .iter()
            .map(|platform| describe(*platform))
            .collect::<Vec<_>>()
            .join(", ")
    };
    anyhow!(
        "this Zed Web build has no remote server for {} (bundled: {available}). \
         Build remote_server on a {} machine and add it with \
         {IMPORT_SCRIPT} <binary> {} {}",
        describe(platform),
        describe(platform),
        platform.os.as_str(),
        platform.arch.as_str(),
    )
}

fn describe(platform: RemotePlatform) -> String {
    format!("{}-{}", platform.os.as_str(), platform.arch.as_str())
}

fn same_platform(left: RemotePlatform, right: RemotePlatform) -> bool {
    left.os == right.os && left.arch == right.arch
}

fn parse_platform(os: &str, arch: &str) -> Result<RemotePlatform> {
    let os = KNOWN_OPERATING_SYSTEMS
        .into_iter()
        .find(|candidate| candidate.as_str() == os)
        .ok_or_else(|| anyhow!("unknown os {os:?} in the manifest"))?;
    let arch = KNOWN_ARCHITECTURES
        .into_iter()
        .find(|candidate| candidate.as_str() == arch)
        .ok_or_else(|| anyhow!("unknown arch {arch:?} in the manifest"))?;
    Ok(RemotePlatform { os, arch })
}

/// The manifest is data next to the binaries, so a file entry must not be able to point
/// anywhere but into the bundle directory.
fn plain_file_name(file: &str) -> Result<&str> {
    let is_plain =
        !file.is_empty() && file != "." && file != ".." && !file.contains(['/', '\\', '\0']);
    if !is_plain {
        bail!("manifest file {file:?} must be a plain file name inside the bundle directory");
    }
    Ok(file)
}

fn sha256_of_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("opening bundled remote server {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("reading bundled remote server {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha256_hex(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    fn platform(os: RemoteOs, arch: RemoteArch) -> RemotePlatform {
        RemotePlatform { os, arch }
    }

    fn write_bundle(
        directory: &Path,
        commit: &str,
        binaries: &[(&str, &str, &str, &[u8])],
    ) -> Result<()> {
        let mut entries = Vec::new();
        for (os, arch, file, content) in binaries {
            fs::write(directory.join(file), content)?;
            entries.push(format!(
                r#"{{"os": "{os}", "arch": "{arch}", "file": "{file}", "sha256": "{}"}}"#,
                sha256_hex(content)
            ));
        }
        fs::write(
            directory.join(MANIFEST_FILE_NAME),
            format!(
                r#"{{"commit": "{commit}", "binaries": [{}]}}"#,
                entries.join(",")
            ),
        )?;
        Ok(())
    }

    #[test]
    fn chooses_the_file_for_the_requested_platform() -> Result<()> {
        let directory = tempfile::tempdir()?;
        write_bundle(
            directory.path(),
            "abc123",
            &[
                (
                    "linux",
                    "x86_64",
                    "zed-remote-server-linux-x86_64",
                    b"linux-x86",
                ),
                (
                    "linux",
                    "aarch64",
                    "zed-remote-server-linux-aarch64",
                    b"linux-arm",
                ),
                (
                    "macos",
                    "aarch64",
                    "zed-remote-server-macos-aarch64",
                    b"mac-arm",
                ),
            ],
        )?;
        let bundle = RemoteServerBundle::load(directory.path())?;

        let linux_arm = bundle.server_for(platform(RemoteOs::Linux, RemoteArch::Aarch64))?;
        assert_eq!(
            linux_arm.path,
            directory.path().join("zed-remote-server-linux-aarch64")
        );
        assert_eq!(linux_arm.version, "abc123");
        let mac_arm = bundle.server_for(platform(RemoteOs::MacOs, RemoteArch::Aarch64))?;
        assert_eq!(
            mac_arm.path,
            directory.path().join("zed-remote-server-macos-aarch64")
        );
        assert_eq!(bundle.commit(), "abc123");
        assert_eq!(bundle.platforms().len(), 3);
        Ok(())
    }

    #[test]
    fn missing_platform_names_the_import_script() -> Result<()> {
        let directory = tempfile::tempdir()?;
        write_bundle(
            directory.path(),
            "abc123",
            &[(
                "linux",
                "x86_64",
                "zed-remote-server-linux-x86_64",
                b"linux-x86",
            )],
        )?;
        let bundle = RemoteServerBundle::load(directory.path())?;

        let error = bundle
            .server_for(platform(RemoteOs::MacOs, RemoteArch::X86_64))
            .expect_err("macos-x86_64 is not in the manifest");
        let message = format!("{error:#}");
        assert!(message.contains("import-remote-server.sh"), "{message}");
        assert!(message.contains("macos-x86_64"), "{message}");
        assert!(message.contains("linux-x86_64"), "{message}");
        Ok(())
    }

    #[test]
    fn server_content_id_is_the_manifest_sha256() -> Result<()> {
        let directory = tempfile::tempdir()?;
        write_bundle(
            directory.path(),
            "abc123",
            &[
                (
                    "linux",
                    "x86_64",
                    "zed-remote-server-linux-x86_64",
                    b"linux-x86",
                ),
                (
                    "linux",
                    "aarch64",
                    "zed-remote-server-linux-aarch64",
                    b"linux-arm",
                ),
            ],
        )?;
        let bundle = RemoteServerBundle::load(directory.path())?;

        let x86 = bundle.server_for(platform(RemoteOs::Linux, RemoteArch::X86_64))?;
        let arm = bundle.server_for(platform(RemoteOs::Linux, RemoteArch::Aarch64))?;
        assert_eq!(x86.content_id, sha256_hex(b"linux-x86"));
        assert_eq!(arm.content_id, sha256_hex(b"linux-arm"));
        // The commit is shared by both, so it cannot tell the two binaries apart.
        assert_eq!(x86.version, arm.version);
        assert_ne!(x86.content_id, arm.content_id);
        Ok(())
    }

    #[test]
    fn sha256_mismatch_is_an_error() -> Result<()> {
        let directory = tempfile::tempdir()?;
        write_bundle(
            directory.path(),
            "abc123",
            &[(
                "linux",
                "x86_64",
                "zed-remote-server-linux-x86_64",
                b"original",
            )],
        )?;
        fs::write(
            directory.path().join("zed-remote-server-linux-x86_64"),
            b"tampered",
        )?;
        let bundle = RemoteServerBundle::load(directory.path())?;

        let error = bundle
            .server_for(platform(RemoteOs::Linux, RemoteArch::X86_64))
            .expect_err("the file no longer matches the manifest");
        let message = format!("{error:#}");
        assert!(message.contains(&sha256_hex(b"tampered")), "{message}");
        assert!(message.contains(&sha256_hex(b"original")), "{message}");
        Ok(())
    }

    #[test]
    fn a_verified_file_is_not_hashed_again() -> Result<()> {
        let directory = tempfile::tempdir()?;
        write_bundle(
            directory.path(),
            "abc123",
            &[(
                "linux",
                "x86_64",
                "zed-remote-server-linux-x86_64",
                b"original",
            )],
        )?;
        let bundle = RemoteServerBundle::load(directory.path())?;
        let linux = platform(RemoteOs::Linux, RemoteArch::X86_64);
        bundle.server_for(linux)?;

        fs::remove_file(directory.path().join("zed-remote-server-linux-x86_64"))?;
        bundle.server_for(linux)?;
        Ok(())
    }

    #[test]
    fn a_listed_file_that_is_missing_is_an_error() -> Result<()> {
        let directory = tempfile::tempdir()?;
        write_bundle(
            directory.path(),
            "abc123",
            &[(
                "linux",
                "x86_64",
                "zed-remote-server-linux-x86_64",
                b"original",
            )],
        )?;
        fs::remove_file(directory.path().join("zed-remote-server-linux-x86_64"))?;
        let bundle = RemoteServerBundle::load(directory.path())?;

        let error = bundle
            .server_for(platform(RemoteOs::Linux, RemoteArch::X86_64))
            .expect_err("the binary is gone");
        assert!(format!("{error:#}").contains("zed-remote-server-linux-x86_64"));
        Ok(())
    }

    #[test]
    fn manifest_without_a_file_is_an_error_naming_the_scripts() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let error =
            RemoteServerBundle::load(directory.path()).expect_err("there is no manifest.json");
        let message = format!("{error:#}");
        assert!(message.contains("web/build.sh"), "{message}");
        assert!(message.contains("import-remote-server.sh"), "{message}");
        Ok(())
    }

    fn parse_error(text: &str) -> String {
        match RemoteServerBundle::parse(Path::new("/bundle"), text) {
            Ok(_) => panic!("expected {text} to be rejected"),
            Err(error) => format!("{error:#}"),
        }
    }

    const GOOD_SHA: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    #[test]
    fn missing_fields_are_errors() {
        assert!(parse_error(r#"{"binaries": []}"#).contains("commit"));
        assert!(parse_error(r#"{"commit": "abc"}"#).contains("binaries"));
        let no_sha =
            r#"{"commit": "abc", "binaries": [{"os": "linux", "arch": "x86_64", "file": "f"}]}"#;
        assert!(parse_error(no_sha).contains("sha256"));
        let no_file = format!(
            r#"{{"commit": "abc", "binaries": [{{"os": "linux", "arch": "x86_64", "sha256": "{GOOD_SHA}"}}]}}"#
        );
        assert!(parse_error(&no_file).contains("file"));
        assert!(parse_error(r#"{"commit": "  ", "binaries": []}"#).contains("commit"));
    }

    #[test]
    fn unknown_platform_is_an_error() {
        let unknown_os = format!(
            r#"{{"commit": "abc", "binaries": [{{"os": "plan9", "arch": "x86_64", "file": "f", "sha256": "{GOOD_SHA}"}}]}}"#
        );
        assert!(parse_error(&unknown_os).contains("plan9"));
        let unknown_arch = format!(
            r#"{{"commit": "abc", "binaries": [{{"os": "linux", "arch": "riscv64", "file": "f", "sha256": "{GOOD_SHA}"}}]}}"#
        );
        assert!(parse_error(&unknown_arch).contains("riscv64"));
    }

    #[test]
    fn malformed_entries_are_errors() {
        let bad_sha = r#"{"commit": "abc", "binaries": [{"os": "linux", "arch": "x86_64", "file": "f", "sha256": "xyz"}]}"#;
        assert!(parse_error(bad_sha).contains("sha256"));
        let duplicate = format!(
            r#"{{"commit": "abc", "binaries": [
                {{"os": "linux", "arch": "x86_64", "file": "a", "sha256": "{GOOD_SHA}"}},
                {{"os": "linux", "arch": "x86_64", "file": "b", "sha256": "{GOOD_SHA}"}}]}}"#
        );
        assert!(parse_error(&duplicate).contains("more than once"));
        assert!(parse_error("not json").contains("parsing the manifest"));
        for escaping in ["../evil", "/etc/passwd", "sub/dir", "..", ""] {
            let text = format!(
                r#"{{"commit": "abc", "binaries": [{{"os": "linux", "arch": "x86_64", "file": "{escaping}", "sha256": "{GOOD_SHA}"}}]}}"#
            );
            assert!(
                parse_error(&text).contains("plain file name"),
                "{escaping:?} was accepted"
            );
        }
    }

    #[test]
    fn upper_case_sha256_in_the_manifest_matches() -> Result<()> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("server"), b"content")?;
        let manifest = format!(
            r#"{{"commit": "abc", "binaries": [{{"os": "linux", "arch": "x86_64", "file": "server", "sha256": "{}"}}]}}"#,
            sha256_hex(b"content").to_ascii_uppercase()
        );
        let bundle = RemoteServerBundle::parse(directory.path(), &manifest)?;
        bundle.server_for(platform(RemoteOs::Linux, RemoteArch::X86_64))?;
        Ok(())
    }

    #[cfg(unix)]
    fn run_import_script(dist_dir: &Path, arguments: &[&str]) -> Result<std::process::Output> {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../")
            .join(IMPORT_SCRIPT);
        std::process::Command::new(script)
            .args(arguments)
            .env("ZED_WEB_DIST_DIR", dist_dir)
            .output()
            .context("running import-remote-server.sh")
    }

    #[cfg(unix)]
    fn write_fake_server(path: &Path, version_output: &str) -> Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        fs::write(path, format!("#!/bin/sh\nprintf '%s' '{version_output}'\n"))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
        Ok(())
    }

    // The script is the only writer of manifest.json, so this is the test that the layout it
    // writes is the layout this module reads.
    #[cfg(unix)]
    #[test]
    fn manifest_written_by_the_import_script_loads() -> Result<()> {
        let dist = tempfile::tempdir()?;
        let sources = tempfile::tempdir()?;
        let linux = sources.path().join("linux-server");
        let mac = sources.path().join("mac-server");
        write_fake_server(&linux, "shell noise\nabc123\n\n")?;
        write_fake_server(&mac, "abc123\n")?;

        let output = run_import_script(
            dist.path(),
            &[linux.to_str().context("path")?, "linux", "x86_64"],
        )?;
        assert!(output.status.success(), "{output:?}");
        let output = run_import_script(
            dist.path(),
            &[
                "--commit",
                "abc123",
                mac.to_str().context("path")?,
                "macos",
                "aarch64",
            ],
        )?;
        assert!(output.status.success(), "{output:?}");
        // Importing the same platform again replaces its entry.
        let output = run_import_script(
            dist.path(),
            &[linux.to_str().context("path")?, "linux", "x86_64"],
        )?;
        assert!(output.status.success(), "{output:?}");

        let directory = dist.path().join("bin").join(BUNDLE_DIRECTORY_NAME);
        let bundle = RemoteServerBundle::load(&directory)?;
        assert_eq!(bundle.commit(), "abc123");
        assert_eq!(bundle.platforms().len(), 2);
        let server = bundle.server_for(platform(RemoteOs::Linux, RemoteArch::X86_64))?;
        assert_eq!(
            server.path,
            directory.join("zed-remote-server-linux-x86_64")
        );
        bundle.server_for(platform(RemoteOs::MacOs, RemoteArch::Aarch64))?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn import_script_refuses_to_mix_commits() -> Result<()> {
        let dist = tempfile::tempdir()?;
        let sources = tempfile::tempdir()?;
        let first = sources.path().join("first");
        let second = sources.path().join("second");
        write_fake_server(&first, "commit-one\n")?;
        write_fake_server(&second, "commit-two\n")?;

        let output = run_import_script(
            dist.path(),
            &[first.to_str().context("path")?, "linux", "x86_64"],
        )?;
        assert!(output.status.success(), "{output:?}");
        let output = run_import_script(
            dist.path(),
            &[second.to_str().context("path")?, "linux", "aarch64"],
        )?;
        assert!(!output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("commit-one"));

        let bundle =
            RemoteServerBundle::load(&dist.path().join("bin").join(BUNDLE_DIRECTORY_NAME))?;
        assert_eq!(bundle.platforms().len(), 1);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn import_script_rejects_unknown_platforms_and_unreadable_versions() -> Result<()> {
        let dist = tempfile::tempdir()?;
        let sources = tempfile::tempdir()?;
        let server = sources.path().join("server");
        write_fake_server(&server, "abc123\n")?;
        let server = server.to_str().context("path")?;

        let output = run_import_script(dist.path(), &[server, "plan9", "x86_64"])?;
        assert!(!output.status.success());
        let output = run_import_script(dist.path(), &[server, "linux", "riscv64"])?;
        assert!(!output.status.success());

        let silent = sources.path().join("silent");
        write_fake_server(&silent, "")?;
        let output = run_import_script(
            dist.path(),
            &[silent.to_str().context("path")?, "linux", "x86_64"],
        )?;
        assert!(!output.status.success(), "{output:?}");
        assert!(!dist.path().join("bin/remote/manifest.json").exists());
        Ok(())
    }

    // A wrong --commit would load fine and only fail after every connect uploads the binary.
    #[cfg(unix)]
    #[test]
    fn import_script_refuses_a_commit_the_binary_does_not_carry() -> Result<()> {
        let dist = tempfile::tempdir()?;
        let sources = tempfile::tempdir()?;
        let server = sources.path().join("mac-server");
        write_fake_server(&server, "commit-b\n")?;

        let output = run_import_script(
            dist.path(),
            &[
                "--commit",
                "commit-a",
                server.to_str().context("path")?,
                "macos",
                "aarch64",
            ],
        )?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "expected --commit commit-a to be refused for a binary built at commit-b, \
             got {} with stderr {stderr:?}",
            output.status
        );
        assert!(stderr.contains("commit-a"), "{stderr}");
        assert!(!dist.path().join("bin/remote/manifest.json").exists());
        Ok(())
    }

    // `version` prints `<build id>+<sha>` when ZED_BUILD_ID was set; only the sha is a literal
    // in the binary, so that form must still import.
    #[cfg(unix)]
    #[test]
    fn import_script_accepts_a_build_id_prefixed_commit() -> Result<()> {
        let dist = tempfile::tempdir()?;
        let sources = tempfile::tempdir()?;
        let server = sources.path().join("mac-server");
        write_fake_server(&server, "abc123\n")?;

        let output = run_import_script(
            dist.path(),
            &[
                "--commit",
                "42+abc123",
                server.to_str().context("path")?,
                "macos",
                "aarch64",
            ],
        )?;
        assert!(output.status.success(), "{output:?}");
        let bundle =
            RemoteServerBundle::load(&dist.path().join("bin").join(BUNDLE_DIRECTORY_NAME))?;
        assert_eq!(bundle.commit(), "42+abc123");
        Ok(())
    }

    #[test]
    fn manifest_with_no_binaries_loads_and_serves_nothing() -> Result<()> {
        let bundle = RemoteServerBundle::parse(
            Path::new("/bundle"),
            r#"{"commit": "abc", "binaries": []}"#,
        )?;
        assert!(
            bundle
                .server_for(platform(RemoteOs::Linux, RemoteArch::X86_64))
                .is_err()
        );
        Ok(())
    }
}
