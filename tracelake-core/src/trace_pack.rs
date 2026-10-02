use crate::span::{Span, Trace};
use std::cmp::Ordering;
use thiserror::Error;

/// Format marker at the start of every trace pack. The trailing digits are the
/// format version; bump them when the layout changes incompatibly.
const MAGIC: [u8; 8] = *b"TRCPAK01";

/// Header length: magic (8) + trace_count (4) + span_count (8) + directory
/// offset (8), all little-endian.
const HEADER_LEN: usize = 8 + 4 + 8 + 8;

/// Footer length: crc32 (4) over every byte before the footer.
const FOOTER_LEN: usize = 4;

/// Errors returned while encoding or reading a trace pack.
#[derive(Debug, Error)]
pub enum TracePackError {
    #[error("not a trace pack: bad magic")]
    BadMagic,

    #[error("trace pack truncated: {len} bytes")]
    Truncated { len: usize },

    #[error("trace pack checksum mismatch: expected {expected:#x}, found {found:#x}")]
    ChecksumMismatch { expected: u32, found: u32 },

    #[error("duplicate trace_id: {trace_id}")]
    DuplicateTraceId { trace_id: String },

    #[error("trace {trace_id} has no spans")]
    EmptyTrace { trace_id: String },

    #[error("trace id is not valid utf-8")]
    InvalidTraceId,

    #[error("trace id is {len} bytes, longer than the u16 limit")]
    TraceIdTooLong { len: usize },

    #[error("directory offset {offset} with {count} entries exceeds pack length {total}")]
    DirectoryOutOfBounds {
        offset: u64,
        count: u64,
        total: usize,
    },

    #[error("trace block offset {offset} length {len} exceeds pack length {total}")]
    BlockOutOfBounds { offset: u64, len: u64, total: usize },

    #[error("trace block has {extra} trailing bytes")]
    TrailingBytes { extra: usize },

    #[error("span decode failed: {0}")]
    SpanDecode(#[from] serde_json::Error),
}

/// Encodes traces into one trace pack. Traces are sorted by trace_id and every
/// span is stored as length-prefixed JSON in the order given; callers pass
/// spans in storage key order (start time, then span_id).
///
/// The layout is:
///
/// ```text
/// [header: magic, trace_count, span_count, directory_offset]
/// [trace blocks: per span a u32 length + JSON bytes]
/// [directory: u64 entry offsets (sorted by trace_id), then entries]
/// [footer: crc32 over all preceding bytes]
/// ```
pub fn encode_trace_pack(traces: &[Trace]) -> Result<Vec<u8>, TracePackError> {
    let mut sorted: Vec<&Trace> = traces.iter().collect();
    sorted.sort_by(|a, b| a.trace_id.cmp(&b.trace_id));

    for pair in sorted.windows(2) {
        if pair[0].trace_id == pair[1].trace_id {
            return Err(TracePackError::DuplicateTraceId {
                trace_id: pair[0].trace_id.clone(),
            });
        }
    }

    let mut blocks: Vec<u8> = Vec::new();
    let mut entries: Vec<(String, u32, u64, u64)> = Vec::with_capacity(sorted.len());
    let mut span_count: u64 = 0;

    for trace in &sorted {
        if trace.spans.is_empty() {
            return Err(TracePackError::EmptyTrace {
                trace_id: trace.trace_id.clone(),
            });
        }

        let block_start = blocks.len();
        for span in &trace.spans {
            let value = serde_json::to_vec(span)?;
            blocks.extend_from_slice(&(value.len() as u32).to_le_bytes());
            blocks.extend_from_slice(&value);
        }

        entries.push((
            trace.trace_id.clone(),
            trace.spans.len() as u32,
            (HEADER_LEN + block_start) as u64,
            (blocks.len() - block_start) as u64,
        ));
        span_count += trace.spans.len() as u64;
    }

    let entry_offsets_pos = (HEADER_LEN + blocks.len()) as u64;
    let entries_pos = entry_offsets_pos + 8 * entries.len() as u64;

    let mut entry_bytes: Vec<u8> = Vec::new();
    let mut entry_offsets: Vec<u64> = Vec::with_capacity(entries.len());

    for (trace_id, span_count, block_offset, block_len) in &entries {
        let trace_id_len =
            u16::try_from(trace_id.len()).map_err(|_| TracePackError::TraceIdTooLong {
                len: trace_id.len(),
            })?;
        entry_offsets.push(entries_pos + entry_bytes.len() as u64);
        entry_bytes.extend_from_slice(&trace_id_len.to_le_bytes());
        entry_bytes.extend_from_slice(trace_id.as_bytes());
        entry_bytes.extend_from_slice(&span_count.to_le_bytes());
        entry_bytes.extend_from_slice(&block_offset.to_le_bytes());
        entry_bytes.extend_from_slice(&block_len.to_le_bytes());
    }

    let mut pack =
        Vec::with_capacity(HEADER_LEN + blocks.len() + entry_bytes.len() + entries.len() * 8);
    pack.extend_from_slice(&MAGIC);
    pack.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    pack.extend_from_slice(&span_count.to_le_bytes());
    pack.extend_from_slice(&entry_offsets_pos.to_le_bytes());
    pack.extend_from_slice(&blocks);
    for offset in entry_offsets {
        pack.extend_from_slice(&offset.to_le_bytes());
    }
    pack.extend_from_slice(&entry_bytes);

    let checksum = crc32fast::hash(&pack);
    pack.extend_from_slice(&checksum.to_le_bytes());

    Ok(pack)
}

/// A decoded, read-only view over trace pack bytes.
///
/// The pack is validated up front (magic, checksum, directory bounds); trace
/// lookups binary-search the directory and decode only the requested block.
#[derive(Debug, Clone, Copy)]
pub struct TracePack<'a> {
    bytes: &'a [u8],
    trace_count: usize,
    span_count: u64,
    entry_offsets_pos: usize,
}

