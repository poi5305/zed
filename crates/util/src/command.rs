use std::ffi::OsStr;
#[cfg(not(target_os = "macos"))]
use std::path::Path;

#[cfg(target_os = "macos")]
mod darwin;

#[cfg(target_os = "macos")]
pub use darwin::{Child, Command, Stdio};

#[cfg(all(target_os = "windows", not(target_family = "wasm")))]
const CREATE_NO_WINDOW: u32 = 0x0800_0000_u32;

pub use gpui_util::new_std_command;

pub fn new_command(program: impl AsRef<OsStr>) -> Command {
    Command::new(program)
}

/// Pipes a one-shot `Command::output` should request from the host.
///
/// `std::process::Command::output` captures stdout and stderr even when the
/// caller never called `.stdout()` / `.stderr()`. The wasm shim must do the
/// same: `list_tmux_sessions` never configures those streams, and a false
/// pipe is the host discarding the listing while the caller sees empty success.
#[cfg(any(test, target_family = "wasm"))]
pub(crate) fn output_capture_pipes(
    stdin_configured: bool,
    stdout_configured: bool,
    stderr_configured: bool,
) -> (bool, bool, bool) {
    let _ = (stdout_configured, stderr_configured);
    (stdin_configured, true, true)
}

#[cfg(all(not(target_os = "macos"), not(target_family = "wasm")))]
pub type Child = smol::process::Child;

#[cfg(all(not(target_os = "macos"), not(target_family = "wasm")))]
pub use std::process::Stdio;

#[cfg(all(not(target_os = "macos"), not(target_family = "wasm")))]
#[derive(Debug)]
pub struct Command(smol::process::Command);

#[cfg(all(not(target_os = "macos"), not(target_family = "wasm")))]
impl Command {
    #[inline]
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        #[cfg(target_os = "windows")]
        {
            use smol::process::windows::CommandExt;
            let mut cmd = smol::process::Command::new(program);
            cmd.creation_flags(CREATE_NO_WINDOW);
            Self(cmd)
        }
        #[cfg(not(target_os = "windows"))]
        Self(smol::process::Command::new(program))
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.0.arg(arg);
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.0.args(args);
        self
    }

    pub fn get_args(&self) -> impl Iterator<Item = &OsStr> {
        self.0.get_args()
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> &mut Self {
        self.0.env(key, val);
        self
    }

    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.0.envs(vars);
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.0.env_remove(key);
        self
    }

    pub fn env_clear(&mut self) -> &mut Self {
        self.0.env_clear();
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.0.current_dir(dir);
        self
    }

    pub fn stdin(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.0.stdin(cfg.into());
        self
    }

    pub fn stdout(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.0.stdout(cfg.into());
        self
    }

    pub fn stderr(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.0.stderr(cfg.into());
        self
    }

    pub fn kill_on_drop(&mut self, kill_on_drop: bool) -> &mut Self {
        self.0.kill_on_drop(kill_on_drop);
        self
    }

    pub fn spawn(&mut self) -> std::io::Result<Child> {
        self.0.spawn()
    }

    pub async fn output(&mut self) -> std::io::Result<std::process::Output> {
        self.0.output().await
    }

    pub async fn status(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.0.status().await
    }

    pub fn get_program(&self) -> &OsStr {
        self.0.get_program()
    }
}

#[cfg(target_family = "wasm")]
pub type Child = smol::process::Child;

#[cfg(target_family = "wasm")]
pub use std::process::Stdio;

#[cfg(target_family = "wasm")]
#[derive(Debug)]
pub struct Command(smol::process::Command);

#[cfg(target_family = "wasm")]
impl Command {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self(smol::process::Command::new(program))
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.0.arg(arg);
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.0.args(args);
        self
    }

    pub fn get_args(&self) -> impl Iterator<Item = &OsStr> {
        self.0.get_args()
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> &mut Self {
        self.0.env(key, val);
        self
    }

    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.0.envs(vars);
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.0.env_remove(key);
        self
    }

    pub fn env_clear(&mut self) -> &mut Self {
        self.0.env_clear();
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.0.current_dir(dir);
        self
    }

    pub fn stdin(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.0.stdin(cfg.into());
        self
    }

    pub fn stdout(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.0.stdout(cfg.into());
        self
    }

    pub fn stderr(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.0.stderr(cfg.into());
        self
    }

    pub fn kill_on_drop(&mut self, kill_on_drop: bool) -> &mut Self {
        self.0.kill_on_drop(kill_on_drop);
        self
    }

    pub fn spawn(&mut self) -> std::io::Result<Child> {
        self.0.spawn()
    }

    pub async fn output(&mut self) -> std::io::Result<std::process::Output> {
        // `std::process::Command::output` captures stdout and stderr even when
        // the caller never configured them. Spawn without those pipes makes the
        // host discard the bytes and `Child::output` returns empty success.
        let (_stdin_pipe, stdout_pipe, stderr_pipe) = output_capture_pipes(false, false, false);
        if stdout_pipe {
            self.stdout(Stdio::piped());
        }
        if stderr_pipe {
            self.stderr(Stdio::piped());
        }
        self.0.output().await
    }

    pub async fn status(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.spawn()?.status().await
    }

    pub fn get_program(&self) -> &OsStr {
        self.0.get_program()
    }
}

#[cfg(test)]
mod tests {
    use super::output_capture_pipes;

    #[test]
    fn output_captures_stdout_and_stderr_even_when_unconfigured() {
        assert_eq!(
            output_capture_pipes(false, false, false),
            (false, true, true),
            "list_tmux_sessions never calls .stdout(); stdout_pipe false is the host discarding the bytes"
        );
    }

    #[test]
    fn output_does_not_invent_a_stdin_pipe() {
        assert_eq!(output_capture_pipes(false, false, false).0, false);
    }

    #[test]
    fn output_keeps_a_caller_configured_stdin_pipe() {
        assert_eq!(output_capture_pipes(true, false, false), (true, true, true));
    }
}
