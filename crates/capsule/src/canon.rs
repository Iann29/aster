//! Canonical capsule codec.
//!
//! One byte string, one meaning. The encoder emits the unique canonical form
//! (length-prefixed strings, fixed-width little-endian integers, maps in
//! strict key order); the decoder is the Capsule Transaction Theorem's
//! W-CANON repair: it rejects duplicate keys, out-of-order keys, invalid
//! tags, truncated input, oversized declared lengths, and trailing bytes,
//! and it accepts exactly the byte strings the encoder can produce. This
//! removes parser/MAC ambiguity — a permissive "last duplicate wins"
//! deserializer would let attacker-controlled bytes carry two meanings
//! across parsing and MAC recomputation.

use crate::{
    DeploymentId, Document, DocumentId, IntervalEndpoint, KeyInterval, RangeCertificate,
    ScanDirection, ScanStop, SnapshotCapsule, TenantId, Value, VersionedDocument,
};

/// v3 appends the ordered range-certificate sequence after the document map.
pub const CAPSULE_DOMAIN: &[u8] = b"aster-capsule-v3\0";

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

pub fn encode_capsule_bytes(capsule: &SnapshotCapsule) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(CAPSULE_DOMAIN);
    put_identity(&mut out, &capsule.tenant, &capsule.deployment, capsule.ts);
    put_u64(&mut out, capsule.docs.len() as u64);
    for (key, value) in &capsule.docs {
        put_str(&mut out, &key.0);
        put_versioned_document(&mut out, value);
    }
    put_u64(&mut out, capsule.ranges.len() as u64);
    for certificate in &capsule.ranges {
        put_range_certificate(&mut out, certificate);
    }
    out
}

fn put_range_certificate(out: &mut Vec<u8>, certificate: &RangeCertificate) {
    match &certificate.interval {
        KeyInterval::Prefix(prefix) => {
            out.push(0);
            put_str(out, prefix);
        }
        KeyInterval::Range { start, end } => {
            out.push(1);
            put_endpoint(out, start);
            put_endpoint(out, end);
        }
    }
    out.push(match certificate.direction {
        ScanDirection::Ascending => 0,
        ScanDirection::Descending => 1,
    });
    put_u64(out, certificate.limit);
    put_u64(out, certificate.keys.len() as u64);
    for key in &certificate.keys {
        put_str(out, &key.0);
    }
    out.push(match certificate.stop {
        ScanStop::Exhausted => 0,
        ScanStop::Boundary => 1,
    });
}

fn put_endpoint(out: &mut Vec<u8>, endpoint: &IntervalEndpoint) {
    match endpoint {
        IntervalEndpoint::Unbounded => out.push(0),
        IntervalEndpoint::Inclusive(key) => {
            out.push(1);
            put_str(out, &key.0);
        }
        IntervalEndpoint::Exclusive(key) => {
            out.push(2);
            put_str(out, &key.0);
        }
    }
}

fn put_identity(out: &mut Vec<u8>, tenant: &TenantId, deployment: &DeploymentId, ts: u64) {
    put_str(out, &tenant.0);
    put_str(out, &deployment.0);
    put_u64(out, ts);
}

fn put_versioned_document(out: &mut Vec<u8>, value: &VersionedDocument) {
    match value.version {
        Some(version) => {
            out.push(1);
            put_u64(out, version);
        }
        None => {
            out.push(0);
        }
    }
    match &value.document {
        Some(document) => {
            out.push(1);
            put_document(out, document);
        }
        None => {
            out.push(0);
        }
    }
}

fn put_document(out: &mut Vec<u8>, document: &Document) {
    put_u64(out, document.len() as u64);
    for (field, value) in document {
        put_str(out, field);
        put_value(out, value);
    }
}

fn put_value(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Int(value) => {
            out.push(b'i');
            out.extend_from_slice(&value.to_le_bytes());
        }
        Value::Text(value) => {
            out.push(b's');
            put_str(out, value);
        }
        Value::Bool(value) => {
            out.push(b'b');
            out.push(u8::from(*value));
        }
        Value::Null => {
            out.push(b'n');
        }
    }
}

pub(crate) fn put_str(out: &mut Vec<u8>, value: &str) {
    put_bytes(out, value.as_bytes());
}

