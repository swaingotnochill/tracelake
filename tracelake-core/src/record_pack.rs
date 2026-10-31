use crate::crc::checksum;
use crate::schema::{FieldDef, FieldType, FieldValue, Record, TableSchema, typed_value};
use serde_json::Value;
use std::cmp::Ordering;
use thiserror::Error;

const MAGIC: [u8; 8] = *b"TRCREC01";
const HEADER_LEN: usize = 8 + 8 + 4 + 8;
const FOOTER_LEN: usize = 4;

#[derive(Debug, Error)]
pub enum RecordPackError {
    #[error("not a record pack: bad magic")]
    BadMagic,

    #[error("record pack truncated: {len} bytes")]
    Truncated { len: usize },

    #[error("record pack checksum mismatch: expected {expected:#x}, found {found:#x}")]
    ChecksumMismatch { expected: u32, found: u32 },

    #[error("record pack was written under schema fingerprint {found}, not {expected}")]
    SchemaMismatch { expected: u64, found: u64 },

    #[error("duplicate record_id: {record_id}")]
    DuplicateRecordId { record_id: String },

    #[error("record id is not valid utf-8")]
    InvalidRecordId,

    #[error("record id is {len} bytes, longer than the u16 limit")]
    RecordIdTooLong { len: usize },

    #[error("directory offset {offset} with {count} entries exceeds pack length {total}")]
    DirectoryOutOfBounds {
        offset: u64,
        count: u64,
        total: usize,
    },

    #[error("record block offset {offset} length {len} exceeds pack length {total}")]
    BlockOutOfBounds { offset: u64, len: u64, total: usize },

    #[error("record block has {extra} trailing bytes")]
    TrailingBytes { extra: usize },

    #[error("record does not match the schema: {0}")]
    Record(#[from] crate::schema::RecordError),

    #[error("record value decode failed: {0}")]
    Json(#[from] serde_json::Error),
}

/// Layout: `[header][record blocks][directory offsets][directory entries][crc32]`.
/// A block is a presence bitmap over the schema fields followed by the present
/// values in schema order, so a reader walks bytes without parsing JSON for
/// fields it did not ask for.
pub fn encode_record_pack(
    schema: &TableSchema,
    records: &[Record],
) -> Result<Vec<u8>, RecordPackError> {
    schema.validate_all(records)?;

    let mut sorted: Vec<(String, &Record)> = records
        .iter()
        .map(|record| schema.record_id(record).map(|id| (id, record)))
        .collect::<Result<_, _>>()?;
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    for pair in sorted.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(RecordPackError::DuplicateRecordId {
                record_id: pair[0].0.to_string(),
            });
        }
    }

    let mut blocks: Vec<u8> = Vec::new();
    let mut entries: Vec<(&str, u64, u64)> = Vec::with_capacity(sorted.len());

    for (record_id, record) in &sorted {
        let block = encode_record_block(schema, record)?;
        let offset = (HEADER_LEN + blocks.len()) as u64;
        blocks.extend_from_slice(&block);
        entries.push((record_id.as_str(), offset, block.len() as u64));
    }

    let entry_offsets_pos = (HEADER_LEN + blocks.len()) as u64;
    let entries_pos = entry_offsets_pos + 8 * entries.len() as u64;

    let mut entry_bytes: Vec<u8> = Vec::new();
    let mut entry_offsets: Vec<u64> = Vec::with_capacity(entries.len());

    for (record_id, block_offset, block_len) in &entries {
        let id_len =
            u16::try_from(record_id.len()).map_err(|_| RecordPackError::RecordIdTooLong {
                len: record_id.len(),
            })?;
        entry_offsets.push(entries_pos + entry_bytes.len() as u64);
        entry_bytes.extend_from_slice(&id_len.to_le_bytes());
        entry_bytes.extend_from_slice(record_id.as_bytes());
        entry_bytes.extend_from_slice(&block_offset.to_le_bytes());
        entry_bytes.extend_from_slice(&block_len.to_le_bytes());
    }

    let mut pack =
        Vec::with_capacity(HEADER_LEN + blocks.len() + entry_bytes.len() + entries.len() * 8);
    pack.extend_from_slice(&MAGIC);
    pack.extend_from_slice(&schema.fingerprint().to_le_bytes());
    pack.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    pack.extend_from_slice(&entry_offsets_pos.to_le_bytes());
    pack.extend_from_slice(&blocks);
    for offset in entry_offsets {
        pack.extend_from_slice(&offset.to_le_bytes());
    }
    pack.extend_from_slice(&entry_bytes);

