use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path as FsPath;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{STORED, STRING, Schema, TEXT, Value as TantivyValue};
use tantivy::{Index, IndexWriter, TantivyDocument, doc};
use tempfile::TempDir;
use thiserror::Error;
use tracelake_core::Trace;

/// Format marker and version for the text index container.
const MAGIC: [u8; 8] = *b"TRCTEXT1";

/// Longest text taken from one span field. Prompts and completions can be
/// megabytes, and an unbounded field makes the index larger than the data it
/// describes.
pub const MAX_INDEXED_TEXT_BYTES: usize = 256 * 1024;

#[derive(Debug, Error)]
pub enum TextIndexError {
    #[error("text index container is truncated or malformed")]
    Container,

    #[error("text index container checksum mismatch")]
    ChecksumMismatch,

    #[error("text index i/o failed: {0}")]
    Io(#[from] io::Error),

    #[error("tantivy failed: {0}")]
    Tantivy(#[from] tantivy::TantivyError),

    #[error("text query is invalid: {0}")]
    Query(#[from] tantivy::query::QueryParserError),

    #[error("text index file name is not valid utf-8")]
    InvalidName,
}

struct TextSchema {
    trace_id: tantivy::schema::Field,
    span_id: tantivy::schema::Field,
    name: tantivy::schema::Field,
    input: tantivy::schema::Field,
    output: tantivy::schema::Field,
    error: tantivy::schema::Field,
    schema: Schema,
}

fn text_schema() -> TextSchema {
    let mut builder = Schema::builder();
    let trace_id = builder.add_text_field("trace_id", STRING | STORED);
    let span_id = builder.add_text_field("span_id", STRING | STORED);
    let name = builder.add_text_field("name", TEXT);
    let input = builder.add_text_field("input", TEXT);
    let output = builder.add_text_field("output", TEXT);
    let error = builder.add_text_field("error", TEXT);

    TextSchema {
        trace_id,
        span_id,
        name,
        input,
        output,
        error,
        schema: builder.build(),
    }
}

/// Builds a Tantivy index over every span's name, input, output, and error, and
/// serializes it into one object that can be stored beside a segment.
/// A built index that stays open. The hot-store cache holds one of these per
/// namespace and searches it directly, so repeated queries do not rebuild or
/// unpack anything.
pub struct CachedTextIndex {
    _dir: TempDir,
    index: Index,
}

impl CachedTextIndex {
    pub fn index(&self) -> &Index {
        &self.index
    }
}

/// Builds an index in a temporary directory that the result keeps alive.
pub fn build_cached_text_index(traces: &[Trace]) -> Result<CachedTextIndex, TextIndexError> {
    let dir = TempDir::new()?;
    let fields = text_schema();
    let index = Index::create_in_dir(dir.path(), fields.schema.clone())?;
    let mut writer: IndexWriter = index.writer(50_000_000)?;

    for trace in traces {
        for span in &trace.spans {
            writer.add_document(doc!(
                fields.trace_id => trace.trace_id.clone(),
                fields.span_id => span.span_id.clone(),
                fields.name => clip(&span.name),
                fields.input => span.input.as_ref().map(json_text).unwrap_or_default(),
                fields.output => span.output.as_ref().map(json_text).unwrap_or_default(),
                fields.error => span.error.clone().map(|error| clip(&error)).unwrap_or_default(),
            ))?;
        }
    }

    writer.commit()?;
    writer.wait_merging_threads()?;

    Ok(CachedTextIndex { _dir: dir, index })
}

/// Builds a Tantivy index over every span's name, input, output, and error, and
/// serializes it into one object that can be stored beside a segment.
pub fn build_text_index(traces: &[Trace]) -> Result<Vec<u8>, TextIndexError> {
    let cached = build_cached_text_index(traces)?;
    collect_container(cached._dir.path())
}

/// Runs one text query against an open index and returns matching trace_ids,
/// best score first.
pub fn search_index(
    index: &Index,
    text_query: &str,
    limit: usize,
) -> Result<Vec<String>, TextIndexError> {
    let fields = text_schema();
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let parser = QueryParser::for_index(
        index,
        vec![fields.name, fields.input, fields.output, fields.error],
    );
    let query = parser.parse_query(text_query)?;
    let top = searcher.search(&query, &TopDocs::with_limit(limit.max(1)))?;

    let mut seen: HashSet<String> = HashSet::new();
    let mut trace_ids = Vec::new();
    for (_score, address) in top {
        let document: TantivyDocument = searcher.doc(address)?;
        let Some(value) = document.get_first(fields.trace_id) else {
            continue;
        };
        let Some(trace_id) = value.as_str() else {
            continue;
        };
        if seen.insert(trace_id.to_string()) {
            trace_ids.push(trace_id.to_string());
        }
    }

    Ok(trace_ids)
}

/// Runs one text query against a serialized index and returns matching
/// trace_ids, best score first.
pub fn search_text_index(
    bytes: &[u8],
    text_query: &str,
    limit: usize,
) -> Result<Vec<String>, TextIndexError> {
    let dir = TempDir::new()?;
    unpack_container(bytes, dir.path())?;

    let index = Index::open_in_dir(dir.path())?;
    search_index(&index, text_query, limit)
}

fn clip(text: &str) -> String {
    match text.char_indices().nth(MAX_INDEXED_TEXT_BYTES) {
        Some((end, _)) => text[..end].to_string(),
        None => text.to_string(),
    }
}

/// Flattens every string in a JSON value so a prompt nested in an object is
/// still searchable by its words.
fn json_text(value: &Value) -> String {
    let mut out = String::new();
    push_json_text(value, &mut out);
    clip(&out)
}

fn push_json_text(value: &Value, out: &mut String) {
    if out.len() >= MAX_INDEXED_TEXT_BYTES {
        return;
    }

    match value {
        Value::String(text) => {
            out.push_str(text);
            out.push(' ');
        }
        Value::Array(items) => {
            for item in items {
                push_json_text(item, out);
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                push_json_text(value, out);
            }
        }
        _ => {}
    }
}

fn collect_container(root: &FsPath) -> Result<Vec<u8>, TextIndexError> {
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    collect_files(root, root, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&(files.len() as u32).to_le_bytes());

    for (name, bytes) in &files {
        let name_len = u16::try_from(name.len()).map_err(|_| TextIndexError::Container)?;
        let bytes_len = u32::try_from(bytes.len()).map_err(|_| TextIndexError::Container)?;
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&bytes_len.to_le_bytes());
        out.extend_from_slice(bytes);
    }

    let checksum = crc32fast::hash(&out);
    out.extend_from_slice(&checksum.to_le_bytes());

    Ok(out)
}

fn collect_files(
    root: &FsPath,
    dir: &FsPath,
    files: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), TextIndexError> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            collect_files(root, &path, files)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }

        let name = path
            .strip_prefix(root)
            .map_err(|_| TextIndexError::Container)?
            .to_string_lossy()
            .to_string();
        if name.ends_with(".lock") {
            continue;
        }

        files.push((name, fs::read(&path)?));
    }

    Ok(())
}

/// An index object is written at query time, so a crafted name must not escape
/// the temporary directory.
fn safe_relative(name: &str) -> Result<&FsPath, TextIndexError> {
    let path = FsPath::new(name);
    if path.is_absolute()
        || !path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
    {
        return Err(TextIndexError::InvalidName);
    }

    Ok(path)
}

fn unpack_container(bytes: &[u8], root: &FsPath) -> Result<(), TextIndexError> {
    if bytes.len() < MAGIC.len() + 4 + 4 {
        return Err(TextIndexError::Container);
    }
    if bytes[..MAGIC.len()] != MAGIC {
        return Err(TextIndexError::Container);
    }

    let body = &bytes[..bytes.len() - 4];
    let expected = u32::from_le_bytes(
        bytes[bytes.len() - 4..]
            .try_into()
            .map_err(|_| TextIndexError::Container)?,
    );
    if crc32fast::hash(body) != expected {
        return Err(TextIndexError::ChecksumMismatch);
    }

    let mut pos = MAGIC.len();
    let file_count = u32::from_le_bytes(
        body.get(pos..pos + 4)
            .ok_or(TextIndexError::Container)?
            .try_into()
            .map_err(|_| TextIndexError::Container)?,
    ) as usize;
    pos += 4;

    for _ in 0..file_count {
        let name_len = u16::from_le_bytes(
            body.get(pos..pos + 2)
                .ok_or(TextIndexError::Container)?
                .try_into()
                .map_err(|_| TextIndexError::Container)?,
        ) as usize;
        pos += 2;
        let name = body
            .get(pos..pos + name_len)
            .ok_or(TextIndexError::Container)?;
        let name = std::str::from_utf8(name).map_err(|_| TextIndexError::InvalidName)?;
        pos += name_len;

        let bytes_len = u32::from_le_bytes(
            body.get(pos..pos + 4)
                .ok_or(TextIndexError::Container)?
                .try_into()
                .map_err(|_| TextIndexError::Container)?,
        ) as usize;
        pos += 4;
        let file = body
            .get(pos..pos + bytes_len)
            .ok_or(TextIndexError::Container)?;
        pos += bytes_len;

        let path = root.join(safe_relative(name)?);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, file)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use serde_json::json;
    use tracelake_core::{Span, SpanStatus, SpanType};

    fn span(trace_id: &str, span_id: &str, input: &str, output: &str, error: Option<&str>) -> Span {
        Span {
            tenant_id: "tenant-1".to_string(),
            namespace: "default".to_string(),
            trace_id: trace_id.to_string(),
            span_id: span_id.to_string(),
            parent_span_id: None,
            span_type: SpanType::Llm,
            name: "completion".to_string(),
            start_time: Utc.with_ymd_and_hms(2026, 10, 1, 12, 0, 0).unwrap(),
            end_time: None,
            status: SpanStatus::Ok,
            attributes: Value::Null,
            input: Some(json!({ "prompt": input })),
            output: Some(json!({ "text": output })),
            tokens: None,
            cost_usd: None,
            model: None,
            provider: None,
            error: error.map(str::to_string),
            tags: Vec::new(),
            scores: Vec::new(),
        }
    }

    fn trace(trace_id: &str, spans: Vec<Span>) -> Trace {
        Trace {
            tenant_id: "tenant-1".to_string(),
            namespace: "default".to_string(),
            trace_id: trace_id.to_string(),
            spans,
        }
    }

    #[test]
    fn a_round_trip_finds_prompts_outputs_and_errors() {
        let traces = vec![
            trace(
                "trace-hallucination",
                vec![span(
                    "trace-hallucination",
                    "s1",
                    "Does the answer hallucinate a refund policy?",
                    "The agent invented a 90 day window.",
                    None,
                )],
            ),
            trace(
                "trace-timeout",
                vec![span(
                    "trace-timeout",
                    "s1",
                    "Call the billing tool",
                    "",
                    Some("tool timeout after 30s"),
                )],
            ),
        ];

        let bytes = build_text_index(&traces).expect("index builds");
        assert_eq!(bytes[..MAGIC.len()], MAGIC);

        let hits = search_text_index(&bytes, "hallucinate", 10).expect("search runs");
        assert_eq!(hits, vec!["trace-hallucination".to_string()]);

        let hits = search_text_index(&bytes, "timeout", 10).expect("search runs");
        assert_eq!(hits, vec!["trace-timeout".to_string()]);

        let hits = search_text_index(&bytes, "refund", 10).expect("search runs");
        assert_eq!(hits, vec!["trace-hallucination".to_string()]);

        let hits = search_text_index(&bytes, "nonexistentterm", 10).expect("search runs");
        assert!(hits.is_empty());
    }

    #[test]
    fn a_trace_with_many_matching_spans_is_returned_once_and_limited() {
        let spans: Vec<Span> = (0..5)
            .map(|index| {
                span(
                    "trace-many",
                    &format!("s{index}"),
                    "refund policy question",
                    "",
                    None,
                )
            })
            .collect();
        let traces = vec![trace("trace-many", spans)];

        let bytes = build_text_index(&traces).unwrap();
        let hits = search_text_index(&bytes, "refund", 10).unwrap();

        assert_eq!(hits, vec!["trace-many".to_string()]);
        assert_eq!(search_text_index(&bytes, "refund", 1).unwrap().len(), 1);
    }

    #[test]
    fn an_empty_index_searches_cleanly() {
        let bytes = build_text_index(&[]).unwrap();

        assert!(
            search_text_index(&bytes, "anything", 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_corrupted_container_is_rejected() {
        let bytes =
            build_text_index(&[trace("t", vec![span("t", "s", "hello", "world", None)])]).unwrap();

        let mut corrupted = bytes.clone();
        let middle = corrupted.len() / 2;
        corrupted[middle] ^= 0xff;
        assert!(matches!(
            search_text_index(&corrupted, "hello", 10),
            Err(TextIndexError::ChecksumMismatch)
        ));

        assert!(matches!(
            search_text_index(&bytes[..bytes.len() - 8], "hello", 10),
            Err(TextIndexError::Container) | Err(TextIndexError::ChecksumMismatch)
        ));
        assert!(matches!(
            search_text_index(b"nope", "hello", 10),
            Err(TextIndexError::Container)
        ));
    }

    #[test]
    fn nested_json_strings_are_indexed() {
        let traces = vec![trace(
            "trace-nested",
            vec![span("trace-nested", "s1", "outer", "", None)],
        )];
        let mut traces = traces;
        traces[0].spans[0].input = Some(json!({
            "messages": [
                { "role": "user", "content": "explain the duplicated charge" }
            ]
        }));

        let bytes = build_text_index(&traces).unwrap();
        let hits = search_text_index(&bytes, "duplicated", 10).unwrap();

        assert_eq!(hits, vec!["trace-nested".to_string()]);
    }

    #[test]
    fn container_names_cannot_escape_the_extraction_directory() {
        assert!(safe_relative("meta.json").is_ok());
        assert!(safe_relative("../escape").is_err());
        assert!(safe_relative("/absolute").is_err());
        assert!(safe_relative("a/../../escape").is_err());
    }

    #[test]
    fn large_fields_are_clipped_not_rejected() {
        let long = "x".repeat(MAX_INDEXED_TEXT_BYTES + 10_000);
        let traces = vec![trace(
            "trace-long",
            vec![span("trace-long", "s1", &long, "", None)],
        )];

        let bytes = build_text_index(&traces).expect("oversized fields clip");
        assert!(!bytes.is_empty());
    }
}