/// Decodes and validates a trace pack from its bytes.
pub fn decode_trace_pack(bytes: &[u8]) -> Result<TracePack<'_>, TracePackError> {
    if bytes.len() < HEADER_LEN + FOOTER_LEN {
        return Err(TracePackError::Truncated { len: bytes.len() });
    }

    if bytes[..MAGIC.len()] != MAGIC {
        return Err(TracePackError::BadMagic);
    }

    let checksum_pos = bytes.len() - FOOTER_LEN;
    let expected = read_u32(bytes, checksum_pos)?;
    let found = crc32fast::hash(&bytes[..checksum_pos]);
    if expected != found {
        return Err(TracePackError::ChecksumMismatch { expected, found });
    }

    let trace_count = read_u32(bytes, MAGIC.len())? as usize;
    let span_count = read_u64(bytes, MAGIC.len() + 4)?;
    let entry_offsets_pos = read_u64(bytes, MAGIC.len() + 12)? as usize;

    let directory_len = 8 * trace_count;
    let directory_end = entry_offsets_pos.checked_add(directory_len).ok_or(
        TracePackError::DirectoryOutOfBounds {
            offset: entry_offsets_pos as u64,
            count: trace_count as u64,
            total: bytes.len(),
        },
    )?;
    if directory_end > checksum_pos {
        return Err(TracePackError::DirectoryOutOfBounds {
            offset: entry_offsets_pos as u64,
            count: trace_count as u64,
            total: bytes.len(),
        });
    }

    Ok(TracePack {
        bytes,
        trace_count,
        span_count,
        entry_offsets_pos,
    })
}

