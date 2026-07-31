/* entropyx survey — sheet controller.
 *
 * Three responsibilities, in order of how the screen fills:
 *   1. consume the bridge's SSE stream and render phase telemetry;
 *   2. lay the streamed rows out as terrain (squarified treemap);
 *   3. hand the seven axes to the WebAssembly scoring kernel so weights
 *      can be re-calibrated without going back to the server.
 */

import initKernel, {
  Kernel, defaultWeights, sumPositive, classNames, metricColumns, contractVersion,
} from './pkg/exkernel.js';

const $ = (id) => document.getElementById(id);

const AXES = [
  { key: 'change_density',      sym: 'Dₙ', short: 'change', name: 'How much it changes', blurb: 'how much change this file takes on' },
  { key: 'author_entropy',      sym: 'Hₐ', short: 'people', name: 'How many people',     blurb: 'how many different people have shaped it' },
  { key: 'temporal_volatility', sym: 'Vₜ', short: 'bursts', name: 'How bursty',          blurb: 'steady work, or sudden panics' },
  { key: 'coupling_stress',     sym: 'Cₛ', short: 'blast', name: 'Blast radius',        blurb: 'how much else moves when this moves' },
  { key: 'blame_youth',         sym: 'Bᵧ', short: 'new', name: 'How new the code is', blurb: 'how much of it was written recently' },
  { key: 'semantic_drift',      sym: 'Sₙ', short: 'interface', name: 'Interface change',    blurb: 'how much of what other code depends on has moved' },
  { key: 'test_cooevolution',   sym: 'Tᶜ', short: 'tests', name: 'Tests keeping up',    blurb: 'lowers the score — change with tests is healthier change', discount: true },
];

const CLASS_LABEL = {
  refactor_convergence: 'Refactor convergence',
  api_drift: 'API drift',
  ownership_fragmentation: 'Ownership fragmentation',
  incident_aftershock: 'Incident aftershock',
  coupled_amplifier: 'Coupled amplifier',
  frozen_neglect: 'Frozen neglect',
};

const EVENT_LABEL = {
  hotspot: 'Hotspot',
  ownership_split: 'Ownership split',
  api_drift: 'API drift',
  rename: 'Rename',
  incident_aftershock: 'Incident aftershock',
};

/* Cells beyond this count stop earning their DOM node. The overflow is
 * stated on the sheet rather than silently dropped. */
const MAX_CELLS = 700;
const BANDS = 7;

const state = {
  dict: null,
  rows: [],
  events: [],
  handles: {},
  kernel: null,
  kernelReady: false,
  weights: null,
  incident: null,
  layout: [],
  selectedEl: null,
  handleByFile: null,
  renamedSet: null,
  evidenceAbort: null,
  scores: null,
  classCodes: null,
  classLookup: null,
  bands: null,
  shownIdx: null,
  phaseMs: {},
  divergence: null,
  people: null,
  cached: false,
  selected: null,
  source: null,
  repo: null,
  t0: 0,
};

/* ── Bootstrapping ─────────────────────────────────────────────────── */

async function boot() {
  drawGutter();
  buildRamp();
  buildClassList();
  buildWeightControls();

  try {
    await initKernel();
    state.kernel = new Kernel();
    state.weights = Array.from(defaultWeights());
    state.kernelReady = true;
    const cols = metricColumns();
    $('kernel-note').textContent =
      `ready · entropyx ${contractVersion()} · ${cols.length} measurements per file`;
    syncWeightControls();
  } catch (e) {
    state.kernelReady = false;
    $('kernel-note').textContent =
      `Live re-scoring is unavailable (${e.message}). The scores below are the ones the scan produced.`;
    $('calib').setAttribute('data-disabled', 'true');
  }

  await loadRepoList();

  $('scan-form').addEventListener('submit', (e) => { e.preventDefault(); startScan(); });
  $('abort-btn').addEventListener('click', abortScan);
  $('reset-weights').addEventListener('click', () => {
    state.weights = Array.from(defaultWeights());
    syncWeightControls();
    rescore();
  });
  $('log-toggle').addEventListener('click', toggleLog);
  $('terrain').addEventListener('keydown', onTerrainKey);
  bindMarkCallout();
  window.addEventListener('resize', debounce(() => { if (state.rows.length) renderTerrain(); }, 180));

  // The brief's "show me" links arrive here with the repository already
  // chosen; a cached scan makes the hand-off feel instant.
  const preset = new URLSearchParams(location.search).get('repo');
  if (preset) { $('repo').value = preset; startScan(); }
}

async function loadRepoList() {
  try {
    const r = await fetch('/api/repos');
    const j = await r.json();
    const list = $('repo-list');
    list.innerHTML = '';
    for (const repo of j.repos || []) {
      const o = document.createElement('option');
      o.value = repo.path;
      o.label = repo.name;
      list.appendChild(o);
    }
    if (j.repos?.length) {
      $('sheet-sub').textContent =
        `${j.repos.length} repositories found under ${j.root}. Pick one, or type any path.`;
    }
  } catch {
    $('sheet-sub').textContent = 'Bridge unreachable. Start exbridge, then reload.';
  }
}

/* ── Scan lifecycle ────────────────────────────────────────────────── */

function startScan() {
  abortScan();
  resetSheet();

  const repo = $('repo').value.trim();
  if (!repo) return;
  state.repo = repo;
  state.t0 = performance.now();

  const params = new URLSearchParams({ repo });
  if ($('fresh').checked) params.set('fresh', 'true');
  const since = $('since').value.trim();
  if (since) params.set('since', since);

  $('blank').hidden = true;
  $('log').hidden = false;
  $('survey-btn').disabled = true;
  $('abort-btn').hidden = false;
  $('log-note').textContent = 'Opening sheet…';
  $('sheet-title').textContent = repo.split('/').filter(Boolean).pop() || repo;
  $('to-brief').href = `./brief.html?repo=${encodeURIComponent(repo)}`;

  const es = new EventSource(`/api/scan?${params}`);
  state.source = es;

  es.addEventListener('meta', (e) => onMeta(JSON.parse(e.data)));
  es.addEventListener('phase', (e) => onPhase(JSON.parse(e.data)));
  es.addEventListener('tick', (e) => onTick(JSON.parse(e.data)));
  es.addEventListener('cached', () => {
    // No phases will fire, so the queued list would sit unresolved forever.
    state.cached = true;
    $('log-list').innerHTML = '';
    $('log-note').textContent =
      'Already measured at this commit, so nothing needed re-running. '
      + 'Tick "re-run even if cached" to measure it again and watch the steps go by.';
  });
  es.addEventListener('dict', (e) => {
    state.dict = JSON.parse(e.data);
    state.authorSet = new Set(state.dict.authors.map((a) => a.toLowerCase()));
  });
  es.addEventListener('rows', (e) => onRows(JSON.parse(e.data)));
  es.addEventListener('events', (e) => onEvents(JSON.parse(e.data)));
  es.addEventListener('handles', (e) => {
    state.handles = JSON.parse(e.data);
    indexHandles();
  });
  es.addEventListener('done', (e) => onDone(JSON.parse(e.data)));
  es.addEventListener('error', (e) => {
    // Distinguish a server-sent `error` event from a transport drop.
    if (e.data) { onError(JSON.parse(e.data).message); }
    else if (es.readyState === EventSource.CLOSED) { onError('Connection to the bridge closed.'); }
  });
}

