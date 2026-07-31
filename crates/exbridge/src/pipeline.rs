//! Re-hosted `entropyx scan` orchestration.
//!
//! `entropyx-cli` keeps its scan pipeline inside `main.rs`, so there is no
//! library entry point to call. This module reproduces that orchestration
//! against the same underlying crates (`entropyx-git`, `-ast`, `-core`,
//! `-graph`, `-tq`) with one addition: a `Progress` sink invoked at every
//! phase boundary, so the bridge can stream real progress instead of
//! guessing at a subprocess.
//!
//! The output must stay bitwise-identical to `entropyx scan`. `tests/parity`
//! asserts that against the installed binary; treat any divergence as a bug
//! here, not upstream.

use entropyx_cli::cache::DiskItemsCache;
use entropyx_core::metric::{
    author_dispersion, blame_youth, change_counts, classify, detect_ownership_split,
    detect_recent_burst, is_incident_subject, is_test_path, saturate_unit, temporal_volatility,
    unit_normalize,
};
use entropyx_core::{Handle, MetricComponents, ScoreWeights, SignalClass, Timestamp, VertexTable};
use entropyx_graph::CoChangeGraph;
use entropyx_tq::{Dict, Enrichments, Event, FileRow, Schema, Summary};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// Phase boundaries of a scan, in execution order.
///
/// `Betweenness` and `Blame` are the two that matter: together they are
/// 85-99% of wall time on every repository measured, and which of them
/// leads flips depending on graph density versus file count at HEAD.
/// Neither cost is visible from cheap up-front metadata. See
/// `docs/PERF.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Open,
    Walk,
    CommitLoop,
    Normalize,
    GraphBuild,
    Betweenness,
    HeadTree,
    Blame,
    Rows,
    Events,
    Handles,
}

impl Phase {
    pub fn id(self) -> &'static str {
        match self {
            Phase::Open => "open",
            Phase::Walk => "walk",
            Phase::CommitLoop => "commit_loop",
            Phase::Normalize => "normalize",
            Phase::GraphBuild => "graph_build",
            Phase::Betweenness => "betweenness",
            Phase::HeadTree => "head_tree",
            Phase::Blame => "blame",
            Phase::Rows => "rows",
            Phase::Events => "events",
            Phase::Handles => "handles",
        }
    }

    /// Human-facing label. The UI renders these verbatim.
    pub fn label(self) -> &'static str {
        match self {
            Phase::Open => "Opening repository",
            Phase::Walk => "Walking commit history",
            Phase::CommitLoop => "Diffing commits and parsing public API",
            Phase::Normalize => "Normalizing change density",
            Phase::GraphBuild => "Building co-change graph",
            Phase::Betweenness => "Computing betweenness centrality",
            Phase::HeadTree => "Reading HEAD tree",
            Phase::Blame => "Blaming files at HEAD",
            Phase::Rows => "Scoring files",
            Phase::Events => "Detecting events",
            Phase::Handles => "Minting evidence handles",
        }
    }

    /// Rough prior share of total runtime. Used only to mark which phases
    /// are worth the user's attention before any timing exists; once a
    /// phase completes, the UI replaces this with its measured share.
    ///
    /// With all three expensive phases parallelised, what is left is
    /// blame on repositories with many files at HEAD (process-spawn
    /// bound) and betweenness on dense graphs (O(V·E), which cores do not
    /// fix). See `docs/PERF.md`.
    pub fn weight(self) -> f64 {
        match self {
            Phase::Betweenness => 0.40,
            Phase::Blame => 0.30,
            Phase::CommitLoop => 0.15,
            Phase::GraphBuild => 0.10,
            _ => 0.005,
        }
    }

    pub const ALL: [Phase; 11] = [
        Phase::Open,
        Phase::Walk,
        Phase::CommitLoop,
        Phase::Normalize,
        Phase::GraphBuild,
        Phase::Betweenness,
        Phase::HeadTree,
        Phase::Blame,
        Phase::Rows,
        Phase::Events,
        Phase::Handles,
    ];
}

