use anyhow::{bail, Context, Result};
use std::{
    collections::{BTreeMap, VecDeque},
    ffi::{CString, OsStr},
    fs::{File, OpenOptions},
    io,
    os::unix::{
        ffi::OsStrExt,
        fs::OpenOptionsExt,
        io::{AsRawFd, FromRawFd},
    },
    path::{Component, Path, PathBuf},
};

/// A staging directory capability. Archive members are resolved relative to this
/// descriptor, and every directory hop refuses symlinks. The archive can still
/// contain symlinks; extraction simply never follows one while mutating staging.
pub(crate) struct ExtractionRoot {
    root: File,
    // A bounded descriptor cache keeps grouped bottle entries fast without
    // retaining one fd per directory (large bottles contain thousands).
    directories: BTreeMap<PathBuf, File>,
    directory_order: VecDeque<PathBuf>,
}

impl ExtractionRoot {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let root = options
            .open(path)
            .with_context(|| format!("open extraction root {}", path.display()))?;
        Ok(Self {
            root,
            directories: BTreeMap::new(),
            directory_order: VecDeque::new(),
        })
    }

    pub(crate) fn create_dir(&mut self, path: &Path) -> Result<()> {
        self.ensure_dir(path).map(|_| ())
    }

    pub(crate) fn require_directory(&self, path: &Path) -> Result<()> {
        self.open_existing_dir(path).map(|_| ())
    }

    /// Creates a new regular file without following either its final component
    /// or any parent component. Returning the descriptor before work is queued
    /// means writer threads never reopen an archive-controlled pathname.
    pub(crate) fn create_file(&mut self, path: &Path) -> Result<File> {
        let (parent, name) = split_parent(path)?;
        let parent = self.ensure_dir(&parent)?;
        let name = c_string(&name)?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("create archive file {}", path.display()));
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    /// Symlink targets deliberately remain unrestricted: packages legitimately
    /// contain links outside their own directory. Safety comes from never using
    /// those links to resolve a later extraction mutation.
    pub(crate) fn create_symlink(&mut self, target: &Path, path: &Path) -> Result<()> {
        let (parent, name) = split_parent(path)?;
        let parent = self.ensure_dir(&parent)?;
        let target = c_string(target.as_os_str())?;
        let name = c_string(&name)?;
        let rc = unsafe { libc::symlinkat(target.as_ptr(), parent.as_raw_fd(), name.as_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("create archive symlink {}", path.display()));
        }
        Ok(())
    }

    pub(crate) fn create_hardlink(&mut self, source: &Path, path: &Path) -> Result<()> {
        let (source_parent, source_name) = split_parent(source)?;
        let source_parent = self.open_existing_dir(&source_parent)?;
        let source_name = c_string(&source_name)?;

        // linkat without AT_SYMLINK_FOLLOW links a symlink inode on platforms
        // that permit it. Bottle hardlinks must instead name a regular member;
        // explicitly reject symlink and other special-file sources.
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        let stat_rc = unsafe {
            libc::fstatat(
                source_parent.as_raw_fd(),
                source_name.as_ptr(),
                metadata.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if stat_rc != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("stat archive hardlink source {}", source.display()));
        }
        let metadata = unsafe { metadata.assume_init() };
        if metadata.st_mode & libc::S_IFMT != libc::S_IFREG {
            bail!(
                "archive hardlink source is not a regular file: {}",
                source.display()
            );
        }

        let (destination_parent, destination_name) = split_parent(path)?;
        let destination_parent = self.ensure_dir(&destination_parent)?;
        let destination_name = c_string(&destination_name)?;
        let rc = unsafe {
            libc::linkat(
                source_parent.as_raw_fd(),
                source_name.as_ptr(),
                destination_parent.as_raw_fd(),
                destination_name.as_ptr(),
                0,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error()).with_context(|| {
                format!(
                    "create archive hardlink {} -> {}",
                    path.display(),
                    source.display()
                )
            });
        }
        Ok(())
    }

    fn ensure_dir(&mut self, path: &Path) -> Result<File> {
        self.walk_dir(path)
    }

    fn open_existing_dir(&self, path: &Path) -> Result<File> {
        self.walk_existing_dir(path)
    }

    fn walk_dir(&mut self, path: &Path) -> Result<File> {
        if path.as_os_str().is_empty() {
            return self.root.try_clone().context("clone extraction root");
        }
        if let Some(cached) = self.directories.get(path) {
            return cached.try_clone().context("clone extraction directory");
        }

        let mut current = self.root.try_clone().context("clone extraction root")?;
        let mut traversed = PathBuf::new();
        for component in normal_components(path)? {
            traversed.push(&component);
            if let Some(cached) = self.directories.get(&traversed) {
                current = cached.try_clone().context("clone extraction directory")?;
                continue;
            }
            mkdir_at(&current, &component, &traversed)?;
            let next = open_dir_at(&current, &component, &traversed)?;
            self.cache_directory(
                traversed.clone(),
                next.try_clone().context("cache extraction directory")?,
            );
            current = next;
        }
        Ok(current)
    }

    fn cache_directory(&mut self, path: PathBuf, directory: File) {
        const MAX_CACHED_DIRECTORY_FDS: usize = 32;

        debug_assert!(!self.directories.contains_key(&path));
        while self.directories.len() >= MAX_CACHED_DIRECTORY_FDS {
            let Some(oldest) = self.directory_order.pop_front() else {
                break;
            };
            self.directories.remove(&oldest);
        }
        self.directory_order.push_back(path.clone());
        self.directories.insert(path, directory);
    }

    fn walk_existing_dir(&self, path: &Path) -> Result<File> {
        let mut current = self.root.try_clone().context("clone extraction root")?;
        let mut traversed = PathBuf::new();
        for component in normal_components(path)? {
            traversed.push(&component);
            if let Some(cached) = self.directories.get(&traversed) {
                current = cached.try_clone().context("clone extraction directory")?;
            } else {
                current = open_dir_at(&current, &component, &traversed)?;
            }
        }
        Ok(current)
    }
}

