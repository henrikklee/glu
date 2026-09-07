//! Rendering a trace as a self-contained single-file HTML timeline and DAG
//! viewer. The renderer embeds trace JSON into a fixed template that renders
//! a Gantt timeline and a dagre-flow graph entirely client-side.
//!
//! The consumed trace shape matches glu's `scheduler::trace_dict` / `dag`:
//! `{ plan, nodes[], edges[], events[] }` where nodes carry `id`/`kind`/
//! `formula`, edges carry `from`/`to`/`reason`, and events carry `node_id`/
//! `phase`/`start`/`end`/`status`/`pool`/`slot`. Each event records the
//! concurrency token (`pool` + `slot`) the scheduler assigned, so the timeline
//! is grouped into one lane per `(pool, slot)`. A whole-node event is one
//! bar; `bottle_prepare` subphases are folded into that bar as coloured
//! segments (`extract`/`writer_wait`/`text_relocate`/`fixed_prefix_relocate`/
//! `macho_patch`/`codesign`), never rendered as separate bars.
//!
//! Rendering is local-only: the HTML is written to a temp path and opened by
//! the CLI; trace data is never uploaded. The `dagre` layout library is stored
//! gzip-compressed in the binary and expanded into the self-contained viewer
//! when it is rendered, so viewing a trace performs no network requests.

use flate2::read::GzDecoder;
use ring::rand::{SecureRandom, SystemRandom};
use std::{io::Read, sync::OnceLock};

const DAGRE_GZIP: &[u8] = include_bytes!("../../assets/dagre-0.8.5.min.js.gz");

fn dagre_js() -> &'static str {
    static DAGRE_JS: OnceLock<String> = OnceLock::new();
    DAGRE_JS.get_or_init(|| {
        let mut source = String::new();
        GzDecoder::new(DAGRE_GZIP)
            .read_to_string(&mut source)
            .expect("embedded dagre asset must be valid gzip-compressed UTF-8");
        assert!(
            !source
                .as_bytes()
                .windows(b"</script".len())
                .any(|window| window.eq_ignore_ascii_case(b"</script")),
            "embedded dagre asset must not close its script element"
        );
        source
    })
}

/// Render a trace `Value` into a standalone HTML document.
///
/// The JSON is inlined into a `<script type="application/json">` block with
/// every `</` escaped to `<\/` so a `</script>` inside the data cannot break
/// out of the script element.
pub fn render_html(trace: &serde_json::Value, title: &str) -> String {
    use std::fmt::Write;

    let safe_trace_json = trace
        .to_string()
        .split("</")
        .collect::<Vec<_>>()
        .join("<\\/");
    let mut page_title = String::new();
    let _ = write!(page_title, "{title}");
    let nonce = csp_nonce();

    HTML_TEMPLATE
        .replace("__TITLE__", &html_attr(&page_title))
        .replace("__CSP_NONCE__", &nonce)
        .replace("__DAGRE_JS__", dagre_js())
        .replace("__TRACE_JSON__", &safe_trace_json)
}

fn csp_nonce() -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = [0_u8; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .expect("system randomness must be available for trace viewer CSP");
    let mut nonce = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        nonce.push(HEX[(byte >> 4) as usize] as char);
        nonce.push(HEX[(byte & 0x0f) as usize] as char);
    }
    nonce
}

/// Escape for an HTML attribute/text-context placeholder (the page `<title>`
/// and the `h1` text). The trace JSON itself is escaped via the `</` guard.
fn html_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Static single-file viewer template. Four placeholders:
///   __TITLE__       -> injected, HTML-attr escaped
///   __CSP_NONCE__   -> fresh random nonce for the two executable scripts
///   __DAGRE_JS__    -> trusted vendored Dagre source
///   __TRACE_JSON__  -> the trace, with `</` escaped to `<\\/`
const HTML_TEMPLATE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'nonce-__CSP_NONCE__'; style-src 'unsafe-inline'; img-src data:; connect-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'" />
<title>__TITLE__</title>
<script nonce="__CSP_NONCE__">__DAGRE_JS__</script>
<style>
/* Keep the brand tokens aligned with glu-www/src/styles/global.css.
   Local font stacks deliberately avoid web-font requests in this offline viewer.
   Chart colours are separate from text accents to keep dark bar labels legible. */
:root {
  color-scheme: light;
  --font-display: 'Cabinet Grotesk', 'General Sans', -apple-system, sans-serif;
  --font-body: 'General Sans', -apple-system, sans-serif;
  --font-mono: 'IBM Plex Mono', ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  --bg: #f3f5f2;
  --panel: #edf0ee;
  --panel-2: #e7ebe8;
  --ink: #14191d;
  --muted: #536068;
  --faint: #69747b;
  --rule: #cbd2cd;
  --rule-strong: #c7cec9;
  --lime: #b5ff3d;
  --acid: #78b800;
  --accent-text: #456b00;
  --accent-soft: rgba(120,184,0,.09);
  --graph-dot: rgba(83,97,107,.14);
  --graph-line: rgba(83,97,107,.05);
  --header-bg: rgba(243,245,242,.77);
  --surface: rgba(255,255,255,.74);
  --chart-bg: var(--bg);
  --lane-rule: rgba(83,97,107,.13);
  --control: #edf0ee;
  --control-hover: #d9dfdb;
  --tooltip-bg: #f3f5f2;
  --edge: #69747b;
  --cyan: #62d7ed;
  --orange: #ffc16b;
  --green: #62dfa0;
  --violet: #c0a0f5;
  --neutral: #a8b7af;
  --commit: var(--lime);
  --red: #b8452f;
  --error-ink: #ffffff;
  --bar-ink: #071116;
  --shadow: 15 18 21;
}
@media (prefers-color-scheme: dark) {
  :root {
    color-scheme: dark;
    --bg: #151a1e;
    --panel: #171c21;
    --panel-2: #181e23;
    --ink: #f5f7f6;
    --muted: #b6c0bd;
    --faint: #8e9a97;
    --rule: #2b343b;
    --rule-strong: #303a42;
    --acid: var(--lime);
    --accent-text: var(--lime);
    --accent-soft: rgba(181,255,61,.08);
    --graph-dot: rgba(135,146,154,.14);
    --graph-line: rgba(135,146,154,.045);
    --header-bg: rgba(21,26,31,.8);
    --surface: rgba(21,26,31,.82);
    --lane-rule: rgba(135,146,154,.13);
    --control: #22292f;
    --control-hover: #2c353b;
    --tooltip-bg: #181e23;
    --edge: #8e9a97;
    --cyan: #83cfe3;
    --orange: #eec18a;
    --green: #91d4ab;
    --violet: #c0aceb;
    --neutral: #9baaa3;
    --commit: var(--lime);
    --red: #ff6b4d;
    --error-ink: var(--bar-ink);
    --shadow: 0 0 0;
  }
}
* { box-sizing: border-box; }
body {
  margin: 0;
  height: 100vh;
  height: 100dvh;
  display: flex;
  flex-direction: column;
  overflow: hidden;
  background-color: var(--bg);
  background-image:
    radial-gradient(circle at 0 0, var(--graph-dot) 1.25px, transparent 1.45px),
    linear-gradient(to right, var(--graph-line) 1px, transparent 1px),
    linear-gradient(to bottom, var(--graph-line) 1px, transparent 1px);
  background-size: 30px 30px;
  background-position: 1px 1px;
  color: var(--ink);
  font: 12px/1.5 var(--font-body);
  -webkit-font-smoothing: antialiased;
  text-rendering: optimizeLegibility;
}
header {
  position: sticky;
  top: 0;
  z-index: 5;
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 1rem;
  padding: 14px 20px;
  border-bottom: 1px solid var(--rule);
  background: var(--header-bg);
  backdrop-filter: blur(16px);
  -webkit-backdrop-filter: blur(16px);
  flex: none;
}
.heading { display: flex; align-items: center; gap: 10px; }
.package-mark { width: 23px; height: 23px; flex: none; color: var(--acid); stroke: currentColor; stroke-width: 2; stroke-linecap: round; stroke-linejoin: round; }
h1 { margin: 0; font: 800 22px/1.1 var(--font-display); overflow-wrap: anywhere; }
#summary { margin-top: 10px; }
.local-note { display: flex; align-items: center; gap: 8px; color: var(--muted); font: 11px var(--font-mono); white-space: nowrap; }
.local-note::before { content: ''; width: 6px; height: 6px; border-radius: 50%; background: var(--acid); box-shadow: 0 0 0 3px var(--accent-soft); }
.pill-row { display: flex; flex-wrap: wrap; gap: 6px; align-items: center; }
.pill {
  border: 1px solid var(--rule);
  background: var(--surface);
  border-radius: 6px;
  padding: 3px 7px;
  color: var(--muted);
  font: 11px/1.5 var(--font-mono);
}
.pill strong { color: var(--ink); font-weight: 650; }
main {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  gap: 10px;
  padding: 0 20px 10px;
}
.tabs { display: flex; gap: 6px; flex: none; padding-top: 10px; }
.tab {
  border: 1px solid var(--rule);
  background: var(--surface);
  color: var(--muted);
  border-radius: 6px;
  padding: 6px 14px;
  font: inherit;
  cursor: pointer;
}
.tab:hover { border-color: var(--acid); }
.tab.active {
  color: var(--bar-ink);
  border-color: var(--lime);
  background: var(--lime);
  font-weight: 600;
}
.card {
  overflow: hidden;
  border: 1px solid var(--rule);
  background: var(--surface);
  border-radius: 12px;
  box-shadow: 0 8px 28px rgb(var(--shadow) / .06);
}
.pane { display: none; flex-direction: column; flex: 1; min-height: 0; }
.pane.active { display: flex; }
.pane-body { flex: 1; min-height: 0; }
.card-head {
  display: flex;
  justify-content: space-between;
  gap: 1rem;
  align-items: center;
  padding: 10px 12px;
  border-bottom: 1px solid var(--rule);
  background: var(--panel);
}
h2 { margin: 0; font: 500 12px/1.4 var(--font-mono); text-transform: uppercase; letter-spacing: .08em; color: var(--accent-text); }
.controls { display: flex; gap: 10px; flex-wrap: wrap; align-items: center; color: var(--muted); }
input, select, button {
  border: 1px solid var(--rule);
  background: var(--control);
  color: var(--ink);
  border-radius: 6px;
  padding: 5px 8px;
  font: inherit;
}
button { cursor: pointer; }
button:hover { border-color: var(--acid); background: var(--control-hover); }
.tab.active:hover { background: var(--lime); }
.controls label { display: inline-flex; align-items: center; gap: 8px; }
input { width: 240px; min-width: 0; }
input::placeholder { color: var(--faint); opacity: 1; }
:focus-visible { outline: 2px solid var(--accent-text); outline-offset: 3px; }
::selection { background: var(--lime); color: var(--bar-ink); }
.gantt, #graphSvg, #tooltip { font-family: var(--font-mono); }
.pill strong, .tick { font-variant-numeric: tabular-nums; }
#ganttWrap { flex: 1; min-height: 0; overflow: auto; touch-action: pan-y; background: var(--chart-bg); }
/* Drag-pan affordances: grabbed cursor + no text selection while the gantt is
   being dragged, and a visible focus ring so keyboard panning (arrows) has an
   obvious target. */
