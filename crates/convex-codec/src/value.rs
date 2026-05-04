//! ConvexValue — the discriminated JSON wire shape Convex apps emit
//! when they pass values across the function-runtime boundary.
//!
//! Ported from `get-convex/convex-backend@main:crates/value/src/json/`.
//! See the upstream module-level doc for the full spec; the short
//! version is:
//!
//! - `null`, `bool`, `string` map straight to JSON.
//! - Regular `f64` (finite, non-`-0.0`) maps to a JSON number.
//! - `i64` always wraps as `{"$integer": "<base64 of LE 8 bytes>"}`
//!   so JS doesn't lose precision (JS numbers are 53-bit).
//! - Special `f64` (NaN, ±Inf, `-0.0`) wraps as `{"$float": "<base64
//!   of LE 8 bytes>"}` for the same reason — JSON can't represent them.
//! - Arbitrary `bytes` wrap as `{"$bytes": "<base64>"}`.
//! - Arrays and objects recurse. Object keys can NOT start with `$`
//!   (reserved for the wrappers above).
//!
//! ## Why a discriminated wrapper?
//!
//! JS only has one numeric type (`number`, an IEEE-754 double). Convex
//! lets users store i64 and special-float values without precision
//! loss; the wrappers carry the byte representation through JSON so
//! the JS-side helpers (`jsonToConvex`) can reconstruct the exact
//! value. Without this, `42n` (BigInt) would silently round-trip as
//! `42` (number) and overflow at 2^53.
//!
//! ## Aster's role
//!
//! The cell's `Convex.asyncSyscall("1.0/get")` shim hands user code a
//! JSON string; user code parses it with `jsonToConvex` (JS helper
//! shipped with `convex/server`). The broker hands back the document
//! bytes from Postgres verbatim — but `aster-store-postgres` v0.5
//! currently stores raw JSON without re-encoding through
//! `to_internal_json`. Once a real Convex value lands in a row body,
//! we'll need this codec to round-trip it for the cell. Tests in this
//! file lock the wire shape so the future round-trip is byte-stable.

use base64::Engine;
use serde_json::Value as JsonValue;

/// A Convex-shaped value. Mirrors `value::ConvexValue` upstream
/// without the heavyweight typed-fields machinery — Aster only needs
/// the wire-format conversion. Nested arrays / objects are owned to
/// keep the API simple; if profiling shows the allocation overhead
/// matters we'll switch to a borrowed walker.
#[derive(Clone, Debug, PartialEq)]
pub enum ConvexValue {
    Null,
    Bool(bool),
    /// 64-bit signed int. Encoded as `{"$integer": "..."}` always —
    /// even small values, because the JS side reads the type from the
    /// wrapper, not by guessing from the JSON shape.
    Int64(i64),
    /// 64-bit float. Encoded as a bare JSON number when it's finite
    /// and not `-0.0`; otherwise as `{"$float": "..."}`. See
    /// `is_special_float` for the exact rule.
    Float64(f64),
    String(String),
    Bytes(Vec<u8>),
    Array(Vec<ConvexValue>),
    /// Object keys are arbitrary strings except they must NOT start
    /// with `$` (reserved for the discriminator wrappers). Decode
    /// rejects such keys; encode does not check (caller's job to
    /// build a valid value).
    ///
    /// Stored as a `Vec<(String, ConvexValue)>` rather than a `HashMap`
    /// to keep `Eq` / `Hash` derivable and to track upstream's
    /// `BTreeMap`-backed `ConvexObject`. Construct via `from_fields`
    /// so the keys are sorted ascending — round-trips through JSON
    /// then re-decode use `serde_json::Map` (BTreeMap-backed by
    /// default), which sorts; matching that here keeps `==` total.
    Object(Vec<(String, ConvexValue)>),
}

