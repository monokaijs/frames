//! Reed-Solomon protection of one slice.
//!
//! A slice is cut into equal, even-length shards (the last padded with
//! zeros), grouped into balanced blocks of at most [`MAX_DATA_SHARDS`]. Each
//! block gets `ceil(data * parity_per_32 / 32)` parity shards, so the same
//! protection level costs the same fraction of every block, small or large.
//! Any `data` of a block's `data + parity` shards rebuild it.

use crate::wire::{MAX_DATA_SHARDS, MAX_PARITY_SHARDS};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    pub data: u8,
    pub parity: u8,
}

/// How one slice is split.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub shard_len: u16,
    pub blocks: Vec<Block>,
}

impl Layout {
    /// Splits `slice_len` bytes into shards of at most `max_shard` bytes.
    pub fn new(slice_len: usize, max_shard: usize, parity_per_32: u8) -> Self {
        assert!(slice_len > 0, "empty slice");
        let max_shard = (max_shard.min(usize::from(u16::MAX)) & !1).max(2);
        let count = slice_len.div_ceil(max_shard);
        let shard_len = slice_len.div_ceil(count).next_multiple_of(2);
        let count = slice_len.div_ceil(shard_len);
        let block_count = count.div_ceil(usize::from(MAX_DATA_SHARDS));
        let base = count / block_count;
        let extra = count % block_count;
        let blocks = (0..block_count)
            .map(|i| {
                let data = (base + usize::from(i < extra)) as u8;
                Block {
                    data,
                    parity: parity_for(data, parity_per_32),
                }
            })
            .collect();
        Self {
            shard_len: shard_len as u16,
            blocks,
        }
    }

    pub fn data_shards(&self) -> usize {
        self.blocks.iter().map(|b| usize::from(b.data)).sum()
    }

    pub fn parity_shards(&self) -> usize {
        self.blocks.iter().map(|b| usize::from(b.parity)).sum()
    }

    /// Index of the first data shard of `block` within the slice.
    pub fn first_shard(&self, block: usize) -> usize {
        self.blocks[..block]
            .iter()
            .map(|b| usize::from(b.data))
            .sum()
    }
}

/// Parity shards for a block of `data` shards.
pub fn parity_for(data: u8, parity_per_32: u8) -> u8 {
    if parity_per_32 == 0 {
        return 0;
    }
    (u16::from(data) * u16::from(parity_per_32))
        .div_ceil(u16::from(MAX_DATA_SHARDS))
        .clamp(1, u16::from(MAX_PARITY_SHARDS)) as u8
}

/// All shards of one block, data then parity.
pub struct EncodedBlock {
    pub shards: Vec<Vec<u8>>,
}

/// Splits `slice` according to `layout` and computes parity.
pub fn encode(slice: &[u8], layout: &Layout) -> Vec<EncodedBlock> {
    let len = usize::from(layout.shard_len);
    let mut out = Vec::with_capacity(layout.blocks.len());
    let mut at = 0;
    for block in &layout.blocks {
        let mut shards = Vec::with_capacity(usize::from(block.data + block.parity));
        for _ in 0..block.data {
            let end = (at + len).min(slice.len());
            let mut shard = slice[at.min(end)..end].to_vec();
            shard.resize(len, 0);
            shards.push(shard);
            at += len;
        }
        if block.parity > 0 {
            let parity = reed_solomon_simd::encode(
                usize::from(block.data),
                usize::from(block.parity),
                &shards,
            )
            .expect("layout respects the codec's shard limits");
            shards.extend(parity);
        }
        out.push(EncodedBlock { shards });
    }
    out
}

