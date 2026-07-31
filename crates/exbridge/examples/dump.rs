//! Print the re-hosted pipeline's tq1 summary to stdout, in the same
//! shape `entropyx scan` emits. Exists so the two can be diffed directly:
//!
//!   entropyx scan REPO --no-cache > a.json
//!   cargo run --release --example dump -- REPO > b.json
//!   diff a.json b.json

fn main() {
    let Some(repo) = std::env::args().nth(1) else {
        eprintln!("usage: dump <repo-path>");
        std::process::exit(2);
    };
    let opts = exbridge::pipeline::ScanOptions {
        no_cache: true,
        ..Default::default()
    };
    match exbridge::pipeline::scan(&repo, &opts, &exbridge::pipeline::Silent) {
        Ok((s, _evidence)) => {
            let mut out = std::io::stdout().lock();
            serde_json::to_writer_pretty(&mut out, &s).expect("write");
            use std::io::Write;
            let _ = out.write_all(b"\n");
        }
        Err(e) => {
            eprintln!("dump: {e}");
            std::process::exit(1);
        }
    }
}
