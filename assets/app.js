/*
 * The dashboard.
 *
 * Plain modules, plain SVG, no framework and no build step - the server ships
 * this file out of its own binary, so there is nothing to install and nothing
 * to fetch from the internet.
 *
 * Two rules run through all of it.
 *
 * Charts are drawn to the dataviz specs: thin marks, a 4px rounded data-end
 * square at the baseline, hairline solid gridlines, selective direct labels,
 * a legend whenever two things share a plot, hover and keyboard-focus parity,
 * and a table view for every single chart - so no value is ever reachable
 * only by hovering.
 *
 * Labels are untrusted. Hostnames, CPU model strings and tag values come out
 * of result files written by machines in the field, so every one of them goes
 * into the DOM as text, never as markup.
 */

const SVG = 'http://www.w3.org/2000/svg';

/* ---------------------------------------------------------------- helpers */

const $ = (sel, root = document) => root.querySelector(sel);

function el(tag, props = {}, kids = []) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (v === null || v === undefined) continue;
    if (k === 'text') node.textContent = v;
    else if (k === 'class') node.className = v;
    else if (k === 'dataset') Object.assign(node.dataset, v);
    else if (k.startsWith('on')) node.addEventListener(k.slice(2), v);
    // A boolean attribute is true by its *presence*, so `selected="false"`
    // selects the option. `{ selected: false }` therefore has to mean no
    // attribute at all — otherwise every option in a run picker is marked
    // selected, the browser shows the last of them, and the control disagrees
    // with what is actually being compared.
    else if (v === false) continue;
    else if (v === true) node.setAttribute(k, '');
    else node.setAttribute(k, v);
  }
  for (const kid of [].concat(kids)) {
    if (kid === null || kid === undefined || kid === false) continue;
    node.append(kid);
  }
  return node;
}

function svg(tag, attrs = {}, kids = []) {
  const node = document.createElementNS(SVG, tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === null || v === undefined) continue;
    node.setAttribute(k, v);
  }
  for (const kid of [].concat(kids)) if (kid) node.append(kid);
  return node;
}

function ink(role) {
  return getComputedStyle(document.documentElement).getPropertyValue(role).trim();
}

const nf0 = new Intl.NumberFormat(undefined, { maximumFractionDigits: 0 });

function num(v, dp) {
  if (v === null || v === undefined || Number.isNaN(v)) return '—';
  if (dp !== undefined) return v.toFixed(dp);
  const a = Math.abs(v);
  if (a >= 1000) return nf0.format(v);
  if (a >= 10) return v.toFixed(1);
  if (a >= 1) return v.toFixed(2);
  return v.toFixed(3);
}

function signed(v, dp = 0) {
  if (v === null || v === undefined) return '—';
  return (v > 0 ? '+' : '') + v.toFixed(dp) + '%';
}

function gib(bytes) {
  return nf0.format(bytes / 1024 ** 3) + ' GiB';
}

function when(iso) {
  const d = new Date(iso);
  if (Number.isNaN(+d)) return iso;
  return d.toLocaleString(undefined, {
    year: 'numeric', month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit',
  });
}

function day(iso) {
  const d = new Date(iso);
  if (Number.isNaN(+d)) return iso;
  return d.toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
}

function ago(days) {
  if (days === null || days === undefined) return 'date unreadable';
  if (days < 1) return 'today';
  if (days < 2) return 'yesterday';
  if (days < 60) return `${Math.round(days)} days ago`;
  if (days < 730) return `${Math.round(days / 30)} months ago`;
  return `${(days / 365).toFixed(1)} years ago`;
}

/** "Jan 2023" from a `YYYY-MM`. */
function monthYear(ym) {
  const [y, mo] = (ym || '').split('-').map(Number);
  if (!y || !mo) return ym || '—';
  return new Date(Date.UTC(y, mo - 1, 1)).toLocaleDateString(undefined, {
    month: 'short', year: 'numeric', timeZone: 'UTC',
  });
}

/** "≤ 3.8 yr" — an upper bound, because the CPU's launch only says the machine
 *  can't be older. */
function maxAge(months) {
  if (months === null || months === undefined) return '—';
  return months < 12 ? `≤ ${months} mo` : `≤ ${(months / 12).toFixed(1)} yr`;
}

/** The age facts for a machine or a run, from loadbearer 1.6.0's age
 *  evidence. Each line is null when its source wasn't recorded. */
function ageFacts(m) {
  const built = m.cpu_launched
    ? `${maxAge(m.max_age_months_now)} — built ${monthYear(m.cpu_launched)} or later`
      + (m.cpu_family ? ` · ${m.cpu_family}` : '')
      + (m.age_source === 'derived' ? ' · from the CPU model' : '')
    : null;
  let firmware = null;
  // The firmware's own version and day where the run recorded them (and the
  // index has them — a run indexed before the index kept them needs
  // `scan --reindex`), the age estimate's month otherwise.
  if (m.firmware_dated || m.bios_date || m.bios_version) {
    firmware = [
      m.bios_version ? `BIOS ${m.bios_version}` : null,
      m.bios_date ? `dated ${m.bios_date}` : m.firmware_dated ? `dated ${monthYear(m.firmware_dated)}` : null,
    ].filter(Boolean).join(', ');
    if (m.firmware_likely_original === true) firmware += ' — probably as shipped, so the machine likely dates from about then';
    if (m.firmware_likely_original === false) firmware += ' — updated since it shipped';
  }
  const drive = m.drive_power_on_hours === null || m.drive_power_on_hours === undefined
    ? null
    : `${nf0.format(m.drive_power_on_hours)} h powered`
      + (m.drive_hours_undercounted
        ? ' (likely undercounted: the drive seems to stop counting when idle)'
        : '')
      + (m.drive_wear_pct === null || m.drive_wear_pct === undefined ? '' : ` · ${m.drive_wear_pct}% of rated endurance used`);
  return { built, firmware, drive };
}

/** The power settings a run had, from loadbearer 1.7.0's `machine.power`:
 *  "on mains · Balanced plan · Best performance mode", and what held the run
 *  back. Null when nothing was recorded. A mode with no plan beside it is a
 *  Linux platform profile. */
function powerFact(m) {
  const parts = [];
  if (m.on_ac === true) parts.push('on mains');
  if (m.on_ac === false) parts.push('on battery');
  if (m.power_plan) parts.push(`${m.power_plan} plan`);
  if (m.power_mode) {
    parts.push(m.power_plan
      ? `${m.power_mode[0].toUpperCase()}${m.power_mode.slice(1)} mode`
      : `${m.power_mode} profile`);
  }
  if (!parts.length) return null;
  const held = Array.isArray(m.power_held_back) ? m.power_held_back.join(', ') : m.power_held_back;
  return parts.join(' · ') + (held ? ` — held back by ${held}` : '');
}

/** Each threshold an override can set, as the line it draws. Plain numbers:
 *  these are the values in the config file, and `regression_frac` the one
 *  fraction among them. */
const LINE_WORDS = {
  regression_frac: (v) => `regressed from ${+(v * 100).toFixed(2)}% down`,
  stale_days: (v) => `stale after ${v} days`,
  battery_health_pct: (v) => `battery worn under ${v}%`,
  unstable_cpu_subtests: (v) => `run unstable from ${v} shaky CPU subtests`,
  drive_wear_pct: (v) => `drive worn from ${v}% used`,
  drive_spare_pct: (v) => `drive worn under ${v}% spare`,
  disk_free_pct: (v) => `disk nearly full under ${v}% free`,
  disk_free_critical_pct: (v) => `critical under ${v}% free`,
  firmware_behind_days: (v) => `firmware behind from ${v} days`,
  firmware_old_years: (v) => `firmware old after ${v} years`,
  replace_age_years: (v) => `old from ${v} years`,
};

/** "lab machines: stale after 365 days" per override that changed how a
 *  machine was judged, or null when none did. A machine with 8% free and no
 *  finding has to be able to say it is judged against another line. */
function thresholdsFact(m) {
  const by = m.overridden_by || [];
  if (!by.length) return null;
  return by.map((o) => `${o.name}: ${Object.entries(o.set)
    .map(([k, v]) => (LINE_WORDS[k] ? LINE_WORDS[k](v) : `${k} ${v}`)).join(', ')}`).join(' · ')
    + ' — set in the configuration, in place of the fleet-wide lines';
}

/** How many machines in view an override gives their own value of one
 *  threshold — what a sentence quoting the fleet-wide value has to admit. */
function overriddenCount(snap, field) {
  return (snap.machines || []).filter((m) => (m.overridden_by || []).some((o) => field in o.set)).length;
}

function debounce(fn, ms) {
  let t;
  return (...a) => { clearTimeout(t); t = setTimeout(() => fn(...a), ms); };
}

/**
 * Y-axis ticks on clean round numbers, per the marks spec.
 *
 * `minStep` exists because these axes count whole things. Without it, a chart
 * whose tallest bar is 2 machines gets a step of 0.5 and an axis reading
 * 0, 1, 1, 2, 2 — the ticks are right, but the integer formatter prints each
 * one twice. One or two machines in a grade bucket is the *normal* case on a
 * small estate, so it showed up immediately once the page was looked at rather
 * than reasoned about.
 */
export function niceTicks(max, count = 4, minStep = 0) {
  if (!(max > 0)) return { max: 1, ticks: [0, 1] };
  const raw = max / count;
  const mag = 10 ** Math.floor(Math.log10(raw));
  const step = Math.max(
    minStep,
    [1, 2, 2.5, 5, 10].map((m) => m * mag).find((s) => s >= raw) || 10 * mag,
  );
  const top = Math.ceil(max / step) * step;
  const ticks = [];
  for (let v = 0; v <= top + 1e-9; v += step) ticks.push(v);
  return { max: top, ticks };
}

/**
 * A bar with its data-end rounded and its baseline end square. The corners are
 * the only place a mark is allowed any decoration, and 4px is the spec.
 */
export function barPath(x, y, w, h, r, dir) {
  const rr = Math.max(0, Math.min(r, w / 2, h / 2));
  if (dir === 'up') {
    return `M${x},${y + h}L${x},${y + rr}Q${x},${y} ${x + rr},${y}` +
      `L${x + w - rr},${y}Q${x + w},${y} ${x + w},${y + rr}L${x + w},${y + h}Z`;
  }
  if (dir === 'right') {
    return `M${x},${y}L${x + w - rr},${y}Q${x + w},${y} ${x + w},${y + rr}` +
      `L${x + w},${y + h - rr}Q${x + w},${y + h} ${x + w - rr},${y + h}L${x},${y + h}Z`;
  }
  // left
  return `M${x + w},${y}L${x + rr},${y}Q${x},${y} ${x},${y + rr}` +
    `L${x},${y + h - rr}Q${x},${y + h} ${x + rr},${y + h}L${x + w},${y + h}Z`;
}

/** Rough text width at a given px size - enough to decide if a label fits. */
export function textWidth(str, px) {
  return String(str).length * px * 0.58;
}

/* ---------------------------------------------------------------- tooltip */

const tip = {
  node: null,
  show(ev, build) {
    if (!this.node) this.node = $('#tooltip');
    this.node.replaceChildren(build());
    this.node.hidden = false;
    this.move(ev);
  },
  move(ev) {
    if (!this.node || this.node.hidden) return;
    const pad = 12;
    const r = this.node.getBoundingClientRect();
    let x = (ev.clientX ?? 0) + pad;
    let y = (ev.clientY ?? 0) + pad;
    if (x + r.width > window.innerWidth - 8) x = ev.clientX - r.width - pad;
    if (y + r.height > window.innerHeight - 8) y = ev.clientY - r.height - pad;
    this.node.style.left = `${Math.max(8, x)}px`;
    this.node.style.top = `${Math.max(8, y)}px`;
  },
  hide() {
    if (this.node) this.node.hidden = true;
  },
};

function tipBlock(title, rows) {
  return el('div', {}, [
    title ? el('div', { class: 'tt-title', text: title }) : null,
    ...rows.map(([label, value, colour]) => el('div', { class: 'tt-row' }, [
      colour ? Object.assign(el('span', { class: 'tt-key' }), { style: `background:${colour}` }) : null,
      el('span', { class: 'tt-value', text: value }),
      el('span', { class: 'tt-name', text: label }),
    ])),
  ]);
}

/**
 * Attach hover and focus to a mark. Focus gets exactly what hover gets, and
 * the caller is expected to hand in a hit target larger than the paint.
 */
function hoverable(node, build) {
  node.addEventListener('pointerenter', (e) => tip.show(e, build));
  node.addEventListener('pointermove', (e) => tip.move(e));
  node.addEventListener('pointerleave', () => tip.hide());
  node.setAttribute('tabindex', '0');
  node.addEventListener('focus', () => {
    const r = node.getBoundingClientRect();
    tip.show({ clientX: r.left + r.width / 2, clientY: r.top }, build);
  });
  node.addEventListener('blur', () => tip.hide());
  return node;
}

/* ------------------------------------------------------------------ charts */

/** Vertical bars. One hue for one series - the axis carries the categories. */
export function columnChart(host, { rows, fill, unit = '', tipTitle }) {
  const width = Math.max(320, host.clientWidth || 520);
  const m = { top: 22, right: 8, bottom: 40, left: 46 };
  const plotH = 170;
  const height = plotH + m.top + m.bottom; // the fixed height includes the axis band
  const plotW = width - m.left - m.right;
  // Whole machines and whole scores, so never a fractional tick.
  const { max, ticks } = niceTicks(Math.max(...rows.map((r) => r.value), 0), 4, 1);
  const y = (v) => m.top + plotH - (v / max) * plotH;
  const band = plotW / Math.max(rows.length, 1);
  const barW = Math.min(24, Math.max(6, band - 14));
  const root = svg('svg', { class: 'chart', viewBox: `0 0 ${width} ${height}`, width, height, role: 'img' });

  for (const t of ticks) {
    root.append(svg('line', {
      x1: m.left, x2: m.left + plotW, y1: y(t), y2: y(t),
      stroke: t === 0 ? ink('--axis') : ink('--grid'), 'stroke-width': 1,
    }));
    root.append(svg('text', {
      x: m.left - 8, y: y(t) + 4, 'text-anchor': 'end',
      fill: ink('--text-muted'), 'font-size': 11, 'font-variant-numeric': 'tabular-nums',
    }, [document.createTextNode(nf0.format(t))]));
  }

  rows.forEach((r, i) => {
    const cx = m.left + band * i + band / 2;
    const h = max > 0 ? (r.value / max) * plotH : 0;
    const colour = r.fill || fill || ink('--series-1');
    if (h > 0) {
      root.append(svg('path', {
        d: barPath(cx - barW / 2, y(r.value), barW, h, 4, 'up'), fill: colour,
      }));
    }
    // Value on the cap - selective by construction: one number per category.
    root.append(svg('text', {
      x: cx, y: y(r.value) - 7, 'text-anchor': 'middle',
      fill: ink('--text-secondary'), 'font-size': 11, 'font-variant-numeric': 'tabular-nums',
    }, [document.createTextNode(nf0.format(r.value))]));
    root.append(svg('text', {
      x: cx, y: m.top + plotH + 16, 'text-anchor': 'middle',
      fill: ink('--text-secondary'), 'font-size': 11,
    }, [document.createTextNode(r.label)]));
    if (r.sublabel) {
      root.append(svg('text', {
        x: cx, y: m.top + plotH + 30, 'text-anchor': 'middle',
        fill: ink('--text-muted'), 'font-size': 10,
      }, [document.createTextNode(r.sublabel)]));
    }
    // The hit target is the whole band, not the painted bar.
    root.append(hoverable(svg('rect', {
      x: m.left + band * i, y: m.top, width: band, height: plotH,
      fill: 'transparent', role: 'img',
      'aria-label': `${r.label}: ${r.value} ${unit}`.trim(),
    }), () => tipBlock(tipTitle ? `${tipTitle} · ${r.label}` : r.label,
      [[unit || 'machines', nf0.format(r.value), r.fill || fill || ink('--series-1')]])));
  });

  root.append(svg('line', {
    x1: m.left, x2: m.left + plotW, y1: m.top + plotH, y2: m.top + plotH,
    stroke: ink('--axis'), 'stroke-width': 1,
  }));
  host.append(root);
}

/**
 * Horizontal bars either side of a neutral zero line: hue carries the sign,
 * length carries the size. Blue/red validated all-pairs in both modes.
 */
export function divergingChart(host, { rows, onSelect }) {
  const width = Math.max(320, host.clientWidth || 520);
  const gutter = Math.min(190, Math.max(90, ...rows.map((r) => textWidth(r.label, 12) + 12)));
  const m = { top: 8, right: 54, bottom: 26, left: gutter };
  const rowH = 26;
  const plotH = rowH * rows.length;
  const height = plotH + m.top + m.bottom;
  const plotW = width - m.left - m.right;
  const span = Math.max(5, ...rows.map((r) => Math.abs(r.value))) * 1.12;
  const zero = m.left + plotW / 2;
  const x = (v) => zero + (v / span) * (plotW / 2);
  const barH = Math.min(24, rowH - 8);
  const root = svg('svg', { class: 'chart', viewBox: `0 0 ${width} ${height}`, width, height });

  for (const g of [-span / 2, span / 2]) {
    root.append(svg('line', {
      x1: x(g), x2: x(g), y1: m.top, y2: m.top + plotH, stroke: ink('--grid'), 'stroke-width': 1,
    }));
    root.append(svg('text', {
      x: x(g), y: height - 8, 'text-anchor': 'middle', fill: ink('--text-muted'), 'font-size': 10,
    }, [document.createTextNode(signed(g, 0))]));
  }

  rows.forEach((r, i) => {
    const cy = m.top + rowH * i + rowH / 2;
    const up = r.value >= 0;
    const colour = up ? ink('--diverge-up') : ink('--diverge-down');
    const w = Math.abs(x(r.value) - zero);
    if (w > 0.6) {
      root.append(svg('path', {
        d: barPath(up ? zero : zero - w, cy - barH / 2, w, barH, 4, up ? 'right' : 'left'),
        fill: colour,
      }));
    }
    root.append(svg('text', {
      x: m.left - 10, y: cy + 4, 'text-anchor': 'end',
      fill: ink('--text-primary'), 'font-size': 12,
    }, [document.createTextNode(r.label)]));
    root.append(svg('text', {
      x: up ? zero + w + 7 : zero - w - 7, y: cy + 4,
      'text-anchor': up ? 'start' : 'end',
      fill: ink('--text-secondary'), 'font-size': 11, 'font-variant-numeric': 'tabular-nums',
    }, [document.createTextNode(signed(r.value, 1))]));

    const hit = hoverable(svg('rect', {
      x: m.left - gutter, y: m.top + rowH * i, width: width - m.right + gutter, height: rowH,
      fill: 'transparent', role: 'img',
      'aria-label': `${r.label}: ${signed(r.value, 1)} versus the cohort median`,
    }), () => tipBlock(r.label, [
      ['vs cohort median', signed(r.value, 1), colour],
      ['score', num(r.score, 0)],
      ...(r.note ? [[r.note, '']] : []),
    ]));
    if (onSelect) {
      hit.addEventListener('click', () => onSelect(r));
      hit.addEventListener('keydown', (e) => { if (e.key === 'Enter') onSelect(r); });
      hit.setAttribute('cursor', 'pointer');
    }
    root.append(hit);
  });

  // The neutral midpoint - gray, per the diverging rule.
  root.append(svg('line', {
    x1: zero, x2: zero, y1: m.top, y2: m.top + plotH, stroke: ink('--axis'), 'stroke-width': 1,
  }));
  // The caption shares a line with the two tick labels, a quarter of the plot
  // either side of it. On a phone, with a long label gutter, that quarter is
  // narrower than the words and they ran into the ticks — so there the
  // midpoint is labelled as what it is on the scale, and the card's own text
  // is left to say what zero means.
  const caption = 'cohort median';
  const tick = Math.max(textWidth(signed(-span / 2, 0), 10), textWidth(signed(span / 2, 0), 10));
  const fits = plotW / 4 >= textWidth(caption, 10) / 2 + tick / 2 + 6;
  root.append(svg('text', {
    x: zero, y: height - 8, 'text-anchor': 'middle', fill: ink('--text-muted'), 'font-size': 10,
  }, [document.createTextNode(fits ? caption : '0')]));
  host.append(root);
}

/**
 * Horizontal stacked bars: a 2px surface gap does the separating, not a stroke.
 *
 * Counts by default. For shares, pass `max: 100` so every row is drawn against
 * the same whole rather than against the longest row, a `format` for the
 * figures, and a `tip` that puts the count behind the share back into the
 * tooltip — a share of two machines and a share of two hundred look the same.
 */
export function stackedChart(host, {
  rows, series, max: fixedMax, unit = 'findings', format = (v) => nf0.format(v), tip,
}) {
  const width = Math.max(320, host.clientWidth || 520);
  const gutter = Math.min(150, Math.max(80, ...rows.map((r) => textWidth(r.label, 12) + 12)));
  const m = { top: 6, right: 46, bottom: 6, left: gutter };
  const rowH = 34;
  const height = rowH * rows.length + m.top + m.bottom;
  const plotW = width - m.left - m.right;
  const max = fixedMax
    || Math.max(1, ...rows.map((r) => series.reduce((a, s) => a + (r.values[s.key] || 0), 0)));
  const barH = Math.min(24, rowH - 12);
  const root = svg('svg', { class: 'chart', viewBox: `0 0 ${width} ${height}`, width, height });

  rows.forEach((r, i) => {
    const cy = m.top + rowH * i + rowH / 2;
    const total = series.reduce((a, s) => a + (r.values[s.key] || 0), 0);
    let cursor = m.left;
    root.append(svg('text', {
      x: m.left - 10, y: cy + 4, 'text-anchor': 'end',
      fill: ink('--text-primary'), 'font-size': 12,
    }, [document.createTextNode(r.label)]));

    series.forEach((s, si) => {
      const v = r.values[s.key] || 0;
      if (!v) return;
      const raw = (v / max) * plotW;
      const gap = si < series.length - 1 ? 2 : 0;
      const w = Math.max(2, raw - gap);
      const dir = cursor + raw >= m.left + plotW - 0.5 ? 'right' : 'square';
      root.append(svg('path', {
        d: dir === 'right'
          ? barPath(cursor, cy - barH / 2, w, barH, 4, 'right')
          : `M${cursor},${cy - barH / 2}L${cursor + w},${cy - barH / 2}`
            + `L${cursor + w},${cy + barH / 2}L${cursor},${cy + barH / 2}Z`,
        fill: s.colour,
      }));
      // Yellow sits below 3:1 on the light surface, so the count is written on
      // the segment whenever it fits; where it doesn't, the tooltip and the
      // table view carry it.
      if (w > textWidth(format(v), 11) + 14) {
        root.append(svg('text', {
          x: cursor + w / 2, y: cy + 4, 'text-anchor': 'middle',
          fill: '#0b0b0b', 'font-size': 11, 'font-weight': 600,
          'font-variant-numeric': 'tabular-nums',
        }, [document.createTextNode(format(v))]));
      }
      root.append(hoverable(svg('rect', {
        x: cursor, y: cy - rowH / 2, width: Math.max(w, 8), height: rowH,
        fill: 'transparent', role: 'img', 'aria-label': `${r.label}, ${s.label}: ${format(v)}`,
      }), () => tipBlock(`${r.label} · ${s.label}`, tip ? tip(r, s, v) : [[unit, format(v), s.colour]])));
      cursor += raw;
    });

    root.append(svg('text', {
      x: m.left + plotW + 10, y: cy + 4, fill: ink('--text-secondary'),
      'font-size': 11, 'font-variant-numeric': 'tabular-nums',
    }, [document.createTextNode(format(total))]));
  });
  host.append(root);
}

