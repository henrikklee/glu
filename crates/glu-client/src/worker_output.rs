//! Captured output for the internal postinstall worker protocol.
//!
//! The worker runs as a child process. Its stdout and stderr are captured by
//! the parent and relayed through `ExecutionEvents`; these are the only direct
//! process-output writes allowed in the client crate.

pub(crate) fn notice(message: &str) {
    if !message.trim().is_empty() {
        eprintln!("{message}");
    }
}

pub(crate) fn stdout_notice(message: &str) {
    if !message.trim().is_empty() {
        println!("{message}");
    }
}