#ganttWrap.panning { cursor: grabbing; user-select: none; -webkit-user-select: none; }
#ganttWrap:focus-visible { outline: 2px solid var(--accent-text); outline-offset: -2px; }
.gantt {
  position: relative;
  min-width: 920px;
  margin: 10px 12px 14px;
  display: grid;
  grid-template-columns: 180px 1fr;
  gap: 0;
}
/* Continuous, quiet row guides rather than boxed label cells. */
.axis, .gantt-corner { border-bottom: 1px solid var(--rule); }
.lane-label, .lane-track { border-bottom: 1px solid var(--lane-rule); }
.gantt-corner {
  grid-column: 1;
  height: 26px;
  position: sticky;
  left: 0;
  top: 0;
  background: var(--chart-bg);
  z-index: 5;
}
.axis {
  grid-column: 2;
  height: 26px;
  position: sticky;
  top: 0;
  background: var(--chart-bg);
  z-index: 2;
}
.tick { position: absolute; top: 0; bottom: 0; color: var(--muted); font-size: 10px; padding-left: 4px; }
/* The 100% tick's label must end at the axis edge, not overflow past it
   (which otherwise forces a spurious horizontal scrollbar). */
.tick:last-child { left: auto !important; right: 0; text-align: right; padding-left: 0; padding-right: 6px; border-left: none; }
.lane-label {
  grid-column: 1;
  height: 32px;
  display: flex;
  align-items: center;
  padding-right: 12px;
  color: var(--muted);
  white-space: nowrap;
  position: sticky;
  left: 0;
  z-index: 4;
  background: var(--chart-bg);
}
.lane-track {
  grid-column: 2;
  height: 32px;
  position: relative;
  overflow: hidden;
  background-image: linear-gradient(90deg, var(--graph-line) 1px, transparent 1px);
  background-size: 10% 100%;
}
.bar {
  position: absolute;
  top: 6px;
  height: 20px;
  min-width: 1px;
  border-radius: 4px;
  box-shadow: 0 0 0 1px rgb(var(--shadow) / .12) inset;
  background: var(--cyan);
  overflow: hidden;
  user-select: none;
  -webkit-user-select: none;
}
/* Segmented-bar internals: each subphase or pool segment is an absolutely
   positioned fill inside the single node bar. The text label sits above. */
