use anyhow::{bail, Result};
use glu_client::GluClient;

/// Resolve a `trace` subcommand target to a trace JSON path.
///
/// `None` -> the most recent trace (`<dir>/last.json`). A target containing a
/// path separator is treated as an explicit file path. Otherwise it is matched
/// against the trace directory: by full filename, by the 6-char short id
/// (e.g. `a1b2c3` -> `trace-...-a1b2c3.json`), or by package name -> the most
/// recent trace for that package. Only current-convention traces
/// (`trace-<ts>-<pkg>-<id>.json`) are matched; previously the loose
/// `ends_with("-<t>.json")` rule captured legacy `trace-<pkg>.json` files, so
/// `view vips` opened an ancient artifact instead of the latest vips run and a
/// package with no recent trace opened a viewer instead of failing early. No
/// match -> error.
pub(super) fn resolve_trace_target(
    client: &GluClient,
    target: Option<&str>,
) -> Result<std::path::PathBuf> {
    let dir = glu_client::trace::writer::trace_dir(&client.config().prefix);
    if !dir.exists() {
        if let Some(t) = target {
            // A direct path might exist even without a trace dir.
            let p = std::path::Path::new(t);
            if p.exists() {
                return Ok(p.to_path_buf());
            }
        }
        // No trace dir and no explicit path: nothing to resolve.
        return match target {
            Some(t) => Err(trace_not_found(&dir, t)),
            None => Err(anyhow::anyhow!("no traces yet — run `glu install` first")),
        };
    }
    let path = match target {
        None => dir.join("last.json"),
        Some(t) if t.contains('/') || t.contains(std::path::MAIN_SEPARATOR) => {
            std::path::PathBuf::from(t)
        }
        Some(t) => {
            let full = dir.join(t);
            if full.exists() {
                full
            } else {
                // Current-convention traces, newest first. Match by 6-char
                // short id first, then by package name -> the most recent run.
                let traces = conforming_traces_newest_first(&dir);
                match traces
                    .iter()
                    .find(|tr| tr.id == t)
                    .or_else(|| traces.iter().find(|tr| tr.pkg == t))
                {
                    Some(tr) => tr.path.clone(),
                    None => return Err(trace_not_found(&dir, t)),
                }
            }
        }
    };
    if !path.exists() {
        bail!("trace not found: {}", path.display());
    }
    Ok(path)
}

/// One current-convention trace file, parsed from its filename.
struct TraceMatch {
    /// `YYYYMMDD HHMMSS` timestamp prefix; sorts lexically by run time.
    timestamp: String,
    /// Package segment of the filename (may itself contain `-`, e.g. `ada-url`).
    pkg: String,
    /// Trailing 6-hex short id.
    id: String,
    path: std::path::PathBuf,
}

/// Read the trace dir and return current-convention
/// (`trace-<YYYYMMDD>-<HHMMSS>-<pkg>-<6hex>.json`) entries, newest first.
///
/// Uses the same strict shape `trace_entry` applies for `glu trace list`, so
/// `view` resolves exactly the traces `list` shows. Legacy files
/// (`trace-<pkg>.json` from before the id convention) and partial/corrupt
/// names are excluded: a bare package target either resolves to its most
/// recent real trace or fails fast.
fn conforming_traces_newest_first(dir: &std::path::Path) -> Vec<TraceMatch> {
    let mut traces: Vec<TraceMatch> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                let path = e.path();
                let name = path.file_name()?.to_string_lossy().into_owned();
                let stem = name.strip_prefix("trace-")?.strip_suffix(".json")?;
                let mut parts = stem.rsplitn(2, '-');
                let id = parts.next()?;
                if id.len() != 6 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
                    return None;
                }
                let head = parts.next()?;
                let mut head_parts = head.splitn(3, '-');
                let date = head_parts.next()?;
                let clock = head_parts.next()?;
                if date.len() != 8
                    || !date.chars().all(|c| c.is_ascii_digit())
                    || clock.len() != 6
                    || !clock.chars().all(|c| c.is_ascii_digit())
                {
                    return None;
                }
                Some(TraceMatch {
                    timestamp: format!("{date} {clock}"),
                    pkg: head_parts.next().unwrap_or("").to_string(),
                    id: id.to_string(),
                    path,
                })
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    // Newest first: the embedded `YYYYMMDD HHMMSS` sorts lexically in that
    // direction when reversed (same rule `list_traces` uses).
    traces.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    traces
}

