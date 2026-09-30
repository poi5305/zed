//! The parts of the browser's ssh relay that do not touch a browser API, so they can be tested
//! natively. `transport::web_relay` wires them to the RPC client and the byte channel.

use anyhow::{Context as _, Result, anyhow, bail};
use serde::Deserialize as _;
use serde_json::Value;
use util::paths::PathStyle;

use crate::{RemoteArch, RemoteOs, RemotePlatform, SshCommandRecipe};

/// The remote `proxy` kills a running server with the same identifier when it is not asked to
/// reconnect, so a browser and a desktop that picked the same workspace id on one host would
/// keep killing each other's server without a prefix only the browser uses.
pub fn prefixed_identifier(identifier: &str) -> String {
    format!("web-{identifier}")
}

#[derive(Clone, Debug)]
pub struct ConnectResponse {
    pub host_id: String,
    pub handle_id: String,
    pub platform: RemotePlatform,
    pub os_version: Option<String>,
    pub path_style: PathStyle,
    pub shell: String,
    pub default_system_shell: String,
    pub recipe: SshCommandRecipe,
}

pub fn parse_connect_response(value: &Value) -> Result<ConnectResponse> {
    let platform = value
        .get("platform")
        .context("RemoteSsh::connect result has no platform")?;
    let os_version = match value.get("os_version") {
        None | Some(Value::Null) => None,
        Some(Value::String(os_version)) => Some(os_version.clone()),
        Some(other) => bail!("RemoteSsh::connect result os_version is not a string: {other}"),
    };
    let recipe = value
        .get("recipe")
        .context("RemoteSsh::connect result has no recipe")?;
    Ok(ConnectResponse {
        host_id: string_field(value, "host_id")?,
        handle_id: string_field(value, "handle_id")?,
        platform: RemotePlatform {
            os: parse_os(&string_field(platform, "os")?)?,
            arch: parse_arch(&string_field(platform, "arch")?)?,
        },
        os_version,
        path_style: parse_path_style(&string_field(value, "path_style")?)?,
        shell: string_field(value, "shell")?,
        default_system_shell: string_field(value, "default_system_shell")?,
        recipe: SshCommandRecipe::deserialize(recipe)
            .context("RemoteSsh::connect result has an invalid recipe")?,
    })
}

/// Maps the close of a `/remote/channel` socket to what `start_proxy` returns.
pub fn exit_code_from_close(code: u16, reason: &str) -> Result<i32> {
    match code {
        1000 => {
            let exit_code = serde_json::from_str::<Value>(reason)
                .ok()
                .and_then(|reason| reason.get("exit_code").and_then(Value::as_i64))
                .and_then(|exit_code| i32::try_from(exit_code).ok());
            exit_code.ok_or_else(|| {
                anyhow!("the ssh relay closed without the proxy's exit code: {reason:?}")
            })
        }
        1006 => Err(anyhow!(
            "relay disconnected: the ssh relay socket closed without a close frame"
        )),
        4400 => Err(anyhow!(
            "the ssh relay refused the channel token (4400): {reason}"
        )),
        4409 => Err(anyhow!(
            "the ssh relay channel was already attached (4409): {reason}"
        )),
        1011 => Err(anyhow!("the ssh relay failed (1011): {reason}")),
        _ => Err(anyhow!("the ssh relay closed with code {code}: {reason}")),
    }
}

/// The next finished item in `queue`.
///
/// An empty `FuturesUnordered` reports the end of the stream (`None`) and then stays
/// terminated. `select_next_some` panics if it is polled again after that, and the connect
/// loop reaches this before any prompt or answer exists. An empty queue therefore stays
/// pending. It must not wake itself: the caller builds a new future after pushing work,
/// and a self-wake would spin until that push.
#[cfg(any(test, target_family = "wasm"))]
pub fn next_in_flight<Fut>(
    queue: &mut futures::stream::FuturesUnordered<Fut>,
) -> impl std::future::Future<Output = Fut::Output> + '_
where
    Fut: std::future::Future + Unpin,
{
    use futures::Stream as _;

    std::future::poll_fn(move |context| {
        if queue.is_empty() {
            std::task::Poll::Pending
        } else {
            match std::pin::Pin::new(&mut *queue).poll_next(context) {
                std::task::Poll::Ready(Some(value)) => std::task::Poll::Ready(value),
                std::task::Poll::Ready(None) | std::task::Poll::Pending => std::task::Poll::Pending,
            }
        }
    })
}

