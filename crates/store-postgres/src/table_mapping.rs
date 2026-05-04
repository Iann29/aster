//! Table-mapping cache — translate Convex IDv6 `table_number` (an
//! ephemeral u32) into the 16-byte tablet UUID `documents.table_id`
//! actually keys on.
//!
//! ## Why this exists
//!
//! When a Convex JS bundle calls `db.get("k01...x")` the string is an
//! IDv6: `(table_number, internal_id)` packed with VInt + Fletcher16 +
//! Crockford base32 (see `aster-convex-codec`). The on-disk schema,
//! however, indexes documents by `(table_id, id)` where `table_id` is
//! the **tablet UUID**, not the user-facing `table_number`. The
//! mapping between the two is itself stored in Postgres, in the
//! system `_tables` tablet.
//!
//! ## Bootstrap
//!
//! 1. `persistence_globals['tables_table_id']` holds the tablet UUID
//!    of the `_tables` tablet itself, JSON-encoded as a quoted base64
//!    URL-safe (no-pad) string. (e.g. `"AbCd...xy"` — 22 chars + 2 quotes)
//! 2. Each row of the `_tables` tablet describes one table: the row's
//!    `id` IS the tablet UUID, and the `json_value` body has shape
//!    `{ "name": "...", "number": 10001, "state": "active"|"hidden"|"deleting", ... }`.
//!
//! Refresh is lazy: load on first miss. v0.5 doesn't refresh
//! periodically because the read path is read-only against a Convex
//! database that can be expected to evolve in slow motion (creating /
//! deleting a table is rare). Adding a TTL is straightforward later.
//!
//! ## What's NOT cached here
//!
//! - Tombstoned (`state = "deleting"`) and hidden tables are skipped
//!   so a reused `table_number` doesn't return the wrong tablet.
//! - Component namespaces (`_tables.namespace`) are ignored for v0.5
//!   — Aster targets the global namespace. A component-aware refresh
//!   will land alongside the module loader (#98).

use std::collections::BTreeMap;
use std::sync::RwLock;

use aster_broker::StoreError;
use base64::Engine;
use deadpool_postgres::Object as PgClient;

/// Resolved tablet identifier: 16-byte UUID-shaped Convex `InternalId`.
pub(crate) type TabletUuid = [u8; 16];

/// Snapshot of `_tables` indexed by `table_number`. Wrapped in a
/// `RwLock` so the hot path (lookups) can run without contention while
/// a refresh holds the write lock briefly.
#[derive(Default)]
pub(crate) struct TableMappingCache {
    inner: RwLock<TableMappingState>,
}

#[derive(Default)]
struct TableMappingState {
    /// Tablet UUID of `_tables` itself — the bootstrap pointer Convex
    /// writes to `persistence_globals['tables_table_id']`. `None` until
    /// the first refresh succeeds.
    tables_tablet_id: Option<TabletUuid>,
    /// Active user + system tables only. Hidden / Deleting entries are
    /// dropped so a reused number can't shadow a deleted tablet.
    by_number: BTreeMap<u32, TabletUuid>,
    /// Same data, indexed by table name. Lets the module-loader path
    /// resolve `_modules` / `_source_packages` (system tablets whose
    /// numbers vary by instance bootstrap order) without scanning.
    by_name: BTreeMap<String, TabletUuid>,
}

impl TableMappingCache {
    /// Build an empty cache. First lookup triggers a refresh.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Look up the tablet UUID for `table_number`. Returns `None` if
    /// the cache hasn't seen this number — caller should refresh and
    /// retry. We don't hide the miss because triggering Postgres
    /// roundtrips silently inside a `read_point` would be surprising.
    pub(crate) fn lookup(&self, table_number: u32) -> Option<TabletUuid> {
        let guard = self.inner.read().expect("table mapping rwlock poisoned");
        guard.by_number.get(&table_number).copied()
    }

    /// Look up the tablet UUID for a table by name (e.g. `_modules`,
    /// `_source_packages`). Used by the module loader path; system
    /// tablets are reserved to numbers under 10_000 but the *exact*
    /// number is instance-specific, so the loader resolves by name.
    pub(crate) fn lookup_by_name(&self, name: &str) -> Option<TabletUuid> {
        let guard = self.inner.read().expect("table mapping rwlock poisoned");
        guard.by_name.get(name).copied()
    }

    /// Refresh the cache from Postgres. Acquires the write lock for
    /// the duration of the SQL — short by design (two queries, one
    /// indexed lookup, one full-tablet scan).
    ///
    /// Strategy: read the bootstrap pointer from `persistence_globals`,
    /// then fetch the latest revision of every row in the `_tables`
    /// tablet. Tombstones are skipped via the `deleted` column;
    /// non-`active` states are skipped via the JSON body.
    pub(crate) async fn refresh(&self, client: &PgClient, schema: &str) -> Result<(), StoreError> {
        let tablet_id = load_tables_tablet_id(client, schema).await?;
        let (by_number, by_name) = load_active_tables(client, schema, &tablet_id).await?;
        let mut guard = self.inner.write().expect("table mapping rwlock poisoned");
        guard.tables_tablet_id = Some(tablet_id);
        guard.by_number = by_number;
        guard.by_name = by_name;
        Ok(())
    }

