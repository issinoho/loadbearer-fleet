/*
 * Lays the dashboard out at four screen sizes in a real browser and asserts
 * what came out.
 *
 * `check-ui.mjs` renders against a DOM shim, which has no CSS engine — so it
 * can prove a chart's geometry and cannot prove that the page fits the screen.
 * This is the other half. It boots the binary against the anonymised fixtures,
 * drives headless Chrome over the DevTools protocol, and for every view at
 * every size checks the three things that were actually wrong:
 *
 *   - **Nothing scrolls sideways.** The topbar was one non-wrapping flex row,
 *     so on a phone the document was 685px wide inside a 390px window: the
 *     tabs clipped and Rescan, Sign out and the theme toggle sat off-screen
 *     where only a horizontal drag could reach them.
 *   - **Nothing is painted past the right edge**, which catches the same fault
 *     hidden inside an `overflow: hidden` ancestor.
 *   - **Only elements that opted in scroll inside themselves** — the tab strip
 *     and a wide table may, a card may not.
 *
 * Plus, under touch emulation, that the controls are big enough to hit.
 *
 * Like `check-ui.mjs`, deliberately not part of `cargo test`: it needs Node and
 * a Chrome. It says so and exits 0 if there is no browser to drive, so it can
 * sit in a chain of checks without failing a machine that cannot run it.
 *
 *     cargo build && node scripts/check-mobile.mjs
 */