impl ConvexValue {
    /// Build an `Object` from a key/value list, sorting keys ascending
    /// so the variant has a single canonical representation. Required
    /// because `from_json` returns sorted objects (serde_json's `Map`
    /// is BTreeMap-backed) and a hand-built `Object(vec![...])` would
    /// otherwise compare unequal after a round-trip.
    pub fn object<I, S>(fields: I) -> Self
    where
        I: IntoIterator<Item = (S, ConvexValue)>,
        S: Into<String>,
    {
        let mut fields: Vec<_> = fields.into_iter().map(|(k, v)| (k.into(), v)).collect();
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        Self::Object(fields)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ValueDecodeError {
    /// `{"$integer": "..."}` payload was not 8 base64 bytes.
    BadInteger(String),
    /// `{"$float": "..."}` payload was not 8 base64 bytes — or the
    /// decoded float was finite-and-normal, which Convex requires to
    /// be encoded as a bare JSON number instead.
    BadFloat(String),
    /// `{"$bytes": "..."}` payload was not valid base64.
    BadBytes(String),
    /// Object had a `$`-prefixed key that wasn't a recognised wrapper.
    /// Both an unknown wrapper and a literal user key starting with
    /// `$` reject — the wire shape is unambiguous about this.
    ReservedKey(String),
    /// JSON number was out of `f64` range (arbitrary-precision JSON
    /// integer, larger than ~1e308). Aster doesn't support these.
    NumberOutOfRange,
}

impl std::fmt::Display for ValueDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadInteger(msg) => write!(f, "invalid $integer payload: {msg}"),
            Self::BadFloat(msg) => write!(f, "invalid $float payload: {msg}"),
            Self::BadBytes(msg) => write!(f, "invalid $bytes payload: {msg}"),
            Self::ReservedKey(k) => {
                write!(
                    f,
                    "object key {k:?} starts with '$' — reserved by ConvexValue"
                )
            }
            Self::NumberOutOfRange => write!(f, "JSON number out of f64 range"),
        }
    }
}

impl std::error::Error for ValueDecodeError {}

