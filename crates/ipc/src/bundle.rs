//! Convex source-package ZIP unpack helper.
//!
//! `_source_packages` rows store a `storageKey` whose `.blob` file is
//! itself a ZIP archive — one entry per module the bundler emitted,
//! plus their source maps. The broker hands the cell those raw bytes
//! over `LoadModuleBundle`; this module turns them into the JS source
//! string for one chosen entry.
//!
//! ## Scope (#98 fatia 5.4.a)
//!
//! - One entry at a time. Looked up by name with a tolerant fallback
//!   on `.js` (we don't yet know the exact naming convention upstream
//!   uses for nested modules; the cell-side loader investigation in
//!   the handoff doc is the right place to make this strict later).
//! - No source-map handling, no transitive imports. Every entry is
//!   treated as a self-contained JS string. The next slice (5.4.b)
//!   compiles that string as a V8 ESM module and wires the
//!   `convex/server`, `convex/values`, `_generated/api` shims; this
//!   slice just delivers the bytes.
//!
//! ## Trust boundary
//!
//! The bytes have already been hash-verified by the broker
//! (`PostgresCapsuleStore::load_module_bundle` validates against
//! `_source_packages.sha256`). We don't re-hash here — the bytes that
//! reach this function came over a UDS the cell trusts.

use std::io::Read;

/// Errors `extract_module_source` can surface. Typed because the
/// cell's main loop maps these into actionable startup messages.
#[derive(Debug)]
pub enum BundleError {
    /// The bytes don't look like a ZIP at all (e.g. truncated download
    /// or wrong storage key on disk).
    Open(String),
    /// ZIP opened, but no entry matched the requested module path
    /// (with or without the `.js` suffix). The included `tried` list
    /// is what we looked for; `available` is what the bundle actually
    /// contains, so the operator can spot the mismatch.
    EntryNotFound {
        tried: Vec<String>,
        available: Vec<String>,
    },
    /// The matching entry exists but its bytes aren't valid UTF-8.
    /// Convex bundles are always UTF-8 JS, so this is fatal.
    NotUtf8(String),
    /// Underlying `std::io` failure pulling the entry's bytes.
    Read(String),
}

impl std::fmt::Display for BundleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open(msg) => write!(f, "bundle: open zip failed: {msg}"),
            Self::EntryNotFound { tried, available } => write!(
                f,
                "bundle: no entry matched any of {tried:?}; available = {available:?}"
            ),
            Self::NotUtf8(msg) => write!(f, "bundle: entry not UTF-8: {msg}"),
            Self::Read(msg) => write!(f, "bundle: read entry: {msg}"),
        }
    }
}

impl std::error::Error for BundleError {}

/// Pull the JS source for `module_path` out of a Convex bundle ZIP.
///
/// Lookup order:
/// 1. `module_path` verbatim — covers callers that already include
///    a `.js` suffix or hit a non-JS asset by design.
/// 2. `<module_path>.js` — covers the common Convex case where the
///    invocation path is the user's `convex/messages` and the
///    bundler emitted `convex/messages.js`.
///
/// On miss, we surface BOTH the names tried AND the names actually
/// present so an operator looking at the error can spot a casing or
/// trailing-slash mismatch immediately.
pub fn extract_module_source(zip_bytes: &[u8], module_path: &str) -> Result<String, BundleError> {
    let cursor = std::io::Cursor::new(zip_bytes);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|err| BundleError::Open(err.to_string()))?;

    let candidates = candidate_names(module_path);

    // First pass: try the candidates in priority order so callers can
    // override our `.js` heuristic by passing the explicit name.
    for candidate in &candidates {
        if let Ok(mut entry) = archive.by_name(candidate) {
            let mut buf = Vec::with_capacity(entry.size() as usize);
            entry
                .read_to_end(&mut buf)
                .map_err(|err| BundleError::Read(err.to_string()))?;
            return String::from_utf8(buf).map_err(|err| BundleError::NotUtf8(err.to_string()));
        }
    }

    // Miss: collect every entry name once for the error message.
    // `ZipArchive::file_names` already returns deduplicated, sorted
    // names — perfect for the operator-facing diagnostic.
    let available = archive
        .file_names()
        .map(|n| n.to_string())
        .collect::<Vec<_>>();
    Err(BundleError::EntryNotFound {
        tried: candidates,
        available,
    })
}

