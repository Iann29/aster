//! Module index — join the `_modules` system tablet with
//! `_source_packages` so a module path resolves to the storage key
//! the bundle bytes live under.
//!
//! ## Why
//!
//! When a Convex JS bundle calls `import { messages } from
//! "./messages"`, the runtime looks up the module by path
//! (`"messages.js"`) — but the source code itself isn't in the
//! `documents` table. It lives in object storage (local FS or S3),
//! and the path-to-storage indirection runs through two tablets:
//!
//! 1. `_modules` row body: `{ path, sourcePackageId, environment, sha256, ... }`
//! 2. `_source_packages` row body: `{ storageKey, sha256, packageSize, ... }`
//!
//! The `sourcePackageId` in (1) is an IDv6 string whose `internal_id`
//! is the row-id in `_source_packages`. So loading `"messages.js"` is:
//!
//! ```text
//! find row where _modules.path == "messages.js"
//!   -> source_package_id (IDv6 string)
//!   -> decode -> internal_id (16 bytes)
//!   -> fetch _source_packages where id == internal_id
//!   -> storage_key
//! ```
//!
//! That `storage_key` is what the local-FS storage adapter resolves to a
//! `<dir>/modules/<key>.blob` path. S3 remains a future adapter.
//!
//! ## Scope
//!
//! This module owns ONLY the indirection — no bundle bytes are read here.
//! `modules_storage.rs` consumes the output type (`ModuleDescriptor`) and
//! fetches the actual ZIP bytes.
//!
//! ## Cache strategy
//!
//! Single eager refresh — same pattern as the table-mapping cache.
//! Convex deploys are infrequent (`npx convex deploy` or a UI push)
//! and a brokerd's lifetime is operator-scale, so loading every
//! module on first miss + refreshing on `find_module` miss is fine.
//! A TTL or change-feed driven invalidation can land later.

use std::collections::BTreeMap;
use std::sync::RwLock;

use aster_broker::StoreError;
use aster_convex_codec::{ConvexValue, DocumentIdV6};
use deadpool_postgres::Object as PgClient;

use crate::table_mapping::{TableMappingCache, TabletUuid};

/// Joined view of `_modules` + `_source_packages` for a single module
/// path. The storage adapter takes one of these and returns bundle
/// bytes; callers can also read the path / storage_key / sha256 directly
/// to validate the wiring.
#[derive(Clone, Debug, PartialEq)]
pub struct ModuleDescriptor {
    /// Module path as the JS bundler emitted it — typically
    /// `"messages.js"`, `"_generated/api.js"`, `"http.js"`.
    pub path: String,
    /// Raw 16-byte InternalId of the source package row. Useful when
    /// callers want to chase further — most won't, since
    /// `storage_key` is the immediate next hop.
    pub source_package_internal_id: [u8; 16],
    /// Object-storage key for the bundle ZIP. Self-hosted Convex
    /// stores it at `<storage_dir>/modules/<storage_key>.blob`; cloud
    /// uses S3.
    pub storage_key: String,
    /// `"isolate"` (V8) or `"node"` (Node.js Lambda). Aster only
    /// targets isolate for v0.5; surfaced here so the loader can
    /// reject node-environment modules with a typed error rather
    /// than executing them in the wrong runtime.
    pub environment: String,
    /// Module-level sha256 (base64) — the user code's content hash,
    /// distinct from the source-package sha256 (which covers the
    /// whole zipped bundle).
    pub module_sha256_base64: String,
    /// Source-package-level sha256 (raw 32 bytes after the
    /// `{"$bytes": ...}` unwrap). The bundle file at `storage_key`
    /// must hash to this value — the storage adapter checks it.
    pub source_package_sha256: Vec<u8>,
    /// Optional unzipped size in bytes — useful for the storage
    /// adapter's pre-allocation. `None` for legacy rows that pre-date
    /// the `packageSize` field.
    pub source_package_unzipped_size: Option<u64>,
}

