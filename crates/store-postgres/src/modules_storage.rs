//! Modules storage — fetch the bundle bytes a `_source_packages`
//! row points at.
//!
//! Pairs with the module index (`module_index.rs`): the index gives
//! you a `ModuleDescriptor` with `storage_key`, this module turns
//! that into the actual zipped JavaScript bundle bytes.
//!
//! ## Layout (self-hosted Convex, local FS)
//!
//! Convex's `LocalDirStorage` writes one file per object key under
//! the configured directory:
//!
//! ```text
//! <base_dir>/modules/<storage_key>.blob
//! ```
//!
//! `<base_dir>` is whatever `--local-storage-dir` (or
//! `LOCAL_STORAGE_DIR` env) on the convex-backend container points
//! at. Self-hosted compose typically mounts a host directory.
//! Aster's brokerd mounts the SAME directory read-only and reads
//! through this adapter.
//!
//! S3 / cloud storage is the same trait but a different impl —
//! lands in a follow-up. For now, the LocalDir variant covers every
//! `kind=aster` deployment Synapse provisions.
//!
//! ## What this module does NOT do
//!
//! - Unzip the bundle. The `.blob` file is itself a ZIP that
//!   contains `<module_path>.js` and `<module_path>.js.map` per
//!   upstream's bundler. The cell-side loader picks the right entry
//!   out of the ZIP — that's the next slice (#98 fatia 3).
//! - Validate the bundle's structure. We just hash-check the bytes.
//! - Cache. The eventual loader will cache parsed-and-compiled JS
//!   per `(path, source_package_id)`; raw bytes don't need their
//!   own cache layer because the FS cache already does the right
//!   thing.

use std::fs;
use std::path::{Path, PathBuf};

use aster_broker::StoreError;

use crate::module_index::ModuleDescriptor;

/// Trait every backend implements. Sync-only because the broker's
/// async island already adapts at the `PostgresCapsuleStore` level —
/// this trait stays simple.
pub(crate) trait ModulesStorage: Send + Sync {
    /// Fetch the raw bundle bytes for `descriptor`. Implementations
    /// MUST verify `bytes.sha256() == descriptor.source_package_sha256`
    /// before returning — a mismatch means either FS corruption or a
    /// stale `_source_packages` row, both of which are operator
    /// emergencies, not user errors.
    fn fetch(&self, descriptor: &ModuleDescriptor) -> Result<Vec<u8>, StoreError>;
}

/// Local-FS implementation. Reads from
/// `<base_dir>/modules/<storage_key>.blob`. The brokerd mounts the
/// directory read-only; we don't need write capability.
pub(crate) struct LocalDirModulesStorage {
    base_dir: PathBuf,
}

impl LocalDirModulesStorage {
    /// Build an adapter rooted at `base_dir`. The directory must
    /// exist at config time; missing-dir is a config error and
    /// crashes brokerd at startup rather than silently 5xx-ing
    /// every invocation. We don't *touch* the dir here (the
    /// existence check happens via the first `fetch` call) so
    /// constructor stays cheap.
    pub(crate) fn new<P: Into<PathBuf>>(base_dir: P) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// Resolve the on-disk path for a storage key. Convex's
    /// LocalDirStorage puts files at `<base>/modules/<key>.blob`
    /// (`<base>` already has `/modules` appended via
    /// `for_use_case(StorageUseCase::Modules)` upstream). Aster's
    /// adapter takes the `<base>/modules` directory directly so
    /// the operator can mount only the modules subdir without
    /// exposing user-uploaded files via the same handle.
    fn path_for(&self, storage_key: &str) -> PathBuf {
        // Convex's storage_key isn't constrained beyond "valid
        // filename"; it's a UUID-shaped string in practice. We don't
        // try to sanitise it because every byte that lands in
        // `_source_packages.storageKey` was written by the trusted
        // committer, not a tenant.
        self.base_dir.join(format!("{storage_key}.blob"))
    }
}

impl ModulesStorage for LocalDirModulesStorage {
    fn fetch(&self, descriptor: &ModuleDescriptor) -> Result<Vec<u8>, StoreError> {
        let path = self.path_for(&descriptor.storage_key);
        let bytes = fs::read(&path).map_err(|err| {
            // Distinguish "missing file" (operator misconfigured the
            // mount) from other I/O errors. The former gets a
            // clearer message because it's the most common foot-gun.
            if err.kind() == std::io::ErrorKind::NotFound {
                StoreError::Backend(format!(
                    "modules storage: bundle not found at {} for path {:?}; \
                     check that brokerd's modules dir mount points at \
                     `<convex_storage>/modules`",
                    path.display(),
                    descriptor.path
                ))
            } else {
                StoreError::Backend(format!("modules storage read {}: {err}", path.display()))
            }
        })?;

        verify_sha256(&bytes, &descriptor.source_package_sha256, &path)?;
        Ok(bytes)
    }
}

/// Hand-rolled sha256 to keep the dep graph small. The bundle blobs
/// are typically a few hundred KB — speed isn't the concern; pulling
/// in the `sha2` crate just for one verify is.
fn verify_sha256(bytes: &[u8], expected: &[u8], location: &Path) -> Result<(), StoreError> {
    let actual = sha256_digest(bytes);
    if actual != expected {
        return Err(StoreError::Backend(format!(
            "modules storage: sha256 mismatch at {} (expected {} bytes, got actual={} expected={})",
            location.display(),
            expected.len(),
            hex(&actual),
            hex(expected),
        )));
    }
    Ok(())
}