/// What we will look up inside the ZIP, in priority order. Kept as a
/// free function so the unit tests can lock the exact suffix policy
/// without exercising a live archive.
fn candidate_names(module_path: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(2);
    out.push(module_path.to_string());
    if !module_path.ends_with(".js") {
        out.push(format!("{module_path}.js"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    /// Build an in-memory ZIP that mimics the shape Convex's bundler
    /// emits: a per-module JS file + its source map. Keeps tests
    /// hermetic — we never touch the filesystem.
    fn build_bundle(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            for (name, bytes) in entries {
                zip.start_file(*name, opts).expect("start_file");
                zip.write_all(bytes).expect("write entry");
            }
            zip.finish().expect("finish");
        }
        buf
    }

    #[test]
    fn candidate_names_appends_js_when_missing() {
        assert_eq!(
            candidate_names("messages"),
            vec!["messages".to_string(), "messages.js".to_string()]
        );
    }

    #[test]
    fn candidate_names_keeps_explicit_suffix() {
        // Caller already named the entry — don't shadow it with a
        // double-`.js.js` candidate.
        assert_eq!(
            candidate_names("messages.js"),
            vec!["messages.js".to_string()]
        );
    }

    #[test]
    fn extract_finds_entry_with_implicit_js_suffix() {
        let bundle = build_bundle(&[
            ("messages.js", b"globalThis.main = async () => 'hi';"),
            ("messages.js.map", b"{}"),
        ]);
        let source = extract_module_source(&bundle, "messages").expect("extract");
        assert!(source.contains("globalThis.main"));
    }

    /// Caller already gave the explicit `.js` name — don't double-suffix.
    #[test]
    fn extract_honours_explicit_name() {
        let bundle = build_bundle(&[("ian.js", b"export const x = 1;")]);
        let source = extract_module_source(&bundle, "ian.js").expect("extract");
        assert_eq!(source, "export const x = 1;");
    }

    #[test]
    fn extract_returns_diagnostic_on_miss() {
        let bundle = build_bundle(&[("messages.js", b"// messages"), ("schema.js", b"// schema")]);
        let err = extract_module_source(&bundle, "missing").unwrap_err();
        match err {
            BundleError::EntryNotFound { tried, available } => {
                assert_eq!(tried, vec!["missing".to_string(), "missing.js".to_string()]);
                // Operator can spot the right name in `available`.
                assert!(available.contains(&"messages.js".to_string()));
                assert!(available.contains(&"schema.js".to_string()));
            }
            other => panic!("expected EntryNotFound, got {other:?}"),
        }
    }

    #[test]
    fn extract_rejects_non_zip_bytes() {
        let err = extract_module_source(b"not a zip", "messages").unwrap_err();
        assert!(matches!(err, BundleError::Open(_)));
    }

    #[test]
    fn extract_rejects_non_utf8_payload() {
        // 0xFE, 0xFF — not valid UTF-8.
        let bundle = build_bundle(&[("binary", &[0xFE, 0xFF, 0x00, 0x80])]);
        let err = extract_module_source(&bundle, "binary").unwrap_err();
        assert!(matches!(err, BundleError::NotUtf8(_)));
    }

    /// Entries with subdirectory paths must work — Convex wraps each
    /// module under its source-package-relative path, so a real
    /// bundle has names like `convex/messages.js`. Locks that we
    /// don't accidentally strip path separators.
    #[test]
    fn extract_handles_nested_paths() {
        let bundle = build_bundle(&[
            ("convex/messages.js", b"// nested"),
            ("convex/_generated/api.js", b"// gen"),
        ]);
        let messages = extract_module_source(&bundle, "convex/messages").expect("messages");
        assert_eq!(messages, "// nested");
        let api = extract_module_source(&bundle, "convex/_generated/api.js").expect("api");
        assert_eq!(api, "// gen");
    }
}
