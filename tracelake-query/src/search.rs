//! `search_traces()`: filter traces by span status, model, and time range,
//! merging the hot SlateDB store with compacted segments.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use slatedb::object_store::path::Path;
use std::collections::{HashMap, HashSet};
use tracelake_compact::{build_text_index, group_spans, search_text_index};
use tracelake_core::{
    SegmentEntry, SegmentFileKind, SegmentStats, Span, SpanStatus, Trace, decode_trace_pack,
    namespace_prefix,
};

use crate::trace_fetch::{QueryError, TraceLakeReader, tombstone_ids};

/// Default result cap when the caller does not set one.
pub const DEFAULT_SEARCH_LIMIT: usize = 100;

/// Trace search filters. A span matches when it satisfies every filter that
/// is set; a trace matches when at least one of its spans matches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceSearch {
    /// Inclusive lower bound on span `start_time`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_start: Option<DateTime<Utc>>,
    /// Inclusive upper bound on span `start_time`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_end: Option<DateTime<Utc>>,
    /// Match spans with exactly this status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<SpanStatus>,
    /// Match spans whose model equals this string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Full-text query over span name, input, output, and error. A trace
    /// matches when any of its spans matches the query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_query: Option<String>,
    /// Maximum number of traces to return, most recent match first.
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    DEFAULT_SEARCH_LIMIT
}

impl Default for TraceSearch {
    fn default() -> Self {
        Self {
            time_start: None,
            time_end: None,
            status: None,
            model: None,
            text_query: None,
            limit: DEFAULT_SEARCH_LIMIT,
        }
    }
}

/// One matching trace: enough to list it in a UI without fetching spans.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceSummary {
    pub trace_id: String,
    /// Every span of the trace, deduplicated across sources.
    pub span_count: u64,
    /// Spans that satisfied all filters.
    pub matched_spans: u64,
    /// Earliest matching span `start_time`.
    pub first_match_time: DateTime<Utc>,
    /// Latest matching span `start_time`.
    pub last_match_time: DateTime<Utc>,
    /// Models on matching spans, sorted and deduplicated.
    pub models: Vec<String>,
    /// First error message among matching spans, in span order.
    pub error: Option<String>,
}

impl TraceLakeReader {
    /// Searches traces by span status, model, and time range.
    ///
    /// Segments whose stats cannot overlap the time filter are skipped
    /// without reading their packs. Remaining packs are read in full and
    /// their spans merged with the hot store — on a span_id conflict the hot
    /// span wins and newer segments override older ones, mirroring
    /// [`TraceLakeReader::get_trace`]. Results are ordered by latest matching
    /// span time, most recent first, and truncated to [`TraceSearch::limit`].
    pub async fn search_traces(
        &self,
        tenant_id: &str,
        namespace: &str,
        search: &TraceSearch,
    ) -> Result<Vec<TraceSummary>, QueryError> {
        namespace_prefix(tenant_id, namespace)?;

        let manifest =
            tracelake_compact::read_manifest(&self.store, &self.base_path, tenant_id, namespace)
                .await?;
        let hot_spans = self.db.get_namespace_spans(tenant_id, namespace).await?;
        let deleted = tombstone_ids(
            manifest.as_ref(),
            self.db.scan_trace_tombstones(tenant_id, namespace).await?,
        );
        let text_hits = self
            .text_hits(manifest.as_ref(), &hot_spans, search)
            .await?;

        let mut by_trace: HashMap<String, HashMap<String, Span>> = HashMap::new();
        let mut keep = |span: Span| {
            if deleted.contains(&span.trace_id) {
                return;
            }
            if let Some(hits) = &text_hits
                && !hits.contains(&span.trace_id)
            {
                return;
            }
            by_trace
                .entry(span.trace_id.clone())
                .or_default()
                .insert(span.span_id.clone(), span);
        };

        if let Some(manifest) = manifest {
            for entry in &manifest.segments {
                if !stats_may_overlap(&entry.stats, search) {
                    continue;
                }

                for spans in self.read_segment_traces(entry).await? {
                    for span in spans {
                        keep(span);
                    }
                }
            }
        }

        // Hot spans are inserted last so they override compacted copies.
        for span in hot_spans {
            keep(span);
        }

        let mut summaries: Vec<TraceSummary> = by_trace
            .iter()
            .filter_map(|(trace_id, spans)| summarize_trace(trace_id, spans, search))
            .collect();

        summaries.sort_by(|a, b| {
            b.last_match_time
                .cmp(&a.last_match_time)
                .then_with(|| a.trace_id.cmp(&b.trace_id))
        });
        summaries.truncate(search.limit);

        Ok(summaries)
    }

