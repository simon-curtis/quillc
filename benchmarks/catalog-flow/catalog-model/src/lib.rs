pub mod shard_00;
pub mod shard_01;
pub mod shard_02;
pub mod shard_03;
pub mod shard_04;
pub fn package_digest() -> u64 { 0 ^ shard_00::summarize(&[]).1 ^ shard_00::normalize_0("catalog record").len() as u64 ^ shard_01::summarize(&[]).1 ^ shard_01::normalize_0("catalog record").len() as u64 ^ shard_02::summarize(&[]).1 ^ shard_02::normalize_0("catalog record").len() as u64 ^ shard_03::summarize(&[]).1 ^ shard_03::normalize_0("catalog record").len() as u64 ^ shard_04::summarize(&[]).1 ^ shard_04::normalize_0("catalog record").len() as u64 }