    /// Helper for tests — install a known mapping without going through
    /// Postgres so the table-translation paths can be exercised in
    /// pure-Rust unit tests too.
    #[cfg(test)]
    pub(crate) fn install_for_test(&self, mapping: BTreeMap<u32, TabletUuid>) {
        let mut guard = self.inner.write().expect("rwlock poisoned");
        guard.by_number = mapping;
        // Doesn't matter for lookup; set a stub so anyone reading
        // notices the cache "feels" loaded.
        guard.tables_tablet_id = Some([0u8; 16]);
    }

    /// Test helper that installs both indexes so module-loader tests
    /// can exercise `lookup_by_name` paths without a real Postgres.
    #[cfg(test)]
    pub(crate) fn install_named_for_test(
        &self,
        by_number: BTreeMap<u32, TabletUuid>,
        by_name: BTreeMap<String, TabletUuid>,
    ) {
        let mut guard = self.inner.write().expect("rwlock poisoned");
        guard.by_number = by_number;
        guard.by_name = by_name;
        guard.tables_tablet_id = Some([0u8; 16]);
    }
}

async fn load_tables_tablet_id(client: &PgClient, schema: &str) -> Result<TabletUuid, StoreError> {
    // The bootstrap pointer. Convex writes it once at instance creation
    // and never rewrites, so this is the most stable handle we have.
    let row = client
        .query_opt(
            &format!(
                "SELECT json_value FROM {schema}.persistence_globals \
                 WHERE key = 'tables_table_id'"
            ),
            &[],
        )
        .await
        .map_err(|err| StoreError::Backend(format!("table mapping pointer: {err}")))?;
    let row = row.ok_or_else(|| {
        StoreError::Backend(
            "table mapping pointer: persistence_globals['tables_table_id'] is missing — \
             database does not look like a Convex instance"
                .into(),
        )
    })?;
    let bytes: Vec<u8> = row.get(0);
    decode_tables_tablet_id(&bytes)
}

/// Bytes are JSON-encoded — `"<base64url 22 chars>"`. We strip the
/// quotes ourselves rather than dragging serde in for one decode; the
/// shape is locked by upstream's `JsonValue::String(...).to_string()`.
fn decode_tables_tablet_id(bytes: &[u8]) -> Result<TabletUuid, StoreError> {
    let s = std::str::from_utf8(bytes)
        .map_err(|err| StoreError::Backend(format!("tables_table_id is not utf-8: {err}")))?;
    let s = s.trim();
    let inner = s
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .ok_or_else(|| {
            StoreError::Backend(format!(
                "tables_table_id JSON shape: expected quoted string, got {s:?}"
            ))
        })?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(inner)
        .map_err(|err| {
            StoreError::Backend(format!("tables_table_id base64url decode failed: {err}"))
        })?;
    raw.try_into().map_err(|raw: Vec<u8>| {
        StoreError::Backend(format!(
            "tables_table_id has unexpected length {} (want 16)",
            raw.len()
        ))
    })
}

async fn load_active_tables(
    client: &PgClient,
    schema: &str,
    tables_tablet_id: &TabletUuid,
) -> Result<(BTreeMap<u32, TabletUuid>, BTreeMap<String, TabletUuid>), StoreError> {
    // Latest revision per row in the `_tables` tablet. Snapshot semantics
    // don't matter here — we want the freshest mapping the database
    // knows about. `DISTINCT ON (id) ORDER BY id, ts DESC` collapses
    // the MVCC history. The `documents_by_table_and_id` index makes
    // this the same access pattern Convex's own bootstrap uses.
    let rows = client
        .query(
            &format!(
                "SELECT DISTINCT ON (id) id, json_value, deleted \
                 FROM {schema}.documents \
                 WHERE table_id = $1 \
                 ORDER BY id ASC, ts DESC"
            ),
            &[&tables_tablet_id.to_vec()],
        )
        .await
        .map_err(|err| StoreError::Backend(format!("table mapping fetch: {err}")))?;

    let mut by_number = BTreeMap::new();
    let mut by_name = BTreeMap::new();
    for row in rows {
        let id_bytes: Vec<u8> = row.get(0);
        let json_bytes: Vec<u8> = row.get(1);
        let deleted: Option<bool> = row.try_get(2).ok();
        if deleted.unwrap_or(false) {
            continue;
        }
        let tablet_uuid: TabletUuid = id_bytes.as_slice().try_into().map_err(|_| {
            StoreError::Backend(format!(
                "table mapping row: tablet uuid should be 16 bytes, got {}",
                id_bytes.len()
            ))
        })?;
        let parsed = parse_table_metadata(&json_bytes)?;
        let TableRow {
            number,
            name,
            state,
        } = parsed;
        if state != "active" {
            continue;
        }
        by_number.insert(number, tablet_uuid);
        // System tablets like `_modules` / `_source_packages` are
        // resolved by name, not number — the exact number is bootstrap-
        // order specific. Aster v0.5 targets the global namespace only,
        // so a single name → tablet entry suffices; if/when component
        // namespaces matter the cache grows a `(namespace, name)` key.
        by_name.insert(name, tablet_uuid);
    }
    Ok((by_number, by_name))
}