/// Per-file commit evidence, retained from the walk the scan already did.
///
/// `entropyx explain` re-opens the repository and re-walks history on every
/// call — measured at 0.47s on a 248-commit repo and 1.21s on a
/// 1,925-commit one. That is a click-to-paint delay on every cell
/// selection, for information this pipeline held in memory moments
/// earlier and threw away. Keeping it costs one `u32` and one `i64` per
/// file-touch; commit subjects are stored once globally, not per touch.
#[derive(Debug, Default)]
pub struct EvidenceIndex {
    /// Every commit in the walk, in walk order (newest first).
    pub commits: Vec<CommitRef>,
    /// Canonical path → indices into `commits`, newest first.
    pub by_path: BTreeMap<String, Vec<TouchRef>>,
}

#[derive(Clone, Debug)]
pub struct CommitRef {
    pub sha: String,
    pub subject: String,
    pub author: String,
}

#[derive(Clone, Copy, Debug)]
pub struct TouchRef {
    pub commit: u32,
    pub time: i64,
}

impl EvidenceIndex {
    /// Touches for a path, or an empty slice. Newest first.
    pub fn touches(&self, path: &str) -> &[TouchRef] {
        self.by_path.get(path).map_or(&[], Vec::as_slice)
    }
}

/// Sink for pipeline progress. Implementations must be cheap and must not
/// block the scan thread for long — the SSE implementation pushes into an
/// unbounded channel.
pub trait Progress: Send + Sync {
    fn phase_start(&self, phase: Phase);
    fn phase_done(&self, phase: Phase, elapsed_ms: u128, detail: serde_json::Value);
    /// Intra-phase tick for the two long phases. `done`/`total` are item
    /// counts (commits walked, files blamed).
    fn tick(&self, phase: Phase, done: usize, total: usize);
    /// True if the client disconnected and the scan should abort.
    fn cancelled(&self) -> bool {
        false
    }
}

/// No-op sink, for the parity test and any non-streaming caller.
pub struct Silent;
impl Progress for Silent {
    fn phase_start(&self, _: Phase) {}
    fn phase_done(&self, _: Phase, _: u128, _: serde_json::Value) {}
    fn tick(&self, _: Phase, _: usize, _: usize) {}
}

#[derive(Debug)]
pub enum ScanError {
    Open(String),
    Walk(String),
    Diff(String),
    HeadTree(String),
    Cancelled,
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanError::Open(e) => write!(f, "open failed: {e}"),
            ScanError::Walk(e) => write!(f, "walk failed: {e}"),
            ScanError::Diff(e) => write!(f, "diff failed: {e}"),
            ScanError::HeadTree(e) => write!(f, "head tree walk failed: {e}"),
            ScanError::Cancelled => write!(f, "cancelled by client"),
        }
    }
}

pub struct ScanOptions {
    pub since: Option<usize>,
    pub no_cache: bool,
    pub weights: ScoreWeights,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            since: None,
            no_cache: false,
            weights: MetricComponents::DEFAULT_WEIGHTS,
        }
    }
}

macro_rules! bail_if_cancelled {
    ($p:expr) => {
        if $p.cancelled() {
            return Err(ScanError::Cancelled);
        }
    };
}

