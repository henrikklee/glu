use crate::path_component::is_safe_path_component;
use anyhow::{bail, Result};
use serde_json::Value;

use super::{base_paths::SUPPORTED_BASES, step_type};

// Homebrew 4dacfe77: install_steps.rb:230-786 (DSL method names) +
// :1105-1109 (Runner arms for the 2026-08 additions). This list is exactly
// Homebrew's supported step types — no more, no less. Every type the DSL can
// emit (31 in the DSL + the 3 added 2026-08: `configure_php`, `bootstrap_cpython`,
// `bootstrap_pypy` at :776-786) plus NOTHING else: the `symlink_tree` type
// string was retired from the Runner (install_steps.rb:1037 is now `when
// "link_dir"` alone; the DSL method `symlink_tree` at :449 emits `link_dir`),
// so glu rejects it too. No Homebrew type may be dropped and no glu-invented
// type may be added; any future change here must be cross-checked against
// install_steps.rb first.
const SUPPORTED_TYPES: &[&str] = &[
    "mkdir",
    "mkdir_p",
    "init_data_dir",
    "touch",
    "move",
    "move_children",
    "move_contents",
    "copy",
    "remove",
    "inreplace",
    "link_dir",
    "link_children",
    "symlink",
    "write",
    "run",
    "terminate_process",
    "change_dylib_id",
    "warn",
    "configure_gcc_runtime",
    "install_gzipped_executable",
    "configure_glibc_runtime",
    "configure_clang_system",
    // Homebrew 4dacfe77: install_steps.rb:776-786 — added 2026-08 (PHP
    // configuration, CPython bootstrap, PyPy bootstrap; implementations in
    // formula_actions.rb:141/200/280). These are the structured successors of
    // the removed `post_install` blocks. NOTE: `bootstrap_pypy` carries an
    // `abi_version` field in the JSON.
    "configure_php",
    "bootstrap_cpython",
    "bootstrap_pypy",
    "set_permissions",
    "set_ownership",
    "compile_gsettings_schemas",
    "gio_querymodules",
    "gdk_pixbuf_query_loaders",
    "gtk_update_icon_cache",
    "update_mime_database",
    "update_desktop_database",
    "delete_keychain_certificate",
];

// glu invention (no Homebrew equivalent): Homebrew's DSL validates step shapes
// at formula parse time (install_steps.rb DSL methods), and the JSON API emits
// only steps the DSL could produce. Per docs/explanation/install-pipeline.md this
// must run before any filesystem mutation.
// PLATFORM (arm64 macOS, v0.1): platform-aware refusal hooks live here.
// `configure_glibc_runtime` is rejected off Linux because upstream has no macOS
// guard and the action requires Linux localedef behavior. `configure_gcc_runtime`
// remains allowed because upstream itself no-ops off Linux.
pub fn validate_structured_postinstall_steps(package_id: &str, steps: &[Value]) -> Result<()> {
    for step in steps {
        let typ = step_type(step)?;
        if !SUPPORTED_TYPES.contains(&typ) {
            bail!("{package_id} uses unsupported postinstall step type {typ}");
        }
        for guard in step
            .get("guards")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let condition = guard.get("condition").and_then(Value::as_str).unwrap_or("");
            if !matches!(condition, "if_exists" | "unless_exists" | "on") {
                bail!("{package_id} uses unsupported postinstall guard {condition}");
            }
        }
        validate_step_shape(package_id, typ, step)?;
        validate_path_specs(package_id, typ, step)?;
    }
    Ok(())
}

