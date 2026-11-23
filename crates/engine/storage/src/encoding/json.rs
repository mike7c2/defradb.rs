//! JSON path+value encoding for indexing
//!
//! Encodes JSON leaf values with their paths for secondary index keys.
//! Format: `[JSON_MARKER][path_parts][ESCAPED_TERM][PATH_END][typed_value]`
//!
//! The path is always ascending, as Go writes it; a descending index inverts
//! only the value. Path parts are encoded as:
//! - Property names: bytes, with their marker, as Go's `EncodeBytesAscending`
//! - Array indices: uvarint (always 0 per Go behavior)

use document::{JsonLeafValue, JsonPath, JsonPathPart, JsonScalarValue};

use super::{
    decode_bool_ascending, decode_bool_descending, decode_bytes_ascending, decode_bytes_descending,
    decode_float64_ascending, decode_float64_descending, decode_if_empty_array, decode_if_null,
    decode_uvarint_ascending, encode_bool_ascending, encode_bool_descending,
    encode_bytes_ascending, encode_bytes_descending, encode_empty_array_ascending,
    encode_empty_array_descending, encode_float64_ascending, encode_float64_descending,
    encode_null_ascending, encode_null_descending, encode_uvarint_ascending, peek_type,
    EncodedType, BYTES_MARKER, ENCODED_NULL, ESCAPED_TERM, INT_MAX, INT_MIN, JSON_MARKER,
};
use crate::corekv::{Error, Result};

/// Path terminator after all path parts, before value
pub const JSON_PATH_END: u8 = b'/'; // 0x2F - matches Go's jsonPathEnd

/// The byte after the path terminator that marks `PathMax`.
const JSON_PATH_MAX: u8 = JSON_PATH_END + 1;

/// Encode a JSON leaf value with path in ascending order.
///
/// The sentinels bound every value at a path, in either direction:
/// - PathMin: `[ESCAPED_TERM][PATH_END][NULL_MARKER]`, at or below every value
/// - PathMax: `[ESCAPED_TERM][PATH_END + 1]`, above every value
pub fn encode_json_ascending(buf: Vec<u8>, leaf: &JsonLeafValue) -> Vec<u8> {
    encode_json(buf, leaf, false)
}

/// Encode a JSON leaf value with path in descending order.
pub fn encode_json_descending(buf: Vec<u8>, leaf: &JsonLeafValue) -> Vec<u8> {
    encode_json(buf, leaf, true)
}

fn encode_json(mut buf: Vec<u8>, leaf: &JsonLeafValue, descending: bool) -> Vec<u8> {
    buf.push(JSON_MARKER);
    buf = encode_path(buf, &leaf.path);
    buf.push(ESCAPED_TERM);
    match &leaf.value {
        JsonScalarValue::PathMax => {
            buf.push(JSON_PATH_MAX);
            buf
        }
        // No descending value starts with the ascending null byte, so this
        // stays below them too.
        JsonScalarValue::PathMin => {
            buf.push(JSON_PATH_END);
            encode_null_ascending(buf)
        }
        value => {
            buf.push(JSON_PATH_END);
            if descending {
                encode_scalar_descending(buf, value)
            } else {
                encode_scalar_ascending(buf, value)
            }
        }
    }
}

/// Encode the JSON path parts.
fn encode_path(mut buf: Vec<u8>, path: &JsonPath) -> Vec<u8> {
    for part in path.iter() {
        match part {
            JsonPathPart::Property(name) => {
                buf = encode_bytes_ascending(buf, name.as_bytes());
            }
            JsonPathPart::Index => {
                // Array indices always encode as 0 per Go behavior
                buf = encode_uvarint_ascending(buf, 0);
            }
            _ => {}
        }
    }
    buf
}