function abortScan() {
  if (state.source) { state.source.close(); state.source = null; }
  $('survey-btn').disabled = false;
  $('abort-btn').hidden = true;
}

function resetSheet() {
  state.dict = null; state.rows = []; state.events = []; state.handles = {};
  state.layout = []; state.selected = null; state.incident = null;
  state.scores = null; state.classCodes = null; state.bands = null;
  state.phaseMs = {}; state.cached = false; state.shownIdx = null;
  state.divergence = null; state.people = null; state.handleByFile = null;
  state.renamedSet = null; state.evidenceAbort?.abort(); state.evidenceAbort = null;
  $('log-list').innerHTML = '';
  $('terrain').innerHTML = '';
  state.selectedEl = null;
  $('timeline').innerHTML = '';
  $('terrain-wrap').hidden = true;
  $('calib').hidden = true;
  $('marks').hidden = true;
  $('diverge').hidden = true;
  $('people').hidden = true;
  $('marginalia').hidden = true;
  $('truncation').hidden = true;
  clearRail();
  $('log').removeAttribute('data-collapsed');
  $('log-toggle').setAttribute('aria-expanded', 'true');
  $('log-toggle').textContent = 'Collapse';
}

function onMeta(m) {
  $('marginalia').hidden = false;
  $('m-head').textContent = m.head ? m.head.slice(0, 12) : 'unborn';
  $('sheet-sub').textContent = m.repo;
  const list = $('log-list');
  list.innerHTML = '';
  m.phases.forEach((p, i) => {
    const li = document.createElement('li');
    li.className = 'phase' + (p.weight >= 0.15 ? ' phase--dominant' : '');
    li.id = `phase-${p.id}`;
    li.dataset.status = 'queued';
    li.style.animationDelay = `${i * 22}ms`;
    li.innerHTML =
      `<span class="phase__name">${p.id}</span>` +
      `<span class="phase__bar"><span class="phase__fill"></span></span>` +
      `<span class="phase__ms">·</span>` +
      `<span class="phase__pct"></span>` +
      `<span class="phase__detail"></span>`;
    li.title = p.label;
    list.appendChild(li);
  });
  $('log-note').textContent =
    'Two steps take almost all the time: working out which files change together, and reading '
    + 'each file\'s history. Which one dominates varies by repository — the bars show the split.';
}

function onPhase(p) {
  const li = $(`phase-${p.id}`);
  if (!li) return;
  if (p.status === 'start') {
    li.dataset.status = 'running';
    li.querySelector('.phase__fill').style.width = '100%';
    li.querySelector('.phase__ms').textContent = '…';
    return;
  }
  li.dataset.status = 'done';
  li.querySelector('.phase__ms').textContent = fmtMs(p.elapsed_ms);
  const d = p.detail || {};
  li.querySelector('.phase__detail').textContent =
    Object.entries(d).map(([k, v]) => `${k} ${fmtNum(v)}`).join('  ');

  state.phaseMs[p.id] = p.elapsed_ms;
  redrawPhaseBars();
}

/* A completed phase's bar is its share of elapsed time, rescaled as each
 * new phase lands. Every bar at 100% would say nothing; this makes the
 * betweenness phase visibly swallow the scan, which is the single most
 * useful thing this screen can teach. */
function redrawPhaseBars() {
  const total = Object.values(state.phaseMs).reduce((a, b) => a + b, 0);
  if (!total) return;
  for (const [id, ms] of Object.entries(state.phaseMs)) {
    const li = $(`phase-${id}`);
    if (!li || li.dataset.status !== 'done') continue;
    // Floor at a hairline so a sub-millisecond phase still reads as run,
    // not as skipped.
    const share = ms / total;
    li.querySelector('.phase__fill').style.width = `${Math.max(1.5, share * 100).toFixed(2)}%`;
    li.dataset.share = share >= 0.5 ? 'dominant' : 'minor';
    li.querySelector('.phase__pct').textContent = share >= 0.01 ? `${Math.round(share * 100)}%` : '';
  }
}

function onTick(t) {
  const li = $(`phase-${t.id}`);
  if (!li || !t.total) return;
  const pct = Math.min(100, (t.done / t.total) * 100);
  li.querySelector('.phase__fill').style.width = `${pct}%`;
  li.querySelector('.phase__ms').textContent = `${Math.round(pct)}%`;
  li.querySelector('.phase__detail').textContent = `${fmtNum(t.done)} / ${fmtNum(t.total)}`;
}

function onRows(chunk) {
  state.rows.push(...chunk.rows);
  if (state.rows.length >= chunk.total) {
    $('terrain-wrap').hidden = false;
    if (state.kernelReady) loadKernel();
    renderTerrain();
    renderClassCounts();
    $('m-files').textContent = fmtNum(chunk.total);
    $('m-authors').textContent = fmtNum(state.dict?.authors?.length ?? 0);
  }
}

function onEvents(chunk) {
  state.events.push(...chunk.events);
  if (state.events.length >= chunk.total) {
    $('m-events').textContent = fmtNum(chunk.total);
    renderTimeline();
  }
}

function onDone(d) {
  abortScan();
  $('m-time').textContent = d.cached
    ? `cached (${fmtMs(d.original_elapsed_ms)})`
    : fmtMs(d.elapsed_ms);
  $('m-digest').textContent = (d.digest || '').slice(0, 16) || '—';
  $('m-digest').title =
    `blake3 of the tq1 summary: ${d.digest}\nentropyx is deterministic — the same repository at the same HEAD always produces this digest.`;
  if (!state.events.length) { $('m-events').textContent = '0'; }
  renderTimeline();
  loadDivergence();
  loadPeople();
  $('log-note').textContent = d.cached
    ? 'Reused an earlier measurement of this commit. Tick "re-run even if cached" to measure it again.'
    : `Survey complete in ${fmtMs(d.elapsed_ms)}.`;
}

function onError(message) {
  abortScan();
  $('log-note').innerHTML = '';
  const p = document.createElement('p');
  p.className = 'err';
  p.textContent = message;
  $('log-note').replaceWith(p);
  p.id = 'log-note';
}

/* ── WebAssembly kernel ────────────────────────────────────────────── */

/* Files carrying at least one incident-tagged commit. Not derivable from
 * the seven axes, so the aftershock override needs it passed in. */
function incidentFlags() {
  const flagged = new Set(
    state.events.filter((e) => e.kind === 'incident_aftershock').map((e) => e.file),
  );
  return Uint8Array.from(state.rows, (r) => (flagged.has(r.file) ? 1 : 0));
}

function loadKernel() {
  const vals = new Float64Array(state.rows.length * 8);
  state.rows.forEach((r, i) => vals.set(r.values, i * 8));
  state.incident = incidentFlags();
  try {
    state.kernel.load(vals, state.incident);
    state.kernel.rescore(Float64Array.from(state.weights));
    cacheKernelOutput();
    $('calib').hidden = false;
  } catch (e) {
    state.kernelReady = false;
    $('kernel-note').textContent = `Live re-scoring could not start: ${e.message}`;
  }
}

/* Pull the kernel's output across the wasm boundary exactly once per
 * rescore. `composites()` and `classes()` each copy the whole array, so
 * calling them per cell would make every retint O(n²) — at 4,000 files
 * that is 16M copies for one slider nudge. */