impl ConvexValue {
    /// Lift a `serde_json::Value` (the on-the-wire shape Convex emits)
    /// into a typed `ConvexValue`. Single-key objects whose key starts
    /// with `$` go through the discriminator path; everything else
    /// recurses structurally.
    pub fn from_json(value: JsonValue) -> Result<Self, ValueDecodeError> {
        match value {
            JsonValue::Null => Ok(Self::Null),
            JsonValue::Bool(b) => Ok(Self::Bool(b)),
            JsonValue::Number(n) => {
                let f = n.as_f64().ok_or(ValueDecodeError::NumberOutOfRange)?;
                Ok(Self::Float64(f))
            }
            JsonValue::String(s) => Ok(Self::String(s)),
            JsonValue::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(Self::from_json(item)?);
                }
                Ok(Self::Array(out))
            }
            JsonValue::Object(map) => {
                if map.len() == 1 {
                    let (k, v) = map.into_iter().next().expect("len==1");
                    return Self::decode_wrapped_or_object(k, v);
                }
                // Multi-key object: every key must be a plain user key.
                // serde_json::Map iterates in sorted order (it's a
                // BTreeMap by default), so the resulting Vec is also
                // sorted — keeps Object equality stable across
                // round-trips.
                let mut out = Vec::with_capacity(map.len());
                for (k, v) in map {
                    if k.starts_with('$') {
                        return Err(ValueDecodeError::ReservedKey(k));
                    }
                    out.push((k, Self::from_json(v)?));
                }
                Ok(Self::Object(out))
            }
        }
    }

    fn decode_wrapped_or_object(k: String, v: JsonValue) -> Result<Self, ValueDecodeError> {
        match k.as_str() {
            "$integer" => {
                let s = v.as_str().ok_or_else(|| {
                    ValueDecodeError::BadInteger("payload is not a string".into())
                })?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(s)
                    .map_err(|err| ValueDecodeError::BadInteger(format!("base64: {err}")))?;
                let arr: [u8; 8] = bytes.as_slice().try_into().map_err(|_| {
                    ValueDecodeError::BadInteger(format!("expected 8 bytes, got {}", bytes.len()))
                })?;
                Ok(Self::Int64(i64::from_le_bytes(arr)))
            }
            "$float" => {
                let s = v
                    .as_str()
                    .ok_or_else(|| ValueDecodeError::BadFloat("payload is not a string".into()))?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(s)
                    .map_err(|err| ValueDecodeError::BadFloat(format!("base64: {err}")))?;
                let arr: [u8; 8] = bytes.as_slice().try_into().map_err(|_| {
                    ValueDecodeError::BadFloat(format!("expected 8 bytes, got {}", bytes.len()))
                })?;
                let f = f64::from_le_bytes(arr);
                // Convex's invariant: `$float` only encodes the values
                // a bare JSON number can't represent (NaN, ±Inf, -0.0).
                // Reject finite-normal floats here so a hostile producer
                // can't double-encode and confuse the JS side.
                if !is_special_float(f) {
                    return Err(ValueDecodeError::BadFloat(format!(
                        "value {f} is normal — must be encoded as a bare JSON number"
                    )));
                }
                Ok(Self::Float64(f))
            }
            "$bytes" => {
                let s = v
                    .as_str()
                    .ok_or_else(|| ValueDecodeError::BadBytes("payload is not a string".into()))?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(s)
                    .map_err(|err| ValueDecodeError::BadBytes(format!("base64: {err}")))?;
                Ok(Self::Bytes(bytes))
            }
            other if other.starts_with('$') => Err(ValueDecodeError::ReservedKey(k)),
            // Single-key user object — recurse normally. Sorting a
            // single entry is a no-op, but go through `object` for
            // consistency with the multi-key path.
            _ => {
                let value = Self::from_json(v)?;
                Ok(Self::object([(k, value)]))
            }
        }
    }

    /// Lower a `ConvexValue` back to the JSON wire shape. Round-trip
    /// is exact: `from_json(to_json(v)) == v` for every value the
    /// wrapper accepts (validated by `roundtrip_*` tests).
    pub fn to_json(&self) -> JsonValue {
        match self {
            Self::Null => JsonValue::Null,
            Self::Bool(b) => JsonValue::Bool(*b),
            Self::Int64(n) => {
                let mut obj = serde_json::Map::with_capacity(1);
                let s = base64::engine::general_purpose::STANDARD.encode(n.to_le_bytes());
                obj.insert("$integer".to_string(), JsonValue::String(s));
                JsonValue::Object(obj)
            }
            Self::Float64(f) => {
                if is_special_float(*f) {
                    let mut obj = serde_json::Map::with_capacity(1);
                    let s = base64::engine::general_purpose::STANDARD.encode(f.to_le_bytes());
                    obj.insert("$float".to_string(), JsonValue::String(s));
                    JsonValue::Object(obj)
                } else {
                    // serde_json refuses to construct a Number from
                    // NaN/Inf — but is_special_float() filters those
                    // out, so this branch is total. -0.0 is also
                    // filtered out (it's "special" per Convex).
                    JsonValue::Number(
                        serde_json::Number::from_f64(*f)
                            .expect("non-special f64 is JSON-encodable"),
                    )
                }
            }
            Self::String(s) => JsonValue::String(s.clone()),
            Self::Bytes(b) => {
                let mut obj = serde_json::Map::with_capacity(1);
                let s = base64::engine::general_purpose::STANDARD.encode(b);
                obj.insert("$bytes".to_string(), JsonValue::String(s));
                JsonValue::Object(obj)
            }
            Self::Array(items) => JsonValue::Array(items.iter().map(Self::to_json).collect()),
            Self::Object(fields) => {
                let mut obj = serde_json::Map::with_capacity(fields.len());
                for (k, v) in fields {
                    obj.insert(k.clone(), v.to_json());
                }
                JsonValue::Object(obj)
            }
        }
    }
}

