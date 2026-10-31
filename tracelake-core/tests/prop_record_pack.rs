use proptest::prelude::*;
use serde_json::{Map, Value, json};
use tracelake_core::{
    FieldDef, FieldType, Record, RecordPackError, TableSchema, decode_record_pack,
    encode_record_pack,
};

const FIELD_ALPHABET: &[char] = &['a', 'b', 'c', '0', '1', '-'];
const VALUE_ALPHABET: &[char] = &['x', 'y', 'z', ' ', '0', '9'];

fn small_string(alphabet: &'static [char]) -> impl Strategy<Value = String> {
    prop::collection::vec(proptest::sample::select(alphabet), 1..=10)
        .prop_map(|chars| chars.into_iter().collect())
}

fn field_name() -> impl Strategy<Value = String> {
    small_string(FIELD_ALPHABET).prop_map(|name| format!("f{name}"))
}

fn field_type() -> impl Strategy<Value = FieldType> {
    prop_oneof![
        Just(FieldType::String),
        Just(FieldType::I64),
        Just(FieldType::F64),
        Just(FieldType::Bool),
        Just(FieldType::Timestamp),
        Just(FieldType::Json),
    ]
}

fn field_value(field_type: FieldType) -> BoxedStrategy<Value> {
    match field_type {
        FieldType::String => small_string(VALUE_ALPHABET).prop_map(Value::from).boxed(),
        FieldType::I64 => any::<i64>().prop_map(Value::from).boxed(),
        FieldType::F64 => (-1_000_000.0f64..1_000_000.0).prop_map(Value::from).boxed(),
        FieldType::Bool => any::<bool>().prop_map(Value::from).boxed(),
        FieldType::Timestamp => (0i64..1_800_000_000_000_000_000)
            .prop_map(Value::from)
            .boxed(),
        FieldType::Json => {
            prop::collection::vec((small_string(FIELD_ALPHABET), any::<i64>()), 0..=3)
                .prop_map(|pairs| {
                    let map: Map<String, Value> = pairs
                        .into_iter()
                        .map(|(k, v)| (k, Value::from(v)))
                        .collect();
                    Value::Object(map)
                })
                .boxed()
        }
    }
}

fn schema_strategy() -> impl Strategy<Value = TableSchema> {
    prop::collection::vec((field_name(), field_type()), 1..=6).prop_map(|declared| {
        let mut builder = TableSchema::builder("prop_table").field("id", FieldType::String);
        let mut seen: Vec<String> = Vec::new();

        for (name, field_type) in declared {
            if name == "id" || seen.contains(&name) {
                continue;
            }
            seen.push(name.clone());
            builder = builder.field(name, field_type);
        }

        builder
            .field("required_text", FieldType::String)
            .primary_key("id")
            .build()
            .expect("generated schemas are valid")
    })
}

fn record_batch(table: TableSchema) -> impl Strategy<Value = Vec<Record>> {
    let field_defs: Vec<(String, FieldType)> = table
        .fields
        .iter()
        .filter(|field| field.name != "id" && field.name != "required_text")
        .map(|field| (field.name.clone(), field.field_type))
        .collect();
    let optional_fields = field_defs.len();

    prop::collection::vec(
        (
            0usize..40,
            prop::collection::vec(any::<bool>(), optional_fields),
            field_defs
                .iter()
                .map(|(_, field_type)| field_value(*field_type))
                .collect::<Vec<_>>(),
        ),
        0..=8,
    )
    .prop_map(move |rows| {
        let mut ids: Vec<usize> = Vec::new();
        let mut out = Vec::new();

        for (offset, presence, values) in rows {
            let id = offset + ids.len() * 1000;
            if ids.contains(&id) {
                continue;
            }
            ids.push(id);

            let mut record = Record::new()
                .set("id", json!(format!("record-{id:06}")))
                .set("required_text", json!("always present"));

            for (index, (name, _)) in field_defs.iter().enumerate() {
                if presence[index] {
                    record = record.set(name.clone(), values[index].clone());
                }
            }

            out.push(record);
        }

        out
    })
}

fn schema_and_records() -> impl Strategy<Value = (TableSchema, Vec<Record>)> {
    schema_strategy().prop_flat_map(|table| {
        record_batch(table.clone()).prop_map(move |records| (table.clone(), records))
    })
}