/**
 * Score over time. A crosshair finds the X so the reader aims at a date rather
 * than at a 2px line.
 *
 * One series by default — a machine's runs, with those measured another way
 * muted. The fleet's timeline passes `series` instead, each a `{ label,
 * colour, value(p) }` on one shared axis, with `time(p)` for where a point
 * sits and `readout(p, series)` for its tooltip. Left out, every one of those
 * falls back to what this drew before it took them.
 */
export function lineChart(host, {
  points, series, time = (p) => p.taken_at, readout: readoutOf,
}) {
  const width = Math.max(320, host.clientWidth || 520);
  const m = { top: 20, right: 54, bottom: 34, left: 46 };
  const plotH = 180;
  const height = plotH + m.top + m.bottom;
  const plotW = width - m.left - m.right;
  const lines = series || [{ value: (p) => p.score, colour: ink('--series-1'), muted: (p) => !p.comparable }];
  const has = (v) => v !== null && v !== undefined;
  const times = points.map((p) => +new Date(time(p)));
  const t0 = Math.min(...times);
  const t1 = Math.max(...times);
  const { max, ticks } = niceTicks(Math.max(...lines.flatMap((s) => points.map((p) => s.value(p) || 0))), 4, 1);
  const x = (t) => (t1 === t0 ? m.left + plotW / 2 : m.left + ((t - t0) / (t1 - t0)) * plotW);
  const y = (v) => m.top + plotH - (v / max) * plotH;
  const root = svg('svg', { class: 'chart', viewBox: `0 0 ${width} ${height}`, width, height });

  for (const t of ticks) {
    root.append(svg('line', {
      x1: m.left, x2: m.left + plotW, y1: y(t), y2: y(t),
      stroke: t === 0 ? ink('--axis') : ink('--grid'), 'stroke-width': 1,
    }));
    root.append(svg('text', {
      x: m.left - 8, y: y(t) + 4, 'text-anchor': 'end', fill: ink('--text-muted'),
      'font-size': 11, 'font-variant-numeric': 'tabular-nums',
    }, [document.createTextNode(nf0.format(t))]));
  }

  const tickCount = Math.min(points.length, 5);
  for (let i = 0; i < tickCount; i += 1) {
    const t = t0 + ((t1 - t0) * i) / Math.max(tickCount - 1, 1);
    root.append(svg('text', {
      x: x(t), y: m.top + plotH + 18, 'text-anchor': 'middle',
      fill: ink('--text-muted'), 'font-size': 11,
    }, [document.createTextNode(day(new Date(t).toISOString()))]));
  }

  // Every line before any marker, so a marker is never painted under another
  // series' line.
  for (const s of lines) {
    const kept = points.filter((p) => !(s.muted && s.muted(p)) && has(s.value(p)));
    const path = kept.map((p, i) => `${i ? 'L' : 'M'}${x(+new Date(time(p)))},${y(s.value(p))}`).join('');
    if (kept.length > 1) {
      root.append(svg('path', {
        d: path, fill: 'none', stroke: s.colour, 'stroke-width': 2,
        'stroke-linejoin': 'round', 'stroke-linecap': 'round',
      }));
    }
  }

  for (const s of lines) {
    points.forEach((p) => {
      if (!has(s.value(p))) return;
      // Runs measured under another configuration are context, not the series:
      // the emphasis pattern rather than a second categorical hue.
      const colour = s.muted && s.muted(p) ? ink('--deemph') : s.colour;
      root.append(svg('circle', {
        cx: x(+new Date(time(p))), cy: y(s.value(p)), r: 4.5, fill: colour,
        stroke: ink('--surface-1'), 'stroke-width': 2,
      }));
    });
  }

  // The latest value of each line, beside its end. Two lines ending close
  // together would print one figure over the other, so a label that would
  // collide steps down a line.
  const last = points[points.length - 1];
  const used = [];
  for (const s of lines) {
    if (!last || !has(s.value(last))) continue;
    let ly = y(s.value(last)) + 4;
    while (used.some((u) => Math.abs(u - ly) < 12)) ly += 12;
    used.push(ly);
    root.append(svg('text', {
      x: Math.min(x(+new Date(time(last))) + 10, width - 4), y: Math.min(ly, height),
      fill: ink('--text-secondary'), 'font-size': 11, 'font-variant-numeric': 'tabular-nums',
    }, [document.createTextNode(nf0.format(s.value(last)))]));
  }

  const hair = svg('line', {
    x1: 0, x2: 0, y1: m.top, y2: m.top + plotH, stroke: ink('--axis'), 'stroke-width': 1,
    visibility: 'hidden',
  });
  root.append(hair);

  const overlay = svg('rect', {
    x: m.left, y: m.top, width: plotW, height: plotH, fill: 'transparent', tabindex: '0',
  });
  const nearest = (clientX) => {
    const box = root.getBoundingClientRect();
    const px = ((clientX - box.left) / box.width) * width;
    let best = points[0];
    let bestD = Infinity;
    for (const p of points) {
      const d = Math.abs(x(+new Date(time(p))) - px);
      if (d < bestD) { bestD = d; best = p; }
    }
    return best;
  };
  const readout = readoutOf ? (p) => readoutOf(p, lines) : (p) => tipBlock(when(p.taken_at), [
    ['score', p.score === null ? '—' : nf0.format(p.score), p.comparable ? ink('--series-1') : ink('--deemph')],
    ['grade', p.grade || '—'],
    ...(p.comparable ? [] : [[`measured with ${p.comparability.preset} / ${p.comparability.build_isa}`, 'not comparable']]),
    ...(p.thermal_limited ? [['clocks fell during the run', 'caveat']] : []),
    ...(p.on_ac === false ? [['measured on battery', 'caveat']] : []),
    ...(p.partial ? [['not everything ran', 'caveat']] : []),
    [`loadbearer ${p.tool_version}`, ''],
  ]);
  overlay.addEventListener('pointermove', (e) => {
    const p = nearest(e.clientX);
    hair.setAttribute('x1', x(+new Date(time(p))));
    hair.setAttribute('x2', x(+new Date(time(p))));
    hair.setAttribute('visibility', 'visible');
    tip.show(e, () => readout(p));
  });
  overlay.addEventListener('pointerleave', () => {
    hair.setAttribute('visibility', 'hidden');
    tip.hide();
  });
  overlay.addEventListener('focus', () => {
    const p = points[points.length - 1];
    const box = overlay.getBoundingClientRect();
    tip.show({ clientX: box.right - 40, clientY: box.top }, () => readout(p));
  });
  overlay.addEventListener('blur', () => tip.hide());
  root.append(overlay);
  host.append(root);
}

/** Half the width of a mark. With its 2px ring a mark is 12px across: small
 *  enough that a few hundred share a plot, and above the 8px floor. */
const MARK_R = 4;

/**
 * One scatter mark: a circle, a square or a triangle, of about equal area so no
 * shape reads as bigger news than another. Filled marks carry the surface ring
 * every marker does and a little transparency, so where they crowd they stay
 * countable; a hollow mark is the outline alone, in its colour.
 */
function markNode(cx, cy, { shape = 'circle', colour, hollow = false }) {
  const paint = hollow
    ? { fill: 'none', stroke: colour, 'stroke-width': 2, 'stroke-opacity': 0.9 }
    : { fill: colour, 'fill-opacity': 0.85, stroke: ink('--surface-1'), 'stroke-width': 2 };
  if (shape === 'square') {
    const h = MARK_R * 0.9;
    return svg('path', {
      d: `M${cx - h},${cy - h}L${cx + h},${cy - h}L${cx + h},${cy + h}L${cx - h},${cy + h}Z`, ...paint,
    });
  }
  if (shape === 'triangle') {
    const h = MARK_R * 1.25;
    return svg('path', {
      d: `M${cx},${cy - h}L${cx + h},${cy + h * 0.7}L${cx - h},${cy + h * 0.7}Z`, ...paint,
    });
  }
  return svg('circle', { cx, cy, r: MARK_R, ...paint });
}

/** A legend whose keys are the marks themselves, for a chart where shape and
 *  fill carry meaning as well as colour. */
export function markLegend(items) {
  return el('div', { class: 'legend' }, items.map((it) => el('span', { class: 'legend-item' }, [
    svg('svg', {
      class: 'legend-mark', width: 14, height: 14, viewBox: '0 0 14 14', 'aria-hidden': 'true',
    }, [markNode(7, 7, it)]),
    el('span', { text: it.label }),
  ])));
}

/**
 * Two continuous measures, one mark per thing. Each point is `{ x, y, colour,
 * shape, hollow, layer }`: the layer orders the paint, lowest first, so the
 * marks that call for a decision sit on top of the ones that don't.
 *
 * Not a hit target per mark. At a few hundred marks those overlap into each
 * other and put a few hundred stops in the tab order, so — as the line chart's
 * crosshair finds the nearest date — one overlay finds the nearest mark, the
 * arrow keys step through them in x order, and Enter does what a click does.
 * On touch the first tap shows a mark and a second opens it, since there is no
 * hover to preview with.
 *
 * `xRef` draws a reference line, labelled `xRefLabel`. Nothing to plot draws
 * nothing: an empty frame is noise, and `empty` says why instead.
 */
export function scatterChart(host, {
  points, xTitle, yTitle, xRef = null, xRefLabel = '', readout, onSelect,
  ariaLabel = 'Scatter plot', empty = 'Nothing to plot.',
}) {
  if (!points.length) {
    host.append(el('div', { class: 'empty', text: empty }));
    return;
  }
  const width = Math.max(320, host.clientWidth || 520);
  const m = { top: 26, right: 16, bottom: 44, left: 48 };
  const plotH = 260;
  const height = plotH + m.top + m.bottom;
  const plotW = width - m.left - m.right;
  const yScale = niceTicks(Math.max(...points.map((p) => p.y), 0), 4, 1);
  // Room for a whole-number tick roughly every 56px, so a phone gets fewer.
  const xScale = niceTicks(Math.max(...points.map((p) => p.x), xRef ?? 0, 1),
    Math.max(2, Math.min(8, Math.floor(plotW / 56))), 1);
  const x = (v) => m.left + (v / xScale.max) * plotW;
  const y = (v) => m.top + plotH - (v / yScale.max) * plotH;
  const root = svg('svg', { class: 'chart', viewBox: `0 0 ${width} ${height}`, width, height });

  for (const t of yScale.ticks) {
    root.append(svg('line', {
      x1: m.left, x2: m.left + plotW, y1: y(t), y2: y(t),
      stroke: t === 0 ? ink('--axis') : ink('--grid'), 'stroke-width': 1,
    }));
    root.append(svg('text', {
      x: m.left - 8, y: y(t) + 4, 'text-anchor': 'end',
      fill: ink('--text-muted'), 'font-size': 11, 'font-variant-numeric': 'tabular-nums',
    }, [document.createTextNode(nf0.format(t))]));
  }
  for (const t of xScale.ticks) {
    root.append(svg('line', {
      x1: x(t), x2: x(t), y1: m.top, y2: m.top + plotH,
      stroke: t === 0 ? ink('--axis') : ink('--grid'), 'stroke-width': 1,
    }));
    root.append(svg('text', {
      x: x(t), y: m.top + plotH + 16, 'text-anchor': 'middle',
      fill: ink('--text-muted'), 'font-size': 11, 'font-variant-numeric': 'tabular-nums',
    }, [document.createTextNode(nf0.format(t))]));
  }
  root.append(svg('text', {
    x: m.left + plotW / 2, y: height - 6, 'text-anchor': 'middle', fill: ink('--text-secondary'), 'font-size': 11,
  }, [document.createTextNode(xTitle)]));
  root.append(svg('text', {
    x: 4, y: m.top - 12, 'text-anchor': 'start', fill: ink('--text-secondary'), 'font-size': 11,
  }, [document.createTextNode(yTitle)]));

  // A threshold, not data: a solid hairline in the axis's ink, labelled on
  // whichever side has room so it never runs off the plot.
  if (xRef !== null) {
    const rx = x(xRef);
    root.append(svg('line', {
      x1: rx, x2: rx, y1: m.top, y2: m.top + plotH, stroke: ink('--text-muted'), 'stroke-width': 1,
    }));
    const right = rx + 6 + textWidth(xRefLabel, 11) <= m.left + plotW;
    root.append(svg('text', {
      x: right ? rx + 6 : rx - 6, y: m.top + 12, 'text-anchor': right ? 'start' : 'end',
      fill: ink('--text-secondary'), 'font-size': 11,
    }, [document.createTextNode(xRefLabel)]));
  }

  const painted = points.slice().sort((a, b) => (a.layer || 0) - (b.layer || 0) || a.x - b.x);
  for (const p of painted) root.append(markNode(x(p.x), y(p.y), p));

  // Keyboard order: left to right, and top to bottom at one age.
  const order = points.slice().sort((a, b) => a.x - b.x || b.y - a.y);
  const ring = svg('circle', {
    cx: x(order[0].x), cy: y(order[0].y), r: MARK_R * 2, fill: 'none',
    stroke: ink('--text-primary'), 'stroke-width': 2, visibility: 'hidden',
  });
  root.append(ring);

  const overlay = svg('rect', {
    x: m.left - 6, y: m.top - 6, width: plotW + 12, height: plotH + 12, fill: 'transparent', tabindex: '0',
    role: 'img',
    'aria-label': `${ariaLabel}. ${points.length} mark(s); the arrow keys step through them`
      + `${onSelect ? ' and Enter opens one' : ''}. The table view lists every one.`,
  });
  const view = (e) => {
    const box = root.getBoundingClientRect();
    return [((e.clientX - box.left) / box.width) * width, ((e.clientY - box.top) / box.height) * height];
  };
  const nearest = ([vx, vy]) => {
    let best = null;
    let bestD = 14 * 14;
    for (const p of points) {
      const d = (x(p.x) - vx) ** 2 + (y(p.y) - vy) ** 2;
      if (d <= bestD) { bestD = d; best = p; }
    }
    return best;
  };
  // Marks that land on the same pixel are one mark to a pointer, so the
  // tooltip owns up to the rest.
  const stacked = (p) => points.filter((q) => Math.abs(x(q.x) - x(p.x)) < 1 && Math.abs(y(q.y) - y(p.y)) < 1).length - 1;
  let current = null;
  const show = (p, ev) => {
    current = p;
    ring.setAttribute('cx', x(p.x));
    ring.setAttribute('cy', y(p.y));
    ring.setAttribute('visibility', 'visible');
    const box = root.getBoundingClientRect();
    const at = ev || {
      clientX: box.left + (x(p.x) / width) * box.width,
      clientY: box.top + (y(p.y) / height) * box.height,
    };
    tip.show(at, () => readout(p, stacked(p)));
  };
  const clear = () => {
    current = null;
    ring.setAttribute('visibility', 'hidden');
    overlay.setAttribute('cursor', 'default');
    tip.hide();
  };
  overlay.addEventListener('pointermove', (e) => {
    const p = nearest(view(e));
    if (!p) { clear(); return; }
    overlay.setAttribute('cursor', onSelect ? 'pointer' : 'default');
    show(p, e);
  });
  overlay.addEventListener('pointerleave', clear);
  overlay.addEventListener('click', (e) => {
    const p = nearest(view(e));
    if (!p) return;
    if (e.pointerType === 'touch' && current !== p) { show(p, e); return; }
    if (onSelect) onSelect(p);
  });
  overlay.addEventListener('focus', () => show(current || order[0]));
  overlay.addEventListener('blur', clear);
  overlay.addEventListener('keydown', (e) => {
    const i = Math.max(0, order.indexOf(current));
    const step = { ArrowRight: 1, ArrowDown: 1, ArrowLeft: -1, ArrowUp: -1 }[e.key];
    let next = null;
    if (step) next = order[(i + step + order.length) % order.length];
    else if (e.key === 'Home') next = order[0];
    else if (e.key === 'End') next = order[order.length - 1];
    else if (e.key === 'Enter' && current && onSelect) { onSelect(current); return; }
    if (next) { e.preventDefault(); show(next); }
  });
  root.append(overlay);
  host.append(root);
}

/**
 * A histogram over a continuous axis: one column per bin, the bins' own edges
 * on the axis, and reference lines for the quantiles in `marks`.
 *
 * Columns rather than touching bars, because the 24px cap holds here as it
 * does everywhere — each sits in the middle of its bin, and the hit target is
 * the whole bin. The mark labels each get a line of their own above the plot,
 * so p10, median and p90 can never print over each other however close they
 * fall. A distribution of identical values is one bin of no width, drawn as a
 * single column in the middle rather than divided by a zero range.
 */
export function histogramChart(host, {
  bins, marks = [], format = (v) => num(v), selected = null, onSelect, caption = '',
}) {
  if (!bins.length) {
    host.append(el('div', { class: 'empty', text: 'Nothing to draw.' }));
    return;
  }
  const width = Math.max(320, host.clientWidth || 520);
  const m = { top: 10 + 13 * marks.length, right: 26, bottom: caption ? 50 : 34, left: 46 };
  const plotH = 170;
  const height = plotH + m.top + m.bottom;
  const plotW = width - m.left - m.right;
  const lo = bins[0].lo;
  const hi = bins[bins.length - 1].hi;
  const span = hi - lo;
  const bandW = span > 0 ? plotW / bins.length : plotW / 3;
  const x = (v) => (span > 0 ? m.left + ((v - lo) / span) * plotW : m.left + plotW / 2);
  const bandX = (i) => (span > 0 ? x(bins[i].lo) : m.left + plotW / 3);
  const { max, ticks } = niceTicks(Math.max(...bins.map((b) => b.machines), 0), 4, 1);
  const y = (v) => m.top + plotH - (v / max) * plotH;
  const root = svg('svg', { class: 'chart', viewBox: `0 0 ${width} ${height}`, width, height, role: 'img' });

  for (const t of ticks) {
    root.append(svg('line', {
      x1: m.left, x2: m.left + plotW, y1: y(t), y2: y(t),
      stroke: t === 0 ? ink('--axis') : ink('--grid'), 'stroke-width': 1,
    }));
    root.append(svg('text', {
      x: m.left - 8, y: y(t) + 4, 'text-anchor': 'end',
      fill: ink('--text-muted'), 'font-size': 11, 'font-variant-numeric': 'tabular-nums',
    }, [document.createTextNode(nf0.format(t))]));
  }

  // A label is kept inside the chart's own box: centred on its point where
  // there is room, pulled in at either edge where there is not.
  const placed = (at, text, px) => {
    const half = textWidth(text, px) / 2;
    return Math.min(Math.max(at, half + 2), width - half - 2);
  };

  const barW = Math.min(24, Math.max(4, bandW - 6));
  bins.forEach((b, i) => {
    const cx = bandX(i) + bandW / 2;
    const h = (b.machines / max) * plotH;
    // The chosen bin keeps the series colour and the rest step back: the
    // emphasis pattern, not a second hue.
    const colour = selected === null || selected === i ? ink('--series-1') : ink('--deemph');
    if (h > 0) {
      root.append(svg('path', { d: barPath(cx - barW / 2, y(b.machines), barW, h, 4, 'up'), fill: colour }));
      if (bandW >= textWidth(nf0.format(b.machines), 11) + 4) {
        root.append(svg('text', {
          x: cx, y: y(b.machines) - 6, 'text-anchor': 'middle',
          fill: ink('--text-secondary'), 'font-size': 11, 'font-variant-numeric': 'tabular-nums',
        }, [document.createTextNode(nf0.format(b.machines))]));
      }
    }
    const range = span > 0 ? `${format(b.lo)} to ${format(b.hi)}` : format(b.lo);
    const hit = hoverable(svg('rect', {
      x: bandX(i), y: m.top, width: bandW, height: plotH, fill: 'transparent', role: 'img',
      'aria-label': `${range}: ${b.machines} machine(s)${onSelect ? '. Enter lists them' : ''}`,
    }), () => tipBlock(range, [['machines', nf0.format(b.machines), colour]]));
    if (onSelect) {
      hit.addEventListener('click', () => onSelect(i));
      hit.addEventListener('keydown', (e) => { if (e.key === 'Enter') onSelect(i); });
      hit.setAttribute('cursor', 'pointer');
    }
    root.append(hit);
  });

  // The edges, as many as fit without touching their neighbours.
  const edges = span > 0 ? [...bins.map((b) => b.lo), hi] : [lo];
  const widest = Math.max(...edges.map((e) => textWidth(format(e), 11)));
  const stride = span > 0 ? Math.max(1, Math.ceil((widest + 8) / bandW)) : 1;
  edges.forEach((e, k) => {
    if (k % stride !== 0) return;
    const text = format(e);
    root.append(svg('text', {
      x: placed(x(e), text, 11), y: m.top + plotH + 16, 'text-anchor': 'middle',
      fill: ink('--text-muted'), 'font-size': 11, 'font-variant-numeric': 'tabular-nums',
    }, [document.createTextNode(text)]));
  });

  // The quantiles: thresholds rather than data, so solid hairlines in the
  // muted ink, each named on a line of its own.
  marks.forEach((mk, i) => {
    const mx = x(mk.value);
    root.append(svg('line', {
      x1: mx, x2: mx, y1: m.top - 2, y2: m.top + plotH, stroke: ink('--text-muted'), 'stroke-width': 1,
    }));
    const text = `${mk.label} ${format(mk.value)}`;
    root.append(svg('text', {
      x: placed(mx, text, 11), y: 11 + 13 * i, 'text-anchor': 'middle',
      fill: ink('--text-secondary'), 'font-size': 11, 'font-variant-numeric': 'tabular-nums',
    }, [document.createTextNode(text)]));
  });

  root.append(svg('line', {
    x1: m.left, x2: m.left + plotW, y1: m.top + plotH, y2: m.top + plotH, stroke: ink('--axis'), 'stroke-width': 1,
  }));
  if (caption) {
    root.append(svg('text', {
      x: m.left + plotW / 2, y: height - 8, 'text-anchor': 'middle', fill: ink('--text-secondary'), 'font-size': 11,
    }, [document.createTextNode(caption)]));
  }
  host.append(root);
}

/**
 * One row per group: a line from p10 to p90 and a mark at the median, all on
 * one shared axis — the overlaid quantile rows a split distribution is drawn
 * as. A row is `{ label, p10, median, p90, machines }`.
 */
export function rangeChart(host, { rows, format = (v) => num(v), onSelect }) {
  if (!rows.length) {
    host.append(el('div', { class: 'empty', text: 'Nothing to draw.' }));
    return;
  }
  const width = Math.max(320, host.clientWidth || 520);
  const gutter = Math.min(170, Math.max(80, ...rows.map((r) => textWidth(r.label, 12) + 12)));
  const m = { top: 8, right: 26, bottom: 26, left: gutter };
  const rowH = 28;
  const plotH = rowH * rows.length;
  const height = plotH + m.top + m.bottom;
  const plotW = width - m.left - m.right;
  let lo = Math.min(...rows.map((r) => r.p10));
  let hi = Math.max(...rows.map((r) => r.p90));
  if (!(hi > lo)) {
    const pad = Math.abs(lo) * 0.05 || 1;
    lo -= pad;
    hi += pad;
  }
  const x = (v) => m.left + ((v - lo) / (hi - lo)) * plotW;
  const root = svg('svg', { class: 'chart', viewBox: `0 0 ${width} ${height}`, width, height });

  for (const [i, t] of [lo, (lo + hi) / 2, hi].entries()) {
    root.append(svg('line', {
      x1: x(t), x2: x(t), y1: m.top, y2: m.top + plotH, stroke: ink('--grid'), 'stroke-width': 1,
    }));
    const text = format(t);
    root.append(svg('text', {
      x: x(t), y: height - 8, 'text-anchor': ['start', 'middle', 'end'][i],
      fill: ink('--text-muted'), 'font-size': 10, 'font-variant-numeric': 'tabular-nums',
    }, [document.createTextNode(text)]));
  }

  rows.forEach((r, i) => {
    const cy = m.top + rowH * i + rowH / 2;
    root.append(svg('text', {
      x: m.left - 10, y: cy + 4, 'text-anchor': 'end', fill: ink('--text-primary'), 'font-size': 12,
    }, [document.createTextNode(r.label)]));
    root.append(svg('line', {
      x1: x(r.p10), x2: x(r.p90), y1: cy, y2: cy, stroke: ink('--series-1'), 'stroke-width': 2,
      'stroke-linecap': 'round',
    }));
    root.append(svg('circle', {
      cx: x(r.median), cy, r: 4.5, fill: ink('--series-1'), stroke: ink('--surface-1'), 'stroke-width': 2,
    }));
    const hit = hoverable(svg('rect', {
      x: 0, y: m.top + rowH * i, width, height: rowH, fill: 'transparent', role: 'img',
      'aria-label': `${r.label}: median ${format(r.median)}, p10 ${format(r.p10)}, p90 ${format(r.p90)}, `
        + `${r.machines} machine(s)`,
    }), () => tipBlock(r.label, [
      ['median', format(r.median), ink('--series-1')],
      ['p10 to p90', `${format(r.p10)} – ${format(r.p90)}`],
      ['machines', nf0.format(r.machines)],
    ]));
    if (onSelect) {
      hit.addEventListener('click', () => onSelect(r));
      hit.addEventListener('keydown', (e) => { if (e.key === 'Enter') onSelect(r); });
      hit.setAttribute('cursor', 'pointer');
    }
    root.append(hit);
  });
  host.append(root);
}

