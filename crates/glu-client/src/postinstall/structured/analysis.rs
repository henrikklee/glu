use super::{
    base_paths::{resolve_base_path, runtime_only_base, BasePathContext},
    gtk_icon_cache_key, is_fontconfig_fc_cache, is_gtk_query_immodules_3, req, step_type,
    version_major, version_major_minor, DEFERRABLE_GLOBAL_TYPES, EXPAND_BASE_TOKENS,
};
use super::{PostinstallAnalysis, PostinstallPlan, PostinstallStepPlan};
use anyhow::Result;
use glu_core::{Prefix, ResolvedPackage};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

// glu invention (deferral machinery): shared static analysis used by DAG
// planning and runtime execution. It resolves each step's possible global
// cache action once, then walks the list backwards so suffix deferrability is
// O(n) instead of repeatedly rescanning the remaining steps.
pub(super) fn analyze_structured_postinstalls(
    prefix: &Prefix,
    package: &ResolvedPackage,
    keg: &Path,
) -> Result<PostinstallAnalysis> {
    if package.install.post_install_steps.is_empty() {
        return Ok(PostinstallAnalysis::empty());
    }
    let ctx = PostinstallPlanningContext {
        prefix,
        package,
        keg,
    };
    let steps = &package.install.post_install_steps;
    let mut global = Vec::with_capacity(steps.len());
    for step in steps {
        global.push(planned_global_step_kind_key(&ctx, step)?);
    }

    let mut per_step = vec![PostinstallStepPlan::Inline; steps.len()];
    let mut suffix_all_deferrable = true;
    for index in (0..steps.len()).rev() {
        if let Some((kind, key)) = global[index].clone() {
            if suffix_all_deferrable {
                per_step[index] = PostinstallStepPlan::DeferredGlobal { kind, key };
            }
        }
        suffix_all_deferrable =
            matches!(per_step[index], PostinstallStepPlan::DeferredGlobal { .. });
    }

    let mut deferred_contributions = BTreeMap::new();
    for plan in &per_step {
        if let PostinstallStepPlan::DeferredGlobal { kind, key } = plan {
            deferred_contributions
                .entry((kind.clone(), key.clone()))
                .or_insert_with(BTreeSet::new)
                .insert(package.name.0.clone());
        }
    }

    Ok(PostinstallAnalysis {
        plan: PostinstallPlan { per_step },
        deferred_contributions,
    })
}

struct PostinstallPlanningContext<'a> {
    prefix: &'a Prefix,
    package: &'a ResolvedPackage,
    keg: &'a Path,
}

fn stable_path(ctx: &PostinstallPlanningContext<'_>, spec: &Value) -> Result<Option<PathBuf>> {
    if let Some(s) = spec.as_str() {
        let Some(value) = stable_expand(ctx, s) else {
            return Ok(None);
        };
        return stable_absolute_path(&value);
    }
    let Some(p) = stable_expand(ctx, spec.get("path").and_then(Value::as_str).unwrap_or("")) else {
        return Ok(None);
    };
    let base = spec.get("base").and_then(Value::as_str);
    match base {
        None | Some("absolute") => stable_absolute_path(&p),
        Some(base) if runtime_only_base(base) => Ok(None),
        Some(base) => Ok(
            stable_base_path(ctx, base, spec.get("formula").and_then(Value::as_str))
                .map(|base| base.join(p)),
        ),
    }
}

fn stable_absolute_path(value: &str) -> Result<Option<PathBuf>> {
    if value == "~" || value.starts_with("~/") {
        return Ok(None);
    }
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(Some(path))
    } else {
        Ok(None)
    }
}

fn stable_expand(ctx: &PostinstallPlanningContext<'_>, text: &str) -> Option<String> {
    if text.contains("{{HOMEBREW_BREW_FILE}}") || text.contains("{{user}}") {
        return None;
    }
    let prefix = ctx.prefix.0.to_string_lossy();
    let cellar = ctx.prefix.0.join("Cellar");
    let mut out = text
        .replace("{{HOMEBREW_PREFIX}}", &prefix)
        .replace("{{HOMEBREW_CELLAR}}", &cellar.to_string_lossy())
        .replace("{{formula_name}}", &ctx.package.name.0)
        .replace("{{name}}", &ctx.package.name.0)
        .replace("{{token}}", &ctx.package.name.0)
        .replace("{{version}}", &ctx.package.version)
        .replace(
            "{{version.major}}",
            &version_major(&ctx.package.version).unwrap_or_default(),
        )
        .replace(
            "{{version.major_minor}}",
            &version_major_minor(&ctx.package.version).unwrap_or_default(),
        );

    for base in EXPAND_BASE_TOKENS {
        let placeholder = format!("{{{{{base}}}}}");
        if !out.contains(&placeholder) {
            continue;
        }
        let base_path = stable_base_path(ctx, base, None)?;
        out = out.replace(&placeholder, &base_path.to_string_lossy());
    }
    Some(out)
}

fn stable_base_path(
    ctx: &PostinstallPlanningContext<'_>,
    base: &str,
    formula: Option<&str>,
) -> Option<PathBuf> {
    resolve_base_path(
        &BasePathContext {
            prefix: &ctx.prefix.0,
            keg: ctx.keg,
            package_name: &ctx.package.name.0,
            formula,
            home: None,
            temp: None,
        },
        base,
    )
}

fn planned_global_kind_key(
    ctx: &PostinstallPlanningContext<'_>,
    step: &Value,
) -> Result<Option<(String, Vec<String>)>> {
    let Some(command) = stable_path(ctx, req(step, "command")?)? else {
        return Ok(None);
    };
    let mut args = Vec::new();
    for value in step
        .get("args")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(arg) = stable_expand(ctx, value.as_str().unwrap_or("")) else {
            return Ok(None);
        };
        args.push(arg);
    }
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
            let Some(stdout_path) = stable_path(ctx, stdout_path)? else {
                return Ok(None);
            };
            return Ok(Some((
                "gtk_query_immodules_3".to_string(),
                vec![stdout_path.to_string_lossy().to_string()],
            )));
        }
    }
    Ok(None)
}

// glu invention (deferral machinery): classifies a step as (kind, key) if it
// is a deferrable global type or a matching legacy cache-tool `run` step. Static
// planning only defers keys resolved from stable prefix/keg/package facts;
// runtime-only HOME/TMPDIR/cwd/env-shaped keys are left inline.
fn planned_global_step_kind_key(
    ctx: &PostinstallPlanningContext<'_>,
    step: &Value,
) -> Result<Option<(String, Vec<String>)>> {
    let typ = step_type(step)?;
    if DEFERRABLE_GLOBAL_TYPES.contains(&typ) {
        if typ == "gdk_pixbuf_query_loaders" {
            return Ok(Some((typ.to_string(), vec![])));
        }
        let Some(path) = stable_path(ctx, req(step, "path")?)? else {
            return Ok(None);
        };
        return Ok(Some((
            typ.to_string(),
            vec![path.to_string_lossy().to_string()],
        )));
    }
    if typ == "run" {
        return planned_global_kind_key(ctx, step);
    }
    Ok(None)
}