fn fixed_schema() -> TableSchema {
    TableSchema::builder("fixed")
        .field("id", FieldType::String)
        .field("score", FieldType::F64)
        .field("task", FieldType::String)
        .primary_key("id")
        .build()
        .expect("schema should build")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn packs_round_trip_every_record_exactly((table, records) in schema_and_records()) {
        let bytes = encode_record_pack(&table, &records).expect("valid records encode");
        let pack = decode_record_pack(&bytes).expect("encoded packs decode");

        prop_assert_eq!(pack.record_count(), records.len());

        let mut expected_ids: Vec<String> = records
            .iter()
            .map(|record| table.record_id(record).expect("id derives"))
            .collect();
        expected_ids.sort();
        prop_assert_eq!(pack.record_ids().expect("ids read"), expected_ids);

        for record in &records {
            let record_id = table.record_id(record).expect("id derives");
            let decoded = pack
                .read_record(&table, &record_id)
                .expect("lookup succeeds")
                .expect("record is present");
            prop_assert_eq!(&decoded, record, "record {}", record_id);
        }
    }

    #[test]
    fn projection_returns_exactly_the_requested_fields(
        (table, records) in schema_and_records(),
        picks in prop::collection::vec(any::<bool>(), 1..=8),
    ) {
        let bytes = encode_record_pack(&table, &records).expect("valid records encode");
        let pack = decode_record_pack(&bytes).expect("encoded packs decode");

        let requested: Vec<&str> = table
            .fields
            .iter()
            .zip(picks.iter())
            .filter(|(_, picked)| **picked)
            .map(|(field, _)| field.name.as_str())
            .collect();

        for record in &records {
            let record_id = table.record_id(record).expect("id derives");
            let decoded = pack
                .read_record_with(&table, &record_id, &requested)
                .expect("lookup succeeds")
                .expect("record is present");

            let expected: Map<String, Value> = record
                .fields
                .iter()
                .filter(|(name, _)| requested.contains(&name.as_str()))
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();

            prop_assert_eq!(decoded.fields, expected, "record {}", record_id);
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic_the_reader(bytes in prop::collection::vec(any::<u8>(), 0..=256)) {
        let schema = fixed_schema();

        if let Ok(pack) = decode_record_pack(&bytes) {
            let _ = pack.record_ids();
            let _ = pack.read_record(&schema, "record-000000");
            let _ = pack.scan(&schema, &["score"]);
        }
    }

    #[test]
    fn an_older_pack_reads_under_a_newer_schema_that_adds_a_field(
        (table, records) in schema_and_records()
    ) {
        let bytes = encode_record_pack(&table, &records).expect("valid records encode");
        let pack = decode_record_pack(&bytes).expect("encoded packs decode");

        let mut extended = table.clone();
        extended.fields.push(FieldDef {
            name: "added".to_string(),
            field_type: FieldType::String,
            required: false,
            default: None,
        });
        table.evolve(&extended).expect("adding an optional field evolves");

        for record in &records {
            let record_id = table.record_id(record).expect("id derives");
            let decoded = pack
                .read_record(&extended, &record_id)
                .expect("lookup succeeds")
                .expect("record is present");
            prop_assert!(decoded.get("added").is_none());
        }
    }

    #[test]
    fn a_type_change_errors_only_when_the_changed_field_is_requested(
        (table, records) in schema_and_records()
    ) {
        let Some(target) = table
            .fields
            .iter()
            .find(|field| field.field_type != FieldType::Json && field.name != table.primary_key)
            .cloned()
        else {
            return Ok(());
        };

        let bytes = encode_record_pack(&table, &records).expect("valid records encode");
        let pack = decode_record_pack(&bytes).expect("encoded packs decode");

        let mut retyped = table.clone();
        for field in &mut retyped.fields {
            if field.name == target.name {
                field.field_type = if target.field_type == FieldType::I64 {
                    FieldType::String
                } else {
                    FieldType::I64
                };
            }
        }

        for record in &records {
            if record.get(&target.name).is_none() {
                continue;
            }
            let record_id = table.record_id(record).expect("id derives");
            let mismatch = pack.read_record_with(&retyped, &record_id, &[&target.name]);
            let is_mismatch = matches!(mismatch, Err(RecordPackError::FieldTypeMismatch { .. }));
            prop_assert!(is_mismatch);
        }
    }
}
