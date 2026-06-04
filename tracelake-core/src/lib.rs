pub mod span;

pub mod key;
pub use key::{
    KeyError, SpanKey, decode_span_key, encode_span_key, namespace_prefix, tenant_prefix,
    trace_prefix,
};

pub use span::{EvalScore, Span, SpanStatus, SpanType, TokenUsage, Trace};
