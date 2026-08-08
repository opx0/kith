//! Content hashing — the one place kith turns bytes into a digest.
//!
//! The digest binds an Item to its bytes; it is never the Item's identity, and
//! it is not a security boundary.

use std::fs::File;
use std::io::Read;
use std::path::Path;

/// The rendering prefix, so a digest written by a future kith that changes
/// algorithms is recognisably *not* ours rather than silently mis-compared.
const PREFIX: &str = "b3:";

/// Characters of digest shown to a Person.
const SHORT_LEN: usize = 12;

const BUF_LEN: usize = 1024 * 1024;

/// Hash a file's contents, rendered as `b3:` + 64 lowercase hex characters.
pub fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Hasher::new();
    let mut buf = vec![0u8; BUF_LEN];

    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            // A signal mid-read is not a failed import.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }

    Ok(hasher.finish())
}

/// Hash bytes already in memory.
pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Hasher::new();
    hasher.update(bytes);
    hasher.finish()
}

/// The first 12 characters of the digest, for display.
///
/// Lenient about a missing prefix and a malformed digest: this renders data a
/// peer wrote, and must never panic on it.
pub fn short(hash: &str) -> &str {
    let digest = hash.strip_prefix(PREFIX).unwrap_or(hash);
    // Char boundaries, not bytes: the input may be anything a peer wrote.
    let end = digest
        .char_indices()
        .nth(SHORT_LEN)
        .map_or(digest.len(), |(i, _)| i);
    &digest[..end]
}

/// Whether a string is a hash kith itself could have written.
///
/// A digest that passes here cannot contain a path separator or a `..`, so the
/// thumbnail cache cannot be walked out of by a malformed record.
pub fn is_well_formed(hash: &str) -> bool {
    match hash.strip_prefix(PREFIX) {
        Some(digest) => {
            digest.len() == 64 && digest.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        }
        None => false,
    }
}

/// Incremental hashing, for bytes that are being written as they are hashed.
pub struct Hasher(blake3::Hasher);

impl Hasher {
    pub fn new() -> Self {
        Self(blake3::Hasher::new())
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    /// The digest so far, rendered. Does not consume the Hasher.
    pub fn finish(&self) -> String {
        format!("{PREFIX}{}", self.0.finalize().to_hex())
    }
}

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The official BLAKE3 test vectors: the empty input, and `00 01 02`.
    const EMPTY: &str = "b3:af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";
    const THREE_BYTES: &str = "b3:e1be4d7a8ab5560aa4199eea339849ba8e293d55ca0a81006726d184519e647f";

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("kith-hash-tests");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn matches_the_published_vector_for_the_empty_input() {
        assert_eq!(hash_bytes(&[]), EMPTY);

        let p = scratch("empty");
        std::fs::write(&p, b"").unwrap();
        assert_eq!(hash_file(&p).unwrap(), EMPTY);
    }

    #[test]
    fn matches_the_published_vector_for_a_short_input() {
        assert_eq!(hash_bytes(&[0, 1, 2]), THREE_BYTES);

        let p = scratch("three-bytes");
        std::fs::write(&p, [0u8, 1, 2]).unwrap();
        assert_eq!(hash_file(&p).unwrap(), THREE_BYTES);
    }

    #[test]
    fn rendering_is_the_prefix_and_sixty_four_lowercase_hex() {
        let h = hash_bytes(b"sunset");
        assert!(h.starts_with("b3:"), "{h} must be self-describing");
        assert_eq!(h.len(), 67, "b3: plus 64 hex characters");
        assert!(is_well_formed(&h));
        assert_eq!(h, h.to_lowercase(), "hex is rendered lowercase, always");
    }

    #[test]
    fn streaming_agrees_with_a_single_pass_across_buffer_boundaries() {
        for len in [BUF_LEN - 1, BUF_LEN, BUF_LEN + 1, BUF_LEN * 2 + 7] {
            let bytes: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let p = scratch(&format!("stream-{len}"));
            std::fs::write(&p, &bytes).unwrap();
            assert_eq!(
                hash_file(&p).unwrap(),
                hash_bytes(&bytes),
                "buffer boundary changed the answer at {len} bytes"
            );
        }
    }

    #[test]
    fn incremental_hashing_agrees_with_one_shot() {
        let mut h = Hasher::new();
        h.update(b"sun");
        h.update(b"");
        h.update(b"set");
        assert_eq!(h.finish(), hash_bytes(b"sunset"));
        // finish() does not consume the state: a verifier may ask twice.
        assert_eq!(h.finish(), hash_bytes(b"sunset"));
    }

    #[test]
    fn the_hash_is_over_bytes_and_nothing_else() {
        let a = scratch("sunset.png");
        let b = scratch("nested-sunset.png");
        std::fs::write(&a, b"same bytes").unwrap();
        std::fs::write(&b, b"same bytes").unwrap();
        assert_eq!(hash_file(&a).unwrap(), hash_file(&b).unwrap());

        std::fs::write(&b, b"same bytes.").unwrap();
        assert_ne!(hash_file(&a).unwrap(), hash_file(&b).unwrap());
    }

    #[test]
    fn short_drops_the_prefix_and_keeps_twelve() {
        assert_eq!(short(EMPTY), "af1349b9f5f9");
        assert_eq!(short(EMPTY).len(), SHORT_LEN);
        assert_eq!(short("af1349b9f5f9a1a6"), "af1349b9f5f9");
        assert_eq!(short("b3:abc"), "abc");
        assert_eq!(short(""), "");
    }

    #[test]
    fn short_survives_a_malformed_hash_from_a_peer() {
        assert_eq!(short("b3:éé"), "éé");
        assert_eq!(short("b3:ééééééééééééééé").chars().count(), SHORT_LEN);
        assert_eq!(short("🌅"), "🌅");
    }

    #[test]
    fn only_a_real_digest_may_become_a_cache_filename() {
        assert!(!is_well_formed("af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"));
        assert!(!is_well_formed("b3:../../etc/passwd"));
        assert!(!is_well_formed("b3:AF1349B9F5F9A1A6A0404DEA36DCC9499BCB25C9ADC112B7CC9A93CAE41F3262"));
        assert!(!is_well_formed("b3:af1349"));
        assert!(!is_well_formed(""));
    }

    #[test]
    fn a_missing_source_is_an_error_the_caller_can_read() {
        let e = hash_file(&scratch("never-written")).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn a_directory_is_an_error_rather_than_a_panic() {
        let dir = scratch("a-directory");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(hash_file(&dir).is_err());
    }
}