/// Run the full scan, reporting progress through `progress`.
///
/// Mirrors `entropyx-cli/src/main.rs::scan` step for step. GitHub
/// enrichment is intentionally omitted — the bridge never makes network
/// calls, so `Summary.enrichments` is always empty here.
pub fn scan<P: Progress>(
    path: &str,
    opts: &ScanOptions,
    progress: &P,
) -> Result<(Summary, EvidenceIndex), ScanError> {
    let weights = opts.weights;

    let t = std::time::Instant::now();
    progress.phase_start(Phase::Open);
    let repo = entropyx_git::Repo::open(path).map_err(|e| ScanError::Open(e.to_string()))?;
    progress.phase_done(Phase::Open, t.elapsed().as_millis(), serde_json::json!({}));

    let t = std::time::Instant::now();
    progress.phase_start(Phase::Walk);
    let walk = repo.walk().map_err(|e| ScanError::Walk(e.to_string()))?;
    let walk_result: Result<Vec<_>, _> = match opts.since {
        Some(n) => walk.take(n).collect(),
        None => walk.collect(),
    };
    let metas = walk_result.map_err(|e| ScanError::Walk(e.to_string()))?;
    progress.phase_done(
        Phase::Walk,
        t.elapsed().as_millis(),
        serde_json::json!({ "commits": metas.len() }),
    );
    bail_if_cancelled!(progress);

    // ---- commit loop: diff each commit against its first parent, and
    // accumulate every per-file series the metrics need.
    let t = std::time::Instant::now();
    progress.phase_start(Phase::CommitLoop);

    let mut per_file_times: BTreeMap<String, Vec<(i64, String)>> = BTreeMap::new();
    let mut per_file_authors: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut per_commit_paths: Vec<Vec<Arc<str>>> = Vec::with_capacity(metas.len());
    let mut path_intern: std::collections::HashMap<String, Arc<str>> =
        std::collections::HashMap::new();
    let mut rename_raw: Vec<(String, String, i64, String)> = Vec::new();
    let mut sn_raw: BTreeMap<String, u64> = BTreeMap::new();
    let mut lineage = entropyx_git::LineageResolver::new();
    let mut incident_times: BTreeMap<String, Vec<(i64, String)>> = BTreeMap::new();
    let mut tc_stats: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut items_cache = if opts.no_cache {
        DiskItemsCache::default()
    } else {
        DiskItemsCache::load_default()
    };

    let total_commits = metas.len();
    // The walk splits into two halves with very different constraints.
    //
    // Every piece of git I/O here — diffing a commit against its parent,
    // resolving blob shas, reading and parsing blobs for their public API
    // — depends only on that one commit. None of it needs to happen in
    // order. What *does* need to happen in order is the lineage resolver:
    // it is a union-find mutated as the walk proceeds, and a commit's
    // canonical paths depend on every rename seen before it. Running that
    // out of order would silently change which trajectory a file's history
    // lands on.
    //
    // So: fetch everything in parallel, then replay in order over data
    // that is already in memory. The replay does no I/O at all.
    let prepared = prepare_commits(path, &metas, progress)?;
    bail_if_cancelled!(progress);

    // Parse every distinct blob the walk will ask about, once, in
    // parallel. Previously each commit warmed its own slice of the cache,
    // which meant the same blob could be waited on repeatedly and the
    // parsing was serialised behind the walk.
    let items = parse_all_blobs(path, &prepared, &mut items_cache);
    bail_if_cancelled!(progress);

    let empty_items: Vec<String> = Vec::new();
    for (i, (commit, prep)) in metas.iter().zip(prepared.iter()).enumerate() {
        if i.is_multiple_of(64) {
            bail_if_cancelled!(progress);
            progress.tick(Phase::CommitLoop, i, total_commits);
        }
        let changes = &prep.changes;
        let commit_is_incident = is_incident_subject(&commit.subject);
        let commit_has_test = changes.iter().any(|c| is_test_path(&c.path));

        for ch in changes {
            if let entropyx_git::ChangeKind::Renamed { from, .. } = &ch.kind {
                lineage.union(from, &ch.path);
            }
        }

        for (ch, api) in changes.iter().zip(prep.api.iter()) {
            let canonical = lineage.canonical(&ch.path);

            if let entropyx_git::ChangeKind::Renamed { from, .. } = &ch.kind {
                rename_raw.push((
                    from.clone(),
                    ch.path.clone(),
                    commit.committer.time,
                    commit.sha.clone(),
                ));
            }
            if commit_is_incident {
                incident_times
                    .entry(canonical.clone())
                    .or_default()
                    .push((commit.committer.time, commit.sha.clone()));
            }

            if let Some(sides) = api {
                let look = |blob: &Option<String>| -> &Vec<String> {
                    blob.as_ref()
                        .and_then(|b| items.get(&(b.clone(), sides.lang)))
                        .unwrap_or(&empty_items)
                };
                let delta = entropyx_ast::public_api_delta_from_items(
                    look(&sides.old_blob),
                    look(&sides.new_blob),
                ) as u64;
                *sn_raw.entry(canonical).or_insert(0) += delta;
            }
        }

        let mut canonical_strings: Vec<String> =
            changes.iter().map(|c| lineage.canonical(&c.path)).collect();
        canonical_strings.sort();
        canonical_strings.dedup();
        let canonical_paths: Vec<Arc<str>> = canonical_strings
            .into_iter()
            .map(|s| {
                path_intern
                    .entry(s.clone())
                    .or_insert_with(|| Arc::from(s.as_str()))
                    .clone()
            })
            .collect();
        for arc_path in &canonical_paths {
            let path: &str = arc_path.as_ref();
            per_file_times
                .entry(path.to_string())
                .or_default()
                .push((commit.committer.time, commit.sha.clone()));
            per_file_authors
                .entry(path.to_string())
                .or_default()
                .push(commit.author.email.clone());
            let stats = tc_stats.entry(path.to_string()).or_insert((0u64, 0u64));
            stats.0 += 1;
            if commit_has_test && !is_test_path(path) {
                stats.1 += 1;
            }
        }
        per_commit_paths.push(canonical_paths);
    }
    progress.phase_done(
        Phase::CommitLoop,
        t.elapsed().as_millis(),
        serde_json::json!({ "commits": total_commits, "paths": per_file_times.len() }),
    );

    // Retain the walk as an evidence index before the per-file series get
    // consumed. Subjects live once in `commits`; a touch is 12 bytes.
    let mut evidence = EvidenceIndex {
        commits: metas
            .iter()
            .map(|m| CommitRef {
                sha: m.sha.clone(),
                subject: m.subject.clone(),
                author: m.author.email.clone(),
            })
            .collect(),
        by_path: BTreeMap::new(),
    };
    let sha_index: std::collections::HashMap<&str, u32> = metas
        .iter()
        .enumerate()
        .map(|(i, m)| (m.sha.as_str(), i as u32))
        .collect();
    for (path, touches) in &per_file_times {
        let refs: Vec<TouchRef> = touches
            .iter()
            .filter_map(|(time, sha)| {
                sha_index.get(sha.as_str()).map(|&commit| TouchRef {
                    commit,
                    time: *time,
                })
            })
            .collect();
        evidence.by_path.insert(path.clone(), refs);
    }

    let t = std::time::Instant::now();
    progress.phase_start(Phase::Normalize);
    let dn = unit_normalize(&change_counts(&per_commit_paths));
    let sn = unit_normalize(&sn_raw);
    progress.phase_done(
        Phase::Normalize,
        t.elapsed().as_millis(),
        serde_json::json!({}),
    );

    // ---- co-change graph. Each commit contributes a clique over the
    // paths it touched, so E grows with the square of commit width. This
    // is what makes betweenness the dominant cost.
    let t = std::time::Instant::now();
    progress.phase_start(Phase::GraphBuild);
    let graph = CoChangeGraph::from_commit_paths(&per_commit_paths);
    let mut degree_raw: BTreeMap<String, u64> = BTreeMap::new();
    for node in graph.nodes() {
        degree_raw.insert(node.clone(), graph.weighted_degree(node));
    }
    let degree_norm = unit_normalize(&degree_raw);
    let node_count = graph.nodes().len();
    let edge_count: usize = degree_raw.len();
    progress.phase_done(
        Phase::GraphBuild,
        t.elapsed().as_millis(),
        serde_json::json!({ "nodes": node_count, "degree_entries": edge_count }),
    );
    bail_if_cancelled!(progress);

    // Betweenness runs on a locally-rebuilt adjacency so it can go
    // parallel; `crates/exbridge/src/brandes.rs` reproduces upstream's
    // node ordering and accumulation order exactly, and its tests assert
    // bit-for-bit equality against `CoChangeGraph`.
    let t = std::time::Instant::now();
    progress.phase_start(Phase::Betweenness);
    let betweenness = {
        let bg = crate::brandes::Graph::from_commit_paths(&per_commit_paths);
        if std::env::var("EXBRIDGE_GRAPH_STATS").is_ok() {
            eprintln!(
                "graph: nodes={} degree_entries={} mean_degree={:.1}",
                bg.node_count(),
                bg.degree_entries(),
                bg.mean_degree()
            );
        }
        bg.betweenness_centrality()
    };
    let cs: BTreeMap<String, f64> = degree_norm
        .iter()
        .map(|(path, &d)| {
            let b = *betweenness.get(path).unwrap_or(&0.0);
            (path.clone(), d.max(b))
        })
        .collect();
    progress.phase_done(
        Phase::Betweenness,
        t.elapsed().as_millis(),
        serde_json::json!({ "nodes": node_count }),
    );
    bail_if_cancelled!(progress);

    let mut vt = VertexTable::new();
    for path in per_file_times.keys() {
        vt.intern_file(path);
    }
    for commit in &metas {
        vt.intern_author(&commit.author.email);
    }

    let t = std::time::Instant::now();
    progress.phase_start(Phase::HeadTree);
    let head_entries: BTreeMap<String, String> = repo
        .head_tree_entries()
        .map_err(|e| ScanError::HeadTree(e.to_string()))?
        .into_iter()
        .collect();
    progress.phase_done(
        Phase::HeadTree,
        t.elapsed().as_millis(),
        serde_json::json!({ "head_files": head_entries.len() }),
    );

    let repo_first = metas.iter().map(|m| m.committer.time).min().unwrap_or(0);
    let repo_last = metas.iter().map(|m| m.committer.time).max().unwrap_or(0);

    // ---- blame. One `git blame` subprocess per file at HEAD, and the
    // reason the pipeline cannot run under wasm at all.
    //
    // Upstream runs these serially, which is 7s of mostly-waiting on a
    // 780-file repository: each call blocks on a child process. They are
    // independent, so they run in parallel here. `blame_youth` is a pure
    // function of the line times and results land in a `BTreeMap`, so the
    // output does not depend on completion order.
    //
    // `Repo` is not shared across threads — each worker opens its own,
    // once per chunk rather than once per file.
    let t = std::time::Instant::now();
    progress.phase_start(Phase::Blame);
    let head_paths: Vec<String> = head_entries.keys().cloned().collect();
    let head_total = head_paths.len();
    let done = std::sync::atomic::AtomicUsize::new(0);

    let chunk = head_total
        .div_ceil(rayon::current_num_threads().max(1))
        .max(1);
    let blamed: Vec<Vec<(String, f64)>> = head_paths
        .par_chunks(chunk)
        .map(|paths| {
            let Ok(repo) = entropyx_git::Repo::open(path) else {
                return Vec::new();
            };
            let mut out = Vec::with_capacity(paths.len());
            for p in paths {
                if let Ok(lines) = repo.blame(p) {
                    let times: Vec<i64> = lines.iter().map(|l| l.author_time).collect();
                    out.push((p.clone(), blame_youth(&times, repo_first, repo_last)));
                }
                let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if n.is_multiple_of(32) {
                    progress.tick(Phase::Blame, n, head_total);
                }
            }
            out
        })
        .collect();
    let by_map: BTreeMap<String, f64> = blamed.into_iter().flatten().collect();
    bail_if_cancelled!(progress);
    progress.phase_done(
        Phase::Blame,
        t.elapsed().as_millis(),
        serde_json::json!({ "blamed": by_map.len(), "head_files": head_total }),
    );

    let t = std::time::Instant::now();
    progress.phase_start(Phase::Rows);
    let mut rows: Vec<FileRow> = Vec::with_capacity(per_file_times.len());
    let mut api_drift_emissions: Vec<(String, i64, String, u32)> = Vec::new();
    for (path, times) in &per_file_times {
        let authors = &per_file_authors[path];
        let fid = vt.intern_file(path);
        let d_n = *dn.get(path).unwrap_or(&0.0);
        let h_a = author_dispersion(authors);
        let raw_times: Vec<i64> = times.iter().map(|(t, _)| *t).collect();
        let v_t = saturate_unit(temporal_volatility(&raw_times));
        let c_s = *cs.get(path).unwrap_or(&0.0);
        let b_y = *by_map.get(path).unwrap_or(&0.0);
        let s_n = *sn.get(path).unwrap_or(&0.0);
        let t_c = if is_test_path(path) {
            1.0
        } else {
            let (total, cotouch) = tc_stats.get(path).copied().unwrap_or((0, 0));
            if total > 0 {
                cotouch as f64 / total as f64
            } else {
                0.0
            }
        };
        let components = MetricComponents {
            change_density: d_n,
            author_entropy: h_a,
            temporal_volatility: v_t,
            coupling_stress: c_s,
            blame_youth: b_y,
            semantic_drift: s_n,
            test_cooevolution: t_c,
        };
        let composite = components.composite(weights);
        let in_aftershock = v_t > 0.3 && incident_times.get(path).is_some_and(|v| !v.is_empty());
        let signal_class = if in_aftershock {
            Some(SignalClass::IncidentAftershock)
        } else {
            classify(&components)
        };
        if signal_class == Some(SignalClass::ApiDrift) {
            let latest = times.iter().max_by_key(|(t, _)| *t);
            let (at, sha) = match latest {
                Some((t, s)) => (*t, s.clone()),
                None => (0, String::new()),
            };
            let raw = sn_raw.get(path).copied().unwrap_or(0) as u32;
            api_drift_emissions.push((path.clone(), at, sha, raw));
        }
        rows.push(FileRow {
            file: fid,
            values: [d_n, h_a, v_t, c_s, b_y, s_n, t_c, composite],
            lineage_confidence: 1.0,
            signal_class,
        });
    }
    progress.phase_done(
        Phase::Rows,
        t.elapsed().as_millis(),
        serde_json::json!({ "rows": rows.len() }),
    );

    let t = std::time::Instant::now();
    progress.phase_start(Phase::Events);
    let mut events: Vec<Event> = rename_raw
        .iter()
        .map(|(from, to, at, sha)| {
            let canonical_to = lineage.canonical(to);
            Event::Rename {
                file: vt.intern_file(&canonical_to),
                at: Timestamp(*at),
                sha: sha.clone(),
                from: from.clone(),
                to: to.clone(),
            }
        })
        .collect();

    const HOTSPOT_THRESHOLD: f64 = 0.5;
    for (path, times) in &per_file_times {
        let raw_times: Vec<i64> = times.iter().map(|(t, _)| *t).collect();
        if let Some(at) = detect_recent_burst(&raw_times, HOTSPOT_THRESHOLD) {
            let sha = times
                .iter()
                .find(|(t, _)| *t == at)
                .map(|(_, s)| s.clone())
                .unwrap_or_default();
            events.push(Event::Hotspot {
                file: vt.intern_file(path),
                at: Timestamp(at),
                sha,
                reason: "recent_burst".to_string(),
            });
        }
    }

    for (path, times) in &per_file_times {
        let Some(authors) = per_file_authors.get(path) else {
            continue;
        };
        let mut chrono: Vec<(i64, &str)> = times
            .iter()
            .zip(authors.iter())
            .map(|((t, _), a)| (*t, a.as_str()))
            .collect();
        chrono.sort_by_key(|(t, _)| *t);
        if let Some((at, split_authors)) = detect_ownership_split(&chrono) {
            let author_ids = split_authors
                .into_iter()
                .map(|a| vt.intern_author(a))
                .collect();
            let sha = times
                .iter()
                .find(|(t, _)| *t == at)
                .map(|(_, s)| s.clone())
                .unwrap_or_default();
            events.push(Event::OwnershipSplit {
                file: vt.intern_file(path),
                at: Timestamp(at),
                sha,
                authors: author_ids,
            });
        }
    }

    const AFTERSHOCK_VT_THRESHOLD: f64 = 0.3;
    for (path, inc_times) in &incident_times {
        if inc_times.is_empty() {
            continue;
        }
        let Some(times) = per_file_times.get(path) else {
            continue;
        };
        let raw_times: Vec<i64> = times.iter().map(|(t, _)| *t).collect();
        let v_t = saturate_unit(temporal_volatility(&raw_times));
        if v_t <= AFTERSHOCK_VT_THRESHOLD {
            continue;
        }
        let latest = inc_times.iter().max_by_key(|(t, _)| *t).unwrap();
        let at = latest.0;
        let sha = latest.1.clone();
        let first = inc_times.iter().map(|(t, _)| *t).min().unwrap();
        let window_days = ((at - first) / 86_400).max(0) as u32;
        events.push(Event::IncidentAftershock {
            file: vt.intern_file(path),
            at: Timestamp(at),
            sha,
            window_days,
        });
    }

    for (path, at, sha, pub_items_changed) in api_drift_emissions {
        events.push(Event::ApiDrift {
            file: vt.intern_file(&path),
            at: Timestamp(at),
            sha,
            pub_items_changed,
        });
    }
    progress.phase_done(
        Phase::Events,
        t.elapsed().as_millis(),
        serde_json::json!({ "events": events.len() }),
    );

    let t = std::time::Instant::now();
    progress.phase_start(Phase::Handles);
    let mut handles: BTreeMap<String, Handle> = BTreeMap::new();
    for path in per_file_times.keys() {
        if let Some(blob_sha) = head_entries.get(path) {
            let fid = vt.intern_file(path);
            let handle = Handle::file(fid, blob_sha);
            handles.insert(handle.key(), handle);
        }
    }
    progress.phase_done(
        Phase::Handles,
        t.elapsed().as_millis(),
        serde_json::json!({ "handles": handles.len() }),
    );

    if !opts.no_cache
        && let Err(e) = items_cache.save()
    {
        eprintln!("exbridge: warning — could not save items cache: {e}");
    }

    Ok((
        Summary {
            schema: Schema::default(),
            dict: Dict::from_vertex(&vt),
            files: rows,
            events,
            handles,
            enrichments: Enrichments::default(),
        },
        evidence,
    ))
}

