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
  root.append(svg('text', {
    x: zero, y: height - 8, 'text-anchor': 'middle', fill: ink('--text-muted'), 'font-size': 10,
  }, [document.createTextNode('cohort median')]));
  host.append(root);
}

/** Horizontal stacked bars: a 2px surface gap does the separating, not a stroke. */
export function stackedChart(host, { rows, series }) {
  const width = Math.max(320, host.clientWidth || 520);
  const gutter = Math.min(150, Math.max(80, ...rows.map((r) => textWidth(r.label, 12) + 12)));
  const m = { top: 6, right: 46, bottom: 6, left: gutter };
  const rowH = 34;
  const height = rowH * rows.length + m.top + m.bottom;
  const plotW = width - m.left - m.right;
  const max = Math.max(1, ...rows.map((r) => series.reduce((a, s) => a + (r.values[s.key] || 0), 0)));
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
      if (w > textWidth(v, 11) + 14) {
        root.append(svg('text', {
          x: cursor + w / 2, y: cy + 4, 'text-anchor': 'middle',
          fill: '#0b0b0b', 'font-size': 11, 'font-weight': 600,
          'font-variant-numeric': 'tabular-nums',
        }, [document.createTextNode(nf0.format(v))]));
      }
      root.append(hoverable(svg('rect', {
        x: cursor, y: cy - rowH / 2, width: Math.max(w, 8), height: rowH,
        fill: 'transparent', role: 'img', 'aria-label': `${r.label}, ${s.label}: ${v}`,
      }), () => tipBlock(`${r.label} · ${s.label}`, [['findings', nf0.format(v), s.colour]])));
      cursor += raw;
    });

    root.append(svg('text', {
      x: m.left + plotW + 10, y: cy + 4, fill: ink('--text-secondary'),
      'font-size': 11, 'font-variant-numeric': 'tabular-nums',
    }, [document.createTextNode(nf0.format(total))]));
  });
  host.append(root);
}

/**
 * Score over time. A crosshair finds the X so the reader aims at a date rather
 * than at a 2px line.
 */