/// Encode a JSON scalar value in ascending order.
fn encode_scalar_ascending(buf: Vec<u8>, value: &JsonScalarValue) -> Vec<u8> {
    match value {
        JsonScalarValue::Null => encode_null_ascending(buf),
        JsonScalarValue::Bool(b) => encode_bool_ascending(buf, *b),
        JsonScalarValue::Number(n) => encode_float64_ascending(buf, *n),
        JsonScalarValue::String(s) => encode_bytes_ascending(buf, s.as_bytes()),
        JsonScalarValue::EmptyArray => encode_empty_array_ascending(buf),
        // PathMin/PathMax are handled in encode_json
        JsonScalarValue::PathMin | JsonScalarValue::PathMax | _ => buf,
    }
}

/// Encode a JSON scalar value in descending order.
fn encode_scalar_descending(buf: Vec<u8>, value: &JsonScalarValue) -> Vec<u8> {
    match value {
        JsonScalarValue::Null => encode_null_descending(buf),
        JsonScalarValue::Bool(b) => encode_bool_descending(buf, *b),
        JsonScalarValue::Number(n) => encode_float64_descending(buf, *n),
        JsonScalarValue::String(s) => encode_bytes_descending(buf, s.as_bytes()),
        JsonScalarValue::EmptyArray => encode_empty_array_descending(buf),
        // PathMin/PathMax are handled in encode_json
        JsonScalarValue::PathMin | JsonScalarValue::PathMax | _ => buf,
    }
}

/// Decode a JSON leaf value from bytes (ascending order).
///
/// Returns the remaining buffer and the decoded JsonLeafValue. `PathMin` has
/// the bytes of a null here, so it reads back as `Null`.
pub fn decode_json_ascending(buf: &[u8]) -> Result<(&[u8], JsonLeafValue)> {
    let (rest, path, at_max) = decode_path(buf)?;
    if at_max {
        return Ok((rest, JsonLeafValue::new(path, JsonScalarValue::PathMax)));
    }
    let (rest, value) = decode_scalar_ascending(rest)?;
    Ok((rest, JsonLeafValue::new(path, value)))
}

/// Decode a JSON leaf value from bytes (descending order).
pub fn decode_json_descending(buf: &[u8]) -> Result<(&[u8], JsonLeafValue)> {
    let (rest, path, at_max) = decode_path(buf)?;
    if at_max {
        return Ok((rest, JsonLeafValue::new(path, JsonScalarValue::PathMax)));
    }
    // A descending null is its own byte, so the ascending one can only be PathMin.
    if let [ENCODED_NULL, rest @ ..] = rest {
        return Ok((rest, JsonLeafValue::new(path, JsonScalarValue::PathMin)));
    }
    let (rest, value) = decode_scalar_descending(rest)?;
    Ok((rest, JsonLeafValue::new(path, value)))
}

/// Decode the marker and path, through the path terminator. The flag is set
/// for a `PathMax` key, which has no value after it.
fn decode_path(buf: &[u8]) -> Result<(&[u8], JsonPath, bool)> {
    if buf.is_empty() || buf[0] != JSON_MARKER {
        return Err(Error::Other(format!(
            "expected JSON_MARKER (0x{:02x}), got 0x{:02x}",
            JSON_MARKER,
            buf.first().unwrap_or(&0)
        )));
    }

    let mut rest = &buf[1..];
    let mut path = JsonPath::new();
    loop {
        match rest {
            [ESCAPED_TERM, JSON_PATH_END, tail @ ..] => return Ok((tail, path, false)),
            [ESCAPED_TERM, JSON_PATH_MAX, tail @ ..] => return Ok((tail, path, true)),
            [BYTES_MARKER, ..] => {
                let (remaining, name) = decode_bytes_ascending(rest)?;
                let name = String::from_utf8(name)
                    .map_err(|e| Error::Other(format!("invalid utf-8 in path: {}", e)))?;
                path = path.append_property(&name);
                rest = remaining;
            }
            [first, ..] if (INT_MIN..=INT_MAX).contains(first) => {
                let (remaining, _idx) = decode_uvarint_ascending(rest)?;
                path = path.append_index();
                rest = remaining;
            }
            [first, ..] => {
                return Err(Error::Other(format!(
                    "unexpected byte 0x{first:02x} in JSON path"
                )))
            }
            [] => return Err(Error::Other("unexpected end of JSON path".into())),
        }
    }
}