    let checksum = checksum(&pack);
    pack.extend_from_slice(&checksum.to_le_bytes());

    Ok(pack)
}

#[derive(Debug, Clone, Copy)]
pub struct RecordPack<'a> {
    bytes: &'a [u8],
    schema_fingerprint: u64,
    record_count: usize,
    entry_offsets_pos: usize,
}

pub fn decode_record_pack(bytes: &[u8]) -> Result<RecordPack<'_>, RecordPackError> {
    if bytes.len() < HEADER_LEN + FOOTER_LEN {
        return Err(RecordPackError::Truncated { len: bytes.len() });
    }

    if bytes[..MAGIC.len()] != MAGIC {
        return Err(RecordPackError::BadMagic);
    }

    let checksum_pos = bytes.len() - FOOTER_LEN;
    let expected = read_u32(bytes, checksum_pos)?;
    let found = checksum(&bytes[..checksum_pos]);
    if expected != found {
        return Err(RecordPackError::ChecksumMismatch { expected, found });
    }

    let schema_fingerprint = read_u64(bytes, MAGIC.len())?;
    let record_count = read_u32(bytes, MAGIC.len() + 8)? as usize;
    let entry_offsets_pos = read_u64(bytes, MAGIC.len() + 12)? as usize;

    let directory_end = entry_offsets_pos.checked_add(8 * record_count).ok_or(
        RecordPackError::DirectoryOutOfBounds {
            offset: entry_offsets_pos as u64,
            count: record_count as u64,
            total: bytes.len(),
        },
    )?;
    if directory_end > checksum_pos || entry_offsets_pos < HEADER_LEN {
        return Err(RecordPackError::DirectoryOutOfBounds {
            offset: entry_offsets_pos as u64,
            count: record_count as u64,
            total: bytes.len(),
        });
    }

    Ok(RecordPack {
        bytes,
        schema_fingerprint,
        record_count,
        entry_offsets_pos,
    })
}

impl RecordPack<'_> {
    pub fn record_count(&self) -> usize {
        self.record_count
    }

    pub fn schema_fingerprint(&self) -> u64 {
        self.schema_fingerprint
    }

    pub fn record_ids(&self) -> Result<Vec<String>, RecordPackError> {
        let mut ids = Vec::with_capacity(self.record_count);
        for index in 0..self.record_count {
            ids.push(self.entry_at(index)?.record_id.to_string());
        }
        Ok(ids)
    }

    pub fn read_record(
        &self,
        schema: &TableSchema,
        record_id: &str,
    ) -> Result<Option<Record>, RecordPackError> {
        self.read_record_with(schema, record_id, &all_field_names(schema))
    }

    pub fn read_record_with(
        &self,
        schema: &TableSchema,
        record_id: &str,
        fields: &[&str],
    ) -> Result<Option<Record>, RecordPackError> {
        self.ensure_schema(schema)?;

        let mut low = 0;
        let mut high = self.record_count;

        while low < high {
            let mid = low + (high - low) / 2;
            let entry = self.entry_at(mid)?;

            match entry.record_id.as_bytes().cmp(record_id.as_bytes()) {
                Ordering::Equal => return self.read_block(&entry, schema, fields).map(Some),
                Ordering::Less => low = mid + 1,
                Ordering::Greater => high = mid,
            }
        }

        Ok(None)
    }

    pub fn scan(
        &self,
        schema: &TableSchema,
        fields: &[&str],
    ) -> Result<Vec<Record>, RecordPackError> {
        self.ensure_schema(schema)?;

        let mut records = Vec::with_capacity(self.record_count);
        for index in 0..self.record_count {
            let entry = self.entry_at(index)?;
            records.push(self.read_block(&entry, schema, fields)?);
        }

        Ok(records)
    }

    fn ensure_schema(&self, schema: &TableSchema) -> Result<(), RecordPackError> {
        let expected = schema.fingerprint();
        if self.schema_fingerprint != expected {
            return Err(RecordPackError::SchemaMismatch {
                expected,
                found: self.schema_fingerprint,
            });
        }

        Ok(())
    }

    fn entry_at(&self, index: usize) -> Result<DirectoryEntry<'_>, RecordPackError> {
        let offset_pos = self.entry_offsets_pos + index * 8;
        let entry_pos = read_u64(self.bytes, offset_pos)? as usize;
        read_entry(self.bytes, entry_pos)
    }

    fn read_block(
        &self,
        entry: &DirectoryEntry<'_>,
        schema: &TableSchema,
        fields: &[&str],
    ) -> Result<Record, RecordPackError> {
        let start = entry.block_offset as usize;
        let end = start.checked_add(entry.block_len as usize).ok_or(
            RecordPackError::BlockOutOfBounds {
                offset: entry.block_offset,
                len: entry.block_len,
                total: self.bytes.len(),
            },
        )?;
        let block = self
            .bytes
            .get(start..end)
            .ok_or(RecordPackError::BlockOutOfBounds {
                offset: entry.block_offset,
                len: entry.block_len,
                total: self.bytes.len(),
            })?;

        decode_record_block(block, schema, fields)
    }
}

