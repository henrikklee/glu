use anyhow::{bail, Context, Result};
use ring::rand::{SecureRandom, SystemRandom};
use std::{
    ffi::{CString, OsStr, OsString},
    fs::{File, OpenOptions},
    io::{self, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::OpenOptionsExt,
        io::{AsRawFd, FromRawFd},
    },
    path::{Component, Path},
};

const TEMP_ATTEMPTS: usize = 16;

/// Opens the directory itself, never a symlink standing in for it.
pub(crate) fn open_directory(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    options
        .open(path)
        .with_context(|| format!("opening state directory {}", path.display()))
}

/// Creates one direct child directory and then opens it without following a
/// pre-existing symlink. Receipt writers use this for the store-owned `.glu`
/// directory rather than calling `create_dir_all` on an archive-controlled path.
pub(crate) fn open_or_create_child_directory(
    parent: &File,
    parent_path: &Path,
    name: &OsStr,
    mode: libc::mode_t,
) -> Result<File> {
    validate_file_name(name)?;
    let name_c = c_string(name)?;
    // SAFETY: parent is an open directory and name_c is a live NUL-terminated
    // copy. mkdirat does not traverse the child when it already exists.
    let rc = unsafe { libc::mkdirat(parent.as_raw_fd(), name_c.as_ptr(), mode) };
    let created = if rc == 0 {
        true
    } else {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error).with_context(|| {
                format!(
                    "creating state directory {}",
                    parent_path.join(name).display()
                )
            });
        }
        false
    };
    let child = open_child_directory(parent, parent_path, name)?;
    if created {
        parent
            .sync_all()
            .with_context(|| format!("syncing state directory {}", parent_path.display()))?;
    }
    Ok(child)
}

/// Durably replaces one file inside an already-open directory.
///
/// The temporary entry is random, same-directory, create-new, and mode 0600.
/// The file is synced before rename and the directory is synced afterward, so
/// a successful return means both bytes and the new directory entry survived
/// the filesystem's durability boundary.
pub(crate) fn replace_file(
    parent: &File,
    parent_path: &Path,
    name: &OsStr,
    bytes: &[u8],
) -> Result<()> {
    validate_file_name(name)?;
    let name_c = c_string(name)?;
    let (mut temp, mut cleanup) = create_temp_file(parent, parent_path, name)?;
    temp.write_all(bytes)
        .with_context(|| format!("writing state file {}", parent_path.join(name).display()))?;
    temp.sync_all()
        .with_context(|| format!("syncing state file {}", parent_path.join(name).display()))?;

    // SAFETY: both names are valid live C strings and both descriptors identify
    // the same parent directory, making this an atomic same-filesystem replace.
    let rc = unsafe {
        libc::renameat(
            parent.as_raw_fd(),
            cleanup.name.as_ptr(),
            parent.as_raw_fd(),
            name_c.as_ptr(),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("replacing state file {}", parent_path.join(name).display()));
    }
    cleanup.armed = false;
    parent
        .sync_all()
        .with_context(|| format!("syncing state directory {}", parent_path.display()))
}

pub(crate) fn replace_file_at_path(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent_path = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("state path has no file name: {}", path.display()))?;
    let parent = open_directory(parent_path)?;
    replace_file(&parent, parent_path, name, bytes)
}

/// Atomically replaces a symlink inside an already-open directory. This avoids
/// the remove-then-create gap previously used for the trace `last.json` link.
pub(crate) fn replace_symlink(
    parent: &File,
    parent_path: &Path,
    name: &OsStr,
    target: &Path,
) -> Result<()> {
    validate_file_name(name)?;
    let name_c = c_string(name)?;
    let target_c = c_string(target.as_os_str())?;
    let mut cleanup = create_temp_symlink(parent, parent_path, name, &target_c)?;
    // SAFETY: both names and the parent descriptor remain valid for the call.
    let rc = unsafe {
        libc::renameat(
            parent.as_raw_fd(),
            cleanup.name.as_ptr(),
            parent.as_raw_fd(),
            name_c.as_ptr(),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("replacing state link {}", parent_path.join(name).display()));
    }
    cleanup.armed = false;
    parent
        .sync_all()
        .with_context(|| format!("syncing state directory {}", parent_path.display()))
}

fn open_child_directory(parent: &File, parent_path: &Path, name: &OsStr) -> Result<File> {
    let name_c = c_string(name)?;
    // SAFETY: parent is an open directory and name_c is a live C string.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error()).with_context(|| {
            format!(
                "opening state directory {}",
                parent_path.join(name).display()
            )
        });
    }
    // SAFETY: fd is newly returned by openat and ownership transfers to File.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn create_temp_symlink(
    parent: &File,
    parent_path: &Path,
    destination: &OsStr,
    target: &CString,
) -> Result<TempEntry> {
    for _ in 0..TEMP_ATTEMPTS {
        let temp_name = random_temp_name(destination)?;
        let temp_c = c_string(&temp_name)?;
        // SAFETY: target and temp_c are live C strings and parent is an open
        // directory. symlinkat stores target bytes without following it.
        let rc = unsafe { libc::symlinkat(target.as_ptr(), parent.as_raw_fd(), temp_c.as_ptr()) };
        if rc == 0 {
            return TempEntry::new(parent, temp_c);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error)
                .with_context(|| format!("creating temporary link in {}", parent_path.display()));
        }
    }
    bail!(
        "could not allocate a unique temporary link in {}",
        parent_path.display()
    )
}