impl TracePack<'_> {
    /// Number of traces in the pack.
    pub fn trace_count(&self) -> usize {
        self.trace_count
    }

    /// Total number of spans across all traces in the pack.
    pub fn span_count(&self) -> u64 {
        self.span_count
    }

    /// Every trace_id in the pack, sorted.
    pub fn trace_ids(&self) -> Result<Vec<String>, TracePackError> {
        let mut ids = Vec::with_capacity(self.trace_count);
        for index in 0..self.trace_count {
            let entry = self.entry_at(index)?;
            ids.push(entry.trace_id.to_string());
        }
        Ok(ids)
    }

    /// Returns the spans for one trace, or `None` when the pack does not hold
    /// it. Spans come back in the order they were encoded.
    pub fn read_trace(&self, trace_id: &str) -> Result<Option<Vec<Span>>, TracePackError> {
        let mut low = 0;
        let mut high = self.trace_count;

        while low < high {
            let mid = low + (high - low) / 2;
            let entry = self.entry_at(mid)?;

            match entry.trace_id.as_bytes().cmp(trace_id.as_bytes()) {
                Ordering::Equal => return self.read_block(&entry).map(Some),
                Ordering::Less => low = mid + 1,
                Ordering::Greater => high = mid,
            }
        }

        Ok(None)
    }

    fn entry_at(&self, index: usize) -> Result<DirectoryEntry<'_>, TracePackError> {
        let offset_pos = self.entry_offsets_pos + index * 8;
        let entry_pos = read_u64(self.bytes, offset_pos)? as usize;
        read_entry(self.bytes, entry_pos)
    }

    fn read_block(&self, entry: &DirectoryEntry<'_>) -> Result<Vec<Span>, TracePackError> {
        let start = entry.block_offset as usize;
        let end = start.checked_add(entry.block_len as usize).ok_or(
            TracePackError::BlockOutOfBounds {
                offset: entry.block_offset,
                len: entry.block_len,
                total: self.bytes.len(),
            },
        )?;
        let block = self
            .bytes
            .get(start..end)
            .ok_or(TracePackError::BlockOutOfBounds {
                offset: entry.block_offset,
                len: entry.block_len,
                total: self.bytes.len(),
            })?;

        let mut spans = Vec::with_capacity(entry.span_count as usize);
        let mut pos = 0;

        while pos < block.len() {
            let span_len = read_u32(block, pos)? as usize;
            let value_start = pos + 4;
            let value_end = value_start
                .checked_add(span_len)
                .ok_or(TracePackError::Truncated { len: block.len() })?;
            let value = block
                .get(value_start..value_end)
                .ok_or(TracePackError::Truncated { len: block.len() })?;
            spans.push(serde_json::from_slice(value)?);
            pos = value_end;
        }

        if pos != block.len() {
            return Err(TracePackError::TrailingBytes {
                extra: block.len() - pos,
            });
        }

        Ok(spans)
    }
}

/// One directory entry: where a trace's block lives.
#[derive(Debug)]
struct DirectoryEntry<'a> {
    trace_id: &'a str,
    span_count: u32,
    block_offset: u64,
    block_len: u64,
}

fn read_entry(bytes: &[u8], pos: usize) -> Result<DirectoryEntry<'_>, TracePackError> {
    let id_len = read_u16(bytes, pos)? as usize;
    let id_start = pos + 2;
    let id_end = id_start
        .checked_add(id_len)
        .ok_or(TracePackError::Truncated { len: bytes.len() })?;
    let id_bytes = bytes
        .get(id_start..id_end)
        .ok_or(TracePackError::Truncated { len: bytes.len() })?;
    let trace_id = std::str::from_utf8(id_bytes).map_err(|_| TracePackError::InvalidTraceId)?;

    Ok(DirectoryEntry {
        trace_id,
        span_count: read_u32(bytes, id_end)?,
        block_offset: read_u64(bytes, id_end + 4)?,
        block_len: read_u64(bytes, id_end + 12)?,
    })
}

fn read_u16(bytes: &[u8], pos: usize) -> Result<u16, TracePackError> {
    let raw = read_array::<2>(bytes, pos)?;
    Ok(u16::from_le_bytes(raw))
}