pub(crate) fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u64(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

pub(crate) fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecodeError {
    UnexpectedEof,
    TrailingBytes,
    WrongDomain,
    InvalidUtf8,
    InvalidTag(u8),
    NonCanonicalOrder,
    CountTooLarge,
    LengthTooLarge,
    InvalidStructure(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnexpectedEof => write!(f, "canonical input ended prematurely"),
            Self::TrailingBytes => write!(f, "canonical input has trailing bytes"),
            Self::WrongDomain => write!(f, "canonical input has a wrong domain string"),
            Self::InvalidUtf8 => write!(f, "canonical string is not valid UTF-8"),
            Self::InvalidTag(tag) => write!(f, "invalid canonical tag byte 0x{tag:02x}"),
            Self::NonCanonicalOrder => {
                write!(
                    f,
                    "map keys are duplicated or not in strict canonical order"
                )
            }
            Self::CountTooLarge => write!(f, "declared count exceeds remaining input"),
            Self::LengthTooLarge => write!(f, "declared length exceeds remaining input"),
            Self::InvalidStructure(message) => {
                write!(f, "capsule structure is invalid: {message}")
            }
        }
    }
}

impl std::error::Error for DecodeError {}

/// Strict inverse of [`encode_capsule_bytes`]. Accepts exactly the canonical
/// encodings: `decode(encode(c)) == c` and `encode(decode(b)) == b`.
///
/// The legacy `root_hash` audit field is derived data, not wire data — it is
/// recomputed after decoding.
pub fn decode_capsule_bytes(input: &[u8]) -> Result<SnapshotCapsule, DecodeError> {
    let mut decoder = Decoder { input, pos: 0 };
    if decoder.take(CAPSULE_DOMAIN.len())? != CAPSULE_DOMAIN {
        return Err(DecodeError::WrongDomain);
    }
    let tenant = TenantId::new(decoder.str()?);
    let deployment = DeploymentId::new(decoder.str()?);
    let ts = decoder.u64()?;
    let count = decoder.count()?;
    let mut docs = std::collections::BTreeMap::new();
    let mut previous: Option<DocumentId> = None;
    for _ in 0..count {
        let key = DocumentId::new(decoder.str()?);
        if previous.as_ref().is_some_and(|prev| *prev >= key) {
            return Err(DecodeError::NonCanonicalOrder);
        }
        let value = decoder.versioned_document()?;
        previous = Some(key.clone());
        docs.insert(key, value);
    }
    let range_count = decoder.count()?;
    let mut ranges = Vec::new();
    for _ in 0..range_count {
        ranges.push(decoder.range_certificate()?);
    }
    if decoder.pos != input.len() {
        return Err(DecodeError::TrailingBytes);
    }
    let mut capsule = SnapshotCapsule {
        tenant,
        deployment,
        ts,
        docs,
        ranges,
        root_hash: 0,
    };
    capsule
        .validate_structure()
        .map_err(|error| DecodeError::InvalidStructure(error.to_string()))?;
    capsule.root_hash = capsule.compute_root_hash();
    Ok(capsule)
}

struct Decoder<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    fn remaining(&self) -> usize {
        self.input.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if n > self.remaining() {
            return Err(DecodeError::UnexpectedEof);
        }
        let slice = &self.input[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn byte(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        let bytes: [u8; 8] = self.take(8)?.try_into().expect("exact slice");
        Ok(u64::from_le_bytes(bytes))
    }

    fn i64(&mut self) -> Result<i64, DecodeError> {
        let bytes: [u8; 8] = self.take(8)?.try_into().expect("exact slice");
        Ok(i64::from_le_bytes(bytes))
    }

    /// A declared element count is bounded by the remaining input: every
    /// element costs at least one byte, so a larger count is unsatisfiable.
    /// Checking up front keeps attacker-declared counts from driving huge
    /// loop bounds or allocations.
    fn count(&mut self) -> Result<u64, DecodeError> {
        let count = self.u64()?;
        if count > self.remaining() as u64 {
            return Err(DecodeError::CountTooLarge);
        }
        Ok(count)
    }

    fn len_prefixed(&mut self) -> Result<&'a [u8], DecodeError> {
        let len = self.u64()?;
        if len > self.remaining() as u64 {
            return Err(DecodeError::LengthTooLarge);
        }
        self.take(len as usize)
    }

