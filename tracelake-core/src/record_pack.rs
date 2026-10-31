use crate::crc::checksum;
use crate::schema::{FieldType, FieldValue, Record, TableSchema, typed_value};
use serde_json::Value;
use std::cmp::Ordering;
use thiserror::Error;

const MAGIC: [u8; 8] = *b"TRCREC01";
const FOOTER_LEN: usize = 4;

#[derive(Debug, Error)]
pub enum RecordPackError {
    #[error("not a record pack: bad magic")]
    BadMagic,

    #[error("record pack truncated: {len} bytes")]
    Truncated { len: usize },

    #[error("record pack checksum mismatch: expected {expected:#x}, found {found:#x}")]
    ChecksumMismatch { expected: u32, found: u32 },

    #[error("duplicate record_id: {record_id}")]
    DuplicateRecordId { record_id: String },

    #[error("record id is not valid utf-8")]
    InvalidRecordId,

    #[error("field name is {len} bytes, longer than the u16 limit")]
    FieldNameTooLong { len: usize },

    #[error("record id is {len} bytes, longer than the u16 limit")]
    RecordIdTooLong { len: usize },

    #[error("pack field '{field}' holds {pack} but the schema declares {requested}")]
    FieldTypeMismatch {
        field: String,
        pack: &'static str,
        requested: &'static str,
    },

    #[error("pack uses unknown field type tag {tag}")]
    UnknownFieldType { tag: u8 },

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

/// One field as the pack stored it. The pack describes itself so a segment
/// written under an older schema stays readable after the schema evolves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackField {
    pub name: String,
    pub field_type: FieldType,
}

/// Layout: `[magic][field descriptors][record count][directory offset]
/// [record blocks][directory offsets][directory entries][crc32]`.
///
/// Each block is a presence bitmap over the pack's own fields followed by the
/// present values in that order. Fields the pack does not hold read back as
/// absent, or as the requested schema's default.
pub fn encode_record_pack(
    schema: &TableSchema,
    records: &[Record],
) -> Result<Vec<u8>, RecordPackError> {
    let declared: Vec<Record> = records
        .iter()
        .map(|record| {
            let mut kept = Record::new();
            for (name, value) in &record.fields {
                if schema.field(name).is_some() {
                    kept.fields.insert(name.clone(), value.clone());
                }
            }
            kept
        })
        .collect();
    schema.validate_all(&declared)?;

    let mut sorted: Vec<(String, &Record)> = declared
        .iter()
        .map(|record| schema.record_id(record).map(|id| (id, record)))
        .collect::<Result<_, _>>()?;
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    for pair in sorted.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(RecordPackError::DuplicateRecordId {
                record_id: pair[0].0.clone(),
            });
        }
    }

    let mut header: Vec<u8> = Vec::new();
    header.extend_from_slice(&MAGIC);
    header.extend_from_slice(&(schema.fields.len() as u16).to_le_bytes());
    for field in &schema.fields {
        let name_len =
            u16::try_from(field.name.len()).map_err(|_| RecordPackError::FieldNameTooLong {
                len: field.name.len(),
            })?;
        header.extend_from_slice(&name_len.to_le_bytes());
        header.extend_from_slice(field.name.as_bytes());
        header.push(field.field_type.tag());
    }
    header.extend_from_slice(&(sorted.len() as u32).to_le_bytes());
    header.extend_from_slice(&0u64.to_le_bytes());

    let mut blocks: Vec<u8> = Vec::new();
    let mut entries: Vec<(&str, u64, u64)> = Vec::with_capacity(sorted.len());

    for (record_id, record) in &sorted {
        let block = encode_record_block(schema, record)?;
        let offset = (header.len() + blocks.len()) as u64;
        blocks.extend_from_slice(&block);
        entries.push((record_id, offset, block.len() as u64));
    }

    let entry_offsets_pos = (header.len() + blocks.len()) as u64;
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

    let directory_offset_pos = header.len() - 8;
    header[directory_offset_pos..].copy_from_slice(&entry_offsets_pos.to_le_bytes());

    let mut pack = Vec::with_capacity(header.len() + blocks.len() + entry_bytes.len() + 8);
    pack.extend_from_slice(&header);
    pack.extend_from_slice(&blocks);
    for offset in entry_offsets {
        pack.extend_from_slice(&offset.to_le_bytes());
    }
    pack.extend_from_slice(&entry_bytes);

    let checksum = checksum(&pack);
    pack.extend_from_slice(&checksum.to_le_bytes());

    Ok(pack)
}

