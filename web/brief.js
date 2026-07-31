/* Codebase brief — the same measurements, written for someone who will
 * never open the terrain.
 *
 * The hard rule here: no finding may assert anything the tools did not
 * measure. There is no health score, no grade, no "risk level". entropyx
 * exists to replace folklore with measurement, and a made-up number on
 * an executive page would be the worst possible betrayal of that. Every
 * sentence below is generated from a count, and every count links back
 * to the file or commit it came from.
 */

const $ = (id) => document.getElementById(id);

const state = { summary: null, dict: null, rows: [], events: [], divergence: null, people: null, meta: {}, repo: null };

/* Column indices into FileRow.values, fixed by the tq1 contract. */
const D_N = 0, H_A = 1, S_N = 5, T_C = 6, COMPOSITE = 7;

boot();

async function boot() {
  await loadRepoList();
  $('brief-form').addEventListener('submit', (e) => { e.preventDefault(); run(); });
  $('print-btn').addEventListener('click', () => window.print());

  const preset = new URLSearchParams(location.search).get('repo');
  if (preset) { $('repo').value = preset; run(); }
}

async function loadRepoList() {
  try {
    const j = await (await fetch('/api/repos')).json();
    $('repo-list').innerHTML = (j.repos || [])
      .map((r) => `<option value="${escapeHtml(r.path)}" label="${escapeHtml(r.name)}">`).join('');
  } catch {
    setStatus('Bridge unreachable. Start exbridge, then reload.');
  }
}

/* ── Gathering ─────────────────────────────────────────────────────── */

function run() {
  const repo = $('repo').value.trim();
  if (!repo) return;
  state.repo = repo;
  state.rows = []; state.events = []; state.dict = null; state.divergence = null;
  $('brief').hidden = true;
  $('run-btn').disabled = true;
  $('status').hidden = false;
  $('status-bar').hidden = false;
  // The walkthrough is an idle-state affordance; once a real measurement
  // is running it is just noise competing with the progress.
  $('demo').hidden = true;
  setStatus('Measuring the repository…');
  $('status-hint').textContent =
    'A first measurement of a large repository can take several minutes. '
    + 'Repeat runs at the same commit are instant.';

  const es = new EventSource(`/api/scan?repo=${encodeURIComponent(repo)}`);

  es.addEventListener('meta', (e) => { state.meta = JSON.parse(e.data); });
  es.addEventListener('phase', (e) => {
    const p = JSON.parse(e.data);
    if (p.status === 'start') setStatus(p.label + '…');
  });
  es.addEventListener('tick', (e) => {
    const t = JSON.parse(e.data);
    if (t.total) $('status-fill').style.width = `${((t.done / t.total) * 100).toFixed(1)}%`;
  });
  es.addEventListener('cached', () => setStatus('Reading the stored measurement…'));
  es.addEventListener('dict', (e) => {
    state.dict = JSON.parse(e.data);
    state.authorSet = new Set(state.dict.authors.map((a) => a.toLowerCase()));
  });
  es.addEventListener('rows', (e) => { state.rows.push(...JSON.parse(e.data).rows); });
  es.addEventListener('events', (e) => { state.events.push(...JSON.parse(e.data).events); });
  es.addEventListener('done', async (e) => {
    es.close();
    state.meta.done = JSON.parse(e.data);
    setStatus('Comparing similar configuration files…');
    try {
      const j = await (await fetch(`/api/fleets?repo=${encodeURIComponent(repo)}`)).json();
      state.divergence = j.error ? null : j;
    } catch { state.divergence = null; }
    setStatus('Looking up contributor names…');
    try {
      const j = await (await fetch(`/api/people?repo=${encodeURIComponent(repo)}`)).json();
      state.people = j.error ? null : j;
    } catch { state.people = null; }
    render();
    $('run-btn').disabled = false;
    $('status').hidden = true;
  });
  es.addEventListener('error', (ev) => {
    if (ev.data) { setStatus(JSON.parse(ev.data).message); }
    else if (es.readyState === EventSource.CLOSED) { setStatus('Connection to the measurement service closed.'); }
    es.close();
    $('run-btn').disabled = false;
    $('status-bar').hidden = true;
  });
}