.seg {
  position: absolute;
  top: 0;
  bottom: 0;
  border-right: 1px solid rgba(0,0,0,.25);
}
.seg:last-child { border-right: none; }
.seg-content {
  position: absolute;
  inset: 0;
  line-height: 20px;
  padding: 0 6px;
  color: var(--bar-ink);
  font-size: 11px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  pointer-events: none;
  z-index: 1;
}
.seg-full { left: 0 !important; width: 100% !important; }
.bar[data-status="error"], .bar[data-status="failed"] { background: var(--red); }
.bar[data-status="error"] .seg, .bar[data-status="failed"] .seg { background: var(--red) !important; }
/* Error labels contrast with the danger fill in each system theme. */
.bar[data-status="error"] .seg-content, .bar[data-status="failed"] .seg-content { color: var(--error-ink); }
.bar.critical::before {
  content: '';
  position: absolute;
  left: 0;
  right: 0;
  top: 0;
  height: 3px;
  background: var(--red);
  z-index: 2;
  pointer-events: none;
}
.bar:hover { outline: 2px solid var(--acid); z-index: 3; }
#tooltip {
  position: fixed;
  z-index: 20;
  max-width: min(520px, calc(100vw - 32px));
  pointer-events: none;
  opacity: 0;
  transform: translate(10px, 10px);
  border: 1px solid var(--rule-strong);
  background: var(--tooltip-bg);
  color: var(--ink);
  border-radius: 12px;
  padding: 9px 10px;
  box-shadow: 0 12px 36px rgb(var(--shadow) / .18);
  overflow-wrap: anywhere;
  white-space: pre-wrap;
  transition: opacity .08s ease;
}
#tooltip.visible { opacity: 1; }
#graphWrap { flex: 1; min-height: 0; overflow: auto; background: var(--chart-bg); display: flex; align-items: center; justify-content: center; }
#graphSvg { width: 100%; max-height: 100%; touch-action: none; cursor: grab; }
#graphSvg.panning { cursor: grabbing; }
.node rect { fill: var(--panel-2); stroke: var(--rule-strong); stroke-width: 1.2; rx: 6; }
.node:hover rect { stroke: var(--acid); }
.node text { fill: var(--ink); font-size: 11px; }
.node .kind { fill: var(--accent-text); font-size: 10px; text-transform: uppercase; }
.edge { fill: none; stroke: var(--edge); stroke-width: 1.2; marker-end: url(#arrow); }
#arrow path { fill: var(--edge); }
.edge-label { fill: var(--muted); font-size: 9px; paint-order: stroke; stroke: var(--chart-bg); stroke-width: 4px; }
.graph-grid { fill: none; stroke: var(--graph-dot); stroke-width: 1; stroke-dasharray: 3 7; }
.row-label { fill: var(--muted); font-size: 10px; paint-order: stroke; stroke: var(--chart-bg); stroke-width: 4px; }
.empty { padding: 24px; color: var(--muted); }
footer { flex: none; color: var(--muted); padding: 0 20px 12px; font-size: 11px; }
@media (max-width: 900px) {
  header { align-items: flex-start; }
  .local-note { display: none; }
  .card-head { align-items: flex-start; flex-direction: column; gap: 12px; }
  #graphHint { flex-basis: 100%; font-size: 11px; }
}
@media (max-width: 600px) {
  header { padding: 16px; }
  h1 { font-size: 21px; }
  main { padding: 0 12px 10px; gap: 10px; }
  #summary { gap: 5px; margin-top: 12px; }
  .pill { padding: 3px 6px; font-size: 10px; }
  .card-head { padding: 12px; }
  .controls { width: 100%; gap: 8px; }
  .controls label:first-child { flex: 1; }
  input { width: 100%; }
  .gantt { grid-template-columns: 140px 1fr; margin: 12px; }
  footer { padding: 0 16px 12px; font-size: 11px; }
}
/* Keep the chart usable in short windows instead of collapsing its scroll area. */
@media (max-height: 600px) {
  body { height: auto; min-height: 100dvh; overflow: auto; }
  .pane-body { flex: auto; height: 50vh; min-height: 220px; }
}
@media (prefers-reduced-motion: reduce) {
  #tooltip { transition: none; }
}
</style>
</head>
<body>
<header>
  <div>
    <div class="heading">
      <svg class="package-mark" viewBox="0 0 24 24" fill="none" aria-hidden="true">
        <path d="M11 21.73a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73z"></path>
        <path d="M12 22V12M3.29 7 12 12l8.71-5M7.5 4.27l9 5.15"></path>
      </svg>
      <h1 id="title">glu trace</h1>
    </div>
    <div class="pill-row" id="summary"></div>
  </div>
  <span class="local-note">Local trace · no uploads</span>
</header>
<main>
  <nav class="tabs" id="tabs" role="tablist" aria-label="Trace views">
    <button type="button" role="tab" id="tab-timeline" aria-selected="true" aria-controls="timeline-pane" data-pane="timeline-pane" class="tab active">Timeline</button>
    <button type="button" role="tab" id="tab-flow" aria-selected="false" aria-controls="flow-pane" data-pane="flow-pane" class="tab">Flow</button>
  </nav>
  <section class="card pane active" id="timeline-pane" role="tabpanel" aria-labelledby="tab-timeline">
    <div class="card-head">
      <h2>Timeline</h2>
      <div class="controls">
        <label>Filter <input id="filter" placeholder="package, phase, pool, status" /></label>
        <label>Scale <select id="scale"><option value="1">fit</option><option value="2">2x</option><option value="4">4x</option><option value="8">8x</option><option value="custom">custom</option></select></label>
      </div>
    </div>
    <div id="ganttWrap" class="pane-body" tabindex="0"><div id="gantt"></div></div>
  </section>
  <section class="card pane" id="flow-pane" role="tabpanel" aria-labelledby="tab-flow" aria-hidden="true">
    <div class="card-head">
      <h2>Flow</h2>
      <div class="controls"><label>Edges <select id="edgeMode"><option value="flow">flow</option><option value="all">all</option></select></label><button id="resetGraph" type="button">Reset zoom</button><span id="graphHint">dagre layout · wheel/pinch to zoom · drag to pan</span></div>
    </div>
    <div id="graphWrap" class="pane-body"><svg id="graphSvg"></svg></div>
  </section>
</main>
<footer>Tip: each row is one concurrency lane (pool + slot). Hover a bar for its lifecycle; segmented bars break prepare into extract/relocate/machO/codesign. Zoom with ctrl+wheel or pinch; when zoomed, pan the time window by dragging, with ←/→, or with trackpad / shift+wheel.</footer>
<div id="tooltip" role="tooltip"></div>
<script id="trace-data" type="application/json">__TRACE_JSON__</script>
<script nonce="__CSP_NONCE__">
const trace = JSON.parse(document.getElementById('trace-data').textContent);
const nodes = trace.nodes || [];
const edges = trace.edges || [];
const events = trace.events || [];
const nodeById = new Map(nodes.map(n => [n.id, n]));
const kindOf = id => (nodeById.get(id) || {}).kind || String(id).split(':')[0];
function nodeLabel(id) {
  const node = nodeById.get(id);
  if (!node) return String(id).split(':').pop() || String(id);
  if (node.label) return node.label;
  if (node.formula) return node.formula;
  return node.id || String(id);
}
function nodeDetail(id) {
  const node = nodeById.get(id);
  const label = nodeLabel(id);
  if (!node) return String(id);
  if (node.kind === 'cache_postinstall') {
    const key = Array.isArray(node.inputs?.key) ? node.inputs.key : [];
    const keyLines = key.length ? `\nkey: ${key.join('\nkey: ')}` : '';
    return `${label}${keyLines}\nnode: ${node.id}`;
  }
  return node.id === label ? node.id : `${label}\nnode: ${node.id}`;
}
const labelOf = e => nodeLabel(e.node_id);
const fullLabelOf = e => e.phase ? `${nodeDetail(e.node_id)}\nphase: ${e.phase}` : nodeDetail(e.node_id);
const fmt = n => `${n.toFixed(n < 10 ? 3 : 2)}s`;
// Wall-clock run time, injected from the trace filename by the CLI when the
// filename carries a `trace-YYYYMMDD-HHMMSS-` prefix (absent for old traces).
const runLine = trace.started_at ? `run: ${trace.started_at}\n` : '';
const observedCriticalPath = computeObservedCriticalPath(edges, events);

function computeObservedCriticalPath(edges, events) {
  const epsilon = 0.001;
  const whole = events.filter(e => e.phase == null && e.node_id != null && Number.isFinite(e.start) && Number.isFinite(e.end));
  const eventByNode = new Map(whole.map(e => [e.node_id, e]));
  const predecessors = new Map();
  const addPred = (to, from, reason) => {
    if (!to || !from || to === from || !eventByNode.has(to) || !eventByNode.has(from)) return;
    if (!predecessors.has(to)) predecessors.set(to, []);
    predecessors.get(to).push({nodeId: from, reason});
  };

  for (const edge of edges) addPred(edge.to, edge.from, edge.reason || 'dependency');

  const lanes = new Map();
  for (const e of whole) {
    const key = `${e.pool || ''}:${e.slot == null ? '?' : e.slot}`;
    if (!lanes.has(key)) lanes.set(key, []);
    lanes.get(key).push(e);
  }
  for (const [key, lane] of lanes) {
    lane.sort((a, b) => (a.start - b.start) || (a.end - b.end) || String(a.node_id).localeCompare(String(b.node_id)));
    for (let i = 1; i < lane.length; i++) {
      const prev = lane[i - 1], next = lane[i];
      if (prev.end <= next.start + epsilon) addPred(next.node_id, prev.node_id, `lane_serial ${key}`);
    }
  }

  const ids = new Set();
  const waitedFor = new Map();
  if (!whole.length) return {ids, waitedFor};

  let current = whole.slice().sort((a, b) => (b.end - a.end) || String(a.node_id).localeCompare(String(b.node_id)))[0];
  const seen = new Set();
  while (current && !seen.has(current.node_id)) {
    ids.add(current.node_id);
    seen.add(current.node_id);
    const candidates = (predecessors.get(current.node_id) || [])
      .map(p => ({...p, event: eventByNode.get(p.nodeId)}))
      .filter(p => p.event && p.event.end <= current.start + epsilon)
      .sort((a, b) => (b.event.end - a.event.end) || String(a.nodeId).localeCompare(String(b.nodeId)));
    const best = candidates[0];
    if (!best) break;
    waitedFor.set(current.node_id, {nodeId: best.nodeId, reason: best.reason});
    current = best.event;
  }
  return {ids, waitedFor};
}

function summarize() {
  const min = events.length ? Math.min(...events.map(e => e.start)) : 0;
  const max = events.length ? Math.max(...events.map(e => e.end)) : 0;
  document.getElementById('title').textContent = `glu trace · ${trace.plan || 'unknown'}`;
  // A lane is a (pool, slot) concurrency token that actually ran a node.
  const lanes = new Set(events.filter(e => e.phase == null && e.pool).map(e => `${e.pool}:${e.slot == null ? '?' : e.slot}`));
  const packageCount = new Set(events.filter(e => e.phase == null).map(labelOf)).size;
  const pills = [];
  if (trace.started_at) pills.push(['started', trace.started_at]);
  pills.push(['packages', packageCount], ['nodes', nodes.length], ['edges', edges.length], ['events', events.length],
    ['lanes', lanes.size], ['duration', fmt(max - min)]);
  const summary = document.getElementById('summary');
  summary.replaceChildren();
  for (const [key, value] of pills) {
    const pill = document.createElement('span');
    pill.className = 'pill';
    pill.append(document.createTextNode(`${key}: `));
    const strong = document.createElement('strong');
    strong.textContent = String(value);
    pill.append(strong);
    summary.append(pill);
  }
}

// Subphase segment colours (the six `bottle_prepare` steps). Falls back to a
// per-pool tint for whole-node bars that have no subphase breakdown.
// CSS variables stay live when the system theme changes, without rebuilding
// either view or losing the current filter, zoom, or pan position.
const SUBPHASE_COLORS = {
  extract: 'var(--orange)',
  writer_wait: 'var(--neutral)',
  text_relocate: 'var(--cyan)',
  fixed_prefix_relocate: 'var(--cyan)',
  macho_patch: 'var(--cyan)',
  codesign: 'var(--violet)',
};
const POOL_COLORS = {
  setup: 'var(--neutral)',
  download: 'var(--cyan)',
  prepare: 'var(--orange)',
  commit: 'var(--commit)',
  postinstall: 'var(--violet)',
  registry: 'var(--green)',
};

let ganttView = null;
// Assigned by setupGanttInteractions: stops any active drag/fling so zooms,
// scale changes, and filter resets never fight a running pan animation.
let cancelGanttPan = () => {};
const GANTT_MIN_SCALE = 1;
const GANTT_MAX_SCALE = 1000000;

function renderGantt() {
  const filter = document.getElementById('filter').value.toLowerCase().trim();
  const shown = events.filter(e => !filter ||
    [e.node_id, nodeLabel(e.node_id), nodeDetail(e.node_id), e.phase || '', e.pool || '', e.status, kindOf(e.node_id)]
      .join(' ').toLowerCase().includes(filter));
  const el = document.getElementById('gantt');
  if (!shown.length) { el.className = 'empty'; el.textContent = 'No events match.'; return; }
  el.className = 'gantt';
  highlightedPackageClass = '';

  // Whole-node events are the bars (one per node). Subphase events are
  // segments folded into their parent node's bar — never their own bar.
  const whole = shown.filter(e => e.phase == null);
  const subphases = shown.filter(e => e.phase != null);
  const subByNode = {};
  for (const s of subphases) {
    (subByNode[s.node_id] = subByNode[s.node_id] || []).push(s);
  }
  for (const k in subByNode) subByNode[k].sort((a, b) => a.start - b.start);

  const packageNames = [...new Set(whole.map(labelOf))].sort();
  const packageClassByName = new Map(packageNames.map((name, i) => [name, `pkg-${i}`]));
  packageHighlightStyle.textContent = packageNames
    .map((_, i) => `.gantt:has(.pkg-${i}:hover) .pkg-${i}{outline:2px solid var(--acid);z-index:3}`)
    .join('\n');

  const fullMin = Math.min(...shown.map(e => e.start));
  const fullMax = Math.max(...shown.map(e => e.end));
  const fullSpan = Math.max(fullMax - fullMin, 0.001);
  if (!ganttView) ganttView = {start: fullMin, end: fullMax};
  ganttView = clampGanttView(ganttView.start, ganttView.end, fullMin, fullMax);
  const min = ganttView.start;
  const max = ganttView.end;
  const span = Math.max(max - min, fullSpan / GANTT_MAX_SCALE);

  // Lanes: every (pool, slot) that ran a whole-node bar, ordered by pool
  // then slot, so capacity per pool reads top-to-bottom.
  const laneKeys = [...new Set(whole.map(e => `${e.pool}:${e.slot == null ? '?' : e.slot}`))].sort();
  const ticks = Array.from({length: 11}, (_, i) => i / 10);
  let html = `<div class="gantt-corner"></div><div class="axis">` +
    ticks.map(t => `<span class="tick" style="left:${t*100}%">${fmt((min - fullMin) + t*span)}</span>`).join('') + `</div>`;

  for (const laneKey of laneKeys) {
    const [pool, slot] = laneKey.split(':');
    const laneLabel = `${pool} #${slot}`;
    const laneEvents = whole.filter(e => (e.pool || '') === pool && (e.slot == null ? '?' : String(e.slot)) === slot && e.end >= min && e.start <= max)
      .sort((a, b) => a.start - b.start);
    html += `<div class="lane-label" title="${escapeAttr(laneLabel)}">${escapeXml(laneLabel)}</div><div class="lane-track">`;
    for (const e of laneEvents) {
      const clippedStart = Math.max(e.start, min);
      const clippedEnd = Math.min(e.end, max);
      const left = ((clippedStart - min) / span) * 100;
      const width = Math.max(((clippedEnd - clippedStart) / span) * 100, 0.0001);
      const label = labelOf(e);
      const kind = kindOf(e.node_id);
      const packageClass = packageClassByName.get(label);
      let baseTip = `${runLine}${fullLabelOf(e)}\npool: ${pool} #${slot}\nstart: ${fmt(e.start-min)}\nduration: ${fmt(e.end-e.start)}\nstatus: ${e.status || 'unknown'}`;
      const criticalWait = observedCriticalPath.waitedFor.get(e.node_id);
      if (observedCriticalPath.ids.has(e.node_id)) {
        baseTip += criticalWait
          ? `\nobserved critical path\nwaited for: ${nodeLabel(criticalWait.nodeId)}\nwaited node: ${criticalWait.nodeId}\nreason: ${criticalWait.reason}`
          : `\nobserved critical path`;
      }
      const phases = subByNode[e.node_id];
      // Segmented bar: a coloured segment per subphase when present, else a
      // single flat segment tinted by pool. Every hoverable chunk carries its
      // full info in `data-tooltip` — no native `title`, which would shadow the
      // styled popover — so hovering any subphase shows the full details.
      let barFill;
      if (phases && phases.length) {
        const bStart = clippedStart, bEnd = clippedEnd, bSpan = Math.max(bEnd - bStart, 1e-9);
        barFill = `<div class="seg-content">${escapeXml(label)}</div>` + phases
          .filter(s => s.end >= bStart && s.start <= bEnd)
          .map(s => {
          const sStart = Math.max(s.start, bStart), sEnd = Math.min(s.end, bEnd);
          const sl = ((sStart - bStart) / bSpan) * 100;
          const sw = Math.max(((sEnd - sStart) / bSpan) * 100, 0.0001);
          const col = SUBPHASE_COLORS[s.phase] || 'var(--cyan)';
          const segTip = `${runLine}${fullLabelOf(s)}\npool: ${s.pool} #${s.slot == null ? '?' : s.slot}\nstart: ${fmt(s.start - min)}\nduration: ${fmt(s.end - s.start)}\nstatus: ${s.status || 'unknown'}`;
          return `<div class="seg" data-phase="${escapeAttr(s.phase)}" style="left:${sl}%;width:${sw}%;background:${col}" data-tooltip="${escapeAttr(segTip)}"></div>`;
        }).join('');
      } else {
        const col = POOL_COLORS[pool] || 'var(--cyan)';
        // Flat segment gets the `seg-content` label treatment (dark text on the
        // bright pool tint) so its label stays readable.
        barFill = `<div class="seg seg-full seg-content" data-phase="" style="background:${col}">${escapeXml(label)}</div>`;
      }
      const criticalClass = observedCriticalPath.ids.has(e.node_id) ? ' critical' : '';
      html += `<div class="bar ${packageClass}${criticalClass}" data-kind="${escapeAttr(kind)}" data-pool="${escapeAttr(pool)}" data-slot="${escapeAttr(String(slot))}" data-status="${escapeAttr(e.status || '')}" data-tooltip="${escapeAttr(baseTip)}" aria-label="${escapeAttr(baseTip)}" style="left:${left}%;width:${width}%">${barFill}</div>`;
    }
    html += `</div>`;
  }
  el.innerHTML = html;
}

function currentFilteredEvents() {
  const filter = document.getElementById('filter').value.toLowerCase().trim();
  return events.filter(e => !filter ||
    [e.node_id, nodeLabel(e.node_id), nodeDetail(e.node_id), e.phase || '', e.pool || '', e.status, kindOf(e.node_id)]
      .join(' ').toLowerCase().includes(filter));
}
function filteredDomain() {
  const shown = currentFilteredEvents();
  if (!shown.length) return null;
  const min = Math.min(...shown.map(e => e.start));
  const max = Math.max(...shown.map(e => e.end));
  return {min, max, span: Math.max(max - min, 0.001)};
}
function clampGanttView(start, end, fullMin, fullMax) {
  const fullSpan = Math.max(fullMax - fullMin, 0.001);
  let span = Math.max(end - start, fullSpan / GANTT_MAX_SCALE);
  span = Math.min(span, fullSpan);
  if (start < fullMin) { end += fullMin - start; start = fullMin; }
  if (end > fullMax) { start -= end - fullMax; end = fullMax; }
  if (start < fullMin) start = fullMin;
  end = start + span;
  if (end > fullMax) { end = fullMax; start = end - span; }
  return {start, end};
}
function setScaleSelect(scale) {
  const select = document.getElementById('scale');
  const exact = [...select.options].find(o => o.value !== 'custom' && Math.abs(Number(o.value) - scale) < 1e-3);
  select.value = exact ? exact.value : 'custom';
}
function setGanttScale(nextScale, anchorClientX = null) {
  const domain = filteredDomain();
  if (!domain) return;
  cancelGanttPan();
  if (!ganttView) ganttView = {start: domain.min, end: domain.max};
  ganttView = clampGanttView(ganttView.start, ganttView.end, domain.min, domain.max);

  const rect = (document.querySelector('#gantt .axis') || document.getElementById('ganttWrap')).getBoundingClientRect();
  const anchorX = anchorClientX == null ? rect.left + rect.width / 2 : anchorClientX;
  const ratio = Math.max(0, Math.min(1, (anchorX - rect.left) / Math.max(rect.width, 1)));
  const oldSpan = ganttView.end - ganttView.start;
  const anchorTime = ganttView.start + ratio * oldSpan;
  const scale = Math.max(GANTT_MIN_SCALE, Math.min(GANTT_MAX_SCALE, nextScale));
  const newSpan = domain.span / scale;
  ganttView = clampGanttView(anchorTime - ratio * newSpan, anchorTime + (1 - ratio) * newSpan, domain.min, domain.max);
  setScaleSelect(scale);
  renderGantt();
  attachGanttTooltips();
  refreshGanttPanCursor();
}
function currentGanttScale() {
  const domain = filteredDomain();
  if (!domain || !ganttView) return 1;
  return domain.span / Math.max(ganttView.end - ganttView.start, domain.span / GANTT_MAX_SCALE);
}
// The gantt keeps the window (zoom) model: the visible time range is always
// mapped to the container width, so the scroll container never overflows.
// Panning therefore moves the window itself rather than scrolling pixels.
// `atGanttFit` matches the rendered state — at fit the window covers the whole
// run and panning is a no-op, exactly like the (previously missing) scrollbar
// would have nothing to scroll.
function atGanttFit() {
  const domain = filteredDomain();
  return !domain || !ganttView || (ganttView.end - ganttView.start) >= domain.span - 1e-6;
}
function refreshGanttPanCursor() {
  const wrap = document.getElementById('ganttWrap');
  wrap.style.cursor = atGanttFit() ? '' : 'grab';
}
function setupGanttInteractions() {
  const wrap = document.getElementById('ganttWrap');
  const pinch = {pointers: new Map(), distance: 0, centerX: 0};
  // Pan state: a single-pointer gesture (mouse or touch) that drags the time
  // window, plus release inertia (fling). Handed off to pinch zoom when a
  // second pointer joins.
  const pan = {
    active: false, pointerId: null,
    startClientX: 0, lastClientX: 0, startView: null,
    samples: [], moveRaf: 0, inertiaRaf: 0, velocity: 0,
  };
  let wheelQueuedPx = 0;
  let wheelRaf = 0;
  const trackWidthPx = () => {
    const track = document.querySelector('#gantt .axis');
    return Math.max(track ? track.getBoundingClientRect().width : 1, 1);
  };
  // Core pan primitive: shift the CURRENT window by signed seconds, clamped to
  // the filtered domain, and re-render. False at fit or when pinned at a
  // domain edge.
  const shiftGanttWindow = dtSeconds => {
    const domain = filteredDomain();
    if (!domain || !ganttView) return false;
    const span = Math.max(ganttView.end - ganttView.start, 0.001);
    if (span >= domain.span - 1e-6) return false;
    const next = clampGanttView(ganttView.start + dtSeconds, ganttView.end + dtSeconds, domain.min, domain.max);
    if (next.start === ganttView.start && next.end === ganttView.end) return false;
    ganttView = next;
    renderGantt();
    attachGanttTooltips();
    return true;
  };
  // Drag mapping: content follows the cursor, so dragging right reveals
  // earlier times (dt negative for positive dx), computed against the window
  // captured at drag start so the gesture stays drift-free.
  const applyDragPan = () => {
    if (!pan.startView || !ganttView) return;
    const domain = filteredDomain();
    if (!domain) return;
    const span = Math.max(pan.startView.end - pan.startView.start, 0.001);
    const dt = (-(pan.lastClientX - pan.startClientX) / trackWidthPx()) * span;
    const next = clampGanttView(pan.startView.start + dt, pan.startView.end + dt, domain.min, domain.max);
    if (next.start === ganttView.start && next.end === ganttView.end) return;
    ganttView = next;
    renderGantt();
    attachGanttTooltips();
  };
  const stopAnimations = () => {
    if (pan.inertiaRaf) { cancelAnimationFrame(pan.inertiaRaf); pan.inertiaRaf = 0; }
    if (pan.moveRaf) { cancelAnimationFrame(pan.moveRaf); pan.moveRaf = 0; }
    if (wheelRaf) { cancelAnimationFrame(wheelRaf); wheelRaf = 0; }
    wheelQueuedPx = 0;
  };
  const stopPan = () => {
    stopAnimations();
    pan.active = false;
    pan.pointerId = null;
    wrap.classList.remove('panning');
  };
  cancelGanttPan = stopPan;
  // Release inertia: keep panning with the flick velocity under exponential
  // friction (~180ms half-life) until it dies out or the window pins at a
  // domain edge. Velocity is px/ms; rightward flick keeps pushing earlier,
  // matching the drag direction.
  const startInertia = () => {
    let lastT = performance.now();
    const step = now => {
      pan.inertiaRaf = 0;
      const dtMs = Math.min(Math.max(now - lastT, 1), 64);
      lastT = now;
      const span = ganttView ? Math.max(ganttView.end - ganttView.start, 0.001) : 1;
      const moved = shiftGanttWindow(-(pan.velocity * dtMs / trackWidthPx()) * span);
      pan.velocity *= Math.exp(-dtMs / 180);
      if (moved && Math.abs(pan.velocity) > 0.02) {
        pan.inertiaRaf = requestAnimationFrame(step);
      }
    };
    pan.inertiaRaf = requestAnimationFrame(step);
  };
  const beginPan = event => {
    if (pan.active) return;
    stopPan();
    pan.active = true;
    pan.pointerId = event.pointerId;
    pan.startClientX = event.clientX;
    pan.lastClientX = event.clientX;
    pan.startView = ganttView ? {start: ganttView.start, end: ganttView.end} : null;
    pan.samples = [];
    wrap.classList.add('panning');
  };
  const endPan = (event, allowInertia) => {
    if (!pan.active || event.pointerId !== pan.pointerId) return;
    pan.active = false;
    pan.pointerId = null;
    wrap.classList.remove('panning');
    if (pan.moveRaf) { cancelAnimationFrame(pan.moveRaf); pan.moveRaf = 0; }
    // Flick velocity from the most recent drag samples (last 100ms).
    const now = performance.now();
    const recent = pan.samples.filter(p => now - p.t < 100);
    pan.samples = [];
    pan.velocity = 0;
    if (allowInertia && recent.length >= 2) {
      const first = recent[0], last = recent[recent.length - 1];
      pan.velocity = Math.max(-4, Math.min(4, (last.x - first.x) / Math.max(last.t - first.t, 1)));
    }
    if (Math.abs(pan.velocity) > 0.05) startInertia();
  };

  wrap.addEventListener('wheel', event => {
    if (event.ctrlKey || event.metaKey) {
      event.preventDefault();
      setGanttScale(currentGanttScale() * Math.exp(-event.deltaY * 0.018), event.clientX);
      return;
    }
    // Trackpad horizontal swipe (deltaX) or shift+wheel pans the time window;
    // plain vertical wheel keeps scrolling the lanes. Positive deltaX means
    // "scroll right" — later times — exactly like a native horizontal
    // scrollbar (dragging is the inverse: content follows the cursor).
    // Trackpad momentum arrives as a burst of further deltaX events, so they
    // are coalesced into one render per animation frame to keep the fling
    // smooth. At fit this is a no-op.
    const dx = event.shiftKey && !event.deltaX ? event.deltaY : event.deltaX;
    if (Math.abs(dx) < 0.5) return;
    event.preventDefault();
    wheelQueuedPx += dx;
    if (!wheelRaf) {
      wheelRaf = requestAnimationFrame(() => {
        wheelRaf = 0;
        if (!wheelQueuedPx || !ganttView) return;
        const span = Math.max(ganttView.end - ganttView.start, 0.001);
        const dt = (wheelQueuedPx / trackWidthPx()) * span;
        wheelQueuedPx = 0;
        shiftGanttWindow(dt);
      });
    }
  }, {passive: false});

  wrap.addEventListener('pointerdown', event => {
    if (event.pointerType === 'mouse' && event.button !== 0) return;
    if (event.ctrlKey || event.metaKey) return;
    wrap.setPointerCapture(event.pointerId);
    pinch.pointers.set(event.pointerId, {x: event.clientX, y: event.clientY});
    if (pinch.pointers.size > 1) {
      // Second pointer joins: hand off from pan to pinch zoom.
      stopPan();
      return;
    }
    stopPan(); // grab always cancels a running fling
    if (!atGanttFit()) beginPan(event);
  });
  wrap.addEventListener('pointermove', event => {
    if (!pinch.pointers.has(event.pointerId)) return;
    pinch.pointers.set(event.pointerId, {x: event.clientX, y: event.clientY});
    if (pan.active && event.pointerId === pan.pointerId) {
      pan.lastClientX = event.clientX;
      pan.samples.push({t: performance.now(), x: event.clientX});
      if (pan.samples.length > 6) pan.samples.shift();
      // Coalesce moves into one render per animation frame.
      if (!pan.moveRaf) {
        pan.moveRaf = requestAnimationFrame(() => {
          pan.moveRaf = 0;
          if (pan.active) applyDragPan();
        });
      }
      return;
    }
    const pts = [...pinch.pointers.values()];
    if (pts.length < 2) return;
    const [a, b] = pts;
    const distance = Math.hypot(a.x - b.x, a.y - b.y);
    const centerX = (a.x + b.x) / 2;
    if (pinch.distance) setGanttScale(currentGanttScale() * Math.pow(distance / pinch.distance, 6), centerX);
    pinch.distance = distance;
    pinch.centerX = centerX;
  });
  for (const type of ['pointerup', 'pointercancel', 'pointerleave']) {
    wrap.addEventListener(type, event => {
      pinch.pointers.delete(event.pointerId);
      if (pinch.pointers.size < 2) pinch.distance = 0;
      // Inertia only on intentional release, not on cancel/leave.
      endPan(event, type === 'pointerup');
    });
  }
  wrap.addEventListener('keydown', event => {
    if (event.key !== 'ArrowLeft' && event.key !== 'ArrowRight' && event.key !== 'Home' && event.key !== 'End') return;
    const domain = filteredDomain();
    if (!domain || !ganttView) return;
    stopPan();
    const span = Math.max(ganttView.end - ganttView.start, 0.001);
    if (event.key === 'Home' || event.key === 'End') {
      // Jump the window to the run's start (left edge) or end (right edge).
      const start = event.key === 'Home' ? domain.min : Math.max(domain.min, domain.max - span);
      ganttView = clampGanttView(start, start + span, domain.min, domain.max);
    } else {
      const dt = (event.key === 'ArrowRight' ? 1 : -1) * span * 0.2;
      ganttView = clampGanttView(ganttView.start + dt, ganttView.end + dt, domain.min, domain.max);
    }
    event.preventDefault();
    renderGantt();
    attachGanttTooltips();
  });
}

function flowEdges() {
  const mode = document.getElementById('edgeMode')?.value || 'flow';
  if (mode === 'all') return edges;

  // These registry fan-in/fan-out constraints are scheduler bookkeeping, not a
  // readable flow chart. Keeping them produces the grey edge wall in large traces.
  const noisy = new Set([
    'registry_after_all_missing_committed',
  ]);
  return edges.filter(e => !noisy.has(e.reason));
}

function truncateGraphLabel(text, max = 22) {
  text = String(text || '');
  return text.length > max ? `${text.slice(0, max - 1)}…` : text;
}

function edgePath(points) {
  if (!points?.length) return '';
  return points.map((p, i) => `${i ? 'L' : 'M'}${p.x},${p.y}`).join(' ');
}

function renderGraph() {
  const svg = document.getElementById('graphSvg');
  if (!nodes.length) { svg.outerHTML = '<div class="empty">No graph nodes.</div>'; return; }
  if (!window.dagre) {
    document.getElementById('graphHint').textContent = 'embedded dagre layout failed to load';
    return;
  }

  const graphEdges = flowEdges();
  const nodeW = 168, nodeH = 48;
  const g = new dagre.graphlib.Graph({multigraph: true, compound: false});
  g.setGraph({
    rankdir: 'LR',
    ranker: 'network-simplex',
    acyclicer: 'greedy',
    nodesep: 34,
    edgesep: 12,
    ranksep: 95,
    marginx: 36,
    marginy: 36,
  });
  g.setDefaultEdgeLabel(() => ({}));

  for (const n of nodes) g.setNode(n.id, {width: nodeW, height: nodeH});
  for (const [i, e] of graphEdges.entries()) {
    if (nodeById.has(e.from) && nodeById.has(e.to)) g.setEdge(e.from, e.to, {reason: e.reason || ''}, `e${i}`);
  }

  dagre.layout(g);
  const width = Math.max(960, Math.ceil(g.graph().width || 960));
  const height = Math.max(420, Math.ceil(g.graph().height || 420));
  svg.setAttribute('viewBox', `0 0 ${width} ${height}`);
  svg.style.height = `${Math.min(height, 1200)}px`;

  const showEdgeLabels = graphEdges.length <= 160;
  let html = `<defs><marker id="arrow" markerWidth="8" markerHeight="8" refX="7" refY="4" orient="auto"><path d="M0,0 L8,4 L0,8 z" fill="var(--edge)"/></marker></defs><g id="graphViewport">`;

  for (const edgeObj of g.edges()) {
    const e = g.edge(edgeObj);
    const d = edgePath(e.points);
    html += `<path class="edge" d="${d}"><title>${escapeXml(e.reason || '')}</title></path>`;
    if (showEdgeLabels && e.points?.length) {
      const p = e.points[Math.floor(e.points.length / 2)];
      html += `<text class="edge-label" x="${p.x}" y="${p.y - 4}" text-anchor="middle">${escapeXml(e.reason || '')}</text>`;
    }
  }

  for (const n of nodes) {
    const p = g.node(n.id);
    if (!p) continue;
    const primary = nodeLabel(n.id);
    html += `<g class="node" transform="translate(${p.x - nodeW / 2},${p.y - nodeH / 2})"><title>${escapeXml(nodeDetail(n.id))}</title><rect width="${nodeW}" height="${nodeH}"/>`;
    html += `<text class="kind" x="10" y="17">${escapeXml(truncateGraphLabel(n.kind || '', 24))}</text>`;
    html += `<text x="10" y="35">${escapeXml(truncateGraphLabel(primary, 24))}</text></g>`;
  }
  html += `</g>`;
  svg.innerHTML = html;
  resetGraphView();
  document.getElementById('graphHint').textContent = `dagre layout · ${nodes.length} nodes · ${graphEdges.length}/${edges.length} edges · wheel/pinch to zoom · drag to pan`;
}

const tooltip = document.getElementById('tooltip');
const packageHighlightStyle = document.createElement('style');
document.head.appendChild(packageHighlightStyle);
let highlightedPackageClass = '';
let tooltipTimer = 0;
function attachGanttTooltips() {
  const gantt = document.getElementById('gantt');
  const wrap = document.getElementById('ganttWrap');
  let activeTarget = null;

  gantt.onmouseover = event => {
    // While an active drag pan is captured on the wrap, suppress the hover
    // popover so it does not chase the pointer across bars.
    if (wrap.classList.contains('panning')) {
      clearTimeout(tooltipTimer);
      tooltip.classList.remove('visible');
      activeTarget = null;
      return;
    }
    // Resolve the deepest info-bearing element: a subphase `.seg` when the
    // pointer is over one of its segments, otherwise the parent `.bar`.
    const target = event.target.closest?.('[data-tooltip]');
    if (!target || !gantt.contains(target) || target === activeTarget) return;
    activeTarget = target;
    highlightPackageClass(gantt, (target.closest('.bar') || {}).dataset?.packageClass || '');

    clearTimeout(tooltipTimer);
    tooltip.classList.remove('visible');
    tooltipTimer = setTimeout(() => {
      if (activeTarget !== target) return;
      tooltip.textContent = target.dataset.tooltip || '';
      tooltip.classList.add('visible');
      moveTooltip(event);
    }, 120);
  };
  gantt.onmousemove = event => {
    if (activeTarget && tooltip.classList.contains('visible')) moveTooltip(event);
  };
  gantt.onmouseout = event => {
    if (!activeTarget) return;
    const nextTarget = event.relatedTarget?.closest?.('[data-tooltip]');
    if (nextTarget === activeTarget) return;
    activeTarget = null;
    clearTimeout(tooltipTimer);
    clearPackageHighlight(gantt);
    tooltip.classList.remove('visible');
  };
}
function highlightPackageClass(gantt, packageClass) {
  if (packageClass === highlightedPackageClass) return;
  if (highlightedPackageClass) gantt.classList.remove(highlightedPackageClass);
  highlightedPackageClass = packageClass;
  if (highlightedPackageClass) gantt.classList.add(highlightedPackageClass);
}
function clearPackageHighlight(gantt) {
  if (highlightedPackageClass) gantt.classList.remove(highlightedPackageClass);
  highlightedPackageClass = '';
}
function moveTooltip(event) {
  const pad = 14;
  const rect = tooltip.getBoundingClientRect();
  let x = event.clientX + 12;
  let y = event.clientY + 12;
  if (x + rect.width + pad > window.innerWidth) x = event.clientX - rect.width - 12;
  if (y + rect.height + pad > window.innerHeight) y = event.clientY - rect.height - 12;
  tooltip.style.left = `${Math.max(pad, x)}px`;
  tooltip.style.top = `${Math.max(pad, y)}px`;
}

const GRAPH_MIN_ZOOM = 0.18;
const GRAPH_MAX_ZOOM = 256;
const graphState = {zoom: 1, panX: 0, panY: 0, pointers: new Map(), lastPinchDistance: 0};
let graphFrame = 0;
function paintGraphTransform() {
  const viewport = document.getElementById('graphViewport');
  if (viewport) viewport.setAttribute('transform', `translate(${graphState.panX} ${graphState.panY}) scale(${graphState.zoom})`);
  const hint = document.getElementById('graphHint');
  const text = `zoom ${Math.round(graphState.zoom * 100)}% · wheel/pinch to zoom · drag to pan`;
  if (hint.textContent !== text) hint.textContent = text;
}
function applyGraphTransform() {
  // Track every input delta, but only mutate the SVG once per display frame.
  // The root SVG coordinate system stays fixed while its inner group moves.
  if (graphFrame) return;
  graphFrame = requestAnimationFrame(() => {
    graphFrame = 0;
    paintGraphTransform();
  });
}
function resetGraphView() {
  cancelAnimationFrame(graphFrame);
  graphFrame = 0;
  graphState.zoom = 1;
  graphState.panX = 0;
  graphState.panY = 0;
  graphState.pointers.clear();
  graphState.lastPinchDistance = 0;
  paintGraphTransform();
}
function svgPoint(svg, clientX, clientY) {
  const point = svg.createSVGPoint();
  point.x = clientX;
  point.y = clientY;
  return point.matrixTransform(svg.getScreenCTM().inverse());
}
function zoomGraphAt(svg, clientX, clientY, factor) {
  const p = svgPoint(svg, clientX, clientY);
  const beforeX = (p.x - graphState.panX) / graphState.zoom;
  const beforeY = (p.y - graphState.panY) / graphState.zoom;
  graphState.zoom = Math.max(GRAPH_MIN_ZOOM, Math.min(GRAPH_MAX_ZOOM, graphState.zoom * factor));
  graphState.panX = p.x - beforeX * graphState.zoom;
  graphState.panY = p.y - beforeY * graphState.zoom;
  applyGraphTransform();
}
function setupGraphInteractions() {
  const svg = document.getElementById('graphSvg');
  if (!svg) return;
  svg.addEventListener('wheel', event => {
    event.preventDefault();
    zoomGraphAt(svg, event.clientX, event.clientY, Math.exp(-event.deltaY * 0.0015));
  }, {passive: false});
  svg.addEventListener('pointerdown', event => {
    svg.setPointerCapture(event.pointerId);
    graphState.pointers.set(event.pointerId, {x: event.clientX, y: event.clientY});
    svg.classList.add('panning');
  });
  svg.addEventListener('pointermove', event => {
    const prev = graphState.pointers.get(event.pointerId);
    if (!prev) return;
    if (graphState.pointers.size === 1) {
      const now = svgPoint(svg, event.clientX, event.clientY);
      const old = svgPoint(svg, prev.x, prev.y);
      graphState.panX += now.x - old.x;
      graphState.panY += now.y - old.y;
      graphState.pointers.set(event.pointerId, {x: event.clientX, y: event.clientY});
      applyGraphTransform();
      return;
    }
    graphState.pointers.set(event.pointerId, {x: event.clientX, y: event.clientY});
    const pts = [...graphState.pointers.values()];
    if (pts.length >= 2) {
      const [a, b] = pts;
      const dist = Math.hypot(a.x - b.x, a.y - b.y);
      const centerX = (a.x + b.x) / 2;
      const centerY = (a.y + b.y) / 2;
      if (graphState.lastPinchDistance) zoomGraphAt(svg, centerX, centerY, dist / graphState.lastPinchDistance);
      graphState.lastPinchDistance = dist;
    }
  });
  for (const type of ['pointerup', 'pointercancel', 'pointerleave']) {
    svg.addEventListener(type, event => {
      graphState.pointers.delete(event.pointerId);
      if (graphState.pointers.size < 2) graphState.lastPinchDistance = 0;
      if (!graphState.pointers.size) svg.classList.remove('panning');
    });
  }
  document.getElementById('resetGraph').addEventListener('click', resetGraphView);
}

function escapeXml(s) {
  return String(s).replace(/[&<>"']/g, ch => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[ch]));
}
function escapeAttr(s) {
  return escapeXml(s).replace(/`/g, '&#96;');
}

summarize();
renderGantt();
renderGraph();
setupGraphInteractions();
setupGanttInteractions();
attachGanttTooltips();
refreshGanttPanCursor();
document.getElementById('filter').addEventListener('input', () => { cancelGanttPan(); ganttView = null; setScaleSelect(1); renderGantt(); attachGanttTooltips(); refreshGanttPanCursor(); });
document.getElementById('scale').addEventListener('change', event => {
  if (event.target.value === 'custom') return;
  setGanttScale(Number(event.target.value || 1));
});
document.getElementById('edgeMode').addEventListener('change', renderGraph);

// Tabbed Timeline/Flow panes: both views are rendered once on load, so
// switching tabs only toggles visibility — no re-layout required.
const tabsEl = document.getElementById('tabs');
tabsEl.addEventListener('click', event => {
  const tab = event.target.closest('.tab');
  if (!tab || !tabsEl.contains(tab) || tab.classList.contains('active')) return;
  for (const t of tabsEl.querySelectorAll('.tab')) {
    const active = t === tab;
    t.classList.toggle('active', active);
    t.setAttribute('aria-selected', String(active));
  }
  for (const pane of document.querySelectorAll('.pane')) {
    const active = pane.id === (tab.dataset.pane || '');
    pane.classList.toggle('active', active);
    pane.setAttribute('aria-hidden', String(!active));
  }
});
tabsEl.addEventListener('keydown', event => {
  if (event.key !== 'ArrowLeft' && event.key !== 'ArrowRight') return;
  const tabs = [...tabsEl.querySelectorAll('.tab')];
  const idx = tabs.indexOf(document.activeElement);
  if (idx < 0) return;
  event.preventDefault();
  const next = tabs[(idx + (event.key === 'ArrowRight' ? 1 : -1) + tabs.length) % tabs.length];
  next.focus();
  next.click();
});
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::render_html;
    use serde_json::json;

    #[test]
    fn inlines_trace_json_and_escapes_close_script() {
        // A trace whose data contains a literal `</script>` payload must not
        // break out of the injected JSON block.
        let trace = json!({
            "plan": "node",
            "nodes": [],
            "edges": [],
            "events": [],
            "note": "</script><script>alert(1)</script>",
        });
        let html = render_html(&trace, "glu trace: x");
        let needle = r#"<script id="trace-data" type="application/json">"#;
        let start = html.find(needle).expect("trace data script");
        let after = &html[start + needle.len()..];
        let end = after.find("</script>").expect("script closes");
        let injected = &after[..end];
        assert!(
            !injected.contains("</script>"),
            "raw close-script must not appear in injected JSON"
        );
        assert!(injected.contains("<\\/script>"));
    }

    #[test]
    fn title_is_escaped() {
        let trace = json!({ "plan": "x", "nodes": [], "edges": [], "events": [] });
        let html = render_html(&trace, r#"a <b> & "c""#);
        let title_block = &html[html.find("<title>").unwrap()..html.find("</title>").unwrap()];
        assert!(!title_block.contains("\"a <b>"));
    }

    #[test]
    fn default_brand_injected() {
        let trace = json!({ "plan": "vips", "nodes": [], "edges": [], "events": [] });
        let html = render_html(&trace, "glu trace: trace-vips.json");
        assert!(html.contains("glu trace"));
    }

    #[test]
    fn viewer_is_self_contained_and_blocks_network_access() {
        let trace = json!({ "plan": "vips", "nodes": [], "edges": [], "events": [] });
        let html = render_html(&trace, "glu trace: vips");
        assert!(!html.contains("<script src="));
        assert!(!html.contains("@import"));
        assert!(!html.contains("<link"));
        assert!(!html.contains("unpkg.com"));
        assert!(!html.contains("__DAGRE_JS__"));
        assert!(html.contains("dagre.graphlib.Graph"));
        assert!(html.contains("connect-src 'none'"));
        assert!(!html.contains("script-src 'unsafe-inline'"));
        assert!(!html.contains("__CSP_NONCE__"));

        let nonce_start = html.find("script-src 'nonce-").unwrap() + "script-src 'nonce-".len();
        let nonce = &html[nonce_start..nonce_start + 32];
        assert!(nonce.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(html.matches(nonce).count(), 3);

        let second = render_html(&trace, "glu trace: vips");
        assert!(
            !second.contains(nonce),
            "each viewer gets a fresh CSP nonce"
        );
    }

    #[test]
    fn summary_uses_text_nodes_for_trace_values() {
        let trace = json!({
            "plan": "vips",
            "started_at": "<img src=x onerror=alert(1)>",
            "nodes": [],
            "edges": [],
            "events": [],
        });
        let html = render_html(&trace, "glu trace: vips");
        assert!(html.contains("strong.textContent = String(value)"));
        assert!(!html.contains("summary').innerHTML"));
    }

    #[test]
    fn viewer_renders_writer_wait_in_muted_grey() {
        let trace = json!({ "plan": "vips", "nodes": [], "edges": [], "events": [] });
        let html = render_html(&trace, "glu trace: vips");
        assert!(html.contains("writer_wait: 'var(--neutral)'"));
    }

    #[test]
    fn viewer_uses_live_system_theme_tokens() {
        let trace = json!({ "plan": "vips", "nodes": [], "edges": [], "events": [] });
        let html = render_html(&trace, "glu trace: vips");
        assert!(html.contains("@media (prefers-color-scheme: dark)"));
        assert!(html.contains("color-scheme: light;"));
        assert!(html.contains("color-scheme: dark;"));
        assert!(html.contains("--bg: #f3f5f2;"));
        assert!(html.contains("--bg: #151a1e;"));
        assert!(html.contains("--lime: #b5ff3d;"));
        assert!(html.contains("#arrow path { fill: var(--edge); }"));
        assert!(html.contains("color: var(--error-ink)"));

        // Generated inline fills must reference live CSS variables, not a
        // palette captured at load time that goes stale on a theme change.
        let palette =
            &html[html.find("const SUBPHASE_COLORS").unwrap()..html.find("let ganttView").unwrap()];
        assert!(!palette.contains('#'));
        assert!(palette.contains("commit: 'var(--commit)'"));
        assert!(html.contains("SUBPHASE_COLORS[s.phase] || 'var(--cyan)'"));
        assert!(html.contains("POOL_COLORS[pool] || 'var(--cyan)'"));
    }

    #[test]
    fn graph_supports_deep_zoom_and_frame_batched_updates() {
        let trace = json!({ "plan": "vips", "nodes": [], "edges": [], "events": [] });
        let html = render_html(&trace, "glu trace: vips");
        assert!(html.contains("const GRAPH_MAX_ZOOM = 256;"));
        assert!(html.contains("Math.min(GRAPH_MAX_ZOOM, graphState.zoom * factor)"));
        assert!(html.contains("if (graphFrame) return;"));
        assert!(html.contains("graphFrame = requestAnimationFrame("));
        assert!(html.contains("cancelAnimationFrame(graphFrame)"));
    }

    #[test]
    fn viewer_has_human_cache_postinstall_labels() {
        let trace = json!({
            "plan": "gstreamer",
            "nodes": [{
                "id": "cache_postinstall:gtk_query_immodules_3:%2Fopt%2Fglustore%2Flib%2Fgtk-3.0%2F3.0.0%2Fimmodules.cache",
                "kind": "cache_postinstall",
                "formula": null,
                "label": "GTK 3 input module cache",
                "inputs": {
                    "kind": "gtk_query_immodules_3",
                    "key": ["/opt/glustore/lib/gtk-3.0/3.0.0/immodules.cache"]
                }
            }],
            "edges": [],
            "events": [],
        });
        let html = render_html(&trace, "glu trace: gstreamer");
        assert!(html.contains("GTK 3 input module cache"));
    }
}
