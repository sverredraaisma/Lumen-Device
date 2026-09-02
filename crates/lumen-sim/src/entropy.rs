//! Reproducible "randomness" for a simulated node.
//!
//! On hardware `Entropy` is a CSPRNG and its output must be unguessable. Here it
//! is a seeded stream, and its output must be *exactly guessable* — a nonce, a
//! UUID or a tie-breaking jitter that differed between two runs would make
//! replay meaningless, and election in particular is decided by a UUID
//! comparison, so an unseeded byte there changes who wins.
//!
//! Nothing in this file is suitable for anything that needs to be secret. That
//! is not a caveat, it is the design.

use lumen_hal::Entropy;

use crate::rng::SimRng;

/// A node's entropy source, drawn from the world seed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SimEntropy {
    rng: SimRng,
}

impl SimEntropy {
    /// A source seeded directly.
    pub fn new(seed: u64) -> Self {
        Self {
            rng: SimRng::new(seed),
        }
    }

    /// A per-node source derived from the world seed.
    ///
    /// Forked by node id so that node 2's key material does not shift when
    /// node 1 consumes a different number of bytes — otherwise adding a
    /// discovery message to one node would silently rewrite every other node's
    /// identity, and a "fix" would look like it broke the whole mesh.
    pub fn for_node(world_seed: u64, node: u16) -> Self {
        let mut root = SimRng::new(world_seed);
        Self {
            rng: root.fork(0xE47_0000 ^ node as u64),
        }
    }

    /// Bytes consumed so far, as the generator's state. Lets a test assert two
    /// runs drew the same amount.
    pub fn state(&self) -> u64 {
        self.rng.state()
    }
}

impl Entropy for SimEntropy {
    fn fill(&mut self, buf: &mut [u8]) {
        self.rng.fill_bytes(buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_seed_gives_the_same_bytes() {
        let mut a = SimEntropy::new(5);
        let mut b = SimEntropy::new(5);
        let (mut x, mut y) = ([0u8; 16], [0u8; 16]);
        a.fill(&mut x);
        b.fill(&mut y);
        assert_eq!(x, y);
        assert_eq!(a.state(), b.state());
    }

    #[test]
    fn successive_draws_differ() {
        let mut a = SimEntropy::new(5);
        let (mut x, mut y) = ([0u8; 16], [0u8; 16]);
        a.fill(&mut x);
        a.fill(&mut y);
        assert_ne!(x, y);
    }

    #[test]
    fn nodes_get_independent_streams_from_one_world_seed() {
        let mut n1 = SimEntropy::for_node(99, 1);
        let mut n2 = SimEntropy::for_node(99, 2);
        let (mut a, mut b) = ([0u8; 8], [0u8; 8]);
        n1.fill(&mut a);
        n2.fill(&mut b);
        assert_ne!(a, b, "two nodes must not share a UUID");

        // And the derivation is stable across runs.
        let mut again = SimEntropy::for_node(99, 1);
        let mut c = [0u8; 8];
        again.fill(&mut c);
        assert_eq!(a, c);
    }

    #[test]
    fn one_nodes_consumption_does_not_move_anothers() {
        let mut early = SimEntropy::for_node(7, 2);
        let mut baseline = [0u8; 8];
        early.fill(&mut baseline);

        // Node 1 burns a lot of entropy; node 2's stream must be untouched.
        let mut greedy = SimEntropy::for_node(7, 1);
        let mut waste = [0u8; 4096];
        greedy.fill(&mut waste);
        let mut late = SimEntropy::for_node(7, 2);
        let mut after = [0u8; 8];
        late.fill(&mut after);
        assert_eq!(baseline, after);
    }

    #[test]
    fn a_different_world_seed_changes_every_node() {
        let mut a = SimEntropy::for_node(1, 3);
        let mut b = SimEntropy::for_node(2, 3);
        let (mut x, mut y) = ([0u8; 8], [0u8; 8]);
        a.fill(&mut x);
        b.fill(&mut y);
        assert_ne!(x, y);
    }

    #[test]
    fn an_empty_fill_consumes_nothing() {
        let mut a = SimEntropy::new(3);
        let before = a.state();
        a.fill(&mut []);
        assert_eq!(a.state(), before);
    }
}