    /// Trace ids matched by the text query, over compacted segments and the
    /// hot store. `None` when the query has no text term.
    async fn text_hits(
        &self,
        manifest: Option<&tracelake_core::SegmentManifest>,
        hot_spans: &[Span],
        search: &TraceSearch,
    ) -> Result<Option<HashSet<String>>, QueryError> {
        let Some(text_query) = &search.text_query else {
            return Ok(None);
        };

        let mut hits = HashSet::new();
        if let Some(manifest) = manifest {
            for entry in &manifest.segments {
                if !stats_may_overlap(&entry.stats, search) {
                    continue;
                }
                hits.extend(
                    self.search_segment_text(entry, text_query, search.limit)
                        .await?,
                );
            }
        }

        hits.extend(search_hot_text(hot_spans, text_query, search.limit)?);

        Ok(Some(hits))
    }

    async fn search_segment_text(
        &self,
        entry: &SegmentEntry,
        text_query: &str,
        limit: usize,
    ) -> Result<HashSet<String>, QueryError> {
        let text_file = entry
            .files
            .iter()
            .find(|file| file.kind == SegmentFileKind::Text)
            .ok_or(QueryError::MissingSegmentFile {
                segment_id: entry.segment_id.clone(),
                kind: SegmentFileKind::Text.extension(),
            })?;

        let bytes = self
            .store
            .get(&Path::from(text_file.path.as_str()))
            .await?
            .bytes()
            .await?;

        Ok(search_text_index(&bytes, text_query, limit)?
            .into_iter()
            .collect())
    }

    /// Reads every trace of one segment pack. The pack is fetched and
    /// validated once, then each directory entry is decoded.
    async fn read_segment_traces(
        &self,
        entry: &SegmentEntry,
    ) -> Result<Vec<Vec<Span>>, QueryError> {
        let pack_file = entry
            .files
            .iter()
            .find(|file| file.kind == SegmentFileKind::TracePack)
            .ok_or(QueryError::MissingSegmentFile {
                segment_id: entry.segment_id.clone(),
                kind: SegmentFileKind::TracePack.extension(),
            })?;

        let bytes = self
            .store
            .get(&Path::from(pack_file.path.as_str()))
            .await?
            .bytes()
            .await?;

        let pack = decode_trace_pack(&bytes)?;
        let mut traces = Vec::with_capacity(pack.trace_count());
        for trace_id in pack.trace_ids()? {
            traces.push(pack.read_trace(&trace_id)?.unwrap_or_default());
        }

        Ok(traces)
    }
}

/// Builds a throwaway index over the hot store so the newest spans are
/// searchable before compaction. Hot data is memtable-sized, and compaction
/// moves it into an indexed segment.
fn search_hot_text(
    hot_spans: &[Span],
    text_query: &str,
    limit: usize,
) -> Result<HashSet<String>, QueryError> {
    if hot_spans.is_empty() {
        return Ok(HashSet::new());
    }

    let traces: Vec<Trace> = group_spans(hot_spans.to_vec())
        .into_iter()
        .flat_map(|group| group.traces)
        .collect();
    let bytes = build_text_index(&traces)?;

    Ok(search_text_index(&bytes, text_query, limit)?
        .into_iter()
        .collect())
}

/// Whether a segment's stats leave any possibility of a matching span for
/// this search. Missing stats never prune.
fn stats_may_overlap(stats: &SegmentStats, search: &TraceSearch) -> bool {
    if let Some(query_start) = search.time_start
        && let Some(segment_max) = stats.start_time_max
        && segment_max < query_start
    {
        return false;
    }
    if let Some(query_end) = search.time_end
        && let Some(segment_min) = stats.start_time_min
        && segment_min > query_end
    {
        return false;
    }

    true
}