fn read_u32(bytes: &[u8], pos: usize) -> Result<u32, TracePackError> {
    let raw = read_array::<4>(bytes, pos)?;
    Ok(u32::from_le_bytes(raw))
}

fn read_u64(bytes: &[u8], pos: usize) -> Result<u64, TracePackError> {
    let raw = read_array::<8>(bytes, pos)?;
    Ok(u64::from_le_bytes(raw))
}

fn read_array<const N: usize>(bytes: &[u8], pos: usize) -> Result<[u8; N], TracePackError> {
    let end = pos
        .checked_add(N)
        .ok_or(TracePackError::Truncated { len: bytes.len() })?;
    let slice = bytes
        .get(pos..end)
        .ok_or(TracePackError::Truncated { len: bytes.len() })?;

    let mut raw = [0u8; N];
    raw.copy_from_slice(slice);
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::{EvalScore, SpanStatus, SpanType, TokenUsage};
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    fn sample_span(trace_id: &str, span_id: &str, seconds: u32) -> Span {
        Span {
            tenant_id: "tenantid-1".to_string(),
            namespace: "default".to_string(),
            trace_id: trace_id.to_string(),
            span_id: span_id.to_string(),
            parent_span_id: None,
            span_type: SpanType::Llm,
            name: format!("span-{span_id}"),
            start_time: Utc.with_ymd_and_hms(2026, 6, 16, 10, 0, seconds).unwrap(),
            end_time: Some(
                Utc.with_ymd_and_hms(2026, 6, 16, 10, 0, seconds + 1)
                    .unwrap(),
            ),
            status: SpanStatus::Ok,
            attributes: json!({ "case": "pack" }),
            input: Some(json!({ "prompt": format!("prompt-{span_id}") })),
            output: Some(json!({ "text": format!("output-{span_id}") })),
            tokens: Some(TokenUsage {
                prompt: 10,
                completion: 20,
                total: 30,
            }),
            cost_usd: Some(0.001),
            model: Some("gpt-4o".to_string()),
            provider: Some("openai".to_string()),
            error: None,
            tags: vec!["pack-test".to_string()],
            scores: vec![],
        }
    }

    fn sample_trace(trace_id: &str, span_ids: &[&str]) -> Trace {
        Trace {
            tenant_id: "tenantid-1".to_string(),
            namespace: "default".to_string(),
            trace_id: trace_id.to_string(),
            spans: span_ids
                .iter()
                .enumerate()
                .map(|(index, span_id)| {
                    sample_span(trace_id, span_id, span_ids.len() as u32 - index as u32)
                })
                .collect(),
        }
    }

    #[test]
    fn empty_pack_round_trips() {
        let bytes = encode_trace_pack(&[]).expect("empty pack should encode");
        let pack = decode_trace_pack(&bytes).expect("empty pack should decode");

        assert_eq!(pack.trace_count(), 0);
        assert_eq!(pack.span_count(), 0);
        assert_eq!(
            pack.trace_ids().expect("ids should read"),
            Vec::<String>::new()
        );
        assert_eq!(
            pack.read_trace("missing").expect("lookup should succeed"),
            None
        );
    }

    #[test]
    fn single_span_trace_round_trips_exactly() {
        let trace = sample_trace("trace-1", &["span-1"]);

        let bytes = encode_trace_pack(std::slice::from_ref(&trace)).expect("pack should encode");
        let pack = decode_trace_pack(&bytes).expect("pack should decode");

        assert_eq!(pack.trace_count(), 1);
        assert_eq!(pack.span_count(), 1);

        let spans = pack
            .read_trace("trace-1")
            .expect("lookup should succeed")
            .expect("trace should exist");

        assert_eq!(spans, trace.spans);
    }

    #[test]
    fn lookup_hits_binary_search_edges_and_misses() {
        let traces = vec![
            sample_trace("trace-000", &["a", "b"]),
            sample_trace("trace-042", &["c"]),
            sample_trace("trace-099", &["d", "e", "f"]),
        ];

        let bytes = encode_trace_pack(&traces).expect("pack should encode");
        let pack = decode_trace_pack(&bytes).expect("pack should decode");

        assert_eq!(
            pack.trace_ids().expect("ids should read"),
            vec!["trace-000", "trace-042", "trace-099"]
        );

        for trace in &traces {
            let spans = pack
                .read_trace(&trace.trace_id)
                .expect("lookup should succeed")
                .expect("trace should exist");
            assert_eq!(spans, trace.spans, "trace {} spans", trace.trace_id);
        }

        assert_eq!(
            pack.read_trace("trace-999").expect("lookup should succeed"),
            None
        );
        assert_eq!(
            pack.read_trace("trace-0").expect("lookup should succeed"),
            None
        );
    }

    #[test]
    fn float_fields_survive_a_pack_round_trip_bit_for_bit() {
        // Regression found by the pack round-trip property: serde_json's
        // default float parsing is best-effort, so `172.0 * 0.001` (one ULP
        // away from the nearest double to 0.172) came back as 0.172 until the
        // `float_roundtrip` feature was enabled.
        let value = 172.0f64 * 0.001;
        assert_ne!(value.to_bits(), 0.172f64.to_bits());

        let mut trace = sample_trace("trace-1", &["span-1"]);
        trace.spans[0].cost_usd = Some(value);
        trace.spans[0].scores = vec![EvalScore {
            name: "cost".to_string(),
            value,
        }];

        let bytes = encode_trace_pack(std::slice::from_ref(&trace)).expect("pack should encode");
        let pack = decode_trace_pack(&bytes).expect("pack should decode");
        let spans = pack
            .read_trace("trace-1")
            .expect("lookup should succeed")
            .expect("trace should exist");

        assert_eq!(spans[0].cost_usd.map(f64::to_bits), Some(value.to_bits()));
        assert_eq!(spans[0].scores[0].value.to_bits(), value.to_bits());
    }

    #[test]
    fn duplicate_trace_ids_are_rejected() {
        let traces = vec![
            sample_trace("trace-1", &["a"]),
            sample_trace("trace-1", &["b"]),
        ];

        let error = encode_trace_pack(&traces).expect_err("duplicate ids should fail");

        assert!(matches!(
            error,
            TracePackError::DuplicateTraceId { ref trace_id } if trace_id == "trace-1"
        ));
    }

    #[test]
    fn empty_trace_is_rejected() {
        let traces = vec![Trace {
            tenant_id: "tenantid-1".to_string(),
            namespace: "default".to_string(),
            trace_id: "trace-1".to_string(),
            spans: Vec::new(),
        }];

        let error = encode_trace_pack(&traces).expect_err("empty trace should fail");

        assert!(matches!(error, TracePackError::EmptyTrace { .. }));
    }

    #[test]
    fn corrupted_payload_fails_checksum() {
        let traces = vec![sample_trace("trace-1", &["a", "b", "c", "d"])];
        let mut bytes = encode_trace_pack(&traces).expect("pack should encode");

        let middle = bytes.len() / 2;
        bytes[middle] ^= 0xff;

        let error = decode_trace_pack(&bytes).expect_err("corrupted pack should fail");

        assert!(matches!(error, TracePackError::ChecksumMismatch { .. }));
    }

    #[test]
    fn truncated_pack_is_rejected() {
        let traces = vec![sample_trace("trace-1", &["a", "b"])];
        let bytes = encode_trace_pack(&traces).expect("pack should encode");

        let cut = decode_trace_pack(&bytes[..bytes.len() - 10])
            .expect_err("pack with the footer cut off should fail");
        assert!(matches!(cut, TracePackError::ChecksumMismatch { .. }));

        let tiny =
            decode_trace_pack(&bytes[..10]).expect_err("pack shorter than a header should fail");
        assert!(matches!(tiny, TracePackError::Truncated { .. }));
    }
}

