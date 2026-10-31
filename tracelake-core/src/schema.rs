use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    String,
    I64,
    F64,
    Bool,
    Json,
    /// Nanoseconds since the Unix epoch. Packs as i64 so time filters compare
    /// numerically without any calendar conversion.
    Timestamp,
}

impl FieldType {
    /// One-byte tag stored in a record pack. Tags are part of the pack format.
    pub fn tag(self) -> u8 {
        match self {
            FieldType::String => 0,
            FieldType::I64 => 1,
            FieldType::F64 => 2,
            FieldType::Bool => 3,
            FieldType::Json => 4,
            FieldType::Timestamp => 5,
        }
    }

    pub fn from_tag(tag: u8) -> Option<FieldType> {
        match tag {
            0 => Some(FieldType::String),
            1 => Some(FieldType::I64),
            2 => Some(FieldType::F64),
            3 => Some(FieldType::Bool),
            4 => Some(FieldType::Json),
            5 => Some(FieldType::Timestamp),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            FieldType::String => "string",
            FieldType::I64 => "i64",
            FieldType::F64 => "f64",
            FieldType::Bool => "bool",
            FieldType::Json => "json",
            FieldType::Timestamp => "timestamp",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldDef {
    pub name: String,
    pub field_type: FieldType,
    /// False means the field may be absent or null.
    pub required: bool,
    /// Substituted when a record, or an older pack, does not carry the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableSchema {
    pub version: u32,
    pub table: String,
    pub primary_key: String,
    pub fields: Vec<FieldDef>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SchemaError {
    #[error("table name cannot be empty")]
    EmptyTable,

    #[error("table name cannot contain '/'")]
    TableContainsDelimiter,

    #[error("field name cannot be empty")]
    EmptyFieldName,

    #[error(
        "field name '{name}' must use only letters, digits, '_', '.', or '-' and be at most 128 bytes"
    )]
    InvalidFieldName { name: String },

    #[error("field '{name}' is declared more than once")]
    DuplicateField { name: String },

    #[error("primary key is not set")]
    MissingPrimaryKey,

    #[error("primary key '{name}' is not a declared field")]
    PrimaryKeyNotDeclared { name: String },

    #[error("primary key '{name}' must have type string")]
    PrimaryKeyNotString { name: String },

    #[error("schema version {version} is not supported")]
    UnsupportedVersion { version: u32 },
}

/// Why a schema cannot replace the registered one.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum EvolutionError {
    #[error("primary key changed from '{from}' to '{to}'")]
    PrimaryKeyChanged { from: String, to: String },

    #[error("new required field '{name}' has no default, so earlier records cannot satisfy it")]
    RequiredWithoutDefault { name: String },
}

#[derive(Debug, Error, PartialEq)]
pub enum RecordError {
    #[error("record id cannot be empty")]
    EmptyRecordId,

    #[error("record id cannot contain '/'")]
    RecordIdContainsDelimiter,

    #[error("required field '{name}' is missing")]
    MissingRequiredField { name: String },

    #[error("field '{name}' is not declared in the schema")]
    UnknownField { name: String },

    #[error("field '{name}' must be {expected}")]
    TypeMismatch {
        name: String,
        expected: &'static str,
    },
}

#[derive(Debug, Clone)]
pub struct SchemaBuilder {
    table: String,
    primary_key: Option<String>,
    fields: Vec<FieldDef>,
}

impl TableSchema {
    pub fn builder(table: impl Into<String>) -> SchemaBuilder {
        SchemaBuilder {
            table: table.into(),
            primary_key: None,
            fields: Vec::new(),
        }
    }

    pub fn field(&self, name: &str) -> Option<&FieldDef> {
        self.fields.iter().find(|field| field.name == name)
    }

    /// Stable across runs and Rust releases, and stored in every record pack so
    /// a pack written under a different shape is rejected instead of misdecoded.
    pub fn fingerprint(&self) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;