#[derive(Debug)]
struct DirectoryEntry<'a> {
    record_id: &'a str,
    block_offset: u64,
    block_len: u64,
}

fn encode_record_block(schema: &TableSchema, record: &Record) -> Result<Vec<u8>, RecordPackError> {
    let bitmap_len = schema.fields.len().div_ceil(8);
    let mut block = vec![0u8; bitmap_len];
    let mut values: Vec<u8> = Vec::new();

    for (index, field) in schema.fields.iter().enumerate() {
        let Some(value) = record.fields.get(&field.name) else {
            continue;
        };
        if value.is_null() {
            continue;
        }

        block[index / 8] |= 1 << (index % 8);
        encode_value(&mut values, field, value)?;
    }

    block.extend_from_slice(&values);
    Ok(block)
}

fn decode_record_block(
    block: &[u8],
    schema: &TableSchema,
    fields: &[&str],
) -> Result<Record, RecordPackError> {
    let bitmap_len = schema.fields.len().div_ceil(8);
    let bitmap = block
        .get(..bitmap_len)
        .ok_or(RecordPackError::Truncated { len: block.len() })?;

    let mut record = Record::new();
    let mut pos = bitmap_len;

    for (index, field) in schema.fields.iter().enumerate() {
        if bitmap[index / 8] & (1 << (index % 8)) == 0 {
            continue;
        }

        let wanted = fields.contains(&field.name.as_str());
        let value = decode_value(block, &mut pos, field, wanted)?;
        if let Some(value) = value {
            record.fields.insert(field.name.clone(), value);
        }
    }

    if pos != block.len() {
        return Err(RecordPackError::TrailingBytes {
            extra: block.len() - pos,
        });
    }

    Ok(record)
}

fn encode_value(out: &mut Vec<u8>, field: &FieldDef, value: &Value) -> Result<(), RecordPackError> {
    match typed_value(field, value)? {
        FieldValue::Str(text) => write_bytes(out, text.as_bytes()),
        FieldValue::I64(number) | FieldValue::Timestamp(number) => {
            out.extend_from_slice(&number.to_le_bytes())
        }
        FieldValue::F64(number) => out.extend_from_slice(&number.to_bits().to_le_bytes()),
        FieldValue::Bool(flag) => out.push(u8::from(flag)),
        FieldValue::Json(value) => write_bytes(out, &serde_json::to_vec(value)?),
    }

    Ok(())
}

fn write_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn decode_value(
    block: &[u8],
    pos: &mut usize,
    field: &FieldDef,
    wanted: bool,
) -> Result<Option<Value>, RecordPackError> {
    match field.field_type {
        FieldType::String => {
            let bytes = read_bytes(block, pos)?;
            if !wanted {
                return Ok(None);
            }
            let text = std::str::from_utf8(bytes).map_err(|_| RecordPackError::InvalidRecordId)?;
            Ok(Some(Value::String(text.to_string())))
        }
        FieldType::I64 | FieldType::Timestamp => {
            read_i64_value(block, pos).map(|number| wanted.then(|| Value::from(number)))
        }
        FieldType::F64 => {
            let raw = read_i64_value(block, pos)?;
            if !wanted {
                return Ok(None);
            }
            Ok(Some(Value::from(f64::from_bits(raw as u64))))
        }
        FieldType::Bool => {
            let flag = read_u8(block, pos)?;
            Ok(wanted.then_some(Value::Bool(flag != 0)))
        }
        FieldType::Json => {
            let bytes = read_bytes(block, pos)?;
            if !wanted {
                return Ok(None);
            }
            Ok(Some(serde_json::from_slice(bytes)?))
        }
    }
}