function cacheKernelOutput() {
  if (state.kernelReady && state.kernel.len === state.rows.length) {
    state.scores = state.kernel.composites();
    state.classCodes = state.kernel.classes();
    state.classLookup = classNames();
  } else {
    state.scores = null;
    state.classCodes = null;
  }
}

function scoreOf(i) {
  return state.scores ? state.scores[i] : state.rows[i].values[7];
}

function classOf(i) {
  if (!state.classCodes) return state.rows[i].signal_class ?? null;
  const code = state.classCodes[i];
  return code ? state.classLookup[code] : null;
}

let rescoreQueued = false;
function rescore() {
  if (!state.kernelReady || !state.rows.length) return;
  if (rescoreQueued) return;
  rescoreQueued = true;
  requestAnimationFrame(() => {
    rescoreQueued = false;
    const t = performance.now();
    state.kernel.rescore(Float64Array.from(state.weights));
    cacheKernelOutput();
    computeBands();
    renderRampScale();
    retintTerrain();
    renderClassCounts();
    if (state.selected !== null) renderRail(state.selected);
    const ms = performance.now() - t;
    $('kernel-note').textContent =
      `re-scored ${fmtNum(state.rows.length)} files in ${ms < 1 ? 'under 1' : ms.toFixed(1)} ms`;
  });
}

/* ── Terrain: squarified treemap ───────────────────────────────────── */

/* Area encodes change density; a floor keeps zero-density files visible
 * rather than collapsing them to nothing. */
function areaOf(row) { return row.values[0] + 0.02; }

function renderTerrain() {
  const host = $('terrain');
  const W = host.clientWidth, H = host.clientHeight;
  if (!W || !H) return;

  const ranked = state.rows
    .map((r, i) => ({ i, a: areaOf(r) }))
    .sort((x, y) => y.a - x.a || x.i - y.i);

  // Cell budget scales with the drawing area. A fixed 700 is right on a
  // desktop sheet and unreadable on a phone, where the same cells would
  // be four pixels across.
  const budget = Math.max(80, Math.min(MAX_CELLS, Math.floor((W * H) / 1000)));
  const shown = ranked.slice(0, budget);
  state.shownIdx = shown.map((x) => x.i);
  computeBands();
  renderRampScale();

  const trunc = $('truncation');
  if (ranked.length > shown.length) {
    trunc.hidden = false;
    trunc.textContent =
      `Showing the ${fmtNum(shown.length)} busiest files of ${fmtNum(ranked.length)}. ` +
      `The other ${fmtNum(ranked.length - shown.length)} were measured too, but are not drawn here.`;
  } else {
    trunc.hidden = true;
  }

  const total = shown.reduce((s, x) => s + x.a, 0) || 1;
  const scale = (W * H) / total;
  state.layout = squarify(shown.map((x) => ({ ...x, v: x.a * scale })), 0, 0, W, H);

  host.innerHTML = '';
  const frag = document.createDocumentFragment();
  state.layout.forEach((box, n) => {
    frag.appendChild(makeCell(box, n));
  });
  host.appendChild(frag);
}

function makeCell(box, n) {
  const row = state.rows[box.i];
  const path = state.dict.files[row.file];
  const el = document.createElement('button');
  el.type = 'button';
  el.className = 'cell';
  el.setAttribute('role', 'listitem');
  el.dataset.i = String(box.i);
  el.style.cssText =
    `left:${box.x}px;top:${box.y}px;width:${box.w}px;height:${box.h}px;--d:${Math.min(n * 4, 900)}ms`;
  const tiny = box.w < 46 || box.h < 22;
  if (tiny) el.dataset.tiny = 'true';

  const label = document.createElement('span');
  label.className = 'cell__label';
  label.textContent = shortPath(path, box.w, box.h);
  el.appendChild(label);

  const sym = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  sym.setAttribute('class', 'cell__sym');
  sym.setAttribute('aria-hidden', 'true');
  el.appendChild(sym);

  el.addEventListener('click', () => select(box.i));
  paintCell(el, box.i, path, box);
  return el;
}

/* Repaint one cell, touching the DOM only where something actually
 * changed. A slider nudge moves a minority of files across a band
 * boundary; rewriting all 509 cells — in particular re-creating each
 * `<use>` element — cost 6ms of the frame for no visible difference. */
function paintCell(el, i, path, box) {
  const c = scoreOf(i);
  const cls = classOf(i);
  const band = bandOf(c);
  const div = state.divergence ? divergenceOf(i) : null;
  const diverges = div?.deviant_keys.length ? 'true' : 'false';

  const prev = el._paint;
  const sameBand = prev && prev.band === band;
  const sameClass = prev && prev.cls === cls;
  const sameDiv = prev && prev.diverges === diverges;
  if (sameBand && sameClass && sameDiv && prev.score === c) return;

  if (!sameBand) {
    el.dataset.band = String(band);
    el.style.backgroundColor = `var(--el-${band})`;
  }
  if (!sameDiv) el.dataset.diverges = diverges;

  const divNote = div?.deviant_keys.length
    ? ` Differs from ${div.fleet_label} on ${div.deviant_keys.join(', ')}.` : '';
  el.setAttribute('aria-label',
    `${path}. Score ${c.toFixed(3)}.${cls ? ` ${CLASS_LABEL[cls]}.` : ''}${divNote}`);
  el.title = `${path}\nscore ${c.toFixed(4)}${cls ? `\n${CLASS_LABEL[cls]}` : ''}${divNote}`;

  // Cell geometry comes from the layout we already computed. Reading it
  // back off the element would force a reflow per cell.
  const big = (box?.w ?? 0) >= 34 && (box?.h ?? 0) >= 26;
  const wantSym = cls && big;
  if (!sameClass || !sameBand || !prev) {
    const sym = el._sym || (el._sym = el.querySelector('.cell__sym'));
    if (wantSym) {
      if (!sameClass || !prev) sym.innerHTML = `<use href="#sym-${cls}"/>`;
      sym.style.color = band >= 6 ? 'var(--paper)' : `var(--c-${cls})`;
      sym.hidden = false;
    } else if (!prev || prev.cls) {
      sym.innerHTML = '';
      sym.hidden = true;
    }
  }

  el._paint = { band, cls, diverges, score: c };
}

function retintTerrain() {
  const kids = $('terrain').children;
  for (let n = 0; n < kids.length; n++) {
    const el = kids[n];
    const i = Number(el.dataset.i);
    paintCell(el, i, state.dict.files[state.rows[i].file], state.layout[n]);
  }
}

/* Contour interval.
 *
 * Composite scores cluster low on almost every repository — a linear
 * 0..1 ramp puts 90% of files in one tint and the map reads flat. A
 * printed survey sheet does not do this either: the cartographer picks a
 * contour interval that suits the terrain being drawn. So the bands are
 * quantiles of *this* repository's distribution, and the legend prints
 * the actual composite at every boundary so nothing is hidden by it. */