        hash = fnv1a(hash, &self.version.to_le_bytes());
        hash = fnv1a(hash, self.table.as_bytes());
        hash = fnv1a(hash, self.primary_key.as_bytes());

        for field in &self.fields {
            hash = fnv1a(hash, field.name.as_bytes());
            hash = fnv1a(hash, &[field.field_type as u8, u8::from(field.required)]);
        }

        hash
    }

    pub fn validate(&self, record: &Record) -> Result<(), RecordError> {
        self.record_id(record)?;

        for name in record.fields.keys() {
            if self.field(name).is_none() {
                return Err(RecordError::UnknownField { name: name.clone() });
            }
        }

        for field in &self.fields {
            match record.fields.get(&field.name) {
                None | Some(Value::Null) if field.required && field.default.is_none() => {
                    return Err(RecordError::MissingRequiredField {
                        name: field.name.clone(),
                    });
                }
                None | Some(Value::Null) => {}
                Some(value) => {
                    typed_value(field, value)?;
                }
            }
        }

        Ok(())
    }

    /// The primary key value, which is also the record's storage identity.
    pub fn record_id(&self, record: &Record) -> Result<String, RecordError> {
        let value = record
            .fields
            .get(&self.primary_key)
            .and_then(Value::as_str)
            .ok_or_else(|| RecordError::MissingRequiredField {
                name: self.primary_key.clone(),
            })?;

        validate_record_id(value)?;
        Ok(value.to_string())
    }

    pub fn validate_all(&self, records: &[Record]) -> Result<(), RecordError> {
        records.iter().try_for_each(|record| self.validate(record))
    }

    /// Fills declared defaults for fields the record does not hold. Applies to
    /// hot records and to packs written before the field existed.
    pub fn fill_defaults(&self, record: &mut Record) {
        for field in &self.fields {
            let Some(default) = &field.default else {
                continue;
            };
            match record.fields.get(&field.name) {
                None | Some(Value::Null) => {
                    record.fields.insert(field.name.clone(), default.clone());
                }
                Some(_) => {}
            }
        }
    }

    /// Fields present in both shapes with different declared types. The
    /// registry uses this to decide whether stored segments need a rewrite.
    pub fn type_changes(&self, next: &TableSchema) -> Vec<(String, FieldType, FieldType)> {
        self.fields
            .iter()
            .filter_map(|field| {
                let replacement = next.field(&field.name)?;
                (replacement.field_type != field.field_type)
                    .then(|| (field.name.clone(), field.field_type, replacement.field_type))
            })
            .collect()
    }

    /// Coerces every present field to its declared type, converting values
    /// written under an older schema. Returns the first value that cannot be
    /// converted.
    pub fn normalize(&self, record: &mut Record) -> Result<(), RecordError> {
        for field in &self.fields {
            let Some(value) = record.fields.get(&field.name) else {
                continue;
            };
            if value.is_null() || typed_value(field, value).is_ok() {
                continue;
            }

            let Some(converted) = coerce_value(value, field.field_type) else {
                return Err(RecordError::TypeMismatch {
                    name: field.name.clone(),
                    expected: field.field_type.as_str(),
                });
            };
            record.fields.insert(field.name.clone(), converted);
        }

        Ok(())
    }

    pub fn same_shape(&self, other: &TableSchema) -> bool {
        self.table == other.table
            && self.primary_key == other.primary_key
            && self.fields == other.fields
    }

    /// Whether `next` may replace `self`. Adding an optional field is safe.
    /// Adding a required field is safe only with a default that older records
    /// read as. Dropping a field hides its values but leaves them in older
    /// packs. A type change is permitted and must be migrated by the registry.
    pub fn evolve(&self, next: &TableSchema) -> Result<(), EvolutionError> {
        if self.primary_key != next.primary_key {
            return Err(EvolutionError::PrimaryKeyChanged {
                from: self.primary_key.clone(),
                to: next.primary_key.clone(),
            });
        }

        for field in &next.fields {
            let added = self.field(&field.name).is_none();
            if added && field.required && field.default.is_none() {
                return Err(EvolutionError::RequiredWithoutDefault {
                    name: field.name.clone(),
                });
            }
        }

        Ok(())
    }

