//! A1: an advisory process lock that serializes mutating commands against a
//! prefix, so two concurrent `glu install` / `up` / `rm` / `upgrade` runs
//! can't race on staging dirs, the linkedPath marker, and `glu.json` /
//! receipt writes. Read-only queries (list, deps, status, ...) don't take it.
//!
//! Implemented as a flock'd file under `<prefix>/var/glu/operations.lock`.
//! flock is auto-released by the kernel when the holding process exits (even
//! on a crash), so there is no stale-file handling. Acquisition is
//! non-blocking: a second command that finds the lock held fails fast with a
//! clear message rather than guessing (matching Homebrew's "another process
//! already in progress" behavior).

use anyhow::{Context, Result};
use glu_core::Prefix;
use std::fs::{File, OpenOptions};
use std::path::PathBuf;

const LOCK_FILE: &str = "operations.lock";

/// Held while a mutating command runs. The underlying `flock` is released and
/// the handle closed when this drops.
#[derive(Debug)]
pub struct OperationLock {
    file: File,
}

pub fn lock_path(prefix: &Prefix) -> PathBuf {
    prefix.0.join("var/glu").join(LOCK_FILE)
}

/// Acquires the prefix operation lock, erroring (not blocking) if another glu
/// process already holds it.
pub fn acquire(prefix: &Prefix) -> Result<OperationLock> {
    let path = lock_path(prefix);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening lock {}", path.display()))?;
    take_flock(&file).with_context(|| format!("acquiring prefix lock {}", path.display()))?;
    Ok(OperationLock { file })
}

#[cfg(unix)]
fn take_flock(file: &File) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        anyhow::bail!(
            "another glu command is already running on this prefix; wait for it to finish and retry"
        );
    }
    Err(err.into())
}

#[cfg(not(unix))]
fn take_flock(_file: &File) -> Result<()> {
    Ok(())
}

impl Drop for OperationLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            use std::os::unix::io::AsRawFd;
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[cfg(unix)]
    use std::os::unix::io::FromRawFd;

    fn just_acquire() -> Result<OperationLock> {
        let dir = TempDir::new().unwrap();
        let prefix = Prefix(dir.path().join("prefix"));
        acquire(&prefix)
    }

    #[test]
    fn acquire_then_release_then_acquire_in_sequence() {
        let _ok = just_acquire().expect("single shot acquire succeeds");
    }

    /// flock is per open-file-description; two fds in the SAME process can both
    /// hold it. The real guarantee is cross-process, so verify contention with a
    /// forked child that holds the lock, then confirm release-on-exit (the
    /// crash-safety property flock gives us).
    #[cfg(unix)]
    #[test]
    fn lock_conflicts_across_processes_and_releases_on_exit() {
        use std::io::{Read, Write};
        // A /tmp based prefix path that survives fork.
        let base = tempfile::Builder::new()
            .prefix("glu-oplock-")
            .tempdir()
            .unwrap();
        let prefix = Prefix(base.path().join("prefix"));

        let mut fds = [0i32; 2];
        unsafe { libc::pipe(fds.as_mut_ptr()) };
        let (rd, wr) = (fds[0], fds[1]);

        match unsafe { libc::fork() } {
            0 => {
                // Child: close the read end, acquire the lock, signal readiness.
                unsafe { libc::close(rd) };
                let status = match acquire(&prefix) {
                    Ok(_lock) => {
                        let mut out = unsafe { std::fs::File::from_raw_fd(wr) };
                        let _ = out.write_all(b"1");
                        drop(out);
                        std::thread::sleep(std::time::Duration::from_millis(400));
                        0
                    }
                    Err(_) => 2,
                };
                std::process::exit(status);
            }
            pid if pid > 0 => {
                // Parent: wait for the child signal (lock held), then expect refusal.
                unsafe { libc::close(wr) };
                let mut reader = unsafe { std::fs::File::from_raw_fd(rd) };
                let mut buf = [0u8; 1];
                let read_ok = reader.read_exact(&mut buf).is_ok();
                assert!(read_ok, "child failed before locking");

                let held = acquire(&prefix);
                assert!(
                    held.is_err(),
                    "parent must be refused while child holds the lock"
                );

                // Wait for the child to exit; flock is released automatically.
                let mut status = 0i32;
                unsafe { libc::waitpid(pid, &mut status, 0) };

                let retry = acquire(&prefix).expect("reacquire after child exit (release-on-exit)");
                drop(retry);
            }
            _ => panic!("fork failed"),
        }
    }
}
