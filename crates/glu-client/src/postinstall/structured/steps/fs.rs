use super::super::*;

// Homebrew 4dacfe77: install_steps.rb:990-1003 (copy case) + 1421-1424
// (step_destination). Non-recursive overwrite is IN-PLACE (`FileUtils.cp` —
// inode/mode preserved); a SYMLINK destination is removed first (rm_f, :997);
// Errno::EEXIST when `!overwrite` and the destination exists (exist? follows
// symlinks). Recursive is `FileUtils.cp_r ... remove_destination:` and uses
// FileUtils.copy_entry semantics: preserve file types, keep symlinks as
// symlinks, and unlink existing file/symlink destinations when overwriting.
pub(in crate::postinstall::structured) fn copy_path(
    source: &Path,
    target: &Path,
    recursive: bool,
    overwrite: bool,
) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let destination = if target.is_dir() {
        target.join(source.file_name().unwrap())
    } else {
        target.to_path_buf()
    };
    if destination.exists() && !overwrite {
        bail!("postinstall target exists: {}", destination.display());
    }
    if recursive && source.is_dir() && !source.is_symlink() {
        copy_entry(source, &destination, false, overwrite)?;
    } else {
        if overwrite && destination.is_symlink() {
            fs::remove_file(&destination)?;
        }
        fs::copy(source, &destination)?;
    }
    Ok(())
}

/// errno for cross-device link — rename(2) fails with EXDEV when source and
/// destination live on different filesystems. 18 on both macOS and Linux;
/// matches FileUtils.mv's `rescue Errno::EXDEV` (fileutils.rb:531).
const EXDEV: i32 = 18;

// Homebrew 4dacfe77: install_steps.rb:968-972 (`FileUtils.mv source, target,
// force: step["force"] == true`), implemented against ruby 2.6 fileutils.rb
// :509-538 (`mv`) + :1563-1578 (`fu_each_src_dest0`):
// - dir target → destination = target/<basename> (File.directory? follows symlinks)
// - an existing destination errors only when it is a real DIRECTORY (lstat,
//   fileutils.rb:524-527 → Errno::EEXIST); an existing file/symlink is silently
//   replaced by rename(2)
// - EXDEV (cross-device) → copy + remove source (fileutils.rb:531-535)
// - upstream `force:` only suppresses SystemCallError (fileutils.rb:537); glu
//   always surfaces errors (postinstall failures fail the install per spec), so
//   `force` is intentionally not threaded through.
pub(in crate::postinstall::structured) fn move_path(source: &Path, target: &Path) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    // FileUtils.mv: single source, destination is a dir → destination/basename.
    let destination = if target.is_dir() {
        target.join(source.file_name().unwrap())
    } else {
        target.to_path_buf()
    };
    // Errno::EEXIST only when the destination is an existing real directory
    // (lstat semantics: a symlink-to-dir is replaced, not descended into).
    if destination.is_dir() && !destination.is_symlink() {
        bail!(
            "postinstall target is a directory: {}",
            destination.display()
        );
    }
    fs::rename(source, &destination).or_else(|err| {
        // Cross-device fallback: FileUtils.mv rescues EXDEV with
        // `copy_entry s, d, true` then removes the source.
        if err.raw_os_error() == Some(EXDEV) {
            copy_entry(source, &destination, true, false)?;
            return remove_any(source);
        }
        Err(err.into())
    })?;
    Ok(())
}