    pub fn validate_shape(&self) -> Result<(), SchemaError> {
        if self.version == 0 {
            return Err(SchemaError::UnsupportedVersion {
                version: self.version,
            });
        }

        validate_table(&self.table)?;

        let mut seen: Vec<&str> = Vec::with_capacity(self.fields.len());
        for field in &self.fields {
            if field.name.is_empty() {
                return Err(SchemaError::EmptyFieldName);
            }
            if field.name.len() > MAX_FIELD_NAME_BYTES
                || !field.name.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-')
                })
            {
                return Err(SchemaError::InvalidFieldName {
                    name: field.name.clone(),
                });
            }
            if seen.contains(&field.name.as_str()) {
                return Err(SchemaError::DuplicateField {
                    name: field.name.clone(),
                });
            }
            seen.push(&field.name);
        }

        let key_field = self
            .fields
            .iter()
            .find(|field| field.name == self.primary_key)
            .ok_or_else(|| SchemaError::PrimaryKeyNotDeclared {
                name: self.primary_key.clone(),
            })?;

        if key_field.field_type != FieldType::String {
            return Err(SchemaError::PrimaryKeyNotString {
                name: self.primary_key.clone(),
            });
        }

        Ok(())
    }
}

impl SchemaBuilder {
    pub fn field(mut self, name: impl Into<String>, field_type: FieldType) -> Self {
        self.push(name.into(), field_type, false);
        self
    }

    pub fn required_field(mut self, name: impl Into<String>, field_type: FieldType) -> Self {
        self.push(name.into(), field_type, true);
        self
    }

    /// A required field that older records and packs may omit, in which case
    /// `default` stands in for it. This is the only way to add a required field
    /// to a table that already holds data.
    pub fn required_field_with_default(
        mut self,
        name: impl Into<String>,
        field_type: FieldType,
        default: Value,
    ) -> Self {
        self.push(name.into(), field_type, true);
        if let Some(field) = self.fields.last_mut() {
            field.default = Some(default);
        }
        self
    }

    pub fn primary_key(mut self, name: impl Into<String>) -> Self {
        self.primary_key = Some(name.into());
        self
    }

    pub fn build(self) -> Result<TableSchema, SchemaError> {
        let schema = TableSchema {
            version: SCHEMA_VERSION,
            table: self.table,
            primary_key: self.primary_key.ok_or(SchemaError::MissingPrimaryKey)?,
            fields: self.fields,
        };

        schema.validate_shape()?;
        Ok(schema)
    }

    /// The single rule set for a schema's shape, applied by [`SchemaBuilder::build`]
    /// on construction and again when a schema is loaded from the object store.
    fn push(&mut self, name: String, field_type: FieldType, required: bool) {
        self.fields.push(FieldDef {
            name,
            field_type,
            required,
            default: None,
        });
    }
}

pub const SCHEMA_VERSION: u32 = 1;

/// Longest accepted field name. Names become Parquet column names and appear
/// in aggregation SQL, so the character set is restricted as well.
pub const MAX_FIELD_NAME_BYTES: usize = 128;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub fields: Map<String, Value>,
}

impl Record {
    pub fn new() -> Self {
        Self { fields: Map::new() }
    }

