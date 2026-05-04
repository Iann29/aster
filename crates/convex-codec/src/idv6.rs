//! Convex IDv6 — `(table_number, internal_id)` pairs encoded with VInt
//! + Fletcher16 footer + Crockford base32. Ported verbatim from
//! `get-convex/convex-backend@main:crates/value/src/id_v6.rs`.
//!
//! Aster only consumes the codec — it never *mints* IDs, those come
//! from Convex's own committer. We still ship the encoder so tests can
//! produce known fixtures and compare bit-for-bit against IDs the
//! upstream backend would write.

use crate::base32::{self, InvalidBase32Error};

/// The table number is encoded in one to five bytes with VInt.
const MIN_TABLE_NUMBER_LEN: usize = 1;
const MAX_TABLE_NUMBER_LEN: usize = 5;

/// The internal ID is always 16 bytes.
const INTERNAL_ID_LEN: usize = 16;

/// The footer is always two bytes: Fletcher16 of the rest XOR version.
const FOOTER_LEN: usize = 2;
const VERSION: u16 = 0;

const MIN_BINARY_LEN: usize = MIN_TABLE_NUMBER_LEN + INTERNAL_ID_LEN + FOOTER_LEN;
const MIN_BASE32_LEN: usize = base32::encoded_len(MIN_BINARY_LEN);

const MAX_BINARY_LEN: usize = MAX_TABLE_NUMBER_LEN + INTERNAL_ID_LEN + FOOTER_LEN;
const MAX_BASE32_LEN: usize = base32::encoded_len(MAX_BINARY_LEN);

/// A decoded Convex IDv6 — table number + 16-byte internal id.
///
/// `table_number` resolves to a tablet UUID via Aster's table-mapping
/// cache (separate slice). The pair `(tablet_uuid, internal_id)` is
/// what the Postgres adapter reads against `documents.(table_id, id)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct DocumentIdV6 {
    pub table_number: u32,
    pub internal_id: [u8; 16],
}

impl DocumentIdV6 {
    pub fn new(table_number: u32, internal_id: [u8; 16]) -> Self {
        debug_assert!(table_number > 0, "Convex table numbers start at 1");
        Self {
            table_number,
            internal_id,
        }
    }

    /// Encode to the canonical base32 string a Convex JS `Id<...>` carries.
    pub fn encode(&self) -> String {
        let mut buf = [0u8; MAX_BINARY_LEN];
        let mut pos = 0;

        pos += vint_encode(self.table_number, &mut buf[pos..]);
        buf[pos..(pos + 16)].copy_from_slice(&self.internal_id);
        pos += 16;

        let footer = fletcher16(&buf[..pos]) ^ VERSION;
        buf[pos..(pos + 2)].copy_from_slice(&footer.to_le_bytes());
        pos += 2;

        base32::encode(&buf[..pos])
    }

