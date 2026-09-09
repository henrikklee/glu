use super::super::*;

// Homebrew 7d2a02d2: install_steps.rb (run_set_permissions) +
// (existing_step_paths) — `chmod [-R] -- <permissions> <paths>`
// without sudo, skipping paths whose `.exist?` is false (including dangling
// symlinks).
pub(in crate::postinstall::structured) fn chmod_paths(
    ctx: &PostinstallContext<'_>,
    step: &Value,
) -> Result<()> {
    let permissions = match step.get("permissions") {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Number(value)) => {
            format!("{:o}", value.as_u64().context("invalid permissions")?)
        }
        _ => bail!("set_permissions missing permissions"),
    };
    let mut paths = Vec::new();
    for spec in step
        .get("paths")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        paths.extend(expand_glob(ctx, spec)?);
    }
    let existing = paths
        .into_iter()
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    if existing.is_empty() {
        return Ok(());
    }
    let mut args = Vec::new();
    if !bool_or(step, "non_recursive", false) {
        args.push("-R".to_string());
    }
    args.push("--".to_string());
    args.push(permissions);
    args.extend(existing);
    run_command(RunCommand::new(
        Some(ctx.env),
        Path::new("/bin/chmod"),
        &args,
    ))?;
    Ok(())
}
// Homebrew 7d2a02d2: install_steps.rb (run_set_ownership). Upstream:
// per-path `Cask::Quarantine.app_management_permissions_granted?` (macOS App
// Management privacy permission, cask/quarantine.rb) raising CaskError with
// remediation when not granted; then `sudo chown [-R] -- user:group paths` with
// user default ::User.current (utils/user.rb — Etc.getpwuid(Process.euid).name;
// glu approximates with $USER) and group default "staff". The "may request
// your password" notice is written to the captured worker stderr. App Management permission
// probing follows cask/quarantine.rb in `app_management_permissions_granted`.
pub(in crate::postinstall::structured) fn chown_paths(
    ctx: &PostinstallContext<'_>,
    step: &Value,
) -> Result<()> {
    let user = step
        .get("user")
        .and_then(Value::as_str)
        .map(|v| expand(ctx, v))
        .or_else(|| Some(ctx.env.user.clone()))
        .unwrap_or_default();
    let group = step
        .get("group")
        .and_then(Value::as_str)
        .map(|v| expand(ctx, v))
        .unwrap_or_else(|| "staff".to_string());
    let mut paths = Vec::new();
    for spec in step
        .get("paths")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        paths.extend(expand_glob(ctx, spec)?);
    }
    let existing = paths.into_iter().filter(|p| p.exists()).collect::<Vec<_>>();
    if existing.is_empty() {
        return Ok(());
    }
    // macOS App Management privacy permission (cask/quarantine.rb) — same
    // remediation message as upstream's CaskError.
    for p in &existing {
        if app_management_permissions_granted(ctx.env, p)? {
            continue;
        }
        bail!(
            "Cannot change the ownership of '{}' because your terminal does not have App Management permissions.\nmacOS prevents modifying apps without these permissions, even when using `sudo`.\nTo fix this, approve the permissions prompt (if one was just shown) or go to\nSystem Settings → Privacy & Security → App Management and add or enable your terminal.\nThen run this command again.",
            p.display()
        );
    }
    crate::worker_output::notice(&format!(
        "Changing ownership of paths required by {} with `sudo` (which may request your password)...",
        ctx.package.name.0
    ));
    let mut args = Vec::new();
    if !bool_or(step, "non_recursive", false) {
        args.push("-R".to_string());
    }
    args.push("--".to_string());
    args.push(format!("{user}:{group}"));
    args.extend(existing.iter().map(|p| p.display().to_string()));
    run_command(RunCommand::new(Some(ctx.env), Path::new("/usr/sbin/chown"), &args).sudo(true))?;
    Ok(())
}

