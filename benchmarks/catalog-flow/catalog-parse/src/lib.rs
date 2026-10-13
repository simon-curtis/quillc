pub mod shard_05;
pub mod shard_06;
pub mod shard_07;
pub mod shard_08;
pub mod shard_09;
pub fn package_digest() -> u64 { catalog_model::package_digest() ^ shard_05::summarize(&[]).1 ^ shard_05::normalize_0("catalog record").len() as u64 ^ shard_06::summarize(&[]).1 ^ shard_06::normalize_0("catalog record").len() as u64 ^ shard_07::summarize(&[]).1 ^ shard_07::normalize_0("catalog record").len() as u64 ^ shard_08::summarize(&[]).1 ^ shard_08::normalize_0("catalog record").len() as u64 ^ shard_09::summarize(&[]).1 ^ shard_09::normalize_0("catalog record").len() as u64 }