#[derive(Debug, Clone)]
pub struct RecordPack<'a> {
    bytes: &'a [u8],
    fields: Vec<PackField>,
    record_count: usize,
    entry_offsets_pos: usize,
}

pub fn decode_record_pack(bytes: &[u8]) -> Result<RecordPack<'_>, RecordPackError> {
    if bytes.len() < MAGIC.len() + 2 + 4 + 8 + FOOTER_LEN {
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

    let mut pos = MAGIC.len();
    let field_count = read_u16(bytes, pos)? as usize;
    pos += 2;
    let fields = read_pack_fields(bytes, &mut pos, field_count)?;

    let record_count = read_u32(bytes, pos)? as usize;
    pos += 4;
    let entry_offsets_pos = read_u64(bytes, pos)? as usize;
    pos += 8;

    let directory_end = entry_offsets_pos.checked_add(8 * record_count).ok_or(
        RecordPackError::DirectoryOutOfBounds {
            offset: entry_offsets_pos as u64,
            count: record_count as u64,
            total: bytes.len(),
        },
    )?;
    if directory_end > checksum_pos || entry_offsets_pos < pos {
        return Err(RecordPackError::DirectoryOutOfBounds {
            offset: entry_offsets_pos as u64,
            count: record_count as u64,
            total: bytes.len(),
        });
    }

    Ok(RecordPack {
        bytes,
        fields,
        record_count,
        entry_offsets_pos,
    })
}

impl RecordPack<'_> {
    pub fn record_count(&self) -> usize {
        self.record_count
    }

    /// The shape the pack was written under.
    pub fn fields(&self) -> &[PackField] {
        &self.fields
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
        let mut records = Vec::with_capacity(self.record_count);
        for index in 0..self.record_count {
            let entry = self.entry_at(index)?;
            records.push(self.read_block(&entry, schema, fields)?);
        }

        Ok(records)
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

        decode_record_block(block, &self.fields, schema, fields)
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
    pack_fields: &[PackField],
    schema: &TableSchema,
    fields: &[&str],
) -> Result<Record, RecordPackError> {
    let bitmap_len = pack_fields.len().div_ceil(8);
    let bitmap = block
        .get(..bitmap_len)
        .ok_or(RecordPackError::Truncated { len: block.len() })?;

    let mut record = Record::new();
    let mut pos = bitmap_len;

    for (index, pack_field) in pack_fields.iter().enumerate() {
        if bitmap[index / 8] & (1 << (index % 8)) == 0 {
            continue;
        }

        let declared = schema.field(&pack_field.name);
        let wanted = fields.contains(&pack_field.name.as_str());

        if let Some(declared) = declared
            && declared.field_type != pack_field.field_type
            && wanted
        {
            return Err(RecordPackError::FieldTypeMismatch {
                field: pack_field.name.clone(),
                pack: pack_field.field_type.as_str(),
                requested: declared.field_type.as_str(),
            });
        }

        let materialize =
            wanted && declared.is_some_and(|def| def.field_type == pack_field.field_type);
        let value = decode_value(block, &mut pos, pack_field.field_type, materialize)?;
        if let Some(value) = value {
            record.fields.insert(pack_field.name.clone(), value);
        }
    }

    if pos != block.len() {
        return Err(RecordPackError::TrailingBytes {
            extra: block.len() - pos,
        });
    }

    for field in &schema.fields {
        if !fields.contains(&field.name.as_str()) || record.fields.contains_key(&field.name) {
            continue;
        }
        if let Some(default) = &field.default {
            record.fields.insert(field.name.clone(), default.clone());
        }
    }

    Ok(record)
}