    fn str(&mut self) -> Result<String, DecodeError> {
        let bytes = self.len_prefixed()?;
        std::str::from_utf8(bytes)
            .map(str::to_string)
            .map_err(|_| DecodeError::InvalidUtf8)
    }

    fn versioned_document(&mut self) -> Result<VersionedDocument, DecodeError> {
        let version = match self.byte()? {
            0 => None,
            1 => Some(self.u64()?),
            tag => return Err(DecodeError::InvalidTag(tag)),
        };
        let document = match self.byte()? {
            0 => None,
            1 => Some(self.document()?),
            tag => return Err(DecodeError::InvalidTag(tag)),
        };
        Ok(VersionedDocument { version, document })
    }

    fn document(&mut self) -> Result<Document, DecodeError> {
        let count = self.count()?;
        let mut document = Document::new();
        let mut previous: Option<String> = None;
        for _ in 0..count {
            let field = self.str()?;
            if previous.as_ref().is_some_and(|prev| *prev >= field) {
                return Err(DecodeError::NonCanonicalOrder);
            }
            let value = self.value()?;
            previous = Some(field.clone());
            document.insert(field, value);
        }
        Ok(document)
    }

    fn value(&mut self) -> Result<Value, DecodeError> {
        match self.byte()? {
            b'i' => Ok(Value::Int(self.i64()?)),
            b's' => Ok(Value::Text(self.str()?)),
            b'b' => match self.byte()? {
                0 => Ok(Value::Bool(false)),
                1 => Ok(Value::Bool(true)),
                tag => Err(DecodeError::InvalidTag(tag)),
            },
            b'n' => Ok(Value::Null),
            tag => Err(DecodeError::InvalidTag(tag)),
        }
    }

    fn range_certificate(&mut self) -> Result<RangeCertificate, DecodeError> {
        let interval = match self.byte()? {
            0 => KeyInterval::Prefix(self.str()?),
            1 => KeyInterval::Range {
                start: self.endpoint()?,
                end: self.endpoint()?,
            },
            tag => return Err(DecodeError::InvalidTag(tag)),
        };
        let direction = match self.byte()? {
            0 => ScanDirection::Ascending,
            1 => ScanDirection::Descending,
            tag => return Err(DecodeError::InvalidTag(tag)),
        };
        let limit = self.u64()?;
        let key_count = self.count()?;
        let mut keys = Vec::new();
        for _ in 0..key_count {
            keys.push(DocumentId::new(self.str()?));
        }
        let stop = match self.byte()? {
            0 => ScanStop::Exhausted,
            1 => ScanStop::Boundary,
            tag => return Err(DecodeError::InvalidTag(tag)),
        };
        Ok(RangeCertificate {
            interval,
            direction,
            limit,
            keys,
            stop,
        })
    }