#[derive(Debug)]
struct TableRow {
    number: u32,
    name: String,
    state: String,
}

/// Parse the `_tables` body JSON. The shape is
/// `{ "name": "...", "number": <i64>, "state": "active"|"hidden"|"deleting", "namespace": <obj?> }`
/// per `crates/common/src/bootstrap_model/tables.rs::SerializedTableMetadata`.
/// We extract `number`, `name`, and `state`; `namespace` is ignored
/// for v0.5 (global-namespace only).
fn parse_table_metadata(bytes: &[u8]) -> Result<TableRow, StoreError> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|err| {
        StoreError::Backend(format!("table mapping row: json parse failed: {err}"))
    })?;
    let number_raw = value
        .get("number")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| {
            StoreError::Backend("table mapping row: missing or non-integer 'number'".into())
        })?;
    if number_raw <= 0 || number_raw > u32::MAX as i64 {
        return Err(StoreError::Backend(format!(
            "table mapping row: 'number' {number_raw} out of u32 range"
        )));
    }
    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            StoreError::Backend("table mapping row: missing or non-string 'name'".into())
        })?
        .to_string();
    let state = value
        .get("state")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            StoreError::Backend("table mapping row: missing or non-string 'state'".into())
        })?
        .to_string();
    Ok(TableRow {
        number: number_raw as u32,
        name,
        state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_tables_tablet_id_handles_canonical_shape() {
        // 16 bytes encoded base64url no-pad is exactly 22 chars.
        let raw = [0xABu8; 16];
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
        assert_eq!(b64.len(), 22);
        let json = format!("\"{b64}\"");
        let decoded = decode_tables_tablet_id(json.as_bytes()).expect("decode");
        assert_eq!(decoded, raw);
    }

    #[test]
    fn decode_tables_tablet_id_rejects_unquoted() {
        // Without the JSON quotes — would be a backend bug, surface
        // it explicitly rather than corrupting the cache.
        let err = decode_tables_tablet_id(b"abcdef").unwrap_err();
        assert!(matches!(err, StoreError::Backend(_)));
    }

    #[test]
    fn decode_tables_tablet_id_rejects_short() {
        // 15 bytes encoded — must reject as wrong length, not silently
        // truncate.
        let raw = [0u8; 15];
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
        let json = format!("\"{b64}\"");
        let err = decode_tables_tablet_id(json.as_bytes()).unwrap_err();
        assert!(matches!(err, StoreError::Backend(ref msg) if msg.contains("length")));
    }

    #[test]
    fn parse_table_metadata_extracts_active_row() {
        let body = br#"{"name":"messages","number":10001,"state":"active","creationTime":1.0}"#;
        let row = parse_table_metadata(body).expect("parse");
        assert_eq!(row.number, 10001);
        assert_eq!(row.state, "active");
    }

    #[test]
    fn parse_table_metadata_rejects_negative_number() {
        let body = br#"{"name":"x","number":-1,"state":"active"}"#;
        let err = parse_table_metadata(body).unwrap_err();
        assert!(matches!(err, StoreError::Backend(ref m) if m.contains("u32 range")));
    }

    #[test]
    fn parse_table_metadata_rejects_missing_fields() {
        // Missing both `number` and `state` — both should fail loudly.
        let err = parse_table_metadata(br#"{"name":"x"}"#).unwrap_err();
        assert!(matches!(err, StoreError::Backend(ref m) if m.contains("number")));
        let err = parse_table_metadata(br#"{"name":"x","number":5}"#).unwrap_err();
        assert!(matches!(err, StoreError::Backend(ref m) if m.contains("state")));
    }

    #[test]
    fn install_for_test_round_trip() {
        let cache = TableMappingCache::new();
        let mut map = BTreeMap::new();
        let uuid = [0x42u8; 16];
        map.insert(10001, uuid);
        cache.install_for_test(map);
        assert_eq!(cache.lookup(10001), Some(uuid));
        assert_eq!(cache.lookup(99999), None);
    }
}
