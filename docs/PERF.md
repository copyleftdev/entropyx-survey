# What a scan actually costs

Measured 2026-07-30/31 against entropyx 0.1.0 on a 64-core Linux box.

**The subjects.** Two are public repositories anyone can clone and reproduce
these numbers against. The other four are private, and appear only as their
measurable shape — which is what actually explains the timings, since scan cost
is driven by graph density and file count, not by what a repository is called.

| subject | commits | files at HEAD | lifetime paths | mean degree | clone |
|---|--:|--:|--:|--:|---|
| repo A | 49 | 96 | 100 | 33 | private |
| CodexBar | 1 925 | 780 | 925 | 199 | `github.com/steipete/CodexBar` |
| repo D | 248 | 2 704 | 2 576 | 382 | private |
| repo C | 72 | 119 | 1 995 | 577 | private |
| repo B | 39 | 956 | 961 | 825 | private |
| TrendRadar | 2 076 | 397 | 4 270 | 3 531 | `github.com/sansan0/TrendRadar` |

"Mean degree" is the co-change graph's average neighbours per node. It is the
single best predictor of scan cost, and the tables below are ordered by it.

This document exists because the intuitive model — "big repository, long scan" —
is wrong, and building a UI on it would have produced a progress bar that lies.

**Read this first:** everything down to *Consequences for the bridge* describes
`entropyx scan`, the CLI, and still holds. The bridge no longer behaves that way
— it parallelises the two dominant phases and runs 2.4–37× faster with
byte-identical output. That work, and how the profile changed as a result, is in
*Making it fast* near the end.

## The headline (serial: `entropyx scan`)

**Two phases are the whole scan, and which one leads depends on the repository.**

- `CoChangeGraph::betweenness_centrality()` — `entropyx-graph/src/lib.rs:99`,
  Brandes' algorithm, serial, O(V·E).
- The blame loop — one `git blame --line-porcelain` subprocess per file at HEAD,
  serially (`entropyx-cli/src/main.rs`, `Repo::blame` at `entropyx-git/src/repo.rs:56`).

Seconds per phase, full history, phases under 0.05 s omitted:

| repo | commit_loop | betweenness | blame | total | leader |
|---|--:|--:|--:|--:|---|
| repo A | 0.34 | ~0 | **1.51** | 2.0 | blame 76% |
| CodexBar | 1.30 | 0.68 | **7.00** | 9.2 | blame 76% |
| repo B | 0.19 | 3.22 | **4.18** | 7.2 | blame 58% |
| repo C | 0.25 | **17.26** | 1.49 | 17.7 | betweenness 92% |
| repo D | 1.13 | **17.83** | 12.72 | 32.2 | betweenness 55% |
| TrendRadar | — | **dominant** | — | 755 | betweenness ~99% |

Together those two are 85–99% of every scan measured. Everything the tool is
nominally *about* — walking commits, diffing trees, parsing ASTs for public-API
deltas — is 1–16%, and usually under 3%.

An earlier draft of this document claimed betweenness was 90%+ of *every* scan.
That was an over-generalisation from the first two repositories measured. On a
repo with many files at HEAD and a sparse co-change graph, blame leads by a wide
margin.

## Commit count does not predict runtime

| repo | commits | HEAD files | V (lifetime paths) | E (Σ C(k,2)) | cold scan |
|---|--:|--:|--:|--:|--:|
| repo A | 49 | 96 | 100 | 1 860 | **2.0 s** |
| repo B | 39 | 956 | 961 | 396 838 | 7.2 s |
| CodexBar | **1 925** | 780 | 925 | 70 083 | **9.2 s** |
| repo C | **72** | 119 | 1 995 | 837 236 | **17.7 s** |
| repo D | 248 | 2 704 | 2 576 | 1 053 551 | 32.2 s |
| TrendRadar | 2 076 | 397 | 4 270 | 7 546 312 | **755 s** |

CodexBar has **27× repo C's commits and scans 2× faster.**

Two independent quantities predict runtime instead:

```
T ≈ V·E / 10⁸  +  0.005…0.015 × HEAD_files
     └ betweenness ┘   └ blame ┘
```