    /// Decode a base32 IDv6 string back to `(table_number, internal_id)`.
    /// Mirrors upstream's strict checks: length bounds, footer match,
    /// table-number != 0, full input consumed, and a re-encode equality
    /// check so non-canonical base32 doesn't sneak through.
    pub fn decode(s: &str) -> Result<Self, IdDecodeError> {
        if s.len() < MIN_BASE32_LEN || MAX_BASE32_LEN < s.len() {
            return Err(IdDecodeError::InvalidLength(s.len()));
        }

        let buf = base32::decode(s).map_err(IdDecodeError::InvalidBase32)?;

        let mut pos = 0;

        let (table_number, bytes_read) = vint_decode(&buf[pos..]).map_err(IdDecodeError::Vint)?;
        pos += bytes_read;
        if table_number == 0 {
            return Err(IdDecodeError::ZeroTableNumber);
        }

        let internal_id_slice = buf
            .get(pos..(pos + 16))
            .ok_or(IdDecodeError::InvalidLength(s.len()))?;
        let internal_id: [u8; 16] = internal_id_slice
            .try_into()
            .expect("slice length 16 by construction");
        pos += 16;

        let expected_footer = fletcher16(&buf[..pos]) ^ VERSION;
        let footer_slice = buf
            .get(pos..(pos + 2))
            .ok_or(IdDecodeError::InvalidLength(s.len()))?;
        let footer_bytes: [u8; 2] = footer_slice
            .try_into()
            .expect("slice length 2 by construction");
        let footer = u16::from_le_bytes(footer_bytes);
        pos += 2;

        if expected_footer != footer {
            return Err(IdDecodeError::FooterMismatch {
                actual: footer,
                expected: expected_footer,
            });
        }
        if pos != buf.len() {
            return Err(IdDecodeError::InvalidLength(s.len()));
        }

        let id = Self::new(table_number, internal_id);

        // Defence in depth: catch non-canonical base32 that passes the
        // chunk decoder. Upstream documents this as "TODO non-canonical
        // base32 still slips through `base32::decode` alone" — the
        // re-encode check is what closes it.
        if id.encode() != s {
            return Err(IdDecodeError::NonCanonical);
        }

        Ok(id)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdDecodeError {
    InvalidBase32(InvalidBase32Error),
    InvalidLength(usize),
    Vint(VintDecodeError),
    ZeroTableNumber,
    FooterMismatch { actual: u16, expected: u16 },
    NonCanonical,
}

impl std::fmt::Display for IdDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidBase32(err) => write!(f, "invalid base32: {err}"),
            Self::InvalidLength(n) => write!(f, "invalid IDv6 length {n}"),
            Self::Vint(err) => write!(f, "invalid IDv6 vint: {err:?}"),
            Self::ZeroTableNumber => write!(f, "table_number must be >= 1"),
            Self::FooterMismatch { actual, expected } => write!(
                f,
                "fletcher16 footer mismatch: actual={actual:#x} expected={expected:#x}"
            ),
            Self::NonCanonical => write!(f, "IDv6 string is not in canonical form"),
        }
    }
}

impl std::error::Error for IdDecodeError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VintDecodeError {
    TooLarge,
    Truncated,
}

// ---- VInt --------------------------------------------------------------
// Variable-length encoding of a non-negative integer. Each byte carries
// 7 payload bits; the high bit is the continuation flag.

fn vint_encode(mut n: u32, out: &mut [u8]) -> usize {
    let mut pos = 0;
    loop {
        if n < 0b1000_0000 {
            out[pos] = n as u8;
            pos += 1;
            break;
        } else {
            out[pos] = ((n & 0b0111_1111) | 0b1000_0000) as u8;
            pos += 1;
            n >>= 7;
        }
    }
    pos
}

fn vint_decode(buf: &[u8]) -> Result<(u32, usize), VintDecodeError> {
    let mut pos = 0;
    let mut n: u32 = 0;
    for i in 0.. {
        if i >= 5 {
            return Err(VintDecodeError::TooLarge);
        }
        let byte = buf
            .get(pos)
            .map(|b| *b as u32)
            .ok_or(VintDecodeError::Truncated)?;
        pos += 1;

        n |= (byte & 0b0111_1111) << (i * 7);

        if byte < 0b1000_0000 {
            break;
        }
    }
    Ok((n, pos))
}

// ---- Fletcher16 mod 256 ------------------------------------------------
// Identical to RFC 1145 Appendix I (the IP-checksum-style variant), used
// here because Convex picked it; not a security property, just a
// non-cryptographic checksum to catch typos.

