use super::super::*;

// Homebrew 4dacfe77: install_steps.rb:1321-1356 (run_init_data_dir).
// mysql/mariadb pass `--user=#{ENV.fetch("USER")}` upstream, so glu fails
// closed when the per-context postinstall USER snapshot is absent. TMPDIR is
// unset via run_command's env_remove (`with_env(TMPDIR: nil)` upstream).
pub(in crate::postinstall::structured) fn init_data_dir(
    ctx: &PostinstallContext<'_>,
    step: &Value,
) -> Result<()> {
    let using = step.get("using").and_then(Value::as_str).unwrap_or("");
    let marker = match using {
        "postgresql_initdb" => "PG_VERSION",
        "mysql_initialize" => "mysql/general_log.CSM",
        "mariadb_install_db" => "mysql/user.frm",
        other => bail!("unknown postinstall data directory initialiser: {other}"),
    };
    let data = path(ctx, req(step, "path")?)?;
    fs::create_dir_all(&data)?;
    if env::var_os("HOMEBREW_GITHUB_ACTIONS").is_some() || data.join(marker).exists() {
        return Ok(());
    }
    let user = ctx.env.user.clone();
    if matches!(using, "mysql_initialize" | "mariadb_install_db") && user.is_empty() {
        bail!("postinstall data directory initialiser {using} requires USER");
    }
    match using {
        "postgresql_initdb" => run_command(RunCommand::new(
            Some(ctx.env),
            &ctx.keg.join("bin/initdb"),
            &[
                format!(
                    "--locale={}",
                    step.get("locale")
                        .and_then(Value::as_str)
                        .unwrap_or("en_US.UTF-8")
                ),
                "-E".into(),
                "UTF-8".into(),
                data.to_string_lossy().to_string(),
            ],
        ))?,
        "mysql_initialize" => run_command(
            RunCommand::new(
                Some(ctx.env),
                &ctx.keg.join("bin/mysqld"),
                &[
                    "--initialize-insecure".into(),
                    format!("--user={user}"),
                    format!("--basedir={}", ctx.keg.display()),
                    format!("--datadir={}", data.display()),
                    "--tmpdir=/tmp".into(),
                ],
            )
            .env_remove("TMPDIR"),
        )?,
        "mariadb_install_db" => run_command(
            RunCommand::new(
                Some(ctx.env),
                &ctx.keg.join("bin/mysql_install_db"),
                &[
                    "--verbose".into(),
                    format!("--user={user}"),
                    format!("--basedir={}", ctx.keg.display()),
                    format!("--datadir={}", data.display()),
                    "--tmpdir=/tmp".into(),
                ],
            )
            .env_remove("TMPDIR"),
        )?,
        _ => unreachable!(),
    };
    Ok(())
}
// Homebrew 4dacfe77: install_steps.rb:1221-1251 (run_terminate_process).
// match: :name → /usr/bin/killall <name>; :full → /usr/bin/pkill -f <name>
// (install_steps.rb:1233-1236); sudo from step (1238); attempts retry with a
// 1s sleep, and must_succeed=false failures surface a notice then continue.
// Provenance note: Homebrew `ohai`s the notices (stdout) with formatting;
// glu writes them to captured worker stderr; the parent relays them through
// `ExecutionEvents` so they do not corrupt the live footer.
pub(in crate::postinstall::structured) fn terminate_process(
    ctx: &PostinstallContext<'_>,
    step: &Value,
) -> Result<()> {
    for notice in step
        .get("notices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        crate::worker_output::notice(&expand(ctx, notice.as_str().unwrap_or("")));
    }
    let name = expand(ctx, step.get("name").and_then(Value::as_str).unwrap_or(""));
    if name.is_empty() {
        return Ok(());
    }
    let (command, args) = if step.get("match").and_then(Value::as_str) == Some("full") {
        // Homebrew 4dacfe77: install_steps.rb:1233-1236 — full match →
        // /usr/bin/pkill -f <name>; name match → /usr/bin/killall <name>.
        (
            PathBuf::from("/usr/bin/pkill"),
            vec!["-f".to_string(), name.clone()],
        )
    } else {
        (PathBuf::from("/usr/bin/killall"), vec![name.clone()])
    };
    let attempts = step.get("attempts").and_then(Value::as_u64).unwrap_or(1);
    // Homebrew 4dacfe77: install_steps.rb:1238 — `sudo: step["sudo"] == true`.
    let sudo = step.get("sudo").and_then(Value::as_bool) == Some(true);
    // Homebrew 4dacfe77: install_steps.rb:1239-1251 — `SystemCommand.run!`
    // raises on nonzero exit and the rescue swallows it when `must_succeed` is
    // false. The raw killall stderr ("No matching processes belonging to you
    // were found") is routine — the agent usually isn't running — and is never
    // surfaced; only the step's `failure_message` (opoo) shows.
    for attempt in 0..attempts {
        let output = run_command(
            RunCommand::new(Some(ctx.env), &command, &args)
                .must_succeed(false)
                .sudo(sudo),
        )?;
        if output.status.success() {
            return Ok(());
        }
        if attempt + 1 < attempts {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    if let Some(message) = step.get("failure_message").and_then(Value::as_str) {
        crate::worker_output::notice(&expand(ctx, message));
    }
    if bool_or(step, "must_succeed", false) {
        bail!("postinstall failed to terminate process: {name}");
    }
    Ok(())
}
/// Sets a dylib's install ID in-process (no `install_name_tool`) for the
/// structured-postinstall `change_dylib_id` step, then ad-hoc re-signs the
/// file. Step type and behavior mirror Homebrew's `InstallSteps.change_dylib_id`
/// (install_steps.rb:27-36); the in-process Mach-O editing is glu's own
/// (`bottle/macho.rs`) instead of ruby-macho. `resolve_source`
/// resolves the source path strictly when set.
///
// Homebrew 4dacfe77: install_steps.rb:27-36 (InstallSteps.change_dylib_id).
// Upstream uses ruby-macho in-process editing too — `MachO::Tools.change_dylib_id`
// + `file.ensure_writable`, then `MachO.codesign! file if Hardware::CPU.arm?`.
// glu's in-process Mach-O rewrite lives in bottle/macho.rs; ad-hoc signing
// lives in bottle/codesign.rs.
// PLATFORM (arm64 macOS, v0.1): Mach-O only, so macOS-only. The ad-hoc
// re-sign below is arm64-compliant (Homebrew re-signs when
// `Hardware::CPU.arm?`, install_steps.rb:33) — gated on `target_arch =
// "aarch64"` so Intel macOS automatically stops re-signing, matching upstream.
// Linux: N/A (no Mach-O bottles).
// `MachO.codesign!` is ad-hoc signing for Homebrew's bottle use; glu uses
// `sign_path_adhoc` (`bottle/codesign.rs`), which preserves ruby-macho's
// CodeSigning identifier rules before signing in-process.
pub(in crate::postinstall::structured) fn change_dylib_id(
    ctx: &PostinstallContext<'_>,
    step: &Value,
) -> Result<()> {
    let source = path(ctx, req(step, "source")?)?;
    let source = if step.get("resolve_source").and_then(Value::as_bool) == Some(true) {
        fs::canonicalize(&source).with_context(|| format!("resolving {}", source.display()))?
    } else {
        source
    };
    let id = expand(ctx, step.get("id").and_then(Value::as_str).unwrap_or(""));
    let mut data = fs::read(&source).with_context(|| format!("reading {}", source.display()))?;
    crate::bottle::macho::set_dylib_id(&mut data, &id)
        .with_context(|| format!("setting dylib id on {}", source.display()))?;
    let _writable = crate::bottle::writer::WritableFileGuard::new(&source)?;
    fs::write(&source, &data).with_context(|| format!("writing {}", source.display()))?;
    if cfg!(target_arch = "aarch64")
        && crate::bottle::macho::header_layout(&data[..data.len().min(24)]).is_some()
    {
        // Homebrew 4dacfe77: install_steps.rb:33 — re-sign only on arm64
        // (`Hardware::CPU.arm?`); Intel macOS must NOT re-sign (PLATFORM marker).
        // The surrounding guard is the `file.ensure_writable` parity for both
        // the dylib-id rewrite and this re-sign, avoiding a second chmod cycle.
        crate::bottle::codesign::sign_path_adhoc_assume_writable(&source)?;
    }
    Ok(())
}
// Homebrew 4dacfe77: install_steps/formula_actions.rb:120-140
// (run_configure_clang_system) + utils/clang.rb:14-31 (write_system_config_files).
// PLATFORM (arm64 macOS, v0.1): macOS-only by construction (the cfg!
// early-return below mirrors upstream's own guard,
// formula_actions.rb:121 `return unless simulating_or_running_on_macos?`).
// Intel: unchanged (still macOS; `uname -m` → x86_64 lands in the same arch
// set upstream expects). Linux: N/A — upstream no-ops off macOS too.
// Sysroot matches utils/clang.rb:17-21: macos_version is `MacOS.version`
// (full version with patch stripped, os/mac.rb:32) and the sysroot is the
// versioned CLT SDK. Unknown kernel/macOS versions fail closed, matching
// upstream's refusal to continue without a kernel version. Config files are
// written atomically below, matching `Pathname#atomic_write`.
pub(in crate::postinstall::structured) fn configure_clang_system(
    ctx: &PostinstallContext<'_>,
) -> Result<()> {
    if cfg!(not(target_os = "macos")) {
        return Ok(());
    }
    let kernel_out = run_command(RunCommand::new(
        Some(ctx.env),
        Path::new("/usr/bin/uname"),
        &["-r".into()],
    ))?;
    let kernel = String::from_utf8_lossy(&kernel_out.stdout)
        .trim()
        .split('.')
        .next()
        .unwrap_or("")
        .to_string();
    if kernel.is_empty() {
        bail!("configure_clang_system could not determine Darwin kernel version");
    }
    let arch_out = run_command(RunCommand::new(
        Some(ctx.env),
        Path::new("/usr/bin/uname"),
        &["-m".into()],
    ))?;
    let arch = String::from_utf8_lossy(&arch_out.stdout).trim().to_string();
    let macos_out = run_command(RunCommand::new(
        Some(ctx.env),
        Path::new("/usr/bin/sw_vers"),
        &["-productVersion".into()],
    ))?;
    let macos_full = String::from_utf8_lossy(&macos_out.stdout)
        .trim()
        .to_string();
    if macos_full.is_empty() {
        bail!("configure_clang_system could not determine macOS version");
    }
    // Homebrew 4dacfe77: MacOS.version (os/mac.rb:32) — strip the patch.
    let macos = strip_patch(&macos_full);
    let mut arches = BTreeSet::from([
        "arm64".to_string(),
        "x86_64".to_string(),
        "aarch64".to_string(),
    ]);
    if !arch.is_empty() {
        arches.insert(arch);
    }
    let names = arches
        .iter()
        .flat_map(|target| {
            [
                format!("{target}-apple-darwin{kernel}.cfg"),
                format!("{target}-apple-macosx{macos}.cfg"),
            ]
        })
        .collect::<Vec<_>>();
    let config_dir = ctx.keg.join("etc/clang");
    if names.iter().all(|name| config_dir.join(name).exists()) {
        return Ok(());
    }
    // Homebrew 4dacfe77: utils/clang.rb:17-21 — versioned CLT SDK; the
    // "running newer than target" branch is dead since target == running here.
    let sdk = format!("/Library/Developer/CommandLineTools/SDKs/MacOSX{macos}.sdk");
    fs::create_dir_all(&config_dir)?;
    for name in names {
        atomic_write(
            &config_dir.join(name),
            format!("-isysroot {sdk}\n").as_bytes(),
        )?;
    }
    Ok(())
}

// ===== ports of the 2026-08 Homebrew step actions (install_steps/formula_actions.rb) =====

// Homebrew 4dacfe77: version.rb:686-692 (Version#major_minor — tokens[0..1], a
// single-component version yields that component). Used by configure_php /
// bootstrap_cpython via context_version_major_minor (install_steps.rb:1370-1374);
// this is also what the missing `{{version.major_minor}}` token expands to.
pub(in crate::postinstall::structured) fn version_major_minor(version: &str) -> Option<String> {
    let mut parts = version.split('.');
    let major = parts.next()?;
    if major.is_empty() {
        return None;
    }
    match parts.next() {
        Some(minor) if !minor.is_empty() => Some(format!("{major}.{minor}")),
        _ => Some(major.to_string()),
    }
}

// Homebrew 4dacfe77: version.rb:656-659 (Version#major — tokens.first). Also
// what the `{{version.major}}` token expands to (install_steps.rb:1367-1369).
pub(in crate::postinstall::structured) fn version_major(version: &str) -> Option<String> {
    let major = version.split('.').next()?;
    if major.is_empty() {
        None
    } else {
        Some(major.to_string())
    }
}

// Homebrew 4dacfe77: Version#strip_patch (used by MacOS.version, os/mac.rb:32)
// — "15.7.9" → "15.7", "15.7" → "15.7", "15" → "15".
fn strip_patch(version: &str) -> String {
    let parts = version.split('.').collect::<Vec<_>>();
    if parts.len() >= 3 {
        parts[..2].join(".")
    } else {
        version.to_string()
    }
}

// FileUtils.chmod with a numeric mode — sets the exact permission bits
// (matches Homebrew's FileUtils.chmod 0755/0644 usage in these actions).
pub(in crate::postinstall::structured) fn chmod_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

// FileUtils.rm_r Dir[base/pattern], force: true — brace-expanded glob removal
// that ignores missing entries and glob errors (Dir[] returns [] on no match).
fn remove_glob_quiet(base: &Path, pattern: &str) -> Result<()> {
    for expanded in brace_expand(&base.join(pattern).to_string_lossy()) {
        if let Ok(matches) = glob::glob(&expanded) {
            for path in matches.filter_map(Result::ok) {
                remove_any(&path)?;
            }
        }
    }
    Ok(())
}

// Ruby Dir.glob / Pathname.glob for exact-one checks — returns the matches.
fn glob_matches(pattern: &Path) -> Result<Vec<PathBuf>> {
    let mut matches = Vec::new();
    for expanded in brace_expand(&pattern.to_string_lossy()) {
        matches.extend(glob::glob(&expanded)?.filter_map(Result::ok));
    }
    Ok(matches)
}

// Homebrew 4dacfe77: install_steps/formula_actions.rb:141-199 (run_configure_php).
// Port notes: context_version_major_minor → version_major_minor;
// `FileUtils.cp_r "#{pear_prefix}/.", pear_path` is a contents copy via
// `copy_entry_contents`; FileUtils.chmod/touch are exact-mode chmod /
// empty-file creation; Utils::Inreplace → inreplace_regexp_file. PLATFORM
// (arm64 macOS, v0.1): php is macOS-installable, so this RUNS on the v0.1
// target. Intel: unchanged. Linux: same paths (HOMEBREW_PREFIX/lib/php/pecl
// etc.); individual non-inreplace writes in this action are plain writes.
pub(in crate::postinstall::structured) fn configure_php(
    ctx: &PostinstallContext<'_>,
) -> Result<()> {
    let keg = ctx.keg;
    let prefix = &ctx.prefix.0;
    let name = &ctx.package.name.0;
    let pear_prefix = keg.join("share").join(name).join("pear");
    let channels = [
        pear_prefix.join(".channels"),
        pear_prefix.join(".channels/.alias"),
    ];
    for directory in &channels {
        if directory.is_dir() {
            chmod_mode(directory, 0o755)?; // FileUtils.chmod 0755, directory
        }
    }
    let mut pear_files: Vec<PathBuf> = [".depdblock", ".filemap", ".depdb", ".lock"]
        .iter()
        .map(|f| pear_prefix.join(f))
        .filter(|p| p.is_file())
        .collect();
    for directory in &channels {
        if directory.is_dir() {
            for child in fs::read_dir(directory)? {
                let child = child?.path();
                if child.is_file() {
                    pear_files.push(child);
                }
            }
        }
    }
    for file in &pear_files {
        chmod_mode(file, 0o644)?; // FileUtils.chmod 0644, pear_files
    }

    let pecl_path = prefix.join("lib/php/pecl");
    fs::create_dir_all(&pecl_path)?;
    let prefix_pecl = keg.join("pecl");
    if prefix_pecl.is_symlink() {
        fs::remove_file(&prefix_pecl)?; // .unlink if symlink
    }
    if !prefix_pecl.exists() {
        std::os::unix::fs::symlink(&pecl_path, &prefix_pecl)?; // File.symlink pecl_path, prefix_pecl
    }
    let php_config_out = run_command(RunCommand::new(
        Some(ctx.env),
        &keg.join("bin/php-config"),
        &["--extension-dir".into()],
    ))?;
    let php_basename = Path::new(String::from_utf8_lossy(&php_config_out.stdout).trim())
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    fs::create_dir_all(pecl_path.join(&php_basename))?;

    let version_major_minor = version_major_minor(&ctx.package.version)
        .context("PHP configuration requires a version")?;
    let pear_dir = if name == "php" {
        "pear".to_string()
    } else {
        format!("pear@{version_major_minor}")
    };
    let pear_path = prefix.join("share").join(&pear_dir);
    copy_entry_contents(&pear_prefix, &pear_path, false, false)?; // FileUtils.cp_r "#{pear_prefix}/.", pear_path
    let php_ext_dir = prefix
        .join("opt")
        .join(name)
        .join("lib/php")
        .join(&php_basename);
    let etc_php = prefix.join("etc").join("php").join(&version_major_minor);
    let pear_config: [(&str, PathBuf); 11] = [
        ("php_ini", etc_php.join("php.ini")),
        ("php_dir", pear_path.clone()),
        ("doc_dir", pear_path.join("doc")),
        ("ext_dir", pecl_path.join(&php_basename)),
        ("bin_dir", prefix.join("opt").join(name).join("bin")),
        ("data_dir", pear_path.join("data")),
        ("cfg_dir", pear_path.join("cfg")),
        ("www_dir", pear_path.join("htdocs")),
        ("man_dir", prefix.join("share/man")),
        ("test_dir", pear_path.join("test")),
        ("php_bin", prefix.join("opt").join(name).join("bin/php")),
    ];
    for (key, value) in &pear_config {
        // value.mkpath if /(?<!bin|man)_dir$/.match?(key)
        if key.ends_with("_dir") && !key.ends_with("bin_dir") && !key.ends_with("man_dir") {
            fs::create_dir_all(value)?;
        }
        run_command(RunCommand::new(
            Some(ctx.env),
            &keg.join("bin/pear"),
            &[
                "config-set".into(),
                key.to_string(),
                value.display().to_string(),
                "system".into(),
            ],
        ))?;
    }
    run_command(RunCommand::new(
        Some(ctx.env),
        &keg.join("bin/pear"),
        &["update-channels".into()],
    ))?;
    if name == "php" {
        return Ok(());
    }

    let ext_config_path = etc_php.join("conf.d/ext-opcache.ini");
    fs::create_dir_all(ext_config_path.parent().unwrap())?;
    let zend_extension_line = format!(
        "zend_extension=\"{}\"",
        php_ext_dir.join("opcache.so").display()
    );
    if ext_config_path.exists() {
        // Utils::Inreplace.inreplace(ext_config_path, /^\s*zend_extension\s*=.*$/, line)
        inreplace_regexp_file(
            &ext_config_path,
            r"^\s*zend_extension\s*=.*$",
            &zend_extension_line,
        )?;
    } else {
        // ext_config_path.atomic_write <<~INI ... INI
        fs::write(
            &ext_config_path,
            format!("[opcache]\n{zend_extension_line}\n"),
        )?;
    }
    Ok(())
}

// Homebrew 4dacfe77: install_steps/formula_actions.rb:200-279 (run_bootstrap_cpython).
// Port notes: ENV.delete("PYTHONPATH") is applied to every child process via
// run_command's env_remove; lib_cellar is the frameworks path on macOS
// (formula_actions.rb:205-210, SimulateSystem — glu is macOS-only in v0.1, the
// linux branch is kept for parity); install_symlink → relative
// create_relative_symlink; Dir[] globs → remove_glob_quiet; Utils::Inreplace →
// inreplace_regexp_file. PLATFORM (arm64 macOS, v0.1): python is macOS-installable,
// so this RUNS on the v0.1 target. Intel: unchanged. Linux: lib_cellar switches
// to keg/lib/python<mm> and openssl@3/sqlite opt dirs resolve the same.
pub(in crate::postinstall::structured) fn bootstrap_cpython(
    ctx: &PostinstallContext<'_>,
) -> Result<()> {
    let keg = ctx.keg;
    let prefix = &ctx.prefix.0;
    let version_major_minor = version_major_minor(&ctx.package.version)
        .context("CPython bootstrap requires a version")?;
    let site_packages = prefix.join(format!("lib/python{version_major_minor}/site-packages"));
    let lib_cellar = if cfg!(target_os = "macos") {
        keg.join(format!(
            "Frameworks/Python.framework/Versions/{version_major_minor}/lib/python{version_major_minor}"
        ))
    } else {
        keg.join(format!("lib/python{version_major_minor}"))
    };
    let site_packages_cellar = lib_cellar.join("site-packages");
    fs::create_dir_all(&site_packages)?; // site_packages.mkpath
    if site_packages_cellar.exists() || site_packages_cellar.is_symlink() {
        remove_any(&site_packages_cellar)?; // FileUtils.rm_rf site_packages_cellar
    }
    create_relative_symlink(&site_packages, &site_packages_cellar)?; // parent.install_symlink site_packages

    // FileUtils.rm_r Dir[site_packages/"sitecustomize.py[co]"], force: true
    remove_glob_quiet(&site_packages, "sitecustomize.py[co]")?;
    for package in ["setuptools", "distribute", "pip", "wheel"] {
        remove_glob_quiet(&site_packages, &format!("{package}[-_.][0-9]*"))?;
        remove_glob_quiet(&site_packages, package)?;
    }

    let python = keg.join(format!("bin/python{version_major_minor}"));
    run_command(
        RunCommand::new(Some(ctx.env), &python, &["-Im".into(), "ensurepip".into()])
            .env_remove("PYTHONPATH"),
    )?;
    let bundled = lib_cellar.join("ensurepip/_bundled");
    let mut wheels = Vec::new();
    for pattern in [
        bundled.join("setuptools-*-py3-none-any.whl"),
        bundled.join("pip-*-py3-none-any.whl"),
        keg.join("libexec/wheel-*-py3-none-any.whl"),
    ] {
        let matches = glob_matches(&pattern)?;
        if matches.len() != 1 {
            bail!(
                "CPython bootstrap wheel must match exactly one path: {}",
                pattern.display()
            );
        }
        wheels.push(matches.into_iter().next().unwrap());
    }
    let mut args = vec![
        "-Im".into(),
        "pip".into(),
        "install".into(),
        "-v".into(),
        "--no-deps".into(),
        "--no-index".into(),
        "--upgrade".into(),
        "--isolated".into(),
        format!("--target={}", site_packages.display()),
    ];
    args.extend(wheels.iter().map(|w| w.display().to_string()));
    run_command(RunCommand::new(Some(ctx.env), &python, &args).env_remove("PYTHONPATH"))?;

    // FileUtils.mv (site_packages/"bin").children, context_path("bin")
    let bin_dir = keg.join("bin");
    fs::create_dir_all(&bin_dir)?;
    for child in fs::read_dir(site_packages.join("bin"))? {
        let child = child?.path();
        move_path(&child, &bin_dir)?;
    }
    fs::remove_dir(site_packages.join("bin"))?; // (site_packages/"bin").rmdir

    // FileUtils.rm_r context_path("bin").glob("pip{,3}"), force: true
    remove_glob_quiet(&bin_dir, "pip{,3}")?;
    // FileUtils.mv context_path("bin")/"wheel", "wheel#{version_major_minor}"
    move_path(
        &bin_dir.join("wheel"),
        &bin_dir.join(format!("wheel{version_major_minor}")),
    )?;

    let libexec_bin = keg.join("libexec/bin");
    fs::create_dir_all(&libexec_bin)?;
    for (short, long) in [
        ("pip", format!("pip{version_major_minor}")),
        ("pip3", format!("pip{version_major_minor}")),
        ("wheel", format!("wheel{version_major_minor}")),
        ("wheel3", format!("wheel{version_major_minor}")),
    ] {
        // (context_path("bin")/long_name).realpath => short_name
        let real = fs::canonicalize(bin_dir.join(&long))
            .with_context(|| format!("resolving {}", bin_dir.join(&long).display()))?;
        create_relative_symlink(&real, &libexec_bin.join(short))?;
    }
    let prefix_bin = prefix.join("bin");
    fs::create_dir_all(&prefix_bin)?;
    // (HOMEBREW_PREFIX/"bin").install_symlink [wheel<mm>, pip<mm>]
    create_relative_symlink(
        &bin_dir.join(format!("wheel{version_major_minor}")),
        &prefix_bin.join(format!("wheel{version_major_minor}")),
    )?;
    create_relative_symlink(
        &bin_dir.join(format!("pip{version_major_minor}")),
        &prefix_bin.join(format!("pip{version_major_minor}")),
    )?;

    // make_cpython_venv_activation_scripts_writable(lib_cellar): chmod u+w on
    // lib_cellar/venv/scripts/**/* files (formula_actions.rb:275-278).
    for entry in glob_matches(&lib_cellar.join("venv/scripts/**/*"))? {
        if entry.is_file() {
            let mode = fs::metadata(&entry)?.permissions().mode();
            chmod_mode(&entry, mode | 0o200)?;
        }
    }
    if version_major_minor != "3.9" {
        return Ok(());
    }

    // distutils.cfg (3.9 only): prefix + openssl@3/sqlite include/lib dirs.
    let include_dirs = [
        prefix.join("include"),
        prefix.join("opt/openssl@3/include"),
        prefix.join("opt/sqlite/include"),
    ];
    let library_dirs = [
        prefix.join("lib"),
        prefix.join("opt/openssl@3/lib"),
        prefix.join("opt/sqlite/lib"),
    ];
    fs::create_dir_all(lib_cellar.join("distutils"))?;
    fs::write(
        lib_cellar.join("distutils/distutils.cfg"),
        format!(
            "[install]\nprefix={}\n[build_ext]\ninclude_dirs={}\nlibrary_dirs={}\n",
            prefix.display(),
            include_dirs
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(":"),
            library_dirs
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(":"),
        ),
    )?;
    // Utils::Inreplace.inreplace(framework_compat,
    //   /^(\s+homebrew_prefix\s+=\s+).*/, "\\1'#{HOMEBREW_PREFIX}'")
    let framework_compat = site_packages.join("setuptools/_distutils/command/_framework_compat.py");
    inreplace_regexp_file(
        &framework_compat,
        r"^(\s+homebrew_prefix\s+=\s+).*",
        &format!(r"\1'{}'", prefix.display()),
    )?;
    Ok(())
}

// Homebrew 4dacfe77: install_steps/formula_actions.rb:280-331
// (run_bootstrap_pypy(abi_version)). Port notes: the module-import probe runs
// with must_succeed=false (@command.run, failures ignored); archives are
// extracted with the `tar` crate (UnpackStrategy.detect for .tar.gz,
// unpack_strategy.rb) into a temp dir under the per-context postinstall TMPDIR;
// `Dir.chdir(source_path)` → run_command's cwd param; install_symlink →
// relative create_relative_symlink.
// PLATFORM (arm64 macOS, v0.1): pypy is macOS-installable, so this RUNS on the
// v0.1 target. Intel: unchanged. Linux: same paths.
pub(in crate::postinstall::structured) fn bootstrap_pypy(
    ctx: &PostinstallContext<'_>,
    abi_version: &str,
) -> Result<()> {
    let keg = ctx.keg;
    let prefix = &ctx.prefix.0;
    let name = &ctx.package.name.0;
    let pypy = keg.join(format!("bin/pypy{abi_version}"));
    for module_name in ["_sqlite3", "_curses", "syslog", "gdbm", "_tkinter"] {
        // @command.run (non-raising): import failures are ignored
        let _ = run_command(
            RunCommand::new(
                Some(ctx.env),
                &pypy,
                &["-c".into(), format!("import {module_name}")],
            )
            .must_succeed(false),
        );
    }
    let site_packages = prefix.join(format!("lib/pypy{abi_version}/site-packages"));
    let libexec_site_packages = keg.join(format!("libexec/lib/pypy{abi_version}/site-packages"));
    let scripts_folder = prefix.join(format!("share/pypy{abi_version}"));
    fs::create_dir_all(&site_packages)?;
    fs::write(site_packages.join(".keepme"), "")?; // FileUtils.touch
    if libexec_site_packages.exists() || libexec_site_packages.is_symlink() {
        remove_any(&libexec_site_packages)?; // FileUtils.rm_rf
    }
    create_relative_symlink(&site_packages, &libexec_site_packages)?;
    if abi_version == "3.9" {
        if scripts_folder.is_symlink() {
            fs::remove_file(&scripts_folder)?; // unlink
                                               // scripts_folder.install_symlink context_path("pkgshare").children
            let pkgshare = keg.join("share").join(name);
            for child in fs::read_dir(&pkgshare)? {
                let child = child?.path();
                create_relative_symlink(&child, &scripts_folder.join(child.file_name().unwrap()))?;
            }
        }
        if !keg.join("libexec/bin").exists() {
            // context_path("libexec").install_symlink scripts_folder => "bin"
            create_relative_symlink(&scripts_folder, &keg.join("libexec/bin"))?;
        }
    }
    fs::create_dir_all(&scripts_folder)?;
    // (libexec_site_packages.parent/"distutils/distutils.cfg").atomic_write
    let distutils = libexec_site_packages
        .parent()
        .unwrap()
        .join("distutils/distutils.cfg");
    fs::create_dir_all(distutils.parent().unwrap())?;
    fs::write(
        &distutils,
        format!("[install]\ninstall-scripts={}\n", scripts_folder.display()),
    )?;

    for package in ["setuptools", "pip"] {
        let archive = keg.join(format!("libexec/post-install-resources/{package}.tar.gz"));
        if !archive.is_file() {
            bail!("PyPy bootstrap archive is missing: {}", archive.display());
        }
        // Dir.mktmpdir("homebrew-pypy-#{package}", HOMEBREW_TEMP)
        let temp_dir = ctx.env.temp.join(format!(
            "glu-pypy-{package}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&temp_dir)?;
        let result = (|| -> Result<()> {
            // UnpackStrategy.detect(archive).extract(to: temporary_path)
            let file = fs::File::open(&archive)?;
            let mut archive_reader = tar::Archive::new(GzDecoder::new(file));
            archive_reader.unpack(&temp_dir)?;
            let children = fs::read_dir(&temp_dir)?
                .filter_map(Result::ok)
                .map(|e| e.path())
                .collect::<Vec<_>>();
            // source_path = the single child dir, else the temp root
            let source_path = if children.len() == 1 && children[0].is_dir() {
                children[0].clone()
            } else {
                temp_dir.clone()
            };
            // Dir.chdir(source_path) { run_command pypy, "-s", "setup.py", ... }
            run_command(
                RunCommand::new(
                    Some(ctx.env),
                    &pypy,
                    &[
                        "-s".into(),
                        "setup.py".into(),
                        "--no-user-cfg".into(),
                        "install".into(),
                        "--force".into(),
                        "--verbose".into(),
                    ],
                )
                .cwd(Some(&source_path)),
            )?;
            Ok(())
        })();
        let _ = remove_any(&temp_dir);
        result?;
    }

    let bin_dir = keg.join("bin");
    fs::create_dir_all(&bin_dir)?;
    // context_path("bin").install_symlink scripts_folder/"pip#{abi}" => "pip_pypy#{abi}"
    create_relative_symlink(
        &scripts_folder.join(format!("pip{abi_version}")),
        &bin_dir.join(format!("pip_pypy{abi_version}")),
    )?;
    let mut prefix_links = vec![bin_dir.join(format!("pip_pypy{abi_version}"))];
    if name == "pypy3" {
        // context_path("bin").install_symlink "pip_pypy#{abi}" => "pip_pypy3"
        create_relative_symlink(
            &bin_dir.join(format!("pip_pypy{abi_version}")),
            &bin_dir.join("pip_pypy3"),
        )?;
        prefix_links.push(bin_dir.join("pip_pypy3"));
    }
    let prefix_bin = prefix.join("bin");
    fs::create_dir_all(&prefix_bin)?;
    // (HOMEBREW_PREFIX/"bin").install_symlink prefix_links
    for link in prefix_links {
        create_relative_symlink(&link, &prefix_bin.join(link.file_name().unwrap()))?;
    }
    Ok(())
}

// Homebrew 4dacfe77: install_steps/formula_actions.rb:66-88
// (run_install_gzipped_executable). Upstream `Zlib::GzipReader` reads one gzip
// member and `target.chmod 0755` sets the mode exactly; `flate2::read::GzDecoder`
// and `chmod_mode(0o755)` match those semantics.
pub(in crate::postinstall::structured) fn install_gzipped_executable(
    ctx: &PostinstallContext<'_>,
    step: &Value,
) -> Result<()> {
    let source = path(ctx, req(step, "source")?)?;
    if !source.exists() {
        return Ok(());
    }
    let target = path(ctx, req(step, "target")?)?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = target.with_file_name(format!(
        ".{}.install-step",
        target.file_name().unwrap_or_default().to_string_lossy()
    ));
    let _ = remove_any(&tmp);
    let result = (|| -> Result<()> {
        let mut src = GzDecoder::new(fs::File::open(&source)?);
        let mut dst = fs::File::create(&tmp)?;
        std::io::copy(&mut src, &mut dst)?;
        if target.exists() || target.is_symlink() {
            fs::remove_file(&target)?;
        }
        fs::rename(&tmp, &target)?;
        fs::remove_file(&source)?;
        chmod_mode(&target, 0o755)?;
        Ok(())
    })();
    let _ = remove_any(&tmp);
    result
}