/* ------------------------------------------------------------ chart cards */

const redraws = [];

export function legend(items) {
  return el('div', { class: 'legend' }, items.map((it) => el('span', { class: 'legend-item' }, [
    Object.assign(el('span', { class: `legend-key${it.line ? ' line' : ''}` }), { style: `background:${it.colour}` }),
    el('span', { text: it.label }),
  ])));
}

export function dataTable({ columns, rows, onRow, sortable, initialSort }) {
  // initialSort is { key: columnIndex, dir: 1 | -1 }; without one, rows keep
  // the order they came in until a header is clicked.
  const state = { key: initialSort?.key ?? null, dir: initialSort?.dir ?? 1 };
  const wrap = el('div', { class: 'table-wrap' });
  const table = el('table', { class: 'data' });

  const draw = () => {
    let body = rows;
    if (state.key !== null) {
      const i = state.key;
      // A row's own sort key wins even when it is null, so "no value" sorts
      // last as the null check below intends. Falling back to the cell for a
      // null key compared '—' or an element against numbers instead.
      body = rows.slice().sort((a, b) => {
        const av = a.sort ? a.sort[i] : a.cells[i];
        const bv = b.sort ? b.sort[i] : b.cells[i];
        if (av === bv) return 0;
        if (av === null || av === undefined) return 1;
        if (bv === null || bv === undefined) return -1;
        return (av > bv ? 1 : -1) * state.dir;
      });
    }
    table.replaceChildren(
      el('thead', {}, [el('tr', {}, columns.map((c, i) => {
        const th = el('th', {
          text: c.label + (state.key === i ? (state.dir > 0 ? ' ▲' : ' ▼') : ''),
          class: [c.num ? 'num' : '', sortable ? 'sortable' : ''].filter(Boolean).join(' '),
        });
        if (sortable) {
          th.addEventListener('click', () => {
            if (state.key === i) state.dir *= -1; else { state.key = i; state.dir = c.num ? -1 : 1; }
            draw();
          });
        }
        return th;
      }))]),
      el('tbody', {}, body.map((r) => {
        const tr = el('tr', { class: onRow ? 'clickable' : null }, r.cells.map((cell, i) => (
          cell instanceof Node
            ? el('td', { class: columns[i].num ? 'num' : columns[i].wrap ? 'wrap' : null }, [cell])
            : el('td', {
              class: columns[i].num ? 'num' : columns[i].wrap ? 'wrap' : null,
              text: cell === null || cell === undefined ? '—' : String(cell),
            })
        )));
        if (onRow) {
          tr.addEventListener('click', () => onRow(r));
          tr.setAttribute('tabindex', '0');
          tr.addEventListener('keydown', (e) => { if (e.key === 'Enter') onRow(r); });
        }
        return tr;
      })),
    );
  };
  draw();
  wrap.append(table);
  return wrap;
}

/**
 * A chart and its table are twins: the toggle swaps one for the other, so
 * every value a chart shows is reachable without a pointer.
 */
function chartCard({ title, subtitle, note, span = 'col-6', draw, table }) {
  const body = el('div', {});
  const toggle = el('button', { type: 'button', class: 'btn btn-quiet', text: 'Table' });
  const entry = { host: body, mode: 'chart', render: null };

  entry.render = () => {
    body.replaceChildren();
    if (entry.mode === 'chart') draw(body); else body.append(table());
  };
  redraws.push(entry);

  toggle.addEventListener('click', () => {
    entry.mode = entry.mode === 'chart' ? 'table' : 'chart';
    toggle.textContent = entry.mode === 'chart' ? 'Table' : 'Chart';
    entry.render();
  });

  const card = el('div', { class: `card chart-card ${span}` }, [
    el('div', { class: 'card-head' }, [
      el('h3', { text: title }),
      el('div', { class: 'card-actions' }, [table ? toggle : null]),
    ]),
    subtitle ? el('p', { class: 'card-sub', text: subtitle }) : null,
    body,
    note ? el('p', { class: 'card-note', text: note }) : null,
  ]);
  // Deferred: clientWidth is 0 until the card is in the document.
  requestAnimationFrame(entry.render);
  return card;
}

window.addEventListener('resize', debounce(() => {
  for (const e of redraws) if (e.host.isConnected && e.mode === 'chart') e.render();
}, 150));

/* ------------------------------------------------------------------- state */

const SEVERITIES = [
  { key: 'critical', label: 'Critical', colour: 'var(--status-critical)' },
  { key: 'warning', label: 'Warning', colour: 'var(--status-warning)' },
  { key: 'info', label: 'Informational', colour: 'var(--status-info)' },
];

const QUEUES = {
  machine: 'The machine',
  measurement: 'The measurement',
  coverage: 'The data we hold',
};

/*
 * The lifecycle decisions, most urgent first, as the server orders them. Only
 * the two that cost money get a colour, and it is a status role that already
 * means "critical" and "warning" here — the rest share the de-emphasis gray,
 * because a third or fourth hue for "investigate" would be a new palette
 * entry with nothing validating it. Shape repeats what colour says, so no
 * decision rests on colour alone.
 */
const DECISIONS = [
  { key: 'replace', label: 'Replace', role: '--status-critical', dot: 'critical', shape: 'square', layer: 2,
    sub: 'the machine, not a part' },
  { key: 'repair', label: 'Repair', role: '--status-warning', dot: 'warning', shape: 'triangle', layer: 1,
    sub: 'a part, not the platform' },
  { key: 'investigate', label: 'Investigate', role: '--deemph', shape: 'circle', layer: 0,
    sub: 'slower than its hardware should be' },
  { key: 'watch', label: 'Watch', role: '--deemph', shape: 'circle', layer: 0, sub: 'old, or graded D' },
  { key: 'keep', label: 'Keep', role: '--deemph', shape: 'circle', layer: 0, sub: 'nothing calls for a decision' },
];

const decisionOf = (key) => DECISIONS.find((d) => d.key === key) || DECISIONS[DECISIONS.length - 1];
const decisionRank = (key) => DECISIONS.findIndex((d) => d.key === key);

/** "at most 6.2 yr" or "about 4.1 yr": the server's best estimate of a
 *  machine's age, worded as the bound from its CPU's launch or as a dating
 *  from its original firmware — which are different claims. */
function ageWords(est) {
  if (!est) return null;
  const yr = (est.months / 12).toFixed(1);
  return est.basis === 'firmware' ? `about ${yr} yr` : `at most ${yr} yr`;
}

/** The decision and its reasons in one line, for the drilldown. */
function lifecycleFact(m) {
  const l = m.lifecycle;
  if (!l) return null;
  if (l.verdict === 'keep') return 'Keep — nothing in the latest run calls for a decision';
  return `${decisionOf(l.verdict).label} — ${l.reasons.join('; ')}`
    + (l.uncertain ? `. Re-measure before acting: ${l.caveats.join('; ').toLowerCase()}` : '');
}

// Exported, with the view functions, so `scripts/check-ui.mjs` can render every
// view against a real snapshot outside a browser. That check is the only thing
// standing between a typo in here and a blank panel in production.
export const state = {
  snap: null, me: null, view: 'overview', key: null, filter: {}, showInfo: false,
  /// The printed report's period in weeks, and the dimension it groups by —
  /// null for no groups section. Its own, so printing does not move the
  /// overview's comparison or the Groups tab's dimension.
  reportWindow: 4,
  reportBy: null,
  /// Whether the queue shows snoozed and suppressed findings too. Acknowledged
  /// ones are always shown, drawn quieter: in hand is still worth seeing.
  showQuiet: false,
  /// The run ids being compared, in the order they were chosen: the first is
  /// the one everything else is measured against.
  runs: [],
  /// What the Groups view groups by, or null for the server's default. Kept
  /// across a trip to another tab, so coming back finds the same grouping.
  by: null,
  /// How many weeks back the overview's "What changed" compares with.
  window: 1,
  /// The decision the Lifecycle view's table is showing, or null for every
  /// one but keep.
  decision: null,
  /// The Components view's settings, each null for the server's default:
  /// the measure, its configuration, a dimension to split it by, the bin
  /// whose machines are listed, and the heatmap's encoding and sort. Kept in
  /// the hash so a view can be linked, and across a trip to another tab.
  measure: null,
  config: null,
  split: null,
  bin: null,
  encoding: null,
  hsort: null,
  hdir: null,
};

/** The filter fields the URL carries, all sent to the server as they are. */
const FILTER_KEYS = ['since_days', 'tags', 'flag', 'q', 'cohort', 'model', 'cpu', 'os', 'lifecycle'];

function parseHash() {
  const raw = location.hash.replace(/^#\/?/, '');
  const [path, query] = raw.split('?');
  const parts = (path || 'overview').split('/');
  const params = new URLSearchParams(query || '');
  const filter = {};
  for (const k of FILTER_KEYS) {
    const v = params.get(k);
    if (v) filter[k] = v;
  }
  // `report` is a view without a tab: a document reached from the overview.
  const named = ['overview', 'cohorts', 'groups', 'components', 'lifecycle', 'machines', 'compare', 'report'];
  const view = parts[0] === 'machine'
    ? 'machine'
    : (named.includes(parts[0]) ? parts[0] : 'overview');
  return {
    view,
    key: parts[0] === 'machine' ? decodeURIComponent(parts.slice(1).join('/')) : null,
    // The runs being compared sit in the path, not the query: the query is the
    // fleet filter, and a comparison is of *named runs*. Keeping them apart
    // means changing the filter cannot silently change what is being compared.
    runs: view === 'compare'
      ? (parts[1] || '').split(',').map(Number).filter((n) => Number.isInteger(n) && n > 0)
      : [],
    // Beside the filter in the query but not part of it: it says how to
    // arrange the machines in view, not which ones are in view.
    by: params.get('by') || null,
    // The same, for the overview's comparison. Passed to the server as it
    // is, so a hand-edited window is refused there, with its reason.
    window: Number(params.get('changes')) || 1,
    // The printed report's period, which defaults to four weeks rather than
    // one: a report is read at a review, and a review is rarely weekly.
    reportWindow: Number(params.get('changes')) || REPORT_WEEKS,
    // The Lifecycle view's tile, which picks the rows of its table. Not the
    // `lifecycle` filter: that narrows the whole fleet, the tiles' own counts
    // included, so choosing a tile through it would zero every other tile.
    decision: params.get('decision') || null,
    // The Components view's own settings, beside the filter for the same
    // reason `by` is. Passed to the server as they are, so a hand-edited
    // measure or sort is refused there, with its reason.
    measure: params.get('measure') || null,
    config: params.get('config') || null,
    split: params.get('split') || null,
    bin: /^\d+$/.test(params.get('bin') || '') ? Number(params.get('bin')) : null,
    encoding: params.get('encoding') || null,
    hsort: params.get('sort') || null,
    hdir: params.get('dir') || null,
    filter,
  };
}

function writeHash({
  view = state.view, key = state.key, filter = state.filter, runs = state.runs, by = state.by,
  window: weeks = state.window, decision = state.decision,
} = {}) {
  const params = new URLSearchParams(filter);
  if (view === 'groups' && by) params.set('by', by);
  if (view === 'lifecycle' && decision) params.set('decision', decision);
  if (view === 'overview' && weeks && weeks !== 1) params.set('changes', String(weeks));
  if (view === 'report') {
    if (state.reportWindow && state.reportWindow !== REPORT_WEEKS) params.set('changes', String(state.reportWindow));
    if (state.reportBy) params.set('by', state.reportBy);
  }
  if (view === 'components') {
    for (const [k, v] of [['measure', state.measure], ['config', state.config], ['split', state.split],
      ['bin', state.bin], ['encoding', state.encoding], ['sort', state.hsort], ['dir', state.hdir]]) {
      if (v !== null && v !== undefined) params.set(k, String(v));
    }
  }
  const q = params.toString();
  let path = view;
  if (view === 'machine') path = `machine/${encodeURIComponent(key)}`;
  if (view === 'compare') path = runs?.length ? `compare/${runs.join(',')}` : 'compare';
  location.hash = `#/${path}${q ? `?${q}` : ''}`;
}

/*
 * A 401 means the session has gone - expired, or the server restarted. Sending
 * the browser to sign in again is the only useful response; the alternative is
 * an error banner the reader can do nothing about.
 */
function signInAgain() {
  const next = location.pathname + location.search + location.hash;
  location.assign(`/auth/login?next=${encodeURIComponent(next)}`);
}

async function fetchJson(url, options) {
  const res = await fetch(url, options);
  if (res.status === 401) {
    signInAgain();
    // Never resolves: the navigation is already under way, and resolving would
    // let a caller render against nothing.
    return new Promise(() => {});
  }
  if (!res.ok) throw new Error(await res.text());
  return res.json();
}

async function fetchSnapshot() {
  const q = new URLSearchParams(state.filter).toString();
  return fetchJson(`/api/snapshot${q ? `?${q}` : ''}`);
}

/** The machines in view, grouped — through the same filter, so the same
 *  machines the snapshot holds and nothing the caller's scope excludes. */
async function fetchGroups(by = state.by) {
  const params = new URLSearchParams(state.filter);
  if (by) params.set('by', by);
  const q = params.toString();
  return fetchJson(`/api/groups${q ? `?${q}` : ''}`);
}

/** The fleet at each of the last few weeks, projected by the server onto the
 *  same filtered, scoped view as the snapshot. */
async function fetchTimeline() {
  const q = new URLSearchParams(state.filter).toString();
  return fetchJson(`/api/timeline${q ? `?${q}` : ''}`);
}

/** What changed in that view since `weeks` weeks ago. */
async function fetchChanges(weeks = state.window) {
  const params = new URLSearchParams(state.filter);
  params.set('window', String(weeks));
  return fetchJson(`/api/changes?${params}`);
}

/*
 * Who is signed in, and what they may do. The UI uses this to label the
 * session and to hide what the caller cannot use - the server enforces the
 * same rules regardless, so this is courtesy rather than security.
 */
async function fetchMe() {
  try {
    return await fetchJson('/api/me');
  } catch {
    return null;
  }
}

function applyIdentity() {
  const me = state.me;
  const who = $('#who');
  const signout = $('#signout');
  const rescan = $('#rescan');
  if (!me) {
    who.hidden = true;
    signout.hidden = true;
    return;
  }
  who.hidden = false;
  who.replaceChildren(
    el('span', { text: me.authenticated ? me.name : 'Local access' }),
    el('b', { text: me.role }),
  );
  who.title = me.email || (me.authenticated ? me.subject : 'No sign-in configured');
  signout.hidden = !me.sign_in_enabled;
  rescan.hidden = !me.may_rescan;
  $('#upload').hidden = !me.may_upload;
}

/**
 * Submit result documents, one request each.
 *
 * One at a time rather than a batch, because the interesting answer is per
 * file: this one was added, that one we already had, this third is not a
 * result at all. A single request for all of them would have to choose between
 * failing everything on one bad file and reporting nothing useful.
 *
 * The server refuses whatever it should refuse — role, scope, size, content —
 * so this reports rather than judges.
 */
export async function uploadAll(files) {
  const btn = $('#upload');
  const label = btn.querySelector('.btn-label') || btn;
  const note = $('#footer-note');
  btn.disabled = true;
  const results = [];
  try {
    for (const [i, file] of files.entries()) {
      label.textContent = files.length > 1 ? `${i + 1} of ${files.length}…` : 'Sending…';
      try {
        const text = await file.text();
        const r = await fetchJson('/api/upload', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: text,
        });
        results.push(`${file.name}: ${r.outcome}${r.hostname ? ` (${r.hostname})` : ''}`);
      } catch (err) {
        results.push(`${file.name}: ${err.message}`);
      }
    }
  } finally {
    btn.disabled = false;
    label.textContent = 'Add results';
  }
  note.textContent = results.join(' · ');
  // Deliberately does not redraw: sending files and refreshing the page are
  // two jobs, and keeping them apart is what lets this be tested without a
  // whole dashboard around it. The caller refreshes.
  return results;
}

/** Say so when a viewer is only seeing part of the estate. */
function scopeNotice() {
  const scopes = state.me?.scopes || [];
  if (!scopes.length) return null;
  const described = scopes
    .map((s) => Object.entries(s).map(([k, v]) => `${k}=${v}`).join(' and '))
    .join(', or ');
  return el('div', { class: 'banner' }, [el('span', {
    text: `You are seeing the machines tagged ${described}. Counts, findings and everything `
      + 'below are for those machines only; cohort medians are still drawn from the whole fleet, '
      + 'so a comparison here means the same as it does anywhere else.',
  })]);
}

/* -------------------------------------------------------------- filter row */

function syncFilterControls() {
  $('#f-since').value = state.filter.since_days || '';
  $('#f-q').value = state.filter.q || '';
  $('#f-flag').value = state.filter.flag || '';

  const flagSelect = $('#f-flag');
  const codes = [...new Set((state.snap?.flags || []).map((f) => f.code))].sort();
  const keep = flagSelect.value;
  flagSelect.replaceChildren(
    el('option', { value: '', text: 'Any' }),
    ...codes.map((c) => el('option', { value: c, text: c.replace(/_/g, ' ') })),
  );
  flagSelect.value = codes.includes(keep) ? keep : '';

  // Tag filters are built from what the fleet actually reports rather than
  // from a fixed list, because tags are whatever the deployment tool passed.
  const host = $('#f-tags');
  const values = new Map();
  for (const m of state.snap?.machines || []) {
    for (const [k, v] of Object.entries(m.tags || {})) {
      if (!values.has(k)) values.set(k, new Set());
      values.get(k).add(v);
    }
  }
  const active = new Map((state.filter.tags || '').split(',').filter(Boolean)
    .map((p) => p.split('=')).filter((p) => p.length === 2));
  // A tag being filtered on must stay offered even though every machine in
  // view now shares its value.
  for (const [k, v] of active) if (!values.has(k)) values.set(k, new Set([v]));

  host.replaceChildren(...[...values.entries()].sort().map(([k, set]) => {
    const sel = el('select', { dataset: { tag: k } }, [
      el('option', { value: '', text: 'Any' }),
      ...[...set].sort().map((v) => el('option', { value: v, text: v })),
    ]);
    sel.value = active.get(k) || '';
    sel.addEventListener('change', () => {
      const next = new Map(active);
      if (sel.value) next.set(k, sel.value); else next.delete(k);
      const tags = [...next.entries()].map(([a, b]) => `${a}=${b}`).join(',');
      applyFilter({ tags: tags || undefined });
    });
    return el('label', { class: 'field' }, [
      el('span', { class: 'field-label', text: k }), sel,
    ]);
  }));

  // A group drilled into from the Groups view. There is no list of values to
  // offer, and a filter nobody can see is one nobody thinks to clear — so
  // each is shown as itself, and pressing it lets go of it.
  const shown = (k) => (k === 'lifecycle' ? decisionOf(state.filter[k]).label : state.filter[k]);
  $('#f-exact').replaceChildren(...EXACT_FILTERS.filter(([k]) => state.filter[k]).map(([k, label]) => (
    el('span', { class: 'field filter-exact' }, [
      el('span', { class: 'field-label', text: label }),
      el('button', {
        type: 'button', class: 'btn', title: shown(k),
        'aria-label': `${label} is ${shown(k)}. Stop filtering by it`,
        onclick: () => applyFilter({ [k]: undefined }),
      }, [el('span', { class: 'filter-exact-value', text: shown(k) }), el('span', { text: ' ✕', 'aria-hidden': 'true' })]),
    ])
  )));
}

/** The exact-match filters, by query parameter, with what to call each. A
 *  decision is set from the Groups view's replace and repair counts. */
const EXACT_FILTERS = [['model', 'System model'], ['cpu', 'CPU'], ['os', 'OS'], ['lifecycle', 'Decision']];

/**
 * The tab strip scrolls inside itself where the tabs outgrow the row — a
 * phone, with six of them — which leaves two jobs no scrollbar does: keep
 * the current tab in view, since it may be the one scrolled off, and show
 * that there is more to either side. The strip hides its scrollbar (a bar
 * under the navigation is worse than none), so the cue is a fade on the edge
 * where tabs continue, set here because only script knows the scroll offset.
 *
 * `reveal` is for a view change or a resize; a scroll by hand only updates the
 * fade, so the strip does not fight the reader's finger.
 */
function syncTabStrip(reveal) {
  const strip = $('#tabs');
  if (!strip) return;
  const active = strip.querySelector('[aria-current="page"]');
  if (reveal && active) {
    // Clear of the 28px fade, so the current tab is never the faded one.
    const pad = 28;
    const s = strip.getBoundingClientRect();
    const t = active.getBoundingClientRect();
    if (t.left < s.left + pad) strip.scrollLeft -= s.left + pad - t.left;
    else if (t.right > s.right - pad) strip.scrollLeft += t.right - (s.right - pad);
  }
  const hidden = strip.scrollWidth - strip.clientWidth;
  strip.classList.toggle('more-before', hidden > 1 && strip.scrollLeft > 1);
  strip.classList.toggle('more-after', hidden > 1 && strip.scrollLeft < hidden - 1);
}

function applyFilter(patch) {
  const filter = { ...state.filter, ...patch };
  for (const k of Object.keys(filter)) if (!filter[k]) delete filter[k];
  state.filter = filter;
  writeHash({ view: state.view === 'machine' ? 'machines' : state.view, filter });
}

