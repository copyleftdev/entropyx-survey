//! Parallel betweenness centrality, bit-for-bit identical to
//! `entropyx-graph`'s serial implementation.
//!
//! Betweenness is the single largest cost in a scan on graph-dense
//! repositories — 17.8s of api-core's 32s, and effectively all of
//! TrendRadar's 755s. Brandes' algorithm is embarrassingly parallel over
//! source vertices, and nothing about it needs to be serial.
//!
//! The catch is floating point. The serial version accumulates
//! `cb[w] += delta_s[w]` for `s` in `0..n`, and f64 addition is not
//! associative: reducing those contributions in any other order can shift
//! the last bits, which would break the byte-for-byte parity this bridge
//! guarantees against `entropyx scan`.
//!
//! So sources are processed in fixed-size batches: a batch is computed in
//! parallel, then its per-source contributions are folded into `cb` **in
//! ascending source order** before the next batch starts. Every `+=` lands
//! in exactly the order the serial loop would have used, so the result is
//! identical to the last bit, not merely close. Peak extra memory is
//! `batch × nodes` f64 — a few megabytes at realistic sizes.
//!
//! `entropyx-graph` keeps its adjacency private, so the graph is rebuilt
//! here from the same `per_commit_paths` the upstream constructor
//! consumes. Node numbering must match: first-seen order across commits,
//! neighbours ascending (upstream stores them in a `BTreeMap`, so its BFS
//! visits them in ascending index order and the predecessor lists — which
//! drive the accumulation order — depend on it).

use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap, VecDeque};

/// Sources per parallel batch. Large enough to keep every core busy,
/// small enough that the transient `batch × nodes` buffer stays small.
const BATCH: usize = 256;

/// Adjacency in the same shape and order `entropyx-graph` builds.
pub struct Graph {
    /// Node index → path, in first-seen order.
    pub nodes: Vec<String>,
    /// Node index → neighbour indices, ascending.
    adj: Vec<Vec<usize>>,
}

