//! Structured postinstall step execution.
//!
//! Provenance: Rust port of Homebrew's declarative `post_install_steps`,
//! reviewed against Homebrew/brew `7d2a02d22aa174b891f7a631d6ce9aecfe643352`
//! (`7d2a02d2`, 2026-09-09). Inline citations use source files and symbols
//! rather than line numbers, which drift when unrelated Ruby code changes.
//!
//! All 34 serialized step types and the specialized formula actions remain
//! supported. Deprecated DSL aliases still emit supported types; do not drop
//! them. Cask-only arch expansion and privileged-step brokering do not apply
//! to formula jobs (`{{arch}}` stays literal). Sandbox staging protection and
//! the expanded package-manager cache environment follow this revision.
//! Formula network-policy ingestion is deferred: the client enforces supplied
//! metadata but still defaults to allowing network when the registry omits it.
//!
//! Upstream sources of truth:
//!
//! - `Library/Homebrew/install_steps.rb` (DSL + Runner) — the step types,
//!   path/guard resolution, and per-step execution semantics.
//! - `Library/Homebrew/install_steps/formula_actions.rb` — the
//!   `configure_*`/`install_gzipped_executable` actions.
//! - `Library/Homebrew/formula.rb` (`run_post_install`, `run_post_install_steps`),
//!   `formula_installer.rb` (`post_install`), `Library/Homebrew/postinstall.rb` — the
//!   surrounding post-install environment (env, HOME, sandbox).
//! - `Library/Homebrew/utils/inreplace.rb`, `utils/string_inreplace_extension.rb`
//!   — inreplace audit semantics.
//! - `Library/Homebrew/utils/clang.rb` (`write_system_config_files`) —
//!   `configure_clang_system`.
//! - `Library/Homebrew/utils/path.rb` — opt-prefix / installed-formula
//!   checks used by the global tool steps.
//!
//! Parts with no Homebrew counterpart are glu inventions (not upstream
//! behavior), documented in `docs/explanation/install-pipeline.md`: the
//! global-postinstall deferral/coalescing machinery. `search_path` is an
//! upstream glob-search base, not a glu invention. Intentional differences from
//! upstream are flagged inline as compatibility notes.
//!
//! Platform & architecture scope
//!
//! glu v0.1 targets Apple Silicon (arm64) macOS only; Intel (x86_64) macOS
//! and Linux are planned but not implemented. Every place in this file that
//! makes a platform/arch assumption is marked with `// PLATFORM:` and states
//! (a) the current v0.1 assumption, (b) what changes for Intel macOS, (c)
//! what changes for Linux — so extending is a matter of following the
//! markers rather than re-deriving what is missing. Unmarked code is
//! platform-neutral.
//!
//! Conventions used inline:
//! - `// Homebrew 7d2a02d2: install_steps.rb (fn)` — provenance citation.
//! - `// Compatibility: ...` — an intentional divergence, silent no-op, or
//!   platform capability not supported by the current target.
//! - `// PLATFORM: ...` — platform/arch assumption (see section above).

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use glu_core::{Prefix, ResolvedPackage};
use serde_json::Value;
#[cfg(test)]
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{self, Write},
    path::{Component, Path, PathBuf},
};

mod analysis;
mod base_paths;
mod command;
#[path = "env.rs"]
mod postinstall_env;
mod resolve;
mod ruby;
mod steps;
mod types;
mod validate;