/// Rebuilds a block's data shards from any `block.data` received shards.
/// `received[i]` is shard `i` (data first, then parity) if it arrived.
pub fn reconstruct(block: Block, received: &[Option<Vec<u8>>]) -> Option<Vec<Vec<u8>>> {
    let data = usize::from(block.data);
    debug_assert_eq!(received.len(), data + usize::from(block.parity));
    if received[..data].iter().all(Option::is_some) {
        return Some(received[..data].iter().flatten().cloned().collect());
    }
    if received.iter().filter(|shard| shard.is_some()).count() < data {
        return None;
    }
    let originals = received[..data]
        .iter()
        .enumerate()
        .filter_map(|(i, shard)| shard.as_ref().map(|s| (i, s)));
    let recovery = received[data..]
        .iter()
        .enumerate()
        .filter_map(|(i, shard)| shard.as_ref().map(|s| (i, s)));
    let restored =
        reed_solomon_simd::decode(data, usize::from(block.parity), originals, recovery).ok()?;
    Some(
        (0..data)
            .map(|i| {
                received[i]
                    .clone()
                    .or_else(|| restored.get(&i).cloned())
                    .expect("decode restores every missing original")
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_are_balanced_even_and_cover_the_slice() {
        for (len, max) in [
            (1, 1159),
            (1159, 1159),
            (1160, 1159),
            (200_000, 1159),
            (7, 4),
        ] {
            let layout = Layout::new(len, max, 4);
            assert_eq!(layout.shard_len % 2, 0);
            assert!(usize::from(layout.shard_len) <= max.max(2));
            assert!(layout.data_shards() * usize::from(layout.shard_len) >= len);
            assert!((layout.data_shards() - 1) * usize::from(layout.shard_len) < len);
            let sizes: Vec<u8> = layout.blocks.iter().map(|b| b.data).collect();
            assert!(sizes.iter().all(|d| *d <= MAX_DATA_SHARDS));
            assert!(sizes.iter().max().unwrap() - sizes.iter().min().unwrap() <= 1);
        }
        assert_eq!(Layout::new(200_000, 1159, 4).blocks.len(), 6);
    }

    #[test]
    fn parity_scales_with_block_size() {
        assert_eq!(parity_for(32, 4), 4);
        assert_eq!(parity_for(16, 4), 2);
        assert_eq!(parity_for(1, 4), 1, "every protected block gets a shard");
        assert_eq!(parity_for(32, 0), 0);
        assert_eq!(parity_for(32, 64), 32, "capped by the wire format");
    }

    #[test]
    fn any_sufficient_subset_rebuilds_the_block() {
        let slice: Vec<u8> = (0..20_000u32).map(|i| (i * 7 + 3) as u8).collect();
        let layout = Layout::new(slice.len(), 1159, 8);
        let blocks = encode(&slice, &layout);
        let mut rebuilt = Vec::new();
        for (b, (block, encoded)) in layout.blocks.iter().zip(&blocks).enumerate() {
            let total = usize::from(block.data + block.parity);
            // Drop as many shards as parity allows, spread across the block.
            let mut received: Vec<Option<Vec<u8>>> =
                encoded.shards.iter().cloned().map(Some).collect();
            for k in 0..usize::from(block.parity) {
                received[(k * 3 + b) % total] = None;
            }
            rebuilt.extend(
                reconstruct(*block, &received)
                    .unwrap()
                    .into_iter()
                    .flatten(),
            );
            received[(usize::from(block.parity) * 3 + b + 1) % total] = None;
            if received.iter().filter(|s| s.is_none()).count() > usize::from(block.parity) {
                assert!(
                    reconstruct(*block, &received).is_none(),
                    "one too many lost"
                );
            }
        }
        rebuilt.truncate(slice.len());
        assert_eq!(rebuilt, slice);
    }

    #[test]
    fn unprotected_blocks_need_every_shard() {
        let slice = vec![9u8; 5_000];
        let layout = Layout::new(slice.len(), 1159, 0);
        let blocks = encode(&slice, &layout);
        let block = layout.blocks[0];
        let mut received: Vec<Option<Vec<u8>>> =
            blocks[0].shards.iter().cloned().map(Some).collect();
        assert!(reconstruct(block, &received).is_some());
        received[0] = None;
        assert!(reconstruct(block, &received).is_none());
    }
}
