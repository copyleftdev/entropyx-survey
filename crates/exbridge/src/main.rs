//! exbridge — streams entropyx scans to the browser over SSE.
//!
//! Why SSE and not a plain JSON endpoint: a scan has unbounded, and
//! largely unpredictable, latency. Measured cold scans in this workspace
//! run from 2s to 755s, and the cost is driven by co-change graph density
//! rather than anything cheap you can read up front. A request/response
//! fetch would sit silent for twelve minutes. See `docs/PERF.md`.

use exbridge::{fleets, people, pipeline, sse};

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, Sse};
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio_stream::wrappers::UnboundedReceiverStream;

use pipeline::{Phase, ScanOptions};

/// Completed scans, keyed by `(canonical repo path, HEAD sha)`. A cold
/// scan of a large repo costs minutes; re-serving one costs nothing, and
/// the key changes the moment the repo moves forward.
type ScanCache = Arc<Mutex<HashMap<String, Arc<CachedScan>>>>;

struct CachedScan {
    summary_json: serde_json::Value,
    elapsed_ms: u128,
    digest: String,
    /// The walk, kept so `explain` never has to redo it.
    evidence: pipeline::EvidenceIndex,
    /// Handle key → canonical path, so a drill-down is a map lookup.
    handle_paths: HashMap<String, String>,
}

#[derive(Clone)]
struct AppState {
    cache: ScanCache,
    repo_root: PathBuf,
}