#[cfg(kani)]
mod verify {
    use super::*;
    use chrono::{DateTime, Utc};
    use serde_json::json;

    /// Minimal spans keep the pack small so crc32 and the directory walk
    /// stay within tight unwind bounds.
    fn fixed_span(trace_id: &str, span_id: &str, nanos: i64) -> Span {
        Span {
            tenant_id: "t".to_string(),
            namespace: "ns".to_string(),
            trace_id: trace_id.to_string(),
            span_id: span_id.to_string(),
            parent_span_id: None,
            span_type: crate::span::SpanType::Llm,
            name: span_id.to_string(),
            start_time: DateTime::from_timestamp_nanos(nanos),
            end_time: None,
            status: crate::span::SpanStatus::Ok,
            attributes: serde_json::Value::Null,
            input: Some(json!({ "prompt": "hello" })),
            output: None,
            tokens: None,
            cost_usd: None,
            model: None,
            provider: None,
            error: None,
            tags: Vec::new(),
            scores: Vec::new(),
        }
    }

    /// Two one-span traces: enough to exercise the directory binary search
    /// on both sides of a probe.
    fn fixed_traces() -> Vec<Trace> {
        vec![
            Trace {
                tenant_id: "t".to_string(),
                namespace: "ns".to_string(),
                trace_id: "trace-1".to_string(),
                spans: vec![fixed_span("trace-1", "s1", 1)],
            },
            Trace {
                tenant_id: "t".to_string(),
                namespace: "ns".to_string(),
                trace_id: "trace-2".to_string(),
                spans: vec![fixed_span("trace-2", "s2", 2)],
            },
        ]
    }