**V** is distinct file paths over the repository's *lifetime*, not at HEAD —
repo C has 119 files at HEAD but 1 995 paths across its history, because
deleted and renamed-away files stay in the graph. **E** is Σ over commits of
C(files_in_commit, 2): every commit contributes a clique over the paths it
touched, so a commit touching *k* files adds k²/2 edges. TrendRadar contains a
single 3 868-file commit, which alone contributes ~7.5M edges.

Predicted vs measured betweenness: repo B 3.8 s / 3.22 s, repo C 17 s /
17.26 s, CodexBar 0.65 s / 0.68 s, repo D 27 s / 17.83 s. Good to roughly
±40%, which is enough to know which phase will lead.

## Three things that do not help

Each of these was tested, not assumed.

**`--since N` is a performance no-op.** It bounds the commit walk only; the
co-change graph and the blame loop are untouched. On repo C, `--since 1`
(one commit, four changed files) took 20.4 s against 22.5 s for full history.

**The disk cache buys nothing measurable.** It caches parsed AST items, which
live in the small commit-loop phase. Populate-then-rerun on repo C: 22.2 s
then 22.7 s.

**`git gc` changes nothing.** repo C ships 3 033 loose objects and issues
~92 000 `openat` calls during a scan, which looks like the culprit until you
measure it: syscall time totals 0.48 s against 21 s of *user* CPU. Repacking a
clone into 2 packfiles: 21.8 s, statistically identical.

## Consequences for the bridge

1. **Scan latency is unbounded and unpredictable from cheap metadata.** You
   cannot estimate it from commit count or repository size on disk. This was the
   argument for SSE over a request/response fetch — a cold TrendRadar scan is
   12 minutes of silence through the CLI. The bridge has since cut that to 27
   seconds; see *Does SSE still earn its place?* for whether the conclusion
   survives (it does, for different reasons).

2. **Cache aggressively, keyed on `(repo, HEAD sha)`.** Re-serving costs
   nothing; re-measuring can cost minutes.

3. **Progress must be phase-shaped, not percentage-shaped.** Nine of eleven
   phases complete in the first fraction of a second and then one or two run for
   the rest of the scan. A linear bar would sit near 80% throughout. The UI
   instead shows each phase's real share of elapsed time — which, usefully, also
   reveals *which* phase led on this particular repository.

4. **Cancellation matters.** A closed browser tab must abort the scan or a
   twelve-minute betweenness computation keeps burning a core. `exbridge`
   detects the dropped SSE receiver and unwinds at the next phase checkpoint.

## The interactive path

Scan cost is one problem; click-to-paint is another, and it was worse than
it looked.

| interaction | before | after |
|---|--:|--:|
| click a cell → commits appear (CodexBar, 1,925 commits) | **1,210 ms** | **12 ms** |
| same, repo D (248 commits) | 470 ms | 1.8 ms |
| re-score all files on a weight change (925 files) | — | 2.2–4.1 ms |
| selection lookup | O(cells) | O(1) |
| handle → path lookup | O(handles), per selection | O(1) |

**`entropyx explain` re-opens the repository and re-walks history on every
call.** That is fine for a CLI invoked occasionally and fatal for a UI that
calls it on every click and every arrow key. The bridge already walked that
history during the scan, so it now keeps an `EvidenceIndex` — commit
subjects stored once globally, 12 bytes per file-touch — and serves
`explain` from memory. The CLI remains the fallback for anything not in
cache.

Verified against the CLI on 60 random handles: **59 identical, 1 different,
and that one is explained** — see below. Handle resolution also moved from a
linear scan over every handle to a map built once.

### One deliberate difference

For a file that was renamed, the index reports the commits of its whole
*trajectory*; `entropyx explain` reports only the commits under its current
literal path. On `UsageFormatter.swift` that is 32 commits versus 22.

The index is the one consistent with the rest of the sheet: entropyx's own
scan computes that file's metrics over the full trajectory, so a score
derived from 32 commits sitting above a list of 22 would be the misleading
pairing. The response carries `"lineage": "trajectory"`, and the rail says
so on any file that was renamed.

### What turned out not to be slow

Worth recording, because two of these looked like obvious targets:

- **The weight sliders were already fine.** A first measurement said 34 ms
  per frame; that was two `requestAnimationFrame` waits in the harness, a
  ~33 ms floor at 60 Hz, not the app. The real work is 2.2–4.1 ms.
- **Reading `clientWidth` per cell** was suspected of thrashing layout. It
  measured 0.3 ms for 509 cells. It was still removed — the geometry is
  already known from the treemap — but it was never the problem.