fn read_i64_value(block: &[u8], pos: &mut usize) -> Result<i64, RecordPackError> {
    let start = *pos;
    let raw = read_array::<8>(block, start)?;
    *pos = start + 8;
    Ok(i64::from_le_bytes(raw))
}

fn read_bytes<'a>(block: &'a [u8], pos: &mut usize) -> Result<&'a [u8], RecordPackError> {
    let start = *pos;
    let len = read_u32(block, start)? as usize;
    let payload_start = start + 4;
    let end = payload_start
        .checked_add(len)
        .ok_or(RecordPackError::Truncated { len: block.len() })?;
    let bytes = block
        .get(payload_start..end)
        .ok_or(RecordPackError::Truncated { len: block.len() })?;
    *pos = end;
    Ok(bytes)
}

fn read_u8(block: &[u8], pos: &mut usize) -> Result<u8, RecordPackError> {
    let byte = *block
        .get(*pos)
        .ok_or(RecordPackError::Truncated { len: block.len() })?;
    *pos += 1;
    Ok(byte)
}

fn read_u16(bytes: &[u8], pos: usize) -> Result<u16, RecordPackError> {
    read_array::<2>(bytes, pos).map(u16::from_le_bytes)
}

fn read_u32(bytes: &[u8], pos: usize) -> Result<u32, RecordPackError> {
    read_array::<4>(bytes, pos).map(u32::from_le_bytes)
}

fn read_u64(bytes: &[u8], pos: usize) -> Result<u64, RecordPackError> {
    read_array::<8>(bytes, pos).map(u64::from_le_bytes)
}

fn read_array<const N: usize>(bytes: &[u8], pos: usize) -> Result<[u8; N], RecordPackError> {
    let end = pos
        .checked_add(N)
        .ok_or(RecordPackError::Truncated { len: bytes.len() })?;
    let slice = bytes
        .get(pos..end)
        .ok_or(RecordPackError::Truncated { len: bytes.len() })?;

    let mut raw = [0u8; N];
    raw.copy_from_slice(slice);
    Ok(raw)
}

fn read_entry(bytes: &[u8], pos: usize) -> Result<DirectoryEntry<'_>, RecordPackError> {
    let id_len = read_u16(bytes, pos)? as usize;
    let id_start = pos + 2;
    let id_end = id_start
        .checked_add(id_len)
        .ok_or(RecordPackError::Truncated { len: bytes.len() })?;
    let id_bytes = bytes
        .get(id_start..id_end)
        .ok_or(RecordPackError::Truncated { len: bytes.len() })?;
    let record_id = std::str::from_utf8(id_bytes).map_err(|_| RecordPackError::InvalidRecordId)?;

    Ok(DirectoryEntry {
        record_id,
        block_offset: read_u64(bytes, id_end)?,
        block_len: read_u64(bytes, id_end + 8)?,
    })
}

