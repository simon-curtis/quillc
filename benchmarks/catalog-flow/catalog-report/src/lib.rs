pub mod shard_25;
pub mod shard_26;
pub mod shard_27;
pub mod shard_28;
pub mod shard_29;
pub fn package_digest() -> u64 { catalog_index::package_digest() ^ shard_25::summarize(&[]).1 ^ shard_25::normalize_0("catalog record").len() as u64 ^ shard_26::summarize(&[]).1 ^ shard_26::normalize_0("catalog record").len() as u64 ^ shard_27::summarize(&[]).1 ^ shard_27::normalize_0("catalog record").len() as u64 ^ shard_28::summarize(&[]).1 ^ shard_28::normalize_0("catalog record").len() as u64 ^ shard_29::summarize(&[]).1 ^ shard_29::normalize_0("catalog record").len() as u64 }