/// Decode a JSON scalar value (ascending).
fn decode_scalar_ascending(buf: &[u8]) -> Result<(&[u8], JsonScalarValue)> {
    let typ = peek_type(buf);
    match typ {
        EncodedType::Null => {
            let (rest, _) = decode_if_null(buf);
            Ok((rest, JsonScalarValue::Null))
        }
        EncodedType::EmptyArray => {
            let (rest, _) = decode_if_empty_array(buf);
            Ok((rest, JsonScalarValue::EmptyArray))
        }
        EncodedType::Bool => {
            let (rest, v) = decode_bool_ascending(buf)?;
            Ok((rest, JsonScalarValue::Bool(v)))
        }
        EncodedType::Float64 => {
            let (rest, v) = decode_float64_ascending(buf)?;
            Ok((rest, JsonScalarValue::Number(v)))
        }
        EncodedType::Int => {
            // JSON numbers are encoded as float64, but handle int just in case
            let (rest, v) = decode_uvarint_ascending(buf)?;
            Ok((rest, JsonScalarValue::Number(v as f64)))
        }
        EncodedType::Bytes | EncodedType::BytesDesc => {
            let (rest, v) = decode_bytes_ascending(buf)?;
            let s = String::from_utf8(v)
                .map_err(|e| Error::Other(format!("invalid utf-8 in JSON string: {}", e)))?;
            Ok((rest, JsonScalarValue::String(s)))
        }
        _ => Err(Error::Other(format!(
            "cannot decode JSON scalar: unknown type {:?} (marker 0x{:02x})",
            typ,
            buf.first().unwrap_or(&0)
        ))),
    }
}

/// Decode a JSON scalar value (descending).
fn decode_scalar_descending(buf: &[u8]) -> Result<(&[u8], JsonScalarValue)> {
    match peek_type(buf) {
        EncodedType::Null => {
            let (rest, _) = decode_if_null(buf);
            Ok((rest, JsonScalarValue::Null))
        }
        EncodedType::EmptyArray => {
            let (rest, _) = decode_if_empty_array(buf);
            Ok((rest, JsonScalarValue::EmptyArray))
        }
        EncodedType::Bool => {
            let (rest, v) = decode_bool_descending(buf)?;
            Ok((rest, JsonScalarValue::Bool(v)))
        }
        EncodedType::Float64 => {
            let (rest, v) = decode_float64_descending(buf)?;
            Ok((rest, JsonScalarValue::Number(v)))
        }
        EncodedType::BytesDesc => {
            let (rest, v) = decode_bytes_descending(buf)?;
            let s = String::from_utf8(v)
                .map_err(|e| Error::Other(format!("invalid utf-8 in JSON string: {}", e)))?;
            Ok((rest, JsonScalarValue::String(s)))
        }
        _ => Err(Error::Other(format!(
            "cannot decode descending JSON scalar: marker 0x{:02x}",
            buf.first().unwrap_or(&0)
        ))),
    }
}

#[cfg(test)]
#[path = "json_array_path_tests.rs"]
mod json_array_path_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_json_simple_number() {
        let leaf = JsonLeafValue::new(JsonPath::new(), JsonScalarValue::Number(168.0));

        let encoded = encode_json_ascending(vec![], &leaf);