fn all_field_names(schema: &TableSchema) -> Vec<&str> {
    schema
        .fields
        .iter()
        .map(|field| field.name.as_str())
        .collect()
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
            .field("epoch", FieldType::I64)
            .field("completed_at", FieldType::Timestamp)
            .field("passed", FieldType::Bool)
            .field("input", FieldType::Json)
            .primary_key("sample_id")
            .build()
            .expect("schema should build")
    }

    fn sample_record(id: &str, score: f64) -> Record {
        Record::new()
            .set("sample_id", json!(id))
            .set("task", json!("gsm8k"))
            .set("score", json!(score))
            .set("epoch", json!(1))
            .set("completed_at", json!(1_782_000_000_000_000_000i64))
            .set("passed", json!(score >= 0.5))
            .set("input", json!({ "question": format!("question for {id}") }))
    }

    #[test]
    fn empty_pack_round_trips() {
        let schema = eval_schema();
        let bytes = encode_record_pack(&schema, &[]).expect("empty pack should encode");
        let pack = decode_record_pack(&bytes).expect("empty pack should decode");

        assert_eq!(pack.record_count(), 0);
        assert_eq!(pack.schema_fingerprint(), schema.fingerprint());
        assert!(pack.record_ids().unwrap().is_empty());
        assert_eq!(pack.read_record(&schema, "absent").unwrap(), None);
        assert!(pack.scan(&schema, &["score"]).unwrap().is_empty());
    }

    #[test]
    fn every_declared_type_round_trips_exactly() {
        let schema = eval_schema();
        let record = sample_record("sample-1", 0.125);

        let bytes =
            encode_record_pack(&schema, std::slice::from_ref(&record)).expect("pack should encode");
        let pack = decode_record_pack(&bytes).expect("pack should decode");

        assert_eq!(pack.read_record(&schema, "sample-1").unwrap(), Some(record));
    }

    #[test]
    fn float_and_timestamp_fields_survive_bit_for_bit() {
        let schema = eval_schema();
        let score = 172.0f64 * 0.001;
        let nanos = 1_782_000_000_123_456_789i64;
        let mut record = sample_record("sample-1", score);
        record
            .fields
            .insert("completed_at".to_string(), json!(nanos));

        let bytes = encode_record_pack(&schema, std::slice::from_ref(&record)).unwrap();
        let pack = decode_record_pack(&bytes).unwrap();
        let decoded = pack.read_record(&schema, "sample-1").unwrap().unwrap();

        assert_eq!(
            decoded
                .get("score")
                .and_then(Value::as_f64)
                .map(f64::to_bits),
            Some(score.to_bits())
        );
        assert_eq!(
            decoded.get("completed_at").and_then(Value::as_i64),
            Some(nanos)
        );
    }

    #[test]
    fn lookup_hits_directory_edges_and_misses() {
        let schema = eval_schema();
        let records = vec![
            sample_record("record-000", 0.1),
            sample_record("record-042", 0.5),
            sample_record("record-999", 0.9),
        ];

        let bytes = encode_record_pack(&schema, &records).expect("pack should encode");
        let pack = decode_record_pack(&bytes).expect("pack should decode");

        assert_eq!(
            pack.record_ids().unwrap(),
            vec!["record-000", "record-042", "record-999"]
        );

        for record in &records {
            let record_id = schema.record_id(record).unwrap();
            assert_eq!(
                pack.read_record(&schema, &record_id).unwrap(),
                Some(record.clone()),
                "record {record_id}"
            );
        }

        assert_eq!(pack.read_record(&schema, "record-0").unwrap(), None);
        assert_eq!(pack.read_record(&schema, "record-9999").unwrap(), None);
    }

    #[test]
    fn projection_materializes_only_requested_fields() {
        let schema = eval_schema();
        let records = vec![sample_record("a", 0.25), sample_record("b", 0.75)];
        let bytes = encode_record_pack(&schema, &records).unwrap();
        let pack = decode_record_pack(&bytes).unwrap();

        let projected = pack
            .read_record_with(&schema, "b", &["score", "task"])
            .unwrap()
            .expect("record should exist");

        assert_eq!(
            projected.fields.keys().cloned().collect::<Vec<_>>(),
            vec!["score".to_string(), "task".to_string()]
        );
        assert_eq!(projected.get("score"), Some(&json!(0.75)));

        let scanned = pack.scan(&schema, &["score"]).unwrap();
        assert_eq!(scanned.len(), 2);
        for record in &scanned {
            assert_eq!(
                record.fields.keys().cloned().collect::<Vec<_>>(),
                vec!["score"]
            );
        }
    }

    #[test]
    fn absent_optional_fields_stay_absent() {
        let schema = eval_schema();
        let record = Record::new()
            .set("sample_id", json!("sparse"))
            .set("task", json!("gsm8k"));

        let bytes = encode_record_pack(&schema, &[record]).unwrap();
        let pack = decode_record_pack(&bytes).unwrap();
        let decoded = pack.read_record(&schema, "sparse").unwrap().unwrap();

        assert_eq!(decoded.fields.len(), 2);
        assert!(decoded.get("score").is_none());
    }

    #[test]
    fn a_pack_refuses_to_decode_under_a_different_schema() {
        let schema = eval_schema();
        let bytes = encode_record_pack(&schema, &[sample_record("a", 0.5)]).unwrap();
        let pack = decode_record_pack(&bytes).unwrap();

        let retyped = TableSchema::builder("inspect_ai_samples")
            .field("sample_id", FieldType::String)
            .required_field("task", FieldType::String)
            .field("score", FieldType::I64)
            .field("epoch", FieldType::I64)
            .field("completed_at", FieldType::Timestamp)
            .field("passed", FieldType::Bool)
            .field("input", FieldType::Json)
            .primary_key("sample_id")
            .build()
            .unwrap();

        assert!(matches!(
            pack.read_record(&retyped, "a"),
            Err(RecordPackError::SchemaMismatch { .. })
        ));
        assert!(matches!(
            pack.scan(&retyped, &["score"]),
            Err(RecordPackError::SchemaMismatch { .. })
        ));
    }

    #[test]
    fn duplicate_ids_and_invalid_records_are_rejected() {
        let schema = eval_schema();
        let duplicate = vec![sample_record("a", 0.1), sample_record("a", 0.2)];

        assert!(matches!(
            encode_record_pack(&schema, &duplicate),
            Err(RecordPackError::DuplicateRecordId { .. })
        ));

        let invalid = Record::new().set("sample_id", json!("b"));
        assert!(matches!(
            encode_record_pack(&schema, &[invalid]),
            Err(RecordPackError::Record(_))
        ));
    }

    #[test]
    fn corrupted_and_truncated_packs_are_rejected() {
        let schema = eval_schema();
        let mut bytes =
            encode_record_pack(&schema, &[sample_record("a", 0.5)]).expect("pack should encode");

        let middle = bytes.len() / 2;
        bytes[middle] ^= 0xff;
        assert!(matches!(
            decode_record_pack(&bytes),
            Err(RecordPackError::ChecksumMismatch { .. })
        ));

        let clean = encode_record_pack(&schema, &[sample_record("a", 0.5)]).unwrap();
        assert!(matches!(
            decode_record_pack(&clean[..clean.len() - 10]),
            Err(RecordPackError::ChecksumMismatch { .. })
        ));
        assert!(matches!(
            decode_record_pack(&clean[..10]),
            Err(RecordPackError::Truncated { .. })
        ));

        let mut magic = clean.clone();
        magic[0] = b'X';
        assert!(matches!(
            decode_record_pack(&magic),
            Err(RecordPackError::BadMagic)
        ));
    }
}