    fn endpoint(&mut self) -> Result<IntervalEndpoint, DecodeError> {
        match self.byte()? {
            0 => Ok(IntervalEndpoint::Unbounded),
            1 => Ok(IntervalEndpoint::Inclusive(DocumentId::new(self.str()?))),
            2 => Ok(IntervalEndpoint::Exclusive(DocumentId::new(self.str()?))),
            tag => Err(DecodeError::InvalidTag(tag)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc_with_i64;

    fn fixture_capsule() -> SnapshotCapsule {
        let mut capsule =
            SnapshotCapsule::empty(TenantId::new("tenant-a"), DeploymentId::new("dep-a"), 42);
        capsule.hydrate_point(DocumentId::new("docs/absent"), VersionedDocument::missing());
        capsule.hydrate_point(
            DocumentId::new("docs/tombstone"),
            VersionedDocument {
                version: Some(7),
                document: None,
            },
        );
        let mut document = doc_with_i64("count", -3);
        document.insert("label".to_string(), Value::Text("olá".to_string()));
        document.insert("live".to_string(), Value::Bool(true));
        document.insert("note".to_string(), Value::Null);
        capsule.hydrate_point(
            DocumentId::new("docs/full"),
            VersionedDocument {
                version: Some(9),
                document: Some(document),
            },
        );
        capsule.hydrate_range(
            RangeCertificate {
                interval: KeyInterval::Prefix("docs/f".into()),
                direction: ScanDirection::Ascending,
                limit: 5,
                keys: vec![DocumentId::new("docs/full")],
                stop: ScanStop::Exhausted,
            },
            Vec::new(),
        );
        capsule
    }

    #[test]
    fn round_trip_decode_of_encode() {
        let capsule = fixture_capsule();
        let bytes = encode_capsule_bytes(&capsule);
        let decoded = decode_capsule_bytes(&bytes).expect("decode");
        assert_eq!(decoded, capsule);
    }

    #[test]
    fn reencode_of_decode_is_byte_identical() {
        let bytes = encode_capsule_bytes(&fixture_capsule());
        let decoded = decode_capsule_bytes(&bytes).expect("decode");
        assert_eq!(encode_capsule_bytes(&decoded), bytes);
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = encode_capsule_bytes(&fixture_capsule());
        bytes.push(0);
        assert_eq!(
            decode_capsule_bytes(&bytes),
            Err(DecodeError::TrailingBytes)
        );
    }

    #[test]
    fn rejects_truncated_input() {
        let bytes = encode_capsule_bytes(&fixture_capsule());
        assert_eq!(
            decode_capsule_bytes(&bytes[..bytes.len() - 1]),
            Err(DecodeError::UnexpectedEof)
        );
    }

    #[test]
    fn rejects_wrong_domain() {
        let mut bytes = encode_capsule_bytes(&fixture_capsule());
        bytes[0] ^= 0x01;
        assert_eq!(decode_capsule_bytes(&bytes), Err(DecodeError::WrongDomain));
    }

    #[test]
    fn rejects_duplicate_doc_keys() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(CAPSULE_DOMAIN);
        put_str(&mut bytes, "tenant-a");
        put_str(&mut bytes, "dep-a");
        put_u64(&mut bytes, 1);
        put_u64(&mut bytes, 2);
        for _ in 0..2 {
            put_str(&mut bytes, "docs/dup");
            bytes.push(0);
            bytes.push(0);
        }
        assert_eq!(
            decode_capsule_bytes(&bytes),
            Err(DecodeError::NonCanonicalOrder)
        );
    }

    #[test]
    fn rejects_out_of_order_doc_keys() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(CAPSULE_DOMAIN);
        put_str(&mut bytes, "tenant-a");
        put_str(&mut bytes, "dep-a");
        put_u64(&mut bytes, 1);
        put_u64(&mut bytes, 2);
        for key in ["docs/b", "docs/a"] {
            put_str(&mut bytes, key);
            bytes.push(0);
            bytes.push(0);
        }
        assert_eq!(
            decode_capsule_bytes(&bytes),
            Err(DecodeError::NonCanonicalOrder)
        );
    }

    #[test]
    fn rejects_duplicate_document_fields() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(CAPSULE_DOMAIN);
        put_str(&mut bytes, "tenant-a");
        put_str(&mut bytes, "dep-a");
        put_u64(&mut bytes, 1);
        put_u64(&mut bytes, 1);
        put_str(&mut bytes, "docs/1");
        bytes.push(0);
        bytes.push(1);
        put_u64(&mut bytes, 2);
        for _ in 0..2 {
            put_str(&mut bytes, "field");
            bytes.push(b'n');
        }
        assert_eq!(
            decode_capsule_bytes(&bytes),
            Err(DecodeError::NonCanonicalOrder)
        );
    }

