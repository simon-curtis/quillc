pub mod shard_20;
pub mod shard_21;
pub mod shard_22;
pub mod shard_23;
pub mod shard_24;
pub fn package_digest() -> u64 { catalog_validate::package_digest() ^ shard_20::summarize(&[]).1 ^ shard_20::normalize_0("catalog record").len() as u64 ^ shard_21::summarize(&[]).1 ^ shard_21::normalize_0("catalog record").len() as u64 ^ shard_22::summarize(&[]).1 ^ shard_22::normalize_0("catalog record").len() as u64 ^ shard_23::summarize(&[]).1 ^ shard_23::normalize_0("catalog record").len() as u64 ^ shard_24::summarize(&[]).1 ^ shard_24::normalize_0("catalog record").len() as u64 }
