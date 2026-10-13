pub mod shard_30;
pub mod shard_31;
pub mod shard_32;
pub mod shard_33;
pub mod shard_34;
pub fn package_digest() -> u64 { catalog_model::package_digest() ^ shard_30::summarize(&[]).1 ^ shard_30::normalize_0("catalog record").len() as u64 ^ shard_31::summarize(&[]).1 ^ shard_31::normalize_0("catalog record").len() as u64 ^ shard_32::summarize(&[]).1 ^ shard_32::normalize_0("catalog record").len() as u64 ^ shard_33::summarize(&[]).1 ^ shard_33::normalize_0("catalog record").len() as u64 ^ shard_34::summarize(&[]).1 ^ shard_34::normalize_0("catalog record").len() as u64 }