    pub fn set(mut self, name: impl Into<String>, value: Value) -> Self {
        self.fields.insert(name.into(), value);
        self
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.fields.get(name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FieldValue<'a> {
    Str(&'a str),
    I64(i64),
    F64(f64),
    Bool(bool),
    Json(&'a Value),
    Timestamp(i64),
}

pub fn typed_value<'a>(field: &FieldDef, value: &'a Value) -> Result<FieldValue<'a>, RecordError> {
    let mismatch = || RecordError::TypeMismatch {
        name: field.name.clone(),
        expected: field.field_type.as_str(),
    };

    match field.field_type {
        FieldType::String => value.as_str().map(FieldValue::Str).ok_or_else(mismatch),
        FieldType::I64 => value.as_i64().map(FieldValue::I64).ok_or_else(mismatch),
        FieldType::F64 => value.as_f64().map(FieldValue::F64).ok_or_else(mismatch),
        FieldType::Bool => value.as_bool().map(FieldValue::Bool).ok_or_else(mismatch),
        FieldType::Json => match value {
            Value::Null => Err(mismatch()),
            other => Ok(FieldValue::Json(other)),
        },
        FieldType::Timestamp => value
            .as_i64()
            .map(FieldValue::Timestamp)
            .ok_or_else(mismatch),
    }
}

/// Converts one JSON value to a declared type. Numbers, booleans, and strings
/// convert when the value is unambiguous. `json` accepts any value and any
/// value converts to `json`. Returns `None` when the value cannot be converted
/// without guessing.
pub fn coerce_value(value: &Value, target: FieldType) -> Option<Value> {
    let field = FieldDef {
        name: String::new(),
        field_type: target,
        required: false,
        default: None,
    };
    if typed_value(&field, value).is_ok() {
        return Some(value.clone());
    }

    match target {
        FieldType::String => match value {
            Value::Number(number) => Some(Value::String(number.to_string())),
            Value::Bool(flag) => Some(Value::String(flag.to_string())),
            _ => None,
        },
        FieldType::I64 | FieldType::Timestamp => match value {
            Value::String(text) => text.parse::<i64>().ok().map(Value::from),
            Value::Number(number) => number
                .as_i64()
                .or_else(|| {
                    number
                        .as_f64()
                        .filter(|value| value.is_finite() && value.fract() == 0.0)
                        .and_then(|value| {
                            (value >= i64::MIN as f64 && value <= i64::MAX as f64)
                                .then_some(value as i64)
                        })
                })
                .map(Value::from),
            Value::Bool(flag) => Some(Value::from(i64::from(*flag))),
            _ => None,
        },
        FieldType::F64 => match value {
            Value::String(text) => text
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite())
                .map(Value::from),
            Value::Number(number) => number.as_f64().map(Value::from),
            Value::Bool(flag) => Some(Value::from(if *flag { 1.0 } else { 0.0 })),
            _ => None,
        },
        FieldType::Bool => match value {
            Value::String(text) => match text.as_str() {
                "true" => Some(Value::Bool(true)),
                "false" => Some(Value::Bool(false)),
                _ => None,
            },
            Value::Number(number) => match number.as_i64() {
                Some(1) => Some(Value::Bool(true)),
                Some(0) => Some(Value::Bool(false)),
                _ => None,
            },
            _ => None,
        },
        FieldType::Json => Some(value.clone()),
    }
}

impl<'a> FieldValue<'a> {
    /// Numeric types order numerically, strings by bytes, and booleans and JSON
    /// return None because only equality is meaningful for them.
    pub fn compare(&self, other: &FieldValue<'_>) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (FieldValue::Str(a), FieldValue::Str(b)) => Some(a.cmp(b)),
            (FieldValue::I64(a), FieldValue::I64(b)) => Some(a.cmp(b)),
            (FieldValue::Timestamp(a), FieldValue::Timestamp(b)) => Some(a.cmp(b)),
            (FieldValue::F64(a), FieldValue::F64(b)) => a.partial_cmp(b),
            _ => None,
        }
    }