function wireFilters() {
  $('#f-since').addEventListener('change', (e) => applyFilter({ since_days: e.target.value }));
  $('#f-flag').addEventListener('change', (e) => applyFilter({ flag: e.target.value }));
  $('#f-q').addEventListener('input', debounce((e) => applyFilter({ q: e.target.value }), 300));
  $('#f-reset').addEventListener('click', () => { state.filter = {}; writeHash({ filter: {} }); });
  $('#filters').addEventListener('submit', (e) => e.preventDefault());

  $('#tabs').addEventListener('click', (e) => {
    const btn = e.target.closest('.tab');
    if (btn) writeHash({ view: btn.dataset.view, key: null });
  });
  $('#tabs').addEventListener('scroll', () => syncTabStrip(false), { passive: true });
  window.addEventListener('resize', debounce(() => syncTabStrip(true), 150));
  // The strip also narrows without the window resizing: a desktop page that
  // grows long enough for a scrollbar loses its width after the view has
  // drawn, and with seven tabs at 740px that was enough to push the last one
  // out from under a strip that had already decided nothing was hidden.
  if (typeof ResizeObserver !== 'undefined') {
    new ResizeObserver(() => syncTabStrip(true)).observe($('#tabs'));
  }

  $('#rescan').addEventListener('click', async () => {
    const btn = $('#rescan');
    const label = btn.querySelector('.btn-label') || btn;
    btn.disabled = true;
    label.textContent = 'Scanning…';
    try {
      const r = await fetchJson('/api/rescan', { method: 'POST' });
      await route();
      $('#footer-note').textContent =
        `Rescanned: ${r.seen} file(s), ${r.ingested} new, ${r.unchanged} already indexed`
        + (r.rejected.length ? `, ${r.rejected.length} unreadable` : '');
    } catch (err) {
      $('#footer-note').textContent = `Rescan failed: ${err.message}`;
    } finally {
      btn.disabled = false;
      label.textContent = 'Rescan folder';
    }
  });

  // A real file input behind the button rather than a drop zone alone, so it
  // is reachable by keyboard and by a screen reader, and so the browser's own
  // file picker does the work.
  $('#upload').addEventListener('click', () => $('#upload-input').click());
  $('#upload-input').addEventListener('change', async (e) => {
    const files = [...e.target.files];
    // Cleared immediately, so choosing the same file twice still fires.
    e.target.value = '';
    if (!files.length) return;
    await uploadAll(files);
    // The server notices its own index changed on the next request anyway;
    // this is asking for that now rather than at the next click.
    await route();
  });

  const themeBtn = $('#theme');
  const stored = localStorage.getItem('lbf-theme');
  if (stored) document.documentElement.dataset.theme = stored;
  themeBtn.addEventListener('click', () => {
    const now = document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark';
    document.documentElement.dataset.theme = now;
    localStorage.setItem('lbf-theme', now);
    // Marks carry resolved hex, so a theme change is a redraw.
    for (const e of redraws) if (e.host.isConnected) e.render();
  });
}

/* -------------------------------------------------------------------- bits */

function severityChip(sev) {
  const label = SEVERITIES.find((s) => s.key === sev)?.label || sev;
  return el('span', { class: 'chip' }, [
    el('span', { class: `chip-dot dot-${sev}` }),
    el('span', { text: label }),
  ]);
}

function machineLink(m) {
  const name = m.hostname || m.key;
  return el('button', {
    type: 'button', class: 'link-button', text: name,
    onclick: () => writeHash({ view: 'machine', key: m.key }),
  });
}

function deltaCell(v) {
  if (v === null || v === undefined) return el('span', { class: 'meta', text: '—' });
  return el('span', { class: v < -0.05 ? 'delta-down' : v > 0.05 ? 'delta-up' : '', text: signed(v, 1) });
}

/* ------------------------------------------------------------------ triage */

/*
 * Acknowledge, snooze, suppress. Triage is an attention layer over the
 * findings, not a change to them: the server still computes every finding,
 * the lifecycle verdict and the history ignore it, and what changes is
 * whether a finding counts as needing attention. Each finding carries its own
 * `triage` (null while live), so the page only ever lays out what the server
 * already decided.
 *
 * The controls are hidden from a caller who may not use them, as Rescan is —
 * a courtesy; the server refuses regardless.
 */

const SNOOZES = [['1 week', 7], ['1 month', 30], ['3 months', 91]];

/** The day, UTC, `days` from today — a snooze's `until`. */
function daysFromNow(days) {
  return new Date(Date.now() + days * 86400000).toISOString().slice(0, 10);
}

const codeWords = (code) => code.replace(/_/g, ' ');

/**
 * Send one triage change, then refetch and redraw, and say what happened in
 * the footer — the same place Rescan reports. Exported, like `uploadAll`, so
 * `check-ui.mjs` can check the request it makes without a page around it.
 */
export async function sendTriage(url, { method = 'POST', body, done } = {}) {
  const note = $('#footer-note');
  try {
    await fetchJson(url, body === undefined ? { method } : {
      method,
      // The server's CSRF defence: only JSON is accepted.
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    }).catch((err) => {
      // A DELETE answers 204 with no body, which `res.json()` cannot parse;
      // that is success, not a failure to report.
      if (method === 'DELETE' && err instanceof SyntaxError) return null;
      throw err;
    });
    if (note) note.textContent = done;
    if ($('#main')) await route();
    return true;
  } catch (err) {
    if (note) note.textContent = `Could not change that finding: ${err.message}`;
    return false;
  }
}

/** "Acknowledged by Iain · 9 Sep 2026 · fan on order", for a quieted finding. */
function triageWords(t) {
  const by = `by ${t.by} · ${day(t.at)}`;
  let what;
  if (t.state === 'acknowledged') what = `Acknowledged ${by}`;
  else if (t.state === 'snoozed') what = `Snoozed until ${t.until} ${by}`;
  else {
    const reach = t.reach === 'machine' ? 'on this machine'
      : t.reach === 'tags' ? `where ${Object.entries(t.tags || {}).map(([k, v]) => `${k}=${v}`).join(', ')}`
        : 'across the fleet';
    what = `Suppressed ${reach} ${by}`;
  }
  return t.note ? `${what} · ${t.note}` : what;
}

/**
 * What a finding says about its triage, and the controls to change it — the
 * one helper both the overview's queue and a machine's Findings card use.
 *
 * Quieted: the state, who, when, any note, and Undo for a caller who may undo
 * it (a suppression is undone by removing its rule, which is an admin's). Live:
 * Acknowledge, Snooze and, for an admin, Suppress. Each opens a small inline
 * panel with an optional note and the choices, rather than a dialog.
 */
function triageControls(f) {
  const me = state.me || {};
  const t = f.triage;
  const which = `${codeWords(f.code)} on ${f.hostname || f.machine_key}`;
  if (t) {
    const undo = t.state === 'suppressed'
      ? me.may_suppress && el('button', {
        type: 'button', class: 'btn btn-quiet', text: 'Remove rule',
        'aria-label': `Remove the rule suppressing ${which}`,
        onclick: () => sendTriage(`/api/suppressions/${t.rule}`, {
          method: 'DELETE', done: `Removed the rule suppressing ${which}.`,
        }),
      })
      : me.may_triage && el('button', {
        type: 'button', class: 'btn btn-quiet', text: 'Undo',
        'aria-label': `Undo: make ${which} live again`,
        onclick: () => sendTriage('/api/triage', {
          body: { machine_key: f.machine_key, code: f.code, action: 'clear' },
          done: `${codeWords(f.code)} on ${f.hostname || f.machine_key} is live again.`,
        }),
      });
    return el('div', { class: 'triage triage-quiet' }, [
      el('span', { class: 'triage-state', text: triageWords(t) }),
      undo || null,
    ]);
  }
  if (!me.may_triage) return null;

  // The panel each action opens: the note first, so it is there to fill in
  // before the choice that sends it.
  const panel = el('div', { class: 'triage-panel', role: 'group' });
  panel.hidden = true;
  const open = (label, choices) => {
    const input = el('input', {
      type: 'text', class: 'triage-note', maxlength: '500',
      placeholder: 'Note (optional)', 'aria-label': `Note for ${which}`,
    });
    panel.setAttribute('aria-label', `${label} ${which}`);
    panel.replaceChildren(
      input,
      ...choices.map(([text, send]) => el('button', {
        type: 'button', class: 'btn', text,
        onclick: () => send(input.value.trim() || undefined),
      })),
      el('button', {
        type: 'button', class: 'btn btn-quiet', text: 'Cancel',
        onclick: () => { panel.hidden = true; panel.replaceChildren(); },
      }),
    );
    panel.hidden = false;
  };
  const mark = (action, until, label) => (note) => sendTriage('/api/triage', {
    body: { machine_key: f.machine_key, code: f.code, action, until, note },
    done: `${label} ${which}.`,
  });
  const actions = [
    el('button', {
      type: 'button', class: 'btn btn-quiet', text: 'Acknowledge',
      'aria-label': `Acknowledge ${which}`,
      onclick: () => open('Acknowledge', [['Acknowledge', mark('acknowledge', undefined, 'Acknowledged')]]),
    }),
    el('button', {
      type: 'button', class: 'btn btn-quiet', text: 'Snooze',
      'aria-label': `Snooze ${which}`,
      onclick: () => open('Snooze', SNOOZES.map(([text, days]) => [
        text, mark('snooze', daysFromNow(days), `Snoozed for ${text}:`),
      ])),
    }),
    me.may_suppress ? el('button', {
      type: 'button', class: 'btn btn-quiet', text: 'Suppress',
      'aria-label': `Suppress ${codeWords(f.code)}`,
      onclick: () => open('Suppress', [
        ['This machine', (note) => sendTriage('/api/suppressions', {
          body: { code: f.code, machine_key: f.machine_key, note },
          done: `Suppressed ${which}.`,
        })],
        ['Fleet-wide', (note) => sendTriage('/api/suppressions', {
          body: { code: f.code, note },
          done: `Suppressed ${codeWords(f.code)} across the fleet.`,
        })],
      ]),
    }) : null,
  ];
  return el('div', { class: 'triage' }, [el('div', { class: 'triage-actions' }, actions), panel]);
}

/** Quieted-but-visible findings are drawn quieter: in hand, not news. */
const findingClass = (f) => (f.triage ? 'finding finding-quieted' : 'finding');

function findingRow(f, machinesByKey) {
  const m = machinesByKey.get(f.machine_key);
  return el('div', { class: 'finding-list' }, [el('div', { class: findingClass(f) }, [
    el('div', {}, [
      el('div', { class: 'finding-head' }, [
        severityChip(f.severity),
        m ? machineLink(m) : el('span', { class: 'finding-who', text: f.hostname || f.machine_key }),
        el('span', { class: 'finding-who', text: f.headline }),
        el('span', { class: 'finding-queue', text: QUEUES[f.kind] || f.kind }),
      ]),
      el('p', { class: 'finding-detail', text: f.detail }),
      triageControls(f),
    ]),
  ])]);
}

/** "3 acknowledged · 1 snoozed · 12 suppressed", or null when none are. */
function quietedWords(q) {
  if (!q) return null;
  const parts = [['acknowledged', q.acknowledged], ['snoozed', q.snoozed], ['suppressed', q.suppressed]]
    .filter(([, n]) => n > 0).map(([w, n]) => `${n} ${w}`);
  return parts.length ? parts.join(' · ') : null;
}

/* ------------------------------------------------------------------- views */

export async function renderOverview(main) {
  const s = state.snap.summary;
  const machinesByKey = new Map(state.snap.machines.map((m) => [m.key, m]));

  const hero = el('div', { class: 'card col-4' }, [el('div', { class: 'hero' }, [
    el('div', { class: 'hero-value', text: nf0.format(s.machines_flagged) }),
    el('div', { class: 'hero-label', text: s.machines_flagged === 1 ? 'machine needs attention' : 'machines need attention' }),
    el('div', {
      class: 'hero-sub',
      text: s.machines_flagged === 0
        ? `Nothing at warning or above across ${nf0.format(s.machines)} machine(s) in view.`
        : `of ${nf0.format(s.machines)} in view · ${s.critical} critical, ${s.warnings} warning`,
    }),
    // Said beside the headline, because a number that went down when nobody
    // fixed anything should say why.
    quietedWords(s.quieted)
      ? el('div', { class: 'hero-sub', text: `Not counted: ${quietedWords(s.quieted)}` })
      : null,
  ])]);

  const tile = (label, value, sub) => el('div', { class: 'card' }, [
    el('div', { class: 'tile-label', text: label }),
    el('div', { class: 'tile-value', text: value }),
    el('div', { class: 'tile-sub', text: sub || '' }),
  ]);

  const tiles = el('div', { class: 'col-8' }, [el('div', { class: 'tiles' }, [
    tile('Median score', s.median_score === null ? '—' : nf0.format(s.median_score),
      s.p10_score === null ? '' : `p10 ${nf0.format(s.p10_score)} – p90 ${nf0.format(s.p90_score)}`),
    tile('Runs indexed', nf0.format(s.runs),
      s.newest_run ? `latest ${day(s.newest_run)}` : ''),
    tile('Measurement caveats', nf0.format(s.thermally_limited + s.partial),
      `${s.thermally_limited} throttled, ${s.partial} partial`),
    tile('Stale', nf0.format(s.stale), `not measured in ${nf0.format(state.snap.thresholds.stale_days)} days`
      + (overriddenCount(state.snap, 'stale_days')
        ? ` — ${nf0.format(overriddenCount(state.snap, 'stale_days'))} machine(s) on a line of their own` : '')),
    tile('Weak identity', nf0.format(s.weak_identity), 'identifier resets on reimage'),
    s.lifecycle ? tile('To replace', nf0.format(s.lifecycle.replace),
      `${s.lifecycle.repair} to repair, ${s.lifecycle.investigate} to investigate · see Lifecycle`) : null,
  ])]);

  const grades = s.grades.filter((g) => g[0] !== 'unrecognised' || g[1] > 0);
  const gradeCard = chartCard({
    title: 'Machines by grade',
    subtitle: 'Against the reference baseline — an absolute answer, so it reflects the age of the hardware as much as its condition.',
    span: 'col-6',
    draw: (host) => columnChart(host, {
      rows: grades.map(([g, n]) => ({ label: g, value: n })),
      unit: 'machines',
    }),
    table: () => dataTable({
      columns: [{ label: 'Grade' }, { label: 'Machines', num: true }],
      rows: grades.map(([g, n]) => ({ cells: [g, n] })),
    }),
  });

  const byQueue = Object.keys(QUEUES).map((k) => ({
    label: QUEUES[k],
    values: Object.fromEntries(SEVERITIES.map((sev) => [
      // Live findings, as the headline counts them.
      sev.key, state.snap.flags.filter((f) => !f.triage && f.kind === k && f.severity === sev.key).length,
    ])),
  }));
  const findingsCard = chartCard({
    title: 'Findings by queue',
    subtitle: 'Split by what you would do about them: fix the machine, fix the collection, or go and collect.',
    span: 'col-6',
    draw: (host) => {
      host.append(legend(SEVERITIES.map((sev) => ({ label: sev.label, colour: ink(`--status-${sev.key}`) }))));
      stackedChart(host, {
        rows: byQueue,
        series: SEVERITIES.map((sev) => ({ key: sev.key, label: sev.label, colour: ink(`--status-${sev.key}`) })),
      });
    },
    table: () => dataTable({
      columns: [{ label: 'Queue' }, ...SEVERITIES.map((sev) => ({ label: sev.label, num: true }))],
      rows: byQueue.map((r) => ({ cells: [r.label, ...SEVERITIES.map((sev) => r.values[sev.key])] })),
    }),
  });

  const scored = state.snap.machines.filter((m) => m.score !== null);
  const bandWidth = 200;
  const top = Math.max(bandWidth, ...scored.map((m) => m.score));
  const bands = [];
  for (let lo = 0; lo < top; lo += bandWidth) {
    bands.push({
      label: nf0.format(lo),
      sublabel: `–${nf0.format(lo + bandWidth - 1)}`,
      value: scored.filter((m) => m.score >= lo && m.score < lo + bandWidth).length,
      lo,
    });
  }
  const spreadCard = chartCard({
    title: 'Score distribution',
    subtitle: 'How the estate is spread, not how it compares. Ranking across configurations is what the cohort view is for.',
    span: 'col-6',
    draw: (host) => columnChart(host, { rows: bands, unit: 'machines' }),
    table: () => dataTable({
      columns: [{ label: 'Score band' }, { label: 'Machines', num: true }],
      rows: bands.map((b) => ({ cells: [`${b.label}${b.sublabel}`, b.value] })),
    }),
  });

  // The queue: live findings, and acknowledged ones drawn quieter after them;
  // snoozed and suppressed ones only when asked for. Live first, keeping the
  // server's most-severe-first order within each.
  const q = s.quieted || { acknowledged: 0, snoozed: 0, suppressed: 0 };
  const hiddenQuiet = q.snoozed + q.suppressed;
  const inQueue = (f) => !f.triage || f.triage.state === 'acknowledged' || state.showQuiet;
  const shown = state.snap.flags
    .filter((f) => (state.showInfo || f.severity !== 'info') && inQueue(f))
    .sort((a, b) => Number(!!a.triage) - Number(!!b.triage));
  const quieted = quietedWords(q);
  const list = el('div', { class: 'card col-6' }, [
    el('div', { class: 'card-head' }, [
      el('h3', { text: 'What to look at' }),
      el('div', { class: 'card-actions' }, [
        el('button', {
          type: 'button', class: 'btn btn-quiet',
          text: state.showInfo ? 'Hide informational' : `Show ${s.info} informational`,
          onclick: () => { state.showInfo = !state.showInfo; render(); },
        }),
        hiddenQuiet ? el('button', {
          type: 'button', class: 'btn btn-quiet',
          text: state.showQuiet ? 'Hide snoozed and suppressed' : `Show ${hiddenQuiet} snoozed or suppressed`,
          'aria-pressed': state.showQuiet ? 'true' : 'false',
          onclick: () => { state.showQuiet = !state.showQuiet; render(); },
        }) : null,
        csvLink('findings'),
      ]),
    ]),
    el('p', {
      class: 'card-sub',
      text: 'Most severe first. Every finding says what it is evidence of and what usually causes it.'
        + (quieted ? ` Quieted: ${quieted}.` : '')
        // Said, because the file and the list differ on purpose: a
        // spreadsheet can filter a row out, and cannot filter one back in.
        + ' The CSV holds every finding in view — informational, snoozed and suppressed ones too, each with '
        + 'its state — whatever this list is showing.',
    }),
    shown.length
      ? el('div', {}, shown.slice(0, 14).map((f) => findingRow(f, machinesByKey)))
      : el('div', { class: 'empty', text: 'Nothing to report in this view.' }),
    shown.length > 14
      ? el('p', { class: 'card-note', text: `${shown.length - 14} more — filter by finding above to work through them.` })
      : null,
  ]);

  const configs = s.configurations.length > 1
    ? el('div', { class: 'card col-12' }, [
      el('div', { class: 'card-head' }, [el('h3', { text: 'Configurations in view' })]),
      el('p', {
        class: 'card-sub',
        text: 'Scores are only comparable within one of these. Machines measured differently are ranked separately, never pooled.',
      }),
      dataTable({
        columns: [{ label: 'Preset · profile · baseline · instruction set' }, { label: 'Machines', num: true }],
        rows: s.configurations.map(([label, n]) => ({ cells: [label, n] })),
      }),
    ])
    : null;

  // The two history cards are placeholders until their data arrives: the
  // rest of the overview comes out of the snapshot already in hand, and need
  // not wait for them — nor disappear if they fail.
  const changesSlot = el('div', { class: 'card col-12', dataset: { card: 'changes' } }, [
    el('div', { class: 'card-head' }, [el('h3', { text: 'What changed' })]),
    el('p', { class: 'card-sub', text: 'Loading…' }),
  ]);
  const overTimeSlot = el('div', { class: 'grid col-12', dataset: { card: 'over-time' } });

  const actions = el('div', { class: 'view-actions' }, [
    el('a', { class: 'btn', href: reportHash(), text: 'Print report' }),
  ]);

  main.append(actions, el('div', { class: 'grid' }, [
    hero, tiles, changesSlot, gradeCard, findingsCard, spreadCard, list, configs, overTimeSlot,
  ]));

  const [changes, timeline] = await Promise.all([
    fetchChanges().catch((err) => err),
    fetchTimeline().catch((err) => err),
  ]);
  changesSlot.replaceChildren(...changesCard(changes, machinesByKey));
  overTimeSlot.replaceChildren(...overTimeCards(timeline));
}

/* --------------------------------------------------------------- over time */

/** The comparisons on offer, as the server accepts them. */
const WINDOWS = [[1, '1 week'], [4, '4 weeks'], [13, '13 weeks']];

/** How the past is worked out, said the way an estate owner would want it. */
const AS_MEASURED = 'as the fleet was last measured at each date, judged by today’s rules';

/** "Nov 4" from a `YYYY-MM-DD`. A bare date parses as UTC midnight, which is
 *  the previous evening west of Greenwich, so it is read and shown as UTC. */
function calendarDay(ymd) {
  const d = new Date(`${ymd}T00:00:00Z`);
  if (Number.isNaN(+d)) return ymd;
  return d.toLocaleDateString(undefined, { month: 'short', day: 'numeric', timeZone: 'UTC' });
}

/** Where a timeline point sits on a time axis: midday, so no time zone moves
 *  it onto the neighbouring day's label. */
const midday = (p) => `${p.at}T12:00:00Z`;

/** A list that shows its first few rows and offers the rest. */
function collapsible(rows, limit = 8) {
  const list = el('div', { class: 'finding-list' }, rows.slice(0, limit));
  if (rows.length <= limit) return list;
  const more = el('button', { type: 'button', class: 'btn btn-quiet', text: `Show all ${rows.length}` });
  let open = false;
  more.addEventListener('click', () => {
    open = !open;
    list.replaceChildren(...(open ? rows : rows.slice(0, limit)));
    more.textContent = open ? 'Show fewer' : `Show all ${rows.length}`;
  });
  return el('div', {}, [list, more]);
}

/** One line of a change list, shaped like a finding without its detail. */
function changeRow(lead, who, what, note, machinesByKey) {
  const m = machinesByKey.get(who.machine_key);
  return el('div', { class: 'finding' }, [el('div', {}, [
    el('div', { class: 'finding-head' }, [
      lead,
      m ? machineLink(m) : el('span', { class: 'finding-who', text: who.hostname || who.machine_key }),
      el('span', { class: 'finding-who', text: what }),
      note ? el('span', { class: 'finding-queue', text: note }) : null,
    ]),
  ])]);
}

const severityLabel = (sev) => SEVERITIES.find((s) => s.key === sev)?.label || sev;

/**
 * "What changed" between now and the chosen window back. The server has done
 * the comparing; this lays out the headline and the lists, news first: new and
 * worse findings, then machines added, grades, and what went away.
 */
