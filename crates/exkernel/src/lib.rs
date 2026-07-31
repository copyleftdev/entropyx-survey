//! entropyx scoring kernel, compiled to WebAssembly.
//!
//! The full `scan` pipeline cannot run in a browser: `entropyx-git`
//! depends on `gix` (needs a real filesystem) and shells out to
//! `git blame --line-porcelain`, and no subprocess exists in wasm32 or
//! WASI. What *is* portable is everything downstream of the git walk —
//! `entropyx-core`'s composite arithmetic and `classify` rules have no
//! I/O, no threads, and no clock.
//!
//! So the split is: the native bridge produces a tq1 `Summary` once, and
//! this kernel re-derives `composite` and `signal_class` from the seven
//! axes as fast as the user can drag a slider. Crucially it calls
//! `MetricComponents::composite` and `classify` directly — the browser
//! is running the same code that produced the original numbers, not a
//! JavaScript re-implementation that could silently drift from it.

use entropyx_core::MetricComponents;
use entropyx_core::metric::SignalClass;
use entropyx_core::metric::{ScoreWeights, classify};
use wasm_bindgen::prelude::*;

/// Column order of the flat `values` buffer, matching
/// `entropyx_tq::Dict::METRIC_COLUMNS`. Column 7 (composite) is written
/// by `rescore`, never read.
const STRIDE: usize = 8;

/// Discriminants for `signal_class`, returned as a `Uint8Array`. 0 means
/// "unclassified" so the common case needs no lookup.
const CLASS_NONE: u8 = 0;
const CLASS_REFACTOR_CONVERGENCE: u8 = 1;
const CLASS_API_DRIFT: u8 = 2;
const CLASS_OWNERSHIP_FRAGMENTATION: u8 = 3;
const CLASS_INCIDENT_AFTERSHOCK: u8 = 4;
const CLASS_COUPLED_AMPLIFIER: u8 = 5;
const CLASS_FROZEN_NEGLECT: u8 = 6;

fn class_code(c: Option<SignalClass>) -> u8 {
    match c {
        None => CLASS_NONE,
        Some(SignalClass::RefactorConvergence) => CLASS_REFACTOR_CONVERGENCE,
        Some(SignalClass::ApiDrift) => CLASS_API_DRIFT,
        Some(SignalClass::OwnershipFragmentation) => CLASS_OWNERSHIP_FRAGMENTATION,
        Some(SignalClass::IncidentAftershock) => CLASS_INCIDENT_AFTERSHOCK,
        Some(SignalClass::CoupledAmplifier) => CLASS_COUPLED_AMPLIFIER,
        Some(SignalClass::FrozenNeglect) => CLASS_FROZEN_NEGLECT,
    }
}

/// V_t threshold above which an incident-tagged file is treated as being
/// in active firefighting. Mirrors the constant in the scan pipeline —
/// if that changes upstream, this must change with it.
const AFTERSHOCK_VT_THRESHOLD: f64 = 0.3;

#[wasm_bindgen]
pub struct Kernel {
    /// Row-major `[n * STRIDE]`. Columns 0..=6 are the seven axes;
    /// column 7 is the composite this kernel writes.
    values: Vec<f64>,
    n: usize,
    /// Per-row flag: does this file have at least one incident-tagged
    /// commit? Not derivable from the axes alone — the caller extracts
    /// it from the summary's `incident_aftershock` events.
    incident: Vec<u8>,
    composites: Vec<f64>,
    classes: Vec<u8>,
}

#[wasm_bindgen]
impl Kernel {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Kernel {
        Kernel {
            values: Vec::new(),
            n: 0,
            incident: Vec::new(),
            composites: Vec::new(),
            classes: Vec::new(),
        }
    }

    /// Load `n` rows. `values.len()` must be `n * 8`; `incident.len()`
    /// must be `n`. Returns an error rather than panicking, so a
    /// malformed load surfaces in JS as a thrown exception instead of a
    /// corrupted wasm instance.
    pub fn load(&mut self, values: &[f64], incident: &[u8]) -> Result<usize, JsError> {
        self.try_load(values, incident)
            .map_err(|e| JsError::new(&e))
    }

    #[wasm_bindgen(getter)]
    pub fn len(&self) -> usize {
        self.n
    }

    #[wasm_bindgen(getter)]
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Recompute every row's composite and signal class under `weights`
    /// (7 values, in `theta_d, theta_h, theta_v, theta_c, theta_b,
    /// theta_s, theta_t` order). Pure and allocation-free past the
    /// buffers allocated in `load`.
    pub fn rescore(&mut self, weights: &[f64]) -> Result<(), JsError> {
        self.try_rescore(weights).map_err(|e| JsError::new(&e))
    }