#[tokio::main]
async fn main() {
    // Default to wherever the server was started. Run it inside a
    // repository and that repository is offered; run it in a directory of
    // repositories and they are all listed. Nothing is assumed about how
    // anyone lays out their disk.
    let repo_root = std::env::var("EXBRIDGE_REPO_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    let repo_root = std::fs::canonicalize(&repo_root).unwrap_or(repo_root);
    let port: u16 = std::env::var("EXBRIDGE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7878);
    let web_dir = std::env::var("EXBRIDGE_WEB_DIR").unwrap_or_else(|_| "web".to_string());

    let state = AppState {
        cache: Arc::new(Mutex::new(HashMap::new())),
        repo_root: repo_root.clone(),
    };

    let app = Router::new()
        .route("/api/describe", get(describe))
        .route("/api/repos", get(list_repos))
        .route("/api/explain", get(explain))
        .route("/api/fleets", get(fleet_divergence))
        .route("/api/people", get(people_enrichment))
        .route("/api/scan", get(scan_stream))
        .fallback_service(
            tower_http::services::ServeDir::new(&web_dir).append_index_html_on_directories(true),
        )
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(state);

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    println!("exbridge listening on http://{addr}");
    println!("  repo root : {}", repo_root.display());
    println!("  web dir   : {web_dir}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("serve");
}

async fn describe() -> Json<serde_json::Value> {
    let d = entropyx_core::Describe::current();
    let mut v = serde_json::to_value(&d).unwrap_or(serde_json::json!({}));
    // Advertise what the bridge adds on top of the CLI contract, so the
    // frontend can feature-detect rather than hardcode.
    v["bridge"] = serde_json::json!({
        "transport": "sse",
        "phases": Phase::ALL.iter().map(|p| serde_json::json!({
            "id": p.id(), "label": p.label(), "weight": p.weight()
        })).collect::<Vec<_>>(),
        "metric_columns": entropyx_tq::Dict::METRIC_COLUMNS,
    });
    Json(v)
}

#[derive(Deserialize)]
struct ReposQuery {
    #[serde(default)]
    root: Option<String>,
}

/// Shallow scan of the configured root for git repositories. One level
/// deep only — deep recursion over a large Projects directory is slow
/// enough to be its own UX problem.
async fn list_repos(
    State(st): State<AppState>,
    Query(q): Query<ReposQuery>,
) -> Json<serde_json::Value> {
    let root = q.root.map(PathBuf::from).unwrap_or(st.repo_root.clone());
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let mut out: Vec<serde_json::Value> = Vec::new();

    let entry = |p: &Path| {
        serde_json::json!({
            "name": p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
            "path": p.to_string_lossy(),
        })
    };

    // The root may itself be a repository — the common case when the
    // server is started from inside one.
    if root.join(".git").exists() {
        out.push(entry(&root));
    }
    if let Ok(entries) = std::fs::read_dir(&root) {
        for e in entries.flatten() {
            let p = e.path();
            if p.join(".git").exists() {
                out.push(entry(&p));
            }
        }
    }
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    out.dedup_by(|a, b| a["path"] == b["path"]);
    Json(serde_json::json!({ "root": root.to_string_lossy(), "repos": out }))
}

#[derive(Deserialize)]
struct ExplainQuery {
    repo: String,
    handle: String,
}

/// Drill-down: the commits behind one file.
///
/// Served from the walk the scan already did. `entropyx explain` reopens
/// the repository and re-walks history on every call — 0.47s on a
/// 248-commit repo, 1.21s on a 1,925-commit one — which is a visible
/// stall on every cell click. The cached `EvidenceIndex` turns that into
/// a map lookup. The CLI remains the fallback for anything not in cache
/// (a repo scanned before this server started, or a non-file handle).
async fn explain(State(st): State<AppState>, Query(q): Query<ExplainQuery>) -> impl IntoResponse {
    let canonical = std::fs::canonicalize(&q.repo)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| q.repo.clone());
    let key = format!(
        "{canonical}@{}",
        head_sha(&canonical).unwrap_or_else(|| "unknown".into())
    );

    let cached = st.cache.lock().ok().and_then(|c| c.get(&key).cloned());
    if let Some(scan) = cached
        && let Some(v) = explain_from_index(&scan, &q.handle)
    {
        return (StatusCode::OK, Json(v));
    }
    explain_via_cli(&q).await
}

/// Reproduce `entropyx explain`'s file view from the retained walk.
fn explain_from_index(scan: &CachedScan, handle: &str) -> Option<serde_json::Value> {
    // Accept a handle key, or a bare path for callers that have one.
    let path = scan.handle_paths.get(handle).cloned().or_else(|| {
        scan.evidence
            .by_path
            .contains_key(handle)
            .then(|| handle.to_string())
    })?;

    let touches = scan.evidence.touches(&path);
    let commits: Vec<serde_json::Value> = touches
        .iter()
        .filter_map(|t| scan.evidence.commits.get(t.commit as usize))
        .map(|c| {
            serde_json::json!({
                "author": c.author,
                "sha": c.sha,
                "subject": c.subject,
            })
        })
        .zip(touches.iter())
        .map(|(mut v, t)| {
            v["time"] = serde_json::json!(t.time);
            v
        })
        .collect();

    let times: Vec<i64> = touches.iter().map(|t| t.time).collect();
    let authors: Vec<&str> = touches
        .iter()
        .filter_map(|t| scan.evidence.commits.get(t.commit as usize))
        .map(|c| c.author.as_str())
        .collect();

    // Share per author, ordered by share descending then email, so the
    // ordering is stable across calls.
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for a in &authors {
        *counts.entry(a).or_insert(0) += 1;
    }
    let total = authors.len() as f64;
    let mut top: Vec<(&str, f64)> = counts
        .into_iter()
        .map(|(email, n)| (email, if total > 0.0 { n as f64 / total } else { 0.0 }))
        .collect();
    top.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(b.0))
    });

    // The CLI caps this list at five; match it so the two views agree.
    top.truncate(5);

    Some(serde_json::json!({
        "schema": { "name": "entropyx-explain", "version": "0.1.0" },
        "kind": "file",
        "path": path,
        "commits": commits,
        "commits_touched": touches.len(),
        "first_commit_time": times.iter().min(),
        "last_commit_time": times.iter().max(),
        "metrics": {
            "author_dispersion": entropyx_core::metric::author_dispersion(&authors),
            "author_entropy_nats": entropyx_core::metric::author_entropy_nats(&authors),
            "change_count": touches.len(),
            "temporal_volatility": entropyx_core::metric::temporal_volatility(&times),
        },
        "top_authors": top
            .into_iter()
            .map(|(email, share)| serde_json::json!({ "email": email, "share": share }))
            .collect::<Vec<_>>(),
        "served_from": "index",
        // Evidence follows the file's *trajectory* — its history under
        // former names as well as its current one — because that is what
        // the score displayed beside it was computed from. `entropyx
        // explain` walks the literal path only, so for a renamed file it
        // reports fewer commits than the metrics were derived from. The
        // UI says when this applies.
        "lineage": "trajectory",
    }))
}

async fn explain_via_cli(q: &ExplainQuery) -> (StatusCode, Json<serde_json::Value>) {
    let out = tokio::process::Command::new("entropyx")
        .arg("explain")
        .arg(&q.repo)
        .arg(&q.handle)
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            match serde_json::from_slice::<serde_json::Value>(&o.stdout) {
                Ok(mut v) => {
                    v["served_from"] = serde_json::json!("cli");
                    (StatusCode::OK, Json(v))
                }
                Err(e) => (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({ "error": format!("bad JSON from entropyx: {e}") })),
                ),
            }
        }
        Ok(o) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": String::from_utf8_lossy(&o.stderr).trim().to_string()
            })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("spawn failed: {e}") })),
        ),
    }
}