function setStatus(t) { $('status-text').textContent = t; }

/* ── Findings ──────────────────────────────────────────────────────── */

function render() {
  const { dict, rows, events } = state;
  if (!dict || !rows.length) { setStatus('The measurement returned no files.'); return; }

  const path = (r) => dict.files[r.file];
  const ranked = [...rows].sort((a, b) => b.values[COMPOSITE] - a.values[COMPOSITE]);
  const times = events.map((e) => e.at);

  $('b-repo').textContent = state.repo.split('/').filter(Boolean).pop() || state.repo;
  $('b-period').textContent = times.length
    ? `Activity recorded ${fmtDate(Math.min(...times))} to ${fmtDate(Math.max(...times))}`
    : 'No dated activity recorded.';

  const findings = [
    keyPersonConcentration(ranked, path),
    unplannedWork(events, path),
    interfaceMovement(rows, path),
    configurationDivergence(),
    concentration(rows),
  ].filter(Boolean);

  $('b-standfirst').textContent = standfirst(findings, rows.length, dict.authors.length);

  $('findings').innerHTML = findings.map((f, i) => `
    <li class="finding">
      <p class="finding__num num">${String(i + 1).padStart(2, '0')}</p>
      <div class="finding__body">
        <h2 class="finding__title">${escapeHtml(f.title)}</h2>
        <p class="finding__headline">${f.headline}</p>
        <p class="finding__detail">${f.detail}</p>
        ${f.items?.length ? `<ul class="finding__items">${f.items.map(
          (it) => `<li><span class="num">${escapeHtml(it.label)}</span>${
            it.note ? `<span class="finding__note">${escapeHtml(it.note)}</span>` : ''}</li>`).join('')}</ul>` : ''}
        <p class="finding__source">${escapeHtml(f.source)}${
          f.link ? ` · <a href="${f.link}">${escapeHtml(f.linkText || 'show me')}</a>` : ''}</p>
      </div>
    </li>`).join('');

  const d = state.meta.done || {};
  $('f-path').textContent = state.repo;
  $('f-head').textContent = (state.meta.head || '').slice(0, 12) || 'unknown';
  $('f-files').textContent = fmtNum(rows.length);
  $('f-authors').textContent = fmtNum(dict.authors.length);
  $('f-digest').textContent = (d.digest || '').slice(0, 16) || '—';
  $('f-when').textContent = new Date().toISOString().slice(0, 10);
  $('to-survey').href = `./index.html?repo=${encodeURIComponent(state.repo)}`;
  $('b-gaps').textContent = gaps();

  // The identity layer is a live crawl. Everything else on this page is
  // reproducible from the commit; saying "every figure is reproducible"
  // without this caveat would be false the moment kraken contributes one.
  const live = state.people?.available && state.people.coverage.resolved > 0;
  $('b-live').hidden = !live;
  if (live) {
    $('b-live').textContent =
      'One exception: contributor identities come from a live crawl of public GitHub activity, '
      + 'not from this repository. That part is not reproducible from the commit alone and may '
      + 'differ if the brief is prepared again.';
  }

  $('brief').hidden = false;
  window.scrollTo(0, 0);
}

function surveyLink() {
  return `./index.html?repo=${encodeURIComponent(state.repo)}`;
}

/* 1 — Key-person concentration.
 * H_a is normalised authorship entropy: 0 means every recorded change to
 * that file came from one person. That is bus factor, measured. */
function keyPersonConcentration(ranked, path) {
  const TOP = Math.min(20, ranked.length);
  const top = ranked.slice(0, TOP);
  const solo = top.filter((r) => r.values[H_A] === 0);
  const soloAll = state.rows.filter((r) => r.values[H_A] === 0).length;

  return {
    title: 'Key-person concentration',
    headline: solo.length
      ? `<strong>${solo.length} of the ${TOP} most-changed files</strong> have only ever been edited by one person.`
      : `Every one of the ${TOP} most-changed files has been edited by more than one person.`,
    detail: solo.length
      ? `Across the whole repository, ${fmtNum(soloAll)} of ${fmtNum(state.rows.length)} files `
        + `(${pct(soloAll / state.rows.length)}) have a single author on record. That is a measure of `
        + `how much knowledge sits with one person. If they are away, someone else has to learn `
        + `these files from scratch before they can safely touch them.`
      : `Work on the busiest parts of this repository has passed through more than one pair of hands. `
        + `That is the healthier pattern — knowledge is shared rather than stranded.`,
    items: solo.slice(0, 5).map((r) => ({
      label: path(r),
      note: `score ${r.values[COMPOSITE].toFixed(2)}`,
    })),
    source: 'Measured from who has actually committed to each file.' + peopleContext(),
    link: surveyLink(),
    linkText: 'see these files in the survey',
  };
}

