//! Content hashing — the binding between an Item and the bytes representing it.
//!
//! One digest per file, computed once and reused three ways (collections spec
//! §2.2): the `add`/`bind` record's `hash` field, the dedup key, and the
//! thumbnail cache key `<content-hash>-<class>.png`. Those three must agree byte
//! for byte on every Device, which is why the rendering lives in exactly one
//! place and nothing outside this module ever calls `blake3` directly.
//!
//! **The hash is a binding, never an identity** (ADR-0004 §4.1). An Item's
//! identity is its ULID and survives a move, a rename and a re-encode; only the
//! binding changes. Two Members who add the same wallpaper converge on one tile
//! because their bytes hash the same, not because their Items do.
//!
//! BLAKE3 in its default configuration: unkeyed, no derivation context, 32-byte
//! output, lowercase hex behind a `b3:` prefix. Computed over the file's bytes
//! and nothing else — no filename, no mtime, no mode.
//!
//! This is **not** a security boundary. Attribution in kith is convention, not
//! cryptography (ADR-0004 §5), so a collision-resistant hash is chosen for
//! correctness under accident, not under attack. What it does buy is ADR-0004
//! §9's affordable O(bytes) rebuild: BLAKE3 runs at GB/s, so losing the cache
//! costs seconds per 5 GB and never costs an Item.

use std::fs::File;
use std::io::Read;
use std::path::Path;

/// The rendering prefix. Self-describing, so a record written by a future kith
/// that changes algorithms is recognisably *not* ours rather than silently
/// mis-compared.
const PREFIX: &str = "b3:";

/// Characters of digest shown to a Person. 48 bits is far past the point where a
/// human would eyeball a collision, and short enough to fit a `doctor` line.
const SHORT_LEN: usize = 12;

/// 1 MiB per read (collections §2.2). Large enough that the syscall cost
/// disappears against the hashing, small enough to stay off the stack and out of
/// the way of a Collection being walked entry by entry.
const BUF_LEN: usize = 1024 * 1024;

/// Hash a file's contents, rendered as `b3:` + 64 lowercase hex characters.
///
/// Streamed rather than read whole: Collections hold thousands of wallpapers and
/// a 4K PNG is tens of megabytes, so `read_to_end` would peak at the size of the
/// largest Item for no gain.
///
/// Errors are the caller's to interpret — a vanished source mid-walk, a
/// permission denial and a directory passed by mistake all arrive here as
/// ordinary `io::Error`, and the import plan renders them as
/// `Verdict::Unreadable` rather than failing the run (collections §3.2).
///
/// *Gap noted:* collections §2.2 asks for `blake3::update_mmap_rayon` above
/// 16 MiB. That method is behind the crate's `mmap` + `rayon` features, which
/// this build does not enable, and Cargo.toml is not this module's to change.
/// Single-threaded streaming still runs at GB/s, so the wedge is unaffected;
/// switching the large-file path on is a one-line change here once the features
/// are on.
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
///
/// For the small reads that never touch a file: an 8 KiB sniff prefix, a
/// generated thumbnail, a test fixture.
pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Hasher::new();
    hasher.update(bytes);
    hasher.finish()
}

/// The first 12 characters of the digest, for display.
///
/// Takes the full rendered hash and drops the prefix. Lenient about a missing
/// prefix and about a truncated or malformed digest, because this is called from
/// render paths — `doctor` printing a duplicate copy, a `--json` envelope — and
/// a display helper that panics on a peer's malformed record would turn a
/// cosmetic problem into a dead Gallery.
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
/// Records arrive from other Devices, and a hash is used as a *filename* — the
/// thumbnail cache key is `<content-hash>-<class>.png` (ADR-0003 §5). A digest
/// checked here can never contain a path separator or a `..`, so the cache
/// cannot be walked out of by a malformed record. Callers that only display a
/// hash do not need this; callers that build a path from one do.
pub fn is_well_formed(hash: &str) -> bool {
    match hash.strip_prefix(PREFIX) {
        Some(digest) => {
            digest.len() == 64 && digest.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        }
        None => false,
    }
}

/// Incremental hashing, for bytes that are being written as they are hashed.
///
/// The import's write phase hashes the copy as it lands (collections §3.3
/// step 2) so that a source changing underneath the run is caught before the
/// record is written, rather than by a reconcile hours later. Exposing this
/// keeps the `b3:` rendering in one place instead of letting each caller
/// `format!` its own.
pub struct Hasher(blake3::Hasher);

impl Hasher {
    pub fn new() -> Self {
        Self(blake3::Hasher::new())
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    /// The digest so far, rendered. Does not consume the Hasher — a caller
    /// verifying a copy wants the answer without giving up the state.
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

    /// The official BLAKE3 test vectors. The empty input, and the 3-byte input
    /// `00 01 02` from the reference `test_vectors.json`. These are the whole
    /// point of this test module: they pin kith's rendering to the published
    /// algorithm, so a Device on a different build cannot disagree about what
    /// the same bytes hash to.
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

    /// Streaming must not depend on where the read boundaries fall — an Item
    /// larger than the buffer has to hash the same on the Device that imported
    /// it and the Device that received it.
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

    /// The incremental face and the one-shot face are the same hash, or the
    /// import's verify step would reject every copy it made.
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

    /// The hash binds bytes, not names. Two files with the same content are one
    /// dedup key; the same name over different content is not.
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
        // Lenient: a bare digest, already-short input, and empty all render.
        assert_eq!(short("af1349b9f5f9a1a6"), "af1349b9f5f9");
        assert_eq!(short("b3:abc"), "abc");
        assert_eq!(short(""), "");
    }

    /// `short` is a display helper on data a peer wrote. It renders garbage; it
    /// never panics on a char boundary.
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

    /// A directory reaching `hash_file` is a bug upstream, but it must arrive as
    /// an error the import can render as `Unreadable`, not as a panic.
    #[test]
    fn a_directory_is_an_error_rather_than_a_panic() {
        let dir = scratch("a-directory");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(hash_file(&dir).is_err());
    }
}
