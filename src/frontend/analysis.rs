//! Pure analysis helpers used while lowering: bit sets for "is this bit driven
//! on every path" and a dependency graph for combinational-loop detection.
//!
//! Both work on plain data so they can be tested without a design.

use std::collections::{HashMap, HashSet};

/// A fixed-width set of bits, used to track which bits of a signal were driven.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Bits {
    width: u32,
    words: Vec<u64>,
}

impl Bits {
    /// An empty set of `width` bits.
    pub(crate) fn new(width: u32) -> Self {
        let words = width.div_ceil(64) as usize;
        Self {
            width,
            words: vec![0; words],
        }
    }

    /// Add `[lo, hi)` to the set. Bits outside the width are ignored.
    pub(crate) fn set_range(&mut self, lo: u32, hi: u32) {
        let hi = hi.min(self.width);
        let mut bit = lo.min(hi);
        while bit < hi {
            let word = (bit / 64) as usize;
            let offset = bit % 64;
            let take = (hi - bit).min(64 - offset);
            let mask = if take == 64 {
                u64::MAX
            } else {
                ((1u64 << take) - 1) << offset
            };
            self.words[word] |= mask;
            bit += take;
        }
    }

    /// True when every bit of `other` is set in `self`.
    pub(crate) fn contains_all(&self, other: &Bits) -> bool {
        (0..self.words.len()).all(|index| {
            let other_word = other.words.get(index).copied().unwrap_or(0);
            self.words[index] & other_word == other_word
        })
    }

    /// Keep only bits that are also in `other`.
    pub(crate) fn intersect_with(&mut self, other: &Bits) {
        for (index, word) in self.words.iter_mut().enumerate() {
            *word &= other.words.get(index).copied().unwrap_or(0);
        }
    }

    /// The contiguous runs of set bits, as half-open `[lo, hi)` ranges.
    pub(crate) fn runs(&self) -> Vec<(u32, u32)> {
        let mut runs = Vec::new();
        let mut bit = 0;
        while bit < self.width {
            if !self.get(bit) {
                bit += 1;
                continue;
            }
            let start = bit;
            while bit < self.width && self.get(bit) {
                bit += 1;
            }
            runs.push((start, bit));
        }
        runs
    }

    /// True when the bit is set.
    pub(crate) fn get(&self, bit: u32) -> bool {
        match self.words.get((bit / 64) as usize) {
            Some(word) => word & (1u64 << (bit % 64)) != 0,
            None => false,
        }
    }

    /// The lowest bit that is set in `other` but not in `self`.
    pub(crate) fn first_missing(&self, other: &Bits) -> Option<u32> {
        for (index, other_word) in other.words.iter().enumerate() {
            let own = self.words.get(index).copied().unwrap_or(0);
            let missing = other_word & !own;
            if missing != 0 {
                return Some(index as u32 * 64 + missing.trailing_zeros());
            }
        }
        None
    }
}

/// Directed dependency graph over signals: an edge `a -> b` means "b's value is
/// computed from a's value", so a cycle is a combinational loop.
#[derive(Clone, Debug, Default)]
pub(crate) struct DependencyGraph {
    edges: HashMap<usize, Vec<usize>>,
}

impl DependencyGraph {
    /// Record that `to` depends on `from`.
    pub(crate) fn add_edge(&mut self, from: usize, to: usize) {
        let entry = self.edges.entry(from).or_default();
        if !entry.contains(&to) {
            entry.push(to);
        }
    }

    /// All nodes mentioned by any edge.
    pub(crate) fn nodes(&self) -> HashSet<usize> {
        let mut nodes = HashSet::new();
        for (from, targets) in &self.edges {
            nodes.insert(*from);
            nodes.extend(targets.iter().copied());
        }
        nodes
    }

