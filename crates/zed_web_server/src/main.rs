use anyhow::Result;

fn main() -> Result<()> {
    // Handled before clap and before any runtime: the askpass script execs
    // `<current_exe> --askpass=<socket>`, and clap would reject the flag.
    if let Some(socket) = zed_web_server::askpass_socket(std::env::args()) {
        askpass::main(&socket);
        return Ok(());
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    if !zed_web_server::ssh_enabled()? {
        return runtime.block_on(zed_web_server::run());
    }
    zed_web_server::ssh_host::run_with_gpui(runtime)
}