export function changesCard(c, machinesByKey, { picker: withPicker = true, list = collapsible } = {}) {
  // The printed report lays this card out too, with its own period chosen
  // elsewhere and every list capped rather than collapsed: hence the options.
  const picker = withPicker ? el('select', {
    'aria-label': 'Compare with',
    onchange: (e) => { state.window = Number(e.target.value); writeHash({ view: 'overview' }); },
  }, WINDOWS.map(([w, label]) => el('option', { value: String(w), text: `${label} ago` }))) : null;
  if (picker) picker.value = String(state.window);
  const head = el('div', { class: 'card-head' }, [
    el('h3', { text: 'What changed' }),
    picker ? el('div', { class: 'card-actions' }, [el('label', { class: 'field' }, [picker])]) : null,
  ]);

  if (c instanceof Error) {
    return [head, el('div', { class: 'banner' }, [
      el('span', { text: c.message }),
      ' ',
      el('button', {
        type: 'button', class: 'link-button', text: 'Compare with a week ago instead',
        onclick: () => { state.window = 1; writeHash({ view: 'overview', window: 1 }); },
      }),
    ])];
  }

  const weeks = WINDOWS.find(([w]) => w === c.window_weeks)?.[1] || `${c.window_weeks} weeks`;
  const sub = el('p', {
    class: 'card-sub',
    text: `${calendarDay(c.from)} to ${calendarDay(c.to)}, ${AS_MEASURED}. A result collected late `
      + 'counts from when it was measured, so a past week can still move.',
  });

  if (c.baseline_empty) {
    const oldest = state.snap.summary.oldest_run;
    return [head, sub, el('div', {
      class: 'empty',
      text: `Nothing in view had been measured ${weeks} ago, so there is nothing to compare against`
        + (oldest ? ` — the earliest run is from ${day(oldest)}. Try a shorter window.` : '.'),
    })];
  }
  const lists = ['new_findings', 'worsened', 'machines_added', 'grade_changes', 'resolved', 'eased'];
  if (lists.every((k) => !c[k].length)) {
    return [head, sub, el('div', {
      class: 'empty', text: `Nothing changed between ${calendarDay(c.from)} and ${calendarDay(c.to)}.`,
    })];
  }

  // Critical and warning lead the figures; informational changes are counted
  // beside them rather than in them, because a dozen machines going stale is
  // real but is not the same news as one drive failing.
  const n = c.counts;
  const serious = (s) => s.critical + s.warning;
  const split = (s) => [`${s.critical} critical · ${s.warning} warning`, s.info ? `${s.info} info` : null]
    .filter(Boolean).join(' · ');
  const figure = (label, value, subtext) => el('div', {}, [
    el('div', { class: 'tile-label', text: label }),
    el('div', { class: 'tile-value', text: nf0.format(value) }),
    el('div', { class: 'tile-sub', text: subtext }),
  ]);
  const figures = el('div', { class: 'tiles' }, [
    figure('New findings', serious(n.new_findings), split(n.new_findings)),
    figure('Worse', n.worsened, `${n.eased} eased`),
    figure('Resolved', serious(n.resolved), split(n.resolved)),
    figure('Machines added', n.machines_added, `${n.machines_added_flagged} need attention`),
    figure('Grades down', n.grades_down, `${n.grades_up} up`),
  ]);

  const queue = (k) => QUEUES[k] || k;
  const finding = (f) => changeRow(severityChip(f.severity), f, f.headline, queue(f.kind), machinesByKey);
  const moved = (f) => changeRow(severityChip(f.to), f, f.headline,
    `was ${severityLabel(f.from).toLowerCase()} · ${queue(f.kind)}`, machinesByKey);
  const sections = [
    ['New findings', c.new_findings.filter((f) => !f.new_machine).map(finding)],
    ['Worse', c.worsened.map(moved)],
    ['Machines added', c.machines_added.map((m) => changeRow(
      el('span', { class: 'chip' }, [el('span', { text: 'new' })]), m, `graded ${m.grade || '—'}`,
      m.flagged ? 'needs attention' : null, machinesByKey,
    ))],
    ['Findings on machines added', c.new_findings.filter((f) => f.new_machine).map(finding)],
    ['Grades', c.grade_changes.map((g) => changeRow(
      el('span', {
        class: `chip grade ${g.direction === 'down' ? 'delta-down' : g.direction === 'up' ? 'delta-up' : ''}`.trim(),
        text: `${g.from || '—'} → ${g.to || '—'}`,
      }),
      g, g.direction === 'down' ? 'graded lower' : g.direction === 'up' ? 'graded higher' : 'grade changed',
      null, machinesByKey,
    ))],
    ['Resolved', c.resolved.map(finding)],
    ['Eased', c.eased.map(moved)],
  ].filter(([, rows]) => rows.length);

  return [head, sub, figures, ...sections.flatMap(([title, rows]) => [
    el('p', { class: 'card-sub change-section', text: `${title} · ${rows.length}` }),
    list(rows),
  ])];
}

/** The fleet's figures week by week: a short explanation, then the charts. */
export function overTimeCards(tl) {
  const intro = (body) => el('div', { class: 'card col-12' }, [
    el('div', { class: 'card-head' }, [el('h3', { text: 'Over time' })]),
    el('p', {
      class: 'card-sub',
      text: `Week by week, ${AS_MEASURED}: each machine counts from its latest run by that date, under the `
        + 'tags it carries now. A result collected late moves the week it was measured in.',
    }),
    body,
  ]);
  if (tl instanceof Error) {
    return [intro(el('div', { class: 'banner' }, [el('span', { text: tl.message })]))];
  }
  const points = tl.points;
  // A line through two points claims a trend that two points cannot show.
  if (points.length < 3) {
    return [intro(el('div', {
      class: 'empty',
      text: points.length
        ? `${points.length === 1 ? 'One week' : 'Two weeks'} of data in view so far. The charts start at three, `
          + 'because a line through fewer claims a trend that isn’t there.'
        : 'No data in view yet.',
    }))];
  }

  const newestFirst = points.slice().reverse();
  const readout = (p, series) => tipBlock(`Week of ${calendarDay(p.at)}`, [
    ...series.map((s) => [s.label, num(s.value(p), 0), s.colour]),
    [`of ${nf0.format(p.machines)} machine(s) in view`, ''],
  ]);
  const line = (host, series) => lineChart(host, { points, series, time: midday, readout });
  const keyed = (series) => legend(series.map((s) => ({ label: s.label, colour: s.colour, line: true })));

  const attentionSeries = () => [
    { label: 'Need attention', colour: ink('--series-1'), value: (p) => p.machines_flagged },
    { label: 'Critical', colour: ink('--status-critical'), value: (p) => p.machines_critical },
  ];
  const attention = chartCard({
    title: 'Machines needing attention',
    subtitle: 'With a critical or warning finding, and how many of those are critical.',
    span: 'col-4',
    draw: (host) => {
      const series = attentionSeries();
      host.append(keyed(series));
      line(host, series);
    },
    table: () => dataTable({
      columns: [{ label: 'Week of' }, { label: 'Machines', num: true }, { label: 'Need attention', num: true },
        { label: 'Critical', num: true }, { label: 'Stale', num: true }],
      rows: newestFirst.map((p) => ({
        cells: [calendarDay(p.at), p.machines, p.machines_flagged, p.machines_critical, p.stale],
      })),
    }),
  });

  const median = chartCard({
    title: 'Median score',
    subtitle: 'Moves when the machines change and when the mix of machines does — a refresh lifts it.',
    span: 'col-4',
    draw: (host) => line(host, [
      { label: 'Median score', colour: ink('--series-1'), value: (p) => p.median_score },
    ]),
    table: () => dataTable({
      columns: [{ label: 'Week of' }, { label: 'Machines', num: true }, { label: 'Median score', num: true }],
      rows: newestFirst.map((p) => ({ cells: [calendarDay(p.at), p.machines, num(p.median_score, 0)] })),
    }),
  });

  // Split at B, with the roles the Groups view's grade chart uses: six grades
  // in one hue would be six lines nobody could tell apart. The table has all.
  const count = (p, keys) => p.grades.filter(([g]) => keys.includes(g)).reduce((a, [, k]) => a + k, 0);
  const gradeSeries = () => [
    { label: 'S to B', colour: ink('--series-1'), value: (p) => count(p, ['S', 'A', 'B']) },
    { label: 'C to F', colour: ink('--deemph'), value: (p) => count(p, ['C', 'D', 'F']) },
  ];
  const gradeKeys = points[points.length - 1].grades.map(([g]) => g);
  const grades = chartCard({
    title: 'Fleet size and grade mix',
    subtitle: 'Machines graded S to B and C to F against the reference baseline.',
    span: 'col-4',
    draw: (host) => {
      const series = gradeSeries();
      host.append(keyed(series));
      line(host, series);
    },
    table: () => dataTable({
      columns: [{ label: 'Week of' }, { label: 'Machines', num: true },
        ...gradeKeys.map((g) => ({ label: g, num: true }))],
      rows: newestFirst.map((p) => ({
        cells: [calendarDay(p.at), p.machines, ...gradeKeys.map((g) => p.grades.find(([k]) => k === g)?.[1] ?? 0)],
      })),
    }),
  });

  return [intro(null), attention, median, grades];
}

export function renderCohorts(main) {
  const byCohort = new Map();
  for (const m of state.snap.machines) {
    if (!byCohort.has(m.cohort)) byCohort.set(m.cohort, []);
    byCohort.get(m.cohort).push(m);
  }
  const cohorts = state.snap.cohorts.filter((c) => c.in_view > 0);
  const comparable = cohorts.filter((c) => c.comparable);
  const thin = cohorts.filter((c) => !c.comparable);
  const flaggedKeys = new Set(state.snap.flags.filter((f) => f.code === 'cohort_outlier').map((f) => f.machine_key));

  const cards = comparable.map((c) => {
    const members = (byCohort.get(c.id) || [])
      .filter((m) => m.cohort_delta_pct !== null)
      .sort((a, b) => a.cohort_delta_pct - b.cohort_delta_pct);
    return chartCard({
      title: c.cpu_model,
      subtitle: `${c.members} machine(s)${c.in_view === c.members ? '' : `, ${c.in_view} in view`}`
        + ` · median ${nf0.format(c.median)} (p10 ${nf0.format(c.p10)} – p90 ${nf0.format(c.p90)})`
        + ` · ${c.comparability.preset} · ${c.comparability.profile}`
        + ` · ${c.comparability.baseline} · ${c.comparability.build_isa}`,
      note: 'Each bar is that machine against the median of the others like it. The comparison holds the '
        + 'hardware and the measurement configuration constant, so it does not inherit the reference '
        + "baseline's uncertainty.",
      span: 'col-12',
      draw: (host) => divergingChart(host, {
        rows: members.map((m) => ({
          label: m.hostname || m.key,
          value: m.cohort_delta_pct,
          score: m.score,
          key: m.key,
          note: flaggedKeys.has(m.key) ? 'flagged as an outlier' : null,
        })),
        onSelect: (r) => writeHash({ view: 'machine', key: r.key }),
      }),
      table: () => dataTable({
        columns: [{ label: 'Machine' }, { label: 'Score', num: true }, { label: 'vs median', num: true },
          { label: 'Grade' }, { label: 'Flagged' }],
        rows: members.map((m) => ({
          cells: [m.hostname || m.key, num(m.score, 0), signed(m.cohort_delta_pct, 1), m.grade,
            flaggedKeys.has(m.key) ? 'yes' : 'no'],
        })),
        onRow: null,
      }),
    });
  });

  const thinCard = thin.length ? el('div', { class: 'card col-12' }, [
    el('div', { class: 'card-head' }, [el('h3', { text: 'Without a peer group' })]),
    el('p', {
      class: 'card-sub',
      text: `Fewer than ${state.snap.thresholds.min_cohort} machines share this hardware and configuration, so there is no median `
        + 'worth testing a member against and the absolute grade is all there is.',
    }),
    dataTable({
      columns: [{ label: 'CPU' }, { label: 'Configuration' }, { label: 'Machines', num: true }],
      rows: thin.map((c) => ({ cells: [c.cpu_model, `${c.comparability.preset} · ${c.comparability.profile} · ${c.comparability.baseline} · ${c.comparability.build_isa}`, c.members] })),
      sortable: true,
    }),
  ]) : null;

  main.append(
    el('div', { class: 'banner' }, [el('span', {
      text: 'Cohort statistics are always drawn from the whole index, never from the current filter. '
        + "Otherwise a machine's shortfall would change depending on what you happened to be looking at.",
    })]),
    el('div', { class: 'grid' }, [
      ...(cards.length ? cards : [el('div', { class: 'card col-12' }, [
        el('div', { class: 'empty', text: 'No cohort in view has enough members to compare against.' }),
      ])]),
      thinCard,
    ]),
  );
}

/** The most groups a chart draws. Past this the bars are too many to read and
 *  the long tail is the table's job. */
const GROUP_CHART_CAP = 12;

/** "System model", "CPU", "OS", "Tag: site". */
function dimensionLabel(by) {
  if (by.startsWith('tag:')) return `Tag: ${by.slice(4)}`;
  return { model: 'System model', cpu: 'CPU', os: 'OS' }[by] || by;
}

function groupLabel(g, by) {
  if (g.value !== null) return g.value;
  return by.startsWith('tag:') ? '(untagged)' : '(not recorded)';
}

/**
 * The filter that selects one group's machines, or null for the group with no
 * value — there is no "has no system model" filter to send. Exact fields for
 * model, CPU and OS rather than the search box, which would also find the
 * model whose name merely starts the same way.
 */
function groupFilter(g, by) {
  if (g.value === null) return null;
  if (by.startsWith('tag:')) {
    const tags = new Map((state.filter.tags || '').split(',').filter(Boolean)
      .map((p) => p.split('=')).filter((p) => p.length === 2));
    tags.set(by.slice(4), g.value);
    return { tags: [...tags.entries()].map(([k, v]) => `${k}=${v}`).join(',') };
  }
  return { [by]: g.value };
}

function drillInto(g, by) {
  const patch = groupFilter(g, by);
  if (!patch) return;
  writeHash({ view: 'machines', key: null, filter: { ...state.filter, ...patch } });
}

/** A group's replace or repair count, which opens those machines when there
 *  are any and the group is one a filter can select — "which site needs
 *  budget" answered, and then shown. */
function decidedIn(g, by, verdict) {
  const n = g[verdict];
  if (n === undefined || n === null) return null;
  const patch = groupFilter(g, by);
  if (!n || !patch) return n;
  return el('button', {
    type: 'button', class: 'link-button', text: nf0.format(n),
    'aria-label': `${nf0.format(n)} to ${verdict} in ${groupLabel(g, by)}: show them`,
    onclick: (e) => {
      e.stopPropagation();
      writeHash({ view: 'machines', key: null, filter: { ...state.filter, ...patch, lifecycle: verdict } });
    },
  });
}

/** "-7.5% · 6 machines, thin", the headline figure as the table states it. */
function vsIdentical(g) {
  if (g.median_cohort_delta_pct === null) return 'no peer group';
  return `${signed(g.median_cohort_delta_pct, 1)} · ${g.with_cohort} with peers${g.thin ? ', thin' : ''}`;
}

/** Share of a group, 0–100, never NaN for an empty one. */
function share(n, of) {
  return of > 0 ? (n / of) * 100 : 0;
}

const pct = (v) => `${Math.round(v)}%`;

export async function renderGroups(main) {
  let data;
  try {
    data = await fetchGroups();
  } catch (err) {
    // An unknown dimension is the likeliest cause — a hand-edited or stale
    // link — so the way back is offered with the server's reason.
    main.append(el('div', { class: 'banner' }, [
      el('span', { text: err.message }),
      ' ',
      el('button', {
        type: 'button', class: 'link-button', text: 'Group by the default instead',
        onclick: () => { state.by = null; writeHash({ view: 'groups', by: null }); },
      }),
    ]));
    return;
  }
  const { by, groups, thresholds } = data;
  const minCohort = thresholds.min_cohort;
  const truncated = groups.length > GROUP_CHART_CAP
    ? `The first ${GROUP_CHART_CAP} of ${groups.length} groups, worst share flagged first; the table has every one.`
    : null;

  // The selector offers what the server says the machines in view carry, plus
  // whatever is selected now — a tag nobody in view carries is still a
  // grouping, and its single "(untagged)" group is an honest answer.
  const dims = data.dimensions.includes(by) ? data.dimensions : [...data.dimensions, by];
  const picker = el('select', {
    'aria-label': 'Group by',
    onchange: (e) => { state.by = e.target.value; writeHash({ view: 'groups', by: e.target.value }); },
  }, dims.map((d) => el('option', { value: d, text: dimensionLabel(d) })));
  picker.value = by;

  const intro = el('div', { class: 'card col-12' }, [
    el('div', { class: 'card-head' }, [el('h3', { text: `${nf0.format(data.machines)} machine(s) in ${groups.length} group(s)` })]),
    el('p', {
      class: 'card-sub',
      text: 'Raw score by site mostly measures how old each site’s hardware is. So groups are compared by '
        + 'the median of their members against identical machines — the same CPU, measured the same way — '
        + 'and a group well below zero points at something about the group: power policy, the image, dust, a '
        + 'bad batch. Score and grade are shown too, as description; they are not the comparison.',
    }),
    el('label', { class: 'field filter-by' }, [el('span', { class: 'field-label', text: 'Group by' }), picker]),
  ]);

  // Thin groups and groups with nothing to compare are left out of the chart
  // rather than drawn in a colour of their own: the palette has no role for
  // "a median of two", and a bar of the same length as a solid one reads as
  // just as solid whatever its colour. The note says how many, and why.
  const comparable = groups.filter((g) => g.median_cohort_delta_pct !== null && !g.thin);
  const thinCount = groups.filter((g) => g.with_cohort > 0 && g.thin).length;
  const noneCount = groups.filter((g) => g.with_cohort === 0).length;
  const plotted = comparable.slice(0, GROUP_CHART_CAP)
    .sort((a, b) => a.median_cohort_delta_pct - b.median_cohort_delta_pct);
  const leftOut = [
    thinCount ? `${thinCount} group(s) left out because fewer than ${minCohort} of their machines have a peer `
      + 'group — a median of so few describes those machines, not the group.' : null,
    noneCount ? `${noneCount} group(s) have no machine with a peer group at all, so there is nothing to compare.` : null,
    comparable.length > GROUP_CHART_CAP
      ? `The first ${GROUP_CHART_CAP} of ${comparable.length} are drawn; the table has every one.` : null,
  ].filter(Boolean).join(' ');

  const identicalCard = chartCard({
    title: 'Against identical machines',
    subtitle: 'Each bar is the median of a group’s machines, each measured against the median of the machines '
      + 'like it across the whole index. Below zero, this group’s machines run slower than the same hardware '
      + 'elsewhere. Select a bar to see its machines.',
    note: leftOut || null,
    span: 'col-12',
    draw: (host) => {
      if (!plotted.length) {
        host.append(el('div', {
          class: 'empty',
          text: `No group in view has ${minCohort} or more machines with a peer group to compare.`,
        }));
        return;
      }
      divergingChart(host, {
        rows: plotted.map((g) => ({
          label: groupLabel(g, by),
          value: g.median_cohort_delta_pct,
          score: g.median_score,
          note: `median of ${g.with_cohort} machine(s) with peers`,
          group: g,
        })),
        onSelect: (r) => drillInto(r.group, by),
      });
    },
    table: () => dataTable({
      columns: [{ label: 'Group' }, { label: 'Machines', num: true }, { label: 'With peers', num: true },
        { label: 'vs identical', num: true }, { label: 'Thin' }],
      rows: groups.map((g) => ({
        cells: [groupLabel(g, by), g.machines, g.with_cohort, signed(g.median_cohort_delta_pct, 1),
          g.with_cohort === 0 ? '—' : g.thin ? 'yes' : 'no'],
        sort: [groupLabel(g, by), g.machines, g.with_cohort, g.median_cohort_delta_pct, g.thin],
      })),
      sortable: true,
    }),
  });

  // Shares rather than counts: group sizes differ by orders of magnitude, and
  // a count bar ranks the biggest group worst for being big. The count each
  // share stands for is in the tooltip and the table.
  const charted = groups.slice(0, GROUP_CHART_CAP);
  const attention = charted.map((g) => ({
    label: groupLabel(g, by),
    group: g,
    values: {
      critical: share(g.machines_critical, g.machines),
      warning: share(g.flagged - g.machines_critical, g.machines),
    },
  }));
  const attentionSeries = [
    { key: 'critical', label: 'Critical' },
    { key: 'warning', label: 'Warning, nothing critical' },
  ];
  const attentionCard = chartCard({
    title: 'Machines needing attention',
    subtitle: 'The share of each group’s machines with a critical or warning finding, by the worst one each carries.',
    note: truncated,
    span: 'col-6',
    draw: (host) => {
      host.append(legend(attentionSeries.map((s) => ({ label: s.label, colour: ink(`--status-${s.key}`) }))));
      stackedChart(host, {
        rows: attention,
        series: attentionSeries.map((s) => ({ ...s, colour: ink(`--status-${s.key}`) })),
        max: 100,
        format: pct,
        tip: (r, s, v) => {
          const n = s.key === 'critical' ? r.group.machines_critical : r.group.flagged - r.group.machines_critical;
          return [[`of ${nf0.format(r.group.machines)} machine(s)`, `${nf0.format(n)} · ${pct(v)}`, s.colour]];
        },
      });
    },
    table: () => dataTable({
      columns: [{ label: 'Group' }, { label: 'Machines', num: true }, { label: 'Flagged', num: true },
        { label: 'Share', num: true }, { label: 'Critical', num: true }],
      rows: groups.map((g) => ({
        cells: [groupLabel(g, by), g.machines, g.flagged, pct(share(g.flagged, g.machines)), g.machines_critical],
        sort: [groupLabel(g, by), g.machines, g.flagged, share(g.flagged, g.machines), g.machines_critical],
      })),
      sortable: true,
    }),
  });

  // Grades share one hue across the dashboard — six steps cannot clear the
  // ordinal lightness gate (see app.css) — so six segments in it would be six
  // indistinguishable blocks. The chart splits the mix in two with roles that
  // already exist, and the table carries every grade.
  const gradeKeys = groups[0]?.grades.map(([g]) => g) || [];
  const upper = ['S', 'A', 'B'];
  const lower = ['C', 'D', 'F'];
  const count = (g, keys) => g.grades.filter(([k]) => keys.includes(k)).reduce((a, [, n]) => a + n, 0);
  const gradeRows = charted.map((g) => ({
    label: groupLabel(g, by),
    group: g,
    values: { upper: share(count(g, upper), g.machines), lower: share(count(g, lower), g.machines) },
  }));
  // Resolved when drawn, not now: marks carry hex, and a theme change redraws.
  const gradeSeries = () => [
    { key: 'upper', label: 'S to B', colour: ink('--series-1'), keys: upper },
    { key: 'lower', label: 'C to F', colour: ink('--deemph'), keys: lower },
  ];
  const gradesCard = chartCard({
    title: 'Grades',
    subtitle: 'Against the reference baseline, so this says how new a group’s hardware is as much as how it is '
      + 'doing. Split at B here; the table has every grade.',
    note: truncated,
    span: 'col-6',
    draw: (host) => {
      const series = gradeSeries();
      host.append(legend(series.map((s) => ({ label: s.label, colour: s.colour }))));
      stackedChart(host, {
        rows: gradeRows,
        series,
        max: 100,
        format: pct,
        tip: (r, s, v) => [[`of ${nf0.format(r.group.machines)} machine(s)`,
          `${nf0.format(count(r.group, s.keys))} · ${pct(v)}`, s.colour]],
      });
    },
    table: () => dataTable({
      columns: [{ label: 'Group' }, { label: 'Machines', num: true },
        ...gradeKeys.map((k) => ({ label: k, num: true }))],
      rows: groups.map((g) => ({
        cells: [groupLabel(g, by), g.machines, ...g.grades.map(([, n]) => n)],
      })),
      sortable: true,
    }),
  });

  const tableCard = el('div', { class: 'card col-12' }, [
    el('div', { class: 'card-head' }, [el('h3', { text: `Every group by ${dimensionLabel(by).toLowerCase()}` })]),
    el('p', {
      class: 'card-sub',
      text: 'Flagged and Critical count machines, not findings. A median marked thin comes from fewer than '
        + `${minCohort} machines with a peer group. Click a column to sort, or a group to see its machines.`,
    }),
    dataTable({
      columns: [{ label: 'Group' }, { label: 'Machines', num: true }, { label: 'Flagged', num: true },
        { label: 'Critical', num: true }, { label: 'Replace', num: true }, { label: 'Repair', num: true },
        { label: 'Median score', num: true },
        { label: 'vs identical', num: true }, { label: 'Trend', num: true }, { label: 'Stale', num: true },
        { label: 'Median age', num: true }, { label: 'Top findings', wrap: true }],
      rows: groups.map((g) => ({
        group: g,
        cells: [
          groupFilter(g, by)
            ? el('button', {
              type: 'button', class: 'link-button', text: groupLabel(g, by),
              onclick: (e) => { e.stopPropagation(); drillInto(g, by); },
            })
            : el('span', { class: 'meta', text: groupLabel(g, by) }),
          g.machines,
          `${g.flagged} (${pct(share(g.flagged, g.machines))})`,
          g.machines_critical,
          decidedIn(g, by, 'replace'),
          decidedIn(g, by, 'repair'),
          num(g.median_score, 0),
          vsIdentical(g),
          g.median_trend_pct === null ? '—' : `${signed(g.median_trend_pct, 1)} · ${g.with_trend}`,
          g.stale,
          g.median_max_age_months === null ? '—' : maxAge(Math.round(g.median_max_age_months)),
          g.top_flags.length
            ? g.top_flags.map((f) => `${f.code.replace(/_/g, ' ')} ${f.machines}`).join(', ')
            : '—',
        ],
        sort: [groupLabel(g, by).toLowerCase(), g.machines, share(g.flagged, g.machines), g.machines_critical,
          g.replace ?? null, g.repair ?? null,
          g.median_score, g.median_cohort_delta_pct, g.median_trend_pct, g.stale, g.median_max_age_months,
          g.top_flags[0]?.machines ?? null],
      })),
      onRow: (r) => drillInto(r.group, by),
      sortable: true,
    }),
  ]);

  main.append(el('div', { class: 'grid' }, groups.length
    ? [intro, identicalCard, attentionCard, gradesCard, tableCard]
    : [intro, el('div', { class: 'card col-12' }, [el('div', { class: 'empty', text: 'No machines in view to group.' })])]));
}