import { spawn, spawnSync } from 'node:child_process';
import {
  copyFileSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

/* ------------------------------------------------------------ what to check */

// 390x844 is an iPhone 13/14; 320x568 the smallest phone still in use; 768 a
// tablet in portrait; 1440 the desktop the layout was designed on, here to
// catch a narrow-screen rule that leaked upwards.
const SIZES = [
  { name: 'phone', width: 390, height: 844, touch: true },
  { name: 'small-phone', width: 320, height: 568, touch: true },
  { name: 'tablet', width: 768, height: 1024, touch: true },
  // Just above the 720px breakpoint: the *desktop* header layout at the
  // tightest width it ever has to hold. That is where a fifth tab first
  // overflowed, and where the next one will.
  { name: 'narrow-desktop', width: 740, height: 900, touch: false },
  { name: 'desktop', width: 1440, height: 900, touch: false },
];

// Three of these hold the widest thing on the site: the machines table, a
// drilldown with its history charts, and a comparison, which is a table whose
// width is chosen by the reader — one column per run.
//
// `drilled` is the machine table as a group on the Groups view leaves it:
// filtered to one exact CPU, which puts that whole CPU string in the filter
// row — the longest thing that row ever has to hold.
//
// `overview-quarter` is the overview comparing with thirteen weeks back, which
// for the fixtures is before anything was measured — the "no baseline" state
// of "What changed", where the plain overview has its lists.
//
// `lifecycle` is the sixth tab, the one that made the strip scroll on a phone,
// and `lifecycle-keep` the same view with a tile chosen.
//
// `components` is the seventh: a histogram, a measure picker holding whole
// subtest names, and the machine-by-component grid, which is a table as wide
// as the fleet has components. `components-split` is the same view with a
// subtest chosen, split by site, a bin listed and the grid in grades — every
// card it can show at once.
const VIEWS = ['overview', 'overview-quarter', 'cohorts', 'groups', 'components', 'components-split',
  'lifecycle', 'lifecycle-keep', 'machines', 'drilled', 'machine', 'compare'];

/** The tab each view should mark as current. */
const TAB_OF = {
  'overview-quarter': 'overview', 'lifecycle-keep': 'lifecycle', 'components-split': 'components', drilled: 'machines', machine: 'machines',
};

// A control smaller than this is a control you miss. 40px is the floor here
// rather than Apple's 44 because the theme toggle is square and 44 looks
// oversized next to 13px text; both are far above the 21px this used to ship.
const MIN_TAP = 40;

/* ------------------------------------------------------------- the browser */

function findChrome() {
  if (process.env.CHROME && existsSync(process.env.CHROME)) return process.env.CHROME;
  const candidates = [
    'C:/Program Files/Google/Chrome/Application/chrome.exe',
    'C:/Program Files (x86)/Google/Chrome/Application/chrome.exe',
    'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe',
    '/usr/bin/google-chrome',
    '/usr/bin/google-chrome-stable',
    '/usr/bin/chromium',
    '/usr/bin/chromium-browser',
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  ];
  return candidates.find((p) => existsSync(p)) ?? null;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Minimal CDP client. Node's global WebSocket means this needs no dependency. */
async function attach(port) {
  let url = null;
  for (let i = 0; i < 80; i++) {
    try {
      const res = await fetch(`http://127.0.0.1:${port}/json/list`);
      const page = (await res.json()).find((t) => t.type === 'page');
      if (page?.webSocketDebuggerUrl) { url = page.webSocketDebuggerUrl; break; }
    } catch { /* the browser is still starting */ }
    await sleep(250);
  }
  if (!url) throw new Error('the browser never exposed a debugger');

  const ws = new WebSocket(url);
  await new Promise((resolve, reject) => {
    ws.addEventListener('open', resolve, { once: true });
    ws.addEventListener('error', () => reject(new Error('could not attach to the browser')), { once: true });
  });

  let id = 0;
  const pending = new Map();
  ws.addEventListener('message', (e) => {
    const msg = JSON.parse(e.data);
    const waiting = msg.id && pending.get(msg.id);
    if (!waiting) return;
    pending.delete(msg.id);
    msg.error ? waiting.reject(new Error(JSON.stringify(msg.error))) : waiting.resolve(msg.result);
  });

  const send = (method, params = {}) => new Promise((resolve, reject) => {
    const n = ++id;
    pending.set(n, { resolve, reject });
    ws.send(JSON.stringify({ id: n, method, params }));
  });

  return { send, close: () => ws.close() };
}

/* ---------------------------------------------------- the page measurement */

/*
 * Runs in the browser. Returns plain data rather than verdicts, so a failure
 * reads as a measurement and not just a boolean.
 */
const MEASURE = `(() => {
  const de = document.documentElement;
  const describe = (el) => el.tagName.toLowerCase() + (el.id ? '#' + el.id : '') +
    (typeof el.className === 'string' && el.className.trim()
      ? '.' + el.className.trim().split(/\\s+/).join('.') : '');

  // Something inside a horizontal scroller is *meant* to extend past the
  // window — that is what the scroller is for, and the table of machines is
  // wider than a phone whatever anyone does. The scroller itself still has to
  // fit, and whether it opted in is checked separately.
  const scrolled = (el) => {
    for (let p = el.parentElement; p && p !== document.body; p = p.parentElement) {
      if (['auto', 'scroll', 'hidden'].includes(getComputedStyle(p).overflowX)) return true;
    }
    return false;
  };

  const past = [];
  const selfScrolls = [];
  for (const el of document.querySelectorAll('body *')) {
    const r = el.getBoundingClientRect();
    if (r.width > 0 || r.height > 0) {
      const over = Math.round(r.right - de.clientWidth);
      // Only the outermost offender: a clipped card reports its every child.
      if (over > 1 && !scrolled(el) && !past.some((p) => p.node.contains(el))) {
        past.push({ node: el, sel: describe(el), over, width: Math.round(r.width) });
      }
    }
    if (el.clientWidth > 0 && el.scrollWidth - el.clientWidth > 1) {
      selfScrolls.push({
        sel: describe(el), clientWidth: el.clientWidth, scrollWidth: el.scrollWidth,
        overflowX: getComputedStyle(el).overflowX,
      });
    }
  }

  const tiny = [];
  for (const el of document.querySelectorAll('.btn, .tab, .field select, .field input')) {
    const r = el.getBoundingClientRect();
    if (r.height === 0 && r.width === 0) continue;
    tiny.push({ sel: describe(el), height: Math.round(r.height), width: Math.round(r.width) });
  }

  const bar = document.querySelector('.topbar');
  return JSON.stringify({
    viewport: de.clientWidth,
    documentWidth: Math.max(de.scrollWidth, document.body.scrollWidth),
    paintedPast: past.map(({ sel, over, width }) => ({ sel, over, width })),
    selfScrolls,
    controls: tiny,
    topbarHeight: bar ? Math.round(bar.getBoundingClientRect().height) : 0,
    bodyText: document.body.innerText.length,
  });
})()`;

/*
 * The tab strip, which is allowed to scroll — six tabs do not fit a phone —
 * on three conditions, measured here rather than trusted: the current tab is
 * wholly inside the strip's visible box, every tab can be scrolled to, and the
 * fade that says "more this way" is on exactly the edges that hide something.
 */
const TABS = `(() => {
  const strip = document.querySelector('#tabs');
  const s = strip.getBoundingClientRect();
  const active = strip.querySelector('[aria-current="page"]');
  const a = active ? active.getBoundingClientRect() : null;
  const tabs = [...strip.querySelectorAll('.tab')].map((t) => {
    const r = t.getBoundingClientRect();
    // Where the tab sits in the strip's scrollable content, not on screen.
    return { view: t.dataset.view, width: r.width, from: r.left - s.left + strip.scrollLeft, to: r.right - s.left + strip.scrollLeft,
      shown: getComputedStyle(t).display !== 'none' && getComputedStyle(t).visibility !== 'hidden' };
  });
  return JSON.stringify({
    overflowX: getComputedStyle(strip).overflowX,
    hidden: strip.scrollWidth - strip.clientWidth,
    scrollWidth: strip.scrollWidth,
    scrollLeft: strip.scrollLeft,
    before: strip.classList.contains('more-before'),
    after: strip.classList.contains('more-after'),
    active: active ? active.dataset.view : null,
    activeLeft: a ? a.left - s.left : null,
    activeRight: a ? s.right - a.right : null,
    tabs,
  });
})()`;

function checkTabs(where, t, want) {
  check(where, t.active === want, `the current tab is ${t.active}, not ${want}`);
  check(where, t.activeLeft !== null && t.activeLeft >= -1 && t.activeRight >= -1,
    `the current tab is not wholly in view (${t.activeLeft?.toFixed(0)}px from the left edge, `
    + `${t.activeRight?.toFixed(0)}px from the right)`);
  for (const tab of t.tabs) {
    check(where, tab.shown && tab.width > 0 && tab.from >= -1 && tab.to <= t.scrollWidth + 1,
      `the ${tab.view} tab cannot be scrolled to`);
  }
  if (t.hidden > 1) {
    check(where, ['auto', 'scroll'].includes(t.overflowX), `the strip hides ${t.hidden}px and cannot scroll`);
    check(where, t.after === t.scrollLeft < t.hidden - 1,
      `${t.after ? 'a' : 'no'} fade on the right with ${Math.round(t.hidden - t.scrollLeft)}px of tabs past it`);
    check(where, t.before === t.scrollLeft > 1,
      `${t.before ? 'a' : 'no'} fade on the left with ${Math.round(t.scrollLeft)}px of tabs past it`);
  } else {
    check(where, !t.before && !t.after, 'a fade says there are more tabs when every one is in view');
  }
}

/* ------------------------------------------------------------- the fixtures */

const root = join(import.meta.dirname, '..');
const binary = ['target/debug/loadbearer-fleet', 'target/debug/loadbearer-fleet.exe',
  'target/release/loadbearer-fleet', 'target/release/loadbearer-fleet.exe']
  .map((p) => join(root, p)).find((p) => existsSync(p));

const failures = [];
const notes = [];
const check = (where, condition, detail) => {
  if (!condition) failures.push(`${where}: ${detail}`);
};

function bail(message) {
  console.log(message);
  process.exit(0);
}

if (!binary) bail('no built binary — run `cargo build` first. Skipping.');
const chrome = findChrome();
if (!chrome) bail('no Chrome or Edge found (set CHROME=/path/to/chrome). Skipping.');

/* --------------------------------------------------------------- the server */

const work = mkdtempSync(join(tmpdir(), 'lbf-mobile-'));
const port = 8790 + (process.pid % 40);

// The collection folder is the fixtures, copied, plus two results dated the
// last couple of days: a later run of one fixture machine, now throttling, and
// a machine new to the fleet with a hostname long enough to test a phone. The
// fixtures alone were measured within a day of each other, so "What changed"
// would only ever say nothing did — and the lists are the layout to check.
const collection = join(work, 'collection');
mkdirSync(collection);
for (const f of readdirSync(join(root, 'tests/fixtures'))) {
  copyFileSync(join(root, 'tests/fixtures', f), join(collection, f));
}
{
  const recent = (days) => new Date(Date.now() - days * 86400000).toISOString();
  const doc = JSON.parse(readFileSync(join(root, 'tests/fixtures/win-modern.json'), 'utf8'));
  doc.timestamp = recent(2);
  doc.telemetry = { source: 'check', sample_count: 9, thermal_limited: true };
  writeFileSync(join(collection, 'later-run.json'), JSON.stringify(doc));
  const arrived = JSON.parse(readFileSync(join(root, 'tests/fixtures/low-end.json'), 'utf8'));
  arrived.timestamp = recent(1);
  arrived.machine.hostname = 'WORKSTATION-WITH-A-VERY-LONG-HOSTNAME-07';
  arrived.machine.identity = { serial: 'SN-CHECK-ARRIVED', smbios_uuid: 'u-check-arrived', machine_id: 'm-check-arrived' };
  writeFileSync(join(collection, 'arrived.json'), JSON.stringify(arrived));
}

// Ingest only ever reads the collection folder, and the index it derives goes
// to a temporary file.
const server = spawn(binary, [
  'serve', collection,
  '--index', join(work, 'index.db'),
  '--bind', `127.0.0.1:${port}`,
], { stdio: ['ignore', 'ignore', 'pipe'] });

let serverLog = '';
server.stderr.on('data', (d) => { serverLog += d; });

const origin = `http://127.0.0.1:${port}`;
let up = false;
for (let i = 0; i < 60 && !up; i++) {
  try {
    up = (await fetch(`${origin}/api/health`)).ok;
  } catch { await sleep(250); }
}
if (!up) {
  console.error(`the dashboard did not start on ${port}:\n${serverLog}`);
  process.exit(1);
}

// A real machine key, so the drilldown is rendered against something.
const snapshot = await (await fetch(`${origin}/api/snapshot`)).json();
const machineKey = snapshot.machines[0].key;
// Three runs: the widest table a reader can ask for on a phone, short of four.
const compareRuns = snapshot.machines.slice(0, 3).map((m) => m.run_id);
const longestCpu = snapshot.machines.map((m) => m.cpu_model)
  .reduce((a, b) => (b.length > a.length ? b : a), '');
check('fixtures', snapshot.machines.length > 0, 'the fixtures produced no machines');

// Two findings quieted through the API, as a person would quiet them, so the
// quieted line — who, when, a long note, Undo — is laid out at every size as
// well as the live controls. The drilldown machine's own findings first, so
// its page carries one.
{
  const warnings = snapshot.flags.filter((f) => f.severity !== 'info')
    .sort((a, b) => Number(b.machine_key === machineKey) - Number(a.machine_key === machineKey));
  const until = new Date(Date.now() + 7 * 86400000).toISOString().slice(0, 10);
  const said = [
    { action: 'acknowledge', note: 'Fan replacement ordered under the warranty; re-measure once it is fitted and the heatsink cleaned.' },
    { action: 'snooze', until },
  ];
  for (const [i, extra] of said.entries()) {
    const f = warnings[i];
    if (!f) continue;
    const r = await fetch(`${origin}/api/triage`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ machine_key: f.machine_key, code: f.code, ...extra }),
    });
    check('triage', r.ok, `could not ${extra.action} ${f.code}: ${await r.text()}`);
  }
}

