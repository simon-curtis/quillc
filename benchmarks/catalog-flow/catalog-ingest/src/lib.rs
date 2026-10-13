pub mod shard_10;
pub mod shard_11;
pub mod shard_12;
pub mod shard_13;
pub mod shard_14;
pub fn package_digest() -> u64 { catalog_parse::package_digest() ^ shard_10::summarize(&[]).1 ^ shard_10::normalize_0("catalog record").len() as u64 ^ shard_11::summarize(&[]).1 ^ shard_11::normalize_0("catalog record").len() as u64 ^ shard_12::summarize(&[]).1 ^ shard_12::normalize_0("catalog record").len() as u64 ^ shard_13::summarize(&[]).1 ^ shard_13::normalize_0("catalog record").len() as u64 ^ shard_14::summarize(&[]).1 ^ shard_14::normalize_0("catalog record").len() as u64 }
