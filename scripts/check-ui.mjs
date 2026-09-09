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

/* ------------------------------------------------------------------- views */

/*
 * The snapshot comes from the binary, run over the anonymised fixtures, so this
 * check exercises the same JSON the browser gets and cannot quietly drift from
 * it. Pass --snapshot <file> to use a captured one instead.
 */
function realSnapshot() {
  const argIdx = process.argv.indexOf('--snapshot');
  if (argIdx > -1) return JSON.parse(readFileSync(process.argv[argIdx + 1], 'utf8'));

  const exe = process.platform === 'win32'
    ? 'target/debug/loadbearer-fleet.exe' : 'target/debug/loadbearer-fleet';
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
  };
  globalThis.fetch = () => Promise.resolve({
    ok: true, json: () => Promise.resolve(payload), text: () => Promise.resolve(''),
  });
  const h = host(1200);
  try {
    await mod.renderMachine(h, m.key);
    const text = textOf(h);
    check('machine', text.includes(m.hostname || m.key), 'the drilldown did not name its machine');
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

/* ------------------------------------------------------------------ report */

for (const n of notes) console.log(`  note  ${n}`);
if (failures.length) {
  console.error(`\n${failures.length} geometry problem(s):`);
  for (const f of failures) console.error(`  FAIL  ${f}`);
  process.exit(1);
}
console.log('\nAll charts render with finite geometry, inside their box, to spec.');
