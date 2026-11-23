//! A descending JSON index keeps its paths ascending, so an array step never
//! reads as a property and `_any` on an array finds its elements.

use integration_test::actions::{AddCollection, AddDoc, Request};
use integration_test::test_case::TestCase;
use serde_json::{json, Value};

fn case(filter: &str, names: &[&str]) {
    let mut actions = vec![AddCollection {
        sdl: "type Users { name: String  custom: JSON @index(direction: DESC) }".into(),
        ..Default::default()
    }
    .into()];
    actions.extend(
        [
            json!({ "name": "Array", "custom": [1, 3] }),
            json!({ "name": "Other", "custom": [5] }),
            json!({ "name": "Scalar", "custom": 3 }),
            json!({ "name": "W", "custom": { "w": [3, "w"] } }),
            json!({ "name": "Deep", "custom": { "a": { "b": [4] } } }),
        ]
        .into_iter()
        .map(|doc| {
            AddDoc {
                collection: "Users".into(),
                doc_map: Some(doc),
                ..Default::default()
            }
            .into()
        }),
    );
    let names: Vec<Value> = names.iter().map(|name| json!({ "name": name })).collect();
    actions.push(
        Request {
            request: format!(
                "query {{ Users(filter: {{custom: {filter}}}, order: {{name: ASC}}) {{ name }} }}"
            )
            .into(),
            expected_data: json!({ "Users": names }),
            ..Default::default()
        }
        .into(),
    );
    TestCase::from_actions(actions).execute();
}

#[test]
#[mantra_macros::req_test("defra.index.json_descending_path")]
fn any_on_a_top_level_array() {
    case("{_any: {_eq: 3}}", &["Array"]);
}

#[test]
#[mantra_macros::req_test("defra.index.json_descending_path")]
fn any_range_on_a_top_level_array() {
    case("{_any: {_gt: 2}}", &["Array", "Other"]);
}

#[test]
#[mantra_macros::req_test("defra.index.json_descending_path")]
fn any_on_an_array_under_a_property() {
    case("{w: {_any: {_eq: \"w\"}}}", &["W"]);
}

#[test]
#[mantra_macros::req_test("defra.index.json_descending_path")]
fn any_on_an_array_under_a_nested_path() {
    case("{a: {b: {_any: {_eq: 4}}}}", &["Deep"]);
}

#[test]
#[mantra_macros::req_test("defra.index.json_descending_path")]
fn a_scan_of_every_entry_reads_each_path() {
    case("{_ne: 3}", &["Array", "Deep", "Other", "W"]);
}