fn create_temp_file(
    parent: &File,
    parent_path: &Path,
    destination: &OsStr,
) -> Result<(File, TempEntry)> {
    for _ in 0..TEMP_ATTEMPTS {
        let temp_name = random_temp_name(destination)?;
        let temp_c = c_string(&temp_name)?;
        // SAFETY: parent is an open directory and temp_c is a live C string.
        // O_EXCL and O_NOFOLLOW ensure an existing entry is never opened.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                temp_c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd >= 0 {
            // SAFETY: fd is newly returned by openat and ownership transfers.
            let file = unsafe { File::from_raw_fd(fd) };
            return Ok((file, TempEntry::new(parent, temp_c)?));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error).with_context(|| {
                format!("creating atomic state temp in {}", parent_path.display())
            });
        }
    }
    bail!(
        "could not allocate a unique atomic state temp in {}",
        parent_path.display()
    )
}

fn random_temp_name(destination: &OsStr) -> Result<OsString> {
    let mut random = [0_u8; 16];
    SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| anyhow::anyhow!("generating atomic state temp name"))?;
    let mut name = OsString::from(".");
    name.push(destination);
    name.push(".glu-tmp-");
    name.push(hex(&random));
    Ok(name)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn validate_file_name(name: &OsStr) -> Result<()> {
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        bail!("state destination is not one file name: {:?}", name);
    }
    Ok(())
}

fn c_string(value: &OsStr) -> Result<CString> {
    CString::new(value.as_bytes()).context("state path contains a NUL byte")
}

struct TempEntry {
    parent: File,
    name: CString,
    armed: bool,
}

impl TempEntry {
    fn new(parent: &File, name: CString) -> Result<Self> {
        Ok(Self {
            parent: parent.try_clone().context("cloning state directory")?,
            name,
            armed: true,
        })
    }
}

impl Drop for TempEntry {
    fn drop(&mut self) {
        if self.armed {
            // SAFETY: parent remains open and name is a live C string. Cleanup
            // intentionally ignores errors because the primary error wins.
            unsafe {
                libc::unlinkat(self.parent.as_raw_fd(), self.name.as_ptr(), 0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn predictable_temp_symlink_is_never_followed() {
        let temp = tempfile::tempdir().unwrap();
        let parent = open_directory(temp.path()).unwrap();
        let outside = temp.path().join("outside");
        let predictable = temp.path().join("record.json.tmp");
        fs::write(&outside, b"sentinel").unwrap();
        std::os::unix::fs::symlink(&outside, &predictable).unwrap();

        replace_file(&parent, temp.path(), OsStr::new("record.json"), b"state").unwrap();

        assert_eq!(fs::read(temp.path().join("record.json")).unwrap(), b"state");
        assert_eq!(fs::read(outside).unwrap(), b"sentinel");
        assert!(predictable.is_symlink());
    }

    #[test]
    fn destination_symlink_is_replaced_not_followed() {
        let temp = tempfile::tempdir().unwrap();
        let parent = open_directory(temp.path()).unwrap();
        let outside = temp.path().join("outside");
        let destination = temp.path().join("record.json");
        fs::write(&outside, b"sentinel").unwrap();
        std::os::unix::fs::symlink(&outside, &destination).unwrap();

        replace_file(&parent, temp.path(), OsStr::new("record.json"), b"state").unwrap();

        assert!(!destination.is_symlink());
        assert_eq!(fs::read(destination).unwrap(), b"state");
        assert_eq!(fs::read(outside).unwrap(), b"sentinel");
    }

    #[test]
    fn child_directory_symlink_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let parent = open_directory(temp.path()).unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, temp.path().join(".glu")).unwrap();

        let error = open_or_create_child_directory(&parent, temp.path(), OsStr::new(".glu"), 0o700)
            .unwrap_err();

        assert!(error.to_string().contains("opening state directory"));
        assert!(fs::read_dir(outside).unwrap().next().is_none());
    }

    #[test]
    fn failed_replace_cleans_up_random_temp() {
        let temp = tempfile::tempdir().unwrap();
        let parent = open_directory(temp.path()).unwrap();
        fs::create_dir(temp.path().join("record.json")).unwrap();

        assert!(replace_file(&parent, temp.path(), OsStr::new("record.json"), b"state").is_err());

        let entries = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert!(entries
            .iter()
            .all(|name| !name.to_string_lossy().contains("glu-tmp")));
    }

    #[test]
    fn concurrent_writers_use_distinct_temps() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().to_path_buf();
        let mut writers = Vec::new();
        for index in 0..8 {
            let parent = open_directory(&path).unwrap();
            let path = path.clone();
            writers.push(std::thread::spawn(move || {
                replace_file(
                    &parent,
                    &path,
                    OsStr::new("record.json"),
                    format!("state-{index}").as_bytes(),
                )
            }));
        }
        for writer in writers {
            writer.join().unwrap().unwrap();
        }

        let final_bytes = fs::read(temp.path().join("record.json")).unwrap();
        assert!(final_bytes.starts_with(b"state-"));
        let entries = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert!(entries
            .iter()
            .all(|name| !name.to_string_lossy().contains("glu-tmp")));
    }

    #[test]
    fn atomic_symlink_replacement_has_no_predictable_temp() {
        let temp = tempfile::tempdir().unwrap();
        let parent = open_directory(temp.path()).unwrap();
        fs::write(temp.path().join("trace.json"), b"{}").unwrap();

        replace_symlink(
            &parent,
            temp.path(),
            OsStr::new("last.json"),
            Path::new("trace.json"),
        )
        .unwrap();

        assert_eq!(
            fs::read_link(temp.path().join("last.json")).unwrap(),
            Path::new("trace.json")
        );
        let entries = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert!(entries
            .iter()
            .all(|name| !name.to_string_lossy().contains("glu-tmp")));
    }
}