        // Should start with JSON_MARKER
        assert_eq!(encoded[0], JSON_MARKER);
        // Should have path terminator after marker (empty path)
        assert_eq!(encoded[1], ESCAPED_TERM);
        assert_eq!(encoded[2], JSON_PATH_END);
        // Value follows
        assert!(encoded.len() > 3);
    }

    #[test]
    fn test_encode_json_with_path() {
        let path = JsonPath::new()
            .append_property("custom")
            .append_property("height");
        let leaf = JsonLeafValue::new(path, JsonScalarValue::Number(168.0));

        let encoded = encode_json_ascending(vec![], &leaf);

        // Should start with JSON_MARKER
        assert_eq!(encoded[0], JSON_MARKER);
        // Path should contain "custom" and "height" somewhere before terminator
        assert!(encoded.len() > 20);
    }

    #[test]
    fn test_encode_json_with_array_path() {
        let path = JsonPath::new().append_property("tags").append_index();
        let leaf = JsonLeafValue::new(path, JsonScalarValue::String("value".to_string()));

        let encoded = encode_json_ascending(vec![], &leaf);

        assert_eq!(encoded[0], JSON_MARKER);
    }

    #[test]
    fn test_encode_json_string() {
        let leaf = JsonLeafValue::new(
            JsonPath::new(),
            JsonScalarValue::String("hello".to_string()),
        );

        let encoded = encode_json_ascending(vec![], &leaf);
        assert_eq!(encoded[0], JSON_MARKER);
    }

    #[test]
    fn test_encode_json_bool() {
        let leaf_true = JsonLeafValue::new(JsonPath::new(), JsonScalarValue::Bool(true));
        let leaf_false = JsonLeafValue::new(JsonPath::new(), JsonScalarValue::Bool(false));

        let encoded_true = encode_json_ascending(vec![], &leaf_true);
        let encoded_false = encode_json_ascending(vec![], &leaf_false);

        // false < true in ascending order
        assert!(encoded_false < encoded_true);
    }

    #[test]
    fn test_encode_json_null() {
        let leaf = JsonLeafValue::new(JsonPath::new(), JsonScalarValue::Null);

        let encoded = encode_json_ascending(vec![], &leaf);
        assert_eq!(encoded[0], JSON_MARKER);
    }

    #[test]
    fn test_encode_json_sort_order_by_path() {
        // Same value, different paths - should sort by path
        let leaf_a = JsonLeafValue::new(
            JsonPath::new().append_property("aaa"),
            JsonScalarValue::Number(1.0),
        );
        let leaf_b = JsonLeafValue::new(
            JsonPath::new().append_property("bbb"),
            JsonScalarValue::Number(1.0),
        );

        let encoded_a = encode_json_ascending(vec![], &leaf_a);
        let encoded_b = encode_json_ascending(vec![], &leaf_b);

        assert!(encoded_a < encoded_b);
    }

    #[test]
    fn test_encode_json_sort_order_by_value() {
        // Same path, different values - should sort by value
        let path = JsonPath::new().append_property("num");
        let leaf_1 = JsonLeafValue::new(path.clone(), JsonScalarValue::Number(1.0));
        let leaf_2 = JsonLeafValue::new(path, JsonScalarValue::Number(2.0));

        let encoded_1 = encode_json_ascending(vec![], &leaf_1);
        let encoded_2 = encode_json_ascending(vec![], &leaf_2);

        assert!(encoded_1 < encoded_2);
    }

    #[test]
    fn test_encode_json_descending_reverses_order() {
        let leaf_1 = JsonLeafValue::new(JsonPath::new(), JsonScalarValue::Number(1.0));
        let leaf_2 = JsonLeafValue::new(JsonPath::new(), JsonScalarValue::Number(2.0));

        let asc_1 = encode_json_ascending(vec![], &leaf_1);
        let asc_2 = encode_json_ascending(vec![], &leaf_2);
        let desc_1 = encode_json_descending(vec![], &leaf_1);
        let desc_2 = encode_json_descending(vec![], &leaf_2);

        // Ascending: 1 < 2
        assert!(asc_1 < asc_2);
        // Descending: 1 > 2
        assert!(desc_1 > desc_2);
    }

    #[test]
    fn descending_scalars_round_trip() {
        let path = JsonPath::new().append_property("a").append_property("b");
        for value in [
            JsonScalarValue::Null,
            JsonScalarValue::Bool(false),
            JsonScalarValue::Bool(true),
            JsonScalarValue::Number(-5.5),
            JsonScalarValue::Number(0.0),
            JsonScalarValue::Number(5.0),
            JsonScalarValue::String("é".into()),
        ] {
            for path in [JsonPath::new(), path.clone()] {
                let leaf = JsonLeafValue::new(path, value.clone());
                let encoded = encode_json_descending(vec![], &leaf);
                let (rest, decoded) =
                    decode_json_descending(&encoded).unwrap_or_else(|e| panic!("{leaf:?}: {e}"));
                assert!(rest.is_empty());
                assert_eq!(decoded, leaf);
            }
        }
    }

    fn paths() -> Vec<JsonPath> {
        vec![
            JsonPath::new(),
            JsonPath::new().append_index(),
            JsonPath::new().append_property("w"),
            JsonPath::new().append_property("tags").append_index(),
            JsonPath::new()
                .append_property("a")
                .append_index()
                .append_property("été"),
            JsonPath::new().append_property("\u{0}\u{1}/"),
        ]
    }

    #[test]
    fn array_paths_round_trip_both_ways() {
        for path in paths() {
            for value in [
                JsonScalarValue::Null,
                JsonScalarValue::Bool(true),
                JsonScalarValue::Number(7.0),
                JsonScalarValue::String("w".into()),
                JsonScalarValue::EmptyArray,
            ] {
                let leaf = JsonLeafValue::new(path.clone(), value);
                let asc = encode_json_ascending(vec![], &leaf);
                let desc = encode_json_descending(vec![], &leaf);
                assert_eq!(
                    decode_json_ascending(&asc).unwrap(),
                    (&[][..], leaf.clone())
                );
                assert_eq!(decode_json_descending(&desc).unwrap(), (&[][..], leaf));
            }
        }
    }

    #[test]
    fn descending_keeps_the_path_ascending() {
        for path in paths() {
            let leaf = JsonLeafValue::new(path.clone(), JsonScalarValue::Number(1.0));
            let asc = encode_json_ascending(vec![], &leaf);
            let desc = encode_json_descending(vec![], &leaf);
            let path_len = encode_path(vec![JSON_MARKER], &path).len() + 2;
            assert_eq!(asc[..path_len], desc[..path_len]);
        }
    }

    #[test]
    fn descending_orders_paths_up_and_values_down() {
        let a = JsonPath::new().append_property("a");
        let b = JsonPath::new().append_property("b");
        let key = |path: &JsonPath, n: f64| {
            encode_json_descending(
                vec![],
                &JsonLeafValue::new(path.clone(), JsonScalarValue::Number(n)),
            )
        };
        assert!(key(&a, 1.0) > key(&a, 2.0));
        assert!(key(&a, 1.0) < key(&b, 2.0));
    }

    #[test]
    fn sentinels_bound_every_value_at_a_path_both_ways() {
        let path = JsonPath::new().append_property("tags").append_index();
        let child = path.append_property("x");
        let min = encode_json_ascending(
            vec![],
            &JsonLeafValue::new(path.clone(), JsonScalarValue::PathMin),
        );
        let max = encode_json_ascending(
            vec![],
            &JsonLeafValue::new(path.clone(), JsonScalarValue::PathMax),
        );
        for value in [
            JsonScalarValue::Null,
            JsonScalarValue::Bool(false),
            JsonScalarValue::Number(-1.0),
            JsonScalarValue::String(String::new()),
            JsonScalarValue::EmptyArray,
        ] {
            for encode in [encode_json_ascending, encode_json_descending] {
                let inside = encode(vec![], &JsonLeafValue::new(path.clone(), value.clone()));
                let outside = encode(vec![], &JsonLeafValue::new(child.clone(), value.clone()));
                assert!(min <= inside && inside < max);
                assert!(!(min <= outside && outside < max));
            }
        }
    }
}