// Homebrew 4dacfe77: install_steps.rb:1004-1025 (remove case).
// Compatibility: `sudo` is implemented (see the per-path logic below),
// including the `"if_needed"` non-writable-parent escalation. glu omits the
// `Cask::Utils.gain_permissions` chflags/chmod/chown retry escalation
// (cask/utils.rb) that runs BEFORE `sudo rm` — upstream tries to make the path
// removable without sudo first; glu goes straight to `sudo rm` when the parent
// is not writable. Outcome converges for the common cases.
pub(in crate::postinstall::structured) fn remove_step(
    ctx: &PostinstallContext<'_>,
    step: &Value,
) -> Result<()> {
    let mut paths = Vec::new();
    for spec in step
        .get("paths")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        paths.extend(expand_glob(ctx, spec)?);
    }
    if let Some(needle) = step.get("symlink_target_contains").and_then(Value::as_str) {
        let needle = expand(ctx, needle);
        paths.retain(|p| {
            p.is_symlink()
                && fs::read_link(p)
                    .map(|t| t.to_string_lossy().contains(&needle))
                    .unwrap_or(false)
        });
    }
    if let Some(needle) = step.get("content_contains").and_then(Value::as_str) {
        let needle = expand(ctx, needle);
        let needle = needle.as_bytes();
        paths.retain(|p| {
            p.is_file()
                && fs::read(p)
                    .map(|data| memchr::memmem::find(&data, needle).is_some())
                    .unwrap_or(false)
        });
    }
    for p in paths {
        if !(p.exists() || p.is_symlink()) {
            continue;
        }
        // Homebrew 4dacfe77: install_steps.rb:1015-1021 — sudo when
        // `step["sudo"] == true` or `"if_needed"` on a non-writable parent
        // (`path.dirname.writable?` = access(W_OK)); `Cask::Utils.gain_permissions_remove`
        // (cask/utils.rb) then removes plainly when the parent is writable, else
        // `sudo /bin/rm [-R] -f -- <path>` (symlinks: `-h`).
        let sudo_requested = matches!(step.get("sudo"), Some(Value::Bool(true)))
            || step.get("sudo").and_then(Value::as_str) == Some("if_needed");
        let parent_writable = p.parent().map(dir_writable).unwrap_or(false);
        if sudo_requested && !parent_writable {
            let recursive_flag = if p.is_symlink() {
                vec!["-h".to_string()]
            } else if p.is_dir() {
                vec!["-R".to_string()]
            } else {
                vec![]
            };
            let mut args = recursive_flag;
            args.push("-f".to_string());
            args.push("--".to_string());
            args.push(p.display().to_string());
            run_command(RunCommand::new(Some(ctx.env), Path::new("/bin/rm"), &args).sudo(true))?;
        } else if bool_or(step, "recursive", false) {
            remove_any(&p)?;
        } else {
            fs::remove_file(&p)?;
        }
    }
    Ok(())
}
// Homebrew 4dacfe77: install_steps.rb:1037-1051 (link_dir case) — walk mirrors
// `source_dir.find` (depth-first pre-order, does not follow symlinked dirs);
// per-entry semantics in `link_dir_entry`.
pub(in crate::postinstall::structured) fn link_dir_step(
    ctx: &PostinstallContext<'_>,
    step: &Value,
) -> Result<()> {
    let source = path(ctx, req(step, "source")?)?;
    let target = path(ctx, req(step, "target")?)?;
    link_dir_tree(&source, &target)
}
// Homebrew 4dacfe77: install_steps.rb:1052-1059 (link_children: each direct
// child linked into target_dir with prefix/suffix, RELATIVE symlinks via
// `install_symlink`, ln_sf force semantics).
pub(in crate::postinstall::structured) fn link_children_step(
    ctx: &PostinstallContext<'_>,
    step: &Value,
) -> Result<()> {
    let source = path(ctx, req(step, "source")?)?;
    let target = path(ctx, req(step, "target")?)?;
    let prefix = expand(
        ctx,
        step.get("prefix").and_then(Value::as_str).unwrap_or(""),
    );
    let suffix = expand(
        ctx,
        step.get("suffix").and_then(Value::as_str).unwrap_or(""),
    );
    fs::create_dir_all(&target)?;
    for child in fs::read_dir(source)? {
        let child = child?.path();
        let name = format!(
            "{prefix}{}{suffix}",
            child.file_name().unwrap().to_string_lossy()
        );
        create_relative_symlink(&child, &target.join(name))?;
    }
    Ok(())
}
// Homebrew 4dacfe77: install_steps.rb:1060-1075 (symlink case) + 1253-1265
// (create_symlink). Matches upstream's flow: glob → multi/`target.directory?`
// fan-out vs single, `link_source` for non-glob, sudo/`if_needed` via
// `/bin/ln -s`, and `FileUtils.rm_f` file/symlink-only force semantics.
pub(in crate::postinstall::structured) fn symlink_step(
    ctx: &PostinstallContext<'_>,
    step: &Value,
) -> Result<()> {
    let target = path(ctx, req(step, "target")?)?;
    if bool_or(step, "source_glob", false) {
        let sources = expand_glob(ctx, req(step, "source")?)?
            .into_iter()
            .filter(|p| p.exists() || p.is_symlink())
            .collect::<Vec<_>>();
        if sources.is_empty() {
            return Ok(());
        }
        if sources.len() > 1 || target.is_dir() {
            fs::create_dir_all(&target)?;
            for source in sources {
                create_symlink(
                    ctx.env,
                    &source.to_string_lossy(),
                    &target.join(source.file_name().unwrap()),
                    step,
                )?;
            }
            return Ok(());
        }
        return create_symlink(ctx.env, &sources[0].to_string_lossy(), &target, step);
    }
    create_symlink(
        ctx.env,
        &link_source(ctx, req(step, "source")?)?,
        &target,
        step,
    )
}

