use std::{
    io::Read as _,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};

/// The server must keep starting when web.json is unreadable JSON but restrict_paths is already
/// decided on the command line: before ssh support, nothing read the file in that case.
#[test]
fn invalid_web_json_does_not_block_startup_when_restrict_paths_is_decided() -> Result<()> {
    let root = tempfile::tempdir()?;
    let static_root = tempfile::tempdir()?;
    std::fs::create_dir_all(root.path().join(".zed"))?;
    std::fs::write(root.path().join(".zed/web.json"), "{bad")?;

    let mut child = Command::new(env!("CARGO_BIN_EXE_zed-web-server"))
        .arg(root.path())
        .arg(static_root.path())
        .args(["--port", "0", "--restrict-paths"])
        .env_remove("ZED_WEB_ALLOW_SSH")
        .env_remove("ZED_WEB_RESTRICT_PATHS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning zed-web-server")?;

    let deadline = Instant::now() + Duration::from_secs(4);
    let exited = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let Some(status) = exited else {
        child.kill()?;
        child.wait()?;
        return Ok(());
    };
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_string(&mut stderr)?;
    }
    panic!(
        "expected the server to keep running (restrict_paths is set by --restrict-paths and ssh is not requested), \
         but it exited with {status}; stderr: {stderr:?}"
    );
}
