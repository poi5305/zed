use std::io;

// Must match `NOT_FOUND_MARKER` in crates/zed_web_server/src/process_rpc.rs. The RPC
// error payload is a bare string, so this prefix is the only way the server can say
// "the program does not exist" without callers matching on a localized OS message.
const NOT_FOUND_MARKER: &str = "zed-web-io-error:NotFound: ";
// Prepended by `wasm_rpc::RpcClient::call` to every server-reported error.
const REMOTE_ERROR_PREFIX: &str = "remote error: ";

pub(crate) fn io_error_from_remote(message: &str) -> io::Error {
    let server_message = message.strip_prefix(REMOTE_ERROR_PREFIX).unwrap_or(message);
    match server_message.strip_prefix(NOT_FOUND_MARKER) {
        Some(detail) => io::Error::new(io::ErrorKind::NotFound, detail),
        None => io::Error::new(io::ErrorKind::Other, message),
    }
}

#[cfg(test)]
mod tests {
    use super::io_error_from_remote;
    use std::io;

    #[test]
    fn marked_remote_error_decodes_to_not_found() {
        let error = io_error_from_remote(
            "remote error: zed-web-io-error:NotFound: running direnv in /w: No such file or directory (os error 2)",
        );
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(
            error.to_string(),
            "running direnv in /w: No such file or directory (os error 2)"
        );
    }

    #[test]
    fn unmarked_remote_error_stays_other() {
        let message = "remote error: running git in /w";
        let error = io_error_from_remote(message);
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), message);
    }

    #[test]
    fn marker_that_is_not_at_the_start_of_the_server_message_is_ignored() {
        let error = io_error_from_remote(
            "remote error: stderr quoted zed-web-io-error:NotFound: elsewhere",
        );
        assert_eq!(error.kind(), io::ErrorKind::Other);
    }
}