/// Whether one span satisfies every set filter.
fn span_matches(span: &Span, search: &TraceSearch) -> bool {
    if let Some(start) = search.time_start
        && span.start_time < start
    {
        return false;
    }
    if let Some(end) = search.time_end
        && span.start_time > end
    {
        return false;
    }
    if let Some(status) = &search.status
        && &span.status != status
    {
        return false;
    }
    if let Some(model) = &search.model
        && span.model.as_deref() != Some(model.as_str())
    {
        return false;
    }

    true
}

/// Builds one trace's summary from its deduplicated spans, or `None` when no
/// span matches the search.
fn summarize_trace(
    trace_id: &str,
    spans: &HashMap<String, Span>,
    search: &TraceSearch,
) -> Option<TraceSummary> {
    let mut matched: Vec<&Span> = spans
        .values()
        .filter(|span| span_matches(span, search))
        .collect();
    if matched.is_empty() {
        return None;
    }

    matched.sort_by(|a, b| {
        a.start_time
            .cmp(&b.start_time)
            .then_with(|| a.span_id.cmp(&b.span_id))
    });

    let mut models: Vec<String> = {
        let unique: HashSet<&str> = matched
            .iter()
            .filter_map(|span| span.model.as_deref())
            .collect();
        unique.into_iter().map(str::to_string).collect()
    };
    models.sort();

    Some(TraceSummary {
        trace_id: trace_id.to_string(),
        span_count: spans.len() as u64,
        matched_spans: matched.len() as u64,
        first_match_time: matched[0].start_time,
        last_match_time: matched[matched.len() - 1].start_time,
        models,
        error: matched.iter().find_map(|span| span.error.clone()),
    })
}