/// Cache of every active module-to-storage indirection.
///
/// Wraps two BTreeMaps under one RwLock so a refresh installs both
/// atomically (a half-applied refresh would let `find_module` succeed
/// but `source_package_for(...)` miss, which is confusing).
#[derive(Default)]
pub(crate) struct ModuleIndex {
    inner: RwLock<ModuleIndexState>,
}

#[derive(Default)]
struct ModuleIndexState {
    by_path: BTreeMap<String, ModuleDescriptor>,
}

impl ModuleIndex {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Look up a module by path. `None` = either not loaded yet or
    /// genuinely absent. Caller is expected to refresh + retry on
    /// `None` exactly once before erroring.
    pub(crate) fn find(&self, path: &str) -> Option<ModuleDescriptor> {
        let guard = self.inner.read().expect("module index rwlock poisoned");
        guard.by_path.get(path).cloned()
    }

    /// All known modules, sorted by path. Used by tests + by future
    /// "list deployed functions" endpoints; the hot path goes through
    /// `find`.
    pub(crate) fn list(&self) -> Vec<ModuleDescriptor> {
        let guard = self.inner.read().expect("module index rwlock poisoned");
        guard.by_path.values().cloned().collect()
    }

    /// Refresh by re-reading both tablets. Acquires the mapping
    /// cache's name index to find the system tablets — caller must
    /// ensure the mapping cache is populated first (typically via
    /// `mapping.refresh()` immediately before this call).
    pub(crate) async fn refresh(
        &self,
        client: &PgClient,
        schema: &str,
        mapping: &TableMappingCache,
    ) -> Result<(), StoreError> {
        let modules_tablet = mapping.lookup_by_name("_modules").ok_or_else(|| {
            StoreError::Backend(
                "module index: `_modules` tablet not found — \
                 was the mapping cache refreshed first?"
                    .into(),
            )
        })?;
        let source_packages_tablet =
            mapping.lookup_by_name("_source_packages").ok_or_else(|| {
                StoreError::Backend(
                    "module index: `_source_packages` tablet not found — \
                 was the mapping cache refreshed first?"
                        .into(),
                )
            })?;

        let module_rows = load_module_rows(client, schema, &modules_tablet).await?;
        let source_packages = load_source_packages(client, schema, &source_packages_tablet).await?;

        // Join: every module row points at exactly one source package
        // by IDv6. A dangling reference (module exists but source
        // package is missing/deleted) is a bug in upstream — we don't
        // crash the whole refresh, we just skip that module and let
        // the operator find out via the empty `find_module` result.
        let mut by_path = BTreeMap::new();
        for module in module_rows {
            let Some(sp) = source_packages.get(&module.source_package_internal_id) else {
                continue;
            };
            by_path.insert(
                module.path.clone(),
                ModuleDescriptor {
                    path: module.path,
                    source_package_internal_id: module.source_package_internal_id,
                    storage_key: sp.storage_key.clone(),
                    environment: module.environment,
                    module_sha256_base64: module.sha256_base64,
                    source_package_sha256: sp.sha256.clone(),
                    source_package_unzipped_size: sp.unzipped_size,
                },
            );
        }

        let mut guard = self.inner.write().expect("module index rwlock poisoned");
        guard.by_path = by_path;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn install_for_test(&self, descriptors: Vec<ModuleDescriptor>) {
        let mut guard = self.inner.write().expect("rwlock poisoned");
        guard.by_path = descriptors
            .into_iter()
            .map(|d| (d.path.clone(), d))
            .collect();
    }
}

#[derive(Debug)]
struct ModuleRow {
    path: String,
    source_package_internal_id: [u8; 16],
    environment: String,
    sha256_base64: String,
}

#[derive(Debug)]
struct SourcePackageRow {
    storage_key: String,
    sha256: Vec<u8>,
    unzipped_size: Option<u64>,
}

async fn load_module_rows(
    client: &PgClient,
    schema: &str,
    tablet: &TabletUuid,
) -> Result<Vec<ModuleRow>, StoreError> {
    // Latest revision per row, deleted rows excluded. The `_modules`
    // tablet has a `deleted` field in its body too (the `deleted`
    // boolean column is the MVCC tombstone, distinct from the body
    // soft-delete) — we filter on both.
    let rows = client
        .query(
            &format!(
                "SELECT DISTINCT ON (id) json_value, deleted \
                 FROM {schema}.documents \
                 WHERE table_id = $1 \
                 ORDER BY id ASC, ts DESC"
            ),
            &[&tablet.to_vec()],
        )
        .await
        .map_err(|err| StoreError::Backend(format!("module index modules: {err}")))?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let bytes: Vec<u8> = row.get(0);
        let deleted: Option<bool> = row.try_get(1).ok();
        if deleted.unwrap_or(false) {
            continue;
        }
        match parse_module_row(&bytes)? {
            Some(m) => out.push(m),
            // Body-level soft delete — caller asked for a deleted-flag
            // so the row stays in the tablet but isn't visible.
            None => continue,
        }
    }
    Ok(out)
}