/* Contributor identity as *context*, never as an accusation.
 *
 * Two reasons this never names an individual. First, identity coverage is
 * partial and skewed — a public crawl resolves the prolific and misses the
 * rest — so "the" key person would often just be whoever happened to
 * resolve, which is a sampling artifact. Second, a name printed in a board
 * pack as the single point of failure carries consequences that a git
 * statistic has not earned. The survey names people; engineers reading it
 * already have git blame. This page reports the shape. */
function peopleContext() {
  const p = state.people;
  if (!p) return '';
  if (!p.available) return ` Contributor names could not be looked up: ${p.reason}`;

  const c = p.coverage;
  if (!c.resolved) return ' No contributor could be matched to a public GitHub profile.';

  const here = p.persons.filter((x) => x.emails.some((e) => state.authorSet?.has(e)));
  const employers = [...new Set(here.map((x) => x.employer).filter(Boolean))];

  return ` ${fmtNum(c.resolved)} of ${fmtNum(c.authors)} contributor addresses match a public GitHub `
    + `profile, and between them they wrote ${pct(c.commit_share)} of the code`
    + (employers.length ? `, affiliated with ${employers.join(', ')}` : '')
    + '.';
}

/* 2 — Unplanned work.
 *
 * entropyx emits exactly one aftershock event per affected file, so the
 * event count *is* the file count — quoting both would dress a tautology
 * up as corroboration. The second fact worth having is how long each
 * firefight ran, which the event carries as `window_days`. */
function unplannedWork(events, path) {
  const after = events.filter((e) => e.kind === 'incident_aftershock');
  if (!events.length) return null;

  const files = state.rows.length;
  const windows = after.map((e) => e.window_days ?? 0).sort((a, b) => b - a);
  const sustained = windows.filter((d) => d >= 7).length;
  const half = medianSplit(after.map((e) => e.at), events.map((e) => e.at));

  return {
    title: 'Unplanned work',
    headline: after.length
      ? `<strong>${fmtNum(after.length)} file${after.length === 1 ? '' : 's'}</strong> — ${pct(after.length / files)} `
        + `of the codebase — show repeated emergency fixes.`
      : 'No firefighting pattern was found in this history.',
    detail: after.length
      ? `${sustained
          ? `${fmtNum(sustained)} of them were being repeatedly patched for a week or longer, the longest for ${fmtNum(windows[0])} days. `
          : `Each was resolved within a week. `}${half} `
        + `None of this was planned work. It comes out of the same budget as everything on the `
        + `roadmap, and a rising share of it is usually the first sign a system is getting harder `
        + `to change safely.`
      : `Either this codebase has not needed emergency fixes in the recorded period, or its commit `
        + `messages do not mark them as such. The measurement reports what it can see and nothing more.`,
    items: after.length
      ? [...after].sort((a, b) => (b.window_days ?? 0) - (a.window_days ?? 0)).slice(0, 5)
          .map((e) => ({
            label: state.dict.files[e.file] ?? `file ${e.file}`,
            note: `${fmtNum(e.window_days ?? 0)} day${(e.window_days ?? 0) === 1 ? '' : 's'}`,
          }))
      : [],
    // A repository whose team writes `fix:` prefixes will register more
    // here than one that does not, for the same underlying incident
    // load. Saying so is the difference between a measurement and a
    // number.
    source: 'Measured from commit subjects marked as fixes or reverts, and the timing of changes around them. '
      + 'This reflects labelling practice as well as incident load — teams using conventional commit prefixes will register more.',
    link: surveyLink(),
    linkText: 'see the timeline',
  };
}