function computeBands() {
  // Band over the cells actually drawn, not the whole repository. The
  // terrain is truncated by change density, which correlates with the
  // composite, so quantiles of the full set would push the drawn subset
  // into the top two tints and flatten the map again. The legend states
  // which population the interval came from.
  const pop = state.shownIdx?.length ? state.shownIdx : state.rows.map((_, i) => i);
  const n = pop.length;
  if (!n) { state.bands = null; return; }
  const sorted = pop.map((i) => scoreOf(i)).sort((a, b) => a - b);
  const cuts = [];
  for (let b = 1; b < BANDS; b++) {
    cuts.push(sorted[Math.min(n - 1, Math.floor((b / BANDS) * n))]);
  }
  // Collapse duplicate cuts (a repo where most files score identically)
  // so two bands never claim the same value.
  for (let i = 1; i < cuts.length; i++) {
    if (cuts[i] <= cuts[i - 1]) cuts[i] = cuts[i - 1];
  }
  state.bands = { cuts, min: sorted[0], max: sorted[n - 1] };
}

function bandOf(c) {
  if (!state.bands) return Math.min(BANDS - 1, Math.floor(Math.max(0, Math.min(1, c)) * BANDS));
  const { cuts } = state.bands;
  let b = 0;
  while (b < cuts.length && c >= cuts[b]) b++;
  return Math.min(BANDS - 1, b);
}

function renderRampScale() {
  const el = $('ramp-scale');
  if (!state.bands) return;
  const { cuts, min, max } = state.bands;
  // Boundaries that round to the same printed value would collide into an
  // unreadable run of digits, so only the first of each run is labelled.
  const vals = [min, ...cuts, max].map((v) => v.toFixed(2));
  el.innerHTML = vals
    .map((v, i) => `<span>${i > 0 && v === vals[i - 1] ? '' : v}</span>`)
    .join('');
  const pop = state.shownIdx?.length || state.rows.length;
  $('ramp-note').textContent =
    `Bands are set from the ${fmtNum(pop)} files on the map, so each holds roughly ${Math.round(pop / BANDS)}.`;
}

/* Squarified treemap (Bruls, Huizing & van Wijk). Keeps cells close to
 * square so labels stay readable and area stays comparable by eye. */
function squarify(items, x, y, w, h) {
  const out = [];
  let rest = items.slice();

  while (rest.length) {
    const short = Math.min(w, h);
    if (short <= 0) break;
    let row = [];
    let best = Infinity;
    while (rest.length) {
      const cand = row.concat(rest[0]);
      const r = worstRatio(cand, short);
      if (r > best) break;
      best = r;
      row = cand;
      rest.shift();
    }
    if (!row.length) { row = [rest.shift()]; }

    const sum = row.reduce((s, it) => s + it.v, 0);
    const thick = short > 0 ? sum / short : 0;

    if (w >= h) {
      let cy = y;
      for (const it of row) {
        const ch = sum > 0 ? (it.v / sum) * h : 0;
        out.push({ i: it.i, x, y: cy, w: thick, h: ch });
        cy += ch;
      }
      x += thick; w -= thick;
    } else {
      let cx = x;
      for (const it of row) {
        const cw = sum > 0 ? (it.v / sum) * w : 0;
        out.push({ i: it.i, x: cx, y, w: cw, h: thick });
        cx += cw;
      }
      y += thick; h -= thick;
    }
  }
  return out;
}

function worstRatio(row, short) {
  const sum = row.reduce((s, it) => s + it.v, 0);
  if (sum <= 0) return Infinity;
  const side = sum / short;
  let worst = 0;
  for (const it of row) {
    const other = it.v / side;
    worst = Math.max(worst, Math.max(side / other, other / side));
  }
  return worst;
}

function shortPath(p, w, h) {
  const parts = p.split('/');
  const file = parts[parts.length - 1];
  if (h > 46 && w > 96 && parts.length > 1) return `${parts.slice(0, -1).join('/')}/\n${file}`;
  const cap = Math.max(6, Math.floor(w / 6));
  return file.length > cap ? `${file.slice(0, cap - 1)}…` : file;
}

/* ── Terrain keyboard navigation ───────────────────────────────────── */

function onTerrainKey(e) {
  if (!state.layout.length) return;
  const keys = ['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown'];
  if (!keys.includes(e.key)) return;
  e.preventDefault();

  const cur = state.selected;
  if (cur === null) { select(state.layout[0].i); return; }

  const from = state.layout.find((b) => b.i === cur);
  if (!from) return;
  const fx = from.x + from.w / 2, fy = from.y + from.h / 2;

  let best = null, bestD = Infinity;
  for (const b of state.layout) {
    if (b.i === cur) continue;
    const bx = b.x + b.w / 2, by = b.y + b.h / 2;
    const dx = bx - fx, dy = by - fy;
    const ok = { ArrowLeft: dx < -1, ArrowRight: dx > 1, ArrowUp: dy < -1, ArrowDown: dy > 1 }[e.key];
    if (!ok) continue;
    // Weight the off-axis distance so movement stays in the pressed
    // direction instead of drifting diagonally.
    const along = Math.abs(e.key === 'ArrowLeft' || e.key === 'ArrowRight' ? dx : dy);
    const across = Math.abs(e.key === 'ArrowLeft' || e.key === 'ArrowRight' ? dy : dx);
    const d = along + across * 2.5;
    if (d < bestD) { bestD = d; best = b; }
  }
  if (best) select(best.i);
}

/* ── Selection and the evidence rail ───────────────────────────────── */

function select(i) {
  if (state.selectedEl) state.selectedEl.setAttribute('aria-selected', 'false');
  state.selected = i;
  const el = $('terrain').querySelector(`.cell[data-i="${i}"]`);
  state.selectedEl = el ?? null;
  if (el) {
    el.setAttribute('aria-selected', 'true');
    el.focus({ preventScroll: true });
  }
  renderRail(i);
  fetchEvidence(i);
}

function clearRail() {
  $('rail-empty').hidden = false;
  $('rail-body').hidden = true;
}

function renderRail(i) {
  const row = state.rows[i];
  const path = state.dict.files[row.file];
  const cls = classOf(i);
  const c = scoreOf(i);

  $('rail-empty').hidden = true;
  $('rail-body').hidden = false;
  $('r-path').textContent = path;
  $('r-composite').textContent = c.toFixed(3);

  const kicker = $('r-class');
  if (cls) {
    kicker.innerHTML = `<svg aria-hidden="true"><use href="#sym-${cls}"/></svg><span>${CLASS_LABEL[cls]}</span>`;
    kicker.style.color = `var(--c-${cls})`;
  } else {
    kicker.innerHTML = '<span>Unclassified</span>';
    kicker.style.color = 'var(--ink-faint)';
  }

  const div = state.divergence ? divergenceOf(i) : null;
  $('r-diverge').hidden = !div;
  if (div) {
    $('r-drift').textContent = `${Math.round(div.drift * 100)}% unlike its peers`;
    $('r-fleet').textContent = div.deviant_keys.length
      ? `Differs from ${div.fleet_label} on ${div.deviant_keys.length} setting${div.deviant_keys.length === 1 ? '' : 's'}.`
      : `Matches ${div.fleet_label} on every setting the group disagrees about.`;
    $('r-deviants').innerHTML = div.deviant_keys
      .map((k) => `<li class="num">${escapeHtml(k)}</li>`).join('')
      + (div.faction_size ? `<li class="muted">${div.faction_size - 1} other file${div.faction_size === 2 ? '' : 's'} drifted the same way</li>` : '');
  }

  $('r-profile').innerHTML = AXES.map((a, n) => `
    <li${a.discount ? ' data-discount="true"' : ''} title="${a.name} — ${a.blurb}">
      <span class="profile__axis">${a.short}</span>
      <span class="profile__track"><span class="profile__bar" style="width:${(row.values[n] * 100).toFixed(1)}%"></span></span>
      <span class="profile__val">${row.values[n].toFixed(2)}</span>
    </li>`).join('');
}

