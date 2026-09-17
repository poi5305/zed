use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::fs_rpc::FsRpc;

pub fn handles(method: &str) -> bool {
    method.starts_with("ShellEnv::")
}

pub fn dispatch(fs_rpc: &FsRpc, method: &str, params: &Value) -> Result<Value> {
    match method {
        "ShellEnv::capture" => capture(fs_rpc, params),
        _ => bail!("unknown shell env method: {method}"),
    }
}

fn refuse_if_directory_restricted(fs_rpc: &FsRpc, directory: &Path) -> Result<()> {
    if fs_rpc.path_escapes_restricted_root(directory)? {
        bail!(
            "ZED_WEB_RESTRICT_PATHS is enabled and the directory {} is outside the workspace, so the shell environment cannot be captured in this deployment",
            directory.display()
        );
    }
    Ok(())
}

fn capture(fs_rpc: &FsRpc, params: &Value) -> Result<Value> {
    let directory = PathBuf::from(required_string(params, "directory")?);
    refuse_if_directory_restricted(fs_rpc, &directory)?;

    let shell_path = required_string(params, "shell_path")?;
    let args = string_array(params, "args")?;
    let environment = smol::block_on(util::shell_env::capture(shell_path, &args, directory))?;
    Ok(json!(environment))
}

fn required_string(params: &Value, key: &str) -> Result<String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("missing {key}"))
}

fn string_array(params: &Value, key: &str) -> Result<Vec<String>> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| anyhow!("{key} must be an array of strings"))
            })
            .collect(),
        Some(_) => bail!("{key} must be an array"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    const DISPATCH_TIMEOUT: Duration = Duration::from_secs(5);

    fn dispatch_with_timeout(
        fs_rpc: &FsRpc,
        method: &str,
        params: &Value,
    ) -> Result<Value, String> {
        let method = method.to_string();
        let params = params.clone();
        let fs_rpc = fs_rpc.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if sender
                .send(dispatch(&fs_rpc, &method, &params).map_err(|error| error.to_string()))
                .is_err()
            {
                return;
            }
        });
        receiver
            .recv_timeout(DISPATCH_TIMEOUT)
            .map_err(|_| "timed out dispatching ShellEnv RPC".to_string())?
    }

    #[test]
    fn capture_refuses_when_directory_is_outside_the_restricted_root() {
        let root = tempfile::tempdir().expect("temp workspace");
        let fs_rpc = FsRpc::new(root.path().to_path_buf(), true).expect("FsRpc");
        let result = dispatch_with_timeout(
            &fs_rpc,
            "ShellEnv::capture",
            &json!({
                "shell_path": "/bin/sh",
                "args": [],
                "directory": "/tmp",
            }),
        );

        assert_eq!(
            result
                .as_ref()
                .map(|_| "captured")
                .map_err(|error| error.contains("ZED_WEB_RESTRICT_PATHS")),
            Err(true),
            "expected a ZED_WEB_RESTRICT_PATHS refusal, got {result:?}"
        );
    }

    #[test]
    fn capture_allows_the_restricted_workspace_root() {
        let root = tempfile::tempdir().expect("temp workspace");
        let fs_rpc = FsRpc::new(root.path().to_path_buf(), true).expect("FsRpc");
        assert_eq!(
            refuse_if_directory_restricted(&fs_rpc, root.path()).map_err(|error| error.to_string()),
            Ok(()),
            "capturing the served project directory is the legitimate input for ShellEnv::capture"
        );
    }

    #[test]
    fn capture_allows_a_subdirectory_of_the_restricted_workspace() {
        let root = tempfile::tempdir().expect("temp workspace");
        let nested = root.path().join("src");
        std::fs::create_dir(&nested).expect("nested project directory");
        let fs_rpc = FsRpc::new(root.path().to_path_buf(), true).expect("FsRpc");
        assert_eq!(
            refuse_if_directory_restricted(&fs_rpc, &nested).map_err(|error| error.to_string()),
            Ok(()),
            "a project subdirectory must not be treated as an escape"
        );
    }

    #[test]
    fn capture_allows_a_path_that_walks_dotdot_but_lands_inside() {
        let root = tempfile::tempdir().expect("temp workspace");
        let nested = root.path().join("src");
        std::fs::create_dir(&nested).expect("nested project directory");
        let walked = nested.join("..");
        let fs_rpc = FsRpc::new(root.path().to_path_buf(), true).expect("FsRpc");
        assert_eq!(
            refuse_if_directory_restricted(&fs_rpc, &walked).map_err(|error| error.to_string()),
            Ok(()),
            "canonicalizing src/.. must still allow the workspace root"
        );
    }

    #[test]
    fn capture_allows_a_directory_that_does_not_exist_yet_inside_the_workspace() {
        let root = tempfile::tempdir().expect("temp workspace");
        let missing = root.path().join("not-created-yet");
        let fs_rpc = FsRpc::new(root.path().to_path_buf(), true).expect("FsRpc");
        assert_eq!(
            refuse_if_directory_restricted(&fs_rpc, &missing).map_err(|error| error.to_string()),
            Ok(()),
            "a missing subdirectory of the workspace must not be refused as an escape"
        );
    }
}