#[derive(Deserialize)]
struct FleetsQuery {
    repo: String,
}

/// Divergence layer: peer artifact sets at HEAD, compared with `wtd`.
///
/// Kept off the scan stream deliberately. It is fast (no graph work) and
/// independent of the tq1 summary, so the sheet loads it after the
/// terrain is already drawn rather than making a 12-minute scan any
/// longer.
async fn fleet_divergence(Query(q): Query<FleetsQuery>) -> impl IntoResponse {
    let repo = match std::fs::canonicalize(&q.repo) {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("{}: {e}", q.repo) })),
            );
        }
    };

    let paths = match tokio::task::spawn_blocking({
        let repo = repo.clone();
        move || {
            entropyx_git::Repo::open(&repo)
                .and_then(|r| r.head_tree_entries())
                .map(|v| v.into_iter().map(|(p, _)| p).collect::<Vec<String>>())
                .map_err(|e| e.to_string())
        }
    })
    .await
    {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e })),
            );
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            );
        }
    };

    let analysis = tokio::task::spawn_blocking(move || fleets::analyze(&repo, &paths)).await;
    match analysis {
        Ok(a) => (
            StatusCode::OK,
            Json(serde_json::to_value(&a).unwrap_or(serde_json::json!({}))),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

#[derive(Deserialize)]
struct PeopleQuery {
    repo: String,
    /// Override the crawl seed. The default is the repo's own GitHub
    /// slug; pointing at a prolific contributor's login instead is the
    /// documented way to map a team ("crawl the hub, not the leaf").
    #[serde(default)]
    seed: Option<String>,
}

/// Contributor identities, via kraken. Optional by construction: a repo
/// with no GitHub origin, or a machine with no token, gets a populated
/// `reason` rather than an error, and the sheet simply omits the layer.
async fn people_enrichment(Query(q): Query<PeopleQuery>) -> impl IntoResponse {
    let repo = match std::fs::canonicalize(&q.repo) {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("{}: {e}", q.repo) })),
            );
        }
    };

    // Coverage is measured against the addresses in the local history,
    // so read them here rather than trusting kraken's own view.
    let authors = tokio::task::spawn_blocking({
        let repo = repo.clone();
        move || collect_authors(&repo)
    })
    .await
    .unwrap_or_default();

    let seed = q.seed.clone();
    let report =
        tokio::task::spawn_blocking(move || people::enrich(&repo, &authors, seed.as_deref())).await;

    match report {
        Ok(r) => (
            StatusCode::OK,
            Json(serde_json::to_value(&r).unwrap_or(serde_json::json!({}))),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

/// Distinct commit-author addresses in the local history.
fn collect_authors(repo: &str) -> Vec<String> {
    let Ok(out) = std::process::Command::new("git")
        .args(["log", "--format=%ae"])
        .current_dir(repo)
        .output()
    else {
        return Vec::new();
    };
    let mut seen = std::collections::BTreeSet::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let e = line.trim().to_ascii_lowercase();
        if !e.is_empty() {
            seen.insert(e);
        }
    }
    seen.into_iter().collect()
}

#[derive(Deserialize)]
struct ScanQuery {
    repo: String,
    #[serde(default)]
    since: Option<usize>,
    #[serde(default)]
    no_cache: Option<bool>,
    /// Bypass the bridge's own completed-scan cache (distinct from
    /// entropyx's AST disk cache, which `no_cache` controls).
    #[serde(default)]
    fresh: Option<bool>,
}

async fn scan_stream(State(st): State<AppState>, Query(q): Query<ScanQuery>) -> impl IntoResponse {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<SseEvent, Infallible>>();

    let repo = q.repo.clone();
    let opts = ScanOptions {
        since: q.since,
        no_cache: q.no_cache.unwrap_or(false),
        ..Default::default()
    };
    let fresh = q.fresh.unwrap_or(false);
    let cache = st.cache.clone();

    tokio::task::spawn_blocking(move || {
        let emitter = sse::Emitter::new(tx);
        run_scan(&repo, opts, fresh, cache, &emitter);
    });

    Sse::new(UnboundedReceiverStream::new(rx)).keep_alive(
        axum::response::sse::KeepAlive::new().interval(std::time::Duration::from_secs(15)),
    )
}

fn run_scan(repo: &str, opts: ScanOptions, fresh: bool, cache: ScanCache, em: &sse::Emitter) {
    let started = std::time::Instant::now();
    let canonical = std::fs::canonicalize(repo)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| repo.to_string());

    if !Path::new(&canonical).join(".git").exists() {
        em.error(&format!("not a git repository: {canonical}"));
        return;
    }

    let head = head_sha(&canonical);
    let key = format!("{canonical}@{}", head.as_deref().unwrap_or("unknown"));

    em.meta(serde_json::json!({
        "repo": canonical,
        "head": head,
        "phases": Phase::ALL.iter().map(|p| serde_json::json!({
            "id": p.id(), "label": p.label(), "weight": p.weight()
        })).collect::<Vec<_>>(),
    }));

    if !fresh && let Some(hit) = cache.lock().ok().and_then(|c| c.get(&key).cloned()) {
        em.cached();
        emit_summary(em, &hit.summary_json);
        em.done(serde_json::json!({
            "elapsed_ms": 0,
            "original_elapsed_ms": hit.elapsed_ms,
            "cached": true,
            "digest": hit.digest,
        }));
        return;
    }

    match pipeline::scan(&canonical, &opts, em) {
        Ok((summary, evidence)) => {
            // Handle key → path, resolved once here rather than scanned
            // linearly on every drill-down.
            let handle_paths: HashMap<String, String> = summary
                .handles
                .iter()
                .filter_map(|(key, h)| match h {
                    entropyx_core::Handle::File { file, .. } => summary
                        .dict
                        .files
                        .get(file.index())
                        .map(|p| (key.clone(), p.clone())),
                    _ => None,
                })
                .collect();
            let value = match serde_json::to_value(&summary) {
                Ok(v) => v,
                Err(e) => {
                    em.error(&format!("serialization failed: {e}"));
                    return;
                }
            };
            let elapsed_ms = started.elapsed().as_millis();
            // Digest the canonical serialization, not the pretty form —
            // entropyx guarantees bitwise-identical output for identical
            // inputs, so this is a determinism receipt the UI can show.
            let digest = serde_json::to_vec(&value)
                .map(|b| blake3::hash(&b).to_hex().to_string())
                .unwrap_or_default();

            emit_summary(em, &value);
            em.done(serde_json::json!({
                "elapsed_ms": elapsed_ms,
                "cached": false,
                "digest": digest,
            }));

            if let Ok(mut c) = cache.lock() {
                c.insert(
                    key,
                    Arc::new(CachedScan {
                        summary_json: value,
                        elapsed_ms,
                        digest,
                        evidence,
                        handle_paths,
                    }),
                );
            }
        }
        Err(pipeline::ScanError::Cancelled) => { /* client vanished; stay quiet */ }
        Err(e) => em.error(&e.to_string()),
    }
}

