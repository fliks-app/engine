//! OpenSubtitles moviehash: file size plus the little-endian u64 words of the first and last
//! 64 KiB, modulo 2^64.

pub const CHUNK: usize = 65536;

/// `None` when the file is under two chunks. `head` and `tail` are the first and last `CHUNK` bytes.
pub fn moviehash(size: u64, head: &[u8], tail: &[u8]) -> Option<String> {
    if size < (CHUNK * 2) as u64 || head.len() != CHUNK || tail.len() != CHUNK {
        return None;
    }
    let sum = head
        .as_chunks::<8>()
        .0
        .iter()
        .chain(tail.as_chunks::<8>().0)
        .fold(size, |acc, w| acc.wrapping_add(u64::from_le_bytes(*w)));
    Some(format!("{sum:016x}"))
}
