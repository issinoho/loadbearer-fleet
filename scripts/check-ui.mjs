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
import {
  existsSync, mkdirSync, readFileSync, rmSync, writeFileSync,
} from 'node:fs';
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
    this.listeners = {};
  }

  setAttribute(k, v) { this.attrs[k] = v; }

  getAttribute(k) { return this.attrs[k]; }

  removeAttribute(k) { delete this.attrs[k]; }

  append(...kids) { this.children.push(...kids.filter(Boolean)); }

  replaceChildren(...kids) { this.children = kids.filter(Boolean); }

  // Kept rather than dropped, so a check can press a button: `fire` below.
  addEventListener(type, fn) { (this.listeners[type] ||= []).push(fn); }

  focus() {}

  select() {}

  remove() {}

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
// Saved views and the table sorts write the address in place.
globalThis.history = { replaceState(_s, _t, url) { globalThis.location.hash = url; } };
globalThis.fetch = () => Promise.reject(new Error('no network in the geometry check'));

/* ---------------------------------------------------------------- the walk */

function walk(node, fn, depth = 0) {
  fn(node, depth);
  for (const kid of node.children || []) walk(kid, fn, depth + 1);
}

/** Press a button, or send any event, as the browser would. */
async function fire(node, type = 'click', ev = {}) {
  for (const fn of node?.listeners?.[type] || []) {
    await fn({ target: node, preventDefault() {}, stopPropagation() {}, ...ev });
  }
}