/* -------------------------------------------------------------- components */

/*
 * The fleet by component: how one measure is spread across the machines in
 * view, and every machine against every graded component. Both are the
 * server's to compute — what may share an axis, and what a cell says, are
 * decisions with reasons (see `analytics::components`) — so this lays out what
 * it is given and says what was left out.
 */

/** The distribution and the heatmap, each through the filter, each with the
 *  view's own settings from the hash. */
async function fetchDistribution() {
  const params = new URLSearchParams(state.filter);
  if (state.measure) params.set('measure', state.measure);
  if (state.config) params.set('config', state.config);
  if (state.split) params.set('split', state.split);
  return fetchJson(`/api/distribution?${params}`);
}

async function fetchHeatmap() {
  const params = new URLSearchParams(state.filter);
  if (state.encoding) params.set('encoding', state.encoding);
  if (state.hsort) params.set('sort', state.hsort);
  if (state.hdir) params.set('dir', state.hdir);
  return fetchJson(`/api/heatmap?${params}`);
}

/** Change one of this view's settings, keeping the rest. A new measure or
 *  configuration has new bins, so the chosen bin is let go. */
function setComponents(patch) {
  const next = {
    measure: state.measure, config: state.config, split: state.split, bin: state.bin,
    encoding: state.encoding, hsort: state.hsort, hdir: state.hdir, ...patch,
  };
  if ('measure' in patch || 'config' in patch) next.bin = null;
  if ('measure' in patch) next.config = null;
  Object.assign(state, next);
  writeHash({ view: 'components' });
}

const DIRECTION_WORDS = {
  higher_is_better: 'higher is better',
  lower_is_better: 'lower is better',
};

/*
 * The heatmap's steps against identical machines. Equal steps on each side of
 * a neutral middle, as the diverging rule asks, and placed where the findings
 * already draw lines: within 5% is run-to-run noise, 15% is about where a
 * cohort outlier starts to be named, and 25% is where it becomes critical.
 * The fills are the validated diverging poles mixed into the surface in
 * app.css, not new colours.
 */
const HEAT_STEPS = [
  { cls: 'heat-down-3', label: '25% or more below', test: (d) => d <= -25 },
  { cls: 'heat-down-2', label: '15–25% below', test: (d) => d <= -15 },
  { cls: 'heat-down-1', label: '5–15% below', test: (d) => d <= -5 },
  { cls: 'heat-mid', label: 'within 5%', test: (d) => d < 5 },
  { cls: 'heat-up-1', label: '5–15% above', test: (d) => d < 15 },
  { cls: 'heat-up-2', label: '15–25% above', test: (d) => d < 25 },
  { cls: 'heat-up-3', label: '25% or more above', test: () => true },
];

const heatStep = (d) => HEAT_STEPS.find((s) => s.test(d));

/** One heatmap cell: the figure as text, the step as a class, and a label a
 *  screen reader can read whole. Colour never says anything alone. */
function heatCell({ measured, delta, grade, score, weak, encoding, what }) {
  let text;
  let cls;
  let words;
  if (!measured) {
    text = '—';
    cls = 'heat-absent';
    words = `${what}: not measured in the latest run`;
  } else if (encoding === 'grade') {
    text = `${grade || '—'} ${num(score, 0)}`;
    cls = 'heat-grade';
    words = `${what}: graded ${grade || 'unrecognised'}, scored ${num(score, 0)}`;
  } else if (delta === null || delta === undefined) {
    text = 'no peers';
    cls = 'heat-none';
    words = `${what}: no peer group to compare against`;
  } else {
    text = signed(delta, 0);
    cls = heatStep(delta).cls;
    words = `${what}: ${signed(delta, 1)} against identical machines`;
  }
  if (weak) words += '; weak — this machine carries the weak component finding for it';
  return el('td', {
    class: `heat ${cls}${weak ? ' heat-weak' : ''}`, title: words, 'aria-label': words,
  }, [
    el('span', { text }),
    weak ? el('span', { class: 'heat-weak-mark', text: 'weak' }) : null,
  ]);
}

function heatLegend(encoding) {
  const swatch = (cls, label, mark) => el('span', { class: 'legend-item' }, [
    el('span', { class: `legend-key heat-key ${cls}` }, mark ? [el('span', { text: mark })] : []),
    el('span', { text: label }),
  ]);
  return el('div', { class: 'legend heat-legend' }, [
    ...(encoding === 'grade' ? [] : HEAT_STEPS.map((s) => swatch(s.cls, s.label))),
    encoding === 'grade' ? null : swatch('heat-none', 'no peer group'),
    swatch('heat-absent', 'not measured'),
    swatch('heat-weak', 'weak component', '!'),
  ]);
}

function heatmapCard(h) {
  const encoding = h.encoding;
  const sortBy = (key) => {
    // The same column again turns it round; a new one starts weakest first,
    // or A to Z for the names.
    const dir = h.sort === key && h.dir === 'asc' ? 'desc' : 'asc';
    setComponents({ hsort: key, hdir: dir });
  };
  const header = (key, label) => {
    const on = h.sort === key;
    const th = el('th', {
      class: `sortable${key === 'host' ? '' : ' num'}`, tabindex: '0',
      'aria-sort': on ? (h.dir === 'asc' ? 'ascending' : 'descending') : 'none',
      text: label + (on ? (h.dir === 'asc' ? ' ▲' : ' ▼') : ''),
    });
    th.addEventListener('click', () => sortBy(key));
    th.addEventListener('keydown', (e) => { if (e.key === 'Enter') sortBy(key); });
    return th;
  };
  const table = el('table', { class: 'data heatmap' }, [
    el('thead', {}, [el('tr', {}, [
      header('host', 'Machine'),
      header('overall', encoding === 'grade' ? 'Overall' : 'Overall vs peers'),
      ...h.components.map((c) => header(c, c)),
    ])]),
    el('tbody', {}, h.rows.map((r) => {
      const name = r.hostname || r.key;
      const tr = el('tr', { class: 'clickable', tabindex: '0' }, [
        el('td', {}, [el('span', { class: 'heat-name', text: name })]),
        heatCell({
          measured: encoding === 'grade' ? r.score !== null : true,
          delta: r.cohort_delta_pct, grade: r.grade, score: r.score, weak: false, encoding, what: `${name} overall`,
        }),
        ...r.cells.map((c) => heatCell({
          measured: c.measured, delta: c.cohort_delta_pct, grade: c.grade, score: c.score, weak: c.weak,
          encoding, what: `${name} ${c.component}`,
        })),
      ]);
      const open = () => writeHash({ view: 'machine', key: r.key });
      tr.addEventListener('click', open);
      tr.addEventListener('keydown', (e) => { if (e.key === 'Enter') open(); });
      return tr;
    })),
  ]);

  const capped = h.shown < h.machines
    ? `Showing ${nf0.format(h.shown)} of ${nf0.format(h.machines)} machines — the first ${nf0.format(h.cap)} by the `
      + 'sort you chose. Filter to narrow.'
    : null;
  const toggle = (value, label) => el('button', {
    type: 'button', class: 'btn', 'aria-pressed': encoding === value ? 'true' : 'false', text: label,
    onclick: () => setComponents({ encoding: value === 'cohort' ? null : value }),
  });
  return el('div', { class: 'card col-12', dataset: { card: 'heatmap' } }, [
    el('div', { class: 'card-head' }, [
      el('h3', { text: 'Every machine, by component' }),
      el('div', { class: 'card-actions', role: 'group', 'aria-label': 'What each cell shows' }, [
        toggle('cohort', 'Against identical machines'), toggle('grade', 'Grade'),
      ]),
    ]),
    el('p', {
      class: 'card-sub',
      text: encoding === 'grade'
        ? 'Each cell is the component’s grade and score against the reference baseline — an absolute answer, so it '
          + 'says how new the hardware is as much as how it is doing. Grades are not coloured: they share one hue '
          + 'across the dashboard, and the letter carries the order.'
        : 'Each cell is the component against the median of the same component on machines with the same CPU, '
          + 'measured the same way, across the whole index — the comparison Groups headlines with. “No peers” '
          + `means fewer than ${h.min_cohort} of them measured it: there is nothing to compare with, which is not `
          + 'the same as average.',
    }),
    heatLegend(encoding),
    el('p', {
      class: 'card-sub',
      text: `“Weak” marks the weak component finding — graded D or F and at least two grades below the machine’s `
        + 'own — in either view, so a cell never looks fine on a machine that carries it. A cell well below its '
        + 'peers without the mark is a different comparison: slower than the same part elsewhere, not a weak part '
        + `on this machine. ${nf0.format(h.weak)} weak component(s) in view. Click a column to sort, or a machine `
        + 'to open it.',
    }),
    capped ? el('p', { class: 'card-note heat-capped', text: capped }) : null,
    h.rows.length
      ? el('div', { class: 'table-wrap' }, [table])
      : el('div', { class: 'empty', text: 'No machines in view.' }),
  ]);
}

/** The measure picker: the overall score, then each component and its
 *  subtests grouped under it, each with how many machines in view have it. */
function measurePicker(d) {
  const n = (k) => ` (${nf0.format(k)})`;
  const select = el('select', {
    'aria-label': 'Measure',
    onchange: (e) => setComponents({ measure: e.target.value === 'overall' ? null : e.target.value }),
  }, [
    el('option', { value: 'overall', text: `Overall score${n(d.overall_machines)}` }),
    ...d.measures.map((g) => el('optgroup', { label: `${g.label}${g.graded ? '' : ' — not graded'}` }, [
      el('option', { value: g.measure, text: `${g.label}, the component score${n(g.machines)}` }),
      ...g.subtests.map((t) => el('option', {
        value: t.measure, text: `${t.label} · ${t.unit}${t.scored ? '' : ' · informational'}${n(t.machines)}`,
      })),
    ])),
  ]);
  select.value = d.measure;
  return el('label', { class: 'field components-field' }, [el('span', { class: 'field-label', text: 'Measure' }), select]);
}

function configPicker(d) {
  if (d.configurations.length < 2) return null;
  const select = el('select', {
    'aria-label': 'Configuration',
    onchange: (e) => setComponents({ config: e.target.value }),
  }, d.configurations.map((c) => el('option', {
    value: c.key, text: `${c.label} (${nf0.format(c.machines)})`,
  })));
  select.value = d.config;
  return el('label', { class: 'field components-field' }, [
    el('span', { class: 'field-label', text: 'Configuration' }), select,
  ]);
}

function splitPicker(d) {
  const select = el('select', {
    'aria-label': 'Split by',
    onchange: (e) => setComponents({ split: e.target.value || null }),
  }, [el('option', { value: '', text: 'Nothing' }), ...d.dimensions.map((v) => el('option', { value: v, text: dimensionLabel(v) }))]);
  select.value = d.split ? d.split.by : '';
  return el('label', { class: 'field components-field' }, [el('span', { class: 'field-label', text: 'Split by' }), select]);
}

/** The machines a chosen bin holds, or with none chosen the lowest and the
 *  highest few — so both ends of the estate are a click from their pages. */
function binCard(d, fmt) {
  const chosen = state.bin !== null && state.bin >= 0 && state.bin < d.bins.length ? state.bin : null;
  const row = (p) => ({
    key: p.key,
    cells: [machineLink({ key: p.key, hostname: p.hostname }), fmt(p.value)],
    sort: [(p.hostname || p.key).toLowerCase(), p.value],
  });
  const table = (points) => dataTable({
    columns: [{ label: 'Machine' }, { label: d.unit === 'score' ? 'Score' : d.unit, num: true }],
    rows: points.map(row),
    onRow: (r) => writeHash({ view: 'machine', key: r.key }),
    sortable: true,
  });
  if (chosen !== null) {
    const b = d.bins[chosen];
    const points = d.values.filter((p) => p.bin === chosen);
    return el('div', { class: 'card col-12', dataset: { card: 'bin' } }, [
      el('div', { class: 'card-head' }, [
        el('h3', { text: `${fmt(b.lo)} to ${fmt(b.hi)} · ${nf0.format(points.length)} machine(s)` }),
        el('div', { class: 'card-actions' }, [el('button', {
          type: 'button', class: 'btn btn-quiet', text: 'Show both ends instead',
          onclick: () => setComponents({ bin: null }),
        })]),
      ]),
      points.length ? table(points) : el('div', { class: 'empty', text: 'No machine falls in this bin.' }),
    ]);
  }
  const few = Math.min(5, Math.floor(d.values.length / 2));
  const ends = few > 0
    ? [...d.values.slice(0, few), ...d.values.slice(d.values.length - few)]
    : d.values;
  return el('div', { class: 'card col-12', dataset: { card: 'bin' } }, [
    el('div', { class: 'card-head' }, [el('h3', { text: 'At each end' })]),
    el('p', {
      class: 'card-sub',
      text: d.values.length > ends.length
        ? `The ${few} lowest and the ${few} highest. Choose a bar to list the machines in it.`
        : 'Every machine drawn. Choose a bar to list just the machines in it.',
    }),
    d.values.length ? table(ends) : el('div', { class: 'empty', text: 'Nothing drawn.' }),
  ]);
}

function distributionCards(d) {
  const fmt = d.unit === 'score' ? (v) => nf0.format(v) : (v) => num(v);
  const unitWords = d.unit === 'score' ? 'score against the baseline' : d.unit;
  const direction = DIRECTION_WORDS[d.direction] || null;
  const notes = [
    d.direction_note,
    d.counted ? null : (d.kind === 'subtest'
      ? 'Informational, or part of a component that is not graded: measured and shown, never part of a grade.'
      : 'Not graded: network and GPU describe the host, its drivers and the link as much as the machine, which is '
        + 'why loadbearer keeps them out of a grade.'),
    ...d.left_out.map((l) => `Left out: ${l.detail}`),
  ].filter(Boolean);
  const chart = chartCard({
    title: `${d.label} · ${unitWords}`,
    subtitle: d.config_label
      ? `${nf0.format(d.included)} of ${nf0.format(d.machines)} machine(s) in view, all measured with `
        + `${d.config_label} — numbers are only comparable within one configuration, so others are drawn on their own.`
        + (d.statistic ? ` Each figure is the ${d.statistic} of its iterations.` : '')
        + (direction ? ` For this measure ${direction}.` : '')
      : 'No machine in view has this measure.',
    note: notes.join(' ') || null,
    span: 'col-12',
    draw: (host) => {
      if (!d.bins.length) {
        host.append(el('div', { class: 'empty', text: 'Nothing to draw: no machine in view has this measure.' }));
        return;
      }
      histogramChart(host, {
        bins: d.bins,
        marks: d.included > 1
          ? [{ label: 'p10', value: d.p10 }, { label: 'median', value: d.median }, { label: 'p90', value: d.p90 }]
          : [],
        format: fmt,
        selected: state.bin,
        onSelect: (i) => setComponents({ bin: state.bin === i ? null : i }),
        caption: direction
          ? `${d.unit === 'score' ? 'score' : d.unit} — ${direction}`
          : `${d.unit} — no direction recorded, so neither end is called better`,
      });
    },
    table: () => dataTable({
      columns: [{ label: 'From' }, { label: 'To' }, { label: 'Machines', num: true }],
      rows: d.bins.map((b) => ({ cells: [fmt(b.lo), fmt(b.hi), b.machines] })),
    }),
  });

  const figures = d.included
    ? el('div', { class: 'col-12' }, [el('div', { class: 'tiles' }, [
      ['Machines drawn', nf0.format(d.included), `of ${nf0.format(d.machines)} in view`],
      ['p10', fmt(d.p10), 'a tenth are at or below'],
      ['Median', fmt(d.median), 'half are at or below'],
      ['p90', fmt(d.p90), 'a tenth are at or above'],
    ].map(([label, value, sub]) => el('div', { class: 'card' }, [
      el('div', { class: 'tile-label', text: label }),
      el('div', { class: 'tile-value', text: value }),
      el('div', { class: 'tile-sub', text: sub }),
    ])))])
    : null;

  const cards = [figures, chart, binCard(d, fmt)];

  if (d.split) {
    const by = d.split.by;
    const plotted = d.split.groups.filter((g) => !g.thin);
    const thin = d.split.groups.length - plotted.length;
    const label = (g) => (g.value !== null ? g.value : (by.startsWith('tag:') ? '(untagged)' : '(not recorded)'));
    cards.push(chartCard({
      title: `${d.label} by ${dimensionLabel(by).toLowerCase()}`,
      subtitle: 'Each line runs from a group’s p10 to its p90, with its median marked, on the axis above. '
        + 'Select a group to see its machines.',
      note: thin
        ? `${thin} group(s) with fewer than ${d.min_cohort} machines drawn are left off — quantiles of so few `
          + 'describe those machines, not the group. The table has every one.'
        : null,
      span: 'col-12',
      draw: (host) => {
        if (!plotted.length) {
          host.append(el('div', { class: 'empty', text: `No group has ${d.min_cohort} or more machines drawn.` }));
          return;
        }
        rangeChart(host, {
          rows: plotted.slice(0, GROUP_CHART_CAP).map((g) => ({ ...g, label: label(g), group: g })),
          format: fmt,
          onSelect: (r) => drillInto(r.group, by),
        });
      },
      table: () => dataTable({
        columns: [{ label: 'Group' }, { label: 'Machines', num: true }, { label: 'p10', num: true },
          { label: 'Median', num: true }, { label: 'p90', num: true }, { label: 'Thin' }],
        rows: d.split.groups.map((g) => ({
          cells: [label(g), g.machines, fmt(g.p10), fmt(g.median), fmt(g.p90), g.thin ? 'yes' : 'no'],
          sort: [label(g).toLowerCase(), g.machines, g.p10, g.median, g.p90, g.thin],
        })),
        sortable: true,
      }),
    }));
  }
  return cards.filter(Boolean);
}

/** A card holding the server's refusal and the way back to the default. */
function refusedCard(err, label, reset) {
  return el('div', { class: 'card col-12' }, [el('div', { class: 'banner' }, [
    el('span', { text: err.message }),
    ' ',
    el('button', { type: 'button', class: 'link-button', text: label, onclick: reset }),
  ])]);
}

export async function renderComponents(main) {
  const [d, h] = await Promise.all([
    fetchDistribution().catch((err) => err),
    fetchHeatmap().catch((err) => err),
  ]);

  const intro = el('div', { class: 'card col-12' }, [
    el('div', { class: 'card-head' }, [el('h3', { text: 'Components across the fleet' })]),
    el('p', {
      class: 'card-sub',
      text: 'How one measure — the overall score, a component, or a single subtest — is spread across the machines '
        + 'in view, and every machine against every graded component. A number is only comparable with another '
        + 'measured the same way, so a distribution draws from one configuration at a time and counts the machines '
        + 'it leaves out. A subtest is drawn as measured, in its own unit; a score is against the baseline.',
    }),
    d instanceof Error ? null : el('div', { class: 'components-pickers' }, [measurePicker(d), configPicker(d), splitPicker(d)]),
  ]);

  const cards = [intro];
  if (d instanceof Error) {
    cards.push(refusedCard(d, 'Show the overall score instead',
      () => setComponents({ measure: null, config: null, split: null })));
  } else {
    cards.push(...distributionCards(d));
  }
  if (h instanceof Error) {
    cards.push(refusedCard(h, 'Sort by the default instead',
      () => setComponents({ encoding: null, hsort: null, hdir: null })));
  } else {
    cards.push(heatmapCard(h));
  }
  main.append(el('div', { class: 'grid' }, cards));
}

/** Tag keys across the machines in view, one table column each. */
function tagKeysOf(machines) {
  return [...new Set(machines.flatMap((m) => Object.keys(m.tags || {})))].sort();
}

/**
 * What each machine calls for: replace, repair, investigate, watch or keep.
 *
 * The server has decided every verdict and written down why; this lays it out.
 * The tiles choose which decision the table lists — a view setting carried in
 * the hash beside the filter, like the Groups view's dimension, because it
 * picks rows here rather than narrowing the fleet everywhere.
 */
