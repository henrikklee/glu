// Run with: node --test scripts/test-trace-package-count.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

// Execute the viewer's actual expression so this catches renderer regressions.
const source = readFileSync(new URL('../crates/glu-client/src/trace/render.rs', import.meta.url), 'utf8');
const expression = source.match(/const packageCount = ([\s\S]*?);/)[1];
const count = new Function('events', 'nodeById', `return ${expression};`);
const node = (id, package_id, formula, label = formula) => ({ id, package_id, formula, label });
const packages = [
  node('download:vips', 'pkg:vips@1', 'vips'),
  node('prepare:vips', 'pkg:vips@1', 'vips'),
  node('prepare:vips2', 'pkg:vips@2', 'vips'),
  node('prepare:legacy', null, 'legacy'),
];
const tasks = [
  node('ghcr_auth:install', null, null),
  node('cache_postinstall:pixbuf', null, null, 'gdk-pixbuf loader cache'),
  node('cache_postinstall:fonts', null, null, 'fontconfig cache'),
];

test('counts identities, deduplicates lifecycle events, supports legacy traces, excludes shared tasks', () => {
  const nodes = [...packages, ...tasks];
  assert.equal(count(nodes.map(n => ({ node_id: n.id })), new Map(nodes.map(n => [n.id, n]))), 3);
});

test('excludes unexecuted nodes, subphases and unknown nodes', () => {
  const nodes = new Map(packages.map(n => [n.id, n]));
  assert.equal(count([
    { node_id: 'download:vips', phase: null },
    { node_id: 'prepare:vips2', phase: 'extract' },
    { node_id: 'missing' },
  ], nodes), 1);
  assert.equal(count([], nodes), 0);
});
