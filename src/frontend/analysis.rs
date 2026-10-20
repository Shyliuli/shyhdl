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
    /// The same dependencies per bit: `(signal, bit)` packed into one key. The
    /// signal-level graph answers "does this output depend on that input", while
    /// this one answers "does this *bit* depend on that bit", which is what
    /// separates `c[i]` from `c[i + 1]` in a carry chain.
    bit_edges: HashMap<usize, Vec<usize>>,
}

/// Bits per packed node. A signal wider than this cannot be represented, so the
/// shift is large enough for any width the language accepts (65536).
const BIT_INDEX: u32 = 16;

fn bit_node(signal: usize, bit: u32) -> usize {
    (signal << BIT_INDEX) | bit as usize
}

fn bit_node_parts(node: usize) -> (usize, u32) {
    (node >> BIT_INDEX, (node & ((1 << BIT_INDEX) - 1)) as u32)
}

impl DependencyGraph {
    /// Record that `to` depends on `from`.
    pub(crate) fn add_edge(&mut self, from: usize, to: usize) {
        let entry = self.edges.entry(from).or_default();
        if !entry.contains(&to) {
            entry.push(to);
        }
    }

    /// Record that every bit of `to` is computed from every bit of `from`.
    pub(crate) fn add_bit_edge(&mut self, from: (usize, u32, u32), to: (usize, u32, u32)) {
        for from_bit in from.1..from.2 {
            for to_bit in to.1..to.2 {
                let entry = self
                    .bit_edges
                    .entry(bit_node(from.0, from_bit))
                    .or_default();
                let node = bit_node(to.0, to_bit);
                if !entry.contains(&node) {
                    entry.push(node);
                }
            }
        }
    }

    /// A cycle in the bit-level graph, as `(signal, bit)` pairs.
    pub(crate) fn find_bit_cycle(&self) -> Option<Vec<(usize, u32)>> {
        let cycle = find_cycle_in(&self.bit_edges)?;
        Some(cycle.into_iter().map(bit_node_parts).collect())
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

/// Depth-first search for a cycle in an adjacency map.
fn find_cycle_in(edges: &HashMap<usize, Vec<usize>>) -> Option<Vec<usize>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        Grey,
        Black,
    }

    let mut nodes: HashSet<usize> = HashSet::new();
    for (from, targets) in edges {
        nodes.insert(*from);
        nodes.extend(targets.iter().copied());
    }
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
            if let Some(targets) = edges.get(&node) {
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