// Homebrew 4dacfe77: ruby 2.6 fileutils.rb:448-455 (cp_r) + 474-482
// (copy_entry) + 1350-1391/1393-1420 (Entry_#copy/copy_metadata). Used for
// recursive `copy`, PHP's `cp_r "src/.", dest` contents copy, and `mv` EXDEV
// fallback. This preserves file TYPES for recursive copies (notably symlinks),
// unlinks file/symlink destinations under `remove_destination`, and preserves
// owner/group/mtime/mode for the `mv` EXDEV `copy_entry s, d, true` path.
pub(in crate::postinstall::structured) fn copy_entry_contents(
    src_dir: &Path,
    dst_dir: &Path,
    preserve_metadata: bool,
    remove_destination: bool,
) -> Result<()> {
    fs::create_dir_all(dst_dir)?;
    for entry in fs::read_dir(src_dir)? {
        let entry = entry?;
        copy_entry(
            &entry.path(),
            &dst_dir.join(entry.file_name()),
            preserve_metadata,
            remove_destination,
        )?;
    }
    Ok(())
}

fn copy_entry(
    src: &Path,
    dst: &Path,
    preserve_metadata: bool,
    remove_destination: bool,
) -> Result<()> {
    let meta = fs::symlink_metadata(src)
        .with_context(|| format!("reading postinstall copy source {}", src.display()))?;
    unlink_copy_destination_if_needed(dst, remove_destination)?;
    if meta.file_type().is_dir() {
        if !dst.exists() {
            fs::create_dir(dst).with_context(|| format!("creating {}", dst.display()))?;
        } else if !dst.is_dir() {
            bail!(
                "postinstall copy destination is not a directory: {}",
                dst.display()
            );
        }
        copy_entry_contents(src, dst, preserve_metadata, remove_destination)?;
        if preserve_metadata {
            copy_entry_metadata(&meta, dst)?;
        }
    } else if meta.file_type().is_symlink() {
        let target =
            fs::read_link(src).with_context(|| format!("reading symlink {}", src.display()))?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, dst)
            .with_context(|| format!("copying symlink {}", dst.display()))?;
        #[cfg(not(unix))]
        bail!(
            "postinstall symlink copy requires unix support: {}",
            src.display()
        );
        if preserve_metadata {
            copy_entry_metadata(&meta, dst)?;
        }
    } else if meta.file_type().is_file() {
        copy_regular_entry_file(src, dst, &meta)?;
        if preserve_metadata {
            copy_entry_metadata(&meta, dst)?;
        }
    } else {
        bail!("unsupported postinstall copy file type: {}", src.display());
    }
    Ok(())
}

fn unlink_copy_destination_if_needed(dst: &Path, remove_destination: bool) -> Result<()> {
    if remove_destination && (dst.is_file() || dst.is_symlink()) {
        fs::remove_file(dst).with_context(|| format!("removing {}", dst.display()))?;
    }
    Ok(())
}

fn copy_regular_entry_file(src: &Path, dst: &Path, meta: &fs::Metadata) -> Result<()> {
    let mut input = fs::File::open(src).with_context(|| format!("opening {}", src.display()))?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        options.mode(meta.mode());
    }
    let mut output = options
        .open(dst)
        .with_context(|| format!("opening {}", dst.display()))?;
    io::copy(&mut input, &mut output)
        .with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
    Ok(())
}

