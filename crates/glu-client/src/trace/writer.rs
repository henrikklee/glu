use anyhow::{Context, Result};
use glu_core::Prefix;
use serde_json::Value;
use std::path::PathBuf;

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
    let tmp = path.with_extension("json.tmp");
    let mut final_bytes = bytes;
    final_bytes.push(b'\n');
    std::fs::write(&tmp, final_bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("moving {} to {}", tmp.display(), path.display()))?;

    let last_path = dir.join("last.json");
    let _ = std::fs::remove_file(&last_path);
    #[cfg(unix)]
    {
        let target = path
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| path.clone());
        std::os::unix::fs::symlink(&target, &last_path)
            .with_context(|| format!("symlink {} -> {}", last_path.display(), target.display()))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::copy(&path, &last_path)
            .with_context(|| format!("copying {} to {}", path.display(), last_path.display()))?;
    }

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
    use super::short_trace_id;

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
}
