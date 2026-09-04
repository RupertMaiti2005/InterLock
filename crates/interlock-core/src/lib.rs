//! Shared types and helpers for interlock: protocol, discovery, client, hook normalization.

pub mod client;
pub mod discovery;
pub mod hooks;
pub mod messages;
pub mod paths;
pub mod protocol;
pub mod shell;

pub use protocol::*;

/// blake3 hash of a file's content, hex encoded. `None` if the file is unreadable or absent.
pub fn hash_file(path: &std::path::Path) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    Some(blake3::hash(&data).to_hex().to_string())
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