fn mkdir_at(parent: &File, name: &OsStr, display: &Path) -> Result<()> {
    let name = c_string(name)?;
    let rc = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o755) };
    if rc == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::AlreadyExists {
        return Ok(());
    }
    Err(error).with_context(|| format!("create archive directory {}", display.display()))
}

fn open_dir_at(parent: &File, name: &OsStr, display: &Path) -> Result<File> {
    let name = c_string(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("open archive directory {}", display.display()));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn split_parent(path: &Path) -> Result<(PathBuf, std::ffi::OsString)> {
    let mut components = normal_components(path)?;
    let Some(name) = components.pop() else {
        bail!("archive path has no final component: {}", path.display());
    };
    let parent = components.into_iter().collect();
    Ok((parent, name))
}

fn normal_components(path: &Path) -> Result<Vec<std::ffi::OsString>> {
    let mut out = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => out.push(value.to_os_string()),
            _ => bail!("unsafe archive path: {}", path.display()),
        }
    }
    Ok(out)
}

fn c_string(value: &OsStr) -> Result<CString> {
    CString::new(value.as_bytes()).context("archive path contains a NUL byte")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn external_symlinks_are_created_but_never_followed() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join("staging");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&staging).unwrap();
        std::fs::create_dir(&outside).unwrap();

        let mut root = ExtractionRoot::open(&staging).unwrap();
        root.create_symlink(&outside, Path::new("external"))
            .unwrap();
        let error = root.create_file(Path::new("external/escaped")).unwrap_err();

        assert!(error.to_string().contains("open archive directory"));
        assert!(!outside.join("escaped").exists());
    }

    #[test]
    fn hardlinks_require_regular_internal_sources() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join("staging");
        std::fs::create_dir(&staging).unwrap();
        let mut root = ExtractionRoot::open(&staging).unwrap();
        root.create_symlink(Path::new("/etc/passwd"), Path::new("source"))
            .unwrap();

        let error = root
            .create_hardlink(Path::new("source"), Path::new("copy"))
            .unwrap_err();

        assert!(error.to_string().contains("not a regular file"));
        assert!(!staging.join("copy").exists());
    }

    #[test]
    fn descriptor_cache_is_bounded_for_large_packages() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join("staging");
        std::fs::create_dir(&staging).unwrap();
        let mut root = ExtractionRoot::open(&staging).unwrap();

        for index in 0..100 {
            root.create_dir(Path::new(&format!("dir-{index}"))).unwrap();
        }

        assert_eq!(root.directories.len(), 32);
        assert_eq!(root.directory_order.len(), 32);
    }

    #[test]
    fn descriptors_support_safe_files_and_hardlinks() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join("staging");
        std::fs::create_dir(&staging).unwrap();
        let mut root = ExtractionRoot::open(&staging).unwrap();
        let mut file = root.create_file(Path::new("lib/source")).unwrap();
        file.write_all(b"payload").unwrap();
        drop(file);

        root.create_hardlink(Path::new("lib/source"), Path::new("lib/copy"))
            .unwrap();

        assert_eq!(std::fs::read(staging.join("lib/copy")).unwrap(), b"payload");
    }
}