/* ----------------------------------------------------------------- the runs */

const profile = join(work, 'chrome');
const debugPort = port + 100;
const browser = spawn(chrome, [
  '--headless=new',
  `--remote-debugging-port=${debugPort}`,
  `--user-data-dir=${profile}`,
  '--no-first-run', '--no-default-browser-check', '--disable-gpu',
  '--window-size=1440,900',
  // A CI container is often too locked down for Chrome's own sandbox, and
  // there is nothing untrusted here to sandbox from — the page is this repo.
  ...(process.env.CI ? ['--no-sandbox'] : []),
  'about:blank',
], { stdio: 'ignore' });

let cdp;
try {
  cdp = await attach(debugPort);
  await cdp.send('Page.enable');
  await cdp.send('Runtime.enable');

  for (const size of SIZES) {
    await cdp.send('Emulation.setDeviceMetricsOverride', {
      width: size.width, height: size.height, deviceScaleFactor: 1, mobile: size.touch,
    });
    // `maxTouchPoints` is rejected outside 1..16 even when disabling, so it is
    // always sent as 1 and `enabled` does the work.
    await cdp.send('Emulation.setTouchEmulationEnabled', {
      enabled: size.touch, maxTouchPoints: 1,
    });

    for (const view of VIEWS) {
      let hash = view;
      if (view === 'machine') hash = `machine/${encodeURIComponent(machineKey)}`;
      // A comparison with nothing chosen is only the picker, which is not the
      // layout worth checking — the table is.
      if (view === 'compare') hash = `compare/${compareRuns.join(',')}`;
      if (view === 'drilled') hash = `machines?cpu=${encodeURIComponent(longestCpu)}`;
      if (view === 'overview-quarter') hash = 'overview?changes=13';
      if (view === 'lifecycle-keep') hash = 'lifecycle?decision=keep';
      if (view === 'components-split') {
        hash = 'components?measure=disk%2Fseq_read&split=tag%3Asite&bin=0&encoding=grade&sort=disk&dir=desc';
      }
      const where = `${size.name}/${view}`;
      // A fresh navigation rather than a hash change, so each measurement is of
      // a page that laid itself out at this size from the start.
      await cdp.send('Page.navigate', { url: `${origin}/#/${hash}` });
      // Waited for rather than slept through: a fixed delay is a flaky check
      // on a loaded CI runner, and the page says when it is done — `main` is
      // filled by the render and `loading` comes off at the end of it.
      const ready = `(() => {
        const m = document.querySelector('main');
        return !!m && !m.classList.contains('loading') && m.innerText.length > 200;
      })()`;
      let painted = false;
      for (let i = 0; i < 60 && !painted; i++) {
        const probe = await cdp.send('Runtime.evaluate', { expression: ready, returnByValue: true });
        painted = probe.result.value === true;
        if (!painted) await sleep(250);
      }
      check(where, painted, 'the view never finished rendering');
      if (!painted) continue;
      // One frame for the layout to settle after the last insertion.
      await sleep(100);

      const result = await cdp.send('Runtime.evaluate', {
        expression: MEASURE, returnByValue: true,
      });
      if (result.exceptionDetails) {
        check(where, false, `the page threw: ${result.exceptionDetails.text}`);
        continue;
      }
      const m = JSON.parse(result.result.value);

      check(where, m.bodyText > 200, `rendered almost nothing (${m.bodyText} chars)`);
      // Not exact: a desktop Chrome spends ~15px of it on a classic scrollbar.
      check(where, m.viewport >= size.width - 20,
        `laid out at ${m.viewport}px in a ${size.width}px window`);
      check(where, m.documentWidth <= m.viewport + 1,
        `the page scrolls sideways: ${m.documentWidth}px of content in ${m.viewport}px`);

      for (const el of m.paintedPast) {
        check(where, false, `${el.sel} is painted ${el.over}px past the right edge`);
      }

      for (const el of m.selfScrolls) {
        check(where, ['auto', 'scroll', 'hidden'].includes(el.overflowX),
          `${el.sel} holds ${el.scrollWidth}px in ${el.clientWidth}px with `
          + `overflow-x: ${el.overflowX} — it will clip rather than scroll`);
      }

      if (size.touch) {
        for (const c of m.controls) {
          check(where, c.height >= MIN_TAP,
            `${c.sel} is ${c.height}px tall; ${MIN_TAP}px is the floor for touch`);
        }
      }

      // An absolute ceiling, not a fraction of the screen: what makes the
      // header tall is how much has to wrap at that *width*, and a short
      // screen does not make a two-row header any worse. 120px is two 40px
      // rows plus its padding and gap, with a little room for the font metrics
      // to differ between a developer's machine and a CI runner — which is
      // exactly how the 320px case got through locally and failed here.
      //
      // The fixtures sign nobody in, so the Sign out button is not in this
      // measurement; a signed-in session on a 320px screen has one more thing
      // in that row and may take three.
      // Matches the CSS breakpoint: below it the header is two deliberate
      // rows, above it one. The numbers are what those cost — two 40px touch
      // rows plus padding and a gap is about 110, and the thing being guarded
      // against is a *third* row at roughly 160. Tight enough to catch that,
      // loose enough that one machine's fonts being wider than another's is
      // not a failure.
      const ceiling = size.width <= 820 ? 130 : 90;
      check(where, m.topbarHeight <= ceiling,
        `the topbar is ${m.topbarHeight}px at ${size.width}px wide (ceiling ${ceiling}px)`);

      // Read on load, which is a fresh navigation straight to this view.
      const tabs = JSON.parse((await cdp.send('Runtime.evaluate', { expression: TABS, returnByValue: true })).result.value);
      checkTabs(where, tabs, TAB_OF[view] || view);

      notes.push(`${where}: ${m.documentWidth}px wide, topbar ${m.topbarHeight}px, `
        + `${m.controls.length} control(s), ${m.selfScrolls.length} scroller(s), `
        + `tab strip hides ${Math.max(0, tabs.hidden)}px`);

      // And on change: from the Lifecycle view, which on a phone leaves the
      // strip scrolled part-way, to the last tab and then the first — each the
      // far end of the strip from the one before.
      if (view === 'lifecycle') {
        for (const next of ['compare', 'overview', 'lifecycle']) {
          await cdp.send('Runtime.evaluate', {
            expression: `document.querySelector('.tab[data-view="${next}"]').click()`,
          });
          const switched = `(() => {
            const m = document.querySelector('main');
            return document.querySelector('.tab[aria-current="page"]')?.dataset.view === '${next}'
              && !!m && !m.classList.contains('loading') && m.innerText.length > 100;
          })()`;
          let done = false;
          for (let i = 0; i < 60 && !done; i++) {
            done = (await cdp.send('Runtime.evaluate', { expression: switched, returnByValue: true })).result.value === true;
            if (!done) await sleep(250);
          }
          check(`${where}→${next}`, done, 'the tab did not switch the view');
          await sleep(100);
          const after = JSON.parse((await cdp.send('Runtime.evaluate', { expression: TABS, returnByValue: true })).result.value);
          checkTabs(`${where}→${next}`, after, next);
        }
      }

      // The overview's history cards fill in after the rest, from their own
      // requests: that they arrived, and arrived as content rather than as a
      // placeholder or an error, is part of the view rendering.
      if (view.startsWith('overview')) {
        const probe = await cdp.send('Runtime.evaluate', {
          expression: `(() => {
            const text = (sel) => document.querySelector(sel)?.innerText || '';
            const t = document.querySelector('[data-card="over-time"]');
            return JSON.stringify({
              changes: text('[data-card="changes"]'), overTime: text('[data-card="over-time"]'),
              charts: t ? t.querySelectorAll('svg').length : 0,
            });
          })()`,
          returnByValue: true,
        });
        const cards = JSON.parse(probe.result.value);
        check(where, cards.changes.includes('What changed') && !cards.changes.includes('Loading'),
          `"What changed" did not render: ${JSON.stringify(cards.changes.slice(0, 120))}`);
        if (view === 'overview') {
          check(where, cards.changes.includes('Machines added') && cards.changes.includes('New findings'),
            'the week\'s changes — a machine added, a new finding — are not listed');
        }
        check(where, cards.overTime.includes('Over time') && cards.charts === 3,
          `the over-time section has ${cards.charts} chart(s), not 3`);
      }
    }
  }
} finally {
  cdp?.close();
  browser.kill();
  server.kill();
  try { rmSync(work, { recursive: true, force: true }); } catch { /* Windows holds the db briefly */ }
}

/* --------------------------------------------------------------- the report */

for (const note of notes) console.log(`  note  ${note}`);

if (failures.length) {
  console.error(`\n${failures.length} layout problem(s):`);
  for (const f of failures) console.error(`  FAIL  ${f}`);
  process.exit(1);
}

console.log('\nEvery view fits every screen, and every control is big enough to hit.');