fn string_field(value: &Value, key: &str) -> Result<String> {
    match value.get(key) {
        Some(Value::String(field)) => Ok(field.clone()),
        Some(other) => bail!("RemoteSsh::connect result {key} is not a string: {other}"),
        None => bail!("RemoteSsh::connect result has no {key}"),
    }
}

fn parse_os(os: &str) -> Result<RemoteOs> {
    [RemoteOs::Linux, RemoteOs::MacOs, RemoteOs::Windows]
        .into_iter()
        .find(|candidate| candidate.as_str() == os)
        .ok_or_else(|| anyhow!("unknown remote os {os:?}"))
}

fn parse_arch(arch: &str) -> Result<RemoteArch> {
    [RemoteArch::X86_64, RemoteArch::Aarch64]
        .into_iter()
        .find(|candidate| candidate.as_str() == arch)
        .ok_or_else(|| anyhow!("unknown remote arch {arch:?}"))
}

fn parse_path_style(path_style: &str) -> Result<PathStyle> {
    match path_style {
        "unix" => Ok(PathStyle::Unix),
        "windows" => Ok(PathStyle::Windows),
        _ => Err(anyhow!("unknown remote path style {path_style:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RecipePathStyle;
    use serde_json::json;

    fn response() -> Value {
        json!({
            "host_id": "h-0123456789abcdef0123456789abcdef",
            "handle_id": "k-0123456789abcdef0123456789abcdef",
            "platform": {"os": "linux", "arch": "x86_64"},
            "os_version": "ubuntu 24.04",
            "path_style": "unix",
            "shell": "/bin/bash",
            "default_system_shell": "/bin/sh",
            "recipe": {
                "ssh_options": ["-o", "ControlMaster=no", "-o", "ControlPath=/tmp/zed-ssh-sessionAbC/ssh.sock"],
                "destination": "andy@devbox",
                "env": {},
                "shell": "/bin/bash",
                "is_windows": false,
                "path_style": "unix"
            }
        })
    }

    fn error_text(result: Result<impl std::fmt::Debug>) -> String {
        match result {
            Ok(value) => panic!("expected an error, got Ok({value:?})"),
            Err(error) => format!("{error:#}"),
        }
    }

    #[test]
    fn prefixed_identifier_adds_web_prefix() {
        assert_eq!(
            prefixed_identifier("dev-workspace-12"),
            "web-dev-workspace-12"
        );
        assert_eq!(prefixed_identifier(""), "web-");
    }

    #[test]
    fn parses_every_connect_response_field() -> Result<()> {
        let parsed = parse_connect_response(&response())?;
        assert_eq!(parsed.host_id, "h-0123456789abcdef0123456789abcdef");
        assert_eq!(parsed.handle_id, "k-0123456789abcdef0123456789abcdef");
        assert_eq!(parsed.platform.os, RemoteOs::Linux);
        assert_eq!(parsed.platform.arch, RemoteArch::X86_64);
        assert_eq!(parsed.os_version.as_deref(), Some("ubuntu 24.04"));
        assert_eq!(parsed.path_style, PathStyle::Unix);
        assert_eq!(parsed.shell, "/bin/bash");
        assert_eq!(parsed.default_system_shell, "/bin/sh");
        assert_eq!(
            parsed.recipe,
            SshCommandRecipe {
                ssh_options: vec![
                    "-o".into(),
                    "ControlMaster=no".into(),
                    "-o".into(),
                    "ControlPath=/tmp/zed-ssh-sessionAbC/ssh.sock".into(),
                ],
                destination: "andy@devbox".into(),
                env: Default::default(),
                shell: "/bin/bash".into(),
                is_windows: false,
                path_style: RecipePathStyle::Unix,
            }
        );
        Ok(())
    }

    #[test]
    fn parses_every_os_and_arch_spelling() -> Result<()> {
        for os in [RemoteOs::Linux, RemoteOs::MacOs, RemoteOs::Windows] {
            for arch in [RemoteArch::X86_64, RemoteArch::Aarch64] {
                let mut value = response();
                value["platform"] = json!({"os": os.as_str(), "arch": arch.as_str()});
                let parsed = parse_connect_response(&value)?;
                assert_eq!((parsed.platform.os, parsed.platform.arch), (os, arch));
            }
        }
        Ok(())
    }

    #[test]
    fn parses_windows_path_style() -> Result<()> {
        let mut value = response();
        value["path_style"] = json!("windows");
        value["recipe"]["path_style"] = json!("windows");
        value["recipe"]["is_windows"] = json!(true);
        let parsed = parse_connect_response(&value)?;
        assert_eq!(parsed.path_style, PathStyle::Windows);
        assert_eq!(parsed.recipe.path_style, RecipePathStyle::Windows);
        assert!(parsed.recipe.is_windows);
        Ok(())
    }

    #[test]
    fn null_or_missing_os_version_is_none() -> Result<()> {
        let mut value = response();
        value["os_version"] = Value::Null;
        assert_eq!(parse_connect_response(&value)?.os_version, None);
        if let Some(object) = value.as_object_mut() {
            object.remove("os_version");
        }
        assert_eq!(parse_connect_response(&value)?.os_version, None);
        Ok(())
    }

    #[test]
    fn non_string_os_version_is_an_error() {
        let mut value = response();
        value["os_version"] = json!(24);
        assert!(error_text(parse_connect_response(&value)).contains("os_version"));
    }

    #[test]
    fn unknown_os_is_an_error() {
        let mut value = response();
        value["platform"]["os"] = json!("freebsd");
        assert!(error_text(parse_connect_response(&value)).contains("freebsd"));
    }

    #[test]
    fn os_spelling_is_case_sensitive() {
        let mut value = response();
        value["platform"]["os"] = json!("Linux");
        assert!(error_text(parse_connect_response(&value)).contains("Linux"));
    }

    #[test]
    fn unknown_arch_is_an_error() {
        let mut value = response();
        value["platform"]["arch"] = json!("riscv64");
        assert!(error_text(parse_connect_response(&value)).contains("riscv64"));
    }

    #[test]
    fn unknown_path_style_is_an_error() {
        let mut value = response();
        value["path_style"] = json!("posix");
        assert!(error_text(parse_connect_response(&value)).contains("posix"));
    }

    #[test]
    fn every_missing_field_is_an_error_naming_it() {
        for key in [
            "host_id",
            "handle_id",
            "platform",
            "path_style",
            "shell",
            "default_system_shell",
            "recipe",
        ] {
            let mut value = response();
            if let Some(object) = value.as_object_mut() {
                object.remove(key);
            }
            let error = error_text(parse_connect_response(&value));
            assert!(error.contains(key), "missing {key}: {error}");
        }
        for key in ["os", "arch"] {
            let mut value = response();
            if let Some(platform) = value["platform"].as_object_mut() {
                platform.remove(key);
            }
            let error = error_text(parse_connect_response(&value));
            assert!(error.contains(key), "missing platform.{key}: {error}");
        }
    }

    #[test]
    fn every_non_string_field_is_an_error_naming_it() {
        for key in [
            "host_id",
            "handle_id",
            "path_style",
            "shell",
            "default_system_shell",
        ] {
            let mut value = response();
            value[key] = json!(1);
            let error = error_text(parse_connect_response(&value));
            assert!(error.contains(key), "non-string {key}: {error}");
        }
    }

    #[test]
    fn invalid_recipe_is_an_error() {
        let mut value = response();
        value["recipe"]["path_style"] = json!("posix");
        assert!(error_text(parse_connect_response(&value)).contains("recipe"));
        let mut value = response();
        value["recipe"] = json!("ssh devbox");
        assert!(error_text(parse_connect_response(&value)).contains("recipe"));
    }

    #[test]
    fn normal_close_with_exit_code_is_ok() -> Result<()> {
        assert_eq!(exit_code_from_close(1000, r#"{"exit_code":0}"#)?, 0);
        assert_eq!(exit_code_from_close(1000, r#"{"exit_code":90}"#)?, 90);
        assert_eq!(exit_code_from_close(1000, r#"{"exit_code": -1}"#)?, -1);
        Ok(())
    }

    #[test]
    fn normal_close_without_parsable_exit_code_is_an_error() {
        for reason in [
            "",
            "client closed",
            "{}",
            r#"{"exit_code":"90"}"#,
            r#"{"exit_code":1.5}"#,
            r#"{"exit_code":4294967296}"#,
            r#"{"exit_code":null}"#,
        ] {
            let error = error_text(exit_code_from_close(1000, reason));
            assert!(error.contains("exit code"), "reason {reason:?}: {error}");
        }
    }

    #[test]
    fn invalid_token_close_is_an_error() {
        let error = error_text(exit_code_from_close(4400, "invalid channel token"));
        assert!(
            error.contains("4400") && error.contains("invalid channel token"),
            "{error}"
        );
    }

    #[test]
    fn already_attached_close_is_an_error() {
        let error = error_text(exit_code_from_close(4409, "channel already attached"));
        assert!(
            error.contains("4409") && error.contains("channel already attached"),
            "{error}"
        );
    }

    #[test]
    fn relay_failure_close_is_an_error_with_its_message() {
        let error = error_text(exit_code_from_close(1011, "failed to decode envelope"));
        assert!(
            error.contains("1011") && error.contains("failed to decode envelope"),
            "{error}"
        );
    }

    #[test]
    fn abnormal_close_is_relay_disconnected() {
        assert!(error_text(exit_code_from_close(1006, "")).contains("relay disconnected"));
    }

    #[test]
    fn a_normal_close_reason_does_not_rescue_other_codes() {
        for code in [1001, 1003, 1006, 1011, 4003, 4400, 4409] {
            let error = error_text(exit_code_from_close(code, r#"{"exit_code":0}"#));
            assert!(!error.is_empty(), "code {code}");
        }
    }

    #[test]
    fn any_other_code_is_an_error_naming_it() {
        for code in [1001, 1003, 4003, 3000] {
            let error = error_text(exit_code_from_close(code, "text frames are not accepted"));
            assert!(error.contains(&code.to_string()), "code {code}: {error}");
        }
    }

    #[test]
    fn empty_in_flight_set_stays_pending_until_a_future_is_pushed() {
        use std::future::Future;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        use futures::stream::FuturesUnordered;
        use futures::task::noop_waker_ref;

        fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
            if let Some(message) = payload.downcast_ref::<&str>() {
                (*message).to_string()
            } else if let Some(message) = payload.downcast_ref::<String>() {
                message.clone()
            } else {
                "non-string panic payload".to_string()
            }
        }

        fn poll_label<T: std::fmt::Display>(
            future: &mut (impl Future<Output = T> + Unpin),
            context: &mut Context<'_>,
        ) -> String {
            let polled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Pin::new(future).poll(context)
            }));
            match polled {
                Ok(Poll::Pending) => "pending".to_string(),
                Ok(Poll::Ready(value)) => format!("ready:{value}"),
                Err(payload) => format!("panicked: {}", panic_message(payload.as_ref())),
            }
        }

        // Two polls, not a wait: an empty queue must stay pending, and a self-wake must not
        // be required for the assertion to finish.
        let mut queue = FuturesUnordered::new();
        let waker = noop_waker_ref();
        let mut context = Context::from_waker(waker);
        let observed = {
            let mut waiting = std::pin::pin!(next_in_flight(&mut queue));
            let first = poll_label(&mut waiting, &mut context);
            let second = poll_label(&mut waiting, &mut context);
            format!("first={first} second={second}")
        };
        assert_eq!(observed, "first=pending second=pending");

        queue.push(std::future::ready(7_i32));
        let mut delivered = std::pin::pin!(next_in_flight(&mut queue));
        assert_eq!(poll_label(&mut delivered, &mut context), "ready:7");
    }
}