/// A parsed blob's public-API items, keyed the way the disk cache keys
/// them: the same blob under two languages is two entries.
type BlobKey = (String, entropyx_ast::Language);
type ItemIndex = HashMap<BlobKey, Vec<String>>;

/// What one commit contributes, once its git I/O is done.
pub struct PreparedCommit {
    changes: Vec<entropyx_git::FileChange>,
    /// Parallel to `changes`. `None` where the path has no language we
    /// can parse, which is most of them.
    api: Vec<Option<ApiSides>>,
}

/// The two blobs whose public-API difference gives a change its S_n
/// contribution. Either side is `None` when there is nothing to read —
/// a root commit's parent, or a deleted file's new side.
struct ApiSides {
    old_blob: Option<String>,
    new_blob: Option<String>,
    lang: entropyx_ast::Language,
}

/// Diff every commit and resolve the blobs its S_n term needs, in
/// parallel.
///
/// Each worker opens its own `Repo` once per chunk. Chunks are contiguous
/// and `par_chunks` preserves order, so the returned vector lines up with
/// `metas` — which the ordered replay depends on.
fn prepare_commits<P: Progress>(
    repo_path: &str,
    metas: &[entropyx_git::CommitMeta],
    progress: &P,
) -> Result<Vec<PreparedCommit>, ScanError> {
    let total = metas.len();
    let done = std::sync::atomic::AtomicUsize::new(0);
    let chunk = total.div_ceil(rayon::current_num_threads().max(1)).max(1);

    let chunks: Vec<Result<Vec<PreparedCommit>, ScanError>> = metas
        .par_chunks(chunk)
        .map(|group| {
            let repo =
                entropyx_git::Repo::open(repo_path).map_err(|e| ScanError::Open(e.to_string()))?;
            let mut out = Vec::with_capacity(group.len());
            for commit in group {
                let changes = repo
                    .diff_from_parent(&commit.sha)
                    .map_err(|e| ScanError::Diff(format!("{} at {}", e, commit.sha)))?;
                let parent_sha = commit.parents.first().map(String::as_str);

                let api = changes
                    .iter()
                    .map(|ch| {
                        let lang = entropyx_ast::language_from_path(&ch.path)?;
                        let old_side = match &ch.kind {
                            entropyx_git::ChangeKind::Renamed { from, .. }
                            | entropyx_git::ChangeKind::Copied { from, .. } => from.as_str(),
                            _ => ch.path.as_str(),
                        };
                        let old_blob =
                            parent_sha.and_then(|ps| repo.blob_sha_at(ps, old_side).ok().flatten());
                        let new_blob = if matches!(&ch.kind, entropyx_git::ChangeKind::Deleted) {
                            None
                        } else {
                            repo.blob_sha_at(&commit.sha, &ch.path).ok().flatten()
                        };
                        Some(ApiSides {
                            old_blob,
                            new_blob,
                            lang,
                        })
                    })
                    .collect();

                out.push(PreparedCommit { changes, api });
                let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if n.is_multiple_of(64) {
                    progress.tick(Phase::CommitLoop, n, total);
                }
            }
            Ok(out)
        })
        .collect();

    let mut prepared = Vec::with_capacity(total);
    for c in chunks {
        prepared.extend(c?);
    }
    Ok(prepared)
}