/* 3 — Public interface moving without tests moving with it. */
function interfaceMovement(rows, path) {
  const drifting = rows.filter((r) => r.signal_class === 'api_drift');
  if (!drifting.length) {
    return {
      title: 'Interface stability',
      headline: 'Nothing here is changing its public interface in the way that usually causes trouble.',
      detail: 'The parts other code depends on — the functions and types this codebase exposes — '
        + 'have been holding still, or changing under a steady enough hand that it has not caused drift.',
      items: [],
      source: 'Measured by reading the functions and types each file exposes, commit by commit.',
    };
  }
  const untested = drifting.filter((r) => r.values[T_C] < 0.2);
  return {
    title: 'Interface movement',
    headline: `<strong>${fmtNum(drifting.length)} file${drifting.length === 1 ? '' : 's'}</strong> `
      + `${drifting.length === 1 ? 'is' : 'are'} changing their public interface while ownership is spread across several people.`,
    detail: untested.length
      ? `${fmtNum(untested.length)} of those changed without tests moving alongside them. When the shape `
        + `of an interface moves and nothing verifies the new shape, the cost usually appears later and `
        + `somewhere else — in a consumer that was not updated.`
      : `Tests moved alongside these changes, which is the pattern you want when an interface shifts.`,
    items: drifting
      .sort((a, b) => b.values[S_N] - a.values[S_N])
      .slice(0, 5)
      .map((r) => ({ label: path(r), note: `tests move ${pct(r.values[T_C])} of the time` })),
    source: 'Measured by comparing what each file exposes from one commit to the next.',
    link: surveyLink(),
    linkText: 'see these files',
  };
}

/* 4 — Configuration divergence, from WhatTheDiff. */
function configurationDivergence() {
  const d = state.divergence;
  if (!d || !d.fleets.length) {
    return {
      title: 'Configuration consistency',
      headline: 'No sets of comparable configuration files were found.',
      detail: 'This check looks for groups of files that should be versions of one another — one '
        + 'config per service, one manifest per package — and reports where they disagree. This '
        + 'repository has no such groups, so there was nothing to compare.',
      items: [],
      source: 'Measured by comparing groups of similar files against each other.',
    };
  }

  const disputed = d.fleets.reduce((n, f) => n + f.report.conflicts.length, 0);
  const deviating = Object.values(d.by_path).filter((v) => v.deviant_keys.length).length;

  if (!disputed) {
    return {
      title: 'Configuration consistency',
      headline: `The ${d.fleets.length} group${d.fleets.length === 1 ? '' : 's'} of comparable configuration files agree on every setting they share.`,
      detail: 'Wherever this codebase repeats the same kind of configuration file, the copies match. '
        + 'That is the outcome you want, and it is worth knowing it holds.',
      items: [],
      source: 'Measured by comparing groups of similar files against each other.',
      link: surveyLink(),
      linkText: 'see the comparison',
    };
  }

  const worst = [...d.fleets]
    .filter((f) => f.report.conflicts.length)
    .sort((a, b) => b.report.conflicts.length - a.report.conflicts.length)[0];
  const example = worst.report.conflicts[0];
  const majority = example.values.reduce((a, b) => (b.count > a.count ? b : a));

  return {
    title: 'Configuration divergence',
    headline: `<strong>${fmtNum(deviating)} file${deviating === 1 ? '' : 's'}</strong> differ from their `
      + `peers on ${fmtNum(disputed)} setting${disputed === 1 ? '' : 's'}.`,
    // Files and occurrences are different denominators: an array-valued
    // key can hold several values inside one file. Everything here is
    // stated in files, which is the unit a reader will assume.
    detail: `For example, <span class="num">${escapeHtml(example.key)}</span> is set in `
      + `${fmtNum(example.holders)} file${example.holders === 1 ? '' : 's'}; ${fmtNum(example.deviants)} of them `
      + `use a value other than the most common one `
      + `(<span class="num">${escapeHtml(String(majority.value).slice(0, 40))}</span>). Each difference is either `
      + `deliberate and undocumented, or an oversight — and from the files alone there is no way to tell `
      + `which, which is the problem.`,
    items: [{ label: worst.fleet_label ?? worst.label, note: `${worst.report.conflicts.length} settings disputed` }],
    source: 'Measured by comparing groups of similar files against each other.',
    link: surveyLink(),
    linkText: 'see the differences',
  };
}