/// Convex routes `f64` through `$float` only when the value can't be
/// represented as a bare JSON number. That's: NaN, ±Inf, and `-0.0`
/// (because JSON treats `0` and `-0` as the same value).
///
/// Subnormal + Normal positive zero stay as bare numbers.
fn is_special_float(f: f64) -> bool {
    if f.is_nan() || f.is_infinite() {
        return true;
    }
    // Negative zero detection. `f == 0.0 && f.is_sign_negative()` is
    // the canonical pattern; Convex's upstream uses an `is_negative_zero`
    // helper with the same semantics.
    f == 0.0 && f.is_sign_negative()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> ConvexValue {
        let json: JsonValue = serde_json::from_str(s).expect("json");
        ConvexValue::from_json(json).expect("decode")
    }

    fn roundtrip(value: ConvexValue) {
        let json = value.to_json();
        let back = ConvexValue::from_json(json).expect("decode after encode");
        assert_eq!(value, back);
    }

    #[test]
    fn primitive_round_trips() {
        roundtrip(ConvexValue::Null);
        roundtrip(ConvexValue::Bool(true));
        roundtrip(ConvexValue::Bool(false));
        roundtrip(ConvexValue::String("hello".into()));
        roundtrip(ConvexValue::String(String::new()));
    }

    #[test]
    fn integer_round_trips_across_bounds() {
        for n in [0i64, 1, -1, 42, i64::MIN, i64::MAX, i32::MAX as i64 + 1] {
            roundtrip(ConvexValue::Int64(n));
        }
    }

    /// Wire shape for `$integer` is little-endian. Lock it so a
    /// serialiser change can't silently swap byte order.
    #[test]
    fn integer_wire_shape_is_little_endian() {
        let v = ConvexValue::Int64(1);
        // 1i64 LE bytes: [01, 00, 00, 00, 00, 00, 00, 00].
        // base64 of that is "AQAAAAAAAAA=".
        assert_eq!(
            v.to_json(),
            serde_json::json!({ "$integer": "AQAAAAAAAAA=" })
        );
    }

    /// Bare numbers stay bare; integers always wrap. The two paths
    /// diverge in JSON shape so the JS side knows whether to read a
    /// number or wait on the helper.
    #[test]
    fn finite_float_stays_bare_number() {
        let v = ConvexValue::Float64(1.5);
        let j = v.to_json();
        assert!(j.is_number(), "expected bare number, got {j:?}");
        assert_eq!(parse("1.5"), v);
    }

    #[test]
    fn nan_inf_neg_zero_use_dollar_float() {
        for f in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.0] {
            let v = ConvexValue::Float64(f);
            let j = v.to_json();
            assert!(j.is_object(), "expected $float wrapper for {f}, got {j:?}");
            // NaN can't `==` itself; test the wrapper shape instead of
            // a value-equality round-trip for the NaN case.
            if f.is_nan() {
                let back = ConvexValue::from_json(j).expect("decode");
                match back {
                    ConvexValue::Float64(x) => assert!(x.is_nan(), "expected NaN back"),
                    other => panic!("expected Float64(NaN), got {other:?}"),
                }
            } else {
                roundtrip(v);
            }
        }
    }

    /// A finite normal float wrapped in `$float` is a hostile shape —
    /// the producer should have used a bare number. Reject so the JS
    /// side never observes the same value via two encodings.
    #[test]
    fn dollar_float_rejects_finite_normal() {
        // 1.5 LE bytes → base64 → wrap.
        let bytes = 1.5f64.to_le_bytes();
        let payload = base64::engine::general_purpose::STANDARD.encode(bytes);
        let json = serde_json::json!({ "$float": payload });
        match ConvexValue::from_json(json) {
            Err(ValueDecodeError::BadFloat(_)) => {}
            other => panic!("expected BadFloat for finite normal, got {other:?}"),
        }
    }

    #[test]
    fn bytes_round_trip() {
        for sample in [
            Vec::<u8>::new(),
            vec![0x00],
            vec![0xFF; 32],
            (0u8..=255).collect(),
        ] {
            roundtrip(ConvexValue::Bytes(sample));
        }
    }

    #[test]
    fn nested_array_and_object_round_trip() {
        let v = ConvexValue::object([
            ("name", ConvexValue::String("ian".into())),
            (
                "tags",
                ConvexValue::Array(vec![
                    ConvexValue::String("alpha".into()),
                    ConvexValue::Int64(7),
                ]),
            ),
            ("blob", ConvexValue::Bytes(vec![1, 2, 3])),
        ]);
        roundtrip(v);
    }

    /// `object()` sorts keys ascending so the variant has a single
    /// canonical layout. Lock that property — a future refactor that
    /// drops the sort would silently break round-trips on objects
    /// constructed with arbitrary insertion order.
    #[test]
    fn object_constructor_sorts_keys() {
        let v = ConvexValue::object([
            ("zebra", ConvexValue::Null),
            ("alpha", ConvexValue::Null),
            ("middle", ConvexValue::Null),
        ]);
        let ConvexValue::Object(fields) = &v else {
            panic!("expected Object");
        };
        let keys: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["alpha", "middle", "zebra"]);
    }

    /// The single-key vs multi-key object distinction matters: a
    /// single key starting with `$` MUST be a known wrapper, but a
    /// multi-key object only triggers the reserved-key check on the
    /// individual keys.
    #[test]
    fn unknown_dollar_key_in_single_field_object_rejects() {
        let json = serde_json::json!({ "$mystery": "anything" });
        match ConvexValue::from_json(json) {
            Err(ValueDecodeError::ReservedKey(k)) => assert_eq!(k, "$mystery"),
            other => panic!("expected ReservedKey, got {other:?}"),
        }
    }

    #[test]
    fn dollar_key_in_multifield_object_rejects() {
        let json = serde_json::json!({ "$dangerous": 1, "ok": 2 });
        match ConvexValue::from_json(json) {
            Err(ValueDecodeError::ReservedKey(k)) => assert_eq!(k, "$dangerous"),
            other => panic!("expected ReservedKey, got {other:?}"),
        }
    }

    /// Single-key user object (key doesn't start with `$`) flows
    /// through the wrapper dispatch and lands on the catch-all,
    /// returning a one-field Object. Make sure the path doesn't
    /// accidentally reject.
    #[test]
    fn single_key_user_object_decodes_normally() {
        let json = serde_json::json!({ "name": "ian" });
        let v = ConvexValue::from_json(json).expect("decode");
        assert_eq!(
            v,
            ConvexValue::Object(vec![("name".into(), ConvexValue::String("ian".into()))])
        );
    }

    /// Bad payloads inside a wrapper must error rather than crash —
    /// covers the worst-case "untrusted JSON from disk" scenario.
    #[test]
    fn bad_integer_payload_rejects_cleanly() {
        let json = serde_json::json!({ "$integer": "not-base64!" });
        match ConvexValue::from_json(json) {
            Err(ValueDecodeError::BadInteger(_)) => {}
            other => panic!("expected BadInteger, got {other:?}"),
        }

        let json = serde_json::json!({ "$integer": "AAAA" });
        match ConvexValue::from_json(json) {
            Err(ValueDecodeError::BadInteger(_)) => {}
            other => panic!("expected BadInteger for short payload, got {other:?}"),
        }
    }

    #[test]
    fn bad_bytes_payload_rejects_cleanly() {
        let json = serde_json::json!({ "$bytes": "%%%not_base64%%%" });
        match ConvexValue::from_json(json) {
            Err(ValueDecodeError::BadBytes(_)) => {}
            other => panic!("expected BadBytes, got {other:?}"),
        }
    }

    /// Smoke a value that came across the wire from Convex JS: a JSON
    /// string (NOT pre-parsed). This is what `Convex.asyncSyscall`
    /// hands us today, so we want it cheap to plug in.
    #[test]
    fn parses_realistic_message_doc() {
        let s = r#"{
          "_id": "k01gh001m4cxmxq3000000000000000",
          "name": "ian",
          "score": {"$integer": "KgAAAAAAAAA="},
          "tags": ["alpha", "beta"]
        }"#;
        let v = parse(s);
        let ConvexValue::Object(fields) = v else {
            panic!("expected object");
        };
        // Order is whatever serde_json::Map preserves (stable BTree
        // ordering by default), but we don't care; just check fields.
        let mut by_name: std::collections::BTreeMap<&str, &ConvexValue> = Default::default();
        for (k, v) in &fields {
            by_name.insert(k.as_str(), v);
        }
        assert_eq!(
            by_name.get("name"),
            Some(&&ConvexValue::String("ian".into()))
        );
        // 0x2A = 42 in little-endian.
        assert_eq!(by_name.get("score"), Some(&&ConvexValue::Int64(42)));
    }
}
