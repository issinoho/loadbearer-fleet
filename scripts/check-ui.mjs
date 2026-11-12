/*
 * Renders the whole dashboard against a minimal DOM and asserts what came out.
 *
 * Two jobs.
 *
 * **Geometry.** The dataviz procedure ends with "render it and look at it" —
 * the palette validator checks colour, not layout. This is the part of looking
 * that can be automated: no NaN or undefined ever reaching an SVG attribute
 * (which silently drops a mark rather than erroring), nothing painted outside
 * its own viewBox, bars capped at the 24px spec, gridlines solid rather than
 * dashed, markers carrying their surface ring, hit targets big enough to hit,
 * and x-axis labels that fit the band they sit in instead of colliding.
 *
 * **That every view runs at all.** A thousand lines of view code that has never
 * executed is not finished code, and a mistyped field name in a browser is a
 * blank panel with an error in a console nobody has open. So each view is
 * rendered against a real snapshot — produced by the binary itself from the
 * anonymised fixtures, so it cannot drift from the API — and any throw fails
 * the check.
 *
 * Deliberately not part of `cargo test`: it needs Node, and neither the server
 * nor its CI does. Run it after touching the dashboard:
 *
 *     cargo build && node scripts/check-ui.mjs
 */

import { spawnSync } from 'node:child_process';
import { existsSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

/* ------------------------------------------------------------- the DOM shim */

class FakeNode {
  constructor(tag) {
    this.tagName = tag;
    this.attrs = {};
    this.children = [];
    this.style = { setProperty() {} };
    this.dataset = {};
    this.textContent = '';
    this.className = '';
    this.hidden = false;
  }

  setAttribute(k, v) { this.attrs[k] = v; }

  getAttribute(k) { return this.attrs[k]; }

  removeAttribute(k) { delete this.attrs[k]; }

  append(...kids) { this.children.push(...kids.filter(Boolean)); }

  replaceChildren(...kids) { this.children = kids.filter(Boolean); }

  addEventListener() {}

  querySelector() { return null; }

  querySelectorAll() { return []; }

  getBoundingClientRect() { return { left: 0, top: 0, right: 700, bottom: 300, width: 700, height: 300 }; }

  get clientWidth() { return this._w ?? 700; }

  set clientWidth(v) { this._w = v; }

  get isConnected() { return true; }
}

globalThis.document = {
  createElement: (t) => new FakeNode(t),
  createElementNS: (_ns, t) => new FakeNode(t),
  createTextNode: (t) => Object.assign(new FakeNode('#text'), { textContent: String(t) }),
  querySelector: () => null,
  querySelectorAll: () => [],
  documentElement: new FakeNode('html'),
  addEventListener() {},
};
globalThis.window = { addEventListener() {}, innerWidth: 1400, innerHeight: 900 };
// The table builder asks `cell instanceof Node` to tell an element from a
// string, so the shim's node class has to answer to that name.
globalThis.Node = FakeNode;
globalThis.getComputedStyle = () => ({ getPropertyValue: () => '#2a78d6' });
globalThis.localStorage = { getItem: () => null, setItem() {} };
// Synchronous, so a chart card draws during the render rather than after it
// and the audit below sees the marks.
globalThis.requestAnimationFrame = (fn) => fn();
globalThis.location = { hash: '' };
globalThis.fetch = () => Promise.reject(new Error('no network in the geometry check'));

/* ---------------------------------------------------------------- the walk */

function walk(node, fn, depth = 0) {
  fn(node, depth);
  for (const kid of node.children || []) walk(kid, fn, depth + 1);
}

function textOf(node) {
  let out = node.textContent || '';
  for (const kid of node.children || []) out += textOf(kid);
  return out;
}

const NUMERIC = new Set(['x', 'y', 'x1', 'x2', 'y1', 'y2', 'cx', 'cy', 'r', 'width', 'height',
  'stroke-width', 'font-size']);

const failures = [];
const notes = [];

function check(name, condition, detail) {
  if (!condition) failures.push(`${name}: ${detail}`);
}

function auditChart(name, host) {
  const root = host.children ? host.children.find((c) => c.tagName === 'svg') : null;
  check(name, !!root, 'no <svg> was produced');
  if (!root) return;
  auditSvg(name, root);
  auditAxisLabels(name, host);
}

/** Every <svg> anywhere under a rendered view. */
function auditTree(name, node) {
  let found = 0;
  walk(node, (n) => {
    if (n.tagName === 'svg') { found += 1; auditSvg(`${name}/svg${found}`, n); }
  });
  return found;
}

function auditSvg(name, root) {
  const width = Number(root.attrs.width);
  const height = Number(root.attrs.height);
  check(name, Number.isFinite(width) && Number.isFinite(height),
    `viewBox is not finite: ${root.attrs.viewBox}`);

  let marks = 0;
  let tightestHit = null;
  let maxBar = 0;

  walk(root, (node) => {
    for (const [k, v] of Object.entries(node.attrs)) {
      const s = String(v);
      if (s.includes('NaN') || s.includes('undefined') || s.includes('Infinity')) {
        failures.push(`${name}: <${node.tagName} ${k}="${s}"> — a bad number silently drops the mark`);
      }
      if (NUMERIC.has(k) && !Number.isFinite(Number(v))) {
        failures.push(`${name}: <${node.tagName} ${k}="${s}"> is not a finite number`);
      }
      if (k === 'stroke-dasharray') {
        failures.push(`${name}: dashed stroke on <${node.tagName}> — grid and axes are solid hairlines`);
      }
    }

    if (node.tagName === 'path' || node.tagName === 'rect' || node.tagName === 'circle') marks += 1;

    // Nothing paints outside its own box. Text anchored at the very edge is
    // allowed a couple of px of overhang; a mark is not.
    const xs = ['x', 'x1', 'x2', 'cx'].map((k) => Number(node.attrs[k])).filter(Number.isFinite);
    const ys = ['y', 'y1', 'y2', 'cy'].map((k) => Number(node.attrs[k])).filter(Number.isFinite);
    const slack = node.tagName === 'text' ? 8 : 2;
    for (const x of xs) {
      check(name, x >= -slack && x <= width + slack,
        `<${node.tagName}> at x=${x} is outside the ${width}px viewBox`);
    }
    for (const y of ys) {
      check(name, y >= -slack && y <= height + slack,
        `<${node.tagName}> at y=${y} is outside the ${height}px viewBox`);
    }

    if (node.tagName === 'rect' && node.attrs.fill === 'transparent') {
      const w = Number(node.attrs.width);
      const h = Number(node.attrs.height);
      // A band that is thin but tall is still easy to hit - the pointer only
      // has to be in the right column. What is unhittable is small in both
      // directions, so that is what fails.
      const thin = Math.min(w, h);
      if (!tightestHit || thin < tightestHit.thin) tightestHit = { w, h, thin };
    }
    if (node.tagName === 'path' && node.attrs.fill && node.attrs.fill !== 'none') {
      const d = String(node.attrs.d);
      const nums = [...d.matchAll(/-?\d+(?:\.\d+)?/g)].map((mm) => Number(mm[0]));
      // The thin dimension of a bar, whichever way it runs.
      const w = Math.max(...nums.filter((_, i) => i % 2 === 0)) - Math.min(...nums.filter((_, i) => i % 2 === 0));
      const h = Math.max(...nums.filter((_, i) => i % 2 === 1)) - Math.min(...nums.filter((_, i) => i % 2 === 1));
      maxBar = Math.max(maxBar, Math.min(w, h));
    }
    if (node.tagName === 'circle') {
      check(name, Number(node.attrs.r) >= 4, `marker r=${node.attrs.r} is below the 8px minimum diameter`);
      check(name, Number(node.attrs['stroke-width']) === 2,
        `marker is missing its 2px surface ring (stroke-width ${node.attrs['stroke-width']})`);
    }
    if (node.tagName === 'path' && node.attrs.fill === 'none') {
      check(name, Number(node.attrs['stroke-width']) === 2,
        `line stroke-width is ${node.attrs['stroke-width']}, spec is 2`);
    }
  });

  check(name, marks > 0, 'produced no marks at all');
  check(name, maxBar <= 24.001, `a bar is ${maxBar.toFixed(1)}px thick; the cap is 24px`);
  if (tightestHit) {
    const { w, h } = tightestHit;
    notes.push(`${name}: tightest hit target ${w.toFixed(0)}x${h.toFixed(0)}px`);
    if (w < 24 && h < 24) {
      failures.push(`${name}: a hit target is only ${w.toFixed(0)}x${h.toFixed(0)}px, `
        + 'which is a pinpoint in both directions');
    }
  }
}

/**
 * No axis may print the same tick twice.
 *
 * The ticks can be numerically correct and still render as 0, 1, 1, 2, 2 once
 * an integer formatter has been through them, which is what a two-machine bar
 * chart used to do. Numbers alone can't catch that, so this reads the text.
 */
function auditAxisLabels(name, host) {
  const root = host.children ? host.children.find((c) => c.tagName === 'svg') : null;
  if (!root) return;
  const seen = new Map();
  walk(root, (n) => {
    // The y-axis ticks: right-aligned text in the left gutter.
    if (n.tagName !== 'text' || n.attrs['text-anchor'] !== 'end') return;
    const text = textOf(n);
    if (!/^[-\d]/.test(text)) return;
    seen.set(text, (seen.get(text) || 0) + 1);
  });
  for (const [text, times] of seen) {
    if (times > 1) {
      failures.push(`${name}: the y-axis prints ${JSON.stringify(text)} ${times} times — `
        + 'correct ticks, duplicated labels');
    }
  }
}

/** x-axis labels must fit the band they sit in, or neighbours collide. */
function auditLabelFit(name, host, bandCount) {
  const root = host.children.find((c) => c.tagName === 'svg');
  if (!root) return;
  const width = Number(root.attrs.width);
  const band = (width - 54) / bandCount;
  walk(root, (node) => {
    if (node.tagName !== 'text' || node.attrs['text-anchor'] !== 'middle') return;
    const text = textOf(node);
    const px = Number(node.attrs['font-size']) || 11;
    const est = text.length * px * 0.58;
    if (est > band) {
      failures.push(`${name}: x-axis label ${JSON.stringify(text)} needs ~${est.toFixed(0)}px `
        + `in a ${band.toFixed(0)}px band — it will collide with its neighbour`);
    }
  });
}

/* -------------------------------------------------------------- the fixtures */

const mod = await import('../assets/app.js');

function host(w = 700) {
  const h = new FakeNode('div');
  h.clientWidth = w;
  return h;
}

// Grade distribution: six ordered categories, one of them empty.
{
  const h = host();
  mod.columnChart(h, {
    rows: [['S', 1], ['A', 1], ['B', 3], ['C', 2], ['D', 0], ['F', 0]].map(([label, value]) => ({ label, value })),
    unit: 'machines',
  });
  auditChart('columnChart/grades', h);
  auditLabelFit('columnChart/grades', h, 6);
}

// A small estate, which is where the axis used to print 0, 1, 1, 2, 2: the
// tallest bar is 2, so without a minimum step the ticks land on halves and the
// integer formatter prints each one twice.
{
  const h = host();
  mod.columnChart(h, {
    rows: [['S', 1], ['A', 2], ['B', 1], ['C', 0], ['D', 0], ['F', 0]]
      .map(([label, value]) => ({ label, value })),
    unit: 'machines',
  });
  auditChart('columnChart/small-counts', h);
}

// Score bands: more categories, longer labels - the collision case.
{
  const h = host();
  const rows = [];
  for (let lo = 0; lo < 2000; lo += 200) {
    rows.push({ label: String(lo), sublabel: `-${lo + 199}`, value: Math.round(Math.random() * 4) });
  }
  mod.columnChart(h, { rows, unit: 'machines' });
  auditChart('columnChart/bands', h);
  auditLabelFit('columnChart/bands', h, rows.length);
}

// Components, including an all-zero degenerate case.
{
  const h = host(360);
  mod.columnChart(h, {
    rows: ['cpu', 'memory', 'disk', 'network', 'gpu'].map((id) => ({ label: id, value: 0, sublabel: 'F' })),
    unit: 'score',
  });
  auditChart('columnChart/all-zero', h);
}

// Cohort deltas: both signs, a zero, and a long hostname.
{
  const h = host();
  mod.divergingChart(h, {
    rows: [
      { label: 'PC-A-VERY-LONG-HOSTNAME-01', value: -31.4, score: 690 },
      { label: 'PC-002', value: -12.5, score: 880 },
      { label: 'PC-003', value: 0, score: 1000 },
      { label: 'PC-004', value: 6.2, score: 1062 },
    ],
  });
  auditChart('divergingChart', h);
}

// A single member, which is where a naive span calculation divides by zero.
{
  const h = host();
  mod.divergingChart(h, { rows: [{ label: 'PC-ONLY', value: 0, score: 1000 }] });
  auditChart('divergingChart/single', h);
}

// Findings by queue, including a row that is entirely empty.
{
  const h = host();
  mod.stackedChart(h, {
    rows: [
      { label: 'The machine', values: { critical: 2, warning: 5, info: 0 } },
      { label: 'The measurement', values: { critical: 0, warning: 6, info: 1 } },
      { label: 'The data we hold', values: { critical: 0, warning: 0, info: 0 } },
    ],
    series: [
      { key: 'critical', label: 'Critical', colour: '#d03b3b' },
      { key: 'warning', label: 'Warning', colour: '#fab219' },
      { key: 'info', label: 'Informational', colour: '#898781' },
    ],
  });
  auditChart('stackedChart', h);
}

// The same chart as shares of a fixed whole, as the Groups view draws it: a
// row at 100%, a row at nothing, and figures carrying their own unit.
{
  const h = host(360);
  mod.stackedChart(h, {
    rows: [
      { label: 'Dell Inc. Latitude 7440 2-in-1', values: { critical: 100, warning: 0 } },
      { label: 'glasgow', values: { critical: 12.5, warning: 37.5 } },
      { label: '(untagged)', values: { critical: 0, warning: 0 } },
    ],
    series: [
      { key: 'critical', label: 'Critical', colour: '#d03b3b' },
      { key: 'warning', label: 'Warning', colour: '#fab219' },
    ],
    max: 100,
    format: (v) => `${Math.round(v)}%`,
  });
  auditChart('stackedChart/shares', h);
  const text = textOf(h);
  check('stackedChart/shares', text.includes('100%') && text.includes('50%'),
    `the totals are not shown as shares: ${text}`);
}

// History: mixed comparability, a missing score, and two runs at the same
// instant (t1 === t0), which is the divide-by-zero case on the time axis.
{
  const h = host();
  const base = {
    grade: 'B', tool_version: '1.5.1', partial: false, thermal_limited: false, on_ac: true,
    comparability: { preset: 'thorough', profile: 'general', baseline: 'reference-v1', build_isa: 'sse2' },
  };
  mod.lineChart(h, {
    points: [
      { ...base, taken_at: '2026-06-01T10:00:00Z', score: 1000, comparable: true },
      { ...base, taken_at: '2026-07-01T10:00:00Z', score: null, comparable: true },
      { ...base, taken_at: '2026-08-01T10:00:00Z', score: 780, comparable: false },
      { ...base, taken_at: '2026-09-01T10:00:00Z', score: 1010, comparable: true },
    ],
  });
  auditChart('lineChart', h);
}
{
  const h = host();
  const p = {
    grade: 'B', tool_version: '1.5.1', partial: false, thermal_limited: true, on_ac: true, score: 900,
    comparable: true, taken_at: '2026-09-01T10:00:00Z',
    comparability: { preset: 'thorough', profile: 'general', baseline: 'reference-v1', build_isa: 'sse2' },
  };
  mod.lineChart(h, { points: [p, { ...p }] });
  auditChart('lineChart/same-instant', h);
}

// The fleet's timeline: two series on one axis, every week of a half-year, a
// week with no median at all, and the two lines ending on the same value —
// where their end labels would print one over the other.
{
  const h = host(420);
  const points = [];
  for (let k = 26; k >= 0; k -= 1) {
    const d = new Date(Date.UTC(2026, 10, 11) - k * 7 * 86400000).toISOString().slice(0, 10);
    points.push({
      at: d, machines: 40 + (26 - k), machines_flagged: 3 + (k % 4), machines_critical: k % 3,
      median_score: k === 20 ? null : 1000 + k * 3,
    });
  }
  points[points.length - 1].machines_critical = points[points.length - 1].machines_flagged;
  mod.lineChart(h, {
    points,
    time: (p) => `${p.at}T12:00:00Z`,
    series: [
      { label: 'Need attention', colour: '#2a78d6', value: (p) => p.machines_flagged },
      { label: 'Critical', colour: '#d03b3b', value: (p) => p.machines_critical },
    ],
    readout: () => null,
  });
  auditChart('lineChart/series', h);
  const svgRoot = h.children.find((c) => c.tagName === 'svg');
  const lines = svgRoot.children.filter((c) => c.tagName === 'path' && c.attrs.fill === 'none');
  check('lineChart/series', lines.length === 2, `${lines.length} line(s) for two series`);
  const ends = svgRoot.children.filter((c) => c.tagName === 'text' && c.attrs['text-anchor'] === undefined);
  const ys = ends.map((t) => Number(t.attrs.y));
  check('lineChart/series', ys.length === 2 && Math.abs(ys[0] - ys[1]) >= 12,
    `the two end labels sit ${Math.abs(ys[0] - ys[1])}px apart and print over each other`);

  const g = host(420);
  mod.lineChart(g, {
    points,
    time: (p) => `${p.at}T12:00:00Z`,
    series: [{ label: 'Median score', colour: '#2a78d6', value: (p) => p.median_score }],
  });
  auditChart('lineChart/median-with-a-gap', g);
}

/*
 * The scatter: one mark per machine. Nothing to plot, one mark, every mark on
 * the same spot (where a naive scale divides by a zero range), and five
 * hundred — at a phone's width as well as a desktop's. Beyond the shared
 * audit: each point becomes exactly one mark, every mark sits inside the plot,
 * an outline mark has no fill, and the reference line is where it says.
 */
function scatterPoints(n, at) {
  const shapes = ['circle', 'square', 'triangle'];
  return Array.from({ length: n }, (_, i) => {
    const p = at ? { ...at } : { x: (i * 7919) % 131 / 10, y: 200 + ((i * 104729) % 1500) };
    return {
      ...p, shape: shapes[i % 3], hollow: i % 2 === 0, layer: i % 3, colour: ['#898781', '#d03b3b', '#fab219'][i % 3],
    };
  });
}

function auditScatter(name, h, points, xRef) {
  const root = h.children.find((c) => c.tagName === 'svg');
  if (!root) { check(name, false, 'no <svg> was produced'); return; }
  auditChart(name, h);
  const width = Number(root.attrs.width);
  const height = Number(root.attrs.height);
  const marks = root.children.filter((c) => (c.tagName === 'circle' && c.attrs.visibility !== 'hidden')
    || (c.tagName === 'path'));
  check(name, marks.length === points.length, `${marks.length} mark(s) for ${points.length} point(s)`);
  for (const mk of marks) {
    const xs = mk.tagName === 'circle'
      ? [Number(mk.attrs.cx)]
      : [...String(mk.attrs.d).matchAll(/-?\d+(?:\.\d+)?/g)].map((v) => Number(v[0])).filter((_, i) => i % 2 === 0);
    for (const v of xs) check(name, v >= 40 && v <= width - 8, `a mark at x=${v} is outside the plot`);
    if (mk.tagName === 'circle') {
      check(name, Number(mk.attrs.cy) >= 16 && Number(mk.attrs.cy) <= height - 40, `a mark at y=${mk.attrs.cy} is outside the plot`);
    }
  }
  const hollow = points.filter((p) => p.hollow).length;
  check(name, marks.filter((mk) => mk.attrs.fill === 'none').length === hollow,
    `${hollow} outline mark(s) expected, ${marks.filter((mk) => mk.attrs.fill === 'none').length} drawn without fill`);
  if (xRef !== undefined) {
    // Every ink is one hex in the shim, so the line is found by its label: a
    // vertical hairline 6px from the text that names it.
    const label = root.children.find((c) => c.tagName === 'text' && textOf(c).startsWith('old from'));
    const lx = label ? Number(label.attrs.x) + (label.attrs['text-anchor'] === 'start' ? -6 : 6) : NaN;
    const line = root.children.find((c) => c.tagName === 'line' && c.attrs.x1 === c.attrs.x2
      && Math.abs(Number(c.attrs.x1) - lx) < 0.01);
    check(name, !!line, 'no labelled vertical reference line');
    const plotRight = width - 16;
    if (label) {
      const right = label.attrs['text-anchor'] === 'start'
        ? Number(label.attrs.x) + textOf(label).length * 11 * 0.58
        : Number(label.attrs.x);
      check(name, right <= plotRight + 0.5, `the reference label runs past the plot (${right.toFixed(0)} > ${plotRight})`);
    }
  }
  const overlay = root.children.find((c) => c.tagName === 'rect' && c.attrs.tabindex === '0');
  check(name, !!overlay && String(overlay.attrs['aria-label']).includes(`${points.length} mark(s)`),
    'the plot is not reachable by keyboard, or does not say how many marks it holds');
}

for (const [label, points, width] of [
  ['one', scatterPoints(1, { x: 3.2, y: 900 }), 700],
  ['one-at-zero', scatterPoints(1, { x: 0, y: 0 }), 700],
  ['same-point', scatterPoints(40, { x: 6, y: 1000 }), 700],
  ['five-hundred', scatterPoints(500), 1200],
  ['five-hundred-phone', scatterPoints(500), 320],
  ['beyond-the-line', scatterPoints(12).map((p) => ({ ...p, x: p.x + 20 })), 700],
]) {
  const h = host(width);
  mod.scatterChart(h, {
    points, xTitle: 'Age, years — best estimate', yTitle: 'Score', xRef: 5, xRefLabel: 'old from 5 yr',
    readout: () => null, onSelect: () => {},
  });
  auditScatter(`scatterChart/${label}`, h, points, 5);
}
{
  const h = host();
  try {
    mod.scatterChart(h, { points: [], xTitle: 'x', yTitle: 'y', readout: () => null, empty: 'Nothing to place on this chart.' });
    check('scatterChart/none', !h.children.some((c) => c.tagName === 'svg'), 'an empty frame was drawn for no marks');
    check('scatterChart/none', textOf(h).includes('Nothing to place'), 'nothing said why the chart is empty');
  } catch (err) {
    failures.push(`scatterChart/none: threw ${err.message}`);
  }
}
{
  // The legend's keys are marks too, so they are held to the marker spec.
  const legend = mod.markLegend([
    { label: 'Replace', shape: 'square', colour: '#d03b3b' },
    { label: 'Repair', shape: 'triangle', colour: '#fab219' },
    { label: 'At most', shape: 'circle', colour: '#898781', hollow: true },
  ]);
  check('markLegend', auditTree('markLegend', legend) === 3, 'not one key per item');
}

/* ------------------------------------------------------------------- views */

/*
 * The snapshot comes from the binary, run over the anonymised fixtures, so this
 * check exercises the same JSON the browser gets and cannot quietly drift from
 * it. Pass --snapshot <file> to use a captured one instead.
 */
function realSnapshot() {
  const argIdx = process.argv.indexOf('--snapshot');
  if (argIdx > -1) return JSON.parse(readFileSync(process.argv[argIdx + 1], 'utf8'));

  // CARGO_TARGET_DIR is honoured, so this still works for anyone who moves
  // their build output somewhere else. CI sets nothing and gets ./target.
  const target = process.env.CARGO_TARGET_DIR || 'target';
  const exe = join(
    target,
    'debug',
    process.platform === 'win32' ? 'loadbearer-fleet.exe' : 'loadbearer-fleet',
  );
  if (!existsSync(exe)) {
    console.error(`
${exe} is not built. Run: cargo build`);
    process.exit(1);
  }
  const db = join(tmpdir(), `lbf-check-${process.pid}.db`);
  const run = (args) => {
    const r = spawnSync(exe, ['--index', db, '--log-level', 'warn', ...args], { encoding: 'utf8' });
    if (r.status !== 0) {
      console.error(`
${exe} ${args.join(' ')} failed:
${r.stderr || r.stdout}`);
      process.exit(1);
    }
    return r.stdout;
  };
  try {
    run(['scan', 'tests/fixtures']);
    return JSON.parse(run(['report', '--json']));
  } finally {
    for (const suffix of ['', '-wal', '-shm']) {
      try { rmSync(db + suffix); } catch { /* already gone */ }
    }
  }
}

/**
 * A comparison built by the binary, for the same reason the snapshot is: the
 * view is written against a shape, and a hand-written fixture of that shape
 * drifts the moment the server's changes. Machine *names* rather than run ids,
 * so this does not depend on two temporary databases numbering their rows
 * identically.
 */
function realCompare(names) {
  const target = process.env.CARGO_TARGET_DIR || 'target';
  const exe = join(
    target,
    'debug',
    process.platform === 'win32' ? 'loadbearer-fleet.exe' : 'loadbearer-fleet',
  );
  const db = join(tmpdir(), `lbf-check-cmp-${process.pid}.db`);
  const run = (args) => {
    const r = spawnSync(exe, ['--index', db, '--log-level', 'warn', ...args], { encoding: 'utf8' });
    if (r.status !== 0) {
      console.error(`\n${exe} ${args.join(' ')} failed:\n${r.stderr || r.stdout}`);
      process.exit(1);
    }
    return r.stdout;
  };
  try {
    run(['scan', 'tests/fixtures']);
    return JSON.parse(run(['compare', '--json', ...names]));
  } finally {
    for (const suffix of ['', '-wal', '-shm']) {
      try { rmSync(db + suffix); } catch { /* already gone */ }
    }
  }
}

/** `report --by <dimension> --json`: the same document `/api/groups` sends. */
function realGrouping(by) {
  const target = process.env.CARGO_TARGET_DIR || 'target';
  const exe = join(
    target,
    'debug',
    process.platform === 'win32' ? 'loadbearer-fleet.exe' : 'loadbearer-fleet',
  );
  const db = join(tmpdir(), `lbf-check-grp-${process.pid}.db`);
  const run = (args) => {
    const r = spawnSync(exe, ['--index', db, '--log-level', 'warn', ...args], { encoding: 'utf8' });
    if (r.status !== 0) {
      console.error(`\n${exe} ${args.join(' ')} failed:\n${r.stderr || r.stdout}`);
      process.exit(1);
    }
    return r.stdout;
  };
  try {
    run(['scan', 'tests/fixtures']);
    return JSON.parse(run(['report', '--by', by, '--json']));
  } finally {
    for (const suffix of ['', '-wal', '-shm']) {
      try { rmSync(db + suffix); } catch { /* already gone */ }
    }
  }
}

/** `report --changes <weeks> --json`: the same document `/api/changes` sends. */
function realChanges(weeks) {
  const target = process.env.CARGO_TARGET_DIR || 'target';
  const exe = join(
    target,
    'debug',
    process.platform === 'win32' ? 'loadbearer-fleet.exe' : 'loadbearer-fleet',
  );
  const db = join(tmpdir(), `lbf-check-chg-${process.pid}.db`);
  const run = (args) => {
    const r = spawnSync(exe, ['--index', db, '--log-level', 'warn', ...args], { encoding: 'utf8' });
    if (r.status !== 0) {
      console.error(`\n${exe} ${args.join(' ')} failed:\n${r.stderr || r.stdout}`);
      process.exit(1);
    }
    return r.stdout;
  };
  try {
    run(['scan', 'tests/fixtures']);
    return JSON.parse(run(['report', '--changes', String(weeks), '--json']));
  } finally {
    for (const suffix of ['', '-wal', '-shm']) {
      try { rmSync(db + suffix); } catch { /* already gone */ }
    }
  }
}

const snapshot = realSnapshot();
check('snapshot', snapshot.machines.length > 0, 'the fixtures produced no machines');
notes.push(`snapshot: ${snapshot.machines.length} machine(s), ${snapshot.flags.length} finding(s), `
  + `${snapshot.cohorts.length} cohort(s)`);

mod.state.snap = snapshot;
mod.state.filter = {};
mod.state.showInfo = true;

function renderView(name, fn) {
  const h = host(1200);
  try {
    fn(h);
  } catch (err) {
    failures.push(`${name}: threw ${err.message}`);
    return;
  }
  const text = textOf(h);
  check(name, text.length > 40, 'rendered almost no text');
  check(name, !text.includes('undefined'), 'the word "undefined" reached the page');
  check(name, !text.includes('NaN'), 'the word "NaN" reached the page');
  const svgs = auditTree(name, h);
  notes.push(`${name}: ${svgs} chart(s), ${text.length} chars of text`);
}

renderView('overview', (h) => mod.renderOverview(h));
renderView('cohorts', (h) => mod.renderCohorts(h));
renderView('machines', (h) => mod.renderMachines(h));

renderView('lifecycle', (h) => mod.renderLifecycle(h));

/*
 * The fixtures are four machines with three to watch and one to keep, so the
 * Lifecycle view is also rendered over a synthetic estate built from them:
 * every decision, an uncertain one, ages dated and bounded, machines with no
 * age and no score, and long hostnames. The verdicts are the server's to
 * decide; what is checked here is that the view lays out what it is given.
 */
const lifecycleEstate = (() => {
  const template = snapshot.machines[0];
  const verdicts = ['replace', 'repair', 'investigate', 'watch', 'keep'];
  const machines = Array.from({ length: 64 }, (_, i) => {
    const verdict = verdicts[i % 5];
    const aged = i % 9 !== 0;
    return {
      ...template,
      key: `SN-LC-${i}`,
      hostname: i === 1 ? 'WORKSTATION-WITH-A-VERY-LONG-HOSTNAME-07' : `LC-${String(i).padStart(2, '0')}`,
      score: i % 17 === 0 ? null : 300 + ((i * 97) % 1300),
      grade: ['S', 'A', 'B', 'C', 'D', 'F'][i % 6],
      battery_health: i % 4 === 0 ? 61 : null,
      worst_drive_wear_pct: i % 3 === 0 ? 93 : null,
      age_estimate: aged ? { months: 6 + ((i * 13) % 110), basis: i % 4 === 0 ? 'firmware' : 'upper_bound' } : null,
      tags: { site: ['glasgow', 'edinburgh'][i % 2] },
      lifecycle: {
        verdict,
        reasons: verdict === 'keep' ? [] : ['Graded F against the reference baseline', 'At most 7.7 years old, by its CPU\'s launch date'],
        uncertain: i % 10 === 0,
        caveats: i % 10 === 0 ? ['Clocks fell during the run'] : [],
      },
    };
  });
  const lifecycle = Object.fromEntries(verdicts.map((v) => [v, machines.filter((m) => m.lifecycle.verdict === v).length]));
  return { ...snapshot, machines, flags: [], summary: { ...snapshot.summary, machines: machines.length, lifecycle } };
})();

/** The Machine column of the Lifecycle view's candidates table, top to bottom. */
function candidateRows(h) {
  let table = null;
  walk(h, (n) => { if (n.tagName === 'table') table = n; });
  const body = table?.children.find((c) => c.tagName === 'tbody');
  return (body?.children || []).map((tr) => textOf(tr.children[0]));
}

{
  mod.state.snap = lifecycleEstate;
  mod.state.decision = null;
  renderView('lifecycle/synthetic', (h) => mod.renderLifecycle(h));
  const h = host(1200);
  mod.renderLifecycle(h);
  const text = textOf(h);
  for (const want of ['Age against score', 'Age alone is never a reason', 'against the reference baseline',
    're-measure first', 'at most', 'about', 'left off', 'not listed; choose Keep']) {
    check('lifecycle/synthetic', text.includes(want), `missing ${JSON.stringify(want)}`);
  }
  const rank = (host) => ['replace', 'repair', 'investigate', 'watch', 'keep']
    .indexOf(lifecycleEstate.machines.find((m) => (m.hostname || m.key) === host)?.lifecycle.verdict);
  const rows = candidateRows(h);
  const listed = lifecycleEstate.machines.filter((m) => m.lifecycle.verdict !== 'keep').length;
  check('lifecycle/synthetic', rows.length === listed, `${rows.length} row(s) for ${listed} machine(s) to decide about`);
  check('lifecycle/synthetic', rows.every((r, i) => i === 0
    || rank(rows[i - 1]) < rank(r) || (rank(rows[i - 1]) === rank(r) && rows[i - 1].localeCompare(r) <= 0)),
  `not most urgent first, then by name: ${rows.join(', ')}`);
  check('lifecycle/synthetic', rows.every((r) => rank(r) !== 4), 'a machine to keep was listed unasked');

  // A tile chosen: only that decision, keep included when it is the one.
  mod.state.decision = 'keep';
  const k = host(1200);
  mod.renderLifecycle(k);
  const kept = candidateRows(k);
  check('lifecycle/keep', kept.length === lifecycleEstate.summary.lifecycle.keep && kept.every((r) => rank(r) === 4),
    `choosing Keep listed ${kept.join(', ')}`);

  // Nothing with an age: the chart says so rather than drawing an empty frame.
  mod.state.decision = null;
  mod.state.snap = { ...lifecycleEstate, machines: lifecycleEstate.machines.map((m) => ({ ...m, age_estimate: null })) };
  const none = host(1200);
  try {
    mod.renderLifecycle(none);
    check('lifecycle/no-ages', textOf(none).includes('No machine in view has both an age estimate and a score'),
      'an estate with no ages did not say why the chart is empty');
  } catch (err) {
    failures.push(`lifecycle/no-ages: threw ${err.message}`);
  }
  mod.state.snap = snapshot;
}

// The machines table opens best first: score descending, unscored last.
{
  const h = host(1200);
  mod.renderMachines(h);
  const scores = [];
  walk(h, (n) => {
    if (String(n.tagName).toLowerCase() !== 'tr') return;
    const cells = n.children.filter((c) => String(c.tagName).toLowerCase() === 'td');
    if (cells.length > 2) scores.push(Number(textOf(cells[2]).replace(/[^\d.-]/g, '')) || null);
  });
  const scored = scores.filter((s) => s !== null);
  check('machines', scores.length === snapshot.machines.length, `${scores.length} row(s) for `
    + `${snapshot.machines.length} machine(s)`);
  check('machines', scored.every((s, i) => i === 0 || scored[i - 1] >= s),
    `not sorted by score, best first: ${scores.join(', ')}`);
  check('machines', scores.indexOf(null) === -1 || scores.slice(scores.indexOf(null)).every((s) => s === null),
    `an unscored machine sorted above a scored one: ${scores.join(', ')}`);
  notes.push(`machines: opens in score order ${scores.join(', ')}`);
}

// Four distinct CPUs means no cohort is large enough to compare against, which
// is the honest result for the fixtures but leaves the diverging chart - the
// centrepiece of that view - unexercised. So: one cohort, five members.
{
  const template = snapshot.machines[0];
  const members = [-28.5, -11.2, -0.4, 0, 5.1].map((delta, i) => ({
    ...template,
    key: `SN-PEER-${i}`,
    hostname: `PEER-${i}`,
    score: Math.round(1000 * (1 + delta / 100)),
    cohort_delta_pct: delta,
  }));
  const cohort = {
    ...(snapshot.cohorts.find((c) => c.id === template.cohort)),
    members: members.length,
    in_view: members.length,
    median: 1000,
    p10: 760,
    p90: 1040,
    comparable: true,
  };
  mod.state.snap = { ...snapshot, machines: members, cohorts: [cohort], flags: [] };
  renderView('cohorts/with-peers', (h) => mod.renderCohorts(h));
  mod.state.snap = snapshot;
}

/*
 * The overview fetches its own history: "What changed" from `/api/changes`
 * and the charts from `/api/timeline`. Answered with what the binary says
 * about the fixtures — which, measured within a day of each other, have
 * either nothing to compare against or nothing that changed — then with a
 * synthetic estate where every list is populated and one is long enough to
 * collapse, and a half-year of weeks. The timeline is synthetic because
 * nothing but the server sends one; its fields are `TimelinePoint`'s.
 */
async function renderOverviewWith(name, { changes, timeline }) {
  globalThis.fetch = (url) => {
    const u = String(url);
    const body = u.startsWith('/api/changes') ? changes : u.startsWith('/api/timeline') ? timeline : null;
    if (body instanceof Error) return Promise.resolve({ ok: false, text: () => Promise.resolve(body.message) });
    return Promise.resolve(body
      ? { ok: true, json: () => Promise.resolve(body), text: () => Promise.resolve('') }
      : { ok: false, text: () => Promise.resolve('unexpected request') });
  };
  const h = host(1200);
  try {
    await mod.renderOverview(h);
  } catch (err) {
    failures.push(`${name}: threw ${err.message}`);
    return null;
  }
  const text = textOf(h);
  check(name, text.includes('What changed'), 'the "What changed" card is missing');
  check(name, text.includes('judged by today’s rules'), 'the reconstruction is not explained');
  check(name, !text.includes('undefined'), 'the word "undefined" reached the page');
  check(name, !text.includes('NaN'), 'the word "NaN" reached the page');
  check(name, !text.includes('Loading…'), 'a placeholder was left behind');
  notes.push(`${name}: ${auditTree(name, h)} chart(s), ${text.length} chars of text`);
  return text;
}

function weeksOf(n, shape = (k) => ({})) {
  const points = [];
  for (let k = n - 1; k >= 0; k -= 1) {
    const at = new Date(Date.UTC(2026, 10, 11) - k * 7 * 86400000).toISOString().slice(0, 10);
    const machines = 30 + (n - k);
    points.push({
      at,
      machines,
      grades: [['S', 2], ['A', 8], ['B', machines - 16], ['C', 4], ['D', 1], ['F', 1]],
      median_score: 1010 - k * 2,
      machines_flagged: 4 + (k % 5),
      machines_critical: 1 + (k % 2),
      critical: 2,
      warnings: 6,
      info: 9,
      queues: { machine: 8, measurement: 5, coverage: 4 },
      stale: k % 3,
      ...shape(k),
    });
  }
  return { generated_at: '2026-11-11T12:00:00Z', weeks: 26, points };
}

{
  const real = [realChanges(1), realChanges(13)];
  const halfYear = weeksOf(27);
  for (const [i, changes] of real.entries()) {
    const text = await renderOverviewWith(`overview/real-${changes.window_weeks}`, { changes, timeline: halfYear });
    if (text) {
      check(`overview/real-${i}`, changes.baseline_empty
        ? text.includes('nothing to compare against')
        : text.includes('Nothing changed between') || text.includes('New findings'),
      'the fixtures\' changes were not described');
      check(`overview/real-${i}`, text.includes('Over time') && text.includes('Fleet size and grade mix'),
        'the over-time charts are missing');
    }
  }

  // Every list, one of them long enough to collapse, and every severity.
  const flags = snapshot.flags.length ? snapshot.flags : [{ machine_key: snapshot.machines[0].key }];
  const who = (i) => {
    const m = snapshot.machines[i % snapshot.machines.length];
    return { machine_key: m.key, hostname: m.hostname };
  };
  const finding = (i, severity, extra = {}) => ({
    ...who(i), code: flags[i % flags.length].code || 'thermal_limited', severity, kind: 'machine',
    headline: `A headline for change ${i}, long enough to wrap on a phone screen`, new_machine: false, ...extra,
  });
  const severities = ['critical', 'warning', 'warning', 'info'];
  const synthetic = {
    window_weeks: 4, from: '2026-10-14', to: '2026-11-11', baseline_empty: false,
    counts: {
      new_findings: { critical: 3, warning: 6, info: 3 }, new_machine_findings: { critical: 0, warning: 1, info: 0 },
      resolved: { critical: 1, warning: 0, info: 0 }, worsened: 1, eased: 1,
      machines_added: 1, machines_added_flagged: 1, grades_down: 1, grades_up: 1,
    },
    new_findings: [
      ...Array.from({ length: 12 }, (_, i) => finding(i, severities[i % 4])),
      { ...finding(0, 'warning'), machine_key: 'SN-ARRIVED', hostname: 'ARRIVED-01', new_machine: true },
    ],
    worsened: [{ ...who(1), code: 'disk_nearly_full', kind: 'machine', from: 'warning', to: 'critical', headline: 'Only 3.0% free on C:\\' }],
    machines_added: [{ machine_key: 'SN-ARRIVED', hostname: 'ARRIVED-01', grade: 'B', flagged: true }],
    grade_changes: [
      { ...who(2), from: 'C', to: 'D', direction: 'down' },
      { ...who(3), from: 'B', to: 'A', direction: 'up' },
    ],
    resolved: [finding(3, 'critical')],
    eased: [{ ...who(0), code: 'disk_nearly_full', kind: 'machine', from: 'critical', to: 'warning', headline: 'Only 8.0% free on C:\\' }],
  };
  const text = await renderOverviewWith('overview/every-change', { changes: synthetic, timeline: halfYear });
  if (text) {
    for (const want of ['New findings · 12', 'Worse · 1', 'Machines added · 1', 'Findings on machines added · 1',
      'Grades · 2', 'Resolved · 1', 'Eased · 1', 'Show all 12', 'was warning', 'C → D', 'ARRIVED-01']) {
      check('overview/every-change', text.includes(want), `missing ${JSON.stringify(want)}`);
    }
    check('overview/every-change', text.indexOf('New findings ·') < text.indexOf('Worse ·')
      && text.indexOf('Worse ·') < text.indexOf('Resolved ·'), 'new and worse do not come first');
    check('overview/every-change', !text.includes('SN-ARRIVED'),
      'a machine with a hostname was shown by its key');
  }

  // Two weeks of data is not a trend, and must not be drawn as one.
  const short = await renderOverviewWith('overview/two-weeks', { changes: real[0], timeline: weeksOf(2) });
  if (short) {
    check('overview/two-weeks', short.includes('claims a trend that isn’t there'),
      'two points were drawn as a line rather than explained');
  }

  // The server refusing a hand-edited window: its reason, and the way back.
  const refused = await renderOverviewWith('overview/refused', {
    changes: new Error('"2" is not a change window. Use 1, 4 or 13 (weeks)'),
    timeline: halfYear,
  });
  if (refused) {
    check('overview/refused', refused.includes('is not a change window') && refused.includes('a week ago instead'),
      "the server's reason and the way back were not shown");
    check('overview/refused', refused.includes('Machines needing attention'), 'one failure took the charts with it');
  }
  mod.state.snap = snapshot;
}

/*
 * The Groups view fetches its own grouping. Answered first with one the binary
 * produced from the fixtures — the real shape, where four distinct CPUs mean
 * nobody has a peer group — and then with a synthetic estate big enough to
 * draw every chart: more groups than the cap, thin ones, one with no peers at
 * all, and the group of machines with no value.
 */
async function renderGroupsWith(name, grouping) {
  globalThis.fetch = (url) => Promise.resolve(String(url).startsWith('/api/groups')
    ? { ok: true, json: () => Promise.resolve(grouping), text: () => Promise.resolve('') }
    : { ok: false, text: () => Promise.resolve('unexpected request') });
  const h = host(1200);
  try {
    await mod.renderGroups(h);
  } catch (err) {
    failures.push(`${name}: threw ${err.message}`);
    return null;
  }
  const text = textOf(h);
  check(name, text.length > 40, 'rendered almost no text');
  check(name, !text.includes('undefined'), 'the word "undefined" reached the page');
  check(name, !text.includes('NaN'), 'the word "NaN" reached the page');
  notes.push(`${name}: ${auditTree(name, h)} chart(s), ${text.length} chars of text`);
  return h;
}

{
  const real = realGrouping('tag:site');
  check('groups', real.groups.reduce((a, g) => a + g.machines, 0) === real.machines,
    'the groups the binary produced do not add up to the machines in view');
  const h = await renderGroupsWith('groups', real);
  if (h) {
    const text = textOf(h);
    check('groups', text.includes('(untagged)'), 'the untagged group is missing');
    check('groups', text.includes('Replace') && text.includes('Repair'), 'the replace and repair columns are missing');
    check('groups', text.includes('no machine with a peer group'),
      'groups with nothing to compare were not explained');
  }

  const template = real.groups[0];
  const synthetic = [];
  for (let i = 0; i < 14; i += 1) {
    const machines = [40, 9, 6, 5, 4, 4, 3, 2, 2, 1, 1, 1, 1, 1][i];
    const withCohort = i === 6 ? 0 : Math.min(machines, i % 5 === 4 ? 2 : machines);
    synthetic.push({
      ...template,
      value: i === 13 ? null : `SITE-${String.fromCharCode(65 + i)}${i === 0 ? '-WITH-A-LONG-NAME' : ''}`,
      machines,
      grades: [['S', 0], ['A', 1], ['B', machines - 1], ['C', 0], ['D', 0], ['F', 0]],
      median_cohort_delta_pct: withCohort ? [-23.4, -8.1, 0, 3.2, -1.5][i % 5] : null,
      with_cohort: withCohort,
      thin: withCohort < real.thresholds.min_cohort,
      flagged: Math.ceil(machines / (i + 1)),
      machines_critical: i % 3 === 0 ? 1 : 0,
      top_flags: [{ code: 'thermal_limited', machines: 1 }],
    });
  }
  const big = await renderGroupsWith('groups/synthetic', { ...real, by: 'model', groups: synthetic, machines: 80 });
  if (big) {
    const text = textOf(big);
    check('groups/synthetic', text.includes(`first 12 of ${synthetic.length}`),
      'the charts did not say they were capped');
    check('groups/synthetic', text.includes('(not recorded)'), 'the group with no model is missing');
    check('groups/synthetic', text.includes('left out because fewer than'),
      'thin groups were left out of the chart without saying so');
    // The full table has a row for every group, capped charts or not.
    let tables = 0;
    let lastRows = 0;
    walk(big, (n) => {
      if (n.tagName !== 'table') return;
      tables += 1;
      const body = n.children.find((c) => c.tagName === 'tbody');
      lastRows = body ? body.children.length : 0;
    });
    check('groups/synthetic', tables > 0 && lastRows === synthetic.length,
      `the table has ${lastRows} row(s) for ${synthetic.length} group(s)`);
  }

  // A stale or hand-edited link naming a dimension that does not exist: the
  // server's reason, and the way back.
  globalThis.fetch = () => Promise.resolve({
    ok: false, text: () => Promise.resolve('"hostname" is not something machines can be grouped by.'),
  });
  const refused = host(1200);
  try {
    await mod.renderGroups(refused);
    check('groups/refused', textOf(refused).includes('not something machines can be grouped by'),
      "the server's reason was not shown");
  } catch (err) {
    failures.push(`groups/refused: threw ${err.message}`);
  }
}

// The drilldown fetches its own detail, so the fetch is answered with a payload
// assembled from the snapshot plus a synthetic history - the branch that only
// appears once a machine has been measured more than once.
{
  const m = snapshot.machines[0];
  const base = {
    grade: m.grade, tool_version: m.tool_version, partial: m.partial,
    thermal_limited: m.thermal_limited, on_ac: m.on_ac, comparability: m.comparability,
  };
  const payload = {
    machine: m,
    flags: snapshot.flags.filter((f) => f.machine_key === m.key),
    cohort: snapshot.cohorts.find((c) => c.id === m.cohort) || null,
    history: [
      { ...base, run_id: 1, taken_at: '2026-05-01T09:00:00Z', score: 1000, source_path: 'a.json' },
      {
        ...base,
        run_id: 2,
        taken_at: '2026-07-01T09:00:00Z',
        score: 940,
        source_path: 'b.json',
        comparability: { ...m.comparability, preset: 'quick' },
      },
      { ...base, run_id: m.run_id, taken_at: m.taken_at, score: m.score, source_path: m.source_path },
    ],
    subtests: [
      {
        component: 'cpu', id: 'aes_gcm', value: 2101.7, unit: 'MiB/s', score: 1662.7,
        ratio: 2.76, cv: 0.0064, confidence: 'high', representative: 'median', scored: true,
      },
      {
        component: 'disk', id: 'rand_write_qd', value: 118000, unit: 'IOPS', score: null,
        ratio: null, cv: null, confidence: 'low', representative: null, scored: false,
      },
    ],
    // None of the fixtures was taken with admin rights on loadbearer 1.6.0,
    // so the drives are synthetic: an NVMe drive with every counter, one
    // raising a warning, and a SATA disk with almost none - the blank cells.
    drives: [
      {
        position: 0, device: 'PhysicalDrive0', interface: 'nvme', model: 'Boot SSD', power_on_hours: 12881,
        power_cycles: 667, unsafe_shutdowns: 195, percentage_used: 104, available_spare_pct: 100,
        data_written_tb: 60.11, media_errors: 0, critical_warning: 0,
      },
      {
        position: 1, device: 'PhysicalDrive1', interface: 'nvme', model: null, power_on_hours: 300,
        power_cycles: 12, unsafe_shutdowns: 0, percentage_used: 1, available_spare_pct: 9,
        data_written_tb: 0.2, media_errors: 3, critical_warning: 1,
      },
      {
        position: 2, device: 'sda', interface: 'sata', model: null, power_on_hours: 40000,
        power_cycles: null, unsafe_shutdowns: null, percentage_used: null, available_spare_pct: null,
        data_written_tb: null, media_errors: null, critical_warning: null,
      },
    ],
  };
  globalThis.fetch = () => Promise.resolve({
    ok: true, json: () => Promise.resolve(payload), text: () => Promise.resolve(''),
  });
  const h = host(1200);
  try {
    await mod.renderMachine(h, m.key);
    const text = textOf(h);
    check('machine', text.includes(m.hostname || m.key), 'the drilldown did not name its machine');
    check('machine', text.includes('Lifecycle') && (text.includes('Keep —') || text.includes('Watch —')),
      'the drilldown does not say what the machine calls for');
    check('machine', text.includes('Drives') && text.includes('PhysicalDrive1') && text.includes('0x01'),
      'the drives table is missing a drive');
    check('machine', !text.includes('undefined'), 'the word "undefined" reached the page');
    check('machine', !text.includes('NaN'), 'the word "NaN" reached the page');
    notes.push(`machine: ${auditTree('machine', h)} chart(s), ${text.length} chars of text`);
  } catch (err) {
    failures.push(`machine: threw ${err.message}`);
  }
}

// A machine that has never been reached: the empty-state path.
{
  globalThis.fetch = () => Promise.resolve({
    ok: false, text: () => Promise.resolve('no machine keyed "gone" in the index'),
  });
  const h = host(1200);
  try {
    await mod.renderMachine(h, 'gone');
    check('machine/missing', textOf(h).includes('no machine keyed'), 'the error was not shown');
  } catch (err) {
    failures.push(`machine/missing: threw ${err.message}`);
  }
}

// The comparison, against one the binary produced.
{
  const [a, b] = snapshot.machines;
  const comparison = realCompare([a.hostname || a.key, b.hostname || b.key]);
  // The first machine gets a *second* run, so the picker renders a dropdown
  // rather than a date. With one run each it never did, and the bug that
  // shipped lived entirely inside that dropdown.
  const earlier = { run_id: a.run_id + 1000, taken_at: '2026-01-01T09:00:00Z' };
  const histories = new Map([
    [a.key, { history: [earlier, { run_id: a.run_id, taken_at: a.taken_at }] }],
    [b.key, { history: [{ run_id: b.run_id, taken_at: b.taken_at }] }],
  ]);
  globalThis.fetch = (url) => {
    const u = String(url);
    const payload = u.startsWith('/api/compare')
      ? comparison
      : histories.get(decodeURIComponent(u.replace('/api/machine/', ''))) || { history: [] };
    return Promise.resolve({
      ok: true, json: () => Promise.resolve(payload), text: () => Promise.resolve(''),
    });
  };
  const h = host(1200);
  try {
    await mod.renderCompare(h, [a.run_id, b.run_id]);
    const text = textOf(h);
    check('compare', text.includes(comparison.overall.summary), 'the verdict was not shown');
    check('compare', text.includes(a.hostname || a.key), 'the first run was not named');
    check('compare', !text.includes('undefined'), 'the word "undefined" reached the page');
    check('compare', !text.includes('NaN'), 'the word "NaN" reached the page');
    // Every component it was handed has to reach the page. Dropping half a
    // comparison is the failure this view could have while still looking fine.
    for (const c of comparison.components) {
      check('compare', text.includes(c.label), `component ${c.id} is missing from the table`);
    }

    // The run picker has to agree with what is being compared. It did not:
    // `{ selected: false }` was written out as `selected="false"`, which is a
    // *present* boolean attribute, so every option was selected, the browser
    // showed the last one, and the dropdown named a different run from the one
    // in the table. Exactly one option may carry it.
    const selects = [];
    walk(h, (n) => {
      // The run pickers, not the "Add a machine…" one beside them.
      if (n.tagName === 'select' && String(n.attrs['aria-label']).startsWith('Which run')) {
        selects.push(n);
      }
    });
    check('compare', selects.length > 0, 'no run picker was rendered to check');
    for (const s of selects) {
      const marked = s.children.filter((o) => o.attrs.selected !== undefined);
      check(
        'compare',
        marked.length <= 1,
        `a run picker marks ${marked.length} of its ${s.children.length} options selected`,
      );
      check(
        'compare',
        String(s.value) === String(a.run_id),
        `a run picker shows ${s.value} while ${a.run_id} is being compared`,
      );
    }
    notes.push(
      `compare: ${comparison.components.length} component(s), `
      + `${comparison.coverage.compared} measurement(s), ${text.length} chars of text`,
    );
  } catch (err) {
    failures.push(`compare: threw ${err.message}`);
  }
}

// Refusing is most of what that endpoint does, so the view has to show the
// reason rather than an empty table.
{
  globalThis.fetch = (url) => Promise.resolve(
    String(url).startsWith('/api/compare')
      ? { ok: false, text: () => Promise.resolve('these runs share no comparable measurements.') }
      : { ok: true, json: () => Promise.resolve({ history: [] }), text: () => Promise.resolve('') },
  );
  const h = host(1200);
  try {
    await mod.renderCompare(h, [1, 2]);
    check(
      'compare/refused',
      textOf(h).includes('no comparable measurements'),
      "the server's reason was not shown",
    );
  } catch (err) {
    failures.push(`compare/refused: threw ${err.message}`);
  }
}

// Triage. The fixtures carry none, so the snapshot is given one finding in
// each state, and the overview's queue and a machine's Findings card are
// rendered as a viewer, a contributor and an admin would see them. What has to
// hold: the controls appear only for the caller who may use them, a quieted
// finding says what was said and by whom, and the queue hides snoozed and
// suppressed findings until asked while still showing acknowledged ones.
{
  const flags = snapshot.flags.filter((f) => f.severity !== 'info');
  check('triage', flags.length >= 4, `the fixtures have ${flags.length} warning(s); four are needed`);
  const [live, acked, snoozed, suppressed] = flags;
  const at = '2026-09-09T12:00:00Z';
  const said = new Map([
    [acked, { state: 'acknowledged', by: 'Iain', at, note: 'fan on order', severity: acked.severity }],
    [snoozed, { state: 'snoozed', by: 'Iain', at, note: null, until: '2026-12-01', severity: snoozed.severity }],
    [suppressed, {
      state: 'suppressed', by: 'Admin', at, note: 'collector quirk', rule: 7, reach: 'tags', tags: { site: 'edinburgh' },
    }],
  ]);
  // Distinct headlines, so each finding can be told apart in the text.
  const tagged = snapshot.flags.map((f) => (said.has(f)
    ? { ...f, triage: said.get(f), headline: `${f.headline} [${said.get(f).state}]` }
    : { ...f, headline: f === live ? `${f.headline} [live]` : f.headline }));
  const triaged = {
    ...snapshot,
    flags: tagged,
    summary: { ...snapshot.summary, quieted: { acknowledged: 1, snoozed: 1, suppressed: 1 } },
  };
  const buttons = (h) => {
    const out = [];
    walk(h, (n) => { if (n.tagName === 'button') out.push(n.textContent); });
    return out;
  };
  const roles = [
    ['viewer', { may_triage: false, may_suppress: false }],
    ['contributor', { may_triage: true, may_suppress: false }],
    ['admin', { may_triage: true, may_suppress: true }],
  ];
  mod.state.snap = triaged;
  // The overview fetches its history cards; here they simply fail, as they
  // may, and the rest of the page must not care.
  globalThis.fetch = () => Promise.reject(new Error('no network in the geometry check'));
  for (const [role, me] of roles) {
    for (const showQuiet of [false, true]) {
      const name = `triage/overview/${role}${showQuiet ? '/all' : ''}`;
      mod.state.me = { role, ...me };
      mod.state.showQuiet = showQuiet;
      const h = host(1200);
      try {
        mod.renderOverview(h);
      } catch (err) {
        failures.push(`${name}: threw ${err.message}`);
        continue;
      }
      const text = textOf(h);
      const b = buttons(h);
      check(name, !text.includes('undefined') && !text.includes('NaN'), 'undefined or NaN reached the page');
      check(name, text.includes('[live]') && text.includes('[acknowledged]'),
        'the queue must show live and acknowledged findings');
      check(name, text.includes('[snoozed]') === showQuiet && text.includes('[suppressed]') === showQuiet,
        `snoozed and suppressed findings should be ${showQuiet ? 'shown' : 'hidden'}`);
      check(name, text.includes('1 acknowledged · 1 snoozed · 1 suppressed'), 'the quieted counts are not said');
      check(name, text.includes('Acknowledged by Iain') && text.includes('fan on order'),
        'an acknowledged finding does not say who, or the note');
      check(name, b.includes('Show 2 snoozed or suppressed') !== showQuiet,
        `the toggle does not offer the hidden count: ${b.join(' | ')}`);
      check(name, b.includes('Acknowledge') === me.may_triage && b.includes('Snooze') === me.may_triage,
        `acknowledge and snooze shown ${b.includes('Acknowledge')} for may_triage ${me.may_triage}`);
      check(name, b.includes('Undo') === me.may_triage, 'undo follows may_triage');
      check(name, b.includes('Suppress') === me.may_suppress, 'suppress follows may_suppress');
      check(name, b.includes('Remove rule') === (me.may_suppress && showQuiet),
        'removing a rule is for an admin, on a suppressed finding in view');
      if (showQuiet) {
        check(name, text.includes('Snoozed until 2026-12-01') && text.includes('Suppressed where site=edinburgh'),
          'a snoozed or suppressed finding does not say so');
      }
    }
  }

  // A machine's own Findings card: every finding, whatever its triage.
  for (const [role, me] of roles) {
    const name = `triage/machine/${role}`;
    mod.state.me = { role, ...me };
    const m = triaged.machines.find((x) => x.key === suppressed.machine_key);
    const payload = {
      machine: m,
      flags: tagged.filter((f) => f.machine_key === m.key),
      cohort: null,
      history: [],
      subtests: [],
    };
    globalThis.fetch = () => Promise.resolve({
      ok: true, json: () => Promise.resolve(payload), text: () => Promise.resolve(''),
    });
    const h = host(1200);
    try {
      await mod.renderMachine(h, m.key);
      const text = textOf(h);
      check(name, !text.includes('undefined') && !text.includes('NaN'), 'undefined or NaN reached the page');
      check(name, text.includes('[suppressed]') && text.includes('Suppressed where site=edinburgh'),
        'the machine page hides or mislabels a suppressed finding');
      check(name, buttons(h).includes('Remove rule') === me.may_suppress, 'remove rule follows may_suppress');
    } catch (err) {
      failures.push(`${name}: threw ${err.message}`);
    }
  }
  mod.state.me = null;
  mod.state.showQuiet = false;
  mod.state.snap = snapshot;

  // The request each control sends: JSON — the server's CSRF defence — and
  // a failure reported in the footer rather than swallowed. A removal is a
  // body-less DELETE whose 204 is success.
  const footer = new FakeNode('p');
  globalThis.document.querySelector = (sel) => (sel === '#footer-note' ? footer : null);
  const seen = [];
  let answer = { ok: true, json: () => Promise.resolve({ triage: null }), text: () => Promise.resolve('') };
  globalThis.fetch = (url, opts) => {
    seen.push({ url: String(url), method: opts?.method, type: opts?.headers?.['Content-Type'], body: opts?.body });
    return Promise.resolve(answer);
  };
  try {
    const body = { machine_key: live.machine_key, code: live.code, action: 'acknowledge', note: 'in hand' };
    const ok = await mod.sendTriage('/api/triage', { body, done: 'Acknowledged it.' });
    check('triage/send', ok && footer.textContent === 'Acknowledged it.', `reported ${footer.textContent}`);
    check('triage/send', seen[0]?.method === 'POST' && seen[0]?.type === 'application/json',
      `sent ${seen[0]?.method} as ${seen[0]?.type}`);
    check('triage/send', JSON.parse(seen[0]?.body || '{}').note === 'in hand', 'the note was not sent');

    answer = { ok: false, text: () => Promise.resolve('a snooze runs until a day after today') };
    const refused = await mod.sendTriage('/api/triage', { body, done: 'no' });
    check('triage/send', !refused && footer.textContent.includes('a snooze runs until'),
      `a refusal has to be reported as itself: ${footer.textContent}`);

    answer = {
      ok: true, json: () => Promise.reject(new SyntaxError('no body')), text: () => Promise.resolve(''),
    };
    const removed = await mod.sendTriage('/api/suppressions/7', { method: 'DELETE', done: 'Removed.' });
    check('triage/send', removed && footer.textContent === 'Removed.', `a 204 read as ${footer.textContent}`);
    check('triage/send', seen[2]?.method === 'DELETE' && seen[2]?.body === undefined, 'the DELETE carried a body');
    notes.push(`triage: ${seen.length} request(s) checked, 3 roles x 2 queue states, machine page x 3`);
  } catch (err) {
    failures.push(`triage/send: threw ${err.message}`);
  }
  globalThis.document.querySelector = () => null;
}

// Submitting results: one request per file, and each answer reported as
// itself. A single bad file must not swallow the others, which is the whole
// reason it is one request each rather than a batch.
{
  const seen = [];
  globalThis.document.querySelector = (sel) =>
    (sel === '#upload' || sel === '#footer-note' ? new FakeNode('button') : null);
  globalThis.fetch = (url, opts) => {
    seen.push({ url: String(url), type: opts?.headers?.['Content-Type'], body: opts?.body });
    const body = String(opts?.body ?? '');
    if (body.includes('"bad"')) {
      return Promise.resolve({
        ok: false, text: () => Promise.resolve('this is not a loadbearer result file'),
      });
    }
    return Promise.resolve({
      ok: true,
      json: () => Promise.resolve({
        outcome: body.includes('"dup"') ? 'already indexed' : 'added',
        hostname: 'FLEET-WIN-01',
      }),
      text: () => Promise.resolve(''),
    });
  };
  const file = (name, text) => ({ name, text: () => Promise.resolve(text) });
  try {
    const results = await mod.uploadAll([
      file('new.json', '{"ok":1}'),
      file('same.json', '{"dup":1}'),
      file('notes.txt', '{"bad":1}'),
    ]);
    check('upload', results.length === 3, `reported ${results.length} of 3 files`);
    check('upload', results[0].includes('added'), `first file: ${results[0]}`);
    check('upload', results[1].includes('already indexed'), `second file: ${results[1]}`);
    check(
      'upload',
      results[2].includes('not a loadbearer result file'),
      `a refusal has to be reported as itself: ${results[2]}`,
    );
    check(
      'upload',
      results.every((r, i) => r.startsWith(['new.json', 'same.json', 'notes.txt'][i])),
      `each line names its own file: ${results.join(' | ')}`,
    );
    const posts = seen.filter((s) => s.url === '/api/upload');
    check('upload', posts.length === 3, `sent ${posts.length} requests for 3 files`);
    check(
      'upload',
      posts.every((p) => p.type === 'application/json'),
      'every upload declares application/json, which is half the CSRF defence',
    );
    notes.push(`upload: ${posts.length} request(s), ${results.length} result(s) reported`);
  } catch (err) {
    failures.push(`upload: threw ${err.message}`);
  }
  globalThis.document.querySelector = () => null;
}

/* ------------------------------------------------------------------ report */

for (const n of notes) console.log(`  note  ${n}`);
if (failures.length) {
  console.error(`\n${failures.length} geometry problem(s):`);
  for (const f of failures) console.error(`  FAIL  ${f}`);
  process.exit(1);
}
console.log('\nAll charts render with finite geometry, inside their box, to spec.');
