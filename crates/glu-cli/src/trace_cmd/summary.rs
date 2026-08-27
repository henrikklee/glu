use crate::{
    command_model::generated_schema,
    tables::{table_row, table_rule},
};
use anyhow::{Context, Result};
use glu_client::GluClient;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

pub(super) fn summary_output(
    client: &GluClient,
    target: Option<&str>,
    show_all_packages: bool,
) -> Result<TraceSummary> {
    let source = super::resolve::resolve_trace_target(client, target)?;
    let mut summary = summarize_trace(&source)?;
    summary.show_all_packages = show_all_packages;
    Ok(summary)
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
#[schemars(rename = "TraceSummaryResult")]
pub(crate) struct TraceSummary {
    #[serde(skip)]
    #[schemars(skip)]
    show_all_packages: bool,
    id: Option<String>,
    timestamp: Option<String>,
    package: String,
    status: String,
    path: String,
    elapsed_seconds: f64,
    phase_wall_seconds: BTreeMap<String, f64>,
    phase_busy_seconds: BTreeMap<String, f64>,
    packages: Vec<PackageSummary>,
    codesign_ran: bool,
    postinstall_ran: bool,
    failure: Option<TraceFailure>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct PackageSummary {
    name: String,
    phase_busy_seconds: BTreeMap<String, f64>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct TraceFailure {
    node_id: Option<String>,
    package: Option<String>,
    phase: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct EventRecord {
    start: f64,
    end: f64,
    phase: String,
    package: Option<String>,
    node_id: Option<String>,
    status: Option<String>,
    error: Option<String>,
}

fn summarize_trace(source: &Path) -> Result<TraceSummary> {
    let display_source = source
        .canonicalize()
        .unwrap_or_else(|_| source.to_path_buf());
    let text = std::fs::read_to_string(&display_source)
        .with_context(|| format!("reading trace {}", display_source.display()))?;
    let trace: Value = serde_json::from_str(&text).context("trace is not valid JSON")?;
    let nodes = node_index(&trace);
    let events = event_records(&trace, &nodes);
    let elapsed_seconds = round_seconds(events_duration(&events));
    let phase_wall_seconds = phase_wall_seconds(&events);
    let phase_busy_seconds = phase_busy_seconds(&events);
    let packages = package_summaries(&events);
    let filename = trace_filename_metadata(&display_source);
    let package = trace["plan"]
        .as_str()
        .or_else(|| trace.get("package").and_then(|v| v.as_str()))
        .or(filename.package.as_deref())
        .unwrap_or("trace")
        .to_string();
    let status = trace["status"].as_str().unwrap_or("ok").to_string();
    let failure = failure_summary(&trace, &events);

    Ok(TraceSummary {
        show_all_packages: false,
        id: filename.id,
        timestamp: filename.timestamp,
        package,
        status,
        path: display_source.display().to_string(),
        elapsed_seconds,
        codesign_ran: phase_busy_seconds.contains_key("codesign"),
        postinstall_ran: phase_busy_seconds.contains_key("postinstall"),
        phase_wall_seconds,
        phase_busy_seconds,
        packages,
        failure,
    })
}

fn node_index(trace: &Value) -> BTreeMap<String, Value> {
    trace["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|node| Some((node["id"].as_str()?.to_string(), node.clone())))
        .collect()
}

fn event_records(trace: &Value, nodes: &BTreeMap<String, Value>) -> Vec<EventRecord> {
    trace["events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|event| {
            let start = event["start"].as_f64()?;
            let end = event["end"].as_f64()?;
            let node_id = event["node_id"].as_str().map(str::to_string);
            let node = node_id.as_ref().and_then(|id| nodes.get(id));
            if event["phase"].as_str().is_none()
                && node.and_then(|n| n["kind"].as_str()) == Some("bottle_prepare")
                && node
                    .and_then(|n| n["subphases"].as_array())
                    .is_some_and(|subphases| !subphases.is_empty())
            {
                return None;
            }
            let phase = event["phase"]
                .as_str()
                .map(canonical_phase)
                .or_else(|| node.and_then(|n| n["kind"].as_str()).map(canonical_kind))
                .unwrap_or_else(|| "other".to_string());
            let package = node
                .and_then(|n| n["formula"].as_str())
                .or_else(|| node.and_then(|n| n["inputs"]["formula_name"].as_str()))
                .map(str::to_string);
            Some(EventRecord {
                start,
                end,
                phase,
                package,
                node_id,
                status: event["status"].as_str().map(str::to_string),
                error: event["error"].as_str().map(str::to_string),
            })
        })
        .collect()
}

fn canonical_phase(phase: &str) -> String {
    match phase {
        "extract" => "extract",
        "codesign" => "codesign",
        "text_relocate" | "fixed_prefix_relocate" | "macho_patch" => "relocate",
        other => other,
    }
    .to_string()
}

fn canonical_kind(kind: &str) -> String {
    match kind {
        "ghcr_bottle_download" => "download",
        "bottle_prepare" => "prepare",
        "keg_link" | "keg_link_existing" => "link",
        "formula_postinstall" => "postinstall",
        "registry_write" => "registry",
        "ghcr_auth" => "setup",
        other => other,
    }
    .to_string()
}

fn events_duration(events: &[EventRecord]) -> f64 {
    let Some(min_start) = events.iter().map(|event| event.start).reduce(f64::min) else {
        return 0.0;
    };
    let Some(max_end) = events.iter().map(|event| event.end).reduce(f64::max) else {
        return 0.0;
    };
    (max_end - min_start).max(0.0)
}

fn phase_busy_seconds(events: &[EventRecord]) -> BTreeMap<String, f64> {
    let mut totals = BTreeMap::new();
    for event in events {
        *totals.entry(event.phase.clone()).or_insert(0.0) += (event.end - event.start).max(0.0);
    }
    round_map(totals)
}

fn phase_wall_seconds(events: &[EventRecord]) -> BTreeMap<String, f64> {
    let mut spans: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();
    for event in events {
        spans
            .entry(event.phase.clone())
            .or_default()
            .push((event.start, event.end));
    }
    let totals = spans
        .into_iter()
        .map(|(phase, spans)| (phase, union_duration(spans)))
        .collect();
    round_map(totals)
}

fn package_summaries(events: &[EventRecord]) -> Vec<PackageSummary> {
    let mut totals: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
    for event in events {
        let Some(package) = &event.package else {
            continue;
        };
        *totals
            .entry(package.clone())
            .or_default()
            .entry(event.phase.clone())
            .or_insert(0.0) += (event.end - event.start).max(0.0);
    }
    totals
        .into_iter()
        .filter_map(|(name, phases)| {
            let mut phases = round_map(phases);
            phases.retain(|_, seconds| *seconds > 0.0);
            (!phases.is_empty()).then_some(PackageSummary {
                name,
                phase_busy_seconds: phases,
            })
        })
        .collect()
}

fn union_duration(mut spans: Vec<(f64, f64)>) -> f64 {
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut total = 0.0;
    let mut current: Option<(f64, f64)> = None;
    for (start, end) in spans {
        let end = end.max(start);
        match current {
            None => current = Some((start, end)),
            Some((cur_start, cur_end)) if start <= cur_end => {
                current = Some((cur_start, cur_end.max(end)));
            }
            Some((cur_start, cur_end)) => {
                total += cur_end - cur_start;
                current = Some((start, end));
            }
        }
    }
    if let Some((start, end)) = current {
        total += end - start;
    }
    total.max(0.0)
}

fn round_map(map: BTreeMap<String, f64>) -> BTreeMap<String, f64> {
    map.into_iter()
        .map(|(key, value)| (key, round_seconds(value)))
        .collect()
}

fn round_seconds(seconds: f64) -> f64 {
    (seconds * 1000.0).round() / 1000.0
}

fn failure_summary(trace: &Value, events: &[EventRecord]) -> Option<TraceFailure> {
    events
        .iter()
        .find(|event| event.status.as_deref() == Some("failed"))
        .map(|event| TraceFailure {
            node_id: event.node_id.clone(),
            package: event.package.clone(),
            phase: Some(event.phase.clone()),
            error: event
                .error
                .clone()
                .or_else(|| trace["error"].as_str().map(str::to_string)),
        })
        .or_else(|| {
            trace["error"].as_str().map(|error| TraceFailure {
                node_id: None,
                package: None,
                phase: None,
                error: Some(error.to_string()),
            })
        })
}

#[derive(Default)]
struct FilenameMetadata {
    id: Option<String>,
    timestamp: Option<String>,
    package: Option<String>,
}

fn trace_filename_metadata(path: &Path) -> FilenameMetadata {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return FilenameMetadata::default();
    };
    let Some(stem) = name
        .strip_prefix("trace-")
        .and_then(|s| s.strip_suffix(".json"))
    else {
        return FilenameMetadata::default();
    };
    let mut parts = stem.rsplitn(2, '-');
    let Some(id) = parts.next() else {
        return FilenameMetadata::default();
    };
    if id.len() != 6 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return FilenameMetadata::default();
    }
    let Some(head) = parts.next() else {
        return FilenameMetadata::default();
    };
    let mut head_parts = head.splitn(3, '-');
    let Some(date) = head_parts.next() else {
        return FilenameMetadata::default();
    };
    let Some(clock) = head_parts.next() else {
        return FilenameMetadata::default();
    };
    if date.len() != 8
        || !date.chars().all(|c| c.is_ascii_digit())
        || clock.len() != 6
        || !clock.chars().all(|c| c.is_ascii_digit())
    {
        return FilenameMetadata::default();
    }
    FilenameMetadata {
        id: Some(id.to_string()),
        timestamp: Some(format!("{date} {clock}")),
        package: head_parts.next().map(str::to_string),
    }
}

pub(super) fn result_schema() -> serde_json::Value {
    generated_schema::<TraceSummary>()
}

pub(crate) fn render_human(summary: &TraceSummary) {
    let id = summary.id.as_deref().unwrap_or("unknown");
    println!(
        "Trace {id} — {} — {} — {}",
        summary.package,
        format_status(&summary.status),
        format_seconds(summary.elapsed_seconds)
    );
    if let Some(timestamp) = &summary.timestamp {
        println!(
            "{}",
            glu_client::style::dim(&format!("{timestamp} · {}", summary.path))
        );
    } else {
        println!("{}", glu_client::style::dim(&summary.path));
    }
    println!();
    println!("Phase totals (wall-clock aware):");
    for phase in preferred_phases(&summary.phase_wall_seconds) {
        let seconds = summary
            .phase_wall_seconds
            .get(&phase)
            .copied()
            .unwrap_or(0.0);
        println!("  {phase:<12} {}", format_seconds(seconds));
    }
    println!();
    println!(
        "Ran: codesign={} postinstall={}",
        yes_no(summary.codesign_ran),
        yes_no(summary.postinstall_ran)
    );
    if let Some(failure) = &summary.failure {
        println!();
        println!("Failure:");
        if let Some(package) = &failure.package {
            println!("  package: {package}");
        }
        if let Some(phase) = &failure.phase {
            println!("  phase:   {phase}");
        }
        if let Some(error) = &failure.error {
            println!("  error:   {error}");
        }
    }
    if !summary.packages.is_empty() {
        println!();
        println!("Packages:");
        print_package_table(summary, summary.show_all_packages);
    }
}

fn print_package_table(summary: &TraceSummary, show_all_packages: bool) {
    let visible = if show_all_packages {
        summary.packages.len()
    } else {
        summary.packages.len().min(12)
    };
    let packages = &summary.packages[..visible];
    let phases = package_table_phases(packages);
    let mut headers = Vec::with_capacity(phases.len() + 1);
    headers.push("Package".to_string());
    headers.extend(phases.iter().map(|phase| title_case_phase(phase)));

    let mut widths: Vec<usize> = headers.iter().map(|header| header.len()).collect();
    for package in packages {
        widths[0] = widths[0].max(package.name.len());
        for (idx, phase) in phases.iter().enumerate() {
            let value = package
                .phase_busy_seconds
                .get(phase)
                .copied()
                .filter(|seconds| *seconds > 0.0)
                .map(format_seconds)
                .unwrap_or_else(|| "".to_string());
            widths[idx + 1] = widths[idx + 1].max(value.len());
        }
    }

    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        println!("{}", table_rule(&widths, '┌', '┬', '┐'));
        let header_cells: Vec<(&str, &str)> = headers
            .iter()
            .map(|header| (header.as_str(), header.as_str()))
            .collect();
        println!("{}", table_row(&header_cells, &widths, true));
        println!("{}", table_rule(&widths, '├', '┼', '┤'));
        for (i, package) in packages.iter().enumerate() {
            if i > 0 {
                println!("{}", table_rule(&widths, '├', '┼', '┤'));
            }
            let mut values = Vec::with_capacity(phases.len() + 1);
            values.push(package.name.clone());
            for phase in &phases {
                values.push(
                    package
                        .phase_busy_seconds
                        .get(phase)
                        .copied()
                        .filter(|seconds| *seconds > 0.0)
                        .map(format_seconds)
                        .unwrap_or_default(),
                );
            }
            let cells: Vec<(&str, &str)> = values
                .iter()
                .map(|value| (value.as_str(), value.as_str()))
                .collect();
            println!("{}", table_row(&cells, &widths, false));
        }
        println!("{}", table_rule(&widths, '└', '┴', '┘'));
    } else {
        println!("{}", plain_table_row(&headers, &widths));
        for package in packages {
            let mut values = Vec::with_capacity(phases.len() + 1);
            values.push(package.name.clone());
            for phase in &phases {
                values.push(
                    package
                        .phase_busy_seconds
                        .get(phase)
                        .copied()
                        .filter(|seconds| *seconds > 0.0)
                        .map(format_seconds)
                        .unwrap_or_default(),
                );
            }
            println!("{}", plain_table_row(&values, &widths));
        }
    }

    if summary.packages.len() > visible {
        println!(
            "{}",
            glu_client::style::dim(&format!(
                "… {} more (use --all or --verbose)",
                summary.packages.len() - visible
            ))
        );
    }
}

fn package_table_phases(packages: &[PackageSummary]) -> Vec<String> {
    let mut present = BTreeMap::new();
    for package in packages {
        for (phase, seconds) in &package.phase_busy_seconds {
            if *seconds > 0.0 {
                present.insert(phase.clone(), ());
            }
        }
    }
    preferred_phases(&present.into_keys().map(|phase| (phase, 1.0)).collect())
}

fn title_case_phase(phase: &str) -> String {
    let mut chars = phase.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn plain_table_row(values: &[String], widths: &[usize]) -> String {
    values
        .iter()
        .zip(widths)
        .map(|(value, width)| format!("{value:<width$}"))
        .collect::<Vec<_>>()
        .join("  ")
        .trim_end()
        .to_string()
}

fn preferred_phases(map: &BTreeMap<String, f64>) -> Vec<String> {
    let mut phases = Vec::new();
    for phase in [
        "setup",
        "download",
        "extract",
        "relocate",
        "codesign",
        "prepare",
        "link",
        "postinstall",
        "registry",
    ] {
        if map.contains_key(phase) {
            phases.push(phase.to_string());
        }
    }
    for phase in map.keys() {
        if !phases.iter().any(|known| known == phase) {
            phases.push(phase.clone());
        }
    }
    phases
}

fn format_status(status: &str) -> String {
    match status {
        "failed" => glu_client::style::yellow(status),
        "ok" => glu_client::style::green(status),
        other => other.to_string(),
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn format_seconds(seconds: f64) -> String {
    if seconds >= 10.0 {
        format!("{seconds:.1}s")
    } else if seconds >= 1.0 {
        format!("{seconds:.2}s")
    } else {
        format!("{:.0}ms", seconds * 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn union_duration_merges_overlaps() {
        assert_eq!(
            round_seconds(union_duration(vec![(0.0, 2.0), (1.0, 3.0), (5.0, 6.0)])),
            4.0
        );
    }

    #[test]
    fn summary_maps_trace_events_to_phase_totals() {
        let trace = json!({
            "plan": "demo",
            "status": "ok",
            "nodes": [
                {"id":"ghcr_bottle_download:demo","kind":"ghcr_bottle_download","formula":"demo"},
                {"id":"bottle_prepare:demo","kind":"bottle_prepare","formula":"demo"},
                {"id":"keg_link:demo","kind":"keg_link","formula":"demo"}
            ],
            "events": [
                {"node_id":"ghcr_bottle_download:demo","start":0.0,"end":2.0,"status":"ok"},
                {"node_id":"bottle_prepare:demo","phase":"extract","start":2.0,"end":3.0,"status":"ok"},
                {"node_id":"bottle_prepare:demo","phase":"codesign","start":3.0,"end":3.5,"status":"ok"},
                {"node_id":"keg_link:demo","start":3.5,"end":4.0,"status":"ok"}
            ]
        });
        let nodes = node_index(&trace);
        let events = event_records(&trace, &nodes);
        let wall = phase_wall_seconds(&events);
        assert_eq!(wall["download"], 2.0);
        assert_eq!(wall["extract"], 1.0);
        assert_eq!(wall["codesign"], 0.5);
        assert_eq!(wall["link"], 0.5);
        assert_eq!(events_duration(&events), 4.0);
    }

    #[test]
    fn generated_schema_accepts_trace_summary_result() {
        let summary = TraceSummary {
            show_all_packages: false,
            id: None,
            timestamp: None,
            package: "demo".to_string(),
            status: "ok".to_string(),
            path: "/tmp/trace.json".to_string(),
            elapsed_seconds: 0.0,
            phase_wall_seconds: BTreeMap::new(),
            phase_busy_seconds: BTreeMap::new(),
            packages: Vec::new(),
            codesign_ran: false,
            postinstall_ran: false,
            failure: None,
        };
        let value = serde_json::to_value(summary).unwrap();
        let schema = result_schema();
        let validator = jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .build(&schema)
            .unwrap();
        assert!(validator.is_valid(&value));
    }

    #[test]
    fn filename_metadata_parses_standard_trace_name() {
        let meta = trace_filename_metadata(Path::new(
            "/tmp/trace-20260825-054734-gstreamer-9719bb.json",
        ));
        assert_eq!(meta.id.as_deref(), Some("9719bb"));
        assert_eq!(meta.timestamp.as_deref(), Some("20260825 054734"));
        assert_eq!(meta.package.as_deref(), Some("gstreamer"));
    }
}