#[cfg(test)]
mod verification;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;
    use slatedb::object_store::ObjectStore;
    use slatedb::object_store::memory::InMemory;
    use std::sync::Arc;
    use tracelake_compact::Compactor;
    use tracelake_core::SpanType;
    use tracelake_ingest::TraceLakeDb;

    const BASE_PATH: &str = "tracelake/segments";

    fn make_span(trace_id: &str, span_id: &str, nanos: i64) -> Span {
        Span {
            tenant_id: "tenantid-1".to_string(),
            namespace: "default".to_string(),
            trace_id: trace_id.to_string(),
            span_id: span_id.to_string(),
            parent_span_id: None,
            span_type: SpanType::Llm,
            name: span_id.to_string(),
            start_time: Utc.with_ymd_and_hms(2026, 6, 16, 10, 0, 0).unwrap()
                + chrono::Duration::nanoseconds(nanos),
            end_time: None,
            status: SpanStatus::Ok,
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

    /// Compacts `spans` through a fresh hot store, giving each batch its own
    /// segment.
    async fn compact_spans(store: &Arc<dyn ObjectStore>, spans: &[Span]) {
        let db = TraceLakeDb::open_in_memory("/search-compact-source")
            .await
            .expect("compaction source db should open");
        db.put_spans(spans).await.expect("spans should write");
        Compactor::new(Arc::clone(store), BASE_PATH)
            .compact(&db)
            .await
            .expect("compaction should succeed");
    }

    /// A reader over a fresh store whose hot store holds `hot`.
    async fn reader_with_hot(hot: &[Span]) -> TraceLakeReader {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let db = TraceLakeDb::open_in_memory("/search-reader")
            .await
            .expect("db should open");
        if !hot.is_empty() {
            db.put_spans(hot).await.expect("hot spans should write");
        }
        TraceLakeReader::new(db, store, BASE_PATH)
    }

    fn failed_search() -> TraceSearch {
        TraceSearch {
            status: Some(SpanStatus::Error),
            ..TraceSearch::default()
        }
    }

    #[tokio::test]
    async fn search_returns_empty_when_no_source_holds_matches() {
        let reader = reader_with_hot(&[]).await;

        let summaries = reader
            .search_traces("tenantid-1", "default", &failed_search())
            .await
            .expect("empty search should not error");

        assert!(summaries.is_empty());
    }

    #[tokio::test]
    async fn search_finds_failed_traces_in_the_hot_store() {
        let mut failed_root = make_span("trace-failed", "span-a", 1_000_000_000);
        failed_root.status = SpanStatus::Error;
        failed_root.error = Some("model timeout".to_string());
        failed_root.model = Some("gpt-4o".to_string());

        let mut failed_child = make_span("trace-failed", "span-b", 2_000_000_000);
        failed_child.status = SpanStatus::Error;
        failed_child.error = Some("child crashed".to_string());
        failed_child.model = Some("claude-3".to_string());

        let mut ok_trace = make_span("trace-ok", "span-a", 3_000_000_000);
        ok_trace.model = Some("gpt-4o".to_string());

        let reader = reader_with_hot(&[failed_root, failed_child, ok_trace]).await;

        let summaries = reader
            .search_traces("tenantid-1", "default", &failed_search())
            .await
            .expect("failed search should succeed");

        assert_eq!(summaries.len(), 1);
        let summary = &summaries[0];
        assert_eq!(summary.trace_id, "trace-failed");
        assert_eq!(summary.span_count, 2);
        assert_eq!(summary.matched_spans, 2);
        assert_eq!(
            summary.models,
            vec!["claude-3".to_string(), "gpt-4o".to_string()]
        );
        assert_eq!(summary.error.as_deref(), Some("model timeout"));
    }

    #[tokio::test]
    async fn search_filters_by_model() {
        let mut gpt = make_span("trace-gpt", "span-a", 1);
        gpt.model = Some("gpt-4o".to_string());
        let mut claude = make_span("trace-claude", "span-a", 2);
        claude.model = Some("claude-3".to_string());

        let reader = reader_with_hot(&[gpt, claude]).await;

        let summaries = reader
            .search_traces(
                "tenantid-1",
                "default",
                &TraceSearch {
                    model: Some("claude-3".to_string()),
                    ..TraceSearch::default()
                },
            )
            .await
            .expect("model search should succeed");

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].trace_id, "trace-claude");
    }

    #[tokio::test]
    async fn search_time_range_uses_stats_to_skip_segments() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        let second = 1_000_000_000i64;
        let early = vec![
            make_span("trace-early", "span-a", 1),
            make_span("trace-early-2", "span-a", 2),
        ];
        compact_spans(&store, &early).await;

        let late: Vec<Span> = (0..2)
            .map(|i| make_span(&format!("trace-late-{i}"), "span-a", second + i + 1))
            .collect();
        compact_spans(&store, &late).await;

        // Delete the late pack: the query window only touches the early
        // segment, so a working stats prune never reads it.
        let manifest = tracelake_compact::read_manifest(&store, BASE_PATH, "tenantid-1", "default")
            .await
            .expect("manifest should read")
            .expect("manifest should exist");
        let late_pack = manifest.segments[1]
            .files
            .iter()
            .find(|file| file.kind == SegmentFileKind::TracePack)
            .expect("pack should exist");
        store
            .delete(&Path::from(late_pack.path.as_str()))
            .await
            .expect("late pack should delete");

        let db = TraceLakeDb::open_in_memory("/search-stats-prune")
            .await
            .expect("db should open");
        let reader = TraceLakeReader::new(db, Arc::clone(&store), BASE_PATH);

        let window = TraceSearch {
            time_start: Some(Utc.with_ymd_and_hms(2026, 6, 16, 9, 59, 59).unwrap()),
            time_end: Some(Utc.with_ymd_and_hms(2026, 6, 16, 10, 0, 1).unwrap()),
            ..TraceSearch::default()
        };
        let summaries = reader
            .search_traces("tenantid-1", "default", &window)
            .await
            .expect("pruned search must not read the late pack");

        let mut ids: Vec<&str> = summaries.iter().map(|s| s.trace_id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["trace-early", "trace-early-2"]);
    }

    #[tokio::test]
    async fn search_prefers_hot_span_versions_over_compacted() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        let original = make_span("trace-1", "span-a", 1);
        compact_spans(&store, std::slice::from_ref(&original)).await;

        // The same span re-written with a newer status after compaction.
        let mut updated = original.clone();
        updated.status = SpanStatus::Error;
        updated.error = Some("failed on retry".to_string());

        let db = TraceLakeDb::open_in_memory("/search-hot-precedence")
            .await
            .expect("db should open");
        db.put_spans(std::slice::from_ref(&updated))
            .await
            .expect("hot span should write");

        let reader = TraceLakeReader::new(db, Arc::clone(&store), BASE_PATH);
        let summaries = reader
            .search_traces("tenantid-1", "default", &failed_search())
            .await
            .expect("precedence search should succeed");

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].trace_id, "trace-1");
        assert_eq!(
            summaries[0].span_count, 1,
            "hot copy should dedup the compacted one"
        );
    }

    #[tokio::test]
    async fn search_merges_matches_across_compacted_and_hot() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        let mut compacted = make_span("trace-1", "span-a", 1);
        compacted.status = SpanStatus::Error;
        compacted.error = Some("compacted failure".to_string());
        compact_spans(&store, std::slice::from_ref(&compacted)).await;

        let mut hot = make_span("trace-1", "span-b", 2);
        hot.status = SpanStatus::Error;

        let db = TraceLakeDb::open_in_memory("/search-merge")
            .await
            .expect("db should open");
        db.put_spans(std::slice::from_ref(&hot))
            .await
            .expect("hot span should write");

        let reader = TraceLakeReader::new(db, Arc::clone(&store), BASE_PATH);
        let summaries = reader
            .search_traces("tenantid-1", "default", &failed_search())
            .await
            .expect("merged search should succeed");

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].span_count, 2);
        assert_eq!(summaries[0].matched_spans, 2);
    }

    #[tokio::test]
    async fn search_returns_most_recent_matches_first_up_to_limit() {
        let spans: Vec<Span> = (0..5)
            .map(|i| make_span(&format!("trace-{i}"), "span-a", (5 - i) as i64))
            .collect();
        let reader = reader_with_hot(&spans).await;

        let summaries = reader
            .search_traces(
                "tenantid-1",
                "default",
                &TraceSearch {
                    limit: 2,
                    ..TraceSearch::default()
                },
            )
            .await
            .expect("limit search should succeed");

        let ids: Vec<&str> = summaries.iter().map(|s| s.trace_id.as_str()).collect();
        // trace-0's span is the latest (5ns), so most-recent-first puts it
        // first.
        assert_eq!(ids, vec!["trace-0", "trace-1"]);
    }

    #[tokio::test]
    async fn search_over_1000_traces_finds_exactly_the_failed_ones() {
        const TRACE_COUNT: usize = 1_000;
        const SPANS_PER_TRACE: usize = 3;

        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        let mut spans = Vec::with_capacity(TRACE_COUNT * SPANS_PER_TRACE);
        let mut expected_failed: Vec<String> = Vec::new();
        for trace_num in 0..TRACE_COUNT {
            let trace_id = format!("trace-{trace_num:04}");
            let failed = trace_num % 7 == 3;
            if failed {
                expected_failed.push(trace_id.clone());
            }

            for span_num in 0..SPANS_PER_TRACE {
                let mut span = make_span(
                    &trace_id,
                    &format!("span-{span_num}"),
                    (trace_num * SPANS_PER_TRACE + span_num) as i64 * 1_000_000,
                );
                span.model = Some(
                    if trace_num % 2 == 0 {
                        "gpt-4o"
                    } else {
                        "claude-3"
                    }
                    .to_string(),
                );
                if failed && span_num == 1 {
                    span.status = SpanStatus::Error;
                    span.error = Some(format!("failure in {trace_id}"));
                }
                spans.push(span);
            }
        }
        compact_spans(&store, &spans).await;

        let db = TraceLakeDb::open_in_memory("/search-1000")
            .await
            .expect("db should open");
        let reader = TraceLakeReader::new(db, Arc::clone(&store), BASE_PATH);

        let summaries = reader
            .search_traces(
                "tenantid-1",
                "default",
                &TraceSearch {
                    status: Some(SpanStatus::Error),
                    limit: TRACE_COUNT,
                    ..TraceSearch::default()
                },
            )
            .await
            .expect("failed search should succeed");

        // Results come back most recent first: highest trace number first.
        let mut expected: Vec<&str> = expected_failed.iter().map(String::as_str).collect();
        expected.reverse();
        let found: Vec<&str> = summaries.iter().map(|s| s.trace_id.as_str()).collect();
        assert_eq!(
            found, expected,
            "search must return exactly the failed traces"
        );

        for summary in &summaries {
            assert_eq!(summary.span_count, SPANS_PER_TRACE as u64);
            assert_eq!(summary.matched_spans, 1);
            assert_eq!(
                summary.error.as_deref(),
                Some(format!("failure in {}", summary.trace_id).as_str())
            );
        }
    }

    #[tokio::test]
    async fn text_search_finds_compacted_traces_by_prompt_and_output() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        let mut hallucination = make_span("trace-hallucination", "span-a", 1);
        hallucination.input = Some(json!({ "prompt": "explain the refund policy" }));
        hallucination.output = Some(json!({ "text": "the agent invented a 90 day window" }));

        let mut refund = make_span("trace-refund", "span-a", 2);
        refund.output = Some(json!({ "text": "refunds post within five business days" }));

        compact_spans(&store, &[hallucination, refund]).await;

        let db = TraceLakeDb::open_in_memory("/search-text")
            .await
            .expect("db should open");
        let reader = TraceLakeReader::new(db, Arc::clone(&store), BASE_PATH);

        let summaries = reader
            .search_traces(
                "tenantid-1",
                "default",
                &TraceSearch {
                    text_query: Some("invented".to_string()),
                    ..TraceSearch::default()
                },
            )
            .await
            .expect("text search should succeed");

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].trace_id, "trace-hallucination");
    }

    #[tokio::test]
    async fn text_search_covers_hot_spans_before_compaction() {
        let mut hot = make_span("trace-hot", "span-a", 1);
        hot.error = Some("tool timeout after 30s".to_string());
        let reader = reader_with_hot(&[hot]).await;

        let summaries = reader
            .search_traces(
                "tenantid-1",
                "default",
                &TraceSearch {
                    text_query: Some("timeout".to_string()),
                    ..TraceSearch::default()
                },
            )
            .await
            .expect("hot text search should succeed");

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].trace_id, "trace-hot");
    }

    #[tokio::test]
    async fn text_search_combines_with_structural_filters() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        let mut failed = make_span("trace-failed", "span-a", 1);
        failed.status = SpanStatus::Error;
        failed.output = Some(json!({ "text": "hallucination in the answer" }));
        let mut ok = make_span("trace-ok", "span-a", 2);
        ok.output = Some(json!({ "text": "hallucination in the answer" }));

        compact_spans(&store, &[failed, ok]).await;
        let db = TraceLakeDb::open_in_memory("/search-text-filter")
            .await
            .expect("db should open");
        let reader = TraceLakeReader::new(db, Arc::clone(&store), BASE_PATH);

        let summaries = reader
            .search_traces(
                "tenantid-1",
                "default",
                &TraceSearch {
                    text_query: Some("hallucination".to_string()),
                    status: Some(SpanStatus::Error),
                    ..TraceSearch::default()
                },
            )
            .await
            .expect("combined search should succeed");

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].trace_id, "trace-failed");
    }

    #[tokio::test]
    #[ignore = "builds 10 segments over 100K spans; run in release for meaningful numbers"]
    async fn bench_search_latency_across_segments() {
        const TRACE_COUNT: usize = 10_000;
        const SPANS_PER_TRACE: usize = 10;
        const BATCH_COUNT: usize = 10;
        const TRACES_PER_BATCH: usize = TRACE_COUNT / BATCH_COUNT;
        const SECOND: i64 = 1_000_000_000;

        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        fn bench_span(trace_num: usize, span_num: usize) -> Span {
            // Each batch sits in its own one-second window, so segment stats
            // can prune; spans within a batch are nanoseconds apart.
            let batch = trace_num / TRACES_PER_BATCH;
            let within = ((trace_num % TRACES_PER_BATCH) * SPANS_PER_TRACE + span_num) as i64;
            let mut span = make_span(
                &format!("trace-{trace_num:05}"),
                &format!("span-{span_num:03}"),
                batch as i64 * SECOND + within,
            );
            if trace_num % 9 == 4 && span_num == 0 {
                span.status = SpanStatus::Error;
                span.error = Some("synthetic failure".to_string());
            }
            span.model = Some(
                if trace_num.is_multiple_of(2) {
                    "gpt-4o"
                } else {
                    "claude-3"
                }
                .to_string(),
            );
            span.input = Some(json!({
                "prompt": "The user asks the support agent to explain the refund policy "
                    .repeat(6)
            }));
            span.output = Some(json!({
                "text": "The agent explains that refunds post within five business days "
                    .repeat(6)
            }));
            span
        }

        // Hot phase: 100K spans in SlateDB, no segments yet.
        let spans: Vec<Span> = (0..TRACE_COUNT)
            .flat_map(|trace_num| {
                (0..SPANS_PER_TRACE).map(move |span_num| bench_span(trace_num, span_num))
            })
            .collect();

        let hot_db = TraceLakeDb::open_in_memory("/bench-search-hot")
            .await
            .expect("hot db should open");
        hot_db.put_spans(&spans).await.expect("spans should write");
        let hot_reader = TraceLakeReader::new(hot_db, Arc::clone(&store), BASE_PATH);

        let hot_started = std::time::Instant::now();
        let hot_summaries = hot_reader
            .search_traces(
                "tenantid-1",
                "default",
                &TraceSearch {
                    status: Some(SpanStatus::Error),
                    limit: 100,
                    ..TraceSearch::default()
                },
            )
            .await
            .expect("hot search should succeed");
        let hot_elapsed = hot_started.elapsed();
        assert_eq!(hot_summaries.len(), 100);

        // Cold phase: compact in batches, each in its own time window.
        for batch in 0..BATCH_COUNT {
            let batch_spans: Vec<Span> = ((batch * TRACES_PER_BATCH)
                ..((batch + 1) * TRACES_PER_BATCH))
                .flat_map(|trace_num| {
                    (0..SPANS_PER_TRACE).map(move |span_num| bench_span(trace_num, span_num))
                })
                .collect();
            compact_spans(&store, &batch_spans).await;
        }

        let cold_db = TraceLakeDb::open_in_memory("/bench-search-cold")
            .await
            .expect("cold db should open");
        let cold_reader = TraceLakeReader::new(cold_db, Arc::clone(&store), BASE_PATH);

        let full_scan = TraceSearch {
            status: Some(SpanStatus::Error),
            limit: 100,
            ..TraceSearch::default()
        };
        let full_started = std::time::Instant::now();
        let full_summaries = cold_reader
            .search_traces("tenantid-1", "default", &full_scan)
            .await
            .expect("full scan should succeed");
        let full_elapsed = full_started.elapsed();
        assert_eq!(full_summaries.len(), 100);

        // Window over exactly one batch: the other nine segments are
        // pruned by stats and never read.
        let base = Utc.with_ymd_and_hms(2026, 6, 16, 10, 0, 0).unwrap();
        let pruned = TraceSearch {
            time_start: Some(base + chrono::Duration::seconds(1)),
            time_end: Some(base + chrono::Duration::seconds(1) + chrono::Duration::milliseconds(1)),
            status: Some(SpanStatus::Error),
            limit: 100,
            ..TraceSearch::default()
        };
        let pruned_started = std::time::Instant::now();
        let pruned_summaries = cold_reader
            .search_traces("tenantid-1", "default", &pruned)
            .await
            .expect("pruned search should succeed");
        let pruned_elapsed = pruned_started.elapsed();
        assert!(!pruned_summaries.is_empty());

        println!(
            "search bench: {TRACE_COUNT} traces x {SPANS_PER_TRACE} spans over \
             {BATCH_COUNT} segments (disjoint 1s windows), in-memory object store"
        );
        println!(
            "hot_error_scan={hot_elapsed:?} cold_full_scan={full_elapsed:?} \
             cold_stats_pruned_1_segment={pruned_elapsed:?}"
        );
    }
}