    #[test]
    fn rejects_noncanonical_bool_byte() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(CAPSULE_DOMAIN);
        put_str(&mut bytes, "tenant-a");
        put_str(&mut bytes, "dep-a");
        put_u64(&mut bytes, 1);
        put_u64(&mut bytes, 1);
        put_str(&mut bytes, "docs/1");
        bytes.push(0);
        bytes.push(1);
        put_u64(&mut bytes, 1);
        put_str(&mut bytes, "flag");
        bytes.push(b'b');
        bytes.push(2);
        assert_eq!(
            decode_capsule_bytes(&bytes),
            Err(DecodeError::InvalidTag(2))
        );
    }

    #[test]
    fn rejects_invalid_option_tag() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(CAPSULE_DOMAIN);
        put_str(&mut bytes, "tenant-a");
        put_str(&mut bytes, "dep-a");
        put_u64(&mut bytes, 1);
        put_u64(&mut bytes, 1);
        put_str(&mut bytes, "docs/1");
        bytes.push(3);
        assert_eq!(
            decode_capsule_bytes(&bytes),
            Err(DecodeError::InvalidTag(3))
        );
    }

    #[test]
    fn rejects_invalid_utf8_string() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(CAPSULE_DOMAIN);
        put_bytes(&mut bytes, &[0xFF, 0xFE]);
        assert_eq!(decode_capsule_bytes(&bytes), Err(DecodeError::InvalidUtf8));
    }

    #[test]
    fn rejects_oversized_declared_length_without_allocating() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(CAPSULE_DOMAIN);
        put_u64(&mut bytes, u64::MAX);
        assert_eq!(
            decode_capsule_bytes(&bytes),
            Err(DecodeError::LengthTooLarge)
        );
    }

    #[test]
    fn rejects_oversized_declared_count() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(CAPSULE_DOMAIN);
        put_str(&mut bytes, "tenant-a");
        put_str(&mut bytes, "dep-a");
        put_u64(&mut bytes, 1);
        put_u64(&mut bytes, u64::MAX);
        assert_eq!(
            decode_capsule_bytes(&bytes),
            Err(DecodeError::CountTooLarge)
        );
    }

    #[test]
    fn decode_recomputes_root_hash() {
        let capsule = fixture_capsule();
        let decoded = decode_capsule_bytes(&encode_capsule_bytes(&capsule)).expect("decode");
        assert_eq!(decoded.root_hash, capsule.compute_root_hash());
    }

    #[test]
    fn range_certificate_sequence_order_is_canonical() {
        let mut capsule = fixture_capsule();
        capsule.hydrate_range(
            RangeCertificate {
                interval: KeyInterval::Range {
                    start: IntervalEndpoint::Inclusive(DocumentId::new("docs/f")),
                    end: IntervalEndpoint::Unbounded,
                },
                direction: ScanDirection::Ascending,
                limit: 1,
                keys: vec![DocumentId::new("docs/full")],
                stop: ScanStop::Boundary,
            },
            Vec::new(),
        );
        let decoded = decode_capsule_bytes(&encode_capsule_bytes(&capsule)).expect("decode");
        assert_eq!(decoded.ranges.len(), 2);
        assert_eq!(decoded.ranges, capsule.ranges);
        assert_eq!(
            decoded.ranges[1].interval,
            KeyInterval::Range {
                start: IntervalEndpoint::Inclusive(DocumentId::new("docs/f")),
                end: IntervalEndpoint::Unbounded,
            }
        );
    }

    #[test]
    fn decode_rejects_stop_inconsistent_certificate() {
        let mut capsule = fixture_capsule();
        capsule.ranges[0].stop = ScanStop::Boundary;
        let bytes = encode_capsule_bytes(&capsule);
        assert!(matches!(
            decode_capsule_bytes(&bytes),
            Err(DecodeError::InvalidStructure(_))
        ));
    }

    #[test]
    fn decode_rejects_range_key_missing_from_docs() {
        let mut capsule = fixture_capsule();
        capsule.ranges[0].keys = vec![DocumentId::new("docs/ghost")];
        let bytes = encode_capsule_bytes(&capsule);
        assert!(matches!(
            decode_capsule_bytes(&bytes),
            Err(DecodeError::InvalidStructure(_))
        ));
    }

    #[test]
    fn decode_rejects_invalid_direction_tag() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(CAPSULE_DOMAIN);
        put_str(&mut bytes, "tenant-a");
        put_str(&mut bytes, "dep-a");
        put_u64(&mut bytes, 1);
        put_u64(&mut bytes, 0);
        put_u64(&mut bytes, 1);
        bytes.push(0);
        put_str(&mut bytes, "docs/");
        bytes.push(2);
        assert_eq!(
            decode_capsule_bytes(&bytes),
            Err(DecodeError::InvalidTag(2))
        );
    }
}