export function lineChart(host, { points }) {
  const width = Math.max(320, host.clientWidth || 520);
  const m = { top: 20, right: 54, bottom: 34, left: 46 };
  const plotH = 180;
  const height = plotH + m.top + m.bottom;
  const plotW = width - m.left - m.right;
  const times = points.map((p) => +new Date(p.taken_at));
  const t0 = Math.min(...times);
  const t1 = Math.max(...times);
  const { max, ticks } = niceTicks(Math.max(...points.map((p) => p.score || 0)), 4, 1);
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

  const comparable = points.filter((p) => p.comparable && p.score !== null);
  const path = comparable.map((p, i) => `${i ? 'L' : 'M'}${x(+new Date(p.taken_at))},${y(p.score)}`).join('');
  if (comparable.length > 1) {
    root.append(svg('path', {
      d: path, fill: 'none', stroke: ink('--series-1'), 'stroke-width': 2,
      'stroke-linejoin': 'round', 'stroke-linecap': 'round',
    }));
  }

  points.forEach((p) => {
    if (p.score === null) return;
    // Runs measured under another configuration are context, not the series:
    // the emphasis pattern rather than a second categorical hue.
    const colour = p.comparable ? ink('--series-1') : ink('--deemph');
    root.append(svg('circle', {
      cx: x(+new Date(p.taken_at)), cy: y(p.score), r: 4.5, fill: colour,
      stroke: ink('--surface-1'), 'stroke-width': 2,
    }));
  });

  const last = points[points.length - 1];
  if (last && last.score !== null) {
    root.append(svg('text', {
      x: Math.min(x(+new Date(last.taken_at)) + 10, width - 4), y: y(last.score) + 4,
      fill: ink('--text-secondary'), 'font-size': 11, 'font-variant-numeric': 'tabular-nums',
    }, [document.createTextNode(nf0.format(last.score))]));
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
      const d = Math.abs(x(+new Date(p.taken_at)) - px);
      if (d < bestD) { bestD = d; best = p; }
    }
    return best;
  };
  const readout = (p) => tipBlock(when(p.taken_at), [
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
    hair.setAttribute('x1', x(+new Date(p.taken_at)));
    hair.setAttribute('x2', x(+new Date(p.taken_at)));
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

/* ------------------------------------------------------------ chart cards */

const redraws = [];

export function legend(items) {
  return el('div', { class: 'legend' }, items.map((it) => el('span', { class: 'legend-item' }, [
    Object.assign(el('span', { class: `legend-key${it.line ? ' line' : ''}` }), { style: `background:${it.colour}` }),
    el('span', { text: it.label }),
  ])));
}

export function dataTable({ columns, rows, onRow, sortable }) {
  const state = { key: null, dir: 1 };
  const wrap = el('div', { class: 'table-wrap' });
  const table = el('table', { class: 'data' });

  const draw = () => {
    let body = rows;
    if (state.key !== null) {
      const i = state.key;
      body = rows.slice().sort((a, b) => {
        const av = a.sort?.[i] ?? a.cells[i];
        const bv = b.sort?.[i] ?? b.cells[i];
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

  const card = el('div', { class: `card ${span}` }, [
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

// Exported, with the view functions, so `scripts/check-ui.mjs` can render every
// view against a real snapshot outside a browser. That check is the only thing
// standing between a typo in here and a blank panel in production.
export const state = {
  snap: null, me: null, view: 'overview', key: null, filter: {}, showInfo: false,
};

function parseHash() {
  const raw = location.hash.replace(/^#\/?/, '');
  const [path, query] = raw.split('?');
  const parts = (path || 'overview').split('/');
  const params = new URLSearchParams(query || '');
  const filter = {};
  for (const k of ['since_days', 'tags', 'flag', 'q', 'cohort']) {
    const v = params.get(k);
    if (v) filter[k] = v;
  }
  return {
    view: parts[0] === 'machine' ? 'machine' : (['overview', 'cohorts', 'machines'].includes(parts[0]) ? parts[0] : 'overview'),
    key: parts[0] === 'machine' ? decodeURIComponent(parts.slice(1).join('/')) : null,
    filter,
  };
}

function writeHash({ view = state.view, key = state.key, filter = state.filter } = {}) {
  const q = new URLSearchParams(filter).toString();
  const path = view === 'machine' ? `machine/${encodeURIComponent(key)}` : view;
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

  $('#rescan').addEventListener('click', async () => {
    const btn = $('#rescan');
    btn.disabled = true;
    btn.textContent = 'Scanning…';
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
      btn.textContent = 'Rescan folder';
    }
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

function findingRow(f, machinesByKey) {
  const m = machinesByKey.get(f.machine_key);
  return el('div', { class: 'finding-list' }, [el('div', { class: 'finding' }, [
    el('div', {}, [
      el('div', { class: 'finding-head' }, [
        severityChip(f.severity),
        m ? machineLink(m) : el('span', { class: 'finding-who', text: f.hostname || f.machine_key }),
        el('span', { class: 'finding-who', text: f.headline }),
        el('span', { class: 'finding-queue', text: QUEUES[f.kind] || f.kind }),
      ]),
      el('p', { class: 'finding-detail', text: f.detail }),
    ]),
  ])]);
}

/* ------------------------------------------------------------------- views */

export function renderOverview(main) {
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
    tile('Stale', nf0.format(s.stale), `not measured in ${nf0.format(state.snap.thresholds.stale_days)} days`),
    tile('Weak identity', nf0.format(s.weak_identity), 'identifier resets on reimage'),
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
      sev.key, state.snap.flags.filter((f) => f.kind === k && f.severity === sev.key).length,
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

  const shown = state.snap.flags.filter((f) => state.showInfo || f.severity !== 'info');
  const list = el('div', { class: 'card col-6' }, [
    el('div', { class: 'card-head' }, [
      el('h3', { text: 'What to look at' }),
      el('div', { class: 'card-actions' }, [
        el('button', {
          type: 'button', class: 'btn btn-quiet',
          text: state.showInfo ? 'Hide informational' : `Show ${s.info} informational`,
          onclick: () => { state.showInfo = !state.showInfo; render(); },
        }),
      ]),
    ]),
    el('p', { class: 'card-sub', text: 'Most severe first. Every finding says what it is evidence of and what usually causes it.' }),
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

  main.append(el('div', { class: 'grid' }, [
    hero, tiles, gradeCard, findingsCard, spreadCard, list, configs,
  ]));
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

export function renderMachines(main) {
  const flagCount = new Map();
  for (const f of state.snap.flags) {
    if (f.severity === 'info') continue;
    flagCount.set(f.machine_key, (flagCount.get(f.machine_key) || 0) + 1);
  }
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
      ago(m.age_days),
      Object.entries(m.tags || {}).map(([k, v]) => `${k}=${v}`).join(' ') || '—',
    ],
    sort: [m.hostname || m.key, m.cpu_model, m.score, m.grade, m.cohort_delta_pct, m.trend_pct,
      flagCount.get(m.key) || 0, -(m.age_days ?? 1e9), null],
  }));

  main.append(el('div', { class: 'grid' }, [el('div', { class: 'card col-12' }, [
    el('div', { class: 'card-head' }, [el('h3', { text: `${state.snap.summary.machines} machine(s)` })]),
    el('p', { class: 'card-sub', text: 'Click a column to sort, or a machine to drill in.' }),
    dataTable({
      columns: [{ label: 'Machine' }, { label: 'CPU' }, { label: 'Score', num: true }, { label: 'Grade' },
        { label: 'vs peers', num: true }, { label: 'vs own history', num: true },
        { label: 'Findings', num: true }, { label: 'Measured' }, { label: 'Tags' }],
      rows,
      sortable: true,
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

  const facts = [
    ['CPU', m.cpu_model],
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
        ...fs.map((f) => el('div', { class: 'finding' }, [el('div', {}, [
          el('div', { class: 'finding-head' }, [
            severityChip(f.severity), el('span', { class: 'finding-who', text: f.headline }),
          ]),
          el('p', { class: 'finding-detail', text: f.detail }),
        ])])),
      ]))
      : el('div', { class: 'empty', text: 'Nothing flagged on this machine.' }),
  ]);

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
    el('div', { class: 'grid' }, [head, heroCard, historyCard, componentCard, flagCard, measurementCard]),
  );
}

/* -------------------------------------------------------------------- boot */

function render() {
  const main = $('#main');
  redraws.length = 0;
  main.replaceChildren();
  for (const tab of document.querySelectorAll('.tab')) {
    const on = tab.dataset.view === state.view
      || (state.view === 'machine' && tab.dataset.view === 'machines');
    if (on) tab.setAttribute('aria-current', 'page'); else tab.removeAttribute('aria-current');
  }
  // Filters scope a fleet, not one machine.
  $('#filters').hidden = state.view === 'machine';

  const notice = scopeNotice();
  if (notice) main.append(notice);

  if (state.view === 'machine') { renderMachine(main, state.key); return; }
  if (state.view === 'cohorts') renderCohorts(main);
  else if (state.view === 'machines') renderMachines(main);
  else renderOverview(main);
}

async function route() {
  const { view, key, filter } = parseHash();
  state.view = view;
  state.key = key;
  state.filter = filter;
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
    syncFilterControls();
    render();
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