/* Built once when handles arrive. This was a linear scan over every
 * handle on each selection — 2,210 entries on a mid-sized repo, walked
 * again on every arrow key. */
function indexHandles() {
  state.handleByFile = new Map();
  for (const [key, h] of Object.entries(state.handles)) {
    if (!state.handleByFile.has(h.file)) state.handleByFile.set(h.file, key);
  }
}

function handleFor(fileId) {
  return state.handleByFile?.get(fileId) ?? null;
}

async function fetchEvidence(i) {
  const row = state.rows[i];
  const box = $('r-evidence');
  const key = handleFor(row.file);
  $('r-handle').textContent = key || '';

  if (!key) {
    box.innerHTML =
      '<p class="muted">This file is no longer in the repository — it was renamed or deleted along the way. There are no current commits to show, but the measurements above still hold.</p>';
    return;
  }

  // Arrow-key navigation fires one of these per keypress. Abandon the
  // previous request rather than letting a slow one land after a newer
  // selection has already been painted.
  state.evidenceAbort?.abort();
  const ctl = new AbortController();
  state.evidenceAbort = ctl;

  box.innerHTML = '<p class="muted">Looking up the commits…</p>';
  try {
    const r = await fetch(
      `/api/explain?repo=${encodeURIComponent(state.repo)}&handle=${encodeURIComponent(key)}`,
      { signal: ctl.signal },
    );
    const j = await r.json();
    if (state.selected !== i) return;
    if (j.error) { box.innerHTML = `<p class="err">${escapeHtml(j.error)}</p>`; return; }

    const commits = j.commits || [];
    const authors = j.top_authors || [];
    if (!commits.length) {
      box.innerHTML = '<p class="muted">No commits found for this file. entropyx reports nothing rather than guessing.</p>';
      return;
    }
    box.innerHTML =
      `<ul>${commits.slice(0, 8).map((c) => `
        <li>
          <span class="commit__subject">${escapeHtml(c.subject)}</span>
          <span class="commit__meta">${c.sha.slice(0, 8)} · ${escapeHtml(c.author)} · ${fmtDate(c.time)}</span>
        </li>`).join('')}</ul>` +
      (commits.length > 8 ? `<p class="muted" style="margin-top:8px">+${commits.length - 8} more, ${j.commits_touched} in total</p>` : '') +
      (renamedFiles().has(state.rows[i].file)
        ? '<p class="muted" style="margin-top:8px">This file was renamed at some point. The history above follows it through the rename, which is what its score was measured from.</p>'
        : '') +
      (authors.length ? `<ul class="authors">${authors.map((a) => {
        const who = personFor(a.email);
        return `<li>
          <span>${who ? `<span class="authors__name">${escapeHtml(who.name)}</span>` : ''}${escapeHtml(a.email)}${
            who?.employer ? `<span class="authors__org num">${escapeHtml(who.employer)}</span>` : ''}</span>
          <span>${(a.share * 100).toFixed(0)}%</span></li>`;
      }).join('')}</ul>` : '');
  } catch (e) {
    if (e.name === 'AbortError') return;
    box.innerHTML = `<p class="err">Could not load the commits: ${escapeHtml(e.message)}</p>`;
  }
}

/* ── Divergence (WhatTheDiff) ──────────────────────────────────────── */

/* Fetched after the terrain is drawn, not folded into the scan stream.
 * It needs no graph work, so making a 12-minute scan wait for it would
 * be pure cost. */
async function loadDivergence() {
  try {
    const r = await fetch(`/api/fleets?repo=${encodeURIComponent(state.repo)}`);
    const j = await r.json();
    if (j.error) { state.divergence = null; return; }
    state.divergence = j;
    renderDivergence();
    retintTerrain();
    if (state.selected !== null) renderRail(state.selected);
  } catch {
    state.divergence = null;
  }
}

function divergenceOf(i) {
  const path = state.dict?.files[state.rows[i].file];
  return state.divergence?.by_path?.[path] ?? null;
}

function renderDivergence() {
  const d = state.divergence;
  if (!d) return;
  $('diverge').hidden = false;

  const withConflicts = d.fleets.filter((f) => f.report.conflicts.length);
  const deviating = Object.values(d.by_path).filter((v) => v.deviant_keys.length).length;

  $('diverge-note').textContent = d.fleets.length
    ? `Compared ${d.fleets.length} group${d.fleets.length === 1 ? '' : 's'} of similar files. `
      + `${deviating} file${deviating === 1 ? ' uses a setting that differs' : 's use a setting that differs'} from the rest.`
    : 'No groups of similar files to compare. This check needs at least two files that are versions of the same thing — one config per service, one manifest per package.';

  $('diverge-body').innerHTML = withConflicts.length
    ? withConflicts.map(fleetBlock).join('')
    : d.fleets.length
      ? '<p class="muted">Every group agrees on every setting they have in common. That is the result you want.</p>'
      : '';

  // Rejected candidates are shown, not hidden — the absence of a fleet
  // should be explainable.
  const rejected = d.rejected || [];
  $('diverge-excluded').hidden = rejected.length === 0;
  $('diverge-excluded-list').innerHTML = rejected
    .map((r) => `<li><span class="num">${escapeHtml(r.label)}</span> — ${escapeHtml(r.reason)}</li>`)
    .join('');

  $('diverge-body').querySelectorAll('[data-path]').forEach((el) => {
    el.addEventListener('click', () => {
      const idx = state.rows.findIndex((row) => state.dict.files[row.file] === el.dataset.path);
      if (idx >= 0) { select(idx); $('terrain').scrollIntoView({ behavior: 'smooth', block: 'center' }); }
    });
  });
}

function fleetBlock(f) {
  const r = f.report;
  const paths = r.artifacts.map((a) => a.path);
  const suppressed = f.identifier_keys_suppressed
    ? ` · ignored ${f.identifier_keys_suppressed} field${f.identifier_keys_suppressed === 1 ? '' : 's'} that just hold ids`
    : '';
  const truncated = f.truncated ? ` · ${fmtNum(f.truncated)} more not compared` : '';

  return `<article class="fleet">
    <header class="fleet__head">
      <h3 class="fleet__name num">${escapeHtml(f.label)}</h3>
      <span class="fleet__meta">${r.conflicts.length} setting${r.conflicts.length === 1 ? '' : 's'} they disagree on${suppressed}${truncated}</span>
    </header>
    <ul class="conflicts">
      ${r.conflicts.map((c) => conflictRow(c, paths)).join('')}
    </ul>
  </article>`;
}