fn copy_entry_metadata(src_meta: &fs::Metadata, dst: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let is_symlink = src_meta.file_type().is_symlink();
        if !is_symlink {
            set_path_times(dst, src_meta)?;
        }
        let mut mode = src_meta.mode();
        if let Err(err) = chown_path(dst, src_meta.uid(), src_meta.gid(), is_symlink) {
            match err.raw_os_error() {
                Some(libc::EPERM) | Some(libc::EACCES) => mode &= 0o1777,
                _ => return Err(err.into()),
            }
        }
        if !is_symlink {
            fs::set_permissions(dst, fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_path_times(path: &Path, meta: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let path = path_cstring(path)?;
    let times = [
        libc::timespec {
            tv_sec: meta.atime(),
            tv_nsec: meta.atime_nsec() as _,
        },
        libc::timespec {
            tv_sec: meta.mtime(),
            tv_nsec: meta.mtime_nsec() as _,
        },
    ];
    // SAFETY: `path` is a NUL-terminated copy of the OS path bytes, and
    // `times` points at two initialized timespec values for the duration of the call.
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
pub(in crate::postinstall::structured) fn chown_path(
    path: &Path,
    uid: u32,
    gid: u32,
    symlink: bool,
) -> io::Result<()> {
    let path = path_cstring(path)?;
    // SAFETY: `path` is a NUL-terminated copy of the OS path bytes and lives
    // for the duration of the call.
    let rc = unsafe {
        if symlink {
            libc::lchown(path.as_ptr(), uid, gid)
        } else {
            libc::chown(path.as_ptr(), uid, gid)
        }
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

// Homebrew 4dacfe77: install_steps.rb:1037-1051 (link_dir walk). Depth-first
// pre-order like `source_dir.find`; does not descend into symlinked dirs
// (Ruby Find does not follow symlinks).
fn link_dir_tree(source: &Path, target: &Path) -> Result<()> {
    link_dir_entry(source, target)?;
    if source.is_dir() && !source.is_symlink() {
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let child = entry.path();
            let link_target = target.join(child.file_name().unwrap());
            link_dir_tree(&child, &link_target)?;
        }
    }
    Ok(())
}

// Homebrew 4dacfe77: install_steps.rb:1037-1051 — one `find` entry:
// skip .DS_Store (:1042); leave existing real dirs (:1043); rm_f files/symlinks
// (:1045); install_symlink files/symlinks / mkpath dirs (:1046-1049).
fn link_dir_entry(source: &Path, link_target: &Path) -> Result<()> {
    if source
        .file_name()
        .map(|n| n == ".DS_Store")
        .unwrap_or(false)
    {
        return Ok(());
    }
    if link_target.is_dir() && !link_target.is_symlink() {
        return Ok(());
    }
    if link_target.exists() || link_target.is_symlink() {
        remove_any(link_target)?;
    }
    if source.is_symlink() || source.is_file() {
        create_relative_symlink(source, link_target)?;
    } else if source.is_dir() {
        fs::create_dir_all(link_target)?;
    }
    Ok(())
}

// Homebrew 4dacfe77: extend/pathname.rb:502-509 (install_symlink_p): mkpath
// the link's parent, realpath it (dstdir), expand the source against dstdir,
// resolve the source's dirname via realpath when it exists, then create a
// RELATIVE symlink at dstdir/<basename> with ln_sf (force) semantics.
pub(in crate::postinstall::structured) fn create_relative_symlink(
    source: &Path,
    link: &Path,
) -> Result<()> {
    let parent = link.parent().context("symlink target has no parent")?;
    fs::create_dir_all(parent)?;
    let dstdir =
        fs::canonicalize(parent).with_context(|| format!("resolving {}", parent.display()))?;
    // Pathname(src).expand_path(dstdir): resolve relative sources against dstdir.
    let mut src = if source.is_absolute() {
        source.to_path_buf()
    } else {
        dstdir.join(source)
    };
    // src = src.dirname.realpath/src.basename if src.dirname.exist?
    if let Some(dir) = src.parent() {
        if dir.exists() {
            src = fs::canonicalize(dir)?.join(src.file_name().unwrap_or_default());
        }
    }
    let link = dstdir.join(link.file_name().unwrap_or_default());
    if link.exists() || link.is_symlink() {
        remove_any(&link)?;
    }
    let rel = relative_path(&dstdir, &src);
    #[cfg(unix)]
    std::os::unix::fs::symlink(rel, &link)?;
    Ok(())
}

// Lexical relative path from directory `from` to `to` (both absolute) — the
// equivalent of Pathname#relative_path_from (extend/pathname.rb:508).
fn relative_path(from: &Path, to: &Path) -> PathBuf {
    let from_c: Vec<Component<'_>> = from.components().collect();
    let to_c: Vec<Component<'_>> = to.components().collect();
    let common = from_c.iter().zip(&to_c).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..from_c.len() {
        out.push("..");
    }
    for c in &to_c[common..] {
        out.push(c.as_os_str());
    }
    out
}
// Homebrew 4dacfe77: install_steps.rb:1253-1265 (create_symlink). Upstream:
// `target.dirname.mkpath`; then when `step["sudo"] == true` or
// `"if_needed"` on a non-writable parent → `sudo /bin/ln -s [-f]`; else
// `FileUtils.rm_f target if force` (files/symlinks only — a real dir target is
// left and `File.symlink` then raises EEXIST) + `File.symlink source, target`.
fn create_symlink(
    post_env: &PostinstallEnvSnapshot,
    source: &str,
    target: &Path,
    step: &Value,
) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let sudo_requested = matches!(step.get("sudo"), Some(Value::Bool(true)))
        || step.get("sudo").and_then(Value::as_str) == Some("if_needed");
    let parent_writable = target.parent().map(dir_writable).unwrap_or(false);
    if sudo_requested && !parent_writable {
        let mut args = vec!["-s".to_string()];
        if bool_or(step, "force", false) {
            args.push("-f".to_string());
        }
        args.push(source.to_string());
        args.push(target.display().to_string());
        run_command(RunCommand::new(Some(post_env), Path::new("/bin/ln"), &args).sudo(true))?;
    } else {
        if bool_or(step, "force", false) {
            // FileUtils.rm_f (force) — removes files/symlinks, ignores errors.
            let _ = fs::remove_file(target);
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(source, target)?;
    }
    Ok(())
}
#[cfg(unix)]
fn path_cstring(path: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL byte"))
}

// Homebrew 4dacfe77: Pathname#writable? = File.writable? = access(path, W_OK).
fn dir_writable(dir: &Path) -> bool {
    #[cfg(unix)]
    {
        let Ok(path) = path_cstring(dir) else {
            return false;
        };
        // SAFETY: `path` is a NUL-terminated copy of the OS path bytes and
        // lives for the duration of the call.
        unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
    }
    #[cfg(not(unix))]
    {
        let probe = dir.join(".glu-write-test");
        let ok = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
            .is_ok();
        if ok {
            let _ = fs::remove_file(&probe);
        }
        ok
    }
}

// glu helper (upstream uses FileUtils.rm_rf / rm_f inline; no named
// equivalent). `is_dir() && !is_symlink()` matches FileUtils.rm_rf's refusal to
// follow symlinks.
pub(in crate::postinstall::structured) fn remove_any(path: &Path) -> Result<()> {
    if path.is_dir() && !path.is_symlink() {
        fs::remove_dir_all(path)?;
    } else if path.exists() || path.is_symlink() {
        fs::remove_file(path)?;
    }
    Ok(())
}
// Homebrew 4dacfe77: install_steps.rb:1213-1223 (resolve_step_source) — glob
// results deduped (.uniq) before the exactly-one check; error names the path.
pub(in crate::postinstall::structured) fn single_source(
    ctx: &PostinstallContext<'_>,
    step: &Value,
) -> Result<PathBuf> {
    if bool_or(step, "source_glob", false) {
        let sources = expand_glob(ctx, req(step, "source")?)?
            .into_iter()
            .filter(|p| p.exists() || p.is_symlink())
            .collect::<BTreeSet<_>>();
        if sources.len() != 1 {
            bail!(
                "postinstall source glob must match exactly one path: {}",
                path(ctx, req(step, "source")?)?.display()
            );
        }
        return Ok(sources.into_iter().next().unwrap());
    }
    path(ctx, req(step, "source")?)
}

// Homebrew 4dacfe77: install_steps.rb:1500-1506 (link_source) — `relative`
// specs link the raw template-expanded path, everything else resolves like a
// path. Matches (modulo the `~`/empty-base gap noted on `path`).
fn link_source(ctx: &PostinstallContext<'_>, spec: &Value) -> Result<String> {
    if spec.get("base").and_then(Value::as_str) == Some("relative") {
        return Ok(expand(
            ctx,
            spec.get("path").and_then(Value::as_str).unwrap_or(""),
        ));
    }
    Ok(path(ctx, spec)?.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    #[cfg(unix)]
    fn copy_entry_preserve_metadata_keeps_modes_for_move_fallback() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(src.join("dir")).unwrap();
        fs::write(src.join("dir/file"), "x").unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o751)).unwrap();
        fs::set_permissions(src.join("dir"), fs::Permissions::from_mode(0o750)).unwrap();
        fs::set_permissions(src.join("dir/file"), fs::Permissions::from_mode(0o640)).unwrap();

        copy_entry(&src, &dst, true, false).unwrap();

        assert_eq!(
            fs::metadata(&dst).unwrap().permissions().mode() & 0o777,
            0o751
        );
        assert_eq!(
            fs::metadata(dst.join("dir")).unwrap().permissions().mode() & 0o777,
            0o750
        );
        assert_eq!(
            fs::metadata(dst.join("dir/file"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
    }
}
