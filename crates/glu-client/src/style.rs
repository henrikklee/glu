//! Minimal terminal styling helpers.
//!
//! ANSI escapes are emitted only when stdout is a terminal and `NO_COLOR`
//! is not set, so piped output stays clean.

use std::io::IsTerminal;

/// Returns true if ANSI styling should be emitted.
fn enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").is_ok_and(|term| term != "dumb")
        && std::io::stdout().is_terminal()
}

/// Wrap `text` in the ANSI dim (muted) style, matching how bun dims
/// package versions in `bun list`.
pub fn dim(text: &str) -> String {
    if enabled() {
        format!("\x1b[2m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// Wrap `text` in the ANSI bold style.
pub fn bold(text: &str) -> String {
    if enabled() {
        format!("\x1b[1m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// Wrap `text` in the ANSI bold + dim style.
pub fn bold_dim(text: &str) -> String {
    if enabled() {
        format!("\x1b[1;2m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// Wrap `text` in the ANSI bold + yellow style.
pub fn bold_yellow(text: &str) -> String {
    if enabled() {
        format!("\x1b[1;33m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// Wrap `text` in the ANSI magenta style.
pub fn magenta(text: &str) -> String {
    if enabled() {
        format!("\x1b[35m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// Wrap `text` in the ANSI bold + magenta style.
pub fn bold_magenta(text: &str) -> String {
    if enabled() {
        format!("\x1b[1;35m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// Wrap `text` in the ANSI bold + blue style used for table headers.
pub fn bold_blue(text: &str) -> String {
    if enabled() {
        format!("\x1b[1;34m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// Wrap `text` in the ANSI cyan style (informational markers, e.g. the
/// `↑` "dependencies shown above" tree marker).
pub fn cyan(text: &str) -> String {
    if enabled() {
        format!("\x1b[36m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// Wrap `text` in the ANSI green style.
pub fn green(text: &str) -> String {
    if enabled() {
        format!("\x1b[32m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// Wrap `text` in the ANSI yellow style.
pub fn yellow(text: &str) -> String {
    if enabled() {
        format!("\x1b[33m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// Wrap `text` in the ANSI red style.
pub fn red(text: &str) -> String {
    if enabled() {
        format!("\x1b[31m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}