// Homebrew 7d2a02d2: cask/quarantine.rb
// (app_management_permissions_granted?). Upstream returns true for
// non-directories; otherwise computes whether the current user/group/mode looks
// writable without sudo, then either performs a plain write probe or a sudo
// touch/rm probe. Only App-Management-style `touch: <path>: Operation not
// permitted` is converted to false; other command errors propagate. The
// undocumented HOMEBREW_NO_APP_MANAGEMENT_PERMISSIONS_PROMPT path is preserved
// as the same post-denial warning/false result.
fn app_management_permissions_granted(
    post_env: &PostinstallEnvSnapshot,
    app: &Path,
) -> Result<bool> {
    if !app.is_dir() {
        return Ok(true);
    }
    let test_file = app.join(".homebrew-write-test");
    if looks_writable_without_sudo(app)? {
        match fs::write(&test_file, "") {
            Ok(_) => {
                let _ = fs::remove_file(&test_file);
                return Ok(true);
            }
            Err(err) if is_eacces_or_eperm(&err) => {}
            Err(err) => return Err(err.into()),
        }
    } else {
        match run_command(
            RunCommand::new(
                Some(post_env),
                Path::new("/usr/bin/touch"),
                &[test_file.display().to_string()],
            )
            .sudo(true),
        ) {
            Ok(_) => {
                run_command(
                    RunCommand::new(
                        Some(post_env),
                        Path::new("/bin/rm"),
                        &[test_file.display().to_string()],
                    )
                    .sudo(true),
                )?;
                return Ok(true);
            }
            Err(err) if is_app_management_touch_denial(&err, &test_file) => {}
            Err(err) => return Err(err),
        }
    }
    if postinstall_env_var_present(
        Some(post_env),
        "HOMEBREW_NO_APP_MANAGEMENT_PERMISSIONS_PROMPT",
    ) {
        crate::worker_output::notice(
            "Your terminal does not have App Management permissions, so glu will delete and reinstall the app.\nThis may result in some configurations (like notification settings or location in the Dock/Launchpad) being lost.\nTo fix this, go to System Settings → Privacy & Security → App Management and add or enable your terminal.",
        );
    }
    Ok(false)
}

fn looks_writable_without_sudo(path: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::symlink_metadata(path)?;
        let mode = meta.mode();
        // SAFETY: simple process identity getters.
        let euid = unsafe { libc::geteuid() };
        let egid = unsafe { libc::getegid() };
        if meta.uid() == euid {
            return Ok(mode & 0o200 != 0);
        }
        if meta.gid() == egid || supplementary_groups_contain(meta.gid()) {
            return Ok(mode & 0o020 != 0);
        }
        Ok(mode & 0o002 != 0)
    }
    #[cfg(not(unix))]
    {
        Ok(dir_writable(path))
    }
}

#[cfg(unix)]
fn supplementary_groups_contain(gid: u32) -> bool {
    // First call asks libc for the number of supplementary groups.
    let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if count <= 0 {
        return false;
    }
    let mut groups = vec![0 as libc::gid_t; count as usize];
    // SAFETY: `groups` has capacity for `count` gid_t values.
    let written = unsafe { libc::getgroups(count, groups.as_mut_ptr()) };
    written > 0 && groups[..written as usize].contains(&gid)
}

fn is_eacces_or_eperm(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM))
}

fn is_app_management_touch_denial(err: &anyhow::Error, test_file: &Path) -> bool {
    let Some(command) = err.downcast_ref::<PostinstallCommandFailed>() else {
        return false;
    };
    let stderr = String::from_utf8_lossy(&command.stderr);
    stderr.contains(&format!(
        "touch: {}: Operation not permitted",
        test_file.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    #[cfg(unix)]
    fn app_management_permission_helpers_match_homebrew_shape() {
        let tmp = TempDir::new().unwrap();
        let app = tmp.path().join("Example.app");
        fs::create_dir_all(&app).unwrap();
        fs::set_permissions(&app, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(looks_writable_without_sudo(&app).unwrap());
        fs::set_permissions(&app, fs::Permissions::from_mode(0o500)).unwrap();
        assert!(!looks_writable_without_sudo(&app).unwrap());
        fs::set_permissions(&app, fs::Permissions::from_mode(0o700)).unwrap();

        let test_file = app.join(".homebrew-write-test");
        let err: anyhow::Error = PostinstallCommandFailed {
            command: PathBuf::from("touch"),
            args: vec![test_file.display().to_string()],
            stderr: format!("touch: {}: Operation not permitted\n", test_file.display())
                .into_bytes(),
        }
        .into();
        assert!(is_app_management_touch_denial(&err, &test_file));
    }
}
