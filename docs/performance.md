# Performance review

Reviewed on 2026-10-31 with `tracelake-bench`. The review found one defect worth
fixing immediately: a cold trace fetch cost grew with segment size instead of
trace size. It is fixed.

## Method

- Hardware: Apple Silicon, 8 cores, macOS, load average about 10 during the runs
  because other work was running. Absolute times are inflated; ratios hold.
- Build: `--release`.
- Store: in-memory object store, so timings include serialization, decoding,
  indexing, and range copies. They exclude network latency. A production S3 run
  adds one GET per range, roughly 20 to 80 ms on a cold connection.
- Dataset: 1,000 traces by 10 spans, 2 KB payload per input and output field,
  about 46 MB serialized.
- Each run executes every phase once, in order, and each phase asserts its row
  and trace counts, so a timing cannot describe work that did not happen.
- Three runs: 2026-10-31, same binary, same machine.

## Results

| Phase | Median | Range |
|-------|--------|-------|
| Ingest, one batch | 176 ms, 56.9k spans/s | 174 to 275 ms |
| Compact (pack, bloom, stats, Tantivy, Parquet) | 957 ms, 10.4k spans/s | 921 to 1,008 ms |
| Fetch cold, first trace | 0.36 ms | 0.24 to 0.39 ms |
| Fetch cold, warm loop | 169 us per trace | 160 to 171 us |
| Fetch hot | 153 us per trace | 153 to 165 us |
| Search cold, status filter | 123 ms for 100 traces | 118 to 135 ms |
| Text search cold | 122 ms for 100 traces | 113 to 123 ms |
| Aggregate spans, group by model | 10.9 ms | 9.9 to 36.6 ms |

The aggregate range is wide because the first run paid one-time setup. The
suite's own correctness counts: 6 matching traces found by the status search
with a 100-trace limit, 2 model groups in the aggregate.

## The defect and the fix

Before the review, a cold fetch read and checksummed the entire segment pack. A
46 MB pack took 9.9 ms per trace. Scaling the corpus confirmed the limiter:

| Corpus | Pack size | Cold fetch per trace |
|--------|-----------|----------------------|
| 100 traces | 4 MB | 1.15 ms |
| 500 traces | 23 MB | 5.1 ms |
| 1,000 traces | 46 MB | 9.9 ms |

The cost was linear in pack size and independent of trace size, which is exactly
wrong for the primary query.

The fix has two parts. Each trace block now carries its own CRC32 in the pack
directory, and `TracePackWriter::read_trace` fetches three ranges instead of the
whole object: the header, the directory, and the one block that holds the trace.
The block checksum is verified after the range read. A corrupted block is
reported even when the whole-pack footer has been restamped, which a unit test
pins.

After the fix, the same fetch is 0.14 to 0.17 ms and does not grow with the pack:

| Corpus | Pack size | Cold fetch per trace (after) |
|--------|-----------|------------------------------|
| 100 traces | 4 MB | 160 us |
| 1,000 traces | 46 MB | 135 us |

That is about 60 times faster at 46 MB, and the cost now follows the trace.

## Remaining limits

- **Search reads whole packs.** A structural search decodes every matching
  segment pack, so its cost follows segment size. Pruning by stats and time is
  what keeps it bounded. Column files and per-field indexes would remove this.
- **Compaction is single-threaded and rescans the hot store.** One compactor
  run builds one segment, so segment size follows the hot buffer. The
  compaction watermark is the top alpha fix.
- **Text search over hot data builds an index per query.** It is correct, not
  cheap. Compaction moves data into indexed segments, after which text search
  reads one segment index object per segment.
- **Aggregation has no filter pushdown.** A filtered aggregate reads every
  column file for the table.
- **No query timeout or concurrency cap.** A broad query can occupy a core for
  seconds.

## Reproduce

```bash
cargo run --release -p tracelake-bench
cargo run --release -p tracelake-bench -- --traces 100 --spans 10 --payload 2048
```

The output states the dataset shape, the elapsed time, the rate, and a
correctness count for each phase.
