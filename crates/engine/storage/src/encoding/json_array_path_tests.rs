use document::{JsonLeafValue, JsonPath, JsonScalarValue};

use super::{
    decode_json_ascending, decode_json_descending, encode_json_ascending, encode_json_descending,
};

type Encode = fn(Vec<u8>, &JsonLeafValue) -> Vec<u8>;
type Decode = fn(&[u8]) -> crate::corekv::Result<(&[u8], JsonLeafValue)>;

const BOTH_WAYS: [(Encode, Decode); 2] = [
    (encode_json_ascending, decode_json_ascending),
    (encode_json_descending, decode_json_descending),
];

fn round_trip(leaf: JsonLeafValue) {
    for (encode, decode) in BOTH_WAYS {
        let encoded = encode(Vec::new(), &leaf);
        let (rest, decoded) = decode(&encoded).unwrap_or_else(|e| panic!("{leaf:?}: {e}"));
        assert!(rest.is_empty());
        assert_eq!(decoded, leaf);
    }
}

#[test]
fn an_array_path_round_trips() {
    round_trip(JsonLeafValue::new(
        JsonPath::new().append_property("custom").append_index(),
        JsonScalarValue::Number(2.0),
    ));
}

#[test]
fn an_array_step_never_equals_a_property_step() {
    let array = JsonPath::new().append_property("custom").append_index();
    for name in ["customw", "custom\u{0}", "custom\u{88}", "\u{88}", ""] {
        let property = JsonPath::new().append_property(name);
        let nested = JsonPath::new()
            .append_property("custom")
            .append_property(name);
        for (encode, _) in BOTH_WAYS {
            let key = |path: &JsonPath| {
                encode(
                    Vec::new(),
                    &JsonLeafValue::new(path.clone(), JsonScalarValue::Number(2.0)),
                )
            };
            assert_ne!(key(&array), key(&property), "{name:?}");
            assert_ne!(key(&array), key(&nested), "{name:?}");
        }
    }
}

#[test]
fn sentinels_bracket_every_value_both_ways() {
    let path = JsonPath::new()
        .append_property("custom")
        .append_property("height");
    let at = |value| JsonLeafValue::new(path.clone(), value);
    for (encode, _) in BOTH_WAYS {
        let min = encode(Vec::new(), &at(JsonScalarValue::PathMin));
        let max = encode(Vec::new(), &at(JsonScalarValue::PathMax));
        for value in [
            JsonScalarValue::Null,
            JsonScalarValue::Bool(true),
            JsonScalarValue::Number(168.0),
            JsonScalarValue::String("z".into()),
            JsonScalarValue::EmptyArray,
        ] {
            let key = encode(Vec::new(), &at(value));
            assert!(min <= key && key < max);
        }
    }
}

#[test]
fn sentinels_round_trip_descending() {
    let path = JsonPath::new()
        .append_property("custom")
        .append_property("height");
    for value in [JsonScalarValue::PathMin, JsonScalarValue::PathMax] {
        let leaf = JsonLeafValue::new(path.clone(), value);
        let encoded = encode_json_descending(Vec::new(), &leaf);
        assert_eq!(decode_json_descending(&encoded).unwrap(), (&[][..], leaf));
    }
}

#[test]
fn path_max_round_trips_ascending() {
    let leaf = JsonLeafValue::new(
        JsonPath::new().append_property("a"),
        JsonScalarValue::PathMax,
    );
    let encoded = encode_json_ascending(Vec::new(), &leaf);
    assert_eq!(decode_json_ascending(&encoded).unwrap(), (&[][..], leaf));
}

#[test]
fn json_null_round_trips_as_null() {
    round_trip(JsonLeafValue::new(
        JsonPath::new()
            .append_property("custom")
            .append_property("value"),
        JsonScalarValue::Null,
    ));
}

#[test]
fn a_unicode_property_then_an_array_step_round_trips() {
    round_trip(JsonLeafValue::new(
        JsonPath::new()
            .append_property("éclair")
            .append_index()
            .append_property("雪"),
        JsonScalarValue::Number(2.0),
    ));
}

#[test]
fn a_property_that_looks_like_the_path_end_round_trips() {
    round_trip(JsonLeafValue::new(
        JsonPath::new().append_property("\u{1}/x"),
        JsonScalarValue::String("v".into()),
    ));
}