    /// Composites in load order.
    pub fn composites(&self) -> Vec<f64> {
        self.composites.clone()
    }

    /// Class codes in load order. See `classNames()` for the mapping.
    pub fn classes(&self) -> Vec<u8> {
        self.classes.clone()
    }

    /// Row indices sorted by composite descending, ties broken by index
    /// so the ordering is stable across calls.
    pub fn rank(&self) -> Vec<u32> {
        let mut idx: Vec<u32> = (0..self.n as u32).collect();
        idx.sort_by(|&a, &b| {
            let ca = self.composites[a as usize];
            let cb = self.composites[b as usize];
            cb.partial_cmp(&ca)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.cmp(&b))
        });
        idx
    }

    /// Count of rows per class code, indexed by code. Cheap enough to
    /// call every frame for the class-distribution strip.
    pub fn class_histogram(&self) -> Vec<u32> {
        let mut h = vec![0u32; 7];
        for &c in &self.classes {
            h[c as usize] += 1;
        }
        h
    }
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}

/// Plain-Rust core of the exported methods. Kept outside the
/// `#[wasm_bindgen]` block because `JsError` cannot be constructed off a
/// wasm target — putting the real logic here keeps the validation and
/// scoring paths unit-testable on the host.
impl Kernel {
    pub fn try_load(&mut self, values: &[f64], incident: &[u8]) -> Result<usize, String> {
        if !values.len().is_multiple_of(STRIDE) {
            return Err(format!(
                "values length {} is not a multiple of {STRIDE}",
                values.len()
            ));
        }
        let n = values.len() / STRIDE;
        if incident.len() != n {
            return Err(format!(
                "incident length {} does not match row count {n}",
                incident.len()
            ));
        }
        self.values = values.to_vec();
        self.incident = incident.to_vec();
        self.n = n;
        self.composites = vec![0.0; n];
        self.classes = vec![CLASS_NONE; n];
        Ok(n)
    }

    pub fn try_rescore(&mut self, weights: &[f64]) -> Result<(), String> {
        if weights.len() != 7 {
            return Err(format!("expected 7 weights, got {}", weights.len()));
        }
        let w = ScoreWeights {
            theta_d: weights[0],
            theta_h: weights[1],
            theta_v: weights[2],
            theta_c: weights[3],
            theta_b: weights[4],
            theta_s: weights[5],
            theta_t: weights[6],
        };

        for i in 0..self.n {
            let base = i * STRIDE;
            let m = MetricComponents {
                change_density: self.values[base],
                author_entropy: self.values[base + 1],
                temporal_volatility: self.values[base + 2],
                coupling_stress: self.values[base + 3],
                blame_youth: self.values[base + 4],
                semantic_drift: self.values[base + 5],
                test_cooevolution: self.values[base + 6],
            };
            let composite = m.composite(w);
            self.values[base + 7] = composite;
            self.composites[i] = composite;

            // IncidentAftershock overrides the static rules, exactly as
            // the native pipeline does.
            let in_aftershock =
                m.temporal_volatility > AFTERSHOCK_VT_THRESHOLD && self.incident[i] != 0;
            self.classes[i] = if in_aftershock {
                CLASS_INCIDENT_AFTERSHOCK
            } else {
                class_code(classify(&m))
            };
        }
        Ok(())
    }
}

/// RFC-007 default weights, in `rescore` order. Sourced from
/// `entropyx-core` so the browser's "reset" lands on exactly the values
/// the native scan used.
#[wasm_bindgen(js_name = defaultWeights)]
pub fn default_weights() -> Vec<f64> {
    let w = MetricComponents::DEFAULT_WEIGHTS;
    vec![
        w.theta_d, w.theta_h, w.theta_v, w.theta_c, w.theta_b, w.theta_s, w.theta_t,
    ]
}

/// Sum of the six positive weights. The UI shows this next to the
/// sliders: entropyx's composite is a convex combination, so a set that
/// does not sum to 1.0 produces scores that are not comparable to a
/// stock scan.
#[wasm_bindgen(js_name = sumPositive)]
pub fn sum_positive(weights: &[f64]) -> f64 {
    if weights.len() != 7 {
        return f64::NAN;
    }
    ScoreWeights {
        theta_d: weights[0],
        theta_h: weights[1],
        theta_v: weights[2],
        theta_c: weights[3],
        theta_b: weights[4],
        theta_s: weights[5],
        theta_t: weights[6],
    }
    .sum_positive()
}