function conflictRow(c, paths) {
  const majority = c.values.reduce((a, b) => (b.count > a.count ? b : a));
  const total = c.values.reduce((n, v) => n + v.count, 0);
  const minorities = c.values.filter((v) => v !== majority);
  const deviantPaths = minorities.flatMap((v) => v.artifacts.map((i) => ({ v, path: paths[i] })));
  const shown = deviantPaths.slice(0, 6);

  return `<li class="conflict">
    <div class="conflict__key num">${escapeHtml(c.key)}</div>
    <div class="conflict__body">
      <p class="conflict__majority">
        <span class="num">${escapeHtml(truncate(majority.value, 40))}</span>
        <span class="muted">${total === c.holders
          ? `used by ${majority.count} of ${c.holders} file${c.holders === 1 ? '' : 's'}`
          : `the usual value — ${majority.count} of ${total} times it appears, across ${c.holders} files`}</span>
      </p>
      <ul class="conflict__deviants">
        ${shown.map(({ v, path }) => `
          <li><button type="button" data-path="${escapeHtml(path ?? '')}">
            <span class="num">${escapeHtml(shortName(path))}</span>
            <span class="conflict__val num">${escapeHtml(truncate(v.value, 28))}</span>
          </button></li>`).join('')}
        ${deviantPaths.length > shown.length
          ? `<li class="muted">+${deviantPaths.length - shown.length} more</li>` : ''}
      </ul>
    </div>
  </li>`;
}

/* File ids that have a rename somewhere in their history. */
function renamedFiles() {
  if (!state.renamedSet) {
    state.renamedSet = new Set(
      state.events.filter((e) => e.kind === 'rename').map((e) => e.file),
    );
  }
  return state.renamedSet;
}

function shortName(p) {
  if (!p) return '(unknown)';
  const parts = p.split('/');
  return parts.length > 1 ? `…/${parts[parts.length - 1]}` : p;
}

function truncate(s, n) {
  const t = String(s);
  return t.length > n ? `${t.slice(0, n - 1)}…` : t;
}


/* ── Contributors (kraken) ─────────────────────────────────────────── */

/* Optional layer. A repository with no GitHub origin, or a machine with
 * no token, gets a stated reason instead of a silent gap. */
async function loadPeople() {
  try {
    const r = await fetch(`/api/people?repo=${encodeURIComponent(state.repo)}`);
    const j = await r.json();
    state.people = j;
    renderPeople();
    if (state.selected !== null) fetchEvidence(state.selected);
  } catch {
    state.people = null;
  }
}

function personFor(email) {
  const p = state.people;
  if (!p?.available) return null;
  const i = p.by_email?.[String(email).toLowerCase()];
  return i === undefined ? null : p.persons[i];
}

function renderPeople() {
  const p = state.people;
  if (!p) return;
  $('people').hidden = false;

  if (!p.available) {
    $('people-note').textContent = `Names could not be looked up — ${p.reason}`;
    $('roster').innerHTML = '';
    return;
  }

  const c = p.coverage;
  // Headcount coverage and contribution coverage are wildly different
  // numbers here, and quoting either alone misleads. Always both.
  $('people-note').innerHTML =
    `Looked up <span class="num">${escapeHtml(p.seed)}</span> on GitHub and put a name to `
    + `<strong>${fmtNum(c.resolved)} of ${fmtNum(c.authors)}</strong> email addresses — `
    + `between them they wrote <strong>${Math.round(c.commit_share * 100)}%</strong> of the commits. `
    + (c.unresolvable
      ? `${fmtNum(c.unresolvable)} ${c.unresolvable === 1 ? 'address is' : 'addresses are'} GitHub no-reply or bot addresses, which can never be matched. `
      : '')
    + `The rest belong to people whose public GitHub activity does not show that address. `
    + `<span class="people__live">This part comes from a live GitHub lookup. Everything else here `
    + `is measured from the repository and will not change; this can.</span>`;

  // Only people who actually touched this repository. kraken returns the
  // seed's wider network too; that is not this repository's roster.
  const here = p.persons.filter((x) => x.emails.some((e) => state.authorSet?.has(e)));
  const shown = here.length ? here : [];

  $('roster').innerHTML = shown.length
    ? shown.map(personCard).join('')
    : '<li class="roster__empty muted">None of this repository\'s contributors could be matched to a public GitHub identity.</li>';
}

function personCard(x) {
  const career = x.career.filter((c) => c.kind === 'corporate');
  return `<li class="person">
    <div class="person__id">
      <span class="person__name">${escapeHtml(x.name || '(unnamed)')}</span>
      ${x.logins.length ? `<span class="person__login num">@${escapeHtml(x.logins[0])}</span>` : ''}
    </div>
    <div class="person__facts">
      ${x.employer ? `<span class="person__fact"><span class="person__k">at</span> <span class="num">${escapeHtml(x.employer)}</span></span>` : ''}
      ${x.orgs.length ? `<span class="person__fact"><span class="person__k">orgs</span> <span class="num">${escapeHtml(x.orgs.slice(0, 4).join(', '))}</span></span>` : ''}
      ${x.work_pattern ? `<span class="person__fact"><span class="person__k">commits</span> ${escapeHtml(x.work_pattern)}</span>` : ''}
    </div>
    ${career.length > 1 ? `<p class="person__career num">${career
      .map((c) => `${escapeHtml(c.domain)} (${c.first_seen.slice(0, 4)})`).join(' → ')}</p>` : ''}
  </li>`;
}

/* ── Survey marks ──────────────────────────────────────────────────── */

/* Individually plotted marks stay legible up to this many per lane. Past
 * it the lane switches to a density profile — 800 stacked glyphs is not a
 * timeline, it is a wall. */
const SPARSE_MARKS = 40;
const MAX_TIERS = 3;
const DENSITY_BINS = 72;

/* Sparse lane: one glyph per event, nudged into up to three tiers so
 * near-simultaneous marks do not sit on top of each other. */
function markLane(evs, kind, min, span, colour) {
  const placed = [];
  const html = evs.map(({ e, i }, n) => {
    const pct = ((e.at - min) / span) * 100;
    let tier = 0;
    while (tier < MAX_TIERS - 1 && placed.some((p) => p.tier === tier && Math.abs(p.pct - pct) < 1.8)) tier++;
    placed.push({ pct, tier });
    const path = state.dict.files[e.file] ?? `file ${e.file}`;
    // No `title`: the callout carries far more, and a native tooltip on
    // top of it would be a second, slower, uglier answer.
    return `<button class="mark" type="button" data-file="${e.file}" data-ei="${i}"
      style="left:${pct.toFixed(2)}%;--tier:${tier};--d:${Math.min(n * 9, 700)}ms;color:${colour}"
      aria-label="${EVENT_LABEL[kind] || kind} on ${escapeHtml(path)}, ${fmtDate(e.at)}">
      <svg aria-hidden="true"><use href="#sym-${kind}"/></svg></button>`;
  }).join('');
  return { html, tiers: Math.max(...placed.map((p) => p.tier), 0) + 1 };
}

/* Dense lane: a profile along the time transect. Bar height is the count
 * in that bin, so clustering reads as relief instead of as a smear. */