fn sha256_digest(bytes: &[u8]) -> Vec<u8> {
    // Inline SHA-256 implementation. Standard FIPS 180-4 reference.
    // Verified bit-for-bit against `openssl dgst -sha256` and the
    // RFC 6234 test vectors in unit tests below.
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    // Pre-processing: padding.
    let bit_len = (bytes.len() as u64) * 8;
    let mut padded = bytes.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = Vec::with_capacity(32);
    for word in h {
        out.extend_from_slice(&word.to_be_bytes());
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// FIPS 180-4 Appendix A test vectors. Locks the inline SHA-256
    /// against an authoritative source — a regression here would
    /// silently let corrupted bundles through.
    #[test]
    fn sha256_known_vectors() {
        let cases = [
            (
                "".as_bytes(),
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                "abc".as_bytes(),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq".as_bytes(),
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
        ];
        for (input, expected) in cases {
            let got = sha256_digest(input);
            assert_eq!(hex(&got), expected, "input={:?}", input);
        }
    }

    /// Verify pass / fail: a matching hash returns Ok, a mismatch
    /// returns Backend(_) naming both digests.
    #[test]
    fn verify_sha256_accepts_match_rejects_mismatch() {
        let bytes = b"hello, modules";
        let good = sha256_digest(bytes);
        verify_sha256(bytes, &good, Path::new("/fake")).expect("match");

        let mut bad = good.clone();
        bad[0] ^= 0xFF;
        let err = verify_sha256(bytes, &bad, Path::new("/fake")).unwrap_err();
        assert!(matches!(err, StoreError::Backend(ref msg) if msg.contains("mismatch")));
    }

    /// End-to-end round-trip via the real FS impl: write a fake
    /// bundle to a temp dir, build a descriptor whose sha256 matches,
    /// confirm `fetch` returns the same bytes.
    #[test]
    fn local_dir_fetch_round_trips() {
        let tmp = tempdir();
        let key = "abc-test-bundle";
        let bundle = b"PK\x03\x04 fake zip bytes for test only";
        let blob_path = tmp.path().join(format!("{key}.blob"));
        fs::write(&blob_path, bundle).unwrap();

        let storage = LocalDirModulesStorage::new(tmp.path());
        let descriptor = ModuleDescriptor {
            path: "messages.js".into(),
            source_package_internal_id: [0u8; 16],
            storage_key: key.into(),
            environment: "isolate".into(),
            module_sha256_base64: "irrelevant".into(),
            source_package_sha256: sha256_digest(bundle),
            source_package_unzipped_size: Some(bundle.len() as u64),
        };
        let got = storage.fetch(&descriptor).expect("fetch");
        assert_eq!(got, bundle);
    }

    /// Missing file path → typed Backend error that names the path
    /// + tells the operator what to check (mount config).
    #[test]
    fn local_dir_fetch_reports_missing_with_actionable_message() {
        let tmp = tempdir();
        let storage = LocalDirModulesStorage::new(tmp.path());
        let descriptor = ModuleDescriptor {
            path: "ghost.js".into(),
            source_package_internal_id: [0u8; 16],
            storage_key: "missing-key".into(),
            environment: "isolate".into(),
            module_sha256_base64: "x".into(),
            source_package_sha256: vec![0u8; 32],
            source_package_unzipped_size: None,
        };
        let err = storage.fetch(&descriptor).unwrap_err();
        match err {
            StoreError::Backend(msg) => {
                assert!(
                    msg.contains("ghost.js"),
                    "msg should name module path: {msg}"
                );
                assert!(
                    msg.contains("modules dir mount"),
                    "msg should hint at the mount config: {msg}"
                );
            }
            other => panic!("expected Backend(...), got {other:?}"),
        }
    }

    /// Wrong sha256 → Backend(_) mentioning both digests so the
    /// operator can spot which side drifted (FS or DB).
    #[test]
    fn local_dir_fetch_rejects_sha_mismatch() {
        let tmp = tempdir();
        let key = "drifted";
        let bundle = b"v2 bytes after a partial deploy";
        fs::write(tmp.path().join(format!("{key}.blob")), bundle).unwrap();

        // descriptor advertises a stale sha256 for the OLD bundle.
        let storage = LocalDirModulesStorage::new(tmp.path());
        let descriptor = ModuleDescriptor {
            path: "stale.js".into(),
            source_package_internal_id: [0u8; 16],
            storage_key: key.into(),
            environment: "isolate".into(),
            module_sha256_base64: "x".into(),
            source_package_sha256: sha256_digest(b"v1 stale"),
            source_package_unzipped_size: None,
        };
        let err = storage.fetch(&descriptor).unwrap_err();
        assert!(matches!(err, StoreError::Backend(ref msg) if msg.contains("mismatch")));
    }

    // Cheap stand-in for `tempfile::TempDir` — we don't have the
    // crate as a dep and don't want to add it just for tests.
    struct TempDirGuard {
        path: PathBuf,
    }
    impl TempDirGuard {
        fn path(&self) -> &Path {
            &self.path
        }
    }
    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
    fn tempdir() -> TempDirGuard {
        let mut path = std::env::temp_dir();
        let nano = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        path.push(format!(
            "aster-modules-storage-test-{nano}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create tempdir");
        // Sanity write: ensures the dir is usable before tests run.
        let probe = path.join(".probe");
        let mut f = fs::File::create(&probe).expect("probe write");
        let _ = f.write_all(b"ok");
        let _ = fs::remove_file(probe);
        TempDirGuard { path }
    }
}
