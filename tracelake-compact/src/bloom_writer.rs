use crate::compactor::TraceGroup;
use tracelake_core::{BloomFilter, DEFAULT_FALSE_POSITIVE_RATE, Record, RecordError, TableSchema};

/// Bloom over record ids, so a point lookup can skip a pack.
pub fn build_record_bloom(
    schema: &TableSchema,
    records: &[Record],
) -> Result<Vec<u8>, RecordError> {
    let mut filter = BloomFilter::new(records.len(), DEFAULT_FALSE_POSITIVE_RATE)
        .expect("the default false positive rate is always between 0 and 1");

    for record in records {
        filter.insert(schema.record_id(record)?.as_bytes());
    }

    Ok(filter.encode())
}

/// Builds a segment's bloom filter over every trace_id, span_id, and tag in
/// the group, serialized and ready to write as the `.bloom` file.
pub fn build_segment_bloom(group: &TraceGroup) -> Vec<u8> {
    let item_count: usize = group
        .traces
        .iter()
        .map(|trace| {
            1 + trace
                .spans
                .iter()
                .map(|span| 1 + span.tags.len())
                .sum::<usize>()
        })
        .sum();

    let mut filter = BloomFilter::new(item_count, DEFAULT_FALSE_POSITIVE_RATE)
        .expect("the default false positive rate is always between 0 and 1");

    for trace in &group.traces {
        filter.insert(trace.trace_id.as_bytes());

        for span in &trace.spans {
            filter.insert(span.span_id.as_bytes());

            for tag in &span.tags {
                filter.insert(tag.as_bytes());
            }
        }
    }

    filter.encode()
}
