//! Fleet detection and `wtd` invocation.
//!
//! entropyx answers "where is the risk". WhatTheDiff answers a different
//! question — "where does a set of peer artifacts disagree" — and the two
//! join on file path. A file that entropyx flags as churning *and* that
//! `wtd` shows deviating from its eleven siblings is a much sharper
//! finding than either tool produces alone.
//!
//! The hard part is deciding what counts as a peer. `wtd` will happily
//! compare any two files, but two unrelated Rust sources score ~0.98
//! drift because almost every primitive is unique to one of them — that
//! is noise, not signal. So detection here is deliberately conservative:
//! artifacts are peers when they share a basename across directories, or
//! share a config-ish extension within one directory.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::process::Command;

/// Extensions worth comparing within a single directory. Deliberately
/// excludes source code: a directory of `.ts` files is a module, not a
/// fleet, and comparing them produces drift near 1.0 for everyone.
const FLEETABLE_EXT: &[&str] = &[
    "yml",
    "yaml",
    "json",
    "jsonc",
    "toml",
    "ini",
    "cfg",
    "conf",
    "env",
    "xml",
    "properties",
    "tf",
    "tfvars",
    "sql",
    "http",
    "plist",
    "gradle",
    "props",
];

/// Source extensions never eligible for the same-basename rule. Nine
/// `lib.rs` files share a name because of language convention, not
/// because they are variants of one another.
const SOURCE_EXT: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "go", "java", "rb", "cpp", "cc", "c", "h",
    "hpp", "cs", "swift", "kt", "php", "scala", "ex", "exs", "zig", "lua", "pl", "sh", "bash",
    "md", "txt", "html", "css", "scss",
];

/// Extensionless files that are still meaningful to compare by name.
const NAMED_FLEETS: &[&str] = &[
    "Dockerfile",
    "Makefile",
    "Justfile",
    "Jenkinsfile",
    "Procfile",
    "Vagrantfile",
    "CODEOWNERS",
];

/// Bounds. `wtd` takes every member on the command line, and a 400-file
/// fleet is both slow and unreadable. Truncation is reported, never
/// silent.
const MAX_MEMBERS: usize = 60;
const MAX_FLEETS: usize = 24;
const MIN_BASENAME_MEMBERS: usize = 2;
const MIN_EXT_MEMBERS: usize = 3;

#[derive(Clone, Debug, Serialize)]
pub struct Fleet {
    pub id: String,
    pub label: String,
    /// `basename` or `extension` — which rule matched.
    pub rule: &'static str,
    pub members: Vec<String>,
    /// Members dropped by `MAX_MEMBERS`, so the UI can say so.
    pub truncated: usize,
}

/// Group HEAD paths into peer sets. Input is repo-relative paths, as
/// they appear in the tq1 dictionary, so the output joins to it directly.
pub fn detect(paths: &[String]) -> Vec<Fleet> {
    let mut by_basename: BTreeMap<&str, Vec<&String>> = BTreeMap::new();
    let mut by_dir_ext: BTreeMap<(&str, &str), Vec<&String>> = BTreeMap::new();

    for p in paths {
        let base = p.rsplit('/').next().unwrap_or(p.as_str());
        let dir = p.rfind('/').map(|i| &p[..i]).unwrap_or("");
        let ext = base.rsplit_once('.').map(|(_, e)| e).unwrap_or("");

        let named = NAMED_FLEETS.iter().any(|n| n.eq_ignore_ascii_case(base));
        if named || (!ext.is_empty() && !SOURCE_EXT.contains(&ext)) {
            by_basename.entry(base).or_default().push(p);
        }
        if FLEETABLE_EXT.contains(&ext) {
            by_dir_ext.entry((dir, ext)).or_default().push(p);
        }
    }

    let mut fleets: Vec<Fleet> = Vec::new();

    for (base, mut members) in by_basename {
        // Two files with the same name in the same directory is
        // impossible; requiring distinct directories keeps this rule
        // meaning "the same artifact, once per component".
        if members.len() < MIN_BASENAME_MEMBERS {
            continue;
        }
        members.sort();
        let (members, truncated) = cap(members);
        fleets.push(Fleet {
            id: format!("basename:{base}"),
            label: format!("{base} × {}", members.len() + truncated),
            rule: "basename",
            members,
            truncated,
        });
    }

    for ((dir, ext), mut members) in by_dir_ext {
        if members.len() < MIN_EXT_MEMBERS {
            continue;
        }
        // Skip when a basename fleet already covers this exact set —
        // otherwise `k8s/*.yaml` and `deployment.yaml × 4` both report
        // the same disagreement.
        if fleets.iter().any(|f| {
            f.members.len() == members.len() && f.members.iter().all(|m| members.contains(&m))
        }) {
            continue;
        }
        members.sort();
        let (members, truncated) = cap(members);
        let where_ = if dir.is_empty() {
            "repository root"
        } else {
            dir
        };
        fleets.push(Fleet {
            id: format!("ext:{dir}:{ext}"),
            label: format!("{}/*.{ext} × {}", where_, members.len() + truncated),
            rule: "extension",
            members,
            truncated,
        });
    }

    // Largest fleets first — they carry the most consensus signal, and
    // the cap below should keep the informative ones.
    fleets.sort_by(|a, b| b.members.len().cmp(&a.members.len()).then(a.id.cmp(&b.id)));
    fleets.truncate(MAX_FLEETS);
    fleets
}

