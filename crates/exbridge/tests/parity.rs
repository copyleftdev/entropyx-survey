//! Parity guard.
//!
//! `exbridge` re-hosts the scan orchestration that lives in
//! `entropyx-cli`'s `main.rs`, because that pipeline has no library entry
//! point to call. Re-hosting means it can drift. This test scans a real
//! repository both ways and asserts the tq1 summaries are identical.
//!
//! It shells out to the installed `entropyx` binary. If that binary is
//! absent the test skips rather than fails — it is a drift detector, not
//! a build dependency.

use std::process::Command;

/// Compare against a repo that actually exercises the interesting paths:
/// renames, multiple authors, incident-tagged commits. Overridable so CI
/// can point at a fixture.
fn target_repo() -> Option<String> {
    if let Ok(p) = std::env::var("EXBRIDGE_PARITY_REPO") {
        return Some(p);
    }
    // Fall back to entropyx's own repository, next to this workspace.
    let guess = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../entropyx");
    std::path::Path::new(guess)
        .join(".git")
        .exists()
        .then(|| guess.to_string())
}

fn cli_scan(repo: &str) -> Option<serde_json::Value> {
    let out = Command::new("entropyx")
        .args(["scan", repo, "--no-cache"])
        .output()
        .ok()?;
    if !out.status.success() {
        eprintln!(
            "entropyx scan failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

#[test]
fn rehosted_pipeline_matches_entropyx_cli() {
    let Some(repo) = target_repo() else {
        eprintln!("skip: no target repo (set EXBRIDGE_PARITY_REPO)");
        return;
    };
    let Some(expected) = cli_scan(&repo) else {
        eprintln!("skip: `entropyx` binary unavailable or failed");
        return;
    };

    let opts = exbridge::pipeline::ScanOptions {
        no_cache: true,
        ..Default::default()
    };
    let (got, _evidence) = exbridge::pipeline::scan(&repo, &opts, &exbridge::pipeline::Silent)
        .expect("re-hosted scan must succeed");
    // Round-trip through text, exactly as the CLI's output was. Comparing
    // a freshly-built `Value` against a parsed one would compare float
    // *representations* rather than the serialized numbers both consumers
    // actually see.
    let got: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&got).expect("serialize")).expect("reparse");

    assert_eq!(expected["schema"], got["schema"], "schema envelope drifted");
    assert_eq!(expected["dict"], got["dict"], "dictionary drifted");
    assert_eq!(expected["events"], got["events"], "events drifted");
    assert_eq!(expected["handles"], got["handles"], "handles drifted");

    // Rows are compared one at a time so a failure names the file and
    // column that drifted instead of dumping the whole array.
    let ex_rows = expected["files"].as_array().expect("files array");
    let got_rows = got["files"].as_array().expect("files array");
    assert_eq!(ex_rows.len(), got_rows.len(), "row count drifted");
    let names = expected["dict"]["files"].as_array().expect("dict.files");
    let cols = expected["dict"]["metrics"]
        .as_array()
        .expect("dict.metrics");
    for (i, (a, b)) in ex_rows.iter().zip(got_rows.iter()).enumerate() {
        if a == b {
            continue;
        }
        let path = a["file"]
            .as_u64()
            .and_then(|f| names.get(f as usize))
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");
        for (j, (p, q)) in a["values"]
            .as_array()
            .into_iter()
            .flatten()
            .zip(b["values"].as_array().into_iter().flatten())
            .enumerate()
        {
            assert_eq!(
                p,
                q,
                "row {i} ({path}) column {j} ({}) drifted",
                cols.get(j).and_then(|v| v.as_str()).unwrap_or("?")
            );
        }
        assert_eq!(
            a["signal_class"], b["signal_class"],
            "row {i} ({path}) signal_class drifted"
        );
        assert_eq!(a, b, "row {i} ({path}) drifted outside values/signal_class");
    }

    assert_eq!(expected, got, "summaries differ outside the named sections");
}
