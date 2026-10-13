pub mod shard_15;
pub mod shard_16;
pub mod shard_17;
pub mod shard_18;
pub mod shard_19;
pub fn package_digest() -> u64 { catalog_ingest::package_digest() ^ shard_15::summarize(&[]).1 ^ shard_15::normalize_0("catalog record").len() as u64 ^ shard_16::summarize(&[]).1 ^ shard_16::normalize_0("catalog record").len() as u64 ^ shard_17::summarize(&[]).1 ^ shard_17::normalize_0("catalog record").len() as u64 ^ shard_18::summarize(&[]).1 ^ shard_18::normalize_0("catalog record").len() as u64 ^ shard_19::summarize(&[]).1 ^ shard_19::normalize_0("catalog record").len() as u64 }
