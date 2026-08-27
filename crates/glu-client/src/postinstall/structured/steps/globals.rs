use super::super::*;

pub(in crate::postinstall::structured) trait HasPrefix {
    fn prefix(&self) -> &Prefix;
    fn postinstall_env(&self) -> &PostinstallEnvSnapshot;
    /// `--verbose`: surface incidental success output (Homebrew streams it).
    fn verbose(&self) -> bool;
}

impl HasPrefix for PostinstallContext<'_> {
    fn prefix(&self) -> &Prefix {
        self.prefix
    }

    fn postinstall_env(&self) -> &PostinstallEnvSnapshot {
        self.env
    }

    fn verbose(&self) -> bool {
        self.verbose
    }
}

impl HasPrefix for GlobalPostinstallContext<'_> {
    fn prefix(&self) -> &Prefix {
        self.prefix
    }

    fn postinstall_env(&self) -> &PostinstallEnvSnapshot {
        self.env
    }

    fn verbose(&self) -> bool {
        self.verbose
    }
}

// Homebrew 4dacfe77: install_steps.rb:1507-1516 (run_formula_tool: resolves
// `Utils::Path.formula_opt_bin(formula)/executable` and raises ArgumentError
// unless it is executable). The per-type commands match upstream's
// run_install_step cases for compile_gsettings_schemas / gio_querymodules /
// gdk_pixbuf_query_loaders / update_mime_database / update_desktop_database
// (install_steps.rb:1115-1133). Deferral/coalescing of these is a glu invention
// (see DEFERRABLE_GLOBAL_TYPES). `fontconfig_fc_cache` is glu-invented: it
// intercepts formula `run` steps of fc-cache (see `global_kind_key`).
pub(in crate::postinstall::structured) fn run_global(
    ctx: &impl HasPrefix,
    kind: &str,
    key: &[String],
) -> Result<()> {
    match kind {
        "compile_gsettings_schemas" => run_tool(ctx, "glib", "glib-compile-schemas", key),
        "gio_querymodules" => run_tool(ctx, "glib", "gio-querymodules", key),
        "gdk_pixbuf_query_loaders" => run_tool(
            ctx,
            "gdk-pixbuf",
            "gdk-pixbuf-query-loaders",
            &["--update-cache".into()],
        ),
        "gtk_update_icon_cache" => {
            // Homebrew 4dacfe77: install_steps.rb:1121-1129 — gtk4 chosen via
            // `Utils::Path.formula_any_version_installed?("gtk4")` (utils/path.rb:128,
            // any installed keg). glu: any keg dir under Cellar/gtk4 (glu kegs are
            // always installed); installed-but-unlinked gtk4 picks gtk4 and then
            // fails on the missing opt binary, same as upstream.
            let gtk4_installed = ctx
                .prefix()
                .0
                .join("Cellar/gtk4")
                .read_dir()
                .map(|entries| entries.flatten().any(|e| e.path().is_dir()))
                .unwrap_or(false);
            let helper = if gtk4_installed {
                ("gtk4", "gtk4-update-icon-cache")
            } else {
                ("gtk+3", "gtk3-update-icon-cache")
            };
            run_tool(
                ctx,
                helper.0,
                helper.1,
                &["-q".into(), "-t".into(), "-f".into(), key[0].clone()],
            )
        }
        "update_mime_database" => run_tool(ctx, "shared-mime-info", "update-mime-database", key),
        "update_desktop_database" => {
            run_tool(ctx, "desktop-file-utils", "update-desktop-database", key)
        }
        "fontconfig_fc_cache" => run_tool(
            ctx,
            "fontconfig",
            "fc-cache",
            &[
                "--force".into(),
                "--really-force".into(),
                "--verbose".into(),
            ],
        ),
        "gtk_query_immodules_3" => run_tool_stdout_to_path(
            ctx,
            "gtk+3",
            "gtk-query-immodules-3.0",
            &[],
            Path::new(&key[0]),
        ),
        _ => bail!("unsupported global postinstall {kind}"),
    }
}