/// Build a `Did you mean ...?`-style error for an unmatched trace target.
///
/// Scans the directory for `trace-*.json` files whose 6-hex short id starts
/// with `target` (so a partial id gets a usable suggestion) and closes the
/// message with the best match.
fn trace_not_found(dir: &std::path::Path, target: &str) -> anyhow::Error {
    let mut close = dir
        .read_dir()
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter_map(|name| {
            let stem = name.strip_prefix("trace-")?.strip_suffix(".json")?;
            let id = stem.rsplit('-').next()?;
            (id.len() == 6 && id.chars().all(|c| c.is_ascii_hexdigit())).then_some(id.to_string())
        })
        .filter(|id| id.starts_with(target))
        .collect::<Vec<_>>();
    close.sort();
    close.dedup();
    let hint = if close.is_empty() {
        String::new()
    } else {
        format!(" Did you mean {}?", close[0])
    };
    anyhow::anyhow!("no trace matched '{target}'{hint}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Client pinned to `prefix` so trace resolution targets the scratch dir.
    fn resolver_for_prefix(prefix: std::path::PathBuf) -> GluClient {
        let config = glu_client::config::ClientConfig {
            prefix: glu_core::Prefix(prefix),
            target: glu_core::Target("test".to_string()),
            registry_base_url: "http://localhost:3000".to_string(),
            distribution_base_url: String::new(),
        };
        GluClient::new(config)
    }

    /// Scratch prefix + trace dir for one resolution test (kept distinct so
    /// parallel tests in the same process don't collide).
    fn trace_test_prefix(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let prefix =
            std::env::temp_dir().join(format!("glu-resolve-{name}-test-{}", std::process::id()));
        let traces = prefix.join("var/glu/traces");
        std::fs::create_dir_all(&traces).unwrap();
        (prefix, traces)
    }

    #[test]
    fn resolve_package_picks_most_recent_trace_not_legacy_file() {
        let (prefix, traces) = trace_test_prefix("package-latest");
        // Two real runs of `vips`, plus a legacy `trace-vips.json` — the file
        // the old loose `ends_with("-vips.json")` rule grabbed.
        for name in [
            "trace-20260815-061043-vips-bfdb64.json",
            "trace-20260818-005229-vips-5f7680.json",
        ] {
            std::fs::write(
                traces.join(name),
                r#"{"plan":"vips","nodes":[],"edges":[],"events":[]}"#,
            )
            .unwrap();
        }
        std::fs::write(
            traces.join("trace-vips.json"),
            r#"{"plan":"vips","nodes":[],"edges":[],"events":[]}"#,
        )
        .unwrap();
        let client = resolver_for_prefix(prefix.clone());

        let resolved = resolve_trace_target(&client, Some("vips")).unwrap();
        assert_eq!(
            resolved.file_name().unwrap().to_string_lossy(),
            "trace-20260818-005229-vips-5f7680.json"
        );
        let _ = std::fs::remove_dir_all(&prefix);
    }

    #[test]
    fn resolve_package_names_with_dashes() {
        let (prefix, traces) = trace_test_prefix("pkg-dashes");
        std::fs::write(
            traces.join("trace-20260815-193022-ada-url-773bd3.json"),
            "{}",
        )
        .unwrap();
        let client = resolver_for_prefix(prefix.clone());
        let resolved = resolve_trace_target(&client, Some("ada-url")).unwrap();
        assert!(resolved
            .to_string_lossy()
            .ends_with("trace-20260815-193022-ada-url-773bd3.json"));
        let _ = std::fs::remove_dir_all(&prefix);
    }

    #[test]
    fn resolve_by_short_id() {
        let (prefix, traces) = trace_test_prefix("short-id");
        std::fs::write(traces.join("trace-20260818-005229-vips-5f7680.json"), "{}").unwrap();
        let client = resolver_for_prefix(prefix.clone());
        let resolved = resolve_trace_target(&client, Some("5f7680")).unwrap();
        assert!(resolved
            .to_string_lossy()
            .ends_with("trace-20260818-005229-vips-5f7680.json"));
        let _ = std::fs::remove_dir_all(&prefix);
    }

    #[test]
    fn resolve_package_with_only_legacy_file_fails_early() {
        // `trace-fd.json` (no id convention) exists, but no real `fd` trace:
        // `view fd` must error before rendering, not open the ancient file.
        let (prefix, traces) = trace_test_prefix("legacy-only");
        std::fs::write(traces.join("trace-fd.json"), "{}").unwrap();
        let client = resolver_for_prefix(prefix.clone());
        let err = resolve_trace_target(&client, Some("fd")).unwrap_err();
        assert!(err.to_string().contains("no trace matched 'fd'"), "{err}");
        let _ = std::fs::remove_dir_all(&prefix);
    }

    #[test]
    fn resolve_explicit_legacy_filename_still_works() {
        // Naming the legacy file by its full filename stays an explicit,
        // working escape hatch.
        let (prefix, traces) = trace_test_prefix("explicit-legacy");
        std::fs::write(traces.join("trace-vips.json"), "{}").unwrap();
        let client = resolver_for_prefix(prefix.clone());
        let resolved = resolve_trace_target(&client, Some("trace-vips.json")).unwrap();
        assert_eq!(
            resolved.file_name().unwrap().to_string_lossy(),
            "trace-vips.json"
        );
        let _ = std::fs::remove_dir_all(&prefix);
    }

    #[test]
    fn resolve_no_target_uses_last_json_or_errors() {
        let (prefix, traces) = trace_test_prefix("last-json");
        let client = resolver_for_prefix(prefix.clone());
        // No last.json -> early error, no viewer.
        let err = resolve_trace_target(&client, None).unwrap_err();
        assert!(err.to_string().contains("trace not found"), "{err}");
        // A present last.json resolves.
        std::fs::write(traces.join("last.json"), "{}").unwrap();
        let resolved = resolve_trace_target(&client, None).unwrap();
        assert_eq!(resolved.file_name().unwrap().to_string_lossy(), "last.json");
        let _ = std::fs::remove_dir_all(&prefix);
    }

    #[test]
    fn resolve_no_trace_dir_errors_early() {
        let prefix =
            std::env::temp_dir().join(format!("glu-resolve-absent-{}", std::process::id()));
        let client = resolver_for_prefix(prefix.clone());
        let err = resolve_trace_target(&client, None).unwrap_err();
        assert!(err.to_string().contains("no traces yet"), "{err}");
        let err = resolve_trace_target(&client, Some("vips")).unwrap_err();
        assert!(err.to_string().contains("no trace matched 'vips'"), "{err}");
        let _ = std::fs::remove_dir_all(&prefix);
    }

    #[test]
    fn resolve_explicit_path_outside_dir() {
        let (prefix, _traces) = trace_test_prefix("explicit-path");
        let external = std::env::temp_dir().join(format!("glu-resolve-ext-{}", std::process::id()));
        std::fs::write(&external, "{}").unwrap();
        let client = resolver_for_prefix(prefix.clone());
        let resolved = resolve_trace_target(&client, Some(&external.to_string_lossy())).unwrap();
        assert_eq!(resolved, external);
        // Missing path fails early.
        let missing = external.with_extension("nope.json");
        let err = resolve_trace_target(&client, Some(&missing.to_string_lossy())).unwrap_err();
        assert!(err.to_string().contains("trace not found"), "{err}");
        let _ = std::fs::remove_file(&external);
        let _ = std::fs::remove_dir_all(&prefix);
    }
}
