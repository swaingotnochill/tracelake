//! Property-based tests for the span storage key codec in `tracelake_core::key`.

use proptest::prelude::*;
use tracelake_core::{
    SpanKey, decode_span_key, encode_span_key, namespace_prefix, tenant_prefix, trace_prefix,
};

/// Small fixed alphabet for id segments: printable, no '/', deterministic.
const SEGMENT_ALPHABET: &[char] = &['a', 'b', 'z', '0', '9', '-', '_'];

/// One admissible id segment: non-empty, no '/', 1..=16 chars.
fn segment() -> impl Strategy<Value = String> {
    prop::collection::vec(proptest::sample::select(SEGMENT_ALPHABET), 1..=16)
        .prop_map(|chars| chars.into_iter().collect())
}

/// Any non-negative i64 encodes: the widest value (19 digits) still fits the
/// zero-padded width-20 timestamp segment.
fn timestamp() -> impl Strategy<Value = i64> {
    0i64..=i64::MAX
}

proptest! {
    /// K-RT: decode(encode(components)) recovers every component.
    #[test]
    fn encode_decode_round_trips_all_admissible_keys(
        tenant in segment(),
        namespace in segment(),
        trace in segment(),
        span in segment(),
        ts in timestamp(),
    ) {
        let key = encode_span_key(&tenant, &namespace, &trace, ts, &span)
            .expect("admissible segments should encode");
        let decoded = decode_span_key(&key)
            .expect("encoded keys should decode");

        prop_assert_eq!(
            decoded,
            SpanKey {
                tenant_id: tenant,
                namespace,
                trace_id: trace,
                start_time_ns: ts,
                span_id: span,
            }
        );
    }

    /// K-ORD: equal-width zero padding keeps byte order equal to
    /// (timestamp, span_id) order within one trace.
    #[test]
    fn keys_order_by_timestamp_then_span_id(
        tenant in segment(),
        namespace in segment(),
        trace in segment(),
        span_a in segment(),
        span_b in segment(),
        ts_a in timestamp(),
        ts_b in timestamp(),
    ) {
        let key_a = encode_span_key(&tenant, &namespace, &trace, ts_a, &span_a)
            .expect("admissible segments should encode");
        let key_b = encode_span_key(&tenant, &namespace, &trace, ts_b, &span_b)
            .expect("admissible segments should encode");

        if ts_a < ts_b {
            prop_assert!(key_a < key_b);
        } else if ts_a > ts_b {
            prop_assert!(key_b < key_a);
        } else if span_a < span_b {
            prop_assert!(key_a < key_b);
        } else if span_b < span_a {
            prop_assert!(key_b < key_a);
        } else {
            prop_assert_eq!(key_a, key_b);
        }
    }

    /// K-ISO: a span key never falls under another trace's prefix — the
    /// guarantee SlateDB prefix scans rely on for trace isolation.
    #[test]
    fn trace_prefixes_never_cross_into_other_traces(
        tenant in segment(),
        namespace in segment(),
        trace_a in segment(),
        trace_b in segment(),
        ts in timestamp(),
        span in segment(),
    ) {
        prop_assume!(trace_a != trace_b);

        let key_a = encode_span_key(&tenant, &namespace, &trace_a, ts, &span)
            .expect("admissible segments should encode");
        let key_b = encode_span_key(&tenant, &namespace, &trace_b, ts, &span)
            .expect("admissible segments should encode");
        let prefix_a = trace_prefix(&tenant, &namespace, &trace_a)
            .expect("admissible segments should encode");
        let prefix_b = trace_prefix(&tenant, &namespace, &trace_b)
            .expect("admissible segments should encode");

        prop_assert!(!key_a.starts_with(&prefix_b));
        prop_assert!(!key_b.starts_with(&prefix_a));
        prop_assert!(key_a.starts_with(&prefix_a));
        prop_assert!(key_b.starts_with(&prefix_b));
    }

    /// K-PFX: prefixes nest — every span key carries its tenant, namespace,
    /// and trace prefixes, and the trace prefix nests inside the namespace
    /// prefix, which nests inside the tenant prefix.
    #[test]
    fn encoded_keys_nest_tenant_namespace_and_trace_prefixes(
        tenant in segment(),
        namespace in segment(),
        trace in segment(),
        ts in timestamp(),
        span in segment(),
    ) {
        let key = encode_span_key(&tenant, &namespace, &trace, ts, &span)
            .expect("admissible segments should encode");
        let tenant_pfx = tenant_prefix(&tenant).expect("admissible segments should encode");
        let namespace_pfx = namespace_prefix(&tenant, &namespace)
            .expect("admissible segments should encode");
        let trace_pfx = trace_prefix(&tenant, &namespace, &trace)
            .expect("admissible segments should encode");

        prop_assert!(key.starts_with(&trace_pfx));
        prop_assert!(trace_pfx.starts_with(&namespace_pfx));
        prop_assert!(namespace_pfx.starts_with(&tenant_pfx));
    }

    /// K-REJ: the decoder is total — arbitrary bytes return Ok or Err, never
    /// panic — and any key it accepts is a fixpoint of encode/decode.
    #[test]
    fn decode_never_panics_and_reaccepts_what_it_returns(
        bytes in prop::collection::vec(any::<u8>(), 0..=64),
    ) {
        if let Ok(decoded) = decode_span_key(&bytes) {
            let re_encoded = encode_span_key(
                &decoded.tenant_id,
                &decoded.namespace,
                &decoded.trace_id,
                decoded.start_time_ns,
                &decoded.span_id,
            )
            .expect("decoded segments re-encode");
            prop_assert_eq!(
                decode_span_key(&re_encoded).expect("re-encoded keys decode"),
                decoded
            );
        }
    }
}
