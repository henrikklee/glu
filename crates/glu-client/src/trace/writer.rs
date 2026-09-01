use crate::state::atomic_write;
use anyhow::{Context, Result};
use glu_core::Prefix;
use serde_json::Value;
use std::{ffi::OsStr, path::PathBuf};

#[derive(Debug, Clone)]
pub struct WrittenTrace {
    pub path: PathBuf,
    pub last_path: PathBuf,
}

pub fn trace_dir(prefix: &Prefix) -> PathBuf {
    prefix.0.join("var/glu/traces")
}

pub fn write_install_trace(
    prefix: &Prefix,
    plan_name: &str,
    trace: &Value,
) -> Result<WrittenTrace> {
    let dir = trace_dir(prefix);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let path = dir.join(format!(
        "trace-{timestamp}-{}-{}.json",
        sanitize_plan_name(plan_name),
        short_trace_id(plan_name)
    ));
    let bytes = serde_json::to_vec_pretty(trace).context("encoding trace JSON")?;
    let mut final_bytes = bytes;
    final_bytes.push(b'\n');
    let trace_directory = atomic_write::open_directory(&dir)?;
    let trace_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("trace path has no file name: {}", path.display()))?;
    atomic_write::replace_file(&trace_directory, &dir, trace_name, &final_bytes)?;

    let last_path = dir.join("last.json");
    let target = path
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| path.clone());
    atomic_write::replace_symlink(&trace_directory, &dir, OsStr::new("last.json"), &target)?;

    Ok(WrittenTrace { path, last_path })
}

/// Short disambiguating suffix (6 hex chars) derived from the plan name and
/// the current time, so same-second runs of the same package don't collide.
fn short_trace_id(plan_name: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let digest = ring::digest::digest(
        &ring::digest::SHA256,
        format!("{plan_name}:{nanos}").as_bytes(),
    );
    digest
        .as_ref()
        .iter()
        .take(3)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn sanitize_plan_name(name: &str) -> String {
    let sanitized = name
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '@' | '.' | '_' | '-' | '+' | '=' => ch,
            _ => '_',
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "install".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_id_is_six_lowercase_hex() {
        let id = short_trace_id("node");
        assert_eq!(id.len(), 6);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(id, id.to_lowercase());
    }

    #[test]
    fn short_ids_differ_across_names() {
        assert_ne!(short_trace_id("node"), short_trace_id("vips"));
    }

    #[test]
    #[cfg(unix)]
    fn trace_and_last_link_are_replaced_without_predictable_temps() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let prefix = Prefix(temp.path().to_path_buf());
        let dir = trace_dir(&prefix);
        std::fs::create_dir_all(&dir).unwrap();
        let outside = temp.path().join("outside");
        std::fs::write(&outside, b"sentinel").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("last.json.tmp")).unwrap();

        let written =
            write_install_trace(&prefix, "node", &serde_json::json!({"schema": "test"})).unwrap();

        assert_eq!(std::fs::read(outside).unwrap(), b"sentinel");
        assert_eq!(
            std::fs::metadata(&written.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read_link(&written.last_path).unwrap(),
            written.path.file_name().unwrap()
        );
        for entry in std::fs::read_dir(dir).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(!name.contains("glu-tmp"), "atomic write left temp: {name}");
        }
    }
}