impl Graph {
    /// Rebuild from per-commit path lists. Mirrors
    /// `CoChangeGraph::from_commit_paths`: each commit is a clique over
    /// its paths, self-edges are skipped, duplicates within a commit are
    /// the caller's problem (the pipeline dedups before this point).
    pub fn from_commit_paths<S: AsRef<str>>(per_commit: &[Vec<S>]) -> Self {
        let mut nodes: Vec<String> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        let mut commit_idxs: Vec<Vec<usize>> = Vec::with_capacity(per_commit.len());
        for commit in per_commit {
            let mut idxs = Vec::with_capacity(commit.len());
            for p in commit {
                let s = p.as_ref();
                let i = match index.get(s) {
                    Some(&i) => i,
                    None => {
                        let i = nodes.len();
                        nodes.push(s.to_string());
                        index.insert(s.to_string(), i);
                        i
                    }
                };
                idxs.push(i);
            }
            commit_idxs.push(idxs);
        }

        // Neighbour sets, then sorted lists. Upstream stores a
        // `BTreeMap<usize, u64>` and iterates its keys, which is ascending
        // order; the traversal must see the same order.
        let mut sets: Vec<std::collections::BTreeSet<usize>> =
            vec![Default::default(); nodes.len()];
        for idxs in &commit_idxs {
            for i in 0..idxs.len() {
                for j in (i + 1)..idxs.len() {
                    let (a, b) = (idxs[i], idxs[j]);
                    if a == b {
                        continue;
                    }
                    sets[a].insert(b);
                    sets[b].insert(a);
                }
            }
        }
        let adj = sets.into_iter().map(|s| s.into_iter().collect()).collect();
        Graph { nodes, adj }
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Directed adjacency entries — twice the undirected edge count.
    pub fn degree_entries(&self) -> usize {
        self.adj.iter().map(Vec::len).sum()
    }

    /// Mean neighbours per node. This decides which accumulation strategy
    /// `betweenness_centrality` uses; see `Strategy`.
    pub fn mean_degree(&self) -> f64 {
        if self.nodes.is_empty() {
            0.0
        } else {
            self.degree_entries() as f64 / self.nodes.len() as f64
        }
    }

    /// Betweenness for every node, normalized exactly as upstream does.
    pub fn betweenness_centrality(&self) -> BTreeMap<String, f64> {
        let n = self.nodes.len();
        let mut cb = vec![0.0_f64; n];

        let mut start = 0;
        while start < n {
            let end = (start + BATCH).min(n);
            // Parallel over sources; each returns its own delta vector.
            let deltas: Vec<Vec<f64>> = (start..end)
                .into_par_iter()
                .map_init(
                    || Scratch::new(n),
                    |scratch, s| self.single_source_delta(s, scratch),
                )
                .collect();
            // Fold in ascending source order — the serial accumulation
            // order, and therefore the serial floating-point result.
            for (offset, delta) in deltas.iter().enumerate() {
                let s = start + offset;
                for (w, d) in delta.iter().enumerate() {
                    if w != s {
                        cb[w] += d;
                    }
                }
            }
            start = end;
        }

        // Brandes double-counts on undirected graphs; upstream normalizes
        // by (n-1)(n-2) and yields zeros below three nodes.
        let denom = if n > 2 {
            ((n - 1) * (n - 2)) as f64
        } else {
            0.0
        };
        let mut out = BTreeMap::new();
        for (i, path) in self.nodes.iter().enumerate() {
            out.insert(path.clone(), if denom > 0.0 { cb[i] / denom } else { 0.0 });
        }
        out
    }

    /// One Brandes source pass. Returns this source's dependency vector,
    /// left un-accumulated so the caller controls the summation order.
    ///
    /// Every buffer comes from reusable per-worker scratch. Upstream
    /// allocates `n` fresh predecessor `Vec`s per source, which is 18
    /// million allocations on TrendRadar with 64 workers contending for
    /// them; clearing retained ones instead costs nothing.
    ///
    /// Recomputing predecessors on the fly (scanning `adj[w]` for
    /// vertices one level closer) was tried and is a trap: it is faster
    /// on sparse graphs but doubled TrendRadar, where mean degree is
    /// 3,531 and the BFS is so shallow that a vertex has ~3,500
    /// neighbours and one predecessor.
    fn single_source_delta(&self, s: usize, scratch: &mut Scratch) -> Vec<f64> {
        let Scratch {
            stack,
            sigma,
            dist,
            delta,
            queue,
            preds,
        } = scratch;
        stack.clear();
        queue.clear();
        sigma.fill(0);
        dist.fill(-1);
        delta.fill(0.0);

        for p in preds.iter_mut() {
            p.clear();
        }

        sigma[s] = 1;
        dist[s] = 0;
        queue.push_back(s);
        while let Some(v) = queue.pop_front() {
            stack.push(v);
            for &w in &self.adj[v] {
                if dist[w] < 0 {
                    dist[w] = dist[v] + 1;
                    queue.push_back(w);
                }
                if dist[w] == dist[v] + 1 {
                    sigma[w] += sigma[v];
                    preds[w].push(v as u32);
                }
            }
        }
        while let Some(w) = stack.pop() {
            for &v in &preds[w] {
                let v = v as usize;
                // Expression kept exactly as upstream writes it. Hoisting
                // the division out of the loop would compute
                // sigma[v] * ((1+delta[w]) / sigma[w]) instead of
                // (sigma[v] / sigma[w]) * (1+delta[w]) — algebraically
                // equal, not bit-equal.
                delta[v] += (sigma[v] as f64 / sigma[w] as f64) * (1.0 + delta[w]);
            }
        }
        delta.clone()
    }
}

/// Per-worker scratch, reused across sources so a batch costs one
/// allocation set per thread rather than one per source.
struct Scratch {
    stack: Vec<usize>,
    sigma: Vec<u64>,
    dist: Vec<i64>,
    delta: Vec<f64>,
    queue: VecDeque<usize>,
    /// Predecessor lists, cleared rather than reallocated between
    /// sources. Upstream allocates `n` fresh `Vec`s per source — 18
    /// million allocations on TrendRadar, contended across every worker.
    /// Retained capacity is modest: a dense graph has a shallow BFS, so
    /// most vertices have exactly one predecessor.
    preds: Vec<Vec<u32>>,
}

impl Scratch {
    fn new(n: usize) -> Self {
        Scratch {
            stack: Vec::with_capacity(n),
            sigma: vec![0; n],
            dist: vec![-1; n],
            delta: vec![0.0; n],
            queue: VecDeque::with_capacity(n),
            preds: vec![Vec::new(); n],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entropyx_graph::CoChangeGraph;

    fn commits(spec: &[&[&str]]) -> Vec<Vec<String>> {
        spec.iter()
            .map(|c| c.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    /// The whole point: identical to the last bit, on graphs shaped like
    /// the ones real repositories produce.
    fn assert_matches_upstream(spec: &[&[&str]]) {
        let per_commit = commits(spec);
        let ours = Graph::from_commit_paths(&per_commit).betweenness_centrality();
        let theirs = CoChangeGraph::from_commit_paths(&per_commit).betweenness_centrality();
        assert_eq!(ours.len(), theirs.len(), "node count");
        for (path, v) in &theirs {
            let got = ours.get(path).unwrap_or_else(|| panic!("missing {path}"));
            assert_eq!(
                got.to_bits(),
                v.to_bits(),
                "{path}: {got} vs {v} (must be bit-identical, not close)"
            );
        }
    }

    #[test]
    fn matches_upstream_on_a_bridge_graph() {
        // Two cliques joined through one file — the shape betweenness is
        // meant to surface.
        assert_matches_upstream(&[
            &["a.rs", "b.rs", "bridge.rs"],
            &["c.rs", "d.rs", "bridge.rs"],
            &["a.rs", "b.rs"],
            &["c.rs", "d.rs"],
        ]);
    }

    #[test]
    fn matches_upstream_on_a_chain() {
        assert_matches_upstream(&[&["a", "b"], &["b", "c"], &["c", "d"], &["d", "e"]]);
    }

    #[test]
    fn matches_upstream_on_a_wide_commit() {
        let wide: Vec<String> = (0..40).map(|i| format!("f{i}.ts")).collect();
        let refs: Vec<&str> = wide.iter().map(String::as_str).collect();
        assert_matches_upstream(&[&refs, &refs[..10], &refs[5..20]]);
    }

    #[test]
    fn matches_upstream_across_batch_boundaries() {
        // More sources than one batch, so the fold-in-order path is
        // exercised rather than a single in-batch pass.
        let n = BATCH + 37;
        let names: Vec<String> = (0..n).map(|i| format!("n{i}")).collect();
        let mut spec: Vec<Vec<&str>> = Vec::new();
        for i in 0..n - 1 {
            spec.push(vec![names[i].as_str(), names[i + 1].as_str()]);
        }
        // A few hubs so the graph is not a bare path.
        spec.push(vec![
            names[0].as_str(),
            names[n / 2].as_str(),
            names[n - 1].as_str(),
        ]);
        let refs: Vec<&[&str]> = spec.iter().map(Vec::as_slice).collect();
        assert_matches_upstream(&refs);
    }

    #[test]
    fn matches_upstream_on_degenerate_graphs() {
        assert_matches_upstream(&[&["only.rs"]]);
        assert_matches_upstream(&[&["a", "b"]]);
        assert_matches_upstream(&[]);
    }

    #[test]
    fn node_order_matches_upstream() {
        let per_commit = commits(&[&["z.rs", "a.rs"], &["m.rs", "z.rs"]]);
        let ours = Graph::from_commit_paths(&per_commit);
        let theirs = CoChangeGraph::from_commit_paths(&per_commit);
        assert_eq!(ours.nodes, theirs.nodes(), "first-seen interning order");
    }
}
