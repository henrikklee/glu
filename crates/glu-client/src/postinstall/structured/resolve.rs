use super::{base_paths::resolve_base_path, base_paths::BasePathContext, *};

// Homebrew 4dacfe77: install_steps.rb:1482-1492 (resolve_path) — blank/
// `absolute` bases call `Pathname#expand_path`, while `relative` stays raw and
// all other bases join against `root_path`.
pub(super) fn path(ctx: &PostinstallContext<'_>, spec: &Value) -> Result<PathBuf> {
    if let Some(s) = spec.as_str() {
        return expand_absolute_path(ctx, &expand(ctx, s));
    }
    let p = expand(ctx, spec.get("path").and_then(Value::as_str).unwrap_or(""));
    let base = spec.get("base").and_then(Value::as_str);
    match base {
        None | Some("absolute") => expand_absolute_path(ctx, &p),
        Some("relative") => Ok(PathBuf::from(p)),
        Some(base) => Ok(base_path(ctx, base, spec.get("formula").and_then(Value::as_str)).join(p)),
    }
}

pub(super) fn expand_absolute_path(ctx: &PostinstallContext<'_>, value: &str) -> Result<PathBuf> {
    let path = if value == "~" {
        ctx.env.home.clone()
    } else if let Some(rest) = value.strip_prefix("~/") {
        ctx.env.home.join(rest)
    } else {
        PathBuf::from(value)
    };
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

// Homebrew 4dacfe77: install_steps.rb:1493-1499 (resolve_command).
// Upstream leaves blank/`relative` commands as raw strings and routes
// everything else through `resolve_path`. A `path`/`search_path` command base
// would RAISE "unknown install step base" upstream; glu rejects those bases in
// validation before this resolver can be reached.
pub(super) fn command_path(ctx: &PostinstallContext<'_>, spec: &Value) -> Result<PathBuf> {
    if let Some(s) = spec.as_str() {
        return Ok(PathBuf::from(expand(ctx, s)));
    }
    let p = expand(ctx, spec.get("path").and_then(Value::as_str).unwrap_or(""));
    let base = spec.get("base").and_then(Value::as_str);
    if base.is_none() || base == Some("relative") {
        return Ok(PathBuf::from(p));
    }
    path(ctx, spec)
}

// Homebrew 4dacfe77: install_steps.rb:1426-1444 (expand_path_glob).
// `path` is normalized to `search_path` upstream; `search_path` is glu's
// protocol name for PATH lookup. Upstream only invokes globbing when the final
// candidate string contains one of `[?*[{]`; no-glob PATH candidates are
// returned unconditionally and later consumers decide whether existence matters.
pub(super) fn expand_glob(ctx: &PostinstallContext<'_>, spec: &Value) -> Result<Vec<PathBuf>> {
    let base = spec.get("base").and_then(Value::as_str);
    if matches!(base, Some("path") | Some("search_path")) {
        let pattern = expand(ctx, spec.get("path").and_then(Value::as_str).unwrap_or(""));
        let mut matches = Vec::new();
        for dir in env::split_paths(&ctx.env.path) {
            let candidate = dir.join(&pattern);
            let candidate_string = candidate.to_string_lossy();
            if has_glob_chars(&candidate_string) {
                for expanded in brace_expand(&candidate_string) {
                    matches.extend(glob::glob(&expanded)?.filter_map(Result::ok));
                }
            } else {
                matches.push(candidate);
            }
        }
        return Ok(matches);
    }
    let p = path(ctx, spec)?;
    let pattern = p.to_string_lossy();
    if !has_glob_chars(&pattern) {
        return Ok(vec![p]);
    }
    let mut matches = Vec::new();
    for expanded in brace_expand(&pattern) {
        matches.extend(glob::glob(&expanded)?.filter_map(Result::ok));
    }
    Ok(matches)
}

pub(super) fn has_glob_chars(pattern: &str) -> bool {
    pattern.contains('?') || pattern.contains('*') || pattern.contains('[') || pattern.contains('{')
}

// glu invention (no Homebrew counterpart: Ruby's Dir.glob has native brace
// expansion; the Rust `glob` crate does not, so glu pre-expands braces before
// handing each concrete pattern to `glob`). Mirrors Ruby's useful shape for
// postinstall steps: nested alternates, `{one}` debracing, escaped braces left
// alone for the glob engine, unmatched `{` producing no patterns, and unmatched
// `}` remaining literal.
pub(super) fn brace_expand(pattern: &str) -> Vec<String> {
    let Some((start, end)) = first_brace_pair(pattern) else {
        return if has_unmatched_open_brace(pattern) {
            Vec::new()
        } else {
            vec![pattern.to_string()]
        };
    };
    let arms = split_brace_arms(&pattern[start + 1..end]);
    let mut out = Vec::new();
    for arm in arms {
        let expanded = format!("{}{}{}", &pattern[..start], arm, &pattern[end + 1..]);
        out.extend(brace_expand(&expanded));
    }
    out
}

pub(super) fn first_brace_pair(pattern: &str) -> Option<(usize, usize)> {
    let mut escaped = false;
    let mut start = None;
    let mut depth = 0usize;
    for (idx, ch) in pattern.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '{' {
            if start.is_none() {
                start = Some(idx);
            }
            depth += 1;
        } else if ch == '}' && depth > 0 {
            depth -= 1;
            if depth == 0 {
                return Some((start.expect("brace start set"), idx));
            }
        }
    }
    None
}

