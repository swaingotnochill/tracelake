use proptest::prelude::*;
use serde_json::{Value, json};
use tracelake_core::{
    EvolutionError, FieldDef, FieldType, Record, RecordError, SchemaError, TableSchema,
};

const NAME_ALPHABET: &[char] = &['a', 'b', 'c', '0', '1'];

fn name() -> impl Strategy<Value = String> {
    prop::collection::vec(proptest::sample::select(NAME_ALPHABET), 1..=6)
        .prop_map(|chars| chars.into_iter().collect())
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

fn value_for(field_type: FieldType) -> Value {
    match field_type {
        FieldType::String => json!("text"),
        FieldType::I64 => json!(42),
        FieldType::F64 => json!(0.5),
        FieldType::Bool => json!(true),
        FieldType::Timestamp => json!(1_790_856_000_000_000_000i64),
        FieldType::Json => json!({ "nested": [1, 2, 3] }),
    }
}

fn wrong_value(field_type: FieldType) -> Option<Value> {
    match field_type {
        FieldType::String => Some(json!(42)),
        FieldType::I64 => Some(json!("not an integer")),
        FieldType::F64 => Some(json!([])),
        FieldType::Bool => Some(json!(0)),
        FieldType::Timestamp => Some(json!({ "at": 1 })),
        // Any non-null JSON value is a valid json field.
        FieldType::Json => None,
    }
}

fn fields() -> impl Strategy<Value = Vec<FieldDef>> {
    prop::collection::vec((name(), field_type(), any::<bool>()), 1..=6).prop_map(|declared| {
        let mut seen: Vec<String> = Vec::new();
        let mut out = Vec::new();

        for (name, field_type, required) in declared {
            if name == "id" || seen.contains(&name) {
                continue;
            }
            seen.push(name.clone());
            out.push(FieldDef {
                name,
                field_type,
                required,
                default: None,
            });
        }

        out
    })
}

fn schema_from(declared: &[FieldDef]) -> TableSchema {
    let mut builder = TableSchema::builder("prop").field("id", FieldType::String);
    for field in declared {
        builder = match (&field.default, field.required) {
            (Some(default), _) => builder.required_field_with_default(
                field.name.clone(),
                field.field_type,
                default.clone(),
            ),
            (None, true) => builder.required_field(field.name.clone(), field.field_type),
            (None, false) => builder.field(field.name.clone(), field.field_type),
        };
    }

    builder
        .primary_key("id")
        .build()
        .expect("generated schema builds")
}

fn schema_and_fields() -> impl Strategy<Value = (TableSchema, Vec<FieldDef>)> {
    fields().prop_map(|declared| (schema_from(&declared), declared))
}

fn schema_and_record() -> impl Strategy<Value = (TableSchema, Vec<FieldDef>, Record)> {
    schema_and_fields().prop_map(|(schema, declared)| {
        let mut record = Record::new().set("id", json!("record-1"));
        for field in &declared {
            record = record.set(field.name.clone(), value_for(field.field_type));
        }

        (schema, declared, record)
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn a_record_built_from_its_schema_always_validates(
        (schema, _declared, record) in schema_and_record()
    ) {
        prop_assert_eq!(schema.validate(&record), Ok(()));
        prop_assert_eq!(schema.record_id(&record), Ok("record-1".to_string()));
    }

    #[test]
    fn dropping_a_required_field_is_reported(
        (schema, declared, record) in schema_and_record()
    ) {
        let Some(required) = declared.iter().find(|field| field.required) else {
            return Ok(());
        };

        let mut broken = record.clone();
        broken.fields.remove(&required.name);
        prop_assert_eq!(
            schema.validate(&broken),
            Err(RecordError::MissingRequiredField {
                name: required.name.clone()
            })
        );
    }

    #[test]
    fn an_undeclared_field_is_reported(
        (schema, _declared, record) in schema_and_record()
    ) {
        let broken = record.set("undeclared", json!(1));
        prop_assert_eq!(
            schema.validate(&broken),
            Err(RecordError::UnknownField {
                name: "undeclared".to_string()
            })
        );
    }

    #[test]
    fn a_value_of_the_wrong_type_is_reported(
        (schema, declared, record) in schema_and_record()
    ) {
        let Some((target, wrong)) = declared
            .iter()
            .find_map(|field| wrong_value(field.field_type).map(|wrong| (field, wrong)))
        else {
            return Ok(());
        };

        let mut broken = record.clone();
        broken.fields.insert(target.name.clone(), wrong.clone());
        prop_assert!(schema.validate(&broken).is_err());
    }

    #[test]
    fn identical_shapes_share_a_fingerprint_and_a_changed_shape_does_not(
        declared in fields(),
        extra_required in any::<bool>(),
    ) {
        let base = schema_from(&declared);
        prop_assert_eq!(base.fingerprint(), schema_from(&declared).fingerprint());

        let mut extended = declared.clone();
        extended.push(FieldDef {
            name: "extra".to_string(),
            field_type: FieldType::String,
            required: extra_required,
            default: None,
        });
        prop_assert_ne!(base.fingerprint(), schema_from(&extended).fingerprint());
    }

    #[test]
    fn a_loaded_shape_without_its_primary_key_is_rejected(declared in fields()) {
        let schema = TableSchema {
            version: tracelake_core::SCHEMA_VERSION,
            table: "prop".to_string(),
            primary_key: "missing".to_string(),
            fields: declared,
        };

        prop_assert_eq!(
            schema.validate_shape(),
            Err(SchemaError::PrimaryKeyNotDeclared {
                name: "missing".to_string()
            })
        );
    }

    #[test]
    fn adding_an_optional_field_always_evolves(declared in fields()) {
        let base = schema_from(&declared);
        let mut extended = declared.clone();
        extended.push(FieldDef {
            name: "added".to_string(),
            field_type: FieldType::String,
            required: false,
            default: None,
        });

        prop_assert_eq!(base.evolve(&schema_from(&extended)), Ok(()));
        prop_assert!(!base.same_shape(&schema_from(&extended)));
        prop_assert!(base.same_shape(&schema_from(&declared)));
    }

    #[test]
    fn a_type_change_is_reported_for_migration(declared in fields()) {
        let Some(target) = declared
            .iter()
            .find(|field| field.name != "id")
            .cloned()
        else {
            return Ok(());
        };

        let base = schema_from(&declared);
        let mut changed = declared.clone();
        for field in &mut changed {
            if field.name == target.name {
                field.field_type = if target.field_type == FieldType::Json {
                    FieldType::String
                } else {
                    FieldType::Json
                };
            }
        }
        let next = schema_from(&changed);

        let changes = base.type_changes(&next);
        prop_assert_eq!(changes.len(), 1);
        prop_assert_eq!(changes[0].0.as_str(), target.name.as_str());
        prop_assert_eq!(base.evolve(&next), Ok(()));
    }

    #[test]
    fn adding_a_required_field_evolves_only_with_a_default(declared in fields()) {
        let base = schema_from(&declared);
        let mut without_default = declared.clone();
        without_default.push(FieldDef {
            name: "required_added".to_string(),
            field_type: FieldType::String,
            required: true,
            default: None,
        });
        prop_assert_eq!(
            base.evolve(&schema_from(&without_default)),
            Err(EvolutionError::RequiredWithoutDefault {
                name: "required_added".to_string()
            })
        );

        let mut with_default = declared.clone();
        with_default.push(FieldDef {
            name: "required_added".to_string(),
            field_type: FieldType::String,
            required: true,
            default: Some(json!("fallback")),
        });
        prop_assert_eq!(base.evolve(&schema_from(&with_default)), Ok(()));
    }

    #[test]
    fn fill_defaults_fills_absent_and_null_values_only(declared in fields()) {
        let mut schema = schema_from(&declared);
        schema.fields.push(FieldDef {
            name: "with_default".to_string(),
            field_type: FieldType::String,
            required: true,
            default: Some(json!("fallback")),
        });

        let mut record = Record::new().set("id", json!("record-1"));
        schema.fill_defaults(&mut record);
        prop_assert_eq!(record.get("with_default"), Some(&json!("fallback")));

        let mut explicit = Record::new()
            .set("id", json!("record-2"))
            .set("with_default", json!("kept"));
        schema.fill_defaults(&mut explicit);
        prop_assert_eq!(explicit.get("with_default"), Some(&json!("kept")));

        let mut nulled = Record::new()
            .set("id", json!("record-3"))
            .set("with_default", serde_json::Value::Null);
        schema.fill_defaults(&mut nulled);
        prop_assert_eq!(nulled.get("with_default"), Some(&json!("fallback")));
    }
}
