# entropyx-survey

[![tip](https://img.shields.io/badge/tip-tokentip.to%2F%40copyleftdev-8a4a22)](https://tokentip.to/@copyleftdev)
[![licence](https://img.shields.io/badge/licence-AGPL--3.0-8a4a22)](LICENSE)

A streaming bridge and browser survey for [entropyx](https://github.com/copyleftdev/entropyx),
plus its scoring kernel compiled to WebAssembly.

![The survey sheet: a squarified treemap of a repository where cell area is change
density and shade is the blended risk score, with a legend, a per-file measurement
profile and the commits behind it](docs/survey.png)

*entropyx surveying its own source. Cell **area** is how much a file changes,
**shade** is its overall score, and the symbols mark the six patterns entropyx
recognises. The right rail reads one file: its seven measurements and the commits
behind them.*

```
  entropyx crates ──┐                          ┌──► index.html  the survey sheet
   git·ast·core·tq  │                          │     terrain, calibration, evidence
   graph            ├─► exbridge (axum) ──SSE──┤
  wtd (WhatTheDiff)─┤    re-hosted scan        ├──► brief.html   the codebase brief
   peer divergence  │    + phase progress      │     five findings, printable
  kraken            │    + divergence          │
   identities       ┘    + identities          └──► exkernel.wasm  live re-weighting
```

Three tools, three questions, joined on two keys:

| tool | unit | question | joins on |
|---|---|---|---|
| entropyx | file | where is the risk | — |
| `wtd` | artifact fleet | where do peers disagree | **file path** |
| kraken | person | who owns it, and who are they | **author email** |

Two audiences, one measurement: the **survey** for engineers, the **brief** for everyone else.

- **`crates/exbridge`** — HTTP server that runs a scan and streams phase
  progress, then the tq1 summary in chunks, over Server-Sent Events.
- **`crates/exkernel`** — `entropyx-core` + `entropyx-tq` compiled to
  `wasm32-unknown-unknown`, so the browser can re-derive `composite` and
  `signal_class` under new weights without a round trip.
- **`web/index.html`** — the survey sheet: terrain, phase telemetry, divergence,
  evidence rail. For people who will open the files.
- **`web/brief.html`** — the codebase brief: five plain-language findings with their
  sources and limits, laid out to print. For people who will not.

Zero build step, zero external requests; fonts are vendored into `web/fonts`.

## Running it

```sh
cargo build --release -p exbridge
wasm-pack build crates/exkernel --target web --release --out-dir ../../web/pkg

EXBRIDGE_REPO_ROOT=$HOME/Project ./target/release/exbridge
# → http://127.0.0.1:7878
```

| env | default | meaning |
|---|---|---|
| `EXBRIDGE_PORT` | `7878` | listen port (loopback only) |
| `EXBRIDGE_REPO_ROOT` | current directory | where to look for repositories |
| `EXBRIDGE_WEB_DIR` | `web` | static root |

`EXBRIDGE_REPO_ROOT` is only a convenience for populating the picker: the root
itself counts if it is a repository, and so does anything one level below it.
You can always type the path to any git repository on the machine instead —
nothing is restricted to that root.

`entropyx` and `wtd` must be on `PATH`. `kraken` is optional; without it (or without a
GitHub token) the identity layer reports why it is absent and everything else works.
A token comes from `GITHUB_TOKEN` or, failing that, `gh auth token`.

## Why SSE

Scan cost is not predicted by repository age, and **not by anything else cheap
enough to check up front**. It is driven by co-change graph density and file
count at HEAD.

`entropyx scan` runs the whole pipeline serially. The bridge parallelises all
three expensive phases — betweenness, the blame loop, and the commit walk — while
keeping output byte-identical. Medians of 5 runs on an idle box:

| repo | `entropyx scan` | bridge | speedup |
|---|--:|--:|--:|
| CodexBar (1,925 commits) | 10.31 s | 1.33 s | 7.1–8.5× |
| repo C (72 commits, dense graph) | 33.66 s | 0.78 s | 39.0–47.8× |
| repo D (2,704 files) | 55.53 s | 2.90 s | 14.9–22.9× |
| **TrendRadar** (2,076 commits) | **755 s** | **28.60 s** | **~26×** |

CodexBar and TrendRadar are public — `github.com/steipete/CodexBar` and
`github.com/sansan0/TrendRadar` — so those two rows are reproducible. The
lettered repositories are private and appear only as their shape;
`docs/PERF.md` lists the dimensions that explain each timing.

Byte-for-byte identical output, including against the original 755-second run.
Ranges are 95% intervals via `agent-calc`; see *Keeping it bit-identical* in
`docs/PERF.md`, because f64 addition is not associative and a naive parallel
reduction changes the answer.

Twelve minutes of silence was the original argument for SSE. At eight seconds
that argument is weaker, and `docs/PERF.md` says so plainly. It still holds:
the cost remains unpredictable up front, cancellation still matters, and the
phase log is the most informative screen in the app — it shows each phase's real
share of elapsed time, and which one led on the repository in front of you.

## API

| endpoint | returns |
|---|---|
| `GET /api/describe` | entropyx's `describe` contract plus the bridge's phase list |
| `GET /api/repos` | git repositories under the configured root |
| `GET /api/scan?repo=…` | **SSE stream** (below); `&since=N`, `&fresh=true`, `&no_cache=true` |
| `GET /api/explain?repo=…&handle=…` | per-file commit evidence, served from the retained walk |
| `GET /api/fleets?repo=…` | peer artifact sets compared with `wtd`, plus a `by_path` join index |
| `GET /api/people?repo=…[&seed=…]` | contributor identities via kraken, plus coverage in both dimensions |

SSE event names, in order: `meta`, `phase` (twice per phase — start and done), `tick`,
`cached`, `dict`, `rows` (chunked, composite-descending), `events` (chunked),
`handles`, `done`, `error`.

Completed scans are cached in memory on `(canonical repo path, HEAD sha)`.
Closing the browser tab aborts the scan — the pipeline checks for a dropped
receiver at every phase boundary, so a closed tab does not leave a twelve-minute
computation running.

## The re-hosted pipeline

`entropyx-cli` keeps its scan orchestration inside `main.rs`, so there is no
library entry point to call and no way to observe phase boundaries from outside.
`crates/exbridge/src/pipeline.rs` reproduces that orchestration against the same
underlying crates, adding a `Progress` sink.

That is a fork, so it is guarded:

```sh
EXBRIDGE_PARITY_REPO=/path/to/repo cargo test --release -p exbridge --test parity
```

The guarantee is stronger than the test — output is **byte-for-byte identical**
to `entropyx scan`, verified byte-for-byte on all six subjects:

```sh
entropyx scan REPO --no-cache > a.json
cargo run --release --example dump -- REPO > b.json
cmp a.json b.json
```

If entropyx's pipeline changes upstream, parity fails and this module is what
needs updating.

## WebAssembly: what ports and what does not

| crate | wasm32 | why |
|---|:--:|---|
| `entropyx-core` | ✅ | no fs, process, net, threads, or clock |
| `entropyx-tq` | ✅ | same |
| `entropyx-graph` | ✅ | same (not currently compiled in) |
| `entropyx-ast` | ⚠️ | pure Rust logic, but 7 tree-sitter **C** grammars need a clang wasm build |
| `entropyx-git` | ❌ | `gix` needs a filesystem, **and** blame shells out to `git blame --line-porcelain` (`repo.rs:56`). No subprocess exists in wasm32 or WASI. |
| `entropyx-github` | ❌ | `ureq` raw sockets |

So `scan` cannot run in a browser. What ports is the part worth porting: the
entire metric, classification, and scoring kernel. `exkernel` (35 KB) loads a
summary's seven axes as a flat `Float64Array` and re-runs
`MetricComponents::composite` and `classify` — **the same code that produced the
numbers**, not a JavaScript re-implementation that could silently drift. 925
files re-score in about 1 ms.

```js
import init, { Kernel, defaultWeights } from './pkg/exkernel.js';
await init();
const k = new Kernel();
k.load(values /* n × 8 f64 */, incidentFlags /* n × u8 */);
k.rescore(Float64Array.from(defaultWeights()));
k.composites(); k.classes(); k.rank(); k.class_histogram();
```

`incidentFlags` is not derivable from the seven axes — the `IncidentAftershock`
override needs to know which files carry an incident-tagged commit, which the
caller extracts from the summary's `incident_aftershock` events.

## Divergence: what WhatTheDiff adds

entropyx sees one repository as files with histories. `wtd` sees it as *sets of peer
artifacts* — one config per service, one manifest per package — and reports where the set
disagrees. Both emit a per-file score in `[0,1]` with an outlier flag, so they overlay
without translation.

The judgement call is what counts as a peer, and `crates/exbridge/src/fleets.rs` is
deliberately conservative about it, because `wtd` will compare anything:

- **Peers are** files sharing a basename across directories (`package.json` × 9), or a
  config-ish extension within one directory (`.github/workflows/*.yml`).
- **Peers are not** source files. Three `lib.rs` share a name by language convention; they
  score ~0.98 drift because almost every primitive is unique to one of them. That is noise.
- **A candidate set is rejected** if it has no universal primitives or mean drift ≥ 0.95 —
  grouping by extension will otherwise put `package.json` next to `tsconfig.json`. Rejections
  are reported with their reason, never silently dropped.
- **Identifier keys are suppressed.** `wtd` flags any scalar key whose value varies; in a
  fleet of 57 payer records `clearinghousePayerId` varies 55 ways. A key only counts when
  some value holds ≥ 30% of the files, so there is a position to deviate *from*. The count of
  suppressed keys is shown.

On a real repository this found 76 files holding a minority value across 9 disputed settings —
including 4 of 60 bot definitions emitting PDF where the other 56 emit JSON.

## Drill-down is served from memory

`entropyx explain` re-walks history on every call — 1.2s on a 1,925-commit
repository — which the UI hits on every click and every arrow key. The scan
already walked that history, so `pipeline::scan` now returns an
`EvidenceIndex` alongside the summary and the bridge answers from it:
**1,210 ms → 12 ms**. The CLI stays as the fallback for anything not cached.

One deliberate difference: for a renamed file the index reports its whole
trajectory while the CLI reports only the current path (32 commits vs 22 on
one CodexBar file). The index matches what the score beside it was computed
from; the response says `"lineage": "trajectory"` and the rail says so on
screen. Verified on 60 random handles — 59 identical, 1 differing exactly
because of a rename.

`docs/PERF.md` has the full interactive-path numbers, including two things
that looked slow and were not.

## Identities: what kraken adds

entropyx knows *email addresses*. kraken knows *people* — real names, employers, org
membership, career history — and they join on the address in `dict.authors`.

**Coverage is partial, skewed, and reported in two dimensions, because one number alone
misleads.** kraken crawls GitHub's public identity graph; entropyx reads local commit
history. They overlap only where a contributor's public activity exposes the same address
they commit with. Measured:

| repo | resolved / authors | share of commits |
|---|--:|--:|
| CodexBar (public, 101 authors) | **1 / 101** | **68%** |
| repo D (private, 16 authors) | **10 / 16** | **93%** |

One percent by headcount and 68% by contribution are the same fact seen two ways; the UI
always states both. Addresses that can never resolve — GitHub noreply relays and bot
identities — are counted separately so they are not mistaken for a coverage failure.

Three judgement calls worth knowing about:

- **Timezone is deliberately not displayed.** kraken infers it from a commit-hour histogram.
  Across 37 contributors the median confidence was 0.00, and it placed a verifiably
  Vienna-based developer in "India / Central Asia" at 0.48. The field is kept in the API
  payload and omitted from every surface.
- **Personal email providers are never employers.** A gmail address is corporate-shaped to a
  naive classifier and says nothing about who anyone works for.
- **This layer is a live crawl, and the only part of the stack that is not reproducible.**
  entropyx and `wtd` produce the same numbers from the same commit forever; kraken hits the
  network and can differ between runs — it returned an empty crawl once during development
  and succeeded on retry. Both surfaces say so rather than letting the digest imply otherwise.

## The brief

`brief.html` is the same measurement written for someone who will never open the terrain.
Five findings — key-person concentration, unplanned work, interface movement, configuration
divergence, and where the work happens — each with a headline number, the files behind it,
and a note on what produced it.

![The codebase brief: a printed-document layout with numbered findings, each with a
headline number, supporting detail, the files behind it and a note on what produced
it](docs/brief.png)

*The same measurement, written for someone who will never open the terrain. It
prints to a three-page PDF.*

It also **never names an individual**. The survey does — engineers reading it already have
git blame — but a name printed in a board pack as the single point of failure carries
consequences a git statistic has not earned, and with partial identity coverage "the" key
person is often just whoever happened to resolve. The brief reports the shape: how many
contributors resolve, what share of the code they wrote, which employers they belong to.

It deliberately has **no health score, no grade, no risk level**. entropyx exists to replace
folklore with measurement; an invented number on an executive page would be the exact failure
it was built to prevent. Every sentence is generated from a count. The page ends by stating
what the measurement cannot see, and lists the known gaps in that specific run — including
when a figure came from cache rather than a fresh measurement.

Two denominators caused real bugs while building it and are worth knowing about:

- An array-valued key such as `portals[].defaultBotId` puts one file in several minority
  buckets, so *occurrences* and *files* are different numbers. Everything user-facing is
  stated in files.
- entropyx emits exactly one aftershock event per affected file, so quoting both the event
  count and the file count dresses a tautology up as corroboration.

The unplanned-work finding also discloses that it counts commits *labelled* as fixes, so a
team using conventional-commit prefixes registers more than one that does not.

## The sheet

The interface treats the repository as terrain. Cell **area** is change density
D<sub>n</sub>; cell **elevation** is the composite, tinted in hypsometric bands
like a topographic quadrangle. Signal classes are drawn as cartographic symbols.

Two things worth knowing about how it reads:

- **The contour interval is chosen from the data.** Composites cluster low on
  nearly every repository, so a linear 0–1 ramp puts almost everything in one
  tint. Bands are quantiles of the files actually drawn, and the legend prints
  the real composite at every boundary.
- **The terrain is truncated**, and says so. The cell budget scales with the
  drawing area; the remaining files are scored and searchable but not drawn.

Design context and constraints are in `.impeccable.md`.