fn encode_value(
    out: &mut Vec<u8>,
    field: &crate::schema::FieldDef,
    value: &Value,
) -> Result<(), RecordPackError> {
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
    field_type: FieldType,
    materialize: bool,
) -> Result<Option<Value>, RecordPackError> {
    match field_type {
        FieldType::String => {
            let bytes = read_bytes(block, pos)?;
            if !materialize {
                return Ok(None);
            }
            let text = std::str::from_utf8(bytes).map_err(|_| RecordPackError::InvalidRecordId)?;
            Ok(Some(Value::String(text.to_string())))
        }
        FieldType::I64 | FieldType::Timestamp => {
            read_i64_value(block, pos).map(|number| materialize.then(|| Value::from(number)))
        }
        FieldType::F64 => {
            let raw = read_i64_value(block, pos)?;
            if !materialize {
                return Ok(None);
            }
            Ok(Some(Value::from(f64::from_bits(raw as u64))))
        }
        FieldType::Bool => {
            let flag = read_u8(block, pos)?;
            Ok(materialize.then_some(Value::Bool(flag != 0)))
        }
        FieldType::Json => {
            let bytes = read_bytes(block, pos)?;
            if !materialize {
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

fn read_pack_fields(
    bytes: &[u8],
    pos: &mut usize,
    field_count: usize,
) -> Result<Vec<PackField>, RecordPackError> {
    let mut fields = Vec::with_capacity(field_count);

    for _ in 0..field_count {
        let name_len = read_u16(bytes, *pos)? as usize;
        *pos += 2;
        let end = pos
            .checked_add(name_len)
            .ok_or(RecordPackError::Truncated { len: bytes.len() })?;
        let name = bytes
            .get(*pos..end)
            .ok_or(RecordPackError::Truncated { len: bytes.len() })?;
        let name = std::str::from_utf8(name)
            .map_err(|_| RecordPackError::InvalidRecordId)?
            .to_string();
        *pos = end;

        let tag = read_u8(bytes, pos)?;
        let field_type =
            FieldType::from_tag(tag).ok_or(RecordPackError::UnknownFieldType { tag })?;
        fields.push(PackField { name, field_type });
    }

    Ok(fields)
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
    use crate::schema::FieldType;
    use serde_json::json;

    fn schema() -> TableSchema {
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
        let schema = schema();
        let bytes = encode_record_pack(&schema, &[]).expect("empty pack should encode");
        let pack = decode_record_pack(&bytes).expect("empty pack should decode");

        assert_eq!(pack.record_count(), 0);
        assert_eq!(pack.fields().len(), schema.fields.len());
        assert!(pack.record_ids().unwrap().is_empty());
        assert_eq!(pack.read_record(&schema, "absent").unwrap(), None);
        assert!(pack.scan(&schema, &["score"]).unwrap().is_empty());
    }

    #[test]
    fn every_declared_type_round_trips_exactly() {
        let schema = schema();
        let record = sample_record("sample-1", 0.125);

        let bytes =
            encode_record_pack(&schema, std::slice::from_ref(&record)).expect("pack should encode");
        let pack = decode_record_pack(&bytes).expect("pack should decode");

        assert_eq!(pack.read_record(&schema, "sample-1").unwrap(), Some(record));
    }

    #[test]
    fn float_and_timestamp_fields_survive_bit_for_bit() {
        let schema = schema();
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
        let schema = schema();
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
        let schema = schema();
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
        let schema = schema();
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
    fn an_older_pack_reads_under_an_added_optional_field() {
        let old = schema();
        let bytes = encode_record_pack(&old, &[sample_record("a", 0.5)]).unwrap();
        let pack = decode_record_pack(&bytes).unwrap();

        let evolved = TableSchema::builder("inspect_ai_samples")
            .field("sample_id", FieldType::String)
            .required_field("task", FieldType::String)
            .field("score", FieldType::F64)
            .field("epoch", FieldType::I64)
            .field("completed_at", FieldType::Timestamp)
            .field("passed", FieldType::Bool)
            .field("input", FieldType::Json)
            .field("scorer", FieldType::String)
            .primary_key("sample_id")
            .build()
            .unwrap();
        old.evolve(&evolved)
            .expect("adding an optional field evolves");

        let decoded = pack.read_record(&evolved, "a").unwrap().unwrap();
        assert!(decoded.get("scorer").is_none());
        assert_eq!(decoded.get("score"), Some(&json!(0.5)));
    }

    #[test]
    fn an_older_pack_reads_an_added_required_field_as_its_default() {
        let old = schema();
        let bytes = encode_record_pack(&old, &[sample_record("a", 0.5)]).unwrap();
        let pack = decode_record_pack(&bytes).unwrap();

        let evolved = TableSchema::builder("inspect_ai_samples")
            .field("sample_id", FieldType::String)
            .required_field("task", FieldType::String)
            .field("score", FieldType::F64)
            .field("epoch", FieldType::I64)
            .field("completed_at", FieldType::Timestamp)
            .field("passed", FieldType::Bool)
            .field("input", FieldType::Json)
            .required_field_with_default("grader", FieldType::String, json!("exact_match"))
            .primary_key("sample_id")
            .build()
            .unwrap();
        old.evolve(&evolved)
            .expect("a defaulted required field evolves");

        let decoded = pack.read_record(&evolved, "a").unwrap().unwrap();
        assert_eq!(decoded.get("grader"), Some(&json!("exact_match")));
    }

    #[test]
    fn a_type_change_is_reported_only_when_the_field_is_requested() {
        let old = schema();
        let bytes = encode_record_pack(&old, &[sample_record("a", 0.5)]).unwrap();
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
            pack.read_record_with(&retyped, "a", &["score"]),
            Err(RecordPackError::FieldTypeMismatch { .. })
        ));
        assert!(pack.read_record_with(&retyped, "a", &["task"]).is_ok());
    }

    #[test]
    fn a_dropped_field_is_hidden_from_the_new_schema() {
        let old = schema();
        let bytes = encode_record_pack(&old, &[sample_record("a", 0.5)]).unwrap();
        let pack = decode_record_pack(&bytes).unwrap();

        let dropped = TableSchema::builder("inspect_ai_samples")
            .field("sample_id", FieldType::String)
            .required_field("task", FieldType::String)
            .field("epoch", FieldType::I64)
            .primary_key("sample_id")
            .build()
            .unwrap();
        old.evolve(&dropped).expect("dropping a field evolves");

        let decoded = pack.read_record(&dropped, "a").unwrap().unwrap();
        assert!(decoded.get("score").is_none());
        assert_eq!(decoded.get("task"), Some(&json!("gsm8k")));
    }

    #[test]
    fn unknown_record_fields_are_dropped_at_encode() {
        let schema = schema();
        let record = sample_record("a", 0.5).set("stale", json!("gone"));

        let bytes = encode_record_pack(&schema, &[record]).unwrap();
        let pack = decode_record_pack(&bytes).unwrap();
        let decoded = pack.read_record(&schema, "a").unwrap().unwrap();

        assert!(decoded.get("stale").is_none());
    }

    #[test]
    fn duplicate_ids_and_invalid_records_are_rejected() {
        let schema = schema();
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
        let schema = schema();
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

    #[test]
    fn an_unknown_field_type_tag_is_rejected() {
        let schema = schema();
        let mut bytes =
            encode_record_pack(&schema, &[sample_record("a", 0.5)]).expect("pack should encode");

        let mut pos = MAGIC.len() + 2;
        let name_len = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        pos += 2 + name_len;
        bytes[pos] = 200;

        let checksum_pos = bytes.len() - FOOTER_LEN;
        let checksum = crate::crc::checksum(&bytes[..checksum_pos]);
        bytes[checksum_pos..].copy_from_slice(&checksum.to_le_bytes());

        assert!(matches!(
            decode_record_pack(&bytes),
            Err(RecordPackError::UnknownFieldType { tag: 200 })
        ));
    }
}

#[cfg(kani)]
mod verify {
    use super::*;

    fn minimal_pack(record_id: &str, payload: &[u8]) -> Vec<u8> {
        let mut header = Vec::new();
        header.extend_from_slice(&MAGIC);
        header.extend_from_slice(&1u16.to_le_bytes());
        header.extend_from_slice(&1u16.to_le_bytes());
        header.extend_from_slice(b"f");
        header.push(FieldType::Json.tag());
        header.extend_from_slice(&1u32.to_le_bytes());
        header.extend_from_slice(&0u64.to_le_bytes());

        let entry_offsets_pos = header.len() + payload.len();
        let entries_pos = entry_offsets_pos + 8;
        header[header.len() - 8..].copy_from_slice(&(entry_offsets_pos as u64).to_le_bytes());

        let mut pack = header;
        pack.extend_from_slice(payload);
        pack.extend_from_slice(&(entries_pos as u64).to_le_bytes());
        pack.extend_from_slice(&(record_id.len() as u16).to_le_bytes());
        pack.extend_from_slice(record_id.as_bytes());
        pack.extend_from_slice(&28u64.to_le_bytes());
        pack.extend_from_slice(&(payload.len() as u64).to_le_bytes());

        let checksum = checksum(&pack);
        pack.extend_from_slice(&checksum.to_le_bytes());
        pack
    }

    /// Any ASCII corruption with the footer checksum restamped either errors or
    /// walks the header and directory without panicking. Schema decode stays
    /// out of the solver by design.
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
            let _ = parsed.fields();
        }
    }
}
