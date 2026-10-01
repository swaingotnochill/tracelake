use crate::compactor::CompactError;
use slatedb::object_store::{ObjectStore, path::Path};
use std::sync::Arc;
use tracelake_core::{Span, decode_trace_pack};

/// Reads traces back from segment trace packs on the object store.
pub struct TracePackWriter {
    store: Arc<dyn ObjectStore>,
}

impl TracePackWriter {
    /// Creates a reader over one object store.
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self { store }
    }

    /// Reads one trace from a pack object, or `None` when the pack does not
    /// hold the trace.
    pub async fn read_trace(
        &self,
        pack_path: &str,
        trace_id: &str,
    ) -> Result<Option<Vec<Span>>, CompactError> {
        let path = Path::from(pack_path);
        let bytes = self.store.get(&path).await?.bytes().await?;

        let pack = decode_trace_pack(&bytes)?;
        Ok(pack.read_trace(trace_id)?)
    }
}
