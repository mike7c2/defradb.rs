//! Range conditions on two paths of one JSON index each keep their own path.

use integration_test::actions::{Actions, AddCollection, AddDoc, Request};
use integration_test::test_case::TestCase;
use serde_json::{json, Value};

fn run(stored: &[Value], filter: &str, names: &[&str]) {
    let mut actions: Vec<Actions> = vec![AddCollection {
        sdl: "type User { name: String  custom: JSON @index }".into(),
        ..Default::default()
    }
    .into()];
    actions.extend(stored.iter().map(|doc| {
        Actions::from(AddDoc {
            collection: "User".into(),
            doc_map: Some(doc.clone()),
            ..Default::default()
        })
    }));
    let names: Vec<Value> = names.iter().map(|name| json!({ "name": name })).collect();
    actions.push(
        Request {
            request: format!("query {{ User(filter: {filter}, order: {{name: ASC}}) {{ name }} }}")
                .into(),
            expected_data: json!({ "User": names }),
            ..Default::default()
        }
        .into(),
    );
    TestCase::from_actions(actions).execute();
}

#[test]
#[mantra_macros::req_test("defra.index.json_range_paths")]
fn ranges_on_two_paths() {
    run(
        &[
            json!({ "name": "Both", "custom": { "a": 30, "b": 5 } }),
            json!({ "name": "OnlyA", "custom": { "a": 30, "b": 25 } }),
            json!({ "name": "OnlyB", "custom": { "a": 1, "b": 5 } }),
        ],
        "{custom: {a: {_gt: 10}, b: {_lt: 20}}}",
        &["Both"],
    );
}

#[test]
#[mantra_macros::req_test("defra.index.json_range_paths")]
fn all_ranges_on_two_paths() {
    run(
        &[
            json!({ "name": "Both", "custom": { "a": [30], "b": [5] } }),
            json!({ "name": "OnlyA", "custom": { "a": [30], "b": [25] } }),
            json!({ "name": "OnlyB", "custom": { "a": [1], "b": [5] } }),
        ],
        "{custom: {a: {_all: {_gt: 10}}, b: {_all: {_lt: 20}}}}",
        &["Both"],
    );
}

#[test]
#[mantra_macros::req_test("defra.index.json_range_paths")]
fn a_range_and_ne_on_two_paths() {
    run(
        &[
            json!({ "name": "Both", "custom": { "a": 30, "b": 5 } }),
            json!({ "name": "Equal", "custom": { "a": 30, "b": 25 } }),
            json!({ "name": "Low", "custom": { "a": 1, "b": 5 } }),
        ],
        "{custom: {a: {_gt: 10}, b: {_ne: 25}}}",
        &["Both"],
    );
}

#[test]
#[mantra_macros::req_test("defra.index.json_range_paths")]
fn two_bounds_on_one_path_still_narrow() {
    run(
        &[
            json!({ "name": "In", "custom": { "a": 15 } }),
            json!({ "name": "Low", "custom": { "a": 5 } }),
            json!({ "name": "High", "custom": { "a": 25 } }),
        ],
        "{custom: {a: {_gt: 10, _lt: 20}}}",
        &["In"],
    );
}

#[test]
#[mantra_macros::req_test("defra.index.json_range_paths")]
fn like_and_ne_on_two_paths() {
    run(
        &[
            json!({ "name": "Both", "custom": { "a": "xa", "b": 5 } }),
            json!({ "name": "Equal", "custom": { "a": "xa", "b": 25 } }),
            json!({ "name": "Unlike", "custom": { "a": "ya", "b": 5 } }),
        ],
        "{custom: {a: {_like: \"x%\"}, b: {_ne: 25}}}",
        &["Both"],
    );
}