    pub fn equals(&self, other: &FieldValue<'_>) -> bool {
        match (self, other) {
            (FieldValue::Str(a), FieldValue::Str(b)) => a == b,
            (FieldValue::I64(a), FieldValue::I64(b)) => a == b,
            (FieldValue::Timestamp(a), FieldValue::Timestamp(b)) => a == b,
            (FieldValue::F64(a), FieldValue::F64(b)) => a == b,
            (FieldValue::Bool(a), FieldValue::Bool(b)) => a == b,
            (FieldValue::Json(a), FieldValue::Json(b)) => a == b,
            _ => false,
        }
    }
}

fn validate_table(table: &str) -> Result<(), SchemaError> {
    if table.is_empty() {
        return Err(SchemaError::EmptyTable);
    }
    if table.contains('/') {
        return Err(SchemaError::TableContainsDelimiter);
    }

    Ok(())
}

fn validate_record_id(record_id: &str) -> Result<(), RecordError> {
    if record_id.is_empty() {
        return Err(RecordError::EmptyRecordId);
    }
    if record_id.contains('/') {
        return Err(RecordError::RecordIdContainsDelimiter);
    }

    Ok(())
}

fn fnv1a(hash: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(hash, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn eval_schema() -> TableSchema {
        TableSchema::builder("inspect_ai_samples")
            .field("sample_id", FieldType::String)
            .required_field("task", FieldType::String)
            .field("score", FieldType::F64)
            .field("completed_at", FieldType::Timestamp)
            .primary_key("sample_id")
            .build()
            .expect("schema should build")
    }

    fn sample_record() -> Record {
        Record::new()
            .set("sample_id", json!("sample-1"))
            .set("task", json!("gsm8k"))
            .set("score", json!(0.75))
            .set("completed_at", json!(1_782_000_000_000_000_000i64))
    }

    #[test]
    fn schema_round_trips_through_json() {
        let schema = eval_schema();

        let encoded = serde_json::to_string(&schema).expect("schema should encode");
        let decoded: TableSchema = serde_json::from_str(&encoded).expect("schema should decode");

        assert_eq!(decoded, schema);
        assert_eq!(decoded.fingerprint(), schema.fingerprint());
    }

    #[test]
    fn fingerprint_changes_with_shape_and_is_stable_for_one_shape() {
        let base = eval_schema();
        let same = eval_schema();
        let reordered = TableSchema::builder("inspect_ai_samples")
            .field("score", FieldType::F64)
            .required_field("task", FieldType::String)
            .field("completed_at", FieldType::Timestamp)
            .field("sample_id", FieldType::String)
            .primary_key("sample_id")
            .build()
            .unwrap();
        let retyped = TableSchema::builder("inspect_ai_samples")
            .field("sample_id", FieldType::String)
            .required_field("task", FieldType::String)
            .field("score", FieldType::I64)
            .field("completed_at", FieldType::Timestamp)
            .primary_key("sample_id")
            .build()
            .unwrap();

        assert_eq!(base.fingerprint(), same.fingerprint());
        assert_ne!(base.fingerprint(), reordered.fingerprint());
        assert_ne!(base.fingerprint(), retyped.fingerprint());
    }

    #[test]
    fn build_rejects_missing_primary_key() {
        let error = TableSchema::builder("t")
            .field("id", FieldType::String)
            .build()
            .expect_err("primary key is required");

        assert_eq!(error, SchemaError::MissingPrimaryKey);
    }

    #[test]
    fn build_rejects_undeclared_or_non_string_primary_key() {
        let undeclared = TableSchema::builder("t")
            .field("id", FieldType::String)
            .primary_key("missing")
            .build()
            .expect_err("primary key must be declared");
        assert_eq!(
            undeclared,
            SchemaError::PrimaryKeyNotDeclared {
                name: "missing".to_string()
            }
        );

        let numeric = TableSchema::builder("t")
            .field("id", FieldType::I64)
            .primary_key("id")
            .build()
            .expect_err("primary key must be a string");
        assert_eq!(
            numeric,
            SchemaError::PrimaryKeyNotString {
                name: "id".to_string()
            }
        );
    }

    #[test]
    fn build_rejects_duplicate_and_empty_names() {
        let duplicate = TableSchema::builder("t")
            .field("id", FieldType::String)
            .field("id", FieldType::I64)
            .primary_key("id")
            .build()
            .expect_err("duplicate fields are invalid");
        assert_eq!(
            duplicate,
            SchemaError::DuplicateField {
                name: "id".to_string()
            }
        );

        let empty = TableSchema::builder("t")
            .field("", FieldType::String)
            .primary_key("id")
            .build()
            .expect_err("empty field names are invalid");
        assert_eq!(empty, SchemaError::EmptyFieldName);

        let quoted = TableSchema::builder("t")
            .field("score\"; DROP TABLE x", FieldType::String)
            .primary_key("id")
            .build()
            .expect_err("quoted field names are invalid");
        assert_eq!(
            quoted,
            SchemaError::InvalidFieldName {
                name: "score\"; DROP TABLE x".to_string()
            }
        );

        let empty_table = TableSchema::builder("")
            .field("id", FieldType::String)
            .primary_key("id")
            .build()
            .expect_err("empty table names are invalid");
        assert_eq!(empty_table, SchemaError::EmptyTable);
    }

    #[test]
    fn valid_record_passes_and_optional_null_is_allowed() {
        let schema = eval_schema();

        schema.validate(&sample_record()).expect("record is valid");

        let mut nulled = sample_record();
        nulled.fields.insert("score".to_string(), Value::Null);
        schema.validate(&nulled).expect("optional null is allowed");
    }

    #[test]
    fn validation_reports_missing_required_unknown_and_mistyped_fields() {
        let schema = eval_schema();

        let mut missing = sample_record();
        missing.fields.remove("task");
        assert_eq!(
            schema.validate(&missing),
            Err(RecordError::MissingRequiredField {
                name: "task".to_string()
            })
        );

        let unknown = sample_record().set("nope", json!(1));
        assert_eq!(
            schema.validate(&unknown),
            Err(RecordError::UnknownField {
                name: "nope".to_string()
            })
        );

        let mistyped = sample_record().set("score", json!("high"));
        assert_eq!(
            schema.validate(&mistyped),
            Err(RecordError::TypeMismatch {
                name: "score".to_string(),
                expected: "f64"
            })
        );

        let empty_id = sample_record().set("sample_id", json!(""));
        assert_eq!(schema.validate(&empty_id), Err(RecordError::EmptyRecordId));

        let slash_id = sample_record().set("sample_id", json!("a/b"));
        assert_eq!(
            schema.validate(&slash_id),
            Err(RecordError::RecordIdContainsDelimiter)
        );

        let absent_id = Record::new().set("task", json!("gsm8k"));
        assert_eq!(
            schema.validate(&absent_id),
            Err(RecordError::MissingRequiredField {
                name: "sample_id".to_string()
            })
        );
    }

    #[test]
    fn every_field_type_narrows_and_compares() {
        let cases = [
            (FieldType::String, json!("b"), json!("a")),
            (FieldType::I64, json!(10), json!(2)),
            (FieldType::F64, json!(2.5), json!(2.25)),
            (FieldType::Timestamp, json!(20), json!(10)),
        ];

        for (field_type, high, low) in cases {
            let field = FieldDef {
                name: "f".to_string(),
                field_type,
                required: false,
                default: None,
            };
            let high = typed_value(&field, &high).expect("high value types");
            let low = typed_value(&field, &low).expect("low value types");

            assert_eq!(
                high.compare(&low),
                Some(std::cmp::Ordering::Greater),
                "{field_type:?}"
            );
            assert!(!high.equals(&low), "{field_type:?}");
            assert!(high.equals(&high), "{field_type:?}");
        }

        let bool_field = FieldDef {
            name: "f".to_string(),
            field_type: FieldType::Bool,
            required: false,
            default: None,
        };
        let yes = typed_value(&bool_field, &json!(true)).unwrap();
        let no = typed_value(&bool_field, &json!(false)).unwrap();
        assert!(yes.equals(&yes));
        assert!(!yes.equals(&no));
        assert_eq!(yes.compare(&no), None);

        let json_field = FieldDef {
            name: "f".to_string(),
            field_type: FieldType::Json,
            required: false,
            default: None,
        };
        let first = json!({ "a": [1, 2] });
        let second = json!({ "a": [1, 2] });
        let object = typed_value(&json_field, &first).unwrap();
        assert!(object.equals(&typed_value(&json_field, &second).unwrap()));
        assert_eq!(object.compare(&object), None);
    }

    #[test]
    fn coercions_cover_the_supported_matrix() {
        assert_eq!(
            coerce_value(&json!("0.5"), FieldType::F64),
            Some(json!(0.5))
        );
        assert_eq!(coerce_value(&json!("7"), FieldType::I64), Some(json!(7)));
        assert_eq!(
            coerce_value(&json!("true"), FieldType::Bool),
            Some(json!(true))
        );
        assert_eq!(coerce_value(&json!(2.0), FieldType::I64), Some(json!(2)));
        assert_eq!(coerce_value(&json!(2.5), FieldType::I64), None);
        assert_eq!(coerce_value(&json!(1), FieldType::Bool), Some(json!(true)));
        assert_eq!(coerce_value(&json!(0), FieldType::Bool), Some(json!(false)));
        assert_eq!(coerce_value(&json!(3), FieldType::String), Some(json!("3")));
        assert_eq!(
            coerce_value(&json!(true), FieldType::String),
            Some(json!("true"))
        );
        assert_eq!(coerce_value(&json!("nope"), FieldType::I64), None);
        assert_eq!(coerce_value(&json!({"a": 1}), FieldType::String), None);
        assert_eq!(
            coerce_value(&json!({"a": 1}), FieldType::Json),
            Some(json!({"a": 1}))
        );
    }

    #[test]
    fn normalize_converts_values_written_under_the_old_schema() {
        let schema = TableSchema::builder("t")
            .field("id", FieldType::String)
            .field("score", FieldType::F64)
            .field("count", FieldType::I64)
            .primary_key("id")
            .build()
            .unwrap();

        let mut record = Record::new()
            .set("id", json!("r"))
            .set("score", json!("0.75"))
            .set("count", json!("3"));
        schema.normalize(&mut record).expect("values convert");

        assert_eq!(record.get("score"), Some(&json!(0.75)));
        assert_eq!(record.get("count"), Some(&json!(3)));
        schema
            .validate(&record)
            .expect("normalized record validates");

        let mut broken = Record::new()
            .set("id", json!("r"))
            .set("score", json!("high"));
        assert!(matches!(
            schema.normalize(&mut broken),
            Err(RecordError::TypeMismatch { .. })
        ));
    }

    #[test]
    fn i64_rejects_fractional_and_f64_accepts_integers() {
        let schema = TableSchema::builder("t")
            .field("id", FieldType::String)
            .field("count", FieldType::I64)
            .field("score", FieldType::F64)
            .primary_key("id")
            .build()
            .unwrap();

        let fractional = Record::new()
            .set("id", json!("r"))
            .set("count", json!(1.5))
            .set("score", json!(1));

        assert!(schema.validate(&fractional).is_err());

        let integral = Record::new()
            .set("id", json!("r"))
            .set("count", json!(1))
            .set("score", json!(1));
        schema.validate(&integral).expect("integer f64 accepted");
    }
}

#[cfg(kani)]
mod verify {
    use super::*;

    /// Every tag the pack format writes maps back to the type that wrote it,
    /// and every byte either maps to a type or is rejected.
    #[kani::proof]
    fn field_type_tags_round_trip() {
        let tag: u8 = kani::any();

        if let Some(field_type) = FieldType::from_tag(tag) {
            assert_eq!(field_type.tag(), tag);
        }
    }
}
