//! Checklist-style install progress footer.
//!
//! One line per phase that will actually run (download, install, per-kind
//! cache rebuilds), computed from the execution plan up front. Lines
//! transition `○ waiting → spinner → ✓ done / ✗ failed`, redrawn in place
//! with cursor moves and line erases so intermediate frames never accumulate
//! in terminal scrollback — the final state is flushed exactly once, then the
//! summary prints below.
//!
//! On a non-TTY (piped) or `--verbose` run, the component degrades to the
//! previous static status lines (`Downloading...`, `Installing...`, ...).

use console::Term;
use glu_client::{
    download::DownloadProgress,
    events::{NodeCompletionStatus, OutputStream},
    format::{human_bytes_whole, human_speed},
    install::{
        dag::{ExecNode, ExecutionPlan, NodeKind},
        scheduler::ExecutionObserver,
    },
    postinstall::global_postinstall_label,
    style,
};
use std::{
    collections::{BTreeSet, VecDeque},
    io::{self, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::Instant,
};

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

#[derive(Debug, Clone, PartialEq, Eq)]
enum PhaseKind {
    Download,
    Install,
    Cache(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhaseState {
    Waiting,
    Active,
    Done,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishStatus {
    Done,
    Failed,
    Interrupted,
}

#[derive(Debug)]
struct Phase {
    kind: PhaseKind,
    total: usize,
    bytes_total: Option<u64>,
    state: PhaseState,
    done: usize,
    bytes_done: u64,
    /// Download phase only: smoothed live download speed in bytes/second.
    speed_bps: u64,
    /// Install phase only: completed `formula_postinstall` nodes. The phase
    /// flips to done only when both `done` (prepares) and this are exhaustive,
    /// so ✓ lands when per-package work is truly finished.
    postinstalls_done: usize,
}

impl Phase {
    fn waiting_text(&self, has_download_phase: bool) -> String {
        match &self.kind {
            PhaseKind::Download => {
                format!(
                    "Downloading {}",
                    glu_client::format::plural(self.total, "artifact")
                )
            }
            PhaseKind::Install => {
                let suffix = if has_download_phase {
                    ""
                } else {
                    " (from cache)"
                };
                format!(
                    "Installing {}{suffix}",
                    glu_client::format::plural(self.total, "package")
                )
            }
            PhaseKind::Cache(label) => format!("Rebuilding {label}"),
        }
    }

    fn running_text(&self, has_download_phase: bool) -> String {
        match &self.kind {
            PhaseKind::Download => {
                format!("Downloading {}/{}", self.done, self.total) + &self.bytes_segment(true)
            }
            PhaseKind::Install => {
                let suffix = if has_download_phase {
                    ""
                } else {
                    " (from cache)"
                };
                format!("Installing {}/{}", self.done, self.total) + suffix
            }
            PhaseKind::Cache(label) => format!("Rebuilding {label}"),
        }
    }

    /// Past-tense labels for the completed (✓) state.
    fn done_text(&self) -> String {
        match &self.kind {
            PhaseKind::Download => {
                format!("Downloaded {}/{}", self.done, self.total) + &self.bytes_segment(false)
            }
            PhaseKind::Install => format!("Installed {}/{}", self.done, self.total),
            PhaseKind::Cache(label) => format!("Rebuilt {label}"),
        }
    }

    /// Failed-state labels for the ✗ line: "Install failed 1/1 (from cache)".
    fn failed_text(&self, has_download_phase: bool) -> String {
        match &self.kind {
            PhaseKind::Download => format!("Download failed {}/{}", self.done, self.total),
            PhaseKind::Install => {
                let suffix = if has_download_phase {
                    ""
                } else {
                    " (from cache)"
                };
                format!("Install failed {}/{}", self.done, self.total) + suffix
            }
            PhaseKind::Cache(label) => format!("Rebuild {label} failed"),
        }
    }

    fn interrupted_text(&self, has_download_phase: bool) -> String {
        match &self.kind {
            PhaseKind::Download => format!("Download interrupted {}/{}", self.done, self.total),
            PhaseKind::Install => {
                let suffix = if has_download_phase {
                    ""
                } else {
                    " (from cache)"
                };
                format!("Install interrupted {}/{}", self.done, self.total) + suffix
            }
            PhaseKind::Cache(label) => format!("Rebuild {label} cache interrupted"),
        }
    }

    /// `· 27/48 MB` while in flight, `· 48 MB` once done, plus the live
    /// download speed in parentheses while actively downloading.
    fn bytes_segment(&self, include_speed: bool) -> String {
        match self.bytes_total {
            None => String::new(),
            Some(total) if self.done >= self.total => {
                format!(" · {}", human_bytes_whole(total))
            }
            Some(total) => {
                let (numerator, denominator, unit) = bytes_pair(self.bytes_done, total);
                let mut segment = format!(" · {numerator}/{denominator} {unit}");
                if include_speed && self.speed_bps > 0 {
                    segment.push_str(&format!(" · {}", human_speed(self.speed_bps)));
                }
                segment
            }
        }
    }
}

/// Whole-number progress pair in the unit of `total` (B/KB/MB/GB), so a
/// partial download of a large bottle never mixes units — a 443 KB start of a
/// 245 MB download renders `0/245 MB`, not `443 KB/245 MB`.
fn bytes_pair(done: u64, total: u64) -> (u64, u64, &'static str) {
    if total >= 1_000_000_000 {
        (done / 1_000_000_000, total / 1_000_000_000, "GB")
    } else if total >= 1_000_000 {
        (done / 1_000_000, total / 1_000_000, "MB")
    } else if total >= 1_000 {
        (done / 1_000, total / 1_000, "KB")
    } else {
        (done, total, "B")
    }
}

#[derive(Debug)]
struct FooterState {
    phases: Vec<Phase>,
    real_downloads: BTreeSet<String>,
    spinner: usize,
    /// (time, total bytes) samples for download speed, kept for ~1s.
    speed_samples: VecDeque<(Instant, u64)>,
    /// Non-dynamic mode only: mirrors the previous static status lines.
    downloading: bool,
    installing: bool,
    printed_caches: Vec<String>,
}

impl FooterState {
    fn from_plan(plan: &ExecutionPlan, install_total: usize) -> Self {
        let real_downloads: BTreeSet<String> = plan
            .edges
            .iter()
            .filter(|edge| edge.reason == "auth")
            .map(|edge| edge.target.clone())
            .collect();

        let mut phases = Vec::new();
        if !real_downloads.is_empty() {
            let mut bytes_total = 0u64;
            let mut known = true;
            for node in &plan.nodes {
                if node.kind == NodeKind::GhcrBottleDownload && real_downloads.contains(&node.id) {
                    match node.inputs.get("size").and_then(|v| v.as_u64()) {
                        Some(size) => bytes_total += size,
                        None => known = false,
                    }
                }
            }
            phases.push(Phase {
                kind: PhaseKind::Download,
                total: real_downloads.len(),
                bytes_total: known.then_some(bytes_total),
                state: PhaseState::Waiting,
                done: 0,
                bytes_done: 0,
                speed_bps: 0,
                postinstalls_done: 0,
            });
        }

        phases.push(Phase {
            kind: PhaseKind::Install,
            total: install_total,
            bytes_total: None,
            state: PhaseState::Waiting,
            done: 0,
            bytes_done: 0,
            speed_bps: 0,
            postinstalls_done: 0,
        });

        let mut seen = BTreeSet::new();
        for node in &plan.nodes {
            if node.kind != NodeKind::CachePostinstall {
                continue;
            }
            let label = cache_label(node);
            if seen.insert(label.clone()) {
                phases.push(Phase {
                    kind: PhaseKind::Cache(label),
                    total: 1,
                    bytes_total: None,
                    state: PhaseState::Waiting,
                    done: 0,
                    bytes_done: 0,
                    speed_bps: 0,
                    postinstalls_done: 0,
                });
            }
        }

        Self {
            phases,
            real_downloads,
            spinner: 0,
            speed_samples: VecDeque::new(),
            downloading: false,
            installing: false,
            printed_caches: Vec::new(),
        }
    }

    /// Samples total downloaded bytes and returns it with a smoothed speed
    /// over the last ~1 second of samples.
    fn sample_download_speed(&mut self, total_bytes: u64) -> u64 {
        let now = Instant::now();
        self.speed_samples.push_back((now, total_bytes));
        while self.speed_samples.len() > 1
            && now.duration_since(self.speed_samples.front().unwrap().0)
                > std::time::Duration::from_secs(1)
        {
            self.speed_samples.pop_front();
        }
        if self.speed_samples.len() < 2 {
            return 0;
        }
        let (t0, b0) = *self.speed_samples.front().unwrap();
        let (t1, b1) = *self.speed_samples.back().unwrap();
        let dt = t1.duration_since(t0).as_secs_f64();
        if dt > 0.0 {
            ((b1 - b0) as f64 / dt) as u64
        } else {
            0
        }
    }

    fn has_download_phase(&self) -> bool {
        self.phases.iter().any(|p| p.kind == PhaseKind::Download)
    }

    /// Phase a node belongs to, if any — used to activate phases on start
    /// and to mark failures. Install activates on any install work; the
    /// earliest is `bottle_prepare`. Cached downloads are ignored.
    fn phase_for_any(&self, node: &ExecNode) -> Option<usize> {
        match node.kind {
            NodeKind::GhcrBottleDownload if self.real_downloads.contains(&node.id) => self
                .phases
                .iter()
                .position(|p| p.kind == PhaseKind::Download),
            NodeKind::BottlePrepare
            | NodeKind::KegLink
            | NodeKind::FormulaPostinstall
            | NodeKind::RegistryWrite => self
                .phases
                .iter()
                .position(|p| p.kind == PhaseKind::Install),
            NodeKind::CachePostinstall => {
                let label = cache_label(node);
                self.phases
                    .iter()
                    .position(|p| matches!(&p.kind, PhaseKind::Cache(l) if *l == label))
            }
            _ => None,
        }
    }

    /// Phase a node *completes* — counted toward `done`. Install counts
    /// `bottle_prepare` (smooth, extraction progress) and `formula_postinstall`
    /// (true per-package completion); the phase flips done only when both are
    /// exhaustive, so ✓ lands when the last package is actually finished.
    fn phase_for_complete(&self, node: &ExecNode) -> Option<usize> {
        match node.kind {
            NodeKind::GhcrBottleDownload if self.real_downloads.contains(&node.id) => self
                .phases
                .iter()
                .position(|p| p.kind == PhaseKind::Download),
            NodeKind::BottlePrepare | NodeKind::FormulaPostinstall => self
                .phases
                .iter()
                .position(|p| p.kind == PhaseKind::Install),
            NodeKind::CachePostinstall => {
                let label = cache_label(node);
                self.phases
                    .iter()
                    .position(|p| matches!(&p.kind, PhaseKind::Cache(l) if *l == label))
            }
            _ => None,
        }
    }

    fn mark_started(&mut self, node: &ExecNode) {
        let Some(idx) = self.phase_for_any(node) else {
            return;
        };
        if self.phases[idx].state == PhaseState::Waiting {
            self.phases[idx].state = PhaseState::Active;
        }
    }

    fn mark_completed(&mut self, node: &ExecNode, status: NodeCompletionStatus) {
        // A failed node marks its phase failed, whatever the node kind.
        if status != NodeCompletionStatus::Ok {
            if let Some(idx) = self.phase_for_any(node) {
                self.phases[idx].state = PhaseState::Failed;
            }
            return;
        }
        let Some(idx) = self.phase_for_complete(node) else {
            return;
        };
        let phase = &mut self.phases[idx];
        if phase.state == PhaseState::Failed {
            return;
        }
        match &phase.kind {
            PhaseKind::Download => {
                phase.done += 1;
                // Live bytes come from DownloadProgress, pulled each tick;
                // the downloader's final report leaves the full size there.
            }
            PhaseKind::Install => match node.kind {
                NodeKind::BottlePrepare => phase.done += 1,
                NodeKind::FormulaPostinstall => phase.postinstalls_done += 1,
                _ => {}
            },
            PhaseKind::Cache(_) => phase.done += 1,
        }
        // A phase is done once its counts are exhaustive. For install both
        // prepares and postinstalls must be complete, so ✓ lands when every
        // package is fully committed and only the trace write remains.
        let prepares_done = phase.done >= phase.total;
        let postinstalls_done =
            !matches!(phase.kind, PhaseKind::Install) || phase.postinstalls_done >= phase.total;
        if prepares_done && postinstalls_done {
            phase.state = PhaseState::Done;
        }
    }

    /// Refresh the Download phase's byte counter and live speed from the
    /// progress map / sampled window.
    fn refresh_download_bytes(&mut self, total_bytes: u64, speed_bps: u64) {
        if let Some(phase) = self
            .phases
            .iter_mut()
            .find(|phase| phase.kind == PhaseKind::Download)
        {
            phase.bytes_done = total_bytes;
            phase.speed_bps = speed_bps;
        }
    }

    fn finish_status(&mut self, status: FinishStatus) {
        for phase in &mut self.phases {
            match status {
                FinishStatus::Done => {
                    if phase.state == PhaseState::Active || phase.state == PhaseState::Waiting {
                        phase.state = PhaseState::Done;
                    }
                }
                FinishStatus::Failed => {
                    if phase.state == PhaseState::Active {
                        phase.state = PhaseState::Failed;
                    }
                }
                FinishStatus::Interrupted => {
                    if phase.state == PhaseState::Active {
                        phase.state = PhaseState::Interrupted;
                    }
                }
            }
        }
    }

    fn render(&self) -> Vec<String> {
        let has_download = self.has_download_phase();
        self.phases
            .iter()
            .map(|phase| {
                let indicator = match phase.state {
                    PhaseState::Waiting => style::dim("○"),
                    PhaseState::Active => SPINNER[self.spinner % SPINNER.len()].to_string(),
                    PhaseState::Done => style::green("✓"),
                    PhaseState::Failed => style::red("✗"),
                    PhaseState::Interrupted => style::yellow("!"),
                };
                let text = match phase.state {
                    PhaseState::Waiting => phase.waiting_text(has_download),
                    PhaseState::Done => phase.done_text(),
                    PhaseState::Failed => phase.failed_text(has_download),
                    PhaseState::Interrupted => phase.interrupted_text(has_download),
                    _ => phase.running_text(has_download),
                };
                format!(" {indicator} {text}")
            })
            .collect()
    }
}

/// Tracks and renders the install progress footer.
pub struct ProgressFooter {
    term: Term,
    dynamic: bool,
    active: AtomicBool,
    drawn: AtomicBool,
    download_progress: DownloadProgress,
    deferred_notices: Mutex<Vec<String>>,
    deferred_stdout_notices: Mutex<Vec<String>>,
    state: Mutex<FooterState>,
}

impl ProgressFooter {
    /// Builds the footer from the execution plan. `dynamic` is false for
    /// piped stdout or `--verbose` runs (static lines only).
    pub fn from_plan(
        plan: &ExecutionPlan,
        install_total: usize,
        dynamic: bool,
        download_progress: DownloadProgress,
    ) -> Self {
        Self {
            term: Term::stdout(),
            dynamic: dynamic && Term::stdout().is_term(),
            active: AtomicBool::new(false),
            drawn: AtomicBool::new(false),
            download_progress,
            deferred_notices: Mutex::new(Vec::new()),
            deferred_stdout_notices: Mutex::new(Vec::new()),
            state: Mutex::new(FooterState::from_plan(plan, install_total)),
        }
    }

    /// Called once before execution: hides the cursor and draws the initial
    /// waiting checklist (dynamic mode only).
    pub fn start(&self) {
        if !self.dynamic {
            return;
        }
        self.active.store(true, Ordering::Relaxed);
        let _ = self.term.hide_cursor();
        self.draw();
    }

    /// Advances the spinner and redraws. No-op when not dynamic.
    pub fn tick(&self) {
        if !self.dynamic {
            return;
        }
        let mut state = self.state.lock().expect("footer state lock poisoned");
        state.spinner = (state.spinner + 1) % SPINNER.len();
        let total = self.download_progress.total_bytes();
        let speed = state.sample_download_speed(total);
        state.refresh_download_bytes(total, speed);
        drop(state);
        self.draw();
    }

    pub fn notice(&self, stream: OutputStream, message: &str) {
        if message.trim().is_empty() {
            return;
        }
        if self.dynamic && self.active.load(Ordering::Relaxed) {
            let notices = match stream {
                OutputStream::Stdout => &self.deferred_stdout_notices,
                OutputStream::Stderr => &self.deferred_notices,
            };
            notices
                .lock()
                .expect("notices lock poisoned")
                .push(message.to_string());
            return;
        }
        match stream {
            OutputStream::Stdout => println!("{message}"),
            OutputStream::Stderr => eprintln!("{message}"),
        }
    }

    /// Marks unfinished phases done/failed and flushes the final frame, then
    /// prints any deferred stderr notices and restores the cursor. After this
    /// the caller prints the summary below.
    pub fn finish_with_status(&self, status: FinishStatus) {
        if !self.dynamic {
            return;
        }
        {
            let mut state = self.state.lock().expect("footer state lock poisoned");
            state.finish_status(status);
        }
        self.draw();
        self.active.store(false, Ordering::Relaxed);
        let stdout_notices = std::mem::take(
            &mut *self
                .deferred_stdout_notices
                .lock()
                .expect("stdout notices lock poisoned"),
        );
        let notices =
            std::mem::take(&mut *self.deferred_notices.lock().expect("notices lock poisoned"));
        if !stdout_notices.is_empty() || !notices.is_empty() {
            // The in-place redraw leaves the cursor at the end-column of the
            // last footer line on the row below. Park it at column 0 and open
            // a blank line so deferred output starts on its own clean row
            // instead of being glued to the footer frame with phantom
            // indentation.
            let _ = self.term.clear_line();
            println!();
        }
        for message in stdout_notices {
            println!("{message}");
        }
        for message in notices {
            // trim(): strips phantom leading indent (in-place redraw artifacts)
            // AND the trailing newline most subprocess blobs carry, so
            // eprintln!'s own newline doesn't leave a stray blank line. Only
            // the very start/end of the whole blob is touched; inner lines
            // keep their formatting.
            eprintln!("{}", message.trim());
        }
        let _ = self.term.show_cursor();
    }

    fn draw(&self) {
        let state = self.state.lock().expect("footer state lock poisoned");
        let lines = state.render();
        let height = lines.len();
        drop(state);

        if !self.drawn.swap(true, Ordering::Relaxed) {
            for line in &lines {
                println!("{line}");
            }
        } else {
            // Rewrite in place, top-to-bottom, without emitting newlines so
            // intermediate frames never enter terminal scrollback. The cursor
            // must move down one line between writes — writing everything on
            // the same row would leave every line but the last stale.
            let _ = self.term.move_cursor_up(height);
            for line in &lines {
                let _ = self.term.clear_line();
                print!("{line}");
                let _ = self.term.move_cursor_down(1);
            }
        }
        let _ = io::stdout().flush();
    }
}

fn cache_label(node: &ExecNode) -> String {
    node.label.clone().unwrap_or_else(|| {
        node.inputs
            .get("kind")
            .and_then(|v| v.as_str())
            .map(global_postinstall_label)
            .unwrap_or_else(|| "global cache".to_string())
    })
}

impl ExecutionObserver for ProgressFooter {
    fn node_started(&self, node: &ExecNode) {
        let mut state = self.state.lock().expect("footer state lock poisoned");
        if self.dynamic {
            state.mark_started(node);
        } else {
            static_started(&mut state, node);
        }
    }

    fn node_completed(&self, node: &ExecNode, status: NodeCompletionStatus) {
        if self.dynamic {
            self.state
                .lock()
                .expect("footer state lock poisoned")
                .mark_completed(node, status);
        }
    }
}

/// Previous static status lines, kept for piped/verbose runs.
fn static_started(state: &mut FooterState, node: &ExecNode) {
    match node.kind {
        NodeKind::GhcrBottleDownload if !state.downloading => {
            // Every package gets a download node regardless of whether its artifact is
            // already cached — only announce "Downloading..." once we actually see one
            // that isn't.
            let already_cached = node
                .inputs
                .get("cache_path")
                .and_then(|v| v.as_str())
                .is_some_and(|path| std::path::Path::new(path).exists());
            if !already_cached {
                state.downloading = true;
                println!("Downloading...");
            }
        }
        NodeKind::BottlePrepare
        | NodeKind::KegLink
        | NodeKind::FormulaPostinstall
        | NodeKind::RegistryWrite
            if !state.installing =>
        {
            state.installing = true;
            if state.downloading {
                println!("Installing...");
            } else {
                println!("Installing from cache...");
            }
        }
        NodeKind::CachePostinstall => {
            let label = cache_label(node);
            if !state.printed_caches.contains(&label) {
                println!("Rebuilding {label}...");
                state.printed_caches.push(label);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glu_client::install::dag::{ExecEdge, ExecNode, ExecPool, NodeKind};
    use glu_core::PackageId;
    use serde_json::json;

    fn node(id: &str, kind: NodeKind, size: Option<u64>) -> ExecNode {
        ExecNode {
            id: id.to_string(),
            kind,
            pool: ExecPool::Setup,
            slot: None,
            package_id: Some(PackageId("pkg".to_string())),
            formula: None,
            label: None,
            inputs: json!({ "size": size, "kind": "compile_gsettings_schemas" }),
            outputs: json!({}),
            subphases: vec![],
            priority: 0.0,
        }
    }

    fn plan(nodes: Vec<ExecNode>, edges: Vec<ExecEdge>) -> ExecutionPlan {
        ExecutionPlan {
            nodes,
            edges,
            pools: Default::default(),
        }
    }

    #[test]
    fn phases_from_plan() {
        let nodes = vec![
            node(
                "ghcr_bottle_download:a",
                NodeKind::GhcrBottleDownload,
                Some(10_000),
            ),
            node(
                "ghcr_bottle_download:b",
                NodeKind::GhcrBottleDownload,
                Some(20_000),
            ),
            node(
                "ghcr_bottle_download:c",
                NodeKind::GhcrBottleDownload,
                Some(30_000),
            ),
            node("bottle_prepare:a", NodeKind::BottlePrepare, None),
        ];
        let edges = vec![
            ExecEdge {
                source: "auth".to_string(),
                target: "ghcr_bottle_download:a".to_string(),
                reason: "auth".to_string(),
            },
            ExecEdge {
                source: "auth".to_string(),
                target: "ghcr_bottle_download:b".to_string(),
                reason: "auth".to_string(),
            },
        ];
        let state = FooterState::from_plan(&plan(nodes, edges), 3);
        assert_eq!(state.phases.len(), 2); // download + install, no cache
        assert_eq!(state.phases[0].kind, PhaseKind::Download);
        assert_eq!(state.phases[0].total, 2);
        assert_eq!(state.phases[0].bytes_total, Some(30_000));
        assert_eq!(state.phases[1].kind, PhaseKind::Install);
        assert_eq!(state.phases[1].total, 3);
    }

    #[test]
    fn cached_downloads_do_not_add_phase() {
        let nodes = vec![node(
            "ghcr_bottle_download:a",
            NodeKind::GhcrBottleDownload,
            None,
        )];
        let state = FooterState::from_plan(&plan(nodes, vec![]), 2);
        assert_eq!(state.phases.len(), 1);
        assert_eq!(state.phases[0].kind, PhaseKind::Install);
    }

    #[test]
    fn cache_phases_are_deduplicated() {
        let nodes = vec![
            node("cache_postinstall:1", NodeKind::CachePostinstall, None),
            node("cache_postinstall:2", NodeKind::CachePostinstall, None),
        ];
        let state = FooterState::from_plan(&plan(nodes, vec![]), 1);
        assert_eq!(state.phases.len(), 2); // install + one cache (same label)
        assert!(matches!(
            &state.phases[1].kind,
            PhaseKind::Cache(l) if l == "GSettings schema cache"
        ));
    }

    #[test]
    fn started_and_completed_counts() {
        let nodes = vec![node(
            "ghcr_bottle_download:a",
            NodeKind::GhcrBottleDownload,
            Some(10_000),
        )];
        let edges = vec![ExecEdge {
            source: "auth".to_string(),
            target: "ghcr_bottle_download:a".to_string(),
            reason: "auth".to_string(),
        }];
        let mut state = FooterState::from_plan(&plan(nodes, edges), 2);

        state.mark_started(&node(
            "ghcr_bottle_download:a",
            NodeKind::GhcrBottleDownload,
            Some(10_000),
        ));
        assert_eq!(state.phases[0].state, PhaseState::Active);
        state.mark_completed(
            &node(
                "ghcr_bottle_download:a",
                NodeKind::GhcrBottleDownload,
                Some(10_000),
            ),
            NodeCompletionStatus::Ok,
        );
        assert_eq!(state.phases[0].done, 1);
        // Bytes come from the live progress map, not completion events.
        assert_eq!(state.phases[0].bytes_done, 0);
        state.refresh_download_bytes(10_000, 0);
        assert_eq!(state.phases[0].bytes_done, 10_000);
        assert_eq!(state.phases[0].state, PhaseState::Done); // count full -> done

        // Install activates on prepare; ✓ waits for both prepares and
        // postinstalls to be exhaustive.
        state.mark_started(&node("bottle_prepare:a", NodeKind::BottlePrepare, None));
        assert_eq!(state.phases[1].state, PhaseState::Active);
        state.mark_completed(
            &node("bottle_prepare:a", NodeKind::BottlePrepare, None),
            NodeCompletionStatus::Ok,
        );
        assert_eq!(state.phases[1].done, 1);
        assert_eq!(state.phases[1].postinstalls_done, 0);
        assert_eq!(state.phases[1].state, PhaseState::Active); // postinstall missing
        state.mark_completed(
            &node("registry_write:a", NodeKind::RegistryWrite, None),
            NodeCompletionStatus::Ok,
        );
        assert_eq!(state.phases[1].done, 1); // registry doesn't count
        state.mark_completed(
            &node("formula_postinstall:a", NodeKind::FormulaPostinstall, None),
            NodeCompletionStatus::Ok,
        );
        assert_eq!(state.phases[1].postinstalls_done, 1);
        assert_eq!(state.phases[1].state, PhaseState::Active); // 1 of 2 packages
        state.mark_completed(
            &node("bottle_prepare:b", NodeKind::BottlePrepare, None),
            NodeCompletionStatus::Ok,
        );
        state.mark_completed(
            &node("formula_postinstall:b", NodeKind::FormulaPostinstall, None),
            NodeCompletionStatus::Ok,
        );
        assert_eq!(state.phases[1].done, 2);
        assert_eq!(state.phases[1].postinstalls_done, 2);
        assert_eq!(state.phases[1].state, PhaseState::Done);
    }

    #[test]
    fn failed_node_marks_phase_failed() {
        // A failed prepare (not the completion node) still fails the phase.
        let nodes = vec![node("bottle_prepare:a", NodeKind::BottlePrepare, None)];
        let mut state = FooterState::from_plan(&plan(nodes, vec![]), 1);
        state.mark_completed(
            &node("bottle_prepare:a", NodeKind::BottlePrepare, None),
            NodeCompletionStatus::Error,
        );
        assert_eq!(state.phases[0].state, PhaseState::Failed);
    }

    #[test]
    fn interrupted_finish_does_not_render_failed() {
        let nodes = vec![node("bottle_prepare:a", NodeKind::BottlePrepare, None)];
        let mut state = FooterState::from_plan(&plan(nodes, vec![]), 1);
        state.mark_started(&node("bottle_prepare:a", NodeKind::BottlePrepare, None));

        state.finish_status(FinishStatus::Interrupted);
        let rendered = state.render();

        assert_eq!(state.phases[0].state, PhaseState::Interrupted);
        assert_eq!(rendered[0].trim(), "! Install interrupted 0/1 (from cache)");
    }

    #[test]
    fn live_download_bytes_flow_into_render() {
        let nodes = vec![node(
            "ghcr_bottle_download:a",
            NodeKind::GhcrBottleDownload,
            Some(30_000_000),
        )];
        let edges = vec![ExecEdge {
            source: "auth".to_string(),
            target: "ghcr_bottle_download:a".to_string(),
            reason: "auth".to_string(),
        }];
        let mut state = FooterState::from_plan(&plan(nodes, edges), 1);
        state.mark_started(&node(
            "ghcr_bottle_download:a",
            NodeKind::GhcrBottleDownload,
            Some(30_000_000),
        ));

        state.refresh_download_bytes(12_400_000, 12_400_000);
        let active = state.render();
        assert!(
            active[0].contains("Downloading 0/1 · 12/30 MB · 12 MB/s"),
            "got: {}",
            active[0]
        );

        state.refresh_download_bytes(30_000_000, 0);
        state.mark_completed(
            &node(
                "ghcr_bottle_download:a",
                NodeKind::GhcrBottleDownload,
                Some(30_000_000),
            ),
            NodeCompletionStatus::Ok,
        );
        let done = state.render();
        assert!(
            done[0].contains("Downloaded 1/1 · 30 MB"),
            "got: {}",
            done[0]
        );
    }

    #[test]
    fn progress_uses_the_totals_unit() {
        // A 443 KB partial of a 245 MB download renders in MB, not mixed units.
        assert_eq!(bytes_pair(443_000, 245_000_000), (0, 245, "MB"));
        assert_eq!(bytes_pair(12_400_000, 245_000_000), (12, 245, "MB"));
        // Small bottles render in KB throughout.
        assert_eq!(bytes_pair(0, 443_000), (0, 443, "KB"));
        assert_eq!(bytes_pair(443_000, 443_000), (443, 443, "KB"));
        assert_eq!(bytes_pair(5_000_000_000, 20_000_000_000), (5, 20, "GB"));
    }

    #[test]
    fn done_uses_past_tense() {
        let nodes = vec![node("bottle_prepare:a", NodeKind::BottlePrepare, None)];
        let mut state = FooterState::from_plan(&plan(nodes, vec![]), 2);
        state.mark_completed(
            &node("bottle_prepare:a", NodeKind::BottlePrepare, None),
            NodeCompletionStatus::Ok,
        );
        state.mark_completed(
            &node("formula_postinstall:a", NodeKind::FormulaPostinstall, None),
            NodeCompletionStatus::Ok,
        );
        state.mark_completed(
            &node("bottle_prepare:b", NodeKind::BottlePrepare, None),
            NodeCompletionStatus::Ok,
        );
        state.mark_completed(
            &node("formula_postinstall:b", NodeKind::FormulaPostinstall, None),
            NodeCompletionStatus::Ok,
        );
        let done = state.render();
        assert!(done[0].contains("✓ Installed 2/2"), "got: {}", done[0]);
    }

    #[test]
    fn render_shapes() {
        let nodes = vec![node("bottle_prepare:a", NodeKind::BottlePrepare, None)];
        let mut state = FooterState::from_plan(&plan(nodes, vec![]), 3);
        let waiting = state.render();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0], " ○ Installing 3 packages (from cache)");

        state.mark_started(&node("bottle_prepare:a", NodeKind::BottlePrepare, None));
        let active = state.render();
        assert!(active[0].starts_with(" ⠋") || active[0].starts_with(" ⠙"));
        assert!(active[0].contains("Installing 0/3 (from cache)"));

        state.mark_completed(
            &node("formula_postinstall:a", NodeKind::FormulaPostinstall, None),
            NodeCompletionStatus::Ok,
        );
        let running = state.render();
        assert!(running[0].contains("Installing 0/3 (from cache)")); // prepare not done yet
        state.mark_completed(
            &node("bottle_prepare:a", NodeKind::BottlePrepare, None),
            NodeCompletionStatus::Ok,
        );
        let running = state.render();
        assert!(running[0].contains("Installing 1/3 (from cache)"));
    }

    #[test]
    fn notices_defer_while_footer_active() {
        let mut footer = ProgressFooter::from_plan(
            &plan(Vec::new(), Vec::new()),
            0,
            false,
            DownloadProgress::default(),
        );
        footer.dynamic = true;
        footer.active.store(true, Ordering::Relaxed);
        footer.notice(OutputStream::Stderr, "stderr message");
        footer.notice(OutputStream::Stdout, "stdout message");
        assert_eq!(
            *footer.deferred_notices.lock().unwrap(),
            vec!["stderr message".to_string()]
        );
        assert_eq!(
            *footer.deferred_stdout_notices.lock().unwrap(),
            vec!["stdout message".to_string()]
        );
    }

    #[test]
    fn empty_notices_are_dropped() {
        let mut footer = ProgressFooter::from_plan(
            &plan(Vec::new(), Vec::new()),
            0,
            false,
            DownloadProgress::default(),
        );
        footer.dynamic = true;
        footer.active.store(true, Ordering::Relaxed);
        footer.notice(OutputStream::Stderr, "");
        footer.notice(OutputStream::Stderr, " \n\t ");
        footer.notice(OutputStream::Stdout, "");
        footer.notice(OutputStream::Stdout, "   ");
        assert!(footer.deferred_notices.lock().unwrap().is_empty());
        assert!(footer.deferred_stdout_notices.lock().unwrap().is_empty());
    }

    #[test]
    fn failed_phase_renders_failed_label() {
        let node = node("bottle_prepare:a", NodeKind::BottlePrepare, None);
        let mut state = FooterState::from_plan(&plan(vec![node.clone()], vec![]), 1);
        state.mark_started(&node);
        state.mark_completed(&node, NodeCompletionStatus::Error);
        let rendered = state.render();
        assert_eq!(rendered.len(), 1);
        assert_eq!(rendered[0].trim(), "✗ Install failed 0/1 (from cache)");
    }
}