async fn load_source_packages(
    client: &PgClient,
    schema: &str,
    tablet: &TabletUuid,
) -> Result<BTreeMap<[u8; 16], SourcePackageRow>, StoreError> {
    let rows = client
        .query(
            &format!(
                "SELECT DISTINCT ON (id) id, json_value, deleted \
                 FROM {schema}.documents \
                 WHERE table_id = $1 \
                 ORDER BY id ASC, ts DESC"
            ),
            &[&tablet.to_vec()],
        )
        .await
        .map_err(|err| StoreError::Backend(format!("module index source packages: {err}")))?;

    let mut out = BTreeMap::new();
    for row in rows {
        let id_bytes: Vec<u8> = row.get(0);
        let bytes: Vec<u8> = row.get(1);
        let deleted: Option<bool> = row.try_get(2).ok();
        if deleted.unwrap_or(false) {
            continue;
        }
        let internal_id: [u8; 16] = id_bytes.as_slice().try_into().map_err(|_| {
            StoreError::Backend(format!(
                "source package row: id is {} bytes, want 16",
                id_bytes.len()
            ))
        })?;
        let parsed = parse_source_package_row(&bytes)?;
        out.insert(internal_id, parsed);
    }
    Ok(out)
}

/// Parse a `_modules` row. Body shape (from upstream
/// `crates/model/src/modules/types.rs::SerializedModuleMetadata`):
///
/// ```json
/// {
///   "path": "messages.js",
///   "sourcePackageId": "k01...",        // IDv6 string
///   "environment": "isolate" | "node",
///   "analyzeResult": null | {...},
///   "sha256": "<base64>",
///   "deleted": true | false  // optional body-level soft delete
/// }
/// ```
///
/// Returns `Ok(None)` for body-soft-deleted rows; that's not an error
/// for the caller, just a "skip this".
fn parse_module_row(bytes: &[u8]) -> Result<Option<ModuleRow>, StoreError> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|err| StoreError::Backend(format!("module row: json parse failed: {err}")))?;
    if value
        .get("deleted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Ok(None);
    }
    let path = value
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| StoreError::Backend("module row: missing or non-string 'path'".into()))?
        .to_string();
    let source_package_id_str = value
        .get("sourcePackageId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            StoreError::Backend("module row: missing or non-string 'sourcePackageId'".into())
        })?;
    let id = DocumentIdV6::decode(source_package_id_str).map_err(|err| {
        StoreError::Backend(format!(
            "module row: sourcePackageId {source_package_id_str:?} not a valid IDv6: {err}"
        ))
    })?;
    let environment = value
        .get("environment")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            StoreError::Backend("module row: missing or non-string 'environment'".into())
        })?
        .to_string();
    let sha256_base64 = value
        .get("sha256")
        .and_then(|v| v.as_str())
        .ok_or_else(|| StoreError::Backend("module row: missing or non-string 'sha256'".into()))?
        .to_string();
    Ok(Some(ModuleRow {
        path,
        source_package_internal_id: id.internal_id,
        environment,
        sha256_base64,
    }))
}

