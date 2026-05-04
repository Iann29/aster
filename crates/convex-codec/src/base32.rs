//! Crockford base32 with the lowercase variant Convex uses.
//!
//! Ported verbatim from `crates/value/src/base32.rs` in
//! `get-convex/convex-backend@main`. Original copyright:
//!
//! > Forked from https://github.com/andreasots/base32 @ 58909ac.
//! > Copyright (c) 2015 The base32 Developers — MIT License.
//!
//! Reproduction here matches Convex's choices verbatim (no permissive
//! decode, no padding) so binary compatibility holds bit-for-bit.

// Crockford's Base32 alphabet (https://www.crockford.com/base32.html) with
// lowercase alphabetical characters. We also don't decode permissively.
const ALPHABET: &[u8] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Lookup table for decoding base32 characters.
/// Maps ASCII byte values to 5-bit indices (0-31). Invalid characters
/// are marked with `0xFF`.
const DECODE_TABLE: [u8; 256] = {
    let mut table = [0xFFu8; 256];
    let mut i = 0;
    while i < 32 {
        table[ALPHABET[i] as usize] = i as u8;
        i += 1;
    }
    table
};

pub const fn encoded_len(len: usize) -> usize {
    let last_chunk = match len % 5 {
        0 => 0,
        1 => 2,
        2 => 4,
        3 => 5,
        4 => 7,
        _ => unreachable!(),
    };
    (len / 5) * 8 + last_chunk
}

pub const fn encoded_buffer_len(len: usize) -> usize {
    len.div_ceil(5) * 8
}

/// Writes the base32-encoding of `data` into `out`, which should have
/// length at least `encoded_buffer_len(data.len())`. Only the first
/// `encoded_len(data.len())` bytes of `out` should be used.
pub fn encode_into(out: &mut [u8], data: &[u8]) {
    for (chunk, out_chunk) in data.chunks(5).zip(out.chunks_mut(8)) {
        let block = chunk.try_into().unwrap_or_else(|_| {
            // Zero-extend the last chunk if necessary.
            let mut block = [0u8; 5];
            block[..chunk.len()].copy_from_slice(chunk);
            block
        });

        fn alphabet(index: u8) -> u8 {
            ALPHABET[index as usize]
        }
        out_chunk[0] = alphabet((block[0] & 0b1111_1000) >> 3);
        out_chunk[1] = alphabet((block[0] & 0b0000_0111) << 2 | ((block[1] & 0b1100_0000) >> 6));
        out_chunk[2] = alphabet((block[1] & 0b0011_1110) >> 1);
        out_chunk[3] = alphabet((block[1] & 0b0000_0001) << 4 | ((block[2] & 0b1111_0000) >> 4));
        out_chunk[4] = alphabet((block[2] & 0b0000_1111) << 1 | (block[3] >> 7));
        out_chunk[5] = alphabet((block[3] & 0b0111_1100) >> 2);
        out_chunk[6] = alphabet((block[3] & 0b0000_0011) << 3 | ((block[4] & 0b1110_0000) >> 5));
        out_chunk[7] = alphabet(block[4] & 0b0001_1111);
    }
}

pub fn encode(data: &[u8]) -> String {
    let mut out = vec![0; encoded_buffer_len(data.len())];
    encode_into(&mut out, data);
    out.truncate(encoded_len(data.len()));
    String::from_utf8(out).expect("base32 alphabet is ASCII")
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct InvalidBase32Error {
    pub character: char,
    pub position: usize,
    pub string: String,
}

impl std::fmt::Display for InvalidBase32Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid character {:?} at position {} in {:?}",
            self.character, self.position, self.string
        )
    }
}

impl std::error::Error for InvalidBase32Error {}

pub fn decode(data: &str) -> Result<Vec<u8>, InvalidBase32Error> {
    let data_bytes = data.as_bytes();
    let out_length = data_bytes.len() * 5 / 8;
    let mut out = Vec::with_capacity(out_length.div_ceil(5) * 5);

    for (chunk_idx, chunk) in data_bytes.chunks(8).enumerate() {
        let mut indexes = [0u8; 8];
        for (i, byte) in chunk.iter().enumerate() {
            let index = DECODE_TABLE[*byte as usize];
            if index == 0xFF {
                let position = chunk_idx * 8 + i;
                let character = data[position..].chars().next().unwrap_or('?');
                return Err(InvalidBase32Error {
                    character,
                    position,
                    string: data.to_string(),
                });
            }
            indexes[i] = index;
        }

        // Regroup our block of 8 5-bit indexes into 5 output bytes.
        out.push((indexes[0] << 3) | (indexes[1] >> 2));
        out.push((indexes[1] << 6) | (indexes[2] << 1) | (indexes[3] >> 4));
        out.push((indexes[3] << 4) | (indexes[4] >> 1));
        out.push((indexes[4] << 7) | (indexes[5] << 2) | (indexes[6] >> 3));
        out.push((indexes[6] << 5) | indexes[7]);
    }

    out.truncate(out_length);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_empty() {
        assert_eq!(encode(&[]), "");
        assert_eq!(decode("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn round_trip_aster_documentid_shape() {
        // 16-byte InternalId-like payload — exercises the full chunk
        // path (3 chunks of 5 bytes + 1 tail of 1 byte).
        let bytes = [0xAA; 16];
        let encoded = encode(&bytes);
        let decoded = decode(&encoded).expect("round-trip");
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn alphabet_is_crockford_lowercase() {
        // Sanity: Crockford's base32 alphabet excludes I, L, O, U to
        // keep it human-typable. Lowercase variant doesn't change that.
        assert!(!ALPHABET.contains(&b'i'));
        assert!(!ALPHABET.contains(&b'l'));
        assert!(!ALPHABET.contains(&b'o'));
        assert!(!ALPHABET.contains(&b'u'));
    }

    #[test]
    fn decode_rejects_invalid_character() {
        let err = decode("0123!").unwrap_err();
        assert_eq!(err.character, '!');
        assert_eq!(err.position, 4);
    }
}
