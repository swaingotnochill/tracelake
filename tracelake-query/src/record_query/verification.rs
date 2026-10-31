//! Property-tier checks for the pure record-filter helpers: row semantics and
//! stats-prune soundness.

use super::*;
use proptest::prelude::*;
use serde_json::json;
use tracelake_compact::build_record_stats;

fn schema() -> TableSchema {
    TableSchema::builder("verify")
        .field("sample_id", FieldType::String)
        .field("task", FieldType::String)
        .field("score", FieldType::F64)
        .field("completed_at", FieldType::Timestamp)
        .primary_key("sample_id")
        .build()
        .expect("schema should build")
}

fn records() -> impl Strategy<Value = Vec<Record>> {
    prop::collection::vec(
        (
            0u8..=40,
            prop::option::of(prop_oneof![Just("gsm8k"), Just("math"), Just("arc")]),
            prop::option::of(0.0f64..1.0),
            prop::option::of(0i64..1_000),
        ),
        0..=16,
    )
    .prop_map(|rows| {
        let mut ids: Vec<u8> = Vec::new();
        let mut out = Vec::new();

        for (id, task, score, at) in rows {
            if ids.contains(&id) {
                continue;
            }
            ids.push(id);

            let mut record = Record::new().set("sample_id", json!(format!("sample-{id:03}")));
            if let Some(task) = task {
                record = record.set("task", json!(task));
            }
            if let Some(score) = score {
                record = record.set("score", json!(score));
            }
            if let Some(at) = at {
                record = record.set("completed_at", json!(at));
            }
            out.push(record);
        }

        out
    })
}

fn filter() -> impl Strategy<Value = RecordFilter> {
    prop_oneof![
        prop_oneof![Just("gsm8k"), Just("math"), Just("nope")]
            .prop_map(|task| RecordFilter::eq("task", json!(task))),
        (0.0f64..1.0).prop_map(|score| RecordFilter::eq("score", json!(score))),
        (0.0f64..1.0).prop_map(|score| RecordFilter::ge("score", json!(score))),
        (0.0f64..1.0).prop_map(|score| RecordFilter::lt("score", json!(score))),
        (0i64..1_000).prop_map(|at| RecordFilter::le("completed_at", json!(at))),
        (0i64..1_000).prop_map(|at| RecordFilter::gt("completed_at", json!(at))),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// A segment is pruned only when no record it holds can match the filter.
    #[test]
    fn pruning_a_segment_means_no_record_in_it_can_match(
        records in records(),
        filter in filter(),
    ) {
        let schema = schema();
        let stats = build_record_stats(&schema, &records).expect("stats build");

        if !record_stats_may_match(&stats, &schema, &filter) {
            for record in &records {
                prop_assert!(
                    !record_matches(record, &schema, &filter).expect("filter evaluates"),
                    "pruned segment still held a matching record"
                );
            }
        }
    }

    /// `ge` is exactly `eq || gt`, and `le` is exactly `eq || lt`.
    #[test]
    fn inclusive_bounds_decompose_into_exclusive_ones(
        records in records(),
        bound in 0.0f64..1.0,
    ) {
        let schema = schema();

        for record in &records {
            let ge = record_matches(record, &schema, &RecordFilter::ge("score", json!(bound))).unwrap();
            let eq = record_matches(record, &schema, &RecordFilter::eq("score", json!(bound))).unwrap();
            let gt = record_matches(record, &schema, &RecordFilter::gt("score", json!(bound))).unwrap();
            prop_assert_eq!(ge, eq || gt);

            let le = record_matches(record, &schema, &RecordFilter::le("score", json!(bound))).unwrap();
            let lt = record_matches(record, &schema, &RecordFilter::lt("score", json!(bound))).unwrap();
            prop_assert_eq!(le, eq || lt);
        }
    }

    /// `and` matches only when every branch matches, `or` when any does.
    #[test]
    fn boolean_combinators_follow_their_laws(
        records in records(),
        left in filter(),
        right in filter(),
    ) {
        let schema = schema();
        let and = RecordFilter::and(vec![left.clone(), right.clone()]);
        let or = RecordFilter::Or(vec![left.clone(), right.clone()]);

        for record in &records {
            let left_matches = record_matches(record, &schema, &left).unwrap();
            let right_matches = record_matches(record, &schema, &right).unwrap();

            prop_assert_eq!(
                record_matches(record, &schema, &and).unwrap(),
                left_matches && right_matches
            );
            prop_assert_eq!(
                record_matches(record, &schema, &or).unwrap(),
                left_matches || right_matches
            );
        }
    }

    /// A record holding a task always matches equality on that value, and a
    /// record without the field never matches a comparison on it.
    #[test]
    fn equality_matches_present_values_and_skips_absent_ones(records in records()) {
        let schema = schema();

        for record in &records {
            let Some(value) = record.get("task") else {
                let filter = RecordFilter::eq("task", json!("gsm8k"));
                prop_assert!(!record_matches(record, &schema, &filter).unwrap());
                continue;
            };

            let filter = RecordFilter::eq("task", value.clone());
            prop_assert!(record_matches(record, &schema, &filter).unwrap());
        }
    }
}
