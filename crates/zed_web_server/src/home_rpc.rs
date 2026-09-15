use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::fs_rpc::FsRpc;

pub fn handles(method: &str) -> bool {
    method.starts_with("Home::")
}

pub fn dispatch(fs_rpc: &FsRpc, method: &str, _params: &Value) -> Result<Value> {
    match method {
        "Home::dirs" => dirs(fs_rpc),
        _ => bail!("unknown home method: {method}"),
    }
}

fn dirs(fs_rpc: &FsRpc) -> Result<Value> {
    let home = paths::home_dir();
    let config = paths::config_dir();
    let data = paths::data_dir();

    // A fabricated `/workspace` (or any other stand-in) would make later `~`
    // expansion fail silently against paths the restricted Fs RPC cannot read.
    if fs_rpc.path_escapes_restricted_root(home)? {
        bail!(
            "ZED_WEB_RESTRICT_PATHS is enabled and the home directory {} is outside the workspace, so ~ expansion is unavailable in this deployment",
            home.display()
        );
    }
    if fs_rpc.path_escapes_restricted_root(config)? {
        bail!(
            "ZED_WEB_RESTRICT_PATHS is enabled and the config directory {} is outside the workspace, so ~ expansion is unavailable in this deployment",
            config.display()
        );
    }
    if fs_rpc.path_escapes_restricted_root(data)? {
        bail!(
            "ZED_WEB_RESTRICT_PATHS is enabled and the data directory {} is outside the workspace, so ~ expansion is unavailable in this deployment",
            data.display()
        );
    }

    Ok(json!({
        "home": home.display().to_string(),
        "config": config.display().to_string(),
        "data": data.display().to_string(),
    }))
}