pub(super) fn has_unmatched_open_brace(pattern: &str) -> bool {
    let mut escaped = false;
    let mut depth = 0usize;
    for ch in pattern.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '{' {
            depth += 1;
        } else if ch == '}' && depth > 0 {
            depth -= 1;
        }
    }
    depth > 0
}

pub(super) fn split_brace_arms(inner: &str) -> Vec<&str> {
    let mut arms = Vec::new();
    let mut escaped = false;
    let mut depth = 0usize;
    let mut arm_start = 0usize;
    for (idx, ch) in inner.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        match ch {
            '{' => depth += 1,
            '}' if depth > 0 => depth -= 1,
            ',' if depth == 0 => {
                arms.push(&inner[arm_start..idx]);
                arm_start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    arms.push(&inner[arm_start..]);
    arms
}

// Homebrew 4dacfe77: install_steps.rb:1517-1533 (root_path) + 1535-1543
// (context_path) + 1544-1555 (formula_base), backed by Formula path methods
// (formula.rb:1067 rack, 1164 libexec, 1314 pkgshare, 1338 frameworks, 1357
// etc, 1366 pkgetc, 1374 var, 1401 bash_completion, 1410 zsh_completion, 1419
// fish_completion, 1428 pwsh_completion, 1508 opt_prefix, 1550 opt_pkgshare).
// Provenance notes: `bash_completion`/`zsh_completion`/`fish_completion`/
// `pwsh_completion` resolve keg-relative, matching formula.rb:1401-1428.
// `staged_path`/`appdir`/`caskroom_path` are Cask DSL bases (cask/dsl.rb:683)
// that Homebrew's formula Runner rejects (install_steps.rb:1538); glu keeps
// them only because the protocol declares the accepted base vocabulary.
// `search_path` is handled in `expand_glob`; `home`/`temp` use glu's fresh
// per-context postinstall env.
pub(super) fn base_path(
    ctx: &PostinstallContext<'_>,
    base: &str,
    formula: Option<&str>,
) -> PathBuf {
    resolve_base_path(
        &BasePathContext {
            prefix: &ctx.prefix.0,
            keg: ctx.keg,
            package_name: &ctx.package.name.0,
            formula,
            home: Some(&ctx.env.home),
            temp: Some(&ctx.env.temp),
        },
        base,
    )
    .unwrap_or_else(|| ctx.prefix.0.clone())
}

/// Every path token `expand` substitutes in free-form script text (run-step
/// args/content), as opposed to `path`/`command_path`, which look up a single
/// named base from a structured `{base, path}` spec. Resolution uses the shared
/// base table in `base_paths.rs`; a token missing here silently passes through
/// as a literal `{{...}}` string into whatever command consumes it (this is what
/// broke on `{{pkgshare}}`).
// Homebrew 4dacfe77: install_steps.rb:876-890 (CONTENT_PATH_TOKENS) — this list
// must match it exactly: the tokens `expand` substitutes in free-form
// args/content. `{{user}}`, `{{version.major}}`, `{{version.major_minor}}` and
// `{{HOMEBREW_BREW_FILE}}` are handled directly in `expand` (they are not path
// tokens). The 7 tokens upstream does NOT substitute in content (`home`,
// `homebrew_prefix`, `formula_opt_prefix`, `opt_pkgshare`, `include`,
// `frameworks`, `formula_pkgetc`) are intentionally absent here so they pass
// through verbatim exactly like Homebrew.
pub(super) const EXPAND_BASE_TOKENS: &[&str] = &[
    "temp",
    "prefix",
    "opt_prefix",
    "bin",
    "sbin",
    "lib",
    "libexec",
    "share",
    "pkgshare",
    "var",
    "etc",
    "pkgetc",
    "staged_path",
    "appdir",
    "caskroom_path",
    "rack",
    "bash_completion",
    "zsh_completion",
    "fish_completion",
    "pwsh_completion",
];

// Homebrew 4dacfe77: install_steps.rb:1358-1402 (expand_template_tokens /
// template_token_value). Unknown tokens pass through verbatim in both
// implementations. `user` → $USER (upstream ENV.fetch("USER"), raises if
// unset — glu passes empty); `HOMEBREW_BREW_FILE` → the glu binary itself
// (upstream: the brew executable; closest analog — a formula invoking it would
// get glu, flagged for review). HOME/TMPDIR/PATH-backed path bases resolve
// through the per-context postinstall env.
pub(super) fn expand(ctx: &PostinstallContext<'_>, text: &str) -> String {
    let prefix = ctx.prefix.0.to_string_lossy();
    let cellar = ctx.prefix.0.join("Cellar");
    let brew_file = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let user = ctx.env.user.clone();
    let mut out = text
        .replace("{{HOMEBREW_BREW_FILE}}", &brew_file)
        .replace("{{HOMEBREW_PREFIX}}", &prefix)
        .replace("{{HOMEBREW_CELLAR}}", &cellar.to_string_lossy())
        .replace("{{formula_name}}", &ctx.package.name.0)
        .replace("{{name}}", &ctx.package.name.0)
        .replace("{{token}}", &ctx.package.name.0)
        .replace("{{user}}", &user)
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
        out = out.replace(&placeholder, &base_path(ctx, base, None).to_string_lossy());
    }
    out
}

// PLATFORM: `on` guards resolve against the COMPILE-TIME target; v0.1 builds
// are macOS (arm64 or Intel — same result). Intel: unchanged (still macOS).
// Linux: works via `target_os = "linux"`, but note Homebrew also honours
// `--simulate-macos`/`--simulate-linux` (SimulateSystem, install_steps.rb:1188-1189)
// which glu has no equivalent for.
// Homebrew 4dacfe77: install_steps.rb:1178-1198 (step_guards_match? /
// guard_matches?) + 1445-1448 (path_spec_exists?). Guard results are memoized
// per run (@guard_results) keyed by the guard spec's canonical JSON; a DANGLING
// SYMLINK does not satisfy `if_exists` upstream (Pathname#exist? follows the
// link). The `on` guard matches upstream modulo `SimulateSystem`
// (`--simulate-macos`/`--simulate-linux`), which glu has no equivalent for
// (compile-time target only).
pub(super) fn guards_match(ctx: &PostinstallContext<'_>, step: &Value) -> Result<bool> {
    for guard in step
        .get("guards")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let key = serde_json::to_string(guard).unwrap_or_default();
        let cached = { ctx.guards.borrow().get(&key).copied() };
        if let Some(matches) = cached {
            if !matches {
                return Ok(false);
            }
            continue;
        }
        let condition = guard.get("condition").and_then(Value::as_str).unwrap_or("");
        let matches = match condition {
            "if_exists" => expand_glob(ctx, guard)?.iter().any(|p| p.exists()),
            "unless_exists" => !expand_glob(ctx, guard)?.iter().any(|p| p.exists()),
            "on" => {
                let value = guard.get("value").and_then(Value::as_str).unwrap_or("");
                (value == "macos" && cfg!(target_os = "macos"))
                    || (value == "linux" && cfg!(target_os = "linux"))
            }
            _ => bail!("unsupported postinstall guard {condition}"),
        };
        ctx.guards.borrow_mut().insert(key, matches);
        if !matches {
            return Ok(false);
        }
    }
    Ok(true)
}