function densityLane(evs, kind, min, span, colour) {
  const bins = new Array(DENSITY_BINS).fill(0);
  for (const e of evs) {
    const b = Math.min(DENSITY_BINS - 1, Math.floor(((e.at - min) / span) * DENSITY_BINS));
    bins[b]++;
  }
  const peak = Math.max(...bins);
  const w = 100 / DENSITY_BINS;
  const html = bins.map((n, i) => {
    if (!n) return '';
    const from = min + (i / DENSITY_BINS) * span;
    const to = min + ((i + 1) / DENSITY_BINS) * span;
    return `<span class="dbar" data-kind="${kind}" data-n="${n}" data-from="${Math.round(from)}" data-to="${Math.round(to)}"
      style="left:${(i * w).toFixed(3)}%;width:${w.toFixed(3)}%;
      height:${Math.max(9, (n / peak) * 100).toFixed(1)}%;background:${colour};--d:${Math.min(i * 7, 500)}ms"></span>`;
  }).join('');
  return {
    html: `<span class="density" role="img" aria-label="Density profile: ${evs.length} events, peak ${peak} in one bin">${html}</span>`,
    tiers: 2.4,
  };
}

function renderTimeline() {
  const host = $('timeline');
  if (!state.events.length || !state.dict) {
    $('marks').hidden = state.events.length === 0 && !state.dict;
    if (state.dict && !state.events.length) {
      $('marks').hidden = false;
      $('marks-note').textContent = 'Nothing to mark here. entropyx records an event only where one of its patterns matched.';
      host.innerHTML = '';
    }
    return;
  }
  $('marks').hidden = false;

  const times = state.events.map((e) => e.at);
  const min = Math.min(...times), max = Math.max(...times);
  const span = Math.max(1, max - min);

  const kinds = [...new Set(state.events.map((e) => e.kind))];
  $('marks-note').textContent =
    `${fmtNum(state.events.length)} marks across ${kinds.length} kinds, ${fmtDate(min)} to ${fmtDate(max)}.`;

  host.innerHTML = kinds.map((kind) => {
    // Carry each event's index in `state.events` so a mark can look its
    // own record back up without duplicating it into the DOM.
    const evs = state.events
      .map((e, i) => ({ e, i }))
      .filter(({ e }) => e.kind === kind)
      .sort((a, b) => a.e.at - b.e.at);
    const colour = `var(--e-${kind})`;
    const body = evs.length > SPARSE_MARKS
      ? densityLane(evs.map(({ e }) => e), kind, min, span, colour)
      : markLane(evs, kind, min, span, colour);
    return `<div class="timeline__lane" data-kind="${kind}" style="--tiers:${body.tiers}">
      <span class="timeline__lanelabel">
        <span class="timeline__lanename"><svg aria-hidden="true" style="color:${colour}"><use href="#sym-${kind}"/></svg>${EVENT_LABEL[kind] || kind}</span>
        <span class="timeline__lanecount num">${fmtNum(evs.length)}</span>
      </span>
      <span class="timeline__track">${body.html}</span>
    </div>`;
  }).join('') +
  `<div class="timeline__ruler"><span>time</span><span><span>${fmtDate(min)}</span><span>${fmtDate(max)}</span></span></div>`;

  host.querySelectorAll('.mark').forEach((btn) => {
    btn.addEventListener('click', () => {
      const fid = Number(btn.dataset.file);
      const idx = state.rows.findIndex((r) => r.file === fid);
      if (idx >= 0) {
        select(idx);
        $('terrain').scrollIntoView({ behavior: 'smooth', block: 'center' });
      }
    });
  });
}


/* ── Mark callout ──────────────────────────────────────────────────── */

/* Every event kind carries detail the timeline was throwing away: a
 * rename knows both of its names, an aftershock knows how long it ran, an
 * ownership split knows exactly who arrived. All of it is already in
 * memory, so the callout is a lookup, not a fetch.
 *
 * One element, delegated listeners on the lane container. Hovering a
 * thousand marks costs the same as hovering one. */
function bindMarkCallout() {
  const host = $('timeline');
  const show = (el) => {
    const html = el.classList.contains('mark')
      ? calloutForEvent(Number(el.dataset.ei))
      : calloutForBin(el);
    if (html) placeTip(el, html, el.dataset.kind || state.events[Number(el.dataset.ei)]?.kind);
  };
  host.addEventListener('pointerover', (ev) => {
    const el = ev.target.closest('.mark, .dbar');
    if (el) show(el);
  });
  // Moving between two marks passes briefly over the track between them.
  // A short grace period keeps the callout from flickering on the way.
  host.addEventListener('pointerout', () => scheduleHide());
  host.addEventListener('focusin', (ev) => {
    const el = ev.target.closest('.mark');
    if (el) show(el);
  });
  host.addEventListener('focusout', hideTip);
  window.addEventListener('scroll', hideTip, { passive: true });
}

function calloutForEvent(i) {
  const e = state.events[i];
  if (!e) return null;
  const path = state.dict.files[e.file] ?? `file ${e.file}`;
  const row = state.rows.findIndex((r) => r.file === e.file);

  let detail = '';
  // A rename names both paths itself, so the standalone path line would
  // just repeat the destination.
  let showPath = true;
  switch (e.kind) {
    case 'rename':
      // The whole point of a rename event, and it was invisible.
      showPath = false;
      detail = `<span class="num">${escapeHtml(e.from)}</span><br>→ <span class="num">${escapeHtml(e.to)}</span>`;
      break;
    case 'incident_aftershock':
      detail = e.window_days > 0
        ? `Patched repeatedly over <span class="num">${fmtNum(e.window_days)}</span> day${e.window_days === 1 ? '' : 's'}.`
        : 'A burst of fixes landing the same day.';
      break;
    case 'api_drift':
      detail = `<span class="num">${fmtNum(e.pub_items_changed)}</span> public declaration${e.pub_items_changed === 1 ? '' : 's'} changed.`;
      break;
    case 'ownership_split': {
      const who = (e.authors || []).map(namedAuthor).filter(Boolean);
      detail = who.length
        ? `Ownership widened to ${who.length}: ${who.slice(0, 4).map(escapeHtml).join(', ')}${who.length > 4 ? `, +${who.length - 4}` : ''}.`
        : 'A second contributor arrived after a long solo run.';
      break;
    }
    case 'hotspot':
      detail = e.reason === 'recent_burst'
        ? 'Most of this file\'s changes landed recently.'
        : escapeHtml(e.reason || '');
      break;
    default:
      detail = '';
  }

  const foot = [];
  if (e.sha) foot.push(e.sha.slice(0, 10));
  if (row >= 0) {
    foot.push(`score ${scoreOf(row).toFixed(2)}`);
    const cls = classOf(row);
    if (cls) foot.push(CLASS_LABEL[cls]);
  }

  return `<div class="tip__head"><span class="tip__kind">${escapeHtml(EVENT_LABEL[e.kind] || e.kind)}</span>
      <span class="tip__when">${fmtDate(e.at)}</span></div>
    ${showPath ? `<div class="tip__path">${escapeHtml(path)}</div>` : ''}
    ${detail ? `<div class="tip__detail">${detail}</div>` : ''}
    ${foot.length ? `<div class="tip__foot">${foot.map(escapeHtml).map((f) => `<span>${f}</span>`).join('')}</div>` : ''}`;
}

/* A density bar stands for a bin, not a file, so it reports the window
 * and the count rather than pretending to name something. */