    /// A cycle if the graph has one, as the signal path that closes it.
    pub(crate) fn find_cycle(&self) -> Option<Vec<usize>> {
        #[derive(Clone, Copy, PartialEq)]
        enum Color {
            Grey,
            Black,
        }

        let nodes = self.nodes();
        let mut colors: HashMap<usize, Color> = HashMap::new();
        let mut order: Vec<usize> = nodes.iter().copied().collect();
        order.sort_unstable();

        for root in order {
            if colors.contains_key(&root) {
                continue;
            }
            // Iterative depth-first search so a deep design cannot overflow the
            // stack.
            let mut path: Vec<usize> = vec![root];
            let mut positions: HashMap<usize, usize> = HashMap::new();
            positions.insert(root, 0);
            colors.insert(root, Color::Grey);

            while let Some(&node) = path.last() {
                let mut descended = false;
                if let Some(targets) = self.edges.get(&node) {
                    for target in targets {
                        match colors.get(target) {
                            Some(Color::Grey) => {
                                let start = positions.get(target).copied().unwrap_or(0);
                                let mut cycle = path[start..].to_vec();
                                cycle.push(*target);
                                return Some(cycle);
                            }
                            Some(Color::Black) => {}
                            None => {
                                colors.insert(*target, Color::Grey);
                                positions.insert(*target, path.len());
                                path.push(*target);
                                descended = true;
                                break;
                            }
                        }
                    }
                }
                if !descended {
                    colors.insert(node, Color::Black);
                    positions.remove(&node);
                    path.pop();
                }
            }
        }
        None
    }

    /// Every node reachable from `start`, `start` excluded.
    pub(crate) fn reachable_from(&self, start: usize) -> HashSet<usize> {
        let mut seen = HashSet::new();
        let mut stack = vec![start];
        while let Some(node) = stack.pop() {
            if let Some(targets) = self.edges.get(&node) {
                for target in targets {
                    if seen.insert(*target) {
                        stack.push(*target);
                    }
                }
            }
        }
        seen.remove(&start);
        seen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_track_ranges_across_word_boundaries() {
        let mut bits = Bits::new(100);
        bits.set_range(60, 70);
        let mut expected = Bits::new(100);
        expected.set_range(60, 70);
        assert!(bits.contains_all(&expected));
        assert_eq!(bits.first_missing(&expected), None);
        let mut wider = Bits::new(100);
        wider.set_range(0, 100);
        assert_eq!(bits.first_missing(&wider), Some(0));
        assert_eq!(Bits::new(100).first_missing(&wider), Some(0));
    }

    #[test]
    fn bits_clamp_out_of_range_requests() {
        let mut bits = Bits::new(8);
        bits.set_range(4, 100);
        let mut expected = Bits::new(8);
        expected.set_range(4, 8);
        assert_eq!(bits, expected);
    }

    #[test]
    fn cycle_is_found_and_reported_as_a_path() {
        let mut graph = DependencyGraph::default();
        graph.add_edge(0, 1);
        graph.add_edge(1, 2);
        graph.add_edge(2, 0);
        let cycle = graph.find_cycle().expect("cycle");
        assert_eq!(
            cycle.first(),
            cycle.last(),
            "cycle path closes on itself: {cycle:?}"
        );
        assert_eq!(cycle.len(), 4);
    }

    #[test]
    fn acyclic_graph_has_no_cycle() {
        let mut graph = DependencyGraph::default();
        graph.add_edge(0, 1);
        graph.add_edge(1, 2);
        graph.add_edge(0, 2);
        assert!(graph.find_cycle().is_none());
        assert_eq!(graph.reachable_from(0), HashSet::from([1, 2]));
    }

    #[test]
    fn self_loop_is_a_cycle() {
        let mut graph = DependencyGraph::default();
        graph.add_edge(3, 3);
        assert_eq!(graph.find_cycle(), Some(vec![3, 3]));
    }

    #[test]
    fn merge_intersects_definite_bits() {
        let mut then_bits = Bits::new(8);
        then_bits.set_range(0, 8);
        let mut else_bits = Bits::new(8);
        else_bits.set_range(0, 4);
        then_bits.intersect_with(&else_bits);
        let mut expected = Bits::new(8);
        expected.set_range(0, 4);
        assert_eq!(then_bits, expected);
    }
}