- **Rewriting each cell's `<use>` symbol** *was* real: 6 ms per repaint for
  work that almost never changes, since `classify` reads the raw axes and is
  therefore invariant under re-weighting. Painting is now incremental.

## Making it fast

Brandes' algorithm is embarrassingly parallel over source vertices, and the
blame loop is N independent subprocesses. Neither needed to be serial. The
bridge parallelises both.

Timings are medians of 5 runs on an idle box, with 95% confidence intervals
from `agent-calc`'s `student_t_interval`; speedup ranges are the conservative
band (slowest plausible baseline over fastest plausible bridge, and the
reverse). Every pair differs at p < 0.01 by Welch's `two_sample_t`.

| repo | mean degree | `entropyx scan` | bridge | speedup |
|---|--:|--:|--:|--:|
| repo A | 33 | 1.98 s | 0.56 s | 3.4–3.7× |
| CodexBar | 199 | 10.31 s | 1.33 s | 7.1–8.5× |
| repo D | 382 | 55.53 s | 2.90 s | 14.9–22.9× |
| repo C | 577 | 33.66 s | 0.78 s | 39.0–47.8× |
| repo B | 825 | 8.87 s | 0.77 s | 9.0–14.4× |
| **TrendRadar** | **3531** | **755 s** | **28.60 s** | **~26×** |

Output is **byte-for-byte identical** on all six, TrendRadar included — checked
against the artefact from the original 755-second run.

*TrendRadar's baseline is a single measurement, not a median; a 755-second run
was not worth repeating five times. It carries no interval and is marked
accordingly.*

### A measurement I got wrong

An earlier draft of this table claimed 109× on TrendRadar, from a single
6.94-second run. That number is not reproducible: repeated runs on an idle box
give 27 s. The 6.94 s reading was taken while other work was in flight and is
simply wrong.

The lesson is the one this document already recorded once, in *What turned out
not to be slow* — measure on a quiet box, take several samples, and report a
spread rather than a point estimate. The numbers above are medians of five with
intervals, which is why they are believable and the 109× was not.

### Keeping it bit-identical

Parallelising Brandes naively changes the answer. The serial version accumulates
`cb[w] += delta_s[w]` for `s` in `0..n`, and f64 addition is not associative — any
other reduction order shifts the last bits and breaks parity.

So sources are processed in fixed batches of 256: a batch is computed in
parallel, then folded into `cb` **in ascending source order** before the next
batch begins. Every `+=` lands in exactly the order the serial loop used. That is
identical to the last bit, not merely close, and `brandes.rs` asserts
`f64::to_bits()` equality against `CoChangeGraph` — including across a batch
boundary, where a sloppy implementation would drift.

Two smaller traps, both caught by those tests:

- Hoisting the division out of the inner loop computes
  `sigma[v] * ((1+delta[w]) / sigma[w])` instead of
  `(sigma[v] / sigma[w]) * (1+delta[w])`. Algebraically equal, not bit-equal.
- `entropyx-graph` keeps its adjacency private, so the graph is rebuilt locally.
  Node numbering must match upstream exactly — first-seen interning order,
  neighbours ascending — because the predecessor lists that drive accumulation
  order depend on it.

Blame needed none of this care: `blame_youth` is a pure function of line times
and results land in a `BTreeMap`, so completion order cannot affect the output.

### Graph density decides the right algorithm

Upstream allocates `n` fresh predecessor `Vec`s per source — 18 million
allocations on TrendRadar, contended across 64 workers. The obvious fix is to
drop predecessors entirely and recompute them by scanning `adj[w]` during
accumulation, which is valid: `preds[w]` is exactly the neighbours of `w` one
BFS level closer.

It is also a trap. Measured across all three variants:

| repo | mean degree | fresh preds | adjacency scan | retained preds |
|---|--:|--:|--:|--:|
| repo A | 33 | 0.72 s | 0.72 s | 0.78 s |
| CodexBar | 199 | 2.45 s | 2.35 s | 2.39 s |
| repo D | 382 | 4.76 s | 3.49 s | 3.67 s |
| repo C | 577 | 1.32 s | 0.91 s | 1.00 s |
| repo B | 825 | 0.87 s | 0.90 s | 0.88 s |
| TrendRadar | 3531 | ~28 s | **55.40 s** | **27.06 s** |

