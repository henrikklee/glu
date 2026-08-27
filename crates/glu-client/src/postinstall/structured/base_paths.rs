use std::path::{Path, PathBuf};

// Homebrew 4dacfe77: install_steps.rb:1517-1568 (root_path/context_path/formula_base)
// + formula.rb:1067-1550 (Formula path methods). `search_path` is a glu
// invention (no Homebrew base). `staged_path`, `appdir`, `caskroom_path` are
// Cask DSL bases (cask/dsl.rb:683) that Homebrew's formula Runner would reject
// with "unknown install step base"; glu accepts the protocol vocabulary but
// these bases should not appear in formula postinstall steps.
pub(super) const SUPPORTED_BASES: &[&str] = &[
    "absolute",
    "relative",
    "search_path",
    "path",
    "home",
    "temp",
    "homebrew_prefix",
    "prefix",
    "opt_prefix",
    "formula_opt_prefix",
    "bin",
    "sbin",
    "lib",
    "libexec",
    "share",
    "pkgshare",
    "opt_pkgshare",
    "include",
    "frameworks",
    "rack",
    "etc",
    "var",
    "pkgetc",
    "formula_pkgetc",
    "bash_completion",
    "zsh_completion",
    "fish_completion",
    "pwsh_completion",
    "staged_path",
    "appdir",
    "caskroom_path",
];

/// Inputs for resolving a structured postinstall base. Runtime execution passes
/// HOME/TMPDIR from the explicit postinstall environment; static planning leaves
/// them absent, causing those runtime-shaped bases to resolve to `None` and the
/// step to stay inline. The base-to-path table itself lives here so planning and
/// execution cannot drift silently.
pub(super) struct BasePathContext<'a> {
    pub(super) prefix: &'a Path,
    pub(super) keg: &'a Path,
    pub(super) package_name: &'a str,
    pub(super) formula: Option<&'a str>,
    pub(super) home: Option<&'a Path>,
    pub(super) temp: Option<&'a Path>,
}

impl BasePathContext<'_> {
    fn formula_name(&self) -> &str {
        self.formula.unwrap_or(self.package_name)
    }
}

pub(super) fn resolve_base_path(ctx: &BasePathContext<'_>, base: &str) -> Option<PathBuf> {
    let name = ctx.formula_name();
    match base {
        // These bases are handled by the caller, not by root-path resolution.
        // Static planning treats them as runtime-shaped and keeps the step inline.
        "absolute" | "relative" | "path" | "search_path" => None,
        "home" => ctx.home.map(Path::to_path_buf),
        "temp" => ctx.temp.map(Path::to_path_buf),
        "homebrew_prefix" => Some(ctx.prefix.to_path_buf()),
        "prefix" | "staged_path" => Some(ctx.keg.to_path_buf()),
        "opt_prefix" | "formula_opt_prefix" => Some(ctx.prefix.join("opt").join(name)),
        "bin" => Some(ctx.keg.join("bin")),
        "sbin" => Some(ctx.keg.join("sbin")),
        "lib" => Some(ctx.keg.join("lib")),
        "libexec" => Some(ctx.keg.join("libexec")),
        "share" => Some(ctx.keg.join("share")),
        "pkgshare" => Some(ctx.keg.join("share").join(ctx.package_name)),
        "opt_pkgshare" => Some(ctx.prefix.join("opt").join(name).join("share").join(name)),
        "include" => Some(ctx.keg.join("include")),
        "frameworks" => Some(ctx.keg.join("Frameworks")),
        "rack" => Some(ctx.prefix.join("Cellar").join(name)),
        "etc" => Some(ctx.prefix.join("etc")),
        "var" => Some(ctx.prefix.join("var")),
        "pkgetc" => Some(ctx.prefix.join("etc").join(ctx.package_name)),
        "formula_pkgetc" => Some(ctx.prefix.join("etc").join(name)),
        // Homebrew 4dacfe77: formula.rb:1401-1428 — these resolve KEG-relative
        // upstream (`prefix/etc/bash_completion.d`, `share/zsh/site-functions`,
        // `share/fish/vendor_completions.d`, `share/pwsh/completions` — formula
        // `prefix` is the keg during postinstall).
        "bash_completion" => Some(ctx.keg.join("etc/bash_completion.d")),
        "zsh_completion" => Some(ctx.keg.join("share/zsh/site-functions")),
        "fish_completion" => Some(ctx.keg.join("share/fish/vendor_completions.d")),
        "pwsh_completion" => Some(ctx.keg.join("share/pwsh/completions")),
        "appdir" => Some(ctx.prefix.join("Applications")),
        // PLATFORM (arm64 macOS, v0.1): `Applications`/`Caskroom` are macOS
        // prefix conventions. Intel: unchanged. Linux: these are Cask-only
        // bases Homebrew's formula Runner rejects anyway (noted above) — keep,
        // but they should never resolve for formulae on any platform.
        "caskroom_path" => Some(ctx.prefix.join("Caskroom")),
        _ => Some(ctx.prefix.to_path_buf()),
    }
}

pub(super) fn runtime_only_base(base: &str) -> bool {
    matches!(base, "relative" | "path" | "search_path" | "home" | "temp")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context<'a>(
        prefix: &'a Path,
        keg: &'a Path,
        home: Option<&'a Path>,
        temp: Option<&'a Path>,
    ) -> BasePathContext<'a> {
        BasePathContext {
            prefix,
            keg,
            package_name: "fixture",
            formula: None,
            home,
            temp,
        }
    }

    #[test]
    fn planning_and_runtime_share_stable_base_resolution() {
        let prefix = Path::new("/opt/glu");
        let keg = Path::new("/opt/glu/Cellar/fixture/1.0");
        let home = Path::new("/private/tmp/glu-home");
        let temp = Path::new("/private/tmp/glu-temp");
        let runtime = context(prefix, keg, Some(home), Some(temp));
        let planning = context(prefix, keg, None, None);

        for base in SUPPORTED_BASES {
            if *base == "absolute" || runtime_only_base(base) {
                continue;
            }
            assert_eq!(
                resolve_base_path(&planning, base),
                resolve_base_path(&runtime, base),
                "stable base {base} must resolve identically during planning and runtime"
            );
        }
    }

    #[test]
    fn runtime_only_bases_are_not_planned_as_stable_paths() {
        let prefix = Path::new("/opt/glu");
        let keg = Path::new("/opt/glu/Cellar/fixture/1.0");
        let home = Path::new("/private/tmp/glu-home");
        let temp = Path::new("/private/tmp/glu-temp");
        let runtime = context(prefix, keg, Some(home), Some(temp));
        let planning = context(prefix, keg, None, None);

        assert_eq!(
            resolve_base_path(&runtime, "home"),
            Some(home.to_path_buf())
        );
        assert_eq!(
            resolve_base_path(&runtime, "temp"),
            Some(temp.to_path_buf())
        );
        for base in ["relative", "path", "search_path", "home", "temp"] {
            assert_eq!(resolve_base_path(&planning, base), None);
        }
    }
}
