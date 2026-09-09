use super::super::*;

// Homebrew 7d2a02d2: install_steps.rb (run_serialised_command).
// `allow_failure` is the JSON inverse of the DSL's `must_succeed`
// (install_steps.rb `"allow_failure" => !must_succeed`, default false —
// run steps fail the install on non-zero exit unless opted out); `sudo` is
// `step["sudo"] == true` (install_steps.rb) and is now implemented.
// Compatibility: glu buffers output and prints it after completion. Homebrew
// streams stdout/stderr through SystemCommand as the child writes
// (print_stdout / print_stderr flags), which matters for long-running steps.
// Provenance note: `writable_paths` / `network_access` are cask step sandbox
// metadata. Formula workers use formula-level paths/network policy, matching
// FormulaInstaller#post_install; these fields do not alter command execution.
pub(in crate::postinstall::structured) fn run_command_step(
    ctx: &PostinstallContext<'_>,
    step: &Value,
) -> Result<()> {
    let command = command_path(ctx, req(step, "command")?)?;
    let args = step
        .get("args")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|v| expand(ctx, v.as_str().unwrap_or("")))
        .collect::<Vec<_>>();
    let cwd = step.get("chdir").map(|v| path(ctx, v)).transpose()?;
    let stdin = if let Some(stdin_path) = step.get("stdin_path") {
        Some(fs::read(path(ctx, stdin_path)?)?)
    } else {
        None
    };
    let envs = step.get("env").and_then(Value::as_object).map(|env| {
        env.iter()
            .map(|(k, v)| (k.clone(), expand(ctx, v.as_str().unwrap_or(""))))
            .collect::<Vec<_>>()
    });
    // Homebrew 7d2a02d2: install_steps.rb — `sudo: step["sudo"] == true`.
    let must_succeed = step.get("allow_failure").and_then(Value::as_bool) != Some(true);
    let sudo = step.get("sudo").and_then(Value::as_bool) == Some(true);
    let output = run_command(
        RunCommand::new(Some(ctx.env), &command, &args)
            .cwd(cwd.as_deref())
            .stdin(stdin)
            .envs(envs)
            .must_succeed(must_succeed)
            .sudo(sudo),
    )?;
    if step
        .get("print_stdout")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        // Deferred while the install footer is live so raw bytes can't shift
        // its in-place redraw; flushed to stdout at footer finish.
        crate::worker_output::stdout_notice(&String::from_utf8_lossy(&output.stdout));
    }
    if !step
        .get("suppress_stderr")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        // Non-fatal: the command either succeeded, or was allow_failure'd —
        // the install continues, so the detail is yellow. Incidental output
        // from a SUCCESSFUL command is omitted in dense mode; verbose logs it
        // (Homebrew streams stderr either way).
        let show_success_output = !output.status.success() || ctx.verbose;
        if show_success_output {
            postinstall_failure_notice(
                &postinstall_label(&ctx.package.name.0, &command, &args),
                &String::from_utf8_lossy(&output.stderr),
                false,
            );
        }
    }
    if let Some(stdout_path) = step.get("stdout_path") {
        // Homebrew 7d2a02d2: install_steps.rb — stdout_path is only
        // written when the command succeeded.
        if output.status.success() {
            let p = path(ctx, stdout_path)?;
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(p, &output.stdout)?;
        }
    }
    Ok(())
}

// glu invention (no Homebrew equivalent): upstream has no special-casing of
// legacy cache-tool `run` steps — a formula emitting one runs its own command.
// glu coalesces known global cache rebuild runs into deferred global flushes.
//
// SUBSTITUTION GUARD: interception substitutes a canonical opt tool at flush
// time, so it must only fire when the step's command IS an installed copy of
// the same cache tool (usually the formula's own keg binary or opt binary) and
// the args/stdout target exactly match the known global rebuild shape. Anything
// else (a formula shipping an unrelated tool, a bare PATH-resolved name, or a
// different invocation) is left alone and runs inline, exactly like Homebrew.
pub(in crate::postinstall::structured) fn global_kind_key(
    ctx: &PostinstallContext<'_>,
    step: &Value,
) -> Result<Option<(String, Vec<String>)>> {
    let command = path(ctx, req(step, "command")?)?;
    let args = step
        .get("args")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|v| expand(ctx, v.as_str().unwrap_or("")))
        .collect::<Vec<_>>();
    if args == ["--force", "--really-force", "--verbose"]
        && is_fontconfig_fc_cache(&ctx.prefix.0, &command)
    {
        return Ok(Some(("fontconfig_fc_cache".to_string(), vec![])));
    }
    if let Some(icon_dir) = gtk_icon_cache_key(&ctx.prefix.0, &command, &args) {
        return Ok(Some(("gtk_update_icon_cache".to_string(), vec![icon_dir])));
    }
    if args.is_empty() && is_gtk_query_immodules_3(&ctx.prefix.0, &command) {
        if let Some(stdout_path) = step.get("stdout_path") {
            return Ok(Some((
                "gtk_query_immodules_3".to_string(),
                vec![path(ctx, stdout_path)?.to_string_lossy().to_string()],
            )));
        }
    }
    Ok(None)
}

pub(in crate::postinstall::structured) fn is_fontconfig_fc_cache(
    prefix: &Path,
    command: &Path,
) -> bool {
    if command == prefix.join("opt/fontconfig/bin/fc-cache") {
        return true;
    }
    // Any installed fontconfig keg's own binary: Cellar/fontconfig/<ver>/bin/fc-cache.
    command.starts_with(prefix.join("Cellar/fontconfig"))
        && command.ends_with(Path::new("bin/fc-cache"))
}

pub(in crate::postinstall::structured) fn gtk_icon_cache_key(
    prefix: &Path,
    command: &Path,
    args: &[String],
) -> Option<String> {
    if !is_gtk_update_icon_cache(prefix, command) {
        return None;
    }
    let icon_dir = match args {
        [a, b, path] if a == "-f" && b == "-t" => path,
        [a, b, path] if a == "-t" && b == "-f" => path,
        [a, b, c, path] if a == "-q" && b == "-t" && c == "-f" => path,
        [a, b, c, path] if a == "-q" && b == "-f" && c == "-t" => path,
        _ => return None,
    };
    Some(icon_dir.clone())
}

fn is_gtk_update_icon_cache(prefix: &Path, command: &Path) -> bool {
    if command == prefix.join("opt/gtk+3/bin/gtk3-update-icon-cache")
        || command == prefix.join("opt/gtk4/bin/gtk4-update-icon-cache")
    {
        return true;
    }
    (command.starts_with(prefix.join("Cellar/gtk+3"))
        && command.ends_with(Path::new("bin/gtk3-update-icon-cache")))
        || (command.starts_with(prefix.join("Cellar/gtk4"))
            && command.ends_with(Path::new("bin/gtk4-update-icon-cache")))
}

pub(in crate::postinstall::structured) fn is_gtk_query_immodules_3(
    prefix: &Path,
    command: &Path,
) -> bool {
    if command == prefix.join("opt/gtk+3/bin/gtk-query-immodules-3.0") {
        return true;
    }
    command.starts_with(prefix.join("Cellar/gtk+3"))
        && command.ends_with(Path::new("bin/gtk-query-immodules-3.0"))
}