fn fletcher16(buf: &[u8]) -> u16 {
    let mut c0 = 0u8;
    let mut c1 = 0u8;
    for byte in buf {
        c0 = c0.wrapping_add(*byte);
        c1 = c1.wrapping_add(c0);
    }
    ((c1 as u16) << 8) | (c0 as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip every table_number length boundary (1, 2, 3, 4, 5
    /// VInt bytes) plus a few in between. Decode-then-encode must give
    /// the same string back.
    #[test]
    fn round_trip_canonical() {
        let payload = [0xAB; 16];
        let cases = [
            1u32,
            127,
            128,
            16_383,
            16_384,
            2_097_151,
            2_097_152,
            268_435_455,
            268_435_456,
            u32::MAX,
        ];
        for &table_number in &cases {
            let id = DocumentIdV6::new(table_number, payload);
            let encoded = id.encode();
            let decoded = DocumentIdV6::decode(&encoded).unwrap_or_else(|err| {
                panic!("decode {encoded:?} for table_number={table_number}: {err}")
            });
            assert_eq!(decoded, id, "table_number={table_number}");
            assert_eq!(decoded.encode(), encoded, "non-canonical");
        }
    }

    /// All-zero internal_id with table_number=1 is the smallest legal
    /// IDv6. Nail down the actual byte count so a future encoding bug
    /// gets caught.
    #[test]
    fn min_id_has_minimum_base32_length() {
        let id = DocumentIdV6::new(1, [0u8; 16]);
        let encoded = id.encode();
        // 1 (vint) + 16 (id) + 2 (footer) = 19 bytes → encoded_len(19) = 31
        assert_eq!(encoded.len(), 31);
        assert_eq!(DocumentIdV6::decode(&encoded).unwrap(), id);
    }

    /// All-ones table_number stretches the VInt to 5 bytes.
    #[test]
    fn max_id_uses_five_byte_vint() {
        let id = DocumentIdV6::new(u32::MAX, [0xFFu8; 16]);
        let encoded = id.encode();
        // 5 + 16 + 2 = 23 bytes → encoded_len(23) = 37
        assert_eq!(encoded.len(), 37);
        assert_eq!(DocumentIdV6::decode(&encoded).unwrap(), id);
    }

    /// table_number = 0 must reject (Convex tables start at 1, the
    /// `Option<NonZeroU32>` upstream guarantees this).
    #[test]
    fn decode_rejects_zero_table_number() {
        // Synthesise a string whose VInt prefix is 0 — this requires
        // round-tripping with a zero number through the encoder, which
        // our debug_assert would normally catch. Build it by hand.
        let mut buf = [0u8; 1 + 16 + 2];
        // VInt(0) = single byte 0x00.
        buf[0] = 0;
        // internal_id = zeros (already).
        let footer = fletcher16(&buf[..17]) ^ VERSION;
        buf[17..19].copy_from_slice(&footer.to_le_bytes());
        let s = base32::encode(&buf);
        match DocumentIdV6::decode(&s) {
            Err(IdDecodeError::ZeroTableNumber) => {}
            other => panic!("expected ZeroTableNumber, got {other:?}"),
        }
    }

    /// Tampering with the internal_id middle invalidates the footer.
    #[test]
    fn decode_rejects_corrupted_payload() {
        let id = DocumentIdV6::new(7, [0x42; 16]);
        let mut s = id.encode().into_bytes();
        // Flip a base32 digit in the middle so the decoded internal_id
        // differs but the length stays valid. Pick a byte the alphabet
        // contains under both flip directions.
        let pos = s.len() / 2;
        s[pos] = if s[pos] == b'0' { b'1' } else { b'0' };
        let s = String::from_utf8(s).unwrap();
        let err = DocumentIdV6::decode(&s).unwrap_err();
        assert!(
            matches!(
                err,
                IdDecodeError::FooterMismatch { .. } | IdDecodeError::NonCanonical
            ),
            "expected FooterMismatch or NonCanonical, got {err:?}"
        );
    }

    /// A length below MIN_BASE32_LEN must reject without panicking on
    /// out-of-bounds slicing.
    #[test]
    fn decode_rejects_too_short() {
        let err = DocumentIdV6::decode("0").unwrap_err();
        assert!(matches!(err, IdDecodeError::InvalidLength(_)));
    }

    /// Empty string is shorter than MIN_BASE32_LEN.
    #[test]
    fn decode_rejects_empty() {
        let err = DocumentIdV6::decode("").unwrap_err();
        assert!(matches!(err, IdDecodeError::InvalidLength(_)));
    }
}