/// Class code → tq1 `signal_class` string, index-aligned to the codes.
#[wasm_bindgen(js_name = classNames)]
pub fn class_names() -> Vec<String> {
    [
        "",
        "refactor_convergence",
        "api_drift",
        "ownership_fragmentation",
        "incident_aftershock",
        "coupled_amplifier",
        "frozen_neglect",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Metric column order, straight from the tq1 dictionary contract.
#[wasm_bindgen(js_name = metricColumns)]
pub fn metric_columns() -> Vec<String> {
    entropyx_tq::Dict::METRIC_COLUMNS
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Contract version this kernel was built against. The UI compares it to
/// the `schema.version` on an incoming summary and warns on mismatch
/// rather than silently scoring against the wrong contract.
#[wasm_bindgen(js_name = contractVersion)]
pub fn contract_version() -> String {
    entropyx_core::CONTRACT_VERSION.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(vals: [f64; 7]) -> Vec<f64> {
        let mut v = vals.to_vec();
        v.push(0.0);
        v
    }

    #[test]
    fn rescore_matches_core_composite() {
        let axes = [0.4, 0.2, 0.5, 0.8, 0.1, 0.7, 0.3];
        let mut k = Kernel::new();
        k.try_load(&row(axes), &[0]).unwrap();
        k.try_rescore(&default_weights()).unwrap();

        let m = MetricComponents {
            change_density: axes[0],
            author_entropy: axes[1],
            temporal_volatility: axes[2],
            coupling_stress: axes[3],
            blame_youth: axes[4],
            semantic_drift: axes[5],
            test_cooevolution: axes[6],
        };
        let expected = m.composite(MetricComponents::DEFAULT_WEIGHTS);
        assert_eq!(
            k.composites()[0],
            expected,
            "kernel must not drift from core"
        );
    }

    #[test]
    fn incident_flag_overrides_static_class() {
        // Volatile file that would otherwise classify as ApiDrift.
        let axes = [0.5, 0.5, 0.9, 0.1, 0.1, 0.9, 0.1];
        let mut k = Kernel::new();
        k.try_load(&row(axes), &[0]).unwrap();
        k.try_rescore(&default_weights()).unwrap();
        assert_ne!(k.classes()[0], CLASS_INCIDENT_AFTERSHOCK);

        let mut k2 = Kernel::new();
        k2.try_load(&row(axes), &[1]).unwrap();
        k2.try_rescore(&default_weights()).unwrap();
        assert_eq!(k2.classes()[0], CLASS_INCIDENT_AFTERSHOCK);
    }

    #[test]
    fn low_volatility_incident_does_not_override() {
        // incident flag set, but V_t below threshold — static rules win.
        let axes = [0.5, 0.5, 0.1, 0.1, 0.1, 0.9, 0.1];
        let mut k = Kernel::new();
        k.try_load(&row(axes), &[1]).unwrap();
        k.try_rescore(&default_weights()).unwrap();
        assert_ne!(k.classes()[0], CLASS_INCIDENT_AFTERSHOCK);
    }

    #[test]
    fn rank_is_composite_descending_and_stable() {
        let mut vals = Vec::new();
        for s in [0.1, 0.9, 0.5] {
            vals.extend(row([0.0, 0.0, 0.0, 0.0, 0.0, s, 0.0]));
        }
        let mut k = Kernel::new();
        k.try_load(&vals, &[0, 0, 0]).unwrap();
        k.try_rescore(&default_weights()).unwrap();
        assert_eq!(k.rank(), vec![1, 2, 0]);
    }

    #[test]
    fn load_rejects_ragged_input() {
        let mut k = Kernel::new();
        assert!(k.try_load(&[0.0; 7], &[0]).is_err());
        assert!(k.try_load(&[0.0; 8], &[0, 0]).is_err());
    }

    #[test]
    fn default_weights_are_convex() {
        let s = sum_positive(&default_weights());
        assert!(
            (s - 1.0).abs() < 1e-12,
            "positive weights must sum to 1, got {s}"
        );
    }

    #[test]
    fn class_names_cover_every_code() {
        assert_eq!(class_names().len(), 7);
        assert_eq!(
            class_names()[CLASS_FROZEN_NEGLECT as usize],
            "frozen_neglect"
        );
    }

    #[test]
    fn metric_columns_match_tq_contract() {
        assert_eq!(metric_columns(), entropyx_tq::Dict::METRIC_COLUMNS.to_vec());
    }
}