/* 5 — How concentrated change is. A Lorenz reading over change density:
 * what share of all recorded change sits in the busiest files. */
function concentration(rows) {
  const dn = rows.map((r) => r.values[D_N]).sort((a, b) => b - a);
  const total = dn.reduce((a, b) => a + b, 0);
  if (total <= 0) return null;

  const shareOfFiles = (frac) => {
    const n = Math.max(1, Math.round(dn.length * frac));
    const s = dn.slice(0, n).reduce((a, b) => a + b, 0);
    return s / total;
  };
  const top5 = shareOfFiles(0.05);
  const top20 = shareOfFiles(0.20);

  return {
    title: 'Where the work happens',
    headline: `<strong>${pct(top5)} of all recorded change</strong> sits in the busiest 5% of files.`,
    detail: `The busiest fifth accounts for ${pct(top20)}. That is not a fault — every codebase has a `
      + `hot core — but it tells you where careful review and good tests pay for themselves, and how `
      + `much rides on a small number of files.`,
    items: [],
    source: `Measured from ${fmtNum(rows.length)} files over the full recorded history.`,
    link: surveyLink(),
    linkText: 'see the map',
  };
}

function standfirst(findings, files, authors) {
  const head = findings[0];
  return `This covers ${fmtNum(files)} file${files === 1 ? '' : 's'} and `
    + `${fmtNum(authors)} contributor${authors === 1 ? '' : 's'}, measured straight from the repository's `
    + `history. There are ${findings.length} findings below, roughly in order of what they tend to cost. `
    + `Each one names the files behind it, so anything here can be checked in minutes.`
    + (head ? '' : '');
}

/* State the known blind spots on the page, not in a footnote nobody reads. */
function gaps() {
  const bits = [];
  if (!state.divergence || !state.divergence.fleets.length) {
    bits.push('no comparable configuration sets were found, so the consistency check had nothing to run against');
  }
  if (state.divergence?.rejected?.length) {
    bits.push(`${state.divergence.rejected.length} candidate file group${state.divergence.rejected.length === 1 ? ' was' : 's were'} excluded from the consistency check because their members are not variants of one another`);
  }
  if (state.meta.done?.cached) {
    bits.push('these figures come from a stored measurement of this exact commit, not a fresh run');
  }
  const p = state.people;
  if (p && !p.available) {
    bits.push(`contributor identities were not resolved (${p.reason})`);
  } else if (p?.coverage?.authors && p.coverage.resolved < p.coverage.authors) {
    bits.push(
      `${fmtNum(p.coverage.authors - p.coverage.resolved)} of ${fmtNum(p.coverage.authors)} contributor `
      + `addresses could not be matched to a public profile`
      + (p.coverage.unresolvable
        ? `, ${fmtNum(p.coverage.unresolvable)} of them GitHub noreply or bot addresses that never can be`
        : ''));
  }
  return bits.length ? `Known gaps in this brief: ${bits.join('; ')}.` : '';
}

/* ── Formatting ────────────────────────────────────────────────────── */

function medianSplit(subset, all) {
  if (!subset.length || !all.length) return '';
  const min = Math.min(...all), max = Math.max(...all);
  if (max === min) return '';
  const mid = (min + max) / 2;
  const late = subset.filter((t) => t >= mid).length;
  const share = late / subset.length;
  if (share >= 0.65) return `Most of it — ${pct(share)} — falls in the more recent half of the period.`;
  if (share <= 0.35) return `Most of it — ${pct(1 - share)} — falls in the earlier half of the period.`;
  return 'It is spread fairly evenly across the period.';
}

function pct(x) { return `${Math.round(x * 100)}%`; }
function fmtNum(n) { return typeof n === 'number' ? n.toLocaleString('en-US') : String(n); }
function fmtDate(epoch) {
  return new Date(epoch * 1000).toLocaleDateString('en-US', { year: 'numeric', month: 'long', day: 'numeric' });
}
function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) =>
    ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
}