fn cap(members: Vec<&String>) -> (Vec<String>, usize) {
    let total = members.len();
    let kept: Vec<String> = members
        .into_iter()
        .take(MAX_MEMBERS)
        .map(String::clone)
        .collect();
    let truncated = total.saturating_sub(kept.len());
    (kept, truncated)
}

// ---- wtd report subset ---------------------------------------------
// Only the fields consumed here are modelled; `wtd.report.v1` carries a
// full evidence graph that would bloat the payload for no gain.

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WtdReport {
    pub corpus: Corpus,
    pub consensus: Consensus,
    pub drift: DriftStats,
    #[serde(default)]
    pub conflicts: Vec<Conflict>,
    #[serde(default)]
    pub factions: Vec<Faction>,
    pub artifacts: Vec<WtdArtifact>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Corpus {
    pub artifacts: u32,
    pub distinct_primitives: u32,
    pub observations: u32,
    pub skipped: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Consensus {
    pub universal: u32,
    pub majority: u32,
    pub minority: u32,
    pub unique: u32,
    pub core_size: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DriftStats {
    pub mean: f64,
    pub stddev: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Conflict {
    pub key: String,
    pub holders: u32,
    pub deviants: u32,
    pub values: Vec<ConflictValue>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ConflictValue {
    pub value: String,
    pub count: u32,
    pub artifacts: Vec<usize>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Faction {
    pub size: u32,
    pub members: Vec<usize>,
    #[serde(default)]
    pub member_paths: Vec<String>,
    pub cohesion: f64,
    #[serde(default)]
    pub signature: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WtdArtifact {
    pub path: String,
    pub kind: String,
    pub drift: f64,
    pub outlier: bool,
    pub primitives: u32,
    pub in_core: u32,
    pub unique: u32,
}

/// Run `wtd` over one fleet. Executed with `cwd` at the repository root
/// and repo-relative member paths, so the reported paths come back in
/// the same namespace as the tq1 dictionary and join without rewriting.
pub fn run_wtd(repo: &str, fleet: &Fleet) -> Result<WtdReport, String> {
    let out = Command::new("wtd")
        .args(&fleet.members)
        .arg("--json")
        .current_dir(repo)
        .output()
        .map_err(|e| format!("wtd not runnable: {e}"))?;
    // Exit 3 is the --fail-on gate, which is not set here; anything
    // nonzero other than that is a real failure.
    if !out.status.success() && out.status.code() != Some(3) {
        return Err(format!(
            "wtd exited {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("wtd JSON unreadable: {e}"))
}

/// Per-path join record. This is what the terrain overlays onto cells.
#[derive(Clone, Debug, Serialize)]
pub struct PathDivergence {
    pub fleet: String,
    pub fleet_label: String,
    pub drift: f64,
    pub outlier: bool,
    /// Conflicting keys where this file holds the minority value — the
    /// actionable subset, not every key the fleet disagrees on.
    pub deviant_keys: Vec<String>,
    /// Faction this file belongs to, if it drifted together with others.
    pub faction_size: Option<u32>,
}

#[derive(Serialize)]
pub struct FleetAnalysis {
    pub fleets: Vec<FleetResult>,
    pub by_path: BTreeMap<String, PathDivergence>,
    /// Candidate sets that turned out not to be peers. Reported rather
    /// than dropped, so the absence of a fleet is explainable.
    pub rejected: Vec<RejectedFleet>,
    /// Fleets that failed to run, with the reason. Never silent.
    pub errors: Vec<FleetError>,
}

#[derive(Serialize)]
pub struct RejectedFleet {
    pub id: String,
    pub label: String,
    pub members: usize,
    pub reason: String,
}

/// Smallest share of holders a value must have to count as the fleet's
/// agreed position. Below this there is no consensus to deviate *from*.
///
/// Set just under a third so an even three-way split still registers —
/// nine services running three different log levels is a real finding,
/// even though no single value holds a majority. Identifier fields sit
/// far below this (a 57-member fleet's best id value scores 0.035).
const MIN_MAJORITY_SHARE: f64 = 0.30;

/// Is this key a genuine disagreement, or just an identifier?
///
/// `wtd` reports any scalar key whose value varies. In a fleet of 57
/// payer records, `clearinghousePayerId` varies 55 ways — that is the
/// field doing its job, not a misconfiguration. A key is only actionable
/// when the fleet has an actual majority position that some members
/// depart from.
fn is_actionable_conflict(c: &Conflict) -> bool {
    let Some(majority) = c.values.iter().max_by_key(|v| v.count) else {
        return false;
    };
    if majority.count < 2 || c.holders == 0 {
        return false;
    }
    f64::from(majority.count) / f64::from(c.holders) >= MIN_MAJORITY_SHARE
}

/// A candidate set is only a fleet if its members actually share
/// structure. Grouping by extension can put `package.json` next to
/// `tsconfig.json`: both are JSON, neither is a variant of the other.
/// With no universal primitives every member scores ~1.0 drift and the
/// "divergence" is meaningless.
fn consensus_verdict(r: &WtdReport) -> Option<String> {
    if r.consensus.core_size == 0 {
        return Some(format!(
            "these {} files have nothing in common, so there is no agreement to measure against",
            r.corpus.artifacts
        ));
    }
    if r.drift.mean >= 0.95 {
        return Some(format!(
            "these files are almost entirely different from one another ({:.0}% unshared) — they \
             are not versions of the same thing",
            r.drift.mean * 100.0
        ));
    }
    None
}

#[derive(Serialize)]
pub struct FleetResult {
    #[serde(flatten)]
    pub fleet: Fleet,
    pub report: WtdReport,
    /// Keys `wtd` flagged that were identifier-like rather than
    /// configuration disagreements. Counted so the filtering is visible.
    pub identifier_keys_suppressed: usize,
}

#[derive(Serialize)]
pub struct FleetError {
    pub fleet: String,
    pub message: String,
}

/// Detect fleets among `paths` and run `wtd` over each.
pub fn analyze(repo: &str, paths: &[String]) -> FleetAnalysis {
    let mut fleets = Vec::new();
    let mut by_path: BTreeMap<String, PathDivergence> = BTreeMap::new();
    let mut rejected = Vec::new();
    let mut errors = Vec::new();

    for fleet in detect(paths) {
        let report = match run_wtd(repo, &fleet) {
            Ok(r) => r,
            Err(message) => {
                errors.push(FleetError {
                    fleet: fleet.id.clone(),
                    message,
                });
                continue;
            }
        };

        if let Some(reason) = consensus_verdict(&report) {
            rejected.push(RejectedFleet {
                id: fleet.id.clone(),
                label: fleet.label.clone(),
                members: fleet.members.len(),
                reason,
            });
            continue;
        }

        // Which artifact indices hold a minority value, per key.
        // Identifier-like keys are dropped first — see
        // `is_actionable_conflict`.
        let mut report = report;
        let before = report.conflicts.len();
        report.conflicts.retain(is_actionable_conflict);
        let suppressed = before - report.conflicts.len();

        // A set, not a list: an array-valued key such as
        // `portals[].defaultBotId` can put one artifact in several
        // minority buckets at once, and reporting the same key five
        // times would inflate every count downstream.
        let mut deviant: BTreeMap<usize, std::collections::BTreeSet<String>> = BTreeMap::new();
        for c in &report.conflicts {
            let Some(majority) = c.values.iter().max_by_key(|v| v.count) else {
                continue;
            };
            for v in &c.values {
                if std::ptr::eq(v, majority) {
                    continue;
                }
                for &idx in &v.artifacts {
                    deviant.entry(idx).or_default().insert(c.key.clone());
                }
            }
        }

        let mut faction_of: BTreeMap<usize, u32> = BTreeMap::new();
        for f in &report.factions {
            for &idx in &f.members {
                faction_of.insert(idx, f.size);
            }
        }

        for (i, a) in report.artifacts.iter().enumerate() {
            // A file can only belong to one fleet in this index; keep the
            // one where it diverges most, since that is the finding.
            let entry = PathDivergence {
                fleet: fleet.id.clone(),
                fleet_label: fleet.label.clone(),
                drift: a.drift,
                outlier: a.outlier,
                deviant_keys: deviant
                    .get(&i)
                    .map(|s| s.iter().cloned().collect())
                    .unwrap_or_default(),
                faction_size: faction_of.get(&i).copied(),
            };
            by_path
                .entry(a.path.clone())
                .and_modify(|prev| {
                    if entry.deviant_keys.len() > prev.deviant_keys.len()
                        || (entry.deviant_keys.len() == prev.deviant_keys.len()
                            && entry.drift > prev.drift)
                    {
                        *prev = entry.clone();
                    }
                })
                .or_insert(entry);
        }

        fleets.push(FleetResult {
            fleet,
            report,
            identifier_keys_suppressed: suppressed,
        });
    }

    // Loudest first: a fleet disagreeing on eight keys is the finding;
    // one with clean consensus is background.
    fleets.sort_by(|a, b| {
        b.report
            .conflicts
            .len()
            .cmp(&a.report.conflicts.len())
            .then(b.report.factions.len().cmp(&a.report.factions.len()))
            .then(a.fleet.id.cmp(&b.fleet.id))
    });

    FleetAnalysis {
        fleets,
        by_path,
        rejected,
        errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn same_basename_across_directories_is_a_fleet() {
        let f = detect(&paths(&[
            "packages/a/package.json",
            "packages/b/package.json",
            "packages/c/package.json",
        ]));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].rule, "basename");
        assert_eq!(f[0].members.len(), 3);
    }

    #[test]
    fn source_files_sharing_a_name_are_not_a_fleet() {
        // Nine lib.rs files share a name by language convention. Comparing
        // them yields drift near 1.0 for every member — pure noise.
        let f = detect(&paths(&[
            "crates/a/src/lib.rs",
            "crates/b/src/lib.rs",
            "crates/c/src/lib.rs",
        ]));
        assert!(f.is_empty(), "got {f:?}");
    }

    #[test]
    fn config_extension_in_one_directory_is_a_fleet() {
        let f = detect(&paths(&[
            ".github/workflows/ci.yml",
            ".github/workflows/release.yml",
            ".github/workflows/nightly.yml",
        ]));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].rule, "extension");
    }

    #[test]
    fn two_files_do_not_make_an_extension_fleet() {
        let f = detect(&paths(&[
            ".github/workflows/ci.yml",
            ".github/workflows/release.yml",
        ]));
        assert!(f.is_empty());
    }

    #[test]
    fn extensionless_named_files_are_fleetable() {
        let f = detect(&paths(&["svc/a/Dockerfile", "svc/b/Dockerfile"]));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].rule, "basename");
    }

    #[test]
    fn oversized_fleets_report_their_truncation() {
        let many: Vec<String> = (0..80).map(|i| format!("pkg/{i}/package.json")).collect();
        let f = detect(&many);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].members.len(), MAX_MEMBERS);
        assert_eq!(f[0].truncated, 80 - MAX_MEMBERS);
        assert!(
            f[0].label.contains("80"),
            "label should state the true size"
        );
    }

    #[test]
    fn markdown_and_prose_are_not_fleets() {
        let f = detect(&paths(&["docs/a/README.md", "docs/b/README.md"]));
        assert!(f.is_empty());
    }
}

#[cfg(test)]
mod conflict_tests {
    use super::*;

    fn conflict(holders: u32, counts: &[u32]) -> Conflict {
        Conflict {
            key: "k".into(),
            holders,
            deviants: holders - counts.iter().max().copied().unwrap_or(0),
            values: counts
                .iter()
                .enumerate()
                .map(|(i, &count)| ConflictValue {
                    value: format!("v{i}"),
                    count,
                    artifacts: vec![i],
                })
                .collect(),
        }
    }

    #[test]
    fn a_real_majority_with_deviants_is_actionable() {
        // log_level: info x3, debug x2 across 5 files.
        assert!(is_actionable_conflict(&conflict(5, &[3, 2])));
    }

    #[test]
    fn identifier_fields_are_not_conflicts() {
        // 57 payer records, 55 distinct ids, best value held twice.
        let mut counts = vec![2, 2];
        counts.extend(std::iter::repeat_n(1, 53));
        assert!(!is_actionable_conflict(&conflict(57, &counts)));
    }

    #[test]
    fn a_lone_deviant_against_a_clear_majority_is_actionable() {
        assert!(is_actionable_conflict(&conflict(12, &[11, 1])));
    }

    #[test]
    fn an_even_three_way_split_still_counts() {
        assert!(is_actionable_conflict(&conflict(9, &[3, 3, 3])));
    }

    #[test]
    fn a_singleton_majority_is_not_consensus() {
        assert!(!is_actionable_conflict(&conflict(2, &[1, 1])));
    }
}