fn validate_step_shape(package_id: &str, typ: &str, step: &Value) -> Result<()> {
    if !step.is_object() {
        bail!("{package_id} postinstall step {typ} must be an object");
    }
    match typ {
        "mkdir" | "mkdir_p" | "touch" => require_key(package_id, typ, step, "path")?,
        "init_data_dir" => {
            require_key(package_id, typ, step, "path")?;
            require_non_empty_string(package_id, typ, step, "using")?;
        }
        "move" | "move_children" | "move_contents" | "copy" => {
            require_key(package_id, typ, step, "source")?;
            require_key(package_id, typ, step, "target")?;
        }
        "remove" => require_array(package_id, typ, step, "paths")?,
        "inreplace" => {
            require_key(package_id, typ, step, "path")?;
            require_string(package_id, typ, step, "before")?;
            require_string(package_id, typ, step, "after")?;
        }
        "link_dir" | "link_children" | "symlink" => {
            require_key(package_id, typ, step, "source")?;
            require_key(package_id, typ, step, "target")?;
        }
        "write" => {
            require_key(package_id, typ, step, "path")?;
            require_string(package_id, typ, step, "content")?;
        }
        "run" => validate_command_spec(package_id, typ, step, "command")?,
        "terminate_process" => require_string(package_id, typ, step, "name")?,
        "change_dylib_id" => {
            require_key(package_id, typ, step, "source")?;
            require_non_empty_string(package_id, typ, step, "id")?;
        }
        "warn" => require_string(package_id, typ, step, "message")?,
        "configure_gcc_runtime" => {}
        "configure_glibc_runtime" => {
            if cfg!(not(target_os = "linux")) {
                bail!("{package_id} uses linux-only postinstall step {typ}");
            }
        }
        "configure_clang_system" | "configure_php" | "bootstrap_cpython" => {}
        "bootstrap_pypy" => {
            require_non_empty_string(package_id, typ, step, "abi_version")?;
            let abi_version = step
                .get("abi_version")
                .and_then(Value::as_str)
                .expect("validated abi_version");
            if !is_safe_path_component(abi_version) {
                bail!("{package_id} postinstall step {typ} has unsafe abi_version {abi_version:?}");
            }
        }
        "install_gzipped_executable" => {
            require_key(package_id, typ, step, "source")?;
            require_key(package_id, typ, step, "target")?;
        }
        "set_permissions" => {
            require_array(package_id, typ, step, "paths")?;
            if !matches!(
                step.get("permissions"),
                Some(Value::String(_)) | Some(Value::Number(_))
            ) {
                bail!("{package_id} postinstall step {typ} requires permissions");
            }
        }
        "set_ownership" => require_array(package_id, typ, step, "paths")?,
        "compile_gsettings_schemas"
        | "gio_querymodules"
        | "gtk_update_icon_cache"
        | "update_mime_database"
        | "update_desktop_database" => require_key(package_id, typ, step, "path")?,
        "gdk_pixbuf_query_loaders" => {}
        "delete_keychain_certificate" => {
            if cfg!(not(target_os = "macos")) {
                bail!("{package_id} uses macOS-only postinstall step {typ}");
            }
            require_non_empty_string(package_id, typ, step, "name")?;
        }
        _ => bail!("unsupported postinstall step type {typ}"),
    }
    Ok(())
}

fn require_key(package_id: &str, typ: &str, step: &Value, key: &str) -> Result<()> {
    if step.get(key).is_none() {
        bail!("{package_id} postinstall step {typ} missing {key}");
    }
    Ok(())
}

fn require_array(package_id: &str, typ: &str, step: &Value, key: &str) -> Result<()> {
    if !matches!(step.get(key), Some(Value::Array(_))) {
        bail!("{package_id} postinstall step {typ} requires array {key}");
    }
    Ok(())
}

fn require_string(package_id: &str, typ: &str, step: &Value, key: &str) -> Result<()> {
    if !matches!(step.get(key), Some(Value::String(_))) {
        bail!("{package_id} postinstall step {typ} requires string {key}");
    }
    Ok(())
}

fn require_non_empty_string(package_id: &str, typ: &str, step: &Value, key: &str) -> Result<()> {
    match step.get(key).and_then(Value::as_str) {
        Some(value) if !value.is_empty() => Ok(()),
        _ => bail!("{package_id} postinstall step {typ} requires non-empty string {key}"),
    }
}

fn validate_command_spec(package_id: &str, typ: &str, step: &Value, key: &str) -> Result<()> {
    let spec = step
        .get(key)
        .ok_or_else(|| anyhow::anyhow!("{package_id} postinstall step {typ} missing {key}"))?;
    if let Some(obj) = spec.as_object() {
        if !matches!(obj.get("path"), Some(Value::String(_))) {
            bail!("{package_id} postinstall step {typ} command requires string path");
        }
        if matches!(
            obj.get("base").and_then(Value::as_str),
            Some("path" | "search_path")
        ) {
            bail!("{package_id} postinstall step {typ} command uses unsupported PATH lookup base");
        }
    } else if !spec.is_string() {
        bail!("{package_id} postinstall step {typ} command must be a path spec");
    }
    Ok(())
}

// glu invention (no Homebrew equivalent — upstream DSL validates at formula
// parse time). Walks every nested `{base,...}` path spec against
// SUPPORTED_BASES before any filesystem mutation.
fn validate_path_specs(package_id: &str, typ: &str, value: &Value) -> Result<()> {
    if let Some(obj) = value.as_object() {
        if let Some(base) = obj.get("base").and_then(Value::as_str) {
            if !SUPPORTED_BASES.contains(&base) {
                bail!("{package_id} uses unsupported base {base} in {typ}");
            }
        }
        for v in obj.values() {
            validate_path_specs(package_id, typ, v)?;
        }
    } else if let Some(arr) = value.as_array() {
        for v in arr {
            validate_path_specs(package_id, typ, v)?;
        }
    }
    Ok(())
}
