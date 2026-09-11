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
import { existsSync, mkdtempSync, rmSync } from 'node:fs';
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
  { name: 'desktop', width: 1440, height: 900, touch: false },
];

// Three of these hold the widest thing on the site: the machines table, a
// drilldown with its history charts, and a comparison, which is a table whose
// width is chosen by the reader — one column per run.
const VIEWS = ['overview', 'cohorts', 'machines', 'machine', 'compare'];

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
// The fixtures directory is the collection folder: ingest only ever reads it,
// and the index it derives goes to a temporary file.
const server = spawn(binary, [
  'serve', join(root, 'tests/fixtures'),
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
check('fixtures', snapshot.machines.length > 0, 'the fixtures produced no machines');

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
      const ceiling = size.width <= 720 ? 120 : 90;
      check(where, m.topbarHeight <= ceiling,
        `the topbar is ${m.topbarHeight}px at ${size.width}px wide (ceiling ${ceiling}px)`);

      notes.push(`${where}: ${m.documentWidth}px wide, topbar ${m.topbarHeight}px, `
        + `${m.controls.length} control(s), ${m.selfScrolls.length} scroller(s)`);
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
