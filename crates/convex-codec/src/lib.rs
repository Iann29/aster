//! Convex IDv6 + Crockford-base32 codec, ported from
//! `get-convex/convex-backend` (`crates/value/src/{id_v6.rs,base32.rs}`)
//! so Aster can decode an ID a Convex-compiled function emits when it
//! calls `await ctx.db.get(id)`.
//!
//! The wire format (verbatim from upstream's docstring):
//!
//! ```text
//! document_id = [ VInt(table_number) ] [ internal ID (16 bytes) ] [ footer (2 bytes) ]
//! footer      = fletcher16( [VInt(table_number)] [internal ID] ) ^ version
//! ```
//!
//! Then the binary is base32-encoded with Crockford's lowercase alphabet
//! (`0123456789abcdefghjkmnpqrstvwxyz`).
//!
//! Aster's `aster-store-postgres` adapter takes a `DocumentId` of the
//! form `<table_hex>/<id_hex>` (16-byte tablet UUID + 16-byte InternalId
//! both hex-encoded). This crate gives us the bridge: decode an IDv6
//! string to `(table_number, internal_id_16)`, look the table_number up
//! in a table-mapping cache (commit #96, separate slice), produce the
//! `<tablet_hex>/<id_hex>` string the store wants. Encoding is
//! symmetric — useful for tests.

pub mod base32;
pub mod idv6;
pub mod value;

pub use idv6::{DocumentIdV6, IdDecodeError};
pub use value::{ConvexValue, ValueDecodeError};