export function renderLifecycle(main) {
  const snap = state.snap;
  const counts = snap.summary.lifecycle || {};
  const years = nf0.format(snap.thresholds.replace_age_years);
  const chosen = DECISIONS.some((d) => d.key === state.decision) ? state.decision : null;

  const tiles = el('div', { class: 'tiles decision-tiles', role: 'group', 'aria-label': 'List machines by decision' },
    DECISIONS.map((d) => el('button', {
      type: 'button',
      class: `card decision-tile${d.key === 'keep' ? ' decision-quiet' : ''}`,
      'aria-pressed': chosen === d.key ? 'true' : 'false',
      onclick: () => writeHash({ view: 'lifecycle', decision: chosen === d.key ? null : d.key }),
    }, [
      el('div', { class: 'tile-label' }, [
        d.dot ? el('span', { class: `chip-dot dot-${d.dot}` }) : null,
        el('span', { text: d.label }),
      ]),
      el('div', { class: 'tile-value', text: nf0.format(counts[d.key] ?? 0) }),
      el('div', { class: 'tile-sub', text: d.sub }),
    ])));

  const intro = el('div', { class: 'card col-12' }, [
    el('div', { class: 'card-head' }, [el('h3', { text: 'What each machine calls for' })]),
    tiles,
    el('p', {
      class: 'card-sub lifecycle-rule',
      text: `Replace a machine graded F, or one at least ${years} years old that is also graded D or F or has a `
        + 'part problem. Repair a part — a worn battery, a failing or worn drive, one weak component — on a '
        + 'machine otherwise worth keeping. Investigate one slower than identical machines or than its own past: '
        + 'it should be faster, and finding out why costs less than buying. Watch one that is old or graded D. '
        + 'Age alone is never a reason to buy: an old machine that performs and whose parts are sound is watched, '
        + 'not replaced. Grades are against the reference baseline, so an F means inadequate against that '
        + 'reference, which an estate with modest needs may reasonably disagree with. A decision marked '
        + '“re-measure first” rests on a run that was throttled, on battery, held back or incomplete.'
        + (overriddenCount(snap, 'replace_age_years')
          ? ` ${nf0.format(overriddenCount(snap, 'replace_age_years'))} machine(s) in view are old from another age, `
            + 'set for them in the configuration; each one’s page says which.'
          : ''),
    }),
  ]);

  // Plotted: machines with both an age estimate and a score. The rest are
  // counted in the note rather than placed at a guessed age.
  const aged = snap.machines.filter((m) => m.age_estimate && m.score !== null && m.lifecycle);
  const noAge = snap.machines.filter((m) => !m.age_estimate).length;
  const noScore = snap.machines.filter((m) => m.age_estimate && m.score === null).length;
  const pointsFor = () => aged.map((m) => {
    const d = decisionOf(m.lifecycle.verdict);
    return {
      x: m.age_estimate.months / 12, y: m.score, shape: d.shape, layer: d.layer, colour: ink(d.role),
      hollow: m.age_estimate.basis !== 'firmware', m, d,
    };
  });
  const readout = (p, more) => tipBlock(p.m.hostname || p.m.key, [
    ['', p.d.label + (p.m.lifecycle.uncertain ? ' · re-measure first' : ''), p.colour],
    ['old', ageWords(p.m.age_estimate)],
    [`graded ${p.m.grade || '—'}`, nf0.format(p.m.score)],
    ...(p.m.lifecycle.reasons.length ? [[p.m.lifecycle.reasons[0], '']] : []),
    ...(more > 0 ? [[`more machine(s) at this point`, nf0.format(more)]] : []),
  ]);
  const scatterCard = chartCard({
    title: 'Age against score',
    subtitle: `Each machine’s best-estimate age against its overall score. The line is ${years} years, from `
      + 'where a machine counts as old'
      + (overriddenCount(snap, 'replace_age_years') ? ' unless the configuration sets another for it' : '')
      + '. Select a mark to open the machine.',
    note: [
      'Hollow marks are upper bounds from the CPU’s launch: the machine is at most that old, so its true place is '
        + 'on or left of its mark. Filled marks are dated by firmware that looks original.',
      noAge ? `${nf0.format(noAge)} machine(s) with no age estimate are left off — their CPU isn’t in the launch `
        + 'table and nothing else dates them.' : null,
      noScore ? `${nf0.format(noScore)} without an overall score are left off.` : null,
    ].filter(Boolean).join(' '),
    span: 'col-12',
    draw: (host) => {
      if (aged.length) {
        host.append(markLegend([
          { label: 'Replace', shape: 'square', colour: ink('--status-critical') },
          { label: 'Repair', shape: 'triangle', colour: ink('--status-warning') },
          { label: 'Investigate, watch or keep', shape: 'circle', colour: ink('--deemph') },
          { label: 'Dated', shape: 'circle', colour: ink('--text-secondary') },
          { label: 'At most', shape: 'circle', colour: ink('--text-secondary'), hollow: true },
        ]));
      }
      scatterChart(host, {
        points: pointsFor(),
        xTitle: 'Age, years — best estimate',
        yTitle: 'Score',
        xRef: snap.thresholds.replace_age_years,
        xRefLabel: `old from ${years} yr`,
        readout,
        onSelect: (p) => writeHash({ view: 'machine', key: p.m.key }),
        ariaLabel: 'Each machine’s age against its score, marked by decision',
        empty: 'No machine in view has both an age estimate and a score.',
      });
    },
    table: () => dataTable({
      columns: [{ label: 'Machine' }, { label: 'Decision' }, { label: 'Age', num: true },
        { label: 'Score', num: true }, { label: 'Grade' }],
      rows: aged.map((m) => ({
        key: m.key,
        cells: [m.hostname || m.key, decisionOf(m.lifecycle.verdict).label, ageWords(m.age_estimate),
          num(m.score, 0), m.grade || '—'],
        sort: [(m.hostname || m.key).toLowerCase(), decisionRank(m.lifecycle.verdict), m.age_estimate.months,
          m.score, gradeSort(m.grade)],
      })),
      onRow: (r) => writeHash({ view: 'machine', key: r.key }),
      sortable: true,
    }),
  });

  const listed = snap.machines
    .filter((m) => m.lifecycle && (chosen ? m.lifecycle.verdict === chosen : m.lifecycle.verdict !== 'keep'))
    .sort((a, b) => decisionRank(a.lifecycle.verdict) - decisionRank(b.lifecycle.verdict)
      || (a.hostname || a.key).localeCompare(b.hostname || b.key));
  const tagKeys = tagKeysOf(listed);
  const pctOf = (v) => (v === null || v === undefined ? null : `${nf0.format(v)}%`);
  const decisionCell = (m) => {
    const d = decisionOf(m.lifecycle.verdict);
    return el('span', { class: 'decision-cell' }, [
      d.dot ? el('span', { class: `chip-dot dot-${d.dot}` }) : null,
      el('span', { text: d.label }),
      m.lifecycle.uncertain
        ? el('span', { class: 'chip', text: 're-measure first', title: m.lifecycle.caveats.join('; ') })
        : null,
    ]);
  };
  const keepHidden = !chosen && counts.keep
    ? ` ${nf0.format(counts.keep)} to keep are not listed; choose Keep above to see them.` : '';
  const tableCard = el('div', { class: 'card col-12' }, [
    el('div', { class: 'card-head' }, [el('h3', {
      text: chosen ? `${decisionOf(chosen).label} · ${listed.length}` : `${listed.length} machine(s) calling for a decision`,
    })]),
    el('p', {
      class: 'card-sub',
      text: 'Most urgent first, then by name; the reasons lead with what decided it. Click a column to sort, or a '
        + `machine to open it.${keepHidden}`,
    }),
    listed.length
      ? dataTable({
        columns: [{ label: 'Machine' }, { label: 'Decision' }, { label: 'Reasons', wrap: true },
          { label: 'Age', num: true }, { label: 'Grade' }, { label: 'Score', num: true },
          { label: 'Battery', num: true }, { label: 'Worst drive wear', num: true }, { label: 'System model' },
          ...tagKeys.map((k) => ({ label: k }))],
        rows: listed.map((m) => ({
          key: m.key,
          cells: [
            machineLink(m), decisionCell(m), m.lifecycle.reasons.join(' · ') || '—', ageWords(m.age_estimate),
            m.grade || '—', num(m.score, 0), pctOf(m.battery_health), pctOf(m.worst_drive_wear_pct),
            m.system_model, ...tagKeys.map((k) => m.tags?.[k] ?? '—'),
          ],
          sort: [(m.hostname || m.key).toLowerCase(), decisionRank(m.lifecycle.verdict),
            m.lifecycle.reasons.length, m.age_estimate?.months ?? null, gradeSort(m.grade),
            m.score, m.battery_health ?? null, m.worst_drive_wear_pct ?? null,
            m.system_model?.toLowerCase() ?? null, ...tagKeys.map((k) => m.tags?.[k]?.toLowerCase() ?? null)],
        })),
        onRow: (r) => writeHash({ view: 'machine', key: r.key }),
        sortable: true,
      })
      : el('div', {
        class: 'empty',
        text: chosen ? `No machine in view to ${chosen}.` : 'Nothing in view calls for a decision.',
      }),
  ]);

  main.append(el('div', { class: 'grid' }, [intro, scatterCard, tableCard]));
}

/** A grade as a sort key, best first: the letters do not sort themselves, and
 *  one this build does not know sorts last, as every missing value does. */
function gradeSort(g) {
  const i = ['S', 'A', 'B', 'C', 'D', 'F'].indexOf(g);
  return i < 0 ? null : i;
}

export function renderMachines(main) {
  const flagCount = new Map();
  for (const f of state.snap.flags) {
    // Live ones only, as "needs attention" counts them everywhere else.
    if (f.severity === 'info' || f.triage) continue;
    flagCount.set(f.machine_key, (flagCount.get(f.machine_key) || 0) + 1);
  }
  // One column per tag key rather than one column holding all of them joined.
  // The joined form could not be sorted usefully: "ring=canary site=glasgow
  // user=jsmith" sorts by whichever key happens to come first alphabetically,
  // so asking for the table by owner gave you it by ring. Same text on screen,
  // one column each, each sortable on its own.
  const tagKeys = [...new Set(
    state.snap.machines.flatMap((m) => Object.keys(m.tags || {})),
  )].sort();

  const rows = state.snap.machines.map((m) => ({
    key: m.key,
    cells: [
      machineLink(m),
      m.cpu_model,
      num(m.score, 0),
      m.grade || '—',
      deltaCell(m.cohort_delta_pct),
      deltaCell(m.trend_pct),
      flagCount.get(m.key) || 0,
      maxAge(m.max_age_months_now),
      ago(m.age_days),
      ...tagKeys.map((k) => m.tags?.[k] ?? '—'),
    ],
    // Machines with no value for a tag sort last either way: dataTable puts
    // null after everything, ascending or descending, so "sort by owner" does
    // not bury the assigned machines under the unassigned ones.
    sort: [m.hostname || m.key, m.cpu_model, m.score, m.grade, m.cohort_delta_pct, m.trend_pct,
      flagCount.get(m.key) || 0, m.max_age_months_now ?? null, -(m.age_days ?? 1e9),
      ...tagKeys.map((k) => m.tags?.[k]?.toLowerCase() ?? null)],
  }));

  main.append(el('div', { class: 'grid' }, [el('div', { class: 'card col-12' }, [
    el('div', { class: 'card-head' }, [
      el('h3', { text: `${state.snap.summary.machines} machine(s)` }),
      el('div', { class: 'card-actions' }, [csvLink('machines')]),
    ]),
    el('p', {
      class: 'card-sub',
      text: 'Click a column to sort, or a machine to drill in. The CSV holds these machines, in this filter, with '
        + 'more columns than fit here: serial, asset tag, the decision and its reason, firmware, drive wear and '
        + 'free space, and every tag.',
    }),
    dataTable({
      columns: [{ label: 'Machine' }, { label: 'CPU' }, { label: 'Score', num: true }, { label: 'Grade' },
        { label: 'vs peers', num: true }, { label: 'vs own history', num: true },
        { label: 'Findings', num: true }, { label: 'Age', num: true }, { label: 'Measured' },
        ...tagKeys.map((k) => ({ label: k }))],
      rows,
      sortable: true,
      // Best first: the fleet's strongest machines at the top, unscored last.
      initialSort: { key: 2, dir: -1 },
    }),
  ])]));
}

export async function renderMachine(main, key) {
  let payload;
  try {
    payload = await fetchJson(`/api/machine/${encodeURIComponent(key)}`);
  } catch (err) {
    main.append(el('div', { class: 'banner' }, [el('span', { text: err.message })]));
    return;
  }
  const { machine: m, flags, cohort, history, subtests } = payload;
  // Absent from a server older than the drive table, as well as empty for a
  // run that recorded none.
  const drives = payload.drives || [];

  const age = ageFacts(m);
  // The fixed volume with the least free space — what `disk_nearly_full`
  // judges. Free bytes to more than whole GiB, since on a nearly full disk
  // the fraction is the point.
  const fullest = m.disk_low_mount
    ? `${m.disk_low_mount} — ${num(m.disk_low_free_pct, 1)}% free`
      + (m.disk_low_free_bytes === null || m.disk_low_free_bytes === undefined
        ? '' : ` (${num(m.disk_low_free_bytes / 1024 ** 3)} GiB)`)
    : null;
  const facts = [
    ...(m.system_model ? [['System', m.system_model]] : []),
    ['CPU', m.cpu_model],
    ['Age', age.built || 'unknown — the CPU isn’t in the launch table'],
    ...(lifecycleFact(m) ? [['Lifecycle', lifecycleFact(m)]] : []),
    ...(thresholdsFact(m) ? [['Judged by', thresholdsFact(m)]] : []),
    ...(age.firmware ? [['Firmware', age.firmware]] : []),
    ...(age.drive ? [['Drive', age.drive]] : []),
    ...(fullest ? [['Fullest volume', fullest]] : []),
    ...(powerFact(m) ? [['Power', powerFact(m)]] : []),
    ...(m.battery_cycles === null || m.battery_cycles === undefined ? [] : [['Battery',
      `${m.battery_cycles} cycles`
        + (m.battery_health === null || m.battery_health === undefined ? '' : ` · ${nf0.format(m.battery_health)}% of design capacity`)]]),
    ['Cores', `${m.cpu_cores} logical`],
    ['Memory', gib(m.ram_bytes)],
    ['OS', m.os || 'unknown'],
    ['Architecture', m.arch],
    ['Serial', m.serial || 'not reported'],
    ['Asset tag', m.asset_tag || 'not reported'],
    ['Attributed by', m.key_kind],
    ['Measured', `${when(m.taken_at)} (${ago(m.age_days)})`],
    ['loadbearer', m.tool_version],
    ['Configuration', `${m.comparability.preset} · ${m.comparability.profile} · ${m.comparability.baseline} · ${m.comparability.build_isa}`],
    ['Source', m.source_path],
  ];

  const head = el('div', { class: 'card col-8' }, [
    el('div', { class: 'detail-head' }, [
      el('div', {}, [
        el('h2', { text: m.hostname || m.key }),
        el('div', { class: 'detail-facts' }, [
          ...Object.entries(m.tags || {}).map(([k, v]) => el('span', { class: 'chip' }, [
            el('span', { text: k }), el('b', { text: v }),
          ])),
          el('span', { class: 'chip' }, [el('span', { text: `${m.runs} run(s) since ${day(m.first_seen)}` })]),
        ]),
      ]),
    ]),
    el('dl', { class: 'kv' }, facts.flatMap(([k, v]) => [el('dt', { text: k }), el('dd', { text: v })])),
  ]);

  const heroCard = el('div', { class: 'card col-4' }, [el('div', { class: 'hero' }, [
    el('div', { class: 'hero-value', text: m.score === null ? '—' : nf0.format(m.score) }),
    el('div', { class: 'hero-label', text: `graded ${m.grade || 'unrecognised'} against ${m.comparability.baseline}` }),
    el('div', {
      class: 'hero-sub',
      text: [
        m.cohort_delta_pct === null
          ? `No peer group: fewer than ${state.snap.thresholds.min_cohort} machines share this hardware and configuration.`
          : `${signed(m.cohort_delta_pct, 1)} against ${cohort ? cohort.members : 0} machines like it (median ${cohort ? nf0.format(cohort.median) : '—'}).`,
        m.trend_pct === null ? 'No earlier comparable run to trend against.' : `${signed(m.trend_pct, 1)} against its own earlier runs.`,
      ].join(' '),
    }),
  ])]);

  const points = history.map((h) => ({
    ...h,
    comparable: JSON.stringify(h.comparability) === JSON.stringify(m.comparability),
  }));
  const mixed = points.some((p) => !p.comparable);
  const historyCard = chartCard({
    title: 'Score history',
    subtitle: 'The same machine over time. Holding the hardware constant makes this the strongest signal there is.',
    note: mixed
      ? 'Grey points were measured under a different preset, profile, baseline or instruction set. They are shown for '
        + 'context and are deliberately left out of the trend, because a machine measured another way has not got slower.'
      : null,
    span: 'col-8',
    draw: (host) => {
      if (points.length < 2) {
        host.append(el('div', { class: 'empty', text: 'One run so far — a trend needs a second.' }));
        return;
      }
      if (mixed) {
        host.append(legend([
          { label: 'This configuration', colour: ink('--series-1'), line: true },
          { label: 'Measured differently', colour: ink('--deemph'), line: true },
        ]));
      }
      lineChart(host, { points });
    },
    table: () => dataTable({
      columns: [{ label: 'Measured' }, { label: 'Score', num: true }, { label: 'Grade' },
        { label: 'Configuration' }, { label: 'Caveats', wrap: true }, { label: 'loadbearer' }],
      rows: points.slice().reverse().map((p) => ({
        cells: [when(p.taken_at), num(p.score, 0), p.grade,
          `${p.comparability.preset} · ${p.comparability.build_isa}`,
          [p.thermal_limited ? 'clocks fell' : null, p.on_ac === false ? 'on battery' : null,
            p.partial ? 'partial' : null].filter(Boolean).join(', ') || 'none',
          p.tool_version],
      })),
    }),
  });

  const comps = m.components;
  const componentCard = chartCard({
    title: 'Components',
    subtitle: 'Where the overall score comes from.',
    note: 'Network and GPU are measured but never folded into a grade: they describe the link and the driver stack '
      + 'rather than the machine, so they are shown in grey.',
    span: 'col-4',
    draw: (host) => {
      host.append(legend([
        { label: 'Graded', colour: ink('--series-1') },
        { label: 'Measured, not graded', colour: ink('--deemph') },
      ]));
      columnChart(host, {
        rows: comps.map((c) => ({
          label: c.id, value: Math.round(c.score), sublabel: c.grade,
          fill: c.graded ? ink('--series-1') : ink('--deemph'),
        })),
        unit: 'score',
      });
    },
    table: () => dataTable({
      columns: [{ label: 'Component' }, { label: 'Score', num: true }, { label: 'Grade' }, { label: 'Graded' }],
      rows: comps.map((c) => ({ cells: [c.id, num(c.score, 0), c.grade, c.graded ? 'yes' : 'no'] })),
    }),
  });

  const grouped = Object.keys(QUEUES).map((q) => [q, flags.filter((f) => f.kind === q)]);
  const flagCard = el('div', { class: 'card col-12' }, [
    el('div', { class: 'card-head' }, [el('h3', { text: 'Findings' })]),
    flags.length
      ? el('div', {}, grouped.filter(([, fs]) => fs.length).flatMap(([q, fs]) => [
        el('p', { class: 'card-sub', text: QUEUES[q] }),
        // Every finding, quieted or not: this is the machine's whole record,
        // not a queue. What was said about each is on it.
        ...fs.map((f) => el('div', { class: findingClass(f) }, [el('div', {}, [
          el('div', { class: 'finding-head' }, [
            severityChip(f.severity), el('span', { class: 'finding-who', text: f.headline }),
          ]),
          el('p', { class: 'finding-detail', text: f.detail }),
          triageControls(f),
        ])])),
      ]))
      : el('div', { class: 'empty', text: 'Nothing flagged on this machine.' }),
  ]);

  // Every drive the latest run could query, not just the boot drive the age
  // line reads. Hidden rather than shown empty: no drives means a run without
  // admin rights or from before loadbearer 1.6.0, which says nothing about
  // the drives themselves. Blank cells are figures the drive doesn't report.
  const opt = (v, suffix = '') => (v === null || v === undefined ? null : `${nf0.format(v)}${suffix}`);
  const driveCard = drives.length
    ? el('div', { class: 'card col-12' }, [
      el('div', { class: 'card-head' }, [el('h3', { text: 'Drives' })]),
      el('p', {
        class: 'card-sub',
        text: 'Health counters from each drive, as of the latest run. Wear is the vendor’s estimate of rated '
          + 'endurance used — it can pass 100% — not a prediction of failure; a non-zero warning is the drive '
          + 'itself reporting a problem.',
      }),
      dataTable({
        columns: [{ label: 'Drive' }, { label: 'Interface' }, { label: 'Hours', num: true },
          { label: 'Wear', num: true }, { label: 'Spare', num: true }, { label: 'Media errors', num: true },
          { label: 'Warning' }],
        rows: drives.map((d) => ({
          cells: [d.model || d.device, d.interface, opt(d.power_on_hours), opt(d.percentage_used, '%'),
            opt(d.available_spare_pct, '%'), opt(d.media_errors),
            d.critical_warning === null || d.critical_warning === undefined
              ? null
              : d.critical_warning === 0 ? 'none' : `0x${d.critical_warning.toString(16).padStart(2, '0')}`],
        })),
      }),
    ])
    : null;

  const measurementCard = el('div', { class: 'card col-12' }, [
    el('div', { class: 'card-head' }, [el('h3', { text: 'Measurements' })]),
    el('p', {
      class: 'card-sub',
      text: 'Every subtest of the latest run. "Statistic" says whether a figure is the median of the iterations or the '
        + 'peak of them — comparing one against the other means nothing.',
    }),
    dataTable({
      columns: [{ label: 'Component' }, { label: 'Subtest' }, { label: 'Value', num: true }, { label: 'Unit' },
        { label: 'Score', num: true }, { label: 'vs baseline', num: true }, { label: 'Spread', num: true },
        { label: 'Confidence' }, { label: 'Statistic' }, { label: 'Graded' }],
      rows: subtests.map((t) => ({
        cells: [t.component, t.id, num(t.value), t.unit, t.score === null ? '—' : num(t.score, 0),
          t.ratio === null ? '—' : `${t.ratio.toFixed(2)}×`,
          t.cv === null ? '—' : `${(t.cv * 100).toFixed(1)}%`,
          t.confidence, t.representative || 'median', t.scored ? 'yes' : 'no'],
        sort: [t.component, t.id, t.value, t.unit, t.score, t.ratio, t.cv, t.confidence, t.representative, t.scored],
      })),
      sortable: true,
    }),
  ]);

  main.append(
    el('div', { class: 'breadcrumb' }, [el('button', {
      type: 'button', class: 'link-button', text: '← All machines',
      onclick: () => writeHash({ view: 'machines', key: null }),
    })]),
    el('div', { class: 'grid' }, [head, heroCard, historyCard, componentCard, flagCard, driveCard, measurementCard]
      .filter(Boolean)),
  );
}

/* ----------------------------------------------------------------- compare */

/** Machines the caller can see, for the picker. Scoped already: this is the
 *  filtered snapshot, and the server refuses anything outside it anyway. */
function pickableMachines() {
  return (state.snap?.machines || []).slice()
    .sort((a, b) => (a.hostname || a.key).localeCompare(b.hostname || b.key));
}

/**
 * The chooser. Machines go in, runs come out — each slot defaulting to the
 * machine's latest run, and swappable for an earlier one.
 *
 * Adding the same machine twice is allowed on purpose: "this machine before
 * the firmware update, and now" is the comparison with the fewest confounders
 * in it, and the only one where the hardware is definitely identical.
 */
function comparePicker(runs, histories) {
  const machines = pickableMachines();
  const byRun = new Map();
  for (const [key, h] of histories) for (const p of h) byRun.set(p.run_id, { key, point: p });

  const slots = runs.map((id, i) => {
    const found = byRun.get(id);
    const m = machines.find((x) => x.key === found?.key);
    const name = m ? (m.hostname || m.key) : `run ${id}`;
    const history = found ? histories.get(found.key) : null;

    let chooser;
    if (history && history.length > 1) {
      chooser = el('select', {
        'aria-label': `Which run of ${name}`,
        onchange: (e) => {
          const next = runs.slice();
          next[i] = Number(e.target.value);
          writeHash({ view: 'compare', runs: next });
        },
      }, history.slice().reverse().map((p) => el('option', {
        value: String(p.run_id), text: when(p.taken_at),
      })));
      // Set as a property, not an attribute: this is what the browser actually
      // reads, and it cannot drift from the run id in the URL the way a
      // per-option flag can.
      chooser.value = String(id);
    } else {
      chooser = el('span', {
        class: 'meta',
        text: found ? when(found.point.taken_at) : 'run not found',
      });
    }

    return el('div', { class: 'compare-slot' }, [
      el('span', { class: 'compare-slot-name', text: `${i === 0 ? 'Baseline: ' : ''}${name}` }),
      chooser,
      el('button', {
        type: 'button', class: 'btn btn-quiet', text: 'Remove',
        'aria-label': `Remove ${name} from the comparison`,
        onclick: () => writeHash({ view: 'compare', runs: runs.filter((_, j) => j !== i) }),
      }),
    ]);
  });

  // A machine already in the comparison stays in the list: choosing it again
  // adds its previous run, which is how you compare a machine with itself.
  const add = el('select', {
    'aria-label': 'Add a machine to the comparison',
    onchange: (e) => {
      const m = machines.find((x) => x.key === e.target.value);
      if (!m) return;
      const used = new Set(runs);
      const history = histories.get(m.key);
      const pick = history
        ? [...history].reverse().map((p) => p.run_id).find((id) => !used.has(id))
        : m.run_id;
      if (pick === undefined || used.has(pick)) return;
      writeHash({ view: 'compare', runs: [...runs, pick] });
    },
  }, [
    el('option', { value: '', text: runs.length >= 4 ? 'Four is the most that fits' : 'Add a machine…', selected: true }),
    ...(runs.length >= 4 ? [] : machines.map((m) => el('option', {
      value: m.key, text: `${m.hostname || m.key} — ${m.cpu_model}`,
    }))),
  ]);

  return el('div', { class: 'card col-12' }, [
    el('div', { class: 'card-head' }, [el('h3', { text: 'Compare runs' })]),
    el('p', {
      class: 'card-sub',
      text: 'Two to four runs, measured against the first. The same machine twice compares it '
        + 'with its own past, which is the comparison with the fewest other things changing.',
    }),
    el('div', { class: 'compare-slots' }, [...slots, add]),
  ]);
}

/** `1.24×` / `0.81×`, coloured the way the rest of the dashboard colours a
 *  delta, and marked when it is the best of the row. */
function relCell(rel, best) {
  const cls = rel > 1.02 ? 'delta-up' : rel < 0.98 ? 'delta-down' : '';
  return el('span', { class: `${cls}${best ? ' compare-best' : ''}`.trim(), text: `${rel.toFixed(2)}×` });
}