The scan doubles the worst case. A dense graph has a shallow BFS: on TrendRadar
a vertex has ~3,500 neighbours and typically one predecessor, so scanning does
thousands of checks per useful update. Retaining and clearing the predecessor
lists keeps the tight inner loop *and* removes the allocation churn — within 10%
of the best variant on every small case, and half the cost on the one that
matters.

`EXBRIDGE_GRAPH_STATS=1` prints node count and mean degree for any scan.

### Parallelising the walk

Once betweenness and blame came down, the commit loop was briefly the largest
phase on most repositories. It does not parallelise as freely as the other two:
the lineage resolver is a union-find mutated as the walk proceeds, and a
commit's canonical paths depend on every rename seen before it. Running that out
of order would silently move a file's history onto the wrong trajectory.

But the *expensive* half has no such constraint. Diffing a commit against its
parent, resolving blob shas, and parsing blobs for their public API depend only
on that one commit. So the walk splits in two: fetch everything in parallel,
then replay in commit order over data already in memory. The replay does no I/O
at all, and blob parsing moved from per-commit batches to one global
deduplicated pass.

| repo | commit_loop before | after | |
|---|--:|--:|--:|
| CodexBar | 1.40 s | 0.56 s | 2.5× |
| repo D | 1.02 s | 0.27 s | 3.8× |
| TrendRadar | 2.66 s | 0.33 s | 8.1× |

### Where the time goes now

Seconds per phase, measured through the bridge:

| repo | commit_loop | graph_build | betweenness | blame | total |
|---|--:|--:|--:|--:|--:|
| CodexBar | 0.56 | — | 0.05 | **0.93** | 2.13 |
| repo D | 0.27 | 0.19 | 0.77 | **1.91** | 3.71 |
| TrendRadar | 0.33 | 1.90 | **26.45** | 0.22 | 29.75 |

Two bottlenecks remain, and they are different problems:

- **Blame**, on repositories with many files at HEAD. It is already parallel, so
  what is left is process-spawn cost — one `git blame` per file, 2,213 of them on
  repo D. Only `gix-blame` landing in-process would remove it.
- **Betweenness**, on dense graphs. At mean degree 3,531 TrendRadar spends 89% of
  its scan there even fully parallel; the algorithm is O(V·E) and no amount of
  cores changes that. Sampling sources would, at the cost of exactness — which
  this bridge does not trade away.

`Phase::weight` — the prior the UI uses to flag which phases are worth watching
before any timing exists — is set from these.

### Does SSE still earn its place?

The original argument was twelve minutes of silence. The worst case is now 27
seconds, so that argument is much weaker and it would be dishonest to keep
citing it.

It still holds, for three reasons that do not depend on the absolute number: the
cost is still unpredictable from cheap metadata, so there is no way to decide up
front whether this scan is the fast kind; the phase log is the most informative
screen in the app and is worth having regardless; and cancellation still
matters, because a closed tab should not leave work running.

## Reproducing

The bridge exposes every phase boundary over SSE without patching entropyx:

```sh
curl -N 'http://127.0.0.1:7878/api/scan?repo=/path/to/repo&fresh=true' \
  | grep -A1 '^event: phase'
```

Graph density for any repository:

```sh
git -C REPO log --format='%H' --name-only | awk '
  /^[0-9a-f]{40}$/ { if (k>1) E += k*(k-1)/2; if (k>mx) mx=k; k=0; n++; next }
  NF { k++ }
  END { if (k>1) E += k*(k-1)/2; printf "commits=%d E=%d widest_commit=%d\n", n, E, mx }'
```

## Not filed upstream

These are observations about entropyx 0.1.0, not defects — betweenness is a
deliberate part of the C_s definition (`max(weighted_degree, betweenness)`), and
the results are correct. But if scan cost ever becomes a priority, the levers in
order of leverage are:

- **Parallelise Brandes across source vertices.** It is embarrassingly parallel
  over the outer `for s in 0..n` loop, and the crate already depends on rayon.
- **Parallelise the blame loop**, or replace the per-file subprocess with
  `gix-blame` when it stabilises. 780 serial process spawns is 7 seconds of
  mostly-waiting.
- **Sample betweenness** for large V, or bound the co-change graph the way
  `--since` bounds the walk.
