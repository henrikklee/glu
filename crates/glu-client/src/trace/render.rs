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
//! Rendering is local-first: the HTML is written to a temp path and opened by
//! the CLI; trace data is never uploaded. The only runtime external is the
//! `dagre` layout library loaded from unpkg (network is not required to view;
//! the graph view degrades with a hint when it fails to load).

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

    HTML_TEMPLATE
        .replace("__TITLE__", &html_attr(&page_title))
        .replace("__TRACE_JSON__", &safe_trace_json)
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

/// Static single-file viewer template. Two placeholders:
///   __TITLE__       -> injected, HTML-attr escaped
///   __TRACE_JSON__  -> the trace, with `</` escaped to `<\\/`
const HTML_TEMPLATE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>__TITLE__</title>
<script src="https://unpkg.com/dagre@0.8.5/dist/dagre.min.js"></script>
<style>
:root {
  color-scheme: dark;
  --bg: #0b0f12;
  --panel: #12181d;
  --panel-2: #171f25;
  --ink: #e8efe7;
  --muted: #8a9894;
  --rule: #263139;
  --acid: #c7ff47;
  --cyan: #53d6ff;
  --orange: #ff9f43;
  --red: #ff5f6d;
  --green: #35d07f;
  --violet: #a98bff;
  --shadow: rgba(0,0,0,.4);
}
* { box-sizing: border-box; }
body {
  margin: 0;
  height: 100vh;
  height: 100dvh;
  display: flex;
  flex-direction: column;
  overflow: hidden;
  background:
    radial-gradient(circle at 15% -10%, rgba(199,255,71,.12), transparent 30rem),
    radial-gradient(circle at 90% 5%, rgba(83,214,255,.09), transparent 28rem),
    linear-gradient(180deg, #0b0f12, #090c0e 45rem);
  color: var(--ink);
  font: 13px/1.45 ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
}
header {
  position: sticky;
  top: 0;
  z-index: 5;
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 1rem;
  padding: 16px 20px;
  border-bottom: 1px solid var(--rule);
  background: rgba(11,15,18,.86);
  backdrop-filter: blur(14px);
  flex: none;
}
h1 { margin: 0; font-size: 18px; letter-spacing: -.02em; }
#summary { margin-top: 10px; }
.pill-row { display: flex; flex-wrap: wrap; gap: 8px; align-items: center; }
.pill {
  border: 1px solid var(--rule);
  background: rgba(255,255,255,.035);
  border-radius: 999px;
  padding: 5px 9px;
  color: var(--muted);
}
.pill strong { color: var(--ink); font-weight: 650; }
main {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  gap: 14px;
  padding: 0 18px 16px;
}
.tabs { display: flex; gap: 8px; flex: none; padding-top: 14px; }
.tab {
  border: 1px solid var(--rule);
  background: #0d1216;
  color: var(--muted);
  border-radius: 10px;
  padding: 7px 12px;
  font: inherit;
  cursor: pointer;
}
.tab:hover { border-color: var(--acid); }
.tab.active {
  color: var(--ink);
  border-color: rgba(199,255,71,.6);
  background: rgba(199,255,71,.08);
}
.card {
  overflow: hidden;
  border: 1px solid var(--rule);
  background: linear-gradient(180deg, rgba(23,31,37,.94), rgba(18,24,29,.94));
  border-radius: 18px;
  box-shadow: 0 18px 60px var(--shadow);
}
.pane { display: none; flex-direction: column; flex: 1; min-height: 0; }
.pane.active { display: flex; }
.pane-body { flex: 1; min-height: 0; }
.card-head {
  display: flex;
  justify-content: space-between;
  gap: 1rem;
  align-items: center;
  padding: 14px 16px;
  border-bottom: 1px solid var(--rule);
}
h2 { margin: 0; font-size: 14px; text-transform: uppercase; letter-spacing: .12em; color: var(--acid); }
.controls { display: flex; gap: 10px; flex-wrap: wrap; align-items: center; color: var(--muted); }
input, select, button {
  border: 1px solid var(--rule);
  background: #0d1216;
  color: var(--ink);
  border-radius: 10px;
  padding: 7px 9px;
  font: inherit;
}
button { cursor: pointer; }
button:hover { border-color: var(--acid); }
#ganttWrap { flex: 1; min-height: 0; overflow: auto; touch-action: pan-y; }
/* Drag-pan affordances: grabbed cursor + no text selection while the gantt is
   being dragged, and a visible focus ring so keyboard panning (arrows) has an
   obvious target. */
#ganttWrap.panning { cursor: grabbing; user-select: none; -webkit-user-select: none; }
#ganttWrap:focus-visible { outline: 2px solid rgba(199,255,71,.45); outline-offset: -2px; }
.gantt {
  position: relative;
  min-width: 920px;
  margin: 14px 16px 20px;
  display: grid;
  grid-template-columns: 210px 1fr;
  gap: 0;
}
.axis, .lane-label, .lane-track, .gantt-corner { border-bottom: 1px solid rgba(255,255,255,.065); }
.gantt-corner {
  grid-column: 1;
  height: 30px;
  position: sticky;
  left: 0;
  top: 0;
  background: var(--panel);
  z-index: 5;
}
.axis {
  grid-column: 2;
  height: 30px;
  position: sticky;
  top: 0;
  background: var(--panel);
  z-index: 2;
}
.tick { position: absolute; top: 0; bottom: 0; border-left: 1px solid rgba(255,255,255,.08); color: var(--muted); font-size: 11px; padding-left: 4px; }
/* The 100% tick's label must end at the axis edge, not overflow past it
   (which otherwise forces a spurious horizontal scrollbar). */
.tick:last-child { left: auto !important; right: 0; text-align: right; padding-left: 0; padding-right: 6px; border-left: none; }
.lane-label {
  grid-column: 1;
  height: 38px;
  display: flex;
  align-items: center;
  padding-right: 12px;
  color: #b9c4c0;
  white-space: nowrap;
  position: sticky;
  left: 0;
  z-index: 4;
  background: var(--panel);
}
.lane-track {
  grid-column: 2;
  height: 38px;
  position: relative;
  overflow: hidden;
  background-image: linear-gradient(90deg, rgba(255,255,255,.035) 1px, transparent 1px);
  background-size: 10% 100%;
}
.bar {
  position: absolute;
  top: 7px;
  height: 24px;
  min-width: 1px;
  border-radius: 7px;
  box-shadow: 0 0 0 1px rgba(255,255,255,.15) inset, 0 6px 16px rgba(0,0,0,.25);
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
  line-height: 24px;
  padding: 0 7px;
  color: #071014;
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
/* Bars forced to the red error background need the dark label flipped to light. */
.bar[data-status="error"] .seg-content, .bar[data-status="failed"] .seg-content { color: var(--ink); }
.bar.critical::before {
  content: '';
  position: absolute;
  left: 0;
  right: 0;
  top: 0;
  height: 3px;
  background: #ff2638;
  box-shadow: 0 0 10px rgba(255,38,56,.7);
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
  border: 1px solid rgba(199,255,71,.45);
  background: rgba(9,12,14,.96);
  color: var(--ink);
  border-radius: 12px;
  padding: 9px 10px;
  box-shadow: 0 16px 50px rgba(0,0,0,.5);
  white-space: pre-wrap;
  transition: opacity .08s ease;
}
#tooltip.visible { opacity: 1; }
#graphWrap { flex: 1; min-height: 0; overflow: auto; padding: 12px; display: flex; align-items: center; justify-content: center; }
#graphSvg { min-width: 960px; width: 100%; max-height: 100%; background: #0d1216; border-radius: 12px; touch-action: none; cursor: grab; }
#graphSvg.panning { cursor: grabbing; }
.node rect { fill: #18232b; stroke: #3a4851; stroke-width: 1.2; rx: 12; }
.node text { fill: var(--ink); font-size: 11px; }
.node .kind { fill: var(--acid); font-size: 10px; text-transform: uppercase; }
.edge { fill: none; stroke: #66747b; stroke-width: 1.2; marker-end: url(#arrow); }
.edge-label { fill: var(--muted); font-size: 9px; paint-order: stroke; stroke: #0d1216; stroke-width: 4px; }
.graph-grid { fill: none; stroke: rgba(255,255,255,.055); stroke-width: 1; stroke-dasharray: 3 7; }
.row-label { fill: var(--muted); font-size: 10px; paint-order: stroke; stroke: #0d1216; stroke-width: 4px; }
.empty { padding: 24px; color: var(--muted); }
footer { color: var(--muted); padding: 0 18px 18px; }
</style>
</head>
<body>
<header>
  <div>
    <h1 id="title">glu trace</h1>
    <div class="pill-row" id="summary"></div>
  </div>
  <div class="pill-row">
    <span class="pill">single-file viewer</span>
  </div>
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
<script>
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
  document.getElementById('summary').innerHTML = pills.map(([k,v]) => `<span class="pill">${k}: <strong>${v}</strong></span>`).join('');
}

// Subphase segment colours (the six `bottle_prepare` steps). Falls back to a
// per-pool tint for whole-node bars that have no subphase breakdown.
const SUBPHASE_COLORS = {
  extract: '#ff9f43',
  writer_wait: '#8a9894',
  text_relocate: '#53d6ff',
  fixed_prefix_relocate: '#53d6ff',
  macho_patch: '#53d6ff',
  codesign: '#a98bff',
};
const POOL_COLORS = {
  setup: '#8a9894',
  download: '#53d6ff',
  prepare: '#ff9f43',
  commit: '#c7ff47',
  postinstall: '#a98bff',
  registry: '#35d07f',
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
          const col = SUBPHASE_COLORS[s.phase] || '#53d6ff';
          const segTip = `${runLine}${fullLabelOf(s)}\npool: ${s.pool} #${s.slot == null ? '?' : s.slot}\nstart: ${fmt(s.start - min)}\nduration: ${fmt(s.end - s.start)}\nstatus: ${s.status || 'unknown'}`;
          return `<div class="seg" data-phase="${escapeAttr(s.phase)}" style="left:${sl}%;width:${sw}%;background:${col}" data-tooltip="${escapeAttr(segTip)}"></div>`;
        }).join('');
      } else {
        const col = POOL_COLORS[pool] || '#53d6ff';
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
    document.getElementById('graphHint').textContent = 'dagre failed to load; check network access for unpkg.com';
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
  let html = `<defs><marker id="arrow" markerWidth="8" markerHeight="8" refX="7" refY="4" orient="auto"><path d="M0,0 L8,4 L0,8 z" fill="#66747b"/></marker></defs><g id="graphViewport">`;

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

const graphState = {zoom: 1, panX: 0, panY: 0, pointers: new Map(), lastPinchDistance: 0};
function applyGraphTransform() {
  const viewport = document.getElementById('graphViewport');
  if (viewport) viewport.setAttribute('transform', `translate(${graphState.panX} ${graphState.panY}) scale(${graphState.zoom})`);
  document.getElementById('graphHint').textContent = `zoom ${Math.round(graphState.zoom * 100)}% · wheel/pinch to zoom · drag to pan`;
}
function resetGraphView() {
  graphState.zoom = 1;
  graphState.panX = 0;
  graphState.panY = 0;
  graphState.pointers.clear();
  graphState.lastPinchDistance = 0;
  applyGraphTransform();
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
  graphState.zoom = Math.max(0.18, Math.min(8, graphState.zoom * factor));
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
    fn viewer_renders_writer_wait_in_muted_grey() {
        let trace = json!({ "plan": "vips", "nodes": [], "edges": [], "events": [] });
        let html = render_html(&trace, "glu trace: vips");
        assert!(html.contains("writer_wait: '#8a9894'"));
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