#[cfg(test)]
use analysis::analyze_structured_postinstalls;
pub(crate) use command::NoticedPostinstallFailure;
use command::{
    postinstall_env_var_present, postinstall_failure_notice, postinstall_label, run_command,
    PostinstallCommandFailed, RunCommand,
};
use postinstall_env::{PostinstallEnv, PostinstallEnvSnapshot};
#[cfg(test)]
use resolve::base_path;
use resolve::{
    brace_expand, command_path, expand, expand_glob, guards_match, path, EXPAND_BASE_TOKENS,
};
use ruby::{atomic_write, inreplace, inreplace_regexp_file};
use steps::{
    bootstrap_cpython, bootstrap_pypy, change_dylib_id, chmod_mode, chmod_paths, chown_path,
    chown_paths, configure_clang_system, configure_php, copy_entry_contents, copy_path,
    create_relative_symlink, delete_keychain_certificate, global_key, global_kind_key,
    gtk_icon_cache_key, init_data_dir, install_gzipped_executable, is_fontconfig_fc_cache,
    is_gtk_query_immodules_3, link_children_step, link_dir_step, move_path, remove_any,
    remove_step, run_command_step, run_global, run_or_defer_global, single_source, symlink_step,
    terminate_process, version_major, version_major_minor,
};
pub use types::{
    DeferredPostinstallItem, DeferredPostinstallQueue, DeferredPostinstallRequest,
    PostinstallAnalysis, PostinstallPlan, PostinstallPlans, PostinstallStepPlan,
};
pub use validate::validate_structured_postinstall_steps;

// glu invention (no Homebrew concept): Homebrew runs every step per-package.
// Deferral + coalescing is an intentional deviation documented in
// docs/explanation/install-pipeline.md ("Global postinstall deferral").
const DEFERRABLE_GLOBAL_TYPES: &[&str] = &[
    "compile_gsettings_schemas",
    "gio_querymodules",
    "gdk_pixbuf_query_loaders",
    "gtk_update_icon_cache",
    "update_mime_database",
    "update_desktop_database",
];

// glu invention (deferral machinery; called from install/orchestrator.rs
// cache_postinstall for each DAG cache node). See `run_global` for the
// underlying Homebrew commands.
pub fn run_deferred_global_postinstall(
    prefix: &Prefix,
    kind: &str,
    key: &[String],
    verbose: bool,
) -> Result<()> {
    let post_env = PostinstallEnv::new(prefix)?;
    run_global(
        &GlobalPostinstallContext {
            prefix,
            env: &post_env.snapshot,
            verbose,
        },
        kind,
        key,
    )
}

// Homebrew 7d2a02d2: install_steps.rb (Runner#run) — per-step loop is
// `run_install_step`, guard evaluation is `step_guards_match?`.
// This in-process runner expects its caller to provide the process security
// boundary. Normal installs call it from `postinstall::sandbox`'s worker with
// a parent-computed plan, so native Rust filesystem steps and child commands
// both run under the same Homebrew-parity sandbox.
pub fn run_structured_postinstalls_with_plan(
    prefix: &Prefix,
    package: &ResolvedPackage,
    keg: &Path,
    plan: &PostinstallPlan,
    deferred: Option<&mut DeferredPostinstallQueue>,
    verbose: bool,
) -> Result<()> {
    if package.install.post_install_steps.is_empty() {
        return Ok(());
    }
    if plan.per_step.len() != package.install.post_install_steps.len() {
        bail!(
            "postinstall plan for {} has {} steps, expected {}",
            package.name.0,
            plan.per_step.len(),
            package.install.post_install_steps.len()
        );
    }
    let post_env = PostinstallEnv::new(prefix)?;
    let mut ctx = PostinstallContext {
        prefix,
        package,
        keg,
        env: &post_env.snapshot,
        deferred,
        verbose,
        guards: RefCell::new(BTreeMap::new()),
    };
    for index in 0..package.install.post_install_steps.len() {
        let step = &package.install.post_install_steps[index];
        if !guards_match(&ctx, step)? {
            continue;
        }
        run_step(&mut ctx, step, &plan.per_step[index])?;
    }
    Ok(())
}

struct PostinstallContext<'a> {
    prefix: &'a Prefix,
    package: &'a ResolvedPackage,
    keg: &'a Path,
    env: &'a PostinstallEnvSnapshot,
    deferred: Option<&'a mut DeferredPostinstallQueue>,
    verbose: bool,
    /// Homebrew 7d2a02d2: install_steps.rb — guard results memoized
    /// per run (@guard_results), keyed by the guard spec's canonical JSON.
    guards: RefCell<BTreeMap<String, bool>>,
}

struct GlobalPostinstallContext<'a> {
    prefix: &'a Prefix,
    env: &'a PostinstallEnvSnapshot,
    verbose: bool,
}