fn run_tool_stdout_to_path(
    ctx: &impl HasPrefix,
    formula: &str,
    executable: &str,
    args: &[String],
    stdout_path: &Path,
) -> Result<()> {
    let exe = ctx
        .prefix()
        .0
        .join("opt")
        .join(formula)
        .join("bin")
        .join(executable);
    let is_executable = fs::metadata(&exe)
        .map(|m| {
            m.is_file() && {
                #[cfg(unix)]
                {
                    m.permissions().mode() & 0o111 != 0
                }
                #[cfg(not(unix))]
                {
                    true
                }
            }
        })
        .unwrap_or(false);
    if !is_executable {
        bail!(
            "{formula} is missing required executable: {}",
            exe.display()
        );
    }
    let output = match run_command(RunCommand::new(Some(ctx.postinstall_env()), &exe, args)) {
        Ok(output) => output,
        Err(err) => {
            let noticed = err
                .downcast_ref::<PostinstallCommandFailed>()
                .map(|failed| {
                    postinstall_failure_notice(
                        &postinstall_label(formula, &failed.command, &failed.args),
                        &String::from_utf8_lossy(&failed.stderr),
                        true,
                    )
                })
                .unwrap_or(false);
            if noticed {
                return Err(NoticedPostinstallFailure(err).into());
            }
            return Err(err);
        }
    };
    if ctx.verbose() {
        postinstall_failure_notice(
            &postinstall_label(formula, &exe, args),
            &String::from_utf8_lossy(&output.stderr),
            false,
        );
    }
    if let Some(parent) = stdout_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(stdout_path, &output.stdout)?;
    Ok(())
}

// Homebrew 4dacfe77: install_steps.rb:1507-1516 (run_formula_tool) — upstream
// resolves `Utils::Path.formula_opt_bin(formula)/executable` and raises
// ArgumentError unless it is executable. glu uses the same path and bails with
// a clear error instead of a raw spawn failure.
fn run_tool(ctx: &impl HasPrefix, formula: &str, executable: &str, args: &[String]) -> Result<()> {
    let exe = ctx
        .prefix()
        .0
        .join("opt")
        .join(formula)
        .join("bin")
        .join(executable);
    let is_executable = fs::metadata(&exe)
        .map(|m| {
            m.is_file() && {
                #[cfg(unix)]
                {
                    m.permissions().mode() & 0o111 != 0
                }
                #[cfg(not(unix))]
                {
                    true
                }
            }
        })
        .unwrap_or(false);
    if !is_executable {
        bail!(
            "{formula} is missing required executable: {}",
            exe.display()
        );
    }
    match run_command(RunCommand::new(Some(ctx.postinstall_env()), &exe, args)) {
        Ok(output) => {
            // Homebrew's run_formula_tool streams stderr; verbose logs it.
            // Stdout stays discarded (upstream print_stdout defaults false).
            if ctx.verbose() {
                postinstall_failure_notice(
                    &postinstall_label(formula, &exe, args),
                    &String::from_utf8_lossy(&output.stderr),
                    false,
                );
            }
            Ok(())
        }
        Err(err) => {
            let noticed = err
                .downcast_ref::<PostinstallCommandFailed>()
                .map(|failed| {
                    postinstall_failure_notice(
                        &postinstall_label(formula, &failed.command, &failed.args),
                        &String::from_utf8_lossy(&failed.stderr),
                        true,
                    )
                })
                .unwrap_or(false);
            if noticed {
                // The notice is the cause report; the marker also stops an
                // enclosing run_step catch (an inline, non-deferred global)
                // from emitting the notice twice.
                return Err(NoticedPostinstallFailure(err).into());
            }
            Err(err)
        }
    }
}

// Homebrew 4dacfe77: install_steps.rb:812-814 (add_rebuild_action: path with
// base :homebrew_prefix) — the coalescing key matches upstream's rebuild paths
// (share/glib-2.0/schemas, lib/gio/modules, share/icons/hicolor, share/mime,
// share/applications). `gdk_pixbuf_query_loaders` takes no path upstream
// (install_steps.rb:566-570); matches.
pub(in crate::postinstall::structured) fn global_key(
    ctx: &PostinstallContext<'_>,
    typ: &str,
    step: &Value,
) -> Result<Vec<String>> {
    if typ == "gdk_pixbuf_query_loaders" {
        return Ok(vec![]);
    }
    Ok(vec![path(ctx, req(step, "path")?)?
        .to_string_lossy()
        .to_string()])
}

// glu invention (deferral machinery; see DEFERRABLE_GLOBAL_TYPES). Homebrew
// runs these steps immediately per-package — deferral is the spec-documented
// deviation (docs/explanation/install-pipeline.md "Global postinstall deferral").
pub(in crate::postinstall::structured) fn run_or_defer_global(
    ctx: &mut PostinstallContext<'_>,
    plan: &PostinstallStepPlan,
    kind: String,
    key: Vec<String>,
) -> Result<()> {
    if matches!(plan, PostinstallStepPlan::DeferredGlobal { kind: planned_kind, key: planned_key } if planned_kind == &kind && planned_key == &key)
    {
        if let Some(queue) = ctx.deferred.as_deref_mut() {
            queue.defer(
                kind,
                key,
                ctx.package.name.0.clone(),
                ctx.package.install.postinstall_network_access_allowed,
            );
            return Ok(());
        }
    }
    run_global(ctx, &kind, &key)
}