function compareTable(data) {
  // Name over date, in two elements rather than one string: a heading of
  // `FLEET-WIN-01 Sep 9, 2026, 02:50 PM` is `nowrap` like every other heading
  // in the dashboard, which made each column 212px wide and pushed every ratio
  // off a phone. The columns should be as wide as their numbers.
  const head = [el('th', {})].concat(data.runs.map((r) => el('th', { class: 'num' }, [
    el('div', { text: r.label }),
    el('div', { class: 'meta', text: when(r.taken_at) }),
  ])));
  const table = el('table', { class: 'data compare' });
  const rows = [];

  // What each machine is, before how it performs: age is the usual reason two
  // machines differ, so it sits above the ratios rather than off to one side.
  // Text, not ratios — "younger" isn't a direction a ratio can carry.
  const facts = data.runs.map(ageFacts);
  const machineRows = [
    ['System', data.runs.map((r) => r.system_model || '—')],
    ['Max age', data.runs.map((r) => maxAge(r.max_age_months_now))],
    ['Built', data.runs.map((r) => (r.cpu_launched ? `${monthYear(r.cpu_launched)} or later` : '—'))],
    ['Firmware', data.runs.map((r) => (r.firmware_dated ? monthYear(r.firmware_dated) : '—'))],
    ['Drive', facts.map((f) => f.drive || '—')],
    ['Battery', data.runs.map((r) => (r.battery_cycles === null || r.battery_cycles === undefined ? '—' : `${r.battery_cycles} cycles`))],
    ['Power', data.runs.map((r) => powerFact(r) || '—')],
  ].filter(([, cells]) => cells.some((c) => c !== '—'));
  if (machineRows.length) {
    rows.push(el('tr', { class: 'compare-component' }, [
      el('th', { scope: 'row' }, [el('span', { text: 'Machine' })]),
      ...data.runs.map(() => el('td', {})),
    ]));
    for (const [label, cells] of machineRows) {
      rows.push(el('tr', {}, [
        el('th', { scope: 'row', class: 'compare-subtest' }, [el('span', { text: label })]),
        ...cells.map((c) => el('td', { class: 'num', text: c })),
      ]));
    }
  }

  // The first run is the reference, so its ratio is 1.00× in every single row.
  // A column of constants is a column of noise: the baseline shows its
  // measurements and leaves comparing to the columns that are doing some.
  for (const c of data.components) {
    rows.push(el('tr', { class: 'compare-component' }, [
      el('th', { scope: 'row' }, [
        el('span', { text: c.label }),
        c.graded ? null : el('span', { class: 'meta', text: ' not counted' }),
      ]),
      ...c.rel.map((r, i) => el('td', { class: 'num' }, [
        i === 0 ? el('span', { class: 'meta', text: 'baseline' }) : relCell(r, i === c.best),
      ])),
    ]));
    for (const s of c.subtests) {
      rows.push(el('tr', {}, [
        el('th', { scope: 'row', class: 'compare-subtest' }, [
          el('span', { text: s.label }),
          el('span', { class: 'meta', text: ` ${s.unit}${s.scored ? '' : ' · informational'}` }),
        ]),
        ...s.values.map((v, i) => el('td', { class: 'num' }, [
          el('div', { text: num(v) }),
          i === 0 ? null : el('div', { class: 'compare-rel' }, [relCell(s.rel[i], i === s.best)]),
        ])),
      ]));
    }
  }

  table.append(el('thead', {}, [el('tr', {}, head)]), el('tbody', {}, rows));
  return el('div', { class: 'table-wrap' }, [table]);
}

export async function renderCompare(main, runs) {
  // Each selected machine's history, so a slot can offer its earlier runs.
  const histories = new Map();
  const machines = pickableMachines();
  const wanted = new Set(runs);
  await Promise.all(machines.map(async (m) => {
    // Every machine's latest run is already known; only fetch the history of
    // one that is actually in play, or that the picker may need to offer.
    if (!wanted.has(m.run_id) && !runs.length) {
      histories.set(m.key, [{ run_id: m.run_id, taken_at: m.taken_at }]);
      return;
    }
    try {
      const payload = await fetchJson(`/api/machine/${encodeURIComponent(m.key)}`);
      histories.set(m.key, payload.history || []);
    } catch {
      histories.set(m.key, [{ run_id: m.run_id, taken_at: m.taken_at }]);
    }
  }));

  const cards = [comparePicker(runs, histories)];

  if (runs.length < 2) {
    cards.push(el('div', { class: 'card col-12' }, [
      el('div', {
        class: 'empty',
        text: machines.length < 2
          ? 'Two machines are needed before anything can be compared.'
          : 'Pick a second run to compare against.',
      }),
    ]));
    main.append(el('div', { class: 'grid' }, cards));
    return;
  }

  let data;
  try {
    data = await fetchJson(`/api/compare?runs=${runs.join(',')}`);
  } catch (err) {
    // The server's refusals are written to be read — a mixed statistic, a
    // missing direction, nothing in common — so they are shown as they are.
    cards.push(el('div', { class: 'card col-12' }, [
      el('div', { class: 'banner' }, [el('span', { text: err.message })]),
    ]));
    main.append(el('div', { class: 'grid' }, cards));
    return;
  }

  const { coverage } = data;
  cards.push(el('div', { class: 'card col-12' }, [
    el('div', { class: 'card-head' }, [el('h3', { text: 'Verdict' })]),
    el('p', { class: 'compare-summary', text: data.overall.summary }),
    el('p', {
      class: 'card-sub',
      text: `From ${coverage.compared} shared measurement(s)`
        + (coverage.left_out ? `, with ${coverage.left_out} left out.` : '.')
        + ' Ratios are against the first run; over 1.00× is better, whichever direction the'
        + ' metric runs in.',
    }),
    compareTable(data),
    data.components.some((c) => !c.graded)
      ? el('p', {
        class: 'card-note',
        text: 'Network and GPU are shown but not counted: they depend on the host, its drivers '
          + 'and whatever the network is doing, which is why they are left out of a grade too.',
      })
      : null,
  ]));

  if (data.warnings.length) {
    cards.push(el('div', { class: 'card col-12' }, [
      el('div', { class: 'card-head' }, [
        el('h3', { text: `${data.warnings.length} thing(s) not compared` }),
      ]),
      el('p', { class: 'card-sub', text: 'Left out rather than guessed at. Each one would have made the numbers mean something else.' }),
      el('ul', { class: 'compare-warnings' }, data.warnings.map((w) => el('li', { text: w }))),
    ]));
  }

  main.append(el('div', { class: 'grid' }, cards));
}

/* ------------------------------------------------------------------ report */

/*
 * The estate as a document, for the people who will read it on paper or as a
 * PDF: a review meeting, a budget request. Printing is the browser's own —
 * Print, or Save as PDF — so there is no PDF library and nothing new to trust,
 * and the print stylesheet in app.css is what turns the page into pages.
 *
 * Every figure comes from the calls the tabs make — the snapshot already in
 * hand, `/api/changes`, `/api/timeline`, `/api/groups` — through the same
 * filter, so the report cannot disagree with the dashboard. What it adds is
 * order and a cap: a review needs the things to act on, not the whole queue,
 * and the CSV is there for the rest.
 */

/** The report's default period: four weeks back. */
const REPORT_WEEKS = 4;

/** The most rows a list in the report prints. Past this a printed list is a
 *  stack of paper nobody reads, and the CSV holds every row. */
export const REPORT_ROWS = 50;

/** The decisions the report lists: the three that call for someone to act. */
const REPORT_DECISIONS = ['replace', 'repair', 'investigate'];

/** The filter in words, for the top of the page — paper has no filter row. */
function filterWords(filter) {
  const said = [];
  if (filter.since_days) said.push(`measured in the last ${filter.since_days} days`);
  if (filter.tags) said.push(`tagged ${filter.tags.split(',').join(' and ')}`);
  if (filter.model) said.push(`system model ${filter.model}`);
  if (filter.cpu) said.push(`CPU ${filter.cpu}`);
  if (filter.os) said.push(`OS ${filter.os}`);
  if (filter.cohort) said.push(`cohort ${filter.cohort}`);
  if (filter.flag) said.push(`carrying ${codeWords(filter.flag)}`);
  if (filter.lifecycle) said.push(`decided ${decisionOf(filter.lifecycle).label.toLowerCase()}`);
  if (filter.q) said.push(`matching “${filter.q}”`);
  return said.length ? `Machines ${said.join(', ')}.` : 'Every machine, no filter applied.';
}

/** The report's address, from the view the reader is on: the same filter,
 *  and the overview's comparison if they chose one. */
function reportHash() {
  const params = new URLSearchParams(state.filter);
  if (state.window && state.window !== 1) params.set('changes', String(state.window));
  const q = params.toString();
  return `#/report${q ? `?${q}` : ''}`;
}

/** Where a table of the current view downloads from: the filter the page was
 *  drawn with, so the file holds the machines on screen. */
function csvHref(table) {
  const q = new URLSearchParams(state.filter).toString();
  return `/api/${table}.csv${q ? `?${q}` : ''}`;
}

/** A plain link to the download, so the browser saves the file itself and
 *  the server names it. */
function csvLink(table, label = 'Download CSV') {
  return el('a', {
    class: 'btn btn-quiet', href: csvHref(table), download: true, text: label,
    'aria-label': `Download the ${table} in view as CSV`,
  });
}

/** One list of the report, capped, with what was left off said in words. */
function cappedTable(columns, rows, more) {
  const shown = rows.slice(0, REPORT_ROWS);
  return [
    dataTable({ columns, rows: shown }),
    rows.length > shown.length
      ? el('p', { class: 'card-note', text: `And ${nf0.format(rows.length - shown.length)} more — ${more}.` })
      : null,
  ];
}

function reportSection(title, kids) {
  return el('section', { class: 'report-section' }, [el('h2', { text: title }), ...kids]);
}

export async function renderReport(main) {
  const snap = state.snap;
  const s = snap.summary;
  const me = state.me || {};
  const machinesByKey = new Map(snap.machines.map((m) => [m.key, m]));
  const weeks = state.reportWindow || REPORT_WEEKS;
  const by = state.reportBy;
  const now = new Date();

  const [changes, timeline, grouping] = await Promise.all([
    fetchChanges(weeks).catch((err) => err),
    fetchTimeline().catch((err) => err),
    by ? fetchGroups(by).catch((err) => err) : null,
  ]);

  // Who, when, where from and over what: the questions a page passed round a
  // meeting gets asked, answered before anything else.
  const build = snap.build && snap.build !== 'unknown' ? ` (${snap.build})` : '';
  // `public_url` is checked at startup only when sign-in is configured; without
  // it the setting may be the default, which is not where this page was read.
  const source = me.sign_in_enabled && me.public_url ? me.public_url : location.origin;
  const scopes = me.scopes || [];
  const head = el('header', { class: 'report-head' }, [
    el('h1', { text: 'Fleet report' }),
    el('p', { class: 'report-meta', text: filterWords(state.filter) }),
    scopes.length ? el('p', {
      class: 'report-meta',
      text: `Limited by sign-in to the machines tagged ${scopes
        .map((sc) => Object.entries(sc).map(([k, v]) => `${k}=${v}`).join(' and ')).join(', or ')}.`,
    }) : null,
    el('p', {
      class: 'report-meta',
      text: `${nf0.format(s.machines)} machine(s) · generated ${when(now.toISOString())}`
        + (me.authenticated && me.name ? ` by ${me.name}` : '')
        + ` · results as read ${when(snap.generated_at)}`,
    }),
    el('p', { class: 'report-meta', text: `${source} · loadbearer-fleet ${snap.version}${build}` }),
  ]);

  // On screen only: the print stylesheet drops all of it.
  const windowPicker = el('select', {
    'aria-label': 'What changed since',
    onchange: (e) => { state.reportWindow = Number(e.target.value); writeHash({ view: 'report' }); },
  }, WINDOWS.map(([w, label]) => el('option', { value: String(w), text: `${label} ago` })));
  windowPicker.value = String(weeks);
  const dims = ['model', 'cpu', 'os', ...tagKeysOf(snap.machines).map((k) => `tag:${k}`)];
  if (by && !dims.includes(by)) dims.push(by);
  const byPicker = el('select', {
    'aria-label': 'Group by',
    onchange: (e) => { state.reportBy = e.target.value || null; writeHash({ view: 'report' }); },
  }, [el('option', { value: '', text: 'No groups' }),
    ...dims.map((d) => el('option', { value: d, text: dimensionLabel(d) }))]);
  byPicker.value = by || '';
  const toolbar = el('div', { class: 'report-toolbar' }, [
    el('button', { type: 'button', class: 'btn', text: 'Print', onclick: () => window.print() }),
    el('label', { class: 'field' }, [el('span', { class: 'field-label', text: 'Changes since' }), windowPicker]),
    el('label', { class: 'field' }, [el('span', { class: 'field-label', text: 'Groups' }), byPicker]),
    csvLink('machines', 'Machines CSV'),
    csvLink('findings', 'Findings CSV'),
  ]);

  // 1. The headline.
  const grades = s.grades.filter((g) => g[0] !== 'unrecognised' || g[1] > 0);
  const figure = (label, value, sub) => el('div', { class: 'card' }, [
    el('div', { class: 'tile-label', text: label }),
    el('div', { class: 'tile-value', text: value }),
    el('div', { class: 'tile-sub', text: sub || '' }),
  ]);
  const quieted = quietedWords(s.quieted);
  const quietTotal = s.quieted ? s.quieted.acknowledged + s.quieted.snoozed + s.quieted.suppressed : 0;
  const summary = reportSection('Summary', [
    el('div', { class: 'tiles' }, [
      figure('Machines', nf0.format(s.machines),
        s.median_score === null ? '' : `median score ${nf0.format(s.median_score)}`),
      figure('Need attention', nf0.format(s.machines_flagged), 'with a live critical or warning finding'),
      figure('Live findings', nf0.format(s.critical + s.warnings + s.info),
        `${s.critical} critical · ${s.warnings} warning · ${s.info} informational`),
      figure('Quieted', nf0.format(quietTotal), quieted || 'nothing acknowledged, snoozed or suppressed'),
    ]),
    el('h3', { class: 'report-sub', text: 'Grades against the reference baseline' }),
    grades.length
      ? dataTable({
        columns: grades.map(([g]) => ({ label: g, num: true })),
        rows: [{ cells: grades.map(([, n]) => n) }],
      })
      : el('p', { class: 'empty', text: 'No machines in view.' }),
  ]);

  // 2. What changed, laid out by the overview's own card without its picker
  // and with each list capped rather than collapsed — a collapsed list on
  // paper is a list with rows silently missing.
  const changed = reportSection(`What changed in ${WINDOWS.find(([w]) => w === weeks)?.[1] || `${weeks} weeks`}`,
    changesCard(changes, machinesByKey, {
      picker: false,
      list: (rows) => {
        const shown = rows.slice(0, REPORT_ROWS);
        return el('div', {}, [
          el('div', { class: 'finding-list' }, shown),
          rows.length > shown.length
            ? el('p', { class: 'card-note', text: `And ${nf0.format(rows.length - shown.length)} more — see the dashboard.` })
            : null,
        ]);
      },
    }).slice(1));

  // 3. What to act on: live critical and warning findings, in the server's
  // order. Informational and quieted ones are counted, not listed.
  const live = snap.flags.filter((f) => !f.triage && f.severity !== 'info');
  const findings = reportSection('Findings to act on', [
    el('p', {
      class: 'card-sub',
      text: `${nf0.format(live.length)} live critical or warning finding(s), most severe first. Not listed: `
        + `${nf0.format(s.info)} informational, and ${nf0.format(quietTotal)} quieted`
        + `${quieted ? ` (${quieted})` : ''} — the findings CSV has every one, with its state.`,
    }),
    ...(live.length
      ? cappedTable(
        [{ label: 'Machine' }, { label: 'Severity' }, { label: 'Finding', wrap: true }, { label: 'Detail', wrap: true }],
        live.map((f) => {
          const m = machinesByKey.get(f.machine_key);
          return {
            cells: [m ? machineLink(m) : (f.hostname || f.machine_key), severityChip(f.severity),
              `${f.headline} · ${QUEUES[f.kind] || f.kind}`, f.detail],
          };
        }),
        'see the findings CSV',
      )
      : [el('p', { class: 'empty', text: 'Nothing at warning or above in view.' })]),
  ]);

  // 4. What each machine calls for: the three decisions that need someone.
  const candidates = snap.machines
    .filter((m) => m.lifecycle && REPORT_DECISIONS.includes(m.lifecycle.verdict))
    .sort((a, b) => decisionRank(a.lifecycle.verdict) - decisionRank(b.lifecycle.verdict)
      || (a.hostname || a.key).localeCompare(b.hostname || b.key));
  const counts = s.lifecycle || {};
  const lifecycle = reportSection('What to do about each machine', [
    el('p', {
      class: 'card-sub',
      text: `${DECISIONS.map((d) => `${nf0.format(counts[d.key] ?? 0)} to ${d.key}`).join(' · ')}. `
        + 'Replace, repair and investigate are listed, each with the reasons the decision rests on.',
    }),
    ...(candidates.length
      ? cappedTable(
        [{ label: 'Machine' }, { label: 'Decision' }, { label: 'Reasons', wrap: true }, { label: 'Age' },
          { label: 'Grade' }, { label: 'System model', wrap: true }],
        candidates.map((m) => ({
          cells: [machineLink(m),
            decisionOf(m.lifecycle.verdict).label + (m.lifecycle.uncertain ? ' — re-measure first' : ''),
            m.lifecycle.reasons.join(' · ') || '—', ageWords(m.age_estimate) || '—', m.grade || '—',
            m.system_model || '—'],
        })),
        'see the Lifecycle tab, or the machines CSV',
      )
      : [el('p', { class: 'empty', text: 'Nothing in view to replace, repair or investigate.' })]),
  ]);

  // 5. Groups, only when the address names a dimension.
  let groups = null;
  if (by && grouping instanceof Error) {
    groups = reportSection('Groups', [el('div', { class: 'banner' }, [el('span', { text: grouping.message })])]);
  } else if (by) {
    groups = reportSection(`By ${dimensionLabel(grouping.by).toLowerCase()}`, [
      el('p', {
        class: 'card-sub',
        text: 'Compared by the median of each group’s machines against identical machines — the same CPU, '
          + 'measured the same way — rather than by raw score, which mostly measures the age of the hardware.',
      }),
      dataTable({
        columns: [{ label: 'Group', wrap: true }, { label: 'Machines', num: true }, { label: 'Flagged', num: true },
          { label: 'Critical', num: true }, { label: 'Replace', num: true }, { label: 'Repair', num: true },
          { label: 'Median score', num: true }, { label: 'vs identical', num: true }],
        rows: grouping.groups.map((g) => ({
          cells: [groupLabel(g, grouping.by), g.machines, `${g.flagged} (${pct(share(g.flagged, g.machines))})`,
            g.machines_critical, g.replace ?? '—', g.repair ?? '—', num(g.median_score, 0), vsIdentical(g)],
        })),
      }),
    ]);
  }

  // 6. The charts, as the overview draws them, under the overview's own
  // heading for them.
  const overTime = el('section', { class: 'report-section' }, [el('div', { class: 'grid' }, overTimeCards(timeline))]);

  main.append(el('article', { class: 'report' }, [
    head, toolbar, summary, changed, findings, lifecycle, groups, overTime,
    el('p', {
      class: 'report-foot',
      text: `Generated ${now.toISOString().replace(/\.\d+Z$/, 'Z')} by loadbearer-fleet ${snap.version}${build}`
        + ` from ${source}. The filter and the sign-in scope apply to every figure; cohort medians are drawn `
        + 'from the whole fleet.',
    }),
  ]));
}

/*
 * Charts carry resolved colours, so a dark-themed page would print light ink
 * on white paper. Print is always the light theme: switched just before the
 * browser lays out the pages and put back after, with a redraw each way. The
 * stylesheet forces the light surface; this is what reaches the marks.
 */
let themeBeforePrint;
window.addEventListener('beforeprint', () => {
  themeBeforePrint = document.documentElement.dataset.theme;
  document.documentElement.dataset.theme = 'light';
  for (const e of redraws) if (e.host.isConnected && e.mode === 'chart') e.render();
});
window.addEventListener('afterprint', () => {
  if (themeBeforePrint) document.documentElement.dataset.theme = themeBeforePrint;
  else delete document.documentElement.dataset.theme;
  for (const e of redraws) if (e.host.isConnected && e.mode === 'chart') e.render();
});

/* -------------------------------------------------------------------- boot */

function render() {
  const main = $('#main');
  redraws.length = 0;
  main.replaceChildren();
  for (const tab of document.querySelectorAll('.tab')) {
    const on = tab.dataset.view === state.view
      || (state.view === 'machine' && tab.dataset.view === 'machines')
      || (state.view === 'report' && tab.dataset.view === 'overview');
    if (on) tab.setAttribute('aria-current', 'page'); else tab.removeAttribute('aria-current');
  }
  syncTabStrip(true);
  // Filters scope a fleet, so a drilldown of one machine hides them. The
  // comparison keeps them: its picker is built from the filtered snapshot, so
  // the filter is what decides which machines are on offer, and hiding the
  // control while it still applied would be the confusing half of both.
  $('#filters').hidden = state.view === 'machine';

  // The report states the scope in its own heading, where it prints.
  const notice = state.view === 'report' ? null : scopeNotice();
  if (notice) main.append(notice);

  if (state.view === 'machine') { renderMachine(main, state.key); return; }
  if (state.view === 'compare') { renderCompare(main, state.runs); return; }
  if (state.view === 'groups') return renderGroups(main);
  if (state.view === 'components') return renderComponents(main);
  if (state.view === 'report') return renderReport(main);
  if (state.view === 'lifecycle') renderLifecycle(main);
  else if (state.view === 'cohorts') renderCohorts(main);
  else if (state.view === 'machines') renderMachines(main);
  else return renderOverview(main);
}

async function route() {
  const {
    view, key, filter, runs, by, window: weeks, decision, reportWindow, ...rest
  } = parseHash();
  state.view = view;
  state.key = key;
  state.filter = filter;
  state.runs = runs;
  // Only the Groups view's own URL names one; elsewhere the last choice is
  // kept for the way back. The overview's window likewise.
  if (view === 'groups') state.by = by;
  if (view === 'overview') state.window = weeks;
  if (view === 'lifecycle') state.decision = decision;
  if (view === 'report') {
    state.reportWindow = reportWindow;
    state.reportBy = by;
  }
  if (view === 'components') Object.assign(state, rest);
  const main = $('#main');
  main.classList.add('loading');
  try {
    if (!state.me) {
      state.me = await fetchMe();
      applyIdentity();
    }
    state.snap = await fetchSnapshot();
    $('#generated').textContent = `indexed ${state.snap.summary.runs} run(s) · read ${when(state.snap.generated_at)}`;
    $('#f-count').textContent = `${state.snap.summary.machines} machine(s) in view`;
    // From the snapshot rather than baked into the page, so it names the build
    // that answered — including right after an upgrade, when the browser may
    // still be holding cached HTML from the old one.
    if (state.snap.version) {
      const build = state.snap.build && state.snap.build !== 'unknown' ? ` (${state.snap.build})` : '';
      $('#footer-build').textContent = `loadbearer-fleet ${state.snap.version}${build}`;
    }
    syncFilterControls();
    // Awaited so `loading` stays on until a view that fetches its own data
    // has drawn it, rather than lifting over a half-built page.
    await render();
  } catch (err) {
    main.replaceChildren(el('div', { class: 'banner' }, [el('span', { text: `Could not load the fleet: ${err.message}` })]));
  } finally {
    main.classList.remove('loading');
  }
}

// Guarded so this module can be imported by the geometry check, which mounts
// no page. In the browser #main is always there.
if ($('#main')) {
  wireFilters();
  window.addEventListener('hashchange', route);
  route();
}