// Homebrew 7d2a02d2: install_steps.rb (run_install_step case dispatch).
// Each arm below cites its upstream behavior; deviations are flagged inline.
fn run_step(
    ctx: &mut PostinstallContext<'_>,
    step: &Value,
    plan: &PostinstallStepPlan,
) -> Result<()> {
    match run_step_dispatch(ctx, step, plan) {
        Ok(()) => Ok(()),
        Err(err) => {
            // A fatal subprocess failure names only the command in the error
            // chain; surface the captured stderr with a dimmed context line
            // (red — the install fails) before it propagates.
            let noticed = err
                .downcast_ref::<PostinstallCommandFailed>()
                .map(|failed| {
                    postinstall_failure_notice(
                        &postinstall_label(&ctx.package.name.0, &failed.command, &failed.args),
                        &String::from_utf8_lossy(&failed.stderr),
                        true,
                    )
                })
                .unwrap_or(false);
            if noticed {
                // The notice is the cause report; the scheduler must not
                // re-show the chain below the error headline.
                return Err(NoticedPostinstallFailure(err).into());
            }
            Err(err)
        }
    }
}

fn run_step_dispatch(
    ctx: &mut PostinstallContext<'_>,
    step: &Value,
    plan: &PostinstallStepPlan,
) -> Result<()> {
    let typ = step_type(step)?;
    match typ {
        // Homebrew 7d2a02d2: install_steps.rb (`mkdir` Runner
        // `.mkdir` — fails if the path exists; same as `fs::create_dir`).
        "mkdir" => fs::create_dir(path(ctx, req(step, "path")?)?)?,
        // Homebrew 7d2a02d2: install_steps.rb (`.mkpath`).
        "mkdir_p" => fs::create_dir_all(path(ctx, req(step, "path")?)?)?,
        // Homebrew 7d2a02d2: install_steps.rb (run_init_data_dir).
        "init_data_dir" => init_data_dir(ctx, step)?,
        // Homebrew 7d2a02d2: install_steps.rb. Homebrew uses
        // `FileUtils.touch` which updates mtime even for existing files; glu
        // now does the same via `File::set_times` (std 1.75+).
        "touch" => {
            let p = path(ctx, req(step, "path")?)?;
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent)?;
            }
            let file = fs::OpenOptions::new().create(true).append(true).open(&p)?;
            let now = std::time::SystemTime::now();
            file.set_times(
                std::fs::FileTimes::new()
                    .set_accessed(now)
                    .set_modified(now),
            )?;
        }
        // Homebrew 7d2a02d2: install_steps.rb (move case:
        // `FileUtils.mv source, target, force: step["force"] == true`).
        // Dir targets move INTO the directory (see `move_path`).
        "move" => move_path(
            &single_source(ctx, step)?,
            &path(ctx, req(step, "target")?)?,
        )?,
        // Homebrew 7d2a02d2: install_steps.rb (move_children/move_contents:
        // `children = source.children.reject { |child| child == target }` then
        // `FileUtils.mv children, target` — each child moved INTO target).
        "move_children" | "move_contents" => {
            let source = path(ctx, req(step, "source")?)?;
            let target = path(ctx, req(step, "target")?)?;
            fs::create_dir_all(&target)?;
            for child in fs::read_dir(source)? {
                let child = child?.path();
                // Homebrew excludes the target child itself; without this a
                // target inside the source would be renamed into itself.
                if child == target {
                    continue;
                }
                move_path(&child, &target)?;
            }
        }
        // Homebrew 7d2a02d2: install_steps.rb + step_destination
        // helper. Homebrew raises Errno::EEXIST when `!overwrite` and the
        // destination exists; for overwrite, non-recursive copies are
        // `FileUtils.cp` (in-place overwrite, symlink removed first) and
        // recursive copies are `FileUtils.cp_r ... remove_destination:`.
        "copy" => copy_path(
            &single_source(ctx, step)?,
            &path(ctx, req(step, "target")?)?,
            bool_or(step, "recursive", false),
            step.get("overwrite").and_then(Value::as_bool) != Some(false),
        )?,
        "remove" => remove_step(ctx, step)?,
        // Homebrew 7d2a02d2: install_steps.rb + utils/inreplace.rb +
        // utils/string_inreplace_extension.rb (audit). Regexp::EXTENDED
        // free-spacing normalization is handled by `strip_extended`.
        "inreplace" => inreplace(ctx, step)?,
        // Homebrew 7d2a02d2: install_steps.rb (link_dir case). The
        // `symlink_tree` TYPE STRING was retired from the Runner — the DSL
        // method `symlink_tree` (install_steps.rb) now emits `link_dir`.
        // `.DS_Store` skip, existing-dir preservation and relative symlinks
        // implemented in `link_dir_tree`.
        "link_dir" => link_dir_step(ctx, step)?,
        // Homebrew 7d2a02d2: install_steps.rb (link_children: relative
        // symlinks with prefix/suffix via `install_symlink`).
        "link_children" => link_children_step(ctx, step)?,
        // Homebrew 7d2a02d2: install_steps.rb (symlink case + create_symlink).
        "symlink" => symlink_step(ctx, step)?,
        // Homebrew 7d2a02d2: install_steps.rb (write case). Upstream
        // raises ArgumentError only when content is nil — empty strings are
        // allowed since 2026-08 (pre-2026-08 `blank?` also rejected "").
        // NOTE: `overwrite` defaults to false here, but `write_file` — the new
        // canonical DSL method (install_steps.rb) — emits `overwrite: true`
        // and `append_newline: false`.
        "write" => {
            let content = step
                .get("content")
                .and_then(Value::as_str)
                .context("postinstall write step requires content")?
                .to_string();
            let p = path(ctx, req(step, "path")?)?;
            if bool_or(step, "overwrite", false) || !p.exists() {
                if let Some(parent) = p.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(p, expand(ctx, &content))?;
            }
        }
        // Homebrew 7d2a02d2: install_steps.rb (run_serialised_command).
        // `allow_failure` is the JSON inverse of the DSL's `must_succeed`
        // (install_steps.rb `"allow_failure" => !must_succeed`); `sudo` is
        // `step["sudo"] == true` (install_steps.rb) — both now implemented.
        // NOTE: `suppress_stderr` below is the JSON field name the DSL emits
        // (install_steps.rb `"suppress_stderr" => !print_stderr`) — the
        // inversion is upstream's, glu reads it as-is; correct, do not "fix".
        "run" => {
            if let Some((kind, key)) = global_kind_key(ctx, step)? {
                run_or_defer_global(ctx, plan, kind, key)?;
            } else {
                run_command_step(ctx, step)?;
            }
        }
        // Homebrew 7d2a02d2: install_steps.rb (run_terminate_process).
        // Name-match uses `/usr/bin/killall <name>`; full-match uses
        // `/usr/bin/pkill -f <name>`.
        "terminate_process" => terminate_process(ctx, step)?,
        // Homebrew 7d2a02d2: install_steps.rb (InstallSteps.change_dylib_id:
        // ruby-macho `MachO::Tools.change_dylib_id` + `MachO.codesign!` on arm64).
        // glu's in-process rewrite lives in bottle/macho.rs, with ad-hoc
        // signing in bottle/codesign.rs using ruby-macho
        // identifier parity.
        "change_dylib_id" => change_dylib_id(ctx, step)?,
        // Homebrew 7d2a02d2: utils/output.rb (opoo → stderr "Warning:").
        "warn" => crate::worker_output::notice(&crate::style::yellow(&format!(
            "Warning: {}",
            expand(
                ctx,
                step.get("message").and_then(Value::as_str).unwrap_or("")
            )
        ))),
        // Homebrew 7d2a02d2: install_steps/formula_actions.rb
        // (run_configure_gcc_runtime / run_configure_glibc_runtime).
        // PLATFORM (arm64 macOS, v0.1): glu is a silent no-op on both —
        // compliant for `configure_gcc_runtime` by construction (upstream's own
        // guard, formula_actions.rb, no-ops off Linux) but NOT for
        // `configure_glibc_runtime` (no upstream guard; runs localedef).
        // Intel: same as arm64 (still macOS).
        // Compatibility: a future Linux implementation must add both actions
        // (GCC specs file + crti.o symlinks; locale data via localedef +
        // /etc/localtime symlinks). Validation rejects `configure_glibc_runtime`
        // off Linux; the macOS `configure_gcc_runtime` no-op matches upstream's
        // own guard.
        "configure_gcc_runtime" | "configure_glibc_runtime" => {}
        // Homebrew 7d2a02d2: install_steps/formula_actions.rb
        // (run_install_gzipped_executable).
        "install_gzipped_executable" => install_gzipped_executable(ctx, step)?,
        // Homebrew 7d2a02d2: install_steps/formula_actions.rb +
        // utils/clang.rb (write_system_config_files).
        "configure_clang_system" => configure_clang_system(ctx)?,
        // Homebrew 7d2a02d2: install_steps.rb (Runner arms) +
        // formula_actions.rb (run_configure_php, run_bootstrap_cpython,
        // run_bootstrap_pypy(abi_version)). Ported in `configure_php` /
        // `bootstrap_cpython` / `bootstrap_pypy` below; `bootstrap_pypy` requires
        // the `abi_version` field (install_steps.rb).
        "configure_php" => configure_php(ctx)?,
        "bootstrap_cpython" => bootstrap_cpython(ctx)?,
        "bootstrap_pypy" => {
            let abi = step
                .get("abi_version")
                .and_then(Value::as_str)
                .unwrap_or("");
            if abi.is_empty() {
                bail!("{} bootstrap_pypy requires abi_version", ctx.package.name.0);
            }
            bootstrap_pypy(ctx, abi)?;
        }
        // Homebrew 7d2a02d2: install_steps.rb (run_set_permissions).
        // chmod runs without sudo upstream; matches.
        "set_permissions" => chmod_paths(ctx, step)?,
        // Homebrew 7d2a02d2: install_steps.rb (run_set_ownership).
        // See the compatibility note on `chown_paths` /
        // `app_management_permissions_granted` for the remaining App
        // Management heuristic difference.
        "set_ownership" => chown_paths(ctx, step)?,
        // Homebrew 7d2a02d2: install_steps.rb (run_formula_tool) —
        // glu defers/coalesces these via DEFERRABLE_GLOBAL_TYPES (glu invention;
        // Homebrew runs them per-package, per docs/explanation/install-pipeline.md).
        typ if DEFERRABLE_GLOBAL_TYPES.contains(&typ) => {
            let key = global_key(ctx, typ, step)?;
            run_or_defer_global(ctx, plan, typ.to_string(), key)?;
        }
        // Homebrew 7d2a02d2: install_steps.rb (delete_keychain_certificate
        // case: /usr/bin/security find/delete-certificate with sudo, optional
        // openssl fingerprint match). Implemented in `delete_keychain_certificate`.
        // PLATFORM (arm64 macOS, v0.1): macOS-ONLY step (`/usr/bin/security`,
        // `/usr/bin/openssl`); Homebrew has no platform guard, so on Linux it
        // would just fail — the package must be refused in validation (no keychain).
        "delete_keychain_certificate" => delete_keychain_certificate(ctx, step)?,
        _ => bail!("unsupported postinstall step type {typ}"),
    }
    Ok(())
}
// glu helpers (no Homebrew equivalent; step fields arrive JSON-normalised from
// the API, so `as_bool`/`as_str` are the JSON value types).
fn bool_or(step: &Value, key: &str, default: bool) -> bool {
    step.get(key).and_then(Value::as_bool).unwrap_or(default)
}
// glu helper (no Homebrew equivalent).
fn req<'a>(step: &'a Value, key: &str) -> Result<&'a Value> {
    step.get(key)
        .ok_or_else(|| anyhow::anyhow!("postinstall step missing {key}"))
}
// glu helper (no Homebrew equivalent; upstream raises on `unknown install
// step` / missing type in run_install_step's else branch, install_steps.rb).
fn step_type(step: &Value) -> Result<&str> {
    step.get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("postinstall step without type"))
}
// glu invention (no Homebrew equivalent — upstream DSL validates at formula
// parse time). Validate the field shapes that the Rust runner requires so an
// unsupported manifest fails before downloads or prefix mutation.

#[cfg(test)]
mod tests;