#[cfg(kani)]
mod verify {
    use super::*;

    fn minimal_pack(record_id: &str, payload: &[u8]) -> Vec<u8> {
        let entry_offsets_pos = HEADER_LEN + payload.len();
        let entries_pos = entry_offsets_pos + 8;

        let mut pack = Vec::new();
        pack.extend_from_slice(&MAGIC);
        pack.extend_from_slice(&0u64.to_le_bytes());
        pack.extend_from_slice(&1u32.to_le_bytes());
        pack.extend_from_slice(&(entry_offsets_pos as u64).to_le_bytes());
        pack.extend_from_slice(payload);
        pack.extend_from_slice(&(entries_pos as u64).to_le_bytes());
        pack.extend_from_slice(&(record_id.len() as u16).to_le_bytes());
        pack.extend_from_slice(record_id.as_bytes());
        pack.extend_from_slice(&(HEADER_LEN as u64).to_le_bytes());
        pack.extend_from_slice(&(payload.len() as u64).to_le_bytes());

        let checksum = checksum(&pack);
        pack.extend_from_slice(&checksum.to_le_bytes());
        pack
    }

    /// Any ASCII corruption with the footer checksum restamped either errors or
    /// walks the directory without panicking. Serde and schema decode stay out
    /// of the solver by design.
    #[kani::proof]
    #[kani::unwind(96)]
    fn restamped_corruptions_never_panic_in_decode_or_directory() {
        let mut pack = minimal_pack("a", &[0xde, 0xad, 0xbe, 0xef]);

        let index: usize = kani::any();
        kani::assume(index < pack.len());
        let byte: u8 = kani::any();
        kani::assume(byte.is_ascii());
        pack[index] = byte;

        let body_len = pack.len() - FOOTER_LEN;
        let checksum = checksum(&pack[..body_len]);
        pack[body_len..].copy_from_slice(&checksum.to_le_bytes());

        if let Ok(parsed) = decode_record_pack(&pack) {
            let _ = parsed.record_ids();
        }
    }
}