/// Chunked emission. Rows go out in composite-descending order so the UI
/// can start unfolding the highest-signal files immediately instead of
/// waiting for the tail — the `file` FileId makes row order irrelevant to
/// correctness.
fn emit_summary(em: &sse::Emitter, summary: &serde_json::Value) {
    const CHUNK: usize = 150;

    if let Some(dict) = summary.get("dict") {
        em.send("dict", dict.clone());
    }

    let empty = Vec::new();
    let mut rows: Vec<&serde_json::Value> = summary
        .get("files")
        .and_then(|f| f.as_array())
        .unwrap_or(&empty)
        .iter()
        .collect();
    rows.sort_by(|a, b| {
        let ca = a["values"][7].as_f64().unwrap_or(0.0);
        let cb = b["values"][7].as_f64().unwrap_or(0.0);
        cb.partial_cmp(&ca).unwrap_or(std::cmp::Ordering::Equal)
    });
    let total = rows.len();
    for (i, chunk) in rows.chunks(CHUNK).enumerate() {
        em.send(
            "rows",
            serde_json::json!({
                "offset": i * CHUNK,
                "total": total,
                "rows": chunk,
            }),
        );
    }

    let events = summary
        .get("events")
        .and_then(|e| e.as_array())
        .unwrap_or(&empty);
    let etotal = events.len();
    for (i, chunk) in events.chunks(CHUNK).enumerate() {
        em.send(
            "events",
            serde_json::json!({
                "offset": i * CHUNK,
                "total": etotal,
                "events": chunk,
            }),
        );
    }

    if let Some(h) = summary.get("handles") {
        em.send("handles", h.clone());
    }
}

fn head_sha(repo: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