/** Every node under `root` that `test` accepts, in document order. */
function findAll(root, test) {
  const out = [];
  walk(root, (n) => { if (test(n)) out.push(n); });
  return out;
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

// Groups are named by system model and by CPU, and those names are long. A
// label is right-aligned against the plot, so one wider than its gutter ran
// out of the left of the chart and across the card beside it — which the
// geometry audit never saw, because the text's own x was inside the box. So:
// every right-aligned label must start inside the chart, at the widths a
// card on the Groups tab really has, and a shortened one keeps its whole
// name in a title and both of its ends on show.
{
  const long = [
    'Dell Inc. Dell Pro Precision 5 14S PW514260',
    '11th Gen Intel(R) Core(TM) i7-1185G7 @ 3.00GHz',
    'AMD Ryzen AI 9 HX PRO 375 w/ Radeon 890M',
    '13th Gen Intel(R) Core(TM) i7-1365U',
    '13th Gen Intel(R) Core(TM) i7-1370P',
    '(not recorded)',
  ];
  const draw = {
    stackedChart: (h) => mod.stackedChart(h, {
      rows: long.map((label, i) => ({ label, values: { critical: i, warning: 10 * i } })),
      series: [
        { key: 'critical', label: 'Critical', colour: '#d03b3b' },
        { key: 'warning', label: 'Warning', colour: '#fab219' },
      ],
      max: 100,
      format: (v) => `${Math.round(v)}%`,
    }),
    divergingChart: (h) => mod.divergingChart(h, {
      rows: long.map((label, i) => ({ label, value: i - 2, score: 1000 })),
    }),
    rangeChart: (h) => mod.rangeChart(h, {
      rows: long.map((label, i) => ({ label, p10: 100 + i, median: 200 + i, p90: 300 + i, machines: 5 })),
    }),
  };
  for (const [chart, render] of Object.entries(draw)) {
    for (const width of [320, 520, 700, 1400]) {
      const name = `${chart}/long labels at ${width}`;
      const h = host(width);
      render(h);
      auditChart(name, h);
      const shown = [];
      walk(h, (n) => {
        if (n.tagName !== 'text' || n.attrs['text-anchor'] !== 'end') return;
        const own = n.children.filter((c) => c.tagName !== 'title').map(textOf).join('');
        if (/^[-+\d.,%]+$/.test(own)) return; // an axis tick or a figure, not a name
        const title = n.children.find((c) => c.tagName === 'title');
        shown.push(own);
        const left = Number(n.attrs.x) - mod.textWidth(own, Number(n.attrs['font-size']));
        check(name, left >= 0, `${JSON.stringify(own)} starts ${Math.round(-left)}px left of the chart`);
        const full = title ? textOf(title) : own;
        check(name, long.includes(full), `${JSON.stringify(own)} does not carry its whole name`);
        if (title) {
          const [head, tail] = own.split('…');
          check(name, tail !== undefined && full.startsWith(head) && full.endsWith(tail),
            `${JSON.stringify(own)} is not ${JSON.stringify(full)} shortened in the middle`);
        }
      });
      check(name, shown.length === long.length, `${shown.length} labels for ${long.length} rows`);
      // The two 13th Gen parts differ only at the end: shortened, they must
      // still be told apart.
      check(name, new Set(shown).size === shown.length, `two rows read the same: ${shown.join(' | ')}`);
    }
  }
  // Nothing rounds to a signed nothing.
  const h = host(520);
  mod.divergingChart(h, { rows: [{ label: 'PC-A', value: -0.04, score: 1000 }, { label: 'PC-B', value: 0.04, score: 1000 }] });
  check('divergingChart/zero', !/[-+]0\.0%/.test(textOf(h)), `a value that rounds to zero is signed: ${textOf(h)}`);
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
function realSnapshot(config = null) {
  const argIdx = process.argv.indexOf('--snapshot');
  if (argIdx > -1 && !config) return JSON.parse(readFileSync(process.argv[argIdx + 1], 'utf8'));

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
  const db = join(tmpdir(), `lbf-check-${process.pid}${config ? '-cfg' : ''}.db`);
  // A configuration, when given, goes through the binary's own reader like
  // anything else: `report` builds its thresholds from it as the server does.
  const cfg = join(tmpdir(), `lbf-check-${process.pid}.toml`);
  if (config) writeFileSync(cfg, config);
  const run = (args) => {
    const r = spawnSync(exe, ['--index', db, '--log-level', 'warn', ...(config ? ['--config', cfg] : []), ...args],
      { encoding: 'utf8' });
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
    if (config) {
      try { rmSync(cfg); } catch { /* already gone */ }
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

/*
 * The Components view's two charts. The histogram: a dozen bins at a desktop's
 * width and a phone's, one bin of identical values (a zero range, where a
 * naive scale divides by nothing), a chosen bin, and the three quantile marks
 * landing on top of each other. Beyond the shared audit, each mark label gets
 * a line of its own, so they cannot print over each other.
 */
for (const [label, bins, width, marks] of [
  ['dozen', Array.from({ length: 12 }, (_, i) => ({ lo: i * 250, hi: (i + 1) * 250, machines: (i * 7) % 11 })), 700,
    [{ label: 'p10', value: 400 }, { label: 'median', value: 1510 }, { label: 'p90', value: 2600 }]],
  ['dozen-phone', Array.from({ length: 12 }, (_, i) => ({ lo: 100000 + i * 25000, hi: 100000 + (i + 1) * 25000, machines: i + 1 })), 320,
    [{ label: 'p10', value: 110000 }, { label: 'median', value: 112000 }, { label: 'p90', value: 114000 }]],
  ['identical', [{ lo: 1000, hi: 1000, machines: 40 }], 700,
    [{ label: 'p10', value: 1000 }, { label: 'median', value: 1000 }, { label: 'p90', value: 1000 }]],
  ['one-machine', [{ lo: 0.42, hi: 0.42, machines: 1 }], 320, []],
]) {
  const h = host(width);
  mod.histogramChart(h, { bins, marks, selected: label === 'dozen' ? 3 : null, onSelect: () => {}, caption: 'MiB/s — higher is better' });
  auditChart(`histogramChart/${label}`, h);
  const root = h.children.find((c) => c.tagName === 'svg');
  if (root) {
    const named = root.children.filter((c) => c.tagName === 'text' && /^(p10|median|p90) /.test(textOf(c)));
    check(`histogramChart/${label}`, named.length === marks.length, `${named.length} quantile label(s) for ${marks.length}`);
    check(`histogramChart/${label}`, new Set(named.map((t) => t.attrs.y)).size === named.length,
      'two quantile labels share a line and print over each other');
    const hits = root.children.filter((c) => c.tagName === 'rect' && c.attrs.fill === 'transparent');
    check(`histogramChart/${label}`, hits.length === bins.length, `${hits.length} hit target(s) for ${bins.length} bin(s)`);
  }
}
{
  const h = host();
  mod.histogramChart(h, { bins: [] });
  check('histogramChart/none', !h.children.some((c) => c.tagName === 'svg') && textOf(h).includes('Nothing to draw'),
    'an empty histogram drew a frame or said nothing');
}
for (const [label, rows, width] of [
  ['groups', [
    { label: 'glasgow', p10: 900, median: 1010, p90: 1200, machines: 30 },
    { label: 'EDINBURGH-WITH-A-LONG-SITE-NAME', p10: 700, median: 800, p90: 950, machines: 6 },
    { label: '2024', p10: 1000, median: 1000, p90: 1000, machines: 4 },
  ], 700],
  ['one-flat', [{ label: 'only', p10: 5, median: 5, p90: 5, machines: 4 }], 320],
]) {
  const h = host(width);
  mod.rangeChart(h, { rows, onSelect: () => {} });
  auditChart(`rangeChart/${label}`, h);
}

/*
 * The Components view fetches its own distribution and heatmap. Answered first
 * with what the binary says about the fixtures — four machines on four CPUs,
 * so every cell has no peer group — then with synthetic shapes for every state
 * the real ones cannot reach: one machine, nobody, a split, a chosen bin, a
 * capped grid of a large estate with weak, empty and unmeasured cells, the
 * grade encoding, and the server refusing a hand-edited measure.
 */
function realReport(args) {
  const target = process.env.CARGO_TARGET_DIR || 'target';
  const exe = join(target, 'debug', process.platform === 'win32' ? 'loadbearer-fleet.exe' : 'loadbearer-fleet');
  const db = join(tmpdir(), `lbf-check-cmp-${process.pid}-${args.join('').replace(/[^a-z]/gi, '')}.db`);
  const run = (a) => {
    const r = spawnSync(exe, ['--index', db, '--log-level', 'warn', ...a], { encoding: 'utf8' });
    if (r.status !== 0) {
      console.error(`\n${exe} ${a.join(' ')} failed:\n${r.stderr || r.stdout}`);
      process.exit(1);
    }
    return r.stdout;
  };
  try {
    run(['scan', 'tests/fixtures']);
    return JSON.parse(run(['report', ...args, '--json']));
  } finally {
    for (const suffix of ['', '-wal', '-shm']) {
      try { rmSync(db + suffix); } catch { /* already gone */ }
    }
  }
}

async function renderComponentsWith(name, { distribution, heatmap, settings = {} }) {
  const answer = (body) => (body instanceof Error
    ? { ok: false, text: () => Promise.resolve(body.message) }
    : { ok: true, json: () => Promise.resolve(body), text: () => Promise.resolve('') });
  globalThis.fetch = (url) => {
    const u = String(url);
    if (u.startsWith('/api/distribution')) return Promise.resolve(answer(distribution));
    if (u.startsWith('/api/heatmap')) return Promise.resolve(answer(heatmap));
    return Promise.resolve({ ok: false, text: () => Promise.resolve('unexpected request') });
  };
  Object.assign(mod.state, {
    measure: null, config: null, split: null, bin: null, encoding: null, hsort: null, hdir: null, ...settings,
  });
  const h = host(1200);
  try {
    await mod.renderComponents(h);
  } catch (err) {
    failures.push(`${name}: threw ${err.message}`);
    return null;
  }
  const text = textOf(h);
  check(name, text.length > 40, 'rendered almost no text');
  check(name, !text.includes('undefined'), 'the word "undefined" reached the page');
  check(name, !text.includes('NaN'), 'the word "NaN" reached the page');
  check(name, !text.includes('[object'), 'an object reached the page as text');
  notes.push(`${name}: ${auditTree(name, h)} chart(s), ${text.length} chars of text`);
  return h;
}

/** The heatmap's body rows. */
function heatRows(h) {
  let rows = [];
  walk(h, (n) => {
    if (n.tagName === 'table' && String(n.className).includes('heatmap')) {
      rows = n.children.find((c) => c.tagName === 'tbody')?.children || [];
    }
  });
  return rows;
}

{
  const overall = realReport(['--distribution', 'overall']);
  const latency = realReport(['--distribution', 'memory/latency']);
  const heat = realReport(['--heatmap']);
  check('components', overall.bins.reduce((a, b) => a + b.machines, 0) === overall.included,
    'the bins the binary produced do not add up to the machines drawn');
  check('components', overall.included + overall.left_out.reduce((a, l) => a + l.machines, 0) === overall.machines,
    'drawn and left out do not add up to the machines in view');

  const real = await renderComponentsWith('components', { distribution: overall, heatmap: heat });
  if (real) {
    const text = textOf(real);
    for (const want of ['Components across the fleet', 'Overall score', 'thorough · general', 'Every machine, by component',
      'no peers', 'At each end', 'median']) {
      check('components', text.includes(want), `missing ${JSON.stringify(want)}`);
    }
    check('components', heatRows(real).length === heat.rows.length, `${heatRows(real).length} heatmap row(s) for ${heat.rows.length}`);
  }

  const sub = await renderComponentsWith('components/subtest', { distribution: latency, heatmap: heat });
  if (sub) {
    const text = textOf(sub);
    check('components/subtest', text.includes('lower is better'), 'a lower-is-better measure does not say so');
    check('components/subtest', text.includes('Left out'), 'the legacy run without the subtest was not counted out');
  }

  // No direction recorded: said, not implied.
  const undirected = await renderComponentsWith('components/no-direction', {
    distribution: { ...latency, direction: null, direction_note: 'No direction is recorded for this measure on some or all of these runs, so this view does not say which end is better.' },
    heatmap: heat,
  });
  if (undirected) {
    const text = textOf(undirected);
    check('components/no-direction', text.includes('neither end is called better') && !text.includes('lower is better'),
      'a measure with no direction implies one');
  }

  // One machine: one bin, no quantile marks to collide, both ends the same.
  const one = overall.values[0];
  const single = {
    ...overall, machines: 1, included: 1, left_out: [], values: [{ ...one, bin: 0 }],
    bins: [{ lo: one.value, hi: one.value, machines: 1 }], p10: one.value, median: one.value, p90: one.value,
    configurations: [{ ...overall.configurations[0], machines: 1 }], overall_machines: 1,
  };
  await renderComponentsWith('components/single', {
    distribution: single, heatmap: { ...heat, machines: 1, shown: 1, rows: heat.rows.slice(0, 1) },
  });

  // Nobody in view.
  const empty = await renderComponentsWith('components/empty', {
    distribution: {
      ...overall, machines: 0, included: 0, left_out: [], values: [], bins: [], p10: null, median: null, p90: null,
      config: null, config_label: null, configurations: [], overall_machines: 0, measures: [], dimensions: ['model', 'cpu', 'os'],
    },
    heatmap: { ...heat, machines: 0, shown: 0, rows: [], components: [], weak: 0, without_peers: 0 },
  });
  if (empty) {
    const text = textOf(empty);
    check('components/empty', text.includes('No machine in view has this measure') && text.includes('No machines in view'),
      'an empty view did not say so');
  }

  // A large estate: capped, sorted on a column, with every kind of cell.
  const template = heat.rows[0];
  const comps = heat.components;
  const deltas = [-31, -19, -8, -2, 0, 3, 9, 17, 40, null];
  const bigRows = Array.from({ length: 200 }, (_, i) => ({
    ...template,
    key: `SN-HEAT-${i}`,
    hostname: i === 3 ? 'WORKSTATION-WITH-A-VERY-LONG-HOSTNAME-07' : `HEAT-${String(i).padStart(3, '0')}`,
    cohort_delta_pct: deltas[i % deltas.length],
    cells: comps.map((c, j) => ({
      component: c,
      measured: (i + j) % 13 !== 0,
      score: 400 + ((i * 37 + j * 91) % 1300),
      grade: ['S', 'A', 'B', 'C', 'D', 'F'][(i + j) % 6],
      cohort_delta_pct: deltas[(i + j * 3) % deltas.length],
      weak: (i + j) % 17 === 0,
    })),
  }));
  const big = { ...heat, machines: 1340, shown: 200, cap: 200, rows: bigRows, weak: 31, without_peers: 12, sort: comps[0], dir: 'asc' };
  const nf = new Intl.NumberFormat(undefined, { maximumFractionDigits: 0 });
  const capped = await renderComponentsWith('components/capped', { distribution: overall, heatmap: big });
  if (capped) {
    const text = textOf(capped);
    check('components/capped', text.includes(`Showing 200 of ${nf.format(1340)} machines`) && text.includes('Filter to narrow'),
      'the cap is not stated');
    check('components/capped', heatRows(capped).length === 200, `${heatRows(capped).length} row(s) for 200`);
    check('components/capped', text.includes('weak') && text.includes('no peers') && text.includes('—'),
      'a weak, a no-peer or an unmeasured cell is missing');
    // Every cell carries its value for a screen reader, not just a colour.
    let unlabelled = 0;
    walk(capped, (n) => {
      if (n.tagName === 'td' && String(n.className).startsWith('heat') && !n.attrs['aria-label']) unlabelled += 1;
    });
    check('components/capped', unlabelled === 0, `${unlabelled} heatmap cell(s) without an accessible label`);
    let sorted = null;
    walk(capped, (n) => { if (n.tagName === 'th' && n.attrs['aria-sort'] === 'ascending') sorted = textOf(n); });
    check('components/capped', sorted && sorted.startsWith(comps[0]), `the sorted column is not marked: ${sorted}`);
  }
  await renderComponentsWith('components/grade', { distribution: overall, heatmap: { ...big, encoding: 'grade' }, settings: { encoding: 'grade' } });

  // A split, with a thin group and the machines with no value; and a bin chosen.
  const split = {
    ...overall,
    split: {
      by: 'tag:site',
      groups: [
        { value: 'glasgow', machines: 9, median: 1100, p10: 900, p90: 1300, thin: false },
        { value: 'edinburgh', machines: 2, median: 800, p10: 760, p90: 840, thin: true },
        { value: null, machines: 5, median: 1000, p10: 950, p90: 1200, thin: false },
      ],
    },
  };
  const splitView = await renderComponentsWith('components/split', { distribution: split, heatmap: heat, settings: { split: 'tag:site', bin: 0 } });
  if (splitView) {
    const text = textOf(splitView);
    check('components/split', text.includes('(untagged)') && text.includes('left off'), 'the split did not explain a thin group');
    check('components/split', !text.includes('At each end'), 'a chosen bin still showed both ends');
  }

  // The server refusing a hand-edited measure: its reason and the way back,
  // with the heatmap unaffected.
  const refused = await renderComponentsWith('components/refused', {
    distribution: new Error('no machine in view has a component called "floppy". Those in view: cpu, disk'),
    heatmap: heat,
  });
  if (refused) {
    const text = textOf(refused);
    check('components/refused', text.includes('called "floppy"') && text.includes('overall score instead'),
      "the server's reason and the way back were not shown");
    check('components/refused', text.includes('Every machine, by component'), 'one refusal took the heatmap with it');
  }
  Object.assign(mod.state, { measure: null, config: null, split: null, bin: null, encoding: null, hsort: null, hdir: null });
  globalThis.fetch = () => Promise.reject(new Error('no network in the geometry check'));
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
    check('machine', !text.includes('Judged by'), 'a machine on the fleet-wide lines claims an override');
    // None of the fixtures was taken by a loadbearer that records the run,
    // the OS detail or the memory, and this payload has no `memory_modules`
    // at all, as from a server older than them: nothing is invented.
    check('machine', text.includes('layout not reported'), 'a run without a memory layout does not say so');
    check('machine', !text.includes('Run ID') && !text.includes('took '), 'a duration or run ID was invented');
    check('machine', !text.includes(' slots'), 'a slot count was invented');
  } catch (err) {
    failures.push(`machine: threw ${err.message}`);
  }
}

// The run, OS-detail and memory blocks of a loadbearer release after 1.8.1,
// none of which the fixtures carry: every block, a partial Linux one, and a
// mixed set of modules too many kinds for one line — each against the same
// real machine, with only those fields set.
{
  const m0 = snapshot.machines.find((x) => (x.os || '').startsWith('Windows 11')) || snapshot.machines[0];
  const dimm = (size, type, ff, rated, running) => ({
    position: 0, size_bytes: size, type, form_factor: ff, speed_mts: rated, configured_speed_mts: running,
  });
  const GiB = 1024 ** 3;
  const cases = [
    {
      name: 'machine/enriched',
      machine: {
        ...m0, os: 'Windows 11 Enterprise', os_edition: 'Enterprise', os_version: '24H2', os_build: '26100.2314',
        os_distro: null, memory_slots: 4, memory_populated: 2, memory_types: ['DDR5'],
        run_uuid: '0f8b5a52-7c1e-4c39-9d0e-5a1f3b2c4d6e', run_duration_secs: 472.318,
      },
      modules: [dimm(16 * GiB, 'DDR5', 'SODIMM', 5600, 4800), dimm(16 * GiB, 'DDR5', 'SODIMM', 5600, 4800)],
      want: ['Windows 11 Enterprise · 24H2 · build 26100.2314',
        '2 of 4 slots · 2 × 16 GiB DDR5 SODIMM, 5600 MT/s (running 4800)',
        'took 7m 52s', '0f8b5a52-7c1e-4c39-9d0e-5a1f3b2c4d6e', 'Took'],
      not: ['Enterprise · Enterprise', 'layout not reported'],
    },
    {
      // Linux as root: the distribution already in `os`, a variant that is
      // not, one slot empty and a module whose firmware gave no speed, and
      // no run ID.
      name: 'machine/linux',
      machine: {
        ...m0, os: 'Linux (Ubuntu 26.04)', os_distro: 'ubuntu', os_version: '26.04', os_edition: 'server',
        os_build: null, memory_slots: 2, memory_populated: 1, memory_types: ['DDR4'],
        run_uuid: null, run_duration_secs: 95.5,
      },
      modules: [dimm(8 * GiB, 'DDR4', 'DIMM', null, null)],
      want: ['Linux (Ubuntu 26.04) · server', '1 of 2 slots · 8 GiB DDR4 DIMM', 'took 1m 36s'],
      not: ['ubuntu', '· build', 'Run ID', 'MT/s', 'layout not reported'],
    },
    {
      // Unprivileged Linux: os-release, and no memory block at all.
      name: 'machine/linux-user',
      machine: {
        ...m0, os: 'Linux (Debian GNU/Linux 13)', os_distro: 'debian', os_version: '13', os_edition: null,
        os_build: null, memory_slots: null, memory_populated: null, memory_types: [],
        run_uuid: null, run_duration_secs: null,
      },
      modules: [],
      want: ['Linux (Debian GNU/Linux 13)', 'layout not reported'],
      not: ['debian', 'took ', ' slots'],
    },
    {
      name: 'machine/mixed-memory',
      machine: {
        ...m0, memory_slots: 4, memory_populated: 4, memory_types: ['DDR4'], run_duration_secs: 3725,
      },
      modules: [dimm(16 * GiB, 'DDR4', 'DIMM', 3200, 2666), dimm(16 * GiB, 'DDR4', 'DIMM', 3200, 2666),
        dimm(8 * GiB, 'DDR4', 'DIMM', 2666, 2666), dimm(512 * 1024 ** 2, 'DDR4', 'DIMM', 2400, null)]
        .map((x, i) => ({ ...x, position: i })),
      want: ['4 of 4 slots · 4 modules of 3 kinds, listed below', 'Memory', 'Rated', 'Running', '512 MiB',
        '2400 MT/s', 'took 1h 02m'],
      not: [],
    },
  ];
  const before = { view: mod.state.view, tables: mod.state.tables };
  for (const c of cases) {
    const history = [
      { grade: m0.grade, tool_version: '1.8.1', partial: false, thermal_limited: null, on_ac: null,
        comparability: m0.comparability, run_id: 1, taken_at: '2026-05-01T09:00:00Z', score: 1000,
        source_path: 'a.json', duration_secs: null, run_uuid: null },
      { grade: c.machine.grade, tool_version: c.machine.tool_version, partial: false, thermal_limited: null,
        on_ac: null, comparability: c.machine.comparability, run_id: c.machine.run_id, taken_at: c.machine.taken_at,
        score: c.machine.score, source_path: c.machine.source_path, duration_secs: c.machine.run_duration_secs,
        run_uuid: c.machine.run_uuid },
    ];
    globalThis.fetch = () => Promise.resolve({
      ok: true,
      json: () => Promise.resolve({
        machine: c.machine, flags: [], cohort: null, history, subtests: [], drives: [], memory_modules: c.modules,
      }),
      text: () => Promise.resolve(''),
    });
    // The history as its table, which is where each run's duration is.
    Object.assign(mod.state, { view: 'machine', tables: { ...before.tables, machine: ['history'] } });
    const h = host(1200);
    try {
      await mod.renderMachine(h, c.machine.key);
      const text = textOf(h);
      for (const w of c.want) check(c.name, text.includes(w), `"${w}" is missing`);
      for (const w of c.not) check(c.name, !text.includes(w), `"${w}" should not be there`);
      check(c.name, !text.includes('undefined') && !text.includes('NaN') && !text.includes('null'),
        'undefined, NaN or null reached the page');
      auditTree(c.name, h);
    } catch (err) {
      failures.push(`${c.name}: threw ${err.message}`);
    }
  }
  Object.assign(mod.state, before);
}

// What a run made with `--collect` adds — uptime, elevation, security — in
// each of its states, none of which the fixtures carry: every shape the CLI
// writes, asked and unreadable, a value this build has never heard of, and
// not asked at all, with and without another machine in view that did ask.
// "Not asked" must leave no trace on the page but the one hint, and that hint
// is drawn only from the snapshot the caller was given.
{
  const win = snapshot.machines.find((x) => (x.os || '').startsWith('Windows 11')) || snapshot.machines[0];
  const lin = snapshot.machines.find((x) => (x.os || '').startsWith('Linux')) || snapshot.machines[0];
  const NONE = {
    run_collect: [], run_elevated: null, booted_at: null, uptime_secs: null, firmware_mode: null, secure_boot: null,
    tpm_present: null, tpm_version: null, disk_encryption: null, disk_encryption_method: null,
  };
  const ALL = ['elevation', 'security', 'uptime'];
  const NEVER = ['unencrypted', 'not encrypted', 'undefined', 'NaN', 'null'];
  const cases = [
    {
      name: 'machine/optin-windows',
      machine: {
        ...win, ...NONE, run_collect: ALL, run_elevated: false, booted_at: '2026-09-09T10:41:28Z', uptime_secs: 14492,
        firmware_mode: 'uefi', secure_boot: true, tpm_present: true, tpm_version: '2.0',
      },
      want: ['UEFI · Secure Boot on · TPM 2.0 · disk encryption not read on Windows', 'up 4h 01m at the time of the run',
        'last restarted', 'not elevated'],
      not: ['Secure Boot off', 'booted ', 'could not be read', '--collect'],
      dts: ['Security', 'Uptime'],
    },
    {
      name: 'machine/optin-linux-luks',
      machine: {
        ...lin, ...NONE, run_collect: ALL, run_elevated: true, booted_at: '2026-09-06T08:00:00Z', uptime_secs: 266400,
        firmware_mode: 'uefi', secure_boot: false, tpm_present: true, tpm_version: '2.0',
        disk_encryption: 'encrypted', disk_encryption_method: 'luks2',
      },
      want: ['UEFI · Secure Boot off · TPM 2.0 · root filesystem on dm-crypt (LUKS2)', 'up 3d 2h at the time of the run',
        'booted ', ' · elevated'],
      not: ['last restarted', 'not elevated', 'not read on Windows', 'elevation not reported'],
      dts: ['Security', 'Uptime'],
    },
    {
      // Only security asked: no Uptime row, nothing about elevation.
      name: 'machine/optin-none-detected',
      machine: {
        ...lin, ...NONE, run_collect: ['security'], firmware_mode: 'uefi', secure_boot: true, tpm_present: false,
        disk_encryption: 'none_detected',
      },
      want: ['Secure Boot on', 'no TPM exposed to the OS',
        'no dm-crypt detected under / (other kinds of disk encryption are not looked for)'],
      not: ['elevat', 'up ', 'Secure Boot off'],
      dts: ['Security'],
      noDts: ['Uptime'],
    },
    {
      name: 'machine/optin-legacy-bios',
      machine: {
        ...lin, ...NONE, run_collect: ['security', 'uptime'], firmware_mode: 'bios', tpm_present: true, tpm_version: '1.2',
        disk_encryption: 'none_detected', booted_at: '2026-08-30T07:15:00Z', uptime_secs: 958000,
      },
      want: ['legacy BIOS boot · Secure Boot does not apply · TPM 1.2', 'up 11d 2h at the time of the run'],
      not: ['Secure Boot off', 'Secure Boot on', 'Secure Boot not reported', 'elevat'],
      dts: ['Security', 'Uptime'],
    },
    {
      // All three asked, nothing readable: each says so, and none is "off".
      name: 'machine/optin-unreadable',
      machine: { ...lin, ...NONE, run_collect: ALL },
      want: ['asked for, but none of it could be read', 'not reported', 'elevation not reported'],
      not: ['Secure Boot off', 'no TPM', 'none_detected', 'not elevated'],
      dts: ['Security', 'Uptime'],
    },
    {
      // Unelevated Linux, without the root-only reads: the one sentence that
      // says why they are missing.
      name: 'machine/optin-unelevated-linux',
      machine: {
        ...lin, ...NONE, run_collect: ['elevation'], run_elevated: false, serial: null, memory_slots: null,
        memory_populated: null,
      },
      want: ['not elevated, so drive health, the serial and the memory layout — root-only on Linux — could not be read'],
      not: ['UEFI', 'Secure Boot'],
      noDts: ['Security', 'Uptime'],
    },
    {
      // Values this build has never heard of are shown as themselves.
      name: 'machine/optin-future',
      machine: {
        ...lin, ...NONE, run_collect: ['network', 'security'], firmware_mode: 'coreboot', secure_boot: true,
        tpm_present: true, tpm_version: '3.0', disk_encryption: 'partial', disk_encryption_method: 'zfs',
      },
      want: ['firmware coreboot · Secure Boot on · TPM 3.0 · root filesystem: partial'],
      not: ['ZFS', 'dm-crypt'],
      dts: ['Security'],
    },
    {
      // Never asked, and nobody else in view asked either: nothing at all.
      name: 'machine/not-asked',
      machine: { ...win, ...NONE },
      others: [],
      want: [],
      not: ['--collect', 'UEFI', 'Secure Boot', 'TPM', 'elevat', 'not collected', 'uptime', 'up '],
      noDts: ['Security', 'Uptime'],
    },
    {
      // Never asked, while another machine in view did: the one quiet line.
      name: 'machine/not-asked-hint',
      machine: { ...win, ...NONE },
      others: [{ ...lin, ...NONE, key: 'OTHER-IN-VIEW', run_collect: ['security', 'uptime'] }],
      want: ['Other machines in view also report their security posture and uptime.',
        '--collect security,uptime on the loadbearer command line adds them to a run.'],
      not: ['Secure Boot', 'TPM', 'not collected', 'elevat'],
      noDts: ['Security', 'Uptime'],
      hint: 1,
    },
  ];
  const before = { snap: mod.state.snap, view: mod.state.view };
  const dtsOf = (h) => findAll(h, (n) => n.tagName === 'dt').map((n) => n.textContent);
  for (const c of cases) {
    // The hint reads the snapshot in hand; without `others` it is the real
    // one, where none of the fixtures asked for anything.
    mod.state.snap = { ...snapshot, machines: [c.machine, ...(c.others || snapshot.machines.filter((x) => x.key !== c.machine.key))] };
    globalThis.fetch = () => Promise.resolve({
      ok: true,
      json: () => Promise.resolve({
        machine: c.machine, flags: [], cohort: null, history: [], subtests: [], drives: [], memory_modules: [],
      }),
      text: () => Promise.resolve(''),
    });
    mod.state.view = 'machine';
    const h = host(1200);
    try {
      await mod.renderMachine(h, c.machine.key);
      const text = textOf(h);
      const dts = dtsOf(h);
      for (const w of c.want) check(c.name, text.includes(w), `"${w}" is missing`);
      for (const w of [...c.not, ...NEVER]) check(c.name, !text.includes(w), `"${w}" should not be there`);
      for (const d of c.dts || []) check(c.name, dts.includes(d), `no ${d} row`);
      for (const d of c.noDts || []) check(c.name, !dts.includes(d), `a ${d} row for something not asked`);
      const hints = findAll(h, (n) => n.dataset?.hint === 'collect').length;
      check(c.name, hints === (c.hint || 0), `${hints} collect hint(s), not ${c.hint || 0}`);
      auditTree(c.name, h);
    } catch (err) {
      failures.push(`${c.name}: threw ${err.message}`);
    }
  }
  Object.assign(mod.state, before);

  // A posture filter that matches nothing says what it can and cannot match,
  // rather than that it names something no longer in the fleet.
  const saved = { snap: mod.state.snap, filter: mod.state.filter, view: mod.state.view };
  mod.state.snap = { ...snapshot, machines: [], flags: [], summary: { ...snapshot.summary, machines: 0 } };
  mod.state.filter = { posture: 'secure_boot_off' };
  mod.state.view = 'machines';
  const notice = mod.emptyFilterNotice();
  check('posture/empty', !!notice && textOf(notice).includes('whose latest run read: Secure Boot off')
    && textOf(notice).includes('are on neither side of it') && !textOf(notice).includes('no longer in the fleet'),
  `the empty posture view does not say why: ${notice ? textOf(notice) : 'no notice'}`);
  Object.assign(mod.state, saved);
  notes.push(`opt-in: ${cases.length} machine page states`);
}

// Machines an override judges against other lines, from a snapshot the binary
// built with a [thresholds] section. "64 days old, nothing stale" has to read
// as judged against 30 days rather than as fine by the fleet's 90, so the
// drilldown says which rule and which line, and every view that quotes a
// fleet-wide value admits the machines on another.
{
  const judged = realSnapshot([
    '[[thresholds.overrides]]',
    'name = "Edinburgh, measured monthly"',
    'tags = { site = "edinburgh" }',
    'stale_days = 30',
    'disk_free_pct = 7.5',
    '[[thresholds.overrides]]',
    'tags = { site = "glasgow" }',
    'replace_age_years = 3',
    '',
  ].join('\n'));
  const m = judged.machines.find((x) => x.hostname === 'FLEET-LNX-01');
  check('machine/override', m && m.overridden_by?.[0]?.name === 'Edinburgh, measured monthly',
    `FLEET-LNX-01 was not judged by the override: ${JSON.stringify(m?.overridden_by)}`);
  check('machine/override', judged.threshold_overrides?.length === 2, 'the snapshot does not carry the rules');
  if (m) {
    globalThis.fetch = () => Promise.resolve({
      ok: true,
      json: () => Promise.resolve({
        machine: m, flags: judged.flags.filter((f) => f.machine_key === m.key), cohort: null, history: [], subtests: [],
      }),
      text: () => Promise.resolve(''),
    });
    const h = host(1200);
    try {
      await mod.renderMachine(h, m.key);
      const text = textOf(h);
      check('machine/override', text.includes('Judged by')
        && text.includes('Edinburgh, measured monthly: disk nearly full under 7.5% free, stale after 30 days'),
      'the drilldown does not say which override judged it, or against what');
      check('machine/override', !text.includes('undefined') && !text.includes('NaN'), 'undefined or NaN reached the page');
    } catch (err) {
      failures.push(`machine/override: threw ${err.message}`);
    }
  }

  globalThis.fetch = () => Promise.reject(new Error('no network in the geometry check'));
  mod.state.snap = judged;
  for (const [name, render, want] of [
    ['overview/override', (h) => mod.renderOverview(h), '2 machine(s) on a line of their own'],
    ['lifecycle/override', (h) => mod.renderLifecycle(h), 'old from another age'],
  ]) {
    const h = host(1200);
    try {
      await render(h);
      const text = textOf(h);
      check(name, text.includes(want), `the fleet-wide value is quoted without admitting the override (${want})`);
      check(name, !text.includes('undefined') && !text.includes('NaN'), 'undefined or NaN reached the page');
    } catch (err) {
      failures.push(`${name}: threw ${err.message}`);
    }
  }
  mod.state.snap = snapshot;
  notes.push(`override: ${judged.machines.filter((x) => x.overridden_by.length).length} machine(s) judged by `
    + `${judged.threshold_overrides.length} rule(s)`);
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

/* ---------------------------------------------------------- printed report */

/*
 * The report view lays the estate out as a document from the calls the tabs
 * make. Rendered from what the binary says — the snapshot, `report --changes
 * 4 --json` and `report --by tag:site --json` over the fixtures — then from an
 * empty estate the binary indexed from an empty folder, then from a synthetic
 * estate long enough to hit the caps. What is asserted is the structure: the
 * sections, in order, the period and the filter stated, the caps said in
 * words, and no control left in the document that print would show.
 */
function realEmpty() {
  const target = process.env.CARGO_TARGET_DIR || 'target';
  const exe = join(target, 'debug', process.platform === 'win32' ? 'loadbearer-fleet.exe' : 'loadbearer-fleet');
  const dir = join(tmpdir(), `lbf-check-empty-${process.pid}`);
  const db = `${dir}.db`;
  mkdirSync(dir, { recursive: true });
  const run = (a) => {
    const r = spawnSync(exe, ['--index', db, '--log-level', 'warn', ...a], { encoding: 'utf8' });
    if (r.status !== 0) {
      console.error(`\n${exe} ${a.join(' ')} failed:\n${r.stderr || r.stdout}`);
      process.exit(1);
    }
    return r.stdout;
  };
  try {
    run(['scan', dir]);
    return {
      snap: JSON.parse(run(['report', '--json'])),
      changes: JSON.parse(run(['report', '--changes', '4', '--json'])),
    };
  } finally {
    for (const suffix of ['', '-wal', '-shm']) {
      try { rmSync(db + suffix); } catch { /* already gone */ }
    }
    try { rmSync(dir, { recursive: true }); } catch { /* already gone */ }
  }
}

async function renderReportWith(name, { snap, changes, timeline, grouping = null, settings = {} }) {
  const seen = [];
  globalThis.fetch = (url) => {
    const u = String(url);
    seen.push(u);
    const body = u.startsWith('/api/changes') ? changes
      : u.startsWith('/api/timeline') ? timeline
        : u.startsWith('/api/groups') ? grouping : null;
    if (body instanceof Error) return Promise.resolve({ ok: false, text: () => Promise.resolve(body.message) });
    return Promise.resolve(body
      ? { ok: true, json: () => Promise.resolve(body), text: () => Promise.resolve('') }
      : { ok: false, text: () => Promise.resolve('unexpected request') });
  };
  globalThis.location = { hash: '', origin: 'http://fleet.example' };
  Object.assign(mod.state, { snap, filter: {}, reportWindow: 4, reportBy: null, ...settings });
  const h = host(820);
  try {
    await mod.renderReport(h);
  } catch (err) {
    failures.push(`${name}: threw ${err.message}`);
    return null;
  }
  const text = textOf(h);
  check(name, !text.includes('undefined'), 'the word "undefined" reached the page');
  check(name, !text.includes('NaN'), 'the word "NaN" reached the page');
  check(name, !text.includes('[object'), 'an object reached the page as text');
  check(name, !text.includes('Loading…'), 'a placeholder was left behind');
  check(name, !text.includes('Show all'), 'a list collapsed behind a button, which paper cannot press');
  // Every section, in the order the brief for a review asks for them.
  const order = ['Fleet report', 'Summary', 'What changed in', 'Findings to act on', 'What to do about each machine',
    ...(settings.reportBy ? ['By '] : []), 'Over time', 'Generated '];
  let at = -1;
  for (const heading of order) {
    const i = text.indexOf(heading, at + 1);
    check(name, i > at, `"${heading}" is missing or out of order`);
    if (i > at) at = i;
  }
  // The same calls the tabs make, and only those: nothing re-derived.
  check(name, seen.some((u) => u.startsWith(`/api/changes?`) && u.includes(`window=${settings.reportWindow || 4}`)),
    `"What changed" was not asked for the report's period: ${seen.join(' ')}`);
  check(name, seen.some((u) => u.startsWith('/api/timeline')), 'the timeline was not asked for');
  check(name, seen.some((u) => u.startsWith('/api/groups')) === !!settings.reportBy,
    `groups were ${settings.reportBy ? 'not ' : ''}asked for: ${seen.join(' ')}`);
  // The only controls a printed page could show are in the toolbar, which
  // print drops, or are `.btn`s, which print drops too; a machine's name is
  // a link-button, which prints as its text.
  const bar = new Set();
  walk(h, (n) => { if (n.className === 'report-toolbar') walk(n, (k) => bar.add(k)); });
  walk(h, (n) => {
    const cls = String(n.className || '');
    if ((n.tagName === 'button' || n.tagName === 'select') && !bar.has(n)
      && !cls.includes('link-button') && !cls.split(' ').includes('btn')) {
      failures.push(`${name}: a ${n.tagName} "${textOf(n).slice(0, 40)}" would print`);
    }
  });
  notes.push(`${name}: ${auditTree(name, h)} chart(s), ${text.length} chars of text`);
  return { h, text };
}

{
  const fixtures = await renderReportWith('report/fixtures', {
    snap: snapshot, changes: realChanges(4), timeline: weeksOf(27),
  });
  if (fixtures) {
    const { text } = fixtures;
    check('report/fixtures', text.includes('Every machine, no filter applied.'), 'the filter is not stated');
    // The line as the report writes it: where it came from, then the build.
    check('report/fixtures', text.includes(`${globalThis.location.origin} · loadbearer-fleet ${snapshot.version}`),
      'the source and the build are not stated');
    const live = snapshot.flags.filter((f) => !f.triage && f.severity !== 'info');
    check('report/fixtures', text.includes(`${live.length} live critical or warning finding(s)`),
      'the live findings are not counted as the snapshot counts them');
    for (const f of live) {
      check('report/fixtures', text.includes(f.detail), `a live finding's detail is missing: ${f.code}`);
    }
    for (const f of snapshot.flags.filter((x) => x.severity === 'info')) {
      check('report/fixtures', !text.includes(f.detail), `an informational finding was listed: ${f.code}`);
    }
    check('report/fixtures', text.includes(`${snapshot.summary.info} informational`), 'informational ones are not counted');
    check('report/fixtures', text.includes('Fleet size and grade mix'), 'the over-time charts are missing');
  }

  // Every machine opted in, posture and all: the report carries none of it.
  // It is a document for a review meeting, and a printed list of which
  // machines lack Secure Boot is what the CLI's opt-in exists to keep from
  // lying around.
  const opted = {
    ...snapshot,
    machines: snapshot.machines.map((m) => ({
      ...m, run_collect: ['elevation', 'security', 'uptime'], run_elevated: false, booted_at: '2026-09-09T10:41:28Z',
      uptime_secs: 14492, firmware_mode: 'uefi', secure_boot: false, tpm_present: false, tpm_version: null,
      disk_encryption: 'none_detected', disk_encryption_method: null,
    })),
  };
  const postured = await renderReportWith('report/opted-in', {
    snap: opted, changes: realChanges(4), timeline: weeksOf(27),
  });
  if (postured) {
    for (const w of ['Secure Boot', 'TPM', 'dm-crypt', 'UEFI', 'elevat', 'uptime', 'Uptime', '--collect']) {
      check('report/opted-in', !postured.text.includes(w), `"${w}" reached the printed report`);
    }
  }

  const grouped = await renderReportWith('report/grouped', {
    snap: snapshot, changes: realChanges(13), timeline: weeksOf(27), grouping: realGrouping('tag:site'),
    settings: { reportBy: 'tag:site', reportWindow: 13, filter: {} },
  });
  if (grouped) {
    check('report/grouped', grouped.text.includes('By tag: site') && grouped.text.includes('glasgow'),
      'the groups are not laid out');
    check('report/grouped', grouped.text.includes('What changed in 13 weeks'), 'the period is not stated');
  }

  const empty = realEmpty();
  check('report/empty', empty.snap.machines.length === 0, 'the empty folder produced machines');
  const none = await renderReportWith('report/empty', {
    snap: empty.snap, changes: empty.changes, timeline: { generated_at: empty.snap.generated_at, weeks: 26, points: [] },
  });
  if (none) {
    for (const want of ['0 machine(s)', 'Nothing at warning or above in view.',
      'Nothing in view to replace, repair or investigate.', 'No data in view yet.']) {
      check('report/empty', none.text.includes(want), `missing ${JSON.stringify(want)}`);
    }
  }

  // Past the cap: the list stops at REPORT_ROWS and says how many it left off,
  // and where they are. Built from the fixtures' own findings and machines.
  const many = structuredClone(snapshot);
  const template = snapshot.flags.find((f) => f.severity !== 'info') || snapshot.flags[0];
  const base = snapshot.machines[0];
  many.machines = Array.from({ length: 70 }, (_, i) => ({
    ...base, key: `K-${i}`, hostname: `PC-${String(i).padStart(3, '0')}`,
    lifecycle: { ...base.lifecycle, verdict: ['replace', 'repair', 'investigate'][i % 3], reasons: [`reason ${i}`] },
  }));
  many.flags = many.machines.map((m, i) => ({
    ...template, machine_key: m.key, hostname: m.hostname, severity: i % 2 ? 'warning' : 'critical', triage: null,
    detail: `detail ${i}`,
  }));
  const capped = await renderReportWith('report/capped', {
    snap: many, changes: realChanges(4), timeline: weeksOf(27), settings: { filter: { tags: 'site=glasgow', q: 'PC' } },
  });
  if (capped) {
    const more = 70 - mod.REPORT_ROWS;
    check('report/capped', capped.text.includes(`And ${more} more — see the findings CSV.`), 'the findings cap is not said');
    check('report/capped', capped.text.includes(`And ${more} more — see the Lifecycle tab, or the machines CSV.`),
      'the lifecycle cap is not said');
    check('report/capped', capped.text.includes(`detail ${mod.REPORT_ROWS - 1}`)
      && !capped.text.includes(`detail ${mod.REPORT_ROWS}`), 'the cap is not where it says');
    check('report/capped', capped.text.includes('tagged site=glasgow') && capped.text.includes('matching “PC”'),
      'the filter is not stated');
    let rows = 0;
    walk(capped.h, (n) => { if (n.tagName === 'tbody') rows = Math.max(rows, n.children.length); });
    check('report/capped', rows === mod.REPORT_ROWS, `a table holds ${rows} rows, not ${mod.REPORT_ROWS}`);
  }
  mod.state.snap = snapshot;
  mod.state.filter = {};
}

/*
 * The CSV links carry the filter the page was drawn with, so the file is the
 * view on screen: the machine table's and the queue's, and the overview's
 * link to the report, which carries it too.
 */
{
  const hrefs = (h) => {
    const out = [];
    walk(h, (n) => { if (n.tagName === 'a') out.push(n.attrs.href); });
    return out;
  };
  mod.state.filter = { tags: 'site=edinburgh', lifecycle: 'watch' };
  const machines = host(1200);
  mod.renderMachines(machines);
  check('csv/machines', hrefs(machines).includes('/api/machines.csv?tags=site%3Dedinburgh&lifecycle=watch'),
    `the download does not carry the filter: ${hrefs(machines).join(' ')}`);
  let download = false;
  walk(machines, (n) => { if (n.tagName === 'a' && 'download' in n.attrs) download = true; });
  check('csv/machines', download, 'the link is not a download');

  globalThis.fetch = () => Promise.resolve({ ok: false, text: () => Promise.resolve('not in this check') });
  const overview = host(1200);
  await mod.renderOverview(overview);
  const links = hrefs(overview);
  check('csv/findings', links.includes('/api/findings.csv?tags=site%3Dedinburgh&lifecycle=watch'),
    `the queue's download does not carry the filter: ${links.join(' ')}`);
  check('csv/findings', textOf(overview).includes('snoozed and suppressed ones too'),
    'the queue does not say the file holds what it hides');
  check('report/link', links.includes('#/report?tags=site%3Dedinburgh&lifecycle=watch'),
    `the overview's report link does not carry the filter: ${links.join(' ')}`);
  notes.push(`csv: ${links.length} link(s) on the overview, the machine table's download carries the filter`);
  mod.state.filter = {};
}

/* ------------------------------------------------------- the URL is the view */

/*
 * Every piece of view state on every tab, set, written to a hash, and that
 * hash loaded fresh — through `applyHash`, the half of `route` that reads the
 * address — into a page that remembers nothing. What comes back must be what
 * went in, and the view drawn from it must show it: the sorted column, the
 * card turned to its table, the toggle pressed. Then the links made before
 * any of this existed, which must still read and write as they did.
 */
const BLANK = {
  key: null, filter: {}, runs: [], by: null, window: 1, decision: null, reportWindow: 4, reportBy: null,
  measure: null, config: null, split: null, bin: null, encoding: null, hsort: null, hdir: null,
  showInfo: false, showQuiet: false, sorts: {}, tables: [],
};
const freshState = () => Object.assign(mod.state, { ...BLANK, view: 'overview', sorts: {}, tables: {}, me: null });
/** What a view's hash is responsible for, so other tabs' settings don't count. */
function viewFacts(v) {
  const own = {
    overview: ['window', 'showInfo', 'showQuiet'], groups: ['by'], lifecycle: ['decision'],
    components: ['measure', 'config', 'split', 'bin', 'encoding', 'hsort', 'hdir'],
    report: ['reportWindow', 'reportBy'], machine: ['key'], compare: ['runs'],
  }[v.view] || [];
  return JSON.stringify({
    view: v.view, filter: v.filter, tables: [...v.tables].sort(),
    sorts: Object.fromEntries(Object.entries(v.sorts).sort()),
    ...Object.fromEntries(own.map((k) => [k, v[k]])),
  });
}
{
  const AUDIT = [
    { view: 'overview', filter: { tags: 'site=glasgow', q: 'PC' }, window: 4, showInfo: true, showQuiet: true, tables: ['grades', 'mix'] },
    { view: 'cohorts', filter: { since_days: '30' }, sorts: { thin: '-machines' } },
    { view: 'groups', filter: { os: 'Windows 11' }, by: 'tag:site', sorts: { groups: '-flagged', identical: 'group' }, tables: ['identical'] },
    {
      view: 'components', measure: 'disk/seq_read', config: 'thorough|general', split: 'tag:site', bin: 0, encoding: 'grade',
      hsort: 'disk', hdir: 'desc', sorts: { bin: 'machine', split: '-median' }, tables: ['distribution'],
    },
    { view: 'lifecycle', filter: { lifecycle: 'watch' }, decision: 'watch', sorts: { decisions: '-age', age: 'score' }, tables: ['age'] },
    { view: 'machines', filter: { flag: 'stale_result', cohort: 'X|y' }, sorts: { machines: 'tag:site' } },
    { view: 'machines', filter: { tags: 'site=glasgow', posture: 'tpm_1.2' } },
    { view: 'machine', key: 'SN WIN/01', sorts: { measurements: '-score' }, tables: ['history'] },
    { view: 'compare', runs: [3, 1, 2], filter: { tags: 'site=glasgow' } },
    { view: 'report', filter: { model: 'Latitude 7440' }, reportWindow: 13, reportBy: 'tag:site', tables: ['median'] },
  ];
  for (const set of AUDIT) {
    const name = `url/${set.view}`;
    const want = { ...BLANK, ...set };
    const hash = mod.buildHash(want);
    freshState();
    mod.applyHash(hash);
    const got = {
      ...mod.state, sorts: mod.state.sorts[set.view] || {}, tables: mod.state.tables[set.view] || [],
    };
    check(name, viewFacts(got) === viewFacts(want), `${hash} reads back as ${viewFacts(got)}, not ${viewFacts(want)}`);
    // And the hash the page would write from there is the one it was given.
    check(name, mod.buildHash({ ...got, view: set.view }) === hash, `${hash} is written back differently`);
    notes.push(`${name}: ${hash}`);
  }

  // Shown, not just held: each view drawn from a hash loaded fresh.
  const sortedBy = (h) => findAll(h, (n) => n.tagName === 'th' && n.attrs['aria-sort'] && n.attrs['aria-sort'] !== 'none')
    .map((n) => `${n.textContent.replace(/ [▲▼]$/, '')} ${n.attrs['aria-sort']}`);
  const toggles = (h) => Object.fromEntries(findAll(h, (n) => n.dataset?.toggle).map((n) => [n.dataset.toggle, n.textContent]));
  const button = (h, text) => findAll(h, (n) => n.tagName === 'button' && n.textContent === text)[0];
  globalThis.fetch = () => Promise.reject(new Error('no network in the geometry check'));

  freshState();
  mod.state.snap = snapshot;
  mod.applyHash('#/machines?order.machines=-tag%3Asite');
  let h = host(1200);
  mod.renderMachines(h);
  check('url/machines/shown', sortedBy(h).join() === 'site descending', `sorted by ${sortedBy(h).join()}`);

  freshState();
  mod.applyHash('#/overview?info=1&table=grades%2Cmix');
  h = host(1200);
  await mod.renderOverview(h);
  check('url/overview/shown', !!button(h, 'Hide informational'), 'the informational findings are not shown');
  check('url/overview/shown', toggles(h).grades === 'Chart' && toggles(h).findings === 'Table',
    `cards in the wrong mode: ${JSON.stringify(toggles(h))}`);

  freshState();
  mod.applyHash('#/lifecycle?decision=watch&order.decisions=-age&table=age');
  h = host(1200);
  mod.renderLifecycle(h);
  const pressed = findAll(h, (n) => n.attrs?.['aria-pressed'] === 'true').map(textOf);
  check('url/lifecycle/shown', pressed.length === 1 && pressed[0].startsWith('Watch'), `pressed: ${pressed.join(' | ')}`);
  check('url/lifecycle/shown', sortedBy(h).includes('Age descending'), `sorted by ${sortedBy(h).join()}`);
  check('url/lifecycle/shown', toggles(h).age === 'Chart', 'the scatter is not shown as its table');

  freshState();
  mod.applyHash('#/cohorts?order.thin=-machines');
  h = host(1200);
  mod.renderCohorts(h);
  check('url/cohorts/shown', sortedBy(h).join() === 'Machines descending', `sorted by ${sortedBy(h).join()}`);

  // A header pressed writes the sort into the address in place, and pressing
  // back to the table's own order takes it out again.
  freshState();
  mod.applyHash('#/machines');
  location.hash = '#/machines';
  h = host(1200);
  mod.renderMachines(h);
  const header = (label) => findAll(h, (n) => n.tagName === 'th' && n.textContent.replace(/ [▲▼]$/, '') === label)[0];
  await fire(header('Machine'));
  check('url/sort-click', location.hash === '#/machines?order.machines=machine', `wrote ${location.hash}`);
  await fire(header('Score'));
  check('url/sort-click', location.hash === '#/machines', `the default sort is written as ${location.hash}`);

  // Every link made before this existed opens as it did, and is written back
  // byte for byte.
  for (const old of ['#/overview', '#/overview?changes=13', '#/lifecycle?decision=keep', '#/groups?tags=site%3Dglasgow&by=os',
    '#/components?measure=disk%2Fseq_read&split=tag%3Asite&bin=0&encoding=grade&sort=disk&dir=desc',
    '#/report?by=tag%3Asite', '#/compare/1,2', '#/machines?cpu=Intel%28R%29+Core%28TM%29', '#/machine/SN-WIN-0001']) {
    check('url/old-links', mod.buildHash(mod.parseHash(old)) === old, `${old} is rewritten as ${mod.buildHash(mod.parseHash(old))}`);
  }

  // A stale address: a sort on a column no table has, a card that does not
  // exist, a filter that matches nothing. Drawn in the table's own order,
  // and the empty view says why.
  freshState();
  mod.applyHash('#/machines?tags=gone%3Dx&order.machines=-tag%3Agone&table=nothing');
  mod.state.snap = { ...snapshot, machines: [], flags: [], summary: { ...snapshot.summary, machines: 0 } };
  h = host(1200);
  try {
    mod.renderMachines(h);
    check('url/stale', sortedBy(h).join() === 'Score descending', `a stale sort drew ${sortedBy(h).join()}`);
    const notice = mod.emptyFilterNotice();
    check('url/stale', !!notice && textOf(notice).includes('No machine in view matches this filter')
      && textOf(notice).includes('tagged gone=x') && !!button(notice, 'Clear the filter'),
    `the empty view does not say why: ${notice ? textOf(notice) : 'no notice'}`);
  } catch (err) {
    failures.push(`url/stale: threw ${err.message}`);
  }
  mod.state.snap = snapshot;
  freshState();
}

/* ------------------------------------------------------------- saved views */

/*
 * Saved views against a storage that lives in memory, and ones that refuse:
 * the panel laid out and its buttons pressed, the document as it is stored,
 * and every way the stored or imported document can be wrong.
 */
function memoryStorage(seed = {}) {
  const m = new Map(Object.entries(seed));
  return {
    getItem: (k) => (m.has(k) ? m.get(k) : null),
    setItem: (k, v) => { m.set(k, String(v)); },
    removeItem: (k) => { m.delete(k); },
    m,
  };
}
{
  const realStorage = globalThis.localStorage;
  const store = memoryStorage();
  globalThis.localStorage = store;
  mod.loadViews();
  check('views/load', mod.saved.status === 'ok' && mod.saved.doc.views.length === 0, `empty storage read as ${mod.saved.status}`);
  const stored = () => JSON.parse(store.getItem('lbf-views'));
  const panel = () => { const p = new FakeNode('div'); mod.renderViewsPanel(p); return p; };
  const press = async (p, action, i = 0) => {
    const b = findAll(p, (n) => n.tagName === 'button' && n.dataset?.action === action)[i];
    check('views/ui', !!b, `no ${action} button`);
    await fire(b);
    return b;
  };
  const typeAndSave = async (p, text) => {
    const input = findAll(p, (n) => n.className === 'views-input')[0];
    check('views/ui', !!input, 'no name field');
    if (!input) return;
    input.value = text;
    await fire(findAll(p, (n) => n.tagName === 'button' && n.textContent === 'Save')[0]);
  };

  // Save, through the panel.
  location.hash = '#/machines?tags=site%3Dglasgow&order.machines=machine';
  let p = panel();
  check('views/ui', textOf(p).includes('This view is not saved.'), 'the panel does not say the view is unsaved');
  await press(p, 'save');
  await typeAndSave(p, '  Glasgow by name  ');
  check('views/save', stored().views.length === 1 && stored().views[0].name === 'Glasgow by name'
    && stored().views[0].hash === mod.canonicalHash(location.hash), `stored ${JSON.stringify(stored())}`);
  check('views/save', stored().version === 1, 'the document carries no version');
  check('views/save', mod.viewsLabel().name === 'Glasgow by name' && !mod.viewsLabel().drift, 'the control does not name the view');
  check('views/save', textOf(p).includes('Saved “Glasgow by name”.'), `the panel said ${textOf(p)}`);

  // A duplicate name, an empty one, a long one: refused, with the reason.
  p = panel();
  await press(p, 'save');
  await typeAndSave(p, 'GLASGOW BY NAME');
  check('views/names', textOf(p).includes('There is already a view called “Glasgow by name”'), 'a duplicate name was not refused aloud');
  check('views/names', mod.saveView('   ') === 'Give the view a name.', 'an empty name was not refused');
  check('views/names', /at most 60 characters/.test(mod.saveView('x'.repeat(61)) || ''), 'a long name was not refused');
  check('views/names', stored().views.length === 1, 'a refused name was stored');
  mod.viewsUi.editing = null;

  // Canonical: the same view with its parameters in another order, a default
  // spelled out, and its tag pairs reversed, is the same view.
  const a = '#/overview?tags=site%3Dglasgow%2Cring%3Dpilot&info=1&changes=4';
  const b = '#/overview?changes=4&info=1&tags=ring%3Dpilot%2Csite%3Dglasgow&quiet=0&bogus=1';
  check('views/canonical', mod.canonicalHash(a) === mod.canonicalHash(b), `${mod.canonicalHash(a)} ≠ ${mod.canonicalHash(b)}`);
  check('views/canonical', mod.canonicalHash('') === mod.canonicalHash('#/overview?changes=1'), 'the bare address is not the overview');
  check('views/canonical', mod.canonicalHash(a) !== mod.canonicalHash('#/overview?changes=4'), 'a filter was lost');
  check('views/canonical', mod.saveView('Overview, pilot', a) === null, 'could not save a second view');
  location.hash = b;
  check('views/canonical', mod.viewsLabel().name === 'Overview, pilot', 'reordered parameters do not match the saved view');

  // Opening one lands on its hash, through the address; then the page moves
  // on and the control says so, with Update to hand.
  location.hash = '#/overview';
  p = panel();
  await press(p, 'open', 0);
  check('views/open', location.hash === stored().views[0].hash, `opened at ${location.hash}`);
  location.hash = '#/machines?tags=site%3Dglasgow&order.machines=-score';
  check('views/modified', mod.viewsLabel().name === 'Glasgow by name' && mod.viewsLabel().drift, 'a changed view is not marked');
  p = panel();
  check('views/modified', textOf(p).includes('Changed since you opened “Glasgow by name”.'), `the panel said ${textOf(p)}`);
  await press(p, 'update-opened');
  check('views/update', stored().views[0].hash === mod.canonicalHash(location.hash) && !mod.viewsLabel().drift,
    'Update did not take the address on screen');

  // Rename, then delete with its second step — and Keep it, which keeps it.
  p = panel();
  await press(p, 'rename', 0);
  await typeAndSave(p, 'Glasgow, worst first');
  check('views/rename', stored().views[0].name === 'Glasgow, worst first' && mod.viewsLabel().name === 'Glasgow, worst first',
    `renamed to ${stored().views[0].name}`);
  p = panel();
  await press(p, 'delete', 0);
  check('views/delete', stored().views.length === 2 && textOf(p).includes('Delete “Glasgow, worst first”?'),
    'delete did not ask first');
  await fire(findAll(p, (n) => n.tagName === 'button' && n.textContent === 'Keep it')[0]);
  check('views/delete', stored().views.length === 2, 'Keep it deleted it');
  await press(p, 'delete', 0);
  await press(p, 'delete-confirm');
  check('views/delete', stored().views.length === 1 && stored().views[0].name === 'Overview, pilot', 'delete did not delete');

  // The default: marked, opened on a bare address, never over a named one.
  p = panel();
  await press(p, 'default', 0);
  check('views/default', stored().default === 'Overview, pilot', 'the default was not stored');
  p = panel();
  check('views/default', findAll(p, (n) => String(n.className).includes('views-default')).length === 1
    && findAll(p, (n) => n.textContent === 'Unset default').length === 1, 'the default is not marked, or cannot be unset');
  mod.loadViews();
  check('views/default', mod.startingView('')?.hash === stored().views[0].hash, 'a bare address does not open the default');
  check('views/default', mod.startingView('#/machines') === null, 'the default overrode a link');
  check('views/default', mod.startingView('#/overview') === null, 'the default overrode #/overview');

  // Export, then import into an empty browser: the same views, and the
  // default stays behind, as one browser's choice.
  const file = mod.exportViews();
  const exported = JSON.parse(file);
  check('views/export', exported.version === 1 && exported.views.length === 1 && !('default' in exported), `exported ${file}`);
  store.m.clear();
  mod.loadViews();
  let r = mod.importViews(file);
  check('views/import', !r.error && r.added === 1 && JSON.stringify(stored().views) === JSON.stringify(exported.views),
    `round trip: ${JSON.stringify(r)} → ${store.getItem('lbf-views')}`);
  // The collision rule: the file's address wins, under the name already here.
  r = mod.importViews(JSON.stringify({ version: 1, views: [
    { name: 'overview, PILOT', hash: '#/overview?changes=13' }, { name: 'New one', hash: '#/groups?by=os' },
  ] }));
  check('views/import', r.added === 1 && r.replaced === 1 && stored().views[0].name === 'Overview, pilot'
    && stored().views[0].hash === '#/overview?changes=13', `merged as ${JSON.stringify(r)} → ${store.getItem('lbf-views')}`);
  // Wrong shapes are refused whole, and nothing changes.
  const before = store.getItem('lbf-views');
  for (const bad of ['not json', '[]', '{"version":2,"views":[]}', '{"version":1,"views":"x"}',
    '{"version":1,"views":[{"name":"x","hash":"https://elsewhere.example/"}]}',
    '{"version":1,"views":[{"name":"a","hash":"#/"},{"name":"A","hash":"#/"}]}',
    JSON.stringify({ version: 1, views: Array.from({ length: 51 }, (_, i) => ({ name: `v${i}`, hash: '#/' })) })]) {
    const refused = mod.importViews(bad);
    check('views/import', !!refused.error, `accepted ${bad.slice(0, 60)}`);
  }
  check('views/import', store.getItem('lbf-views') === before, 'a refused file changed the list');

  // The cap, said rather than silent — and an import that would pass it is
  // refused whole.
  store.m.clear();
  mod.loadViews();
  for (let i = 0; i < mod.VIEWS_MAX; i += 1) mod.saveView(`View ${i}`, `#/machines?q=${i}`);
  check('views/cap', stored().views.length === mod.VIEWS_MAX, `${stored().views.length} saved`);
  check('views/cap', /50 views is the most/.test(mod.saveView('One more', '#/groups') || ''), 'the cap was silent');
  r = mod.importViews(JSON.stringify({ version: 1, views: [{ name: 'Over the top', hash: '#/groups' }] }));
  check('views/cap', /Nothing was imported/.test(r.error || '') && stored().views.length === mod.VIEWS_MAX,
    'an import past the cap was not refused');

  // Corrupt storage, or a version from the future: ignored without a throw,
  // left exactly as it was, and saving over it refused.
  for (const corrupt of ['{oops', '{"version":9,"views":[]}', '{"version":1,"views":[{"name":""}]}', '"a string"']) {
    store.m.clear();
    store.setItem('lbf-views', corrupt);
    try {
      mod.loadViews();
      check('views/corrupt', mod.saved.status === 'unreadable' && mod.saved.doc.views.length === 0,
        `${corrupt} read as ${mod.saved.status}`);
      check('views/corrupt', !!mod.saveView('Anything', '#/groups'), `saved over ${corrupt}`);
      check('views/corrupt', store.getItem('lbf-views') === corrupt, `${corrupt} was overwritten`);
      p = panel();
      check('views/corrupt', textOf(p).includes('cannot read') && !findAll(p, (n) => n.dataset?.action === 'save').length,
        'the panel does not say why it cannot save');
    } catch (err) {
      failures.push(`views/corrupt: ${corrupt} threw ${err.message}`);
    }
  }
  // Discard is the deliberate way out, in two steps.
  p = panel();
  await press(p, 'discard');
  await press(p, 'discard-confirm');
  check('views/corrupt', mod.saved.status === 'ok' && store.getItem('lbf-views') === null, 'discarding did not clear them');

  // No storage at all: a browser that throws on touching it, and one whose
  // writes fail. The control still copies a link, and says why it cannot save.
  for (const [what, broken] of [
    ['throws', { getItem() { throw new Error('SecurityError'); }, setItem() { throw new Error('SecurityError'); }, removeItem() {} }],
    ['full', { getItem: () => null, setItem() { throw new Error('QuotaExceededError'); }, removeItem() {} }],
  ]) {
    globalThis.localStorage = broken;
    try {
      mod.loadViews();
      check(`views/unavailable/${what}`, mod.saved.status === 'unavailable', `read as ${mod.saved.status}`);
      check(`views/unavailable/${what}`, /not available in this browser/.test(mod.saveView('x', '#/groups') || ''),
        'saving did not say it is unavailable');
      p = panel();
      check(`views/unavailable/${what}`, textOf(p).includes('not available in this browser')
        && findAll(p, (n) => n.dataset?.action === 'copy').length === 1
        && !findAll(p, (n) => n.dataset?.action === 'save').length, 'the panel does not offer Copy link alone');
      // No clipboard here either: the link is put in a field instead.
      await press(p, 'copy');
      check(`views/unavailable/${what}`, mod.viewsUi.copyField && /copy it from there/.test(mod.viewsUi.note),
        'Copy link has no fallback');
      mod.viewsUi.copyField = false;
    } catch (err) {
      failures.push(`views/unavailable/${what}: threw ${err.message}`);
    }
  }
  // Nothing saved holds more than the address: a name and a hash per view.
  globalThis.localStorage = memoryStorage();
  mod.loadViews();
  mod.saveView('Plain', '#/machines?q=PC');
  const doc = JSON.parse(globalThis.localStorage.getItem('lbf-views'));
  check('views/contents', JSON.stringify(Object.keys(doc).sort()) === '["default","version","views"]'
    && doc.views.every((v) => JSON.stringify(Object.keys(v).sort()) === '["hash","name"]'), `stored ${JSON.stringify(doc)}`);
  notes.push('views: save, rename, update, delete, default, canonical match, export and import, the cap, '
    + 'corrupt and unavailable storage');
  globalThis.localStorage = realStorage;
  mod.saved.opened = null;
  mod.loadViews();
  location.hash = '';
}

/* ------------------------------------------------------------------ report */

for (const n of notes) console.log(`  note  ${n}`);
if (failures.length) {
  console.error(`\n${failures.length} geometry problem(s):`);
  for (const f of failures) console.error(`  FAIL  ${f}`);
  process.exit(1);
}
console.log('\nAll charts render with finite geometry, inside their box, to spec.');