    /// Proves encode then read_trace recovers the exact spans (P-RT on a
    /// fixed corpus; arbitrary corpora are covered by the proptest suite).
    #[kani::proof]
    #[kani::unwind(30)]
    fn fixed_packs_round_trip_exactly() {
        let traces = fixed_traces();

        let bytes = encode_trace_pack(&traces).expect("fixed traces encode");
        let pack = decode_trace_pack(&bytes).expect("valid packs decode");

        assert_eq!(pack.trace_count(), 2);
        assert_eq!(pack.span_count(), 2);
        assert_eq!(
            pack.trace_ids().expect("ids should read"),
            vec!["trace-1".to_string(), "trace-2".to_string()]
        );
        assert_eq!(
            pack.read_trace("trace-1")
                .expect("lookup should succeed")
                .expect("trace should exist"),
            traces[0].spans
        );
        assert_eq!(
            pack.read_trace("trace-2")
                .expect("lookup should succeed")
                .expect("trace should exist"),
            traces[1].spans
        );
        assert_eq!(
            pack.read_trace("trace-9").expect("misses should succeed"),
            None
        );
    }

    /// Proves the deep parsers (directory walk, entry reader, block reader,
    /// span decode) never panic for ANY single-byte corruption of a valid
    /// pack. The footer is re-stamped with the recomputed crc32 so decoding
    /// proceeds past the checksum; without the restamp every corruption dies
    /// at the checksum comparison and the deep paths are unreachable.
    #[kani::proof]
    #[kani::unwind(128)]
    fn decoding_restamped_single_byte_corruptions_never_panics() {
        let mut bytes = encode_trace_pack(&fixed_traces()).expect("fixed traces encode");

        let index: usize = kani::any();
        let value: u8 = kani::any();
        kani::assume(index < bytes.len());
        bytes[index] = value;

        let checksum_pos = bytes.len() - FOOTER_LEN;
        let checksum = crc32fast::hash(&bytes[..checksum_pos]);
        bytes[checksum_pos..checksum_pos + 4].copy_from_slice(&checksum.to_le_bytes());

        if let Ok(pack) = decode_trace_pack(&bytes) {
            let _ = pack.trace_ids();
            let _ = pack.read_trace("trace-1");
            let _ = pack.read_trace("trace-9");
        }
    }
}