function calloutForBin(el) {
  const n = Number(el.dataset.n);
  const kind = el.dataset.kind;
  return `<div class="tip__head"><span class="tip__kind">${escapeHtml(EVENT_LABEL[kind] || kind)}</span>
      <span class="tip__when">${fmtDate(Number(el.dataset.from))}</span></div>
    <div class="tip__detail"><span class="num">${fmtNum(n)}</span> in the ${daysBetween(el)} to ${fmtDate(Number(el.dataset.to))}.</div>
    <div class="tip__foot"><span>too many to plot individually — this lane is a density profile</span></div>`;
}

function daysBetween(el) {
  const d = Math.max(1, Math.round((Number(el.dataset.to) - Number(el.dataset.from)) / 86400));
  return `${d} day${d === 1 ? '' : 's'}`;
}

/* AuthorId → the best name we have: kraken's if resolved, else the
 * address entropyx recorded. */
function namedAuthor(id) {
  const email = state.dict?.authors?.[id];
  if (!email) return null;
  return personFor(email)?.name || email;
}

let hideTimer = null;

function scheduleHide() {
  clearTimeout(hideTimer);
  hideTimer = setTimeout(hideTip, 80);
}

function placeTip(anchor, html, kind) {
  clearTimeout(hideTimer);
  const tip = $('tip');
  tip.innerHTML = html;
  tip.hidden = false;
  tip.setAttribute('aria-hidden', 'false');
  if (kind) tip.style.setProperty('--tip-accent', `var(--e-${kind}, oklch(82% 0.09 60))`);

  const r = anchor.getBoundingClientRect();
  const t = tip.getBoundingClientRect();
  const pad = 8;
  let left = r.left + r.width / 2 - t.width / 2;
  left = Math.max(pad, Math.min(left, window.innerWidth - t.width - pad));
  // Above the mark by default; below when there is no room up there.
  let top = r.top - t.height - 10;
  if (top < pad) top = r.bottom + 10;
  tip.style.left = `${Math.round(left)}px`;
  tip.style.top = `${Math.round(top)}px`;
  tip.dataset.show = 'true';
}

function hideTip() {
  const tip = $('tip');
  if (tip.dataset.show !== 'true') return;
  tip.dataset.show = 'false';
  tip.setAttribute('aria-hidden', 'true');
  // Keep it out of the a11y tree and off the hit-test path once faded.
  setTimeout(() => {
    if (tip.dataset.show !== 'true') tip.hidden = true;
  }, 140);
}

/* ── Legend, ramp, weights ─────────────────────────────────────────── */

function buildRamp() {
  $('ramp').innerHTML = Array.from({ length: BANDS }, (_, i) =>
    `<span class="ramp__step" style="background:var(--el-${i})"></span>`).join('');
}

function buildClassList() {
  $('classlist').innerHTML = Object.entries(CLASS_LABEL).map(([k, label]) =>
    `<li data-class="${k}" style="color:var(--c-${k})">
       <svg aria-hidden="true"><use href="#sym-${k}"/></svg>
       <span style="color:var(--ink)">${label}</span>
       <span class="count"></span>
     </li>`).join('');
}

/* Counts for the legend.
 *
 * This is a *map* legend, so the first number has to be what is drawn.
 * Counting the whole repository here told the reader there were 223
 * frozen-neglect files while the map showed one — the other 222 are in
 * the undrawn tail, because quiet files are exactly the ones that lose
 * the change-density cut. Both numbers are useful, so both are shown. */
function renderClassCounts() {
  const all = {};
  for (let i = 0; i < state.rows.length; i++) {
    const c = classOf(i);
    if (c) all[c] = (all[c] || 0) + 1;
  }
  const drawn = {};
  for (const i of state.shownIdx || []) {
    const c = classOf(i);
    if (c) drawn[c] = (drawn[c] || 0) + 1;
  }
  const truncated = (state.shownIdx?.length ?? 0) < state.rows.length;

  $('classlist').querySelectorAll('li').forEach((li) => {
    const k = li.dataset.class;
    const onMap = drawn[k] || 0;
    const total = all[k] || 0;
    const cell = li.querySelector('.count');
    cell.textContent = truncated ? `${fmtNum(onMap)} / ${fmtNum(total)}` : fmtNum(total);
    cell.title = truncated
      ? `${fmtNum(onMap)} drawn on the map, ${fmtNum(total)} in the whole repository`
      : `${fmtNum(total)} files`;
    li.dataset.empty = total === 0 ? 'true' : 'false';
  });

  $('class-note').textContent = truncated
    ? 'on the map / in the whole repository'
    : '';
}

function buildWeightControls() {
  $('weights').innerHTML = AXES.map((a, i) => `
    <div class="weight">
      <label class="weight__label" for="w${i}"><b>${a.sym}</b> ${a.name}</label>
      <span class="weight__val" id="wv${i}">0.00</span>
      <input type="range" id="w${i}" min="0" max="0.6" step="0.01" value="0" aria-describedby="wv${i}">
    </div>`).join('');

  AXES.forEach((_, i) => {
    $(`w${i}`).addEventListener('input', (e) => {
      state.weights[i] = Number(e.target.value);
      $(`wv${i}`).textContent = state.weights[i].toFixed(2);
      updateSum();
      rescore();
    });
  });
}

function syncWeightControls() {
  if (!state.weights) return;
  AXES.forEach((_, i) => {
    $(`w${i}`).value = String(state.weights[i]);
    $(`wv${i}`).textContent = state.weights[i].toFixed(2);
  });
  updateSum();
}

function updateSum() {
  const s = sumPositive(Float64Array.from(state.weights));
  const off = Math.abs(s - 1) > 0.005;
  const el = $('wsum');
  el.dataset.off = off ? 'true' : 'false';
  el.textContent = off
    ? `weights add up to ${s.toFixed(2)} — these scores no longer match a standard scan`
    : `weights add up to ${s.toFixed(2)} — these scores match a standard scan`;
}

/* ── Chrome ────────────────────────────────────────────────────────── */

function drawGutter() {
  $('gutter-ticks').innerHTML = Array.from({ length: 26 }, (_, i) => {
    const major = i % 4 === 0;
    return `<span class="gutter__tick${major ? ' gutter__tick--major' : ''}"
      data-label="${major ? String(i * 4).padStart(2, '0') : ''}"
      style="animation-delay:${i * 18}ms"></span>`;
  }).join('');
}

function toggleLog(force) {
  const log = $('log');
  const collapsed = force === true ? true : log.dataset.collapsed !== 'true';
  log.dataset.collapsed = collapsed ? 'true' : 'false';
  $('log-toggle').setAttribute('aria-expanded', collapsed ? 'false' : 'true');
  $('log-toggle').textContent = collapsed ? 'Expand' : 'Collapse';
}

/* ── Formatting ────────────────────────────────────────────────────── */

function fmtMs(ms) {
  if (ms == null) return '—';
  if (ms < 1000) return `${ms} ms`;
  if (ms < 60000) return `${(ms / 1000).toFixed(2)} s`;
  const m = Math.floor(ms / 60000);
  return `${m}m ${((ms % 60000) / 1000).toFixed(0)}s`;
}

function fmtNum(n) {
  return typeof n === 'number' ? n.toLocaleString('en-US') : String(n);
}

function fmtDate(epoch) {
  return new Date(epoch * 1000).toISOString().slice(0, 10);
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) =>
    ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
}

function debounce(fn, ms) {
  let t;
  return (...a) => { clearTimeout(t); t = setTimeout(() => fn(...a), ms); };
}

boot();