/// Parse a `_source_packages` row. Body shape (from upstream
/// `crates/model/src/source_packages/types.rs::SerializedSourcePackage`):
///
/// ```json
/// {
///   "storageKey": "<string>",
///   "sha256":     {"$bytes": "<base64>"},      // serde_bytes via ConvexValue
///   "externalPackageId": null | "<id>",
///   "packageSize": null | {
///     "zippedSizeBytes":   {"$integer": "<base64 LE 8>"},
///     "unzippedSizeBytes": {"$integer": "<base64 LE 8>"}
///   },
///   "nodeVersion": null | "18" | "20" | ...
/// }
/// ```
///
/// We use `aster-convex-codec`'s `ConvexValue` to handle the typed
/// JSON wrappers — exactly the use case PR #13 was designed for.
fn parse_source_package_row(bytes: &[u8]) -> Result<SourcePackageRow, StoreError> {
    let raw_json: serde_json::Value = serde_json::from_slice(bytes).map_err(|err| {
        StoreError::Backend(format!("source package row: json parse failed: {err}"))
    })?;
    let value = ConvexValue::from_json(raw_json).map_err(|err| {
        StoreError::Backend(format!(
            "source package row: ConvexValue decode failed: {err}"
        ))
    })?;
    let fields = match value {
        ConvexValue::Object(fields) => fields,
        other => {
            return Err(StoreError::Backend(format!(
                "source package row: expected object, got {other:?}"
            )));
        }
    };
    let lookup = |key: &str| fields.iter().find(|(k, _)| k == key).map(|(_, v)| v);

    let storage_key = match lookup("storageKey") {
        Some(ConvexValue::String(s)) => s.clone(),
        other => {
            return Err(StoreError::Backend(format!(
                "source package row: 'storageKey' must be string, got {other:?}"
            )));
        }
    };
    let sha256 = match lookup("sha256") {
        Some(ConvexValue::Bytes(b)) => b.clone(),
        other => {
            return Err(StoreError::Backend(format!(
                "source package row: 'sha256' must be $bytes, got {other:?}"
            )));
        }
    };

    // packageSize is optional and itself an object with two
    // $integer-wrapped fields. Walk it carefully so a legacy row
    // (no field, or null field) still parses.
    let unzipped_size = match lookup("packageSize") {
        None | Some(ConvexValue::Null) => None,
        Some(ConvexValue::Object(inner)) => inner
            .iter()
            .find(|(k, _)| k == "unzippedSizeBytes")
            .and_then(|(_, v)| match v {
                ConvexValue::Int64(n) if *n >= 0 => Some(*n as u64),
                _ => None,
            }),
        Some(other) => {
            return Err(StoreError::Backend(format!(
                "source package row: 'packageSize' must be object or null, got {other:?}"
            )));
        }
    };

    Ok(SourcePackageRow {
        storage_key,
        sha256,
        unzipped_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aster_convex_codec::DocumentIdV6;

    /// Build the JSON body a Convex `_modules` row carries. Used by
    /// the parse tests below so they stay close to the real wire
    /// shape.
    fn module_row_json(path: &str, source_package_id: &DocumentIdV6) -> Vec<u8> {
        serde_json::json!({
            "path": path,
            "sourcePackageId": source_package_id.encode(),
            "environment": "isolate",
            "analyzeResult": null,
            "sha256": "deadbeef",
        })
        .to_string()
        .into_bytes()
    }

    fn source_package_row_json(storage_key: &str, unzipped: u64) -> Vec<u8> {
        // Use ConvexValue to round-trip — locks the wire shape against
        // the same codec the production path uses.
        let value = ConvexValue::object([
            ("storageKey", ConvexValue::String(storage_key.into())),
            ("sha256", ConvexValue::Bytes(b"\xCA\xFE\xBA\xBE".to_vec())),
            ("externalPackageId", ConvexValue::Null),
            (
                "packageSize",
                ConvexValue::object([
                    ("zippedSizeBytes", ConvexValue::Int64((unzipped / 2) as i64)),
                    ("unzippedSizeBytes", ConvexValue::Int64(unzipped as i64)),
                ]),
            ),
            ("nodeVersion", ConvexValue::Null),
        ]);
        value.to_json().to_string().into_bytes()
    }

    #[test]
    fn parse_module_row_extracts_fields() {
        let id = DocumentIdV6::new(10001, [0xAA; 16]);
        let bytes = module_row_json("messages.js", &id);
        let row = parse_module_row(&bytes)
            .expect("parse")
            .expect("not soft-deleted");
        assert_eq!(row.path, "messages.js");
        assert_eq!(row.source_package_internal_id, [0xAA; 16]);
        assert_eq!(row.environment, "isolate");
        assert_eq!(row.sha256_base64, "deadbeef");
    }

    #[test]
    fn parse_module_row_skips_body_soft_delete() {
        let id = DocumentIdV6::new(10001, [0xAA; 16]);
        let v = serde_json::json!({
            "path": "old.js",
            "sourcePackageId": id.encode(),
            "environment": "isolate",
            "sha256": "abc",
            "deleted": true,
        });
        let bytes = v.to_string().into_bytes();
        assert!(parse_module_row(&bytes).expect("parse").is_none());
    }

    #[test]
    fn parse_module_row_rejects_bad_idv6() {
        let v = serde_json::json!({
            "path": "x.js",
            "sourcePackageId": "not-a-real-idv6",
            "environment": "isolate",
            "sha256": "abc",
        });
        let err = parse_module_row(&v.to_string().into_bytes()).unwrap_err();
        assert!(matches!(err, StoreError::Backend(ref m) if m.contains("IDv6")));
    }

    #[test]
    fn parse_source_package_extracts_storage_key_and_size() {
        let bytes = source_package_row_json("modules/abc123", 4096);
        let row = parse_source_package_row(&bytes).expect("parse");
        assert_eq!(row.storage_key, "modules/abc123");
        assert_eq!(row.sha256, b"\xCA\xFE\xBA\xBE");
        assert_eq!(row.unzipped_size, Some(4096));
    }

    /// Legacy rows from before `packageSize` was added must still
    /// parse cleanly. The size becomes `None` so the storage adapter
    /// can fall back to "stream + measure".
    #[test]
    fn parse_source_package_legacy_no_package_size() {
        let value = ConvexValue::object([
            ("storageKey", ConvexValue::String("legacy/pkg".into())),
            ("sha256", ConvexValue::Bytes(vec![0u8; 32])),
            ("externalPackageId", ConvexValue::Null),
            ("nodeVersion", ConvexValue::Null),
            // No `packageSize` field at all.
        ]);
        let bytes = value.to_json().to_string().into_bytes();
        let row = parse_source_package_row(&bytes).expect("parse");
        assert_eq!(row.storage_key, "legacy/pkg");
        assert_eq!(row.unzipped_size, None);
    }

    #[test]
    fn parse_source_package_rejects_missing_storage_key() {
        let value = ConvexValue::object([
            ("sha256", ConvexValue::Bytes(vec![0u8; 32])),
            ("externalPackageId", ConvexValue::Null),
            ("nodeVersion", ConvexValue::Null),
        ]);
        let bytes = value.to_json().to_string().into_bytes();
        let err = parse_source_package_row(&bytes).unwrap_err();
        assert!(matches!(err, StoreError::Backend(ref m) if m.contains("storageKey")));
    }

    /// `find` round-trip on the in-memory test installation —
    /// covers the API surface without needing Postgres.
    #[test]
    fn install_and_find() {
        let idx = ModuleIndex::new();
        let descriptor = ModuleDescriptor {
            path: "messages.js".into(),
            source_package_internal_id: [0xCC; 16],
            storage_key: "modules/abc".into(),
            environment: "isolate".into(),
            module_sha256_base64: "deadbeef".into(),
            source_package_sha256: vec![0xDE, 0xAD],
            source_package_unzipped_size: Some(1024),
        };
        idx.install_for_test(vec![descriptor.clone()]);
        assert_eq!(idx.find("messages.js"), Some(descriptor.clone()));
        assert_eq!(idx.find("missing.js"), None);
        assert_eq!(idx.list(), vec![descriptor]);
    }
}