/// Read and parse every distinct blob the walk will ask about.
///
/// Keyed on `(blob sha, language)` exactly as the disk cache is, so a
/// blob that appears as both "new side here" and "old side there" is
/// parsed once. Entries already on disk are not re-parsed; new ones are
/// written back so the next run skips them too.
fn parse_all_blobs(
    repo_path: &str,
    prepared: &[PreparedCommit],
    cache: &mut DiskItemsCache,
) -> ItemIndex {
    let mut wanted: HashSet<BlobKey> = HashSet::new();
    for prep in prepared {
        for sides in prep.api.iter().flatten() {
            for blob in [&sides.old_blob, &sides.new_blob].into_iter().flatten() {
                wanted.insert((blob.clone(), sides.lang));
            }
        }
    }

    let mut items: ItemIndex = HashMap::new();
    let mut to_parse: Vec<BlobKey> = Vec::new();
    for key in wanted {
        match cache.get(&key.0, key.1) {
            Some(v) => {
                items.insert(key, v);
            }
            None => to_parse.push(key),
        }
    }

    // Sorted by blob sha so the work splits the same way every run.
    // Parsing is pure, so order cannot affect the result, but a stable
    // split keeps timings comparable between runs. (`Language` is not
    // `Ord`; the sha alone is enough to make the order deterministic.)
    to_parse.sort_by(|a, b| a.0.cmp(&b.0));
    let chunk = to_parse
        .len()
        .div_ceil(rayon::current_num_threads().max(1))
        .max(1);
    let parsed: Vec<Vec<(BlobKey, Vec<String>)>> = to_parse
        .par_chunks(chunk)
        .map(|group| {
            let Ok(repo) = entropyx_git::Repo::open(repo_path) else {
                return Vec::new();
            };
            group
                .iter()
                .map(|(sha, lang)| {
                    let content = repo.blob_by_sha(sha).ok().flatten().unwrap_or_default();
                    let parsed =
                        entropyx_ast::parse_public_items(&content, *lang).unwrap_or_default();
                    ((sha.clone(), *lang), parsed)
                })
                .collect()
        })
        .collect();

    for (key, v) in parsed.into_iter().flatten() {
        cache.insert(key.0.clone(), key.1, v.clone());
        items.insert(key, v);
    }
    items
}
