//! Convex wire codecs, ported from `get-convex/convex-backend@main`
//! (`crates/value/src/{id_v6,base32,json}.rs`) so Aster can decode the
//! strings and values a Convex-compiled function emits at the
//! `await ctx.db.get(id)` / `await ctx.db.query(...).collect()` boundary.
//!
//! ## Modules
//!
//! - [`base32`]: Crockford lowercase base32 (no padding, no permissive
//!   decoder). Backs `idv6` but is exposed for tests.
//! - [`idv6`]: `DocumentIdV6` — `(table_number, internal_id)` pairs
//!   encoded as `[ VInt(table_number) ] [ internal_id (16 bytes) ]
//!   [ footer (2 bytes) ]` with `footer = fletcher16(rest) ^ version`,
//!   then base32-encoded with the alphabet above. `aster-store-postgres`
//!   uses this + the `_tables`-backed mapping cache (#96) to turn the
//!   string a JS bundle hands to `db.get(id)` into the on-disk
//!   `(table_id, id)` byte pair.
//! - [`value`]: `ConvexValue` — the discriminated JSON wire shape Convex
//!   uses to ferry typed values through JSON without precision loss
//!   (`{"$integer": "..."}`, `{"$float": "..."}`, `{"$bytes": "..."}`).
//!   Round-trips cleanly through `from_json` / `to_json`.
//!
//! ## What's NOT here
//!
//! - The Convex `Storage` / `_modules` / `_source_packages` layer.
//!   Loading a real `convex/_generated/server.ts` bundle is a separate
//!   piece (#98) and lives in `aster-store-postgres` once it lands —
//!   it pulls the bundle bytes from local FS or S3 by `storage_key`,
//!   not from the `documents` table.

pub mod base32;
pub mod idv6;
pub mod value;

pub use idv6::{DocumentIdV6, IdDecodeError};
pub use value::{ConvexValue, ValueDecodeError};
