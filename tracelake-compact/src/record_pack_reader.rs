use crate::compactor::CompactError;
use slatedb::object_store::{ObjectStore, path::Path};
use std::sync::Arc;
use tracelake_core::{Record, TableSchema, decode_record_pack};

/// Reads records back from record packs on the object store.
#[derive(Clone)]
pub struct RecordPackReader {
    store: Arc<dyn ObjectStore>,
}

impl RecordPackReader {
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self { store }
    }

    /// Reads every record in one pack, materializing only `fields`.
    pub async fn read_pack(
        &self,
        pack_path: &str,
        schema: &TableSchema,
        fields: &[&str],
    ) -> Result<Vec<Record>, CompactError> {
        let bytes = self
            .store
            .get(&Path::from(pack_path))
            .await?
            .bytes()
            .await?;

        Ok(decode_record_pack(&bytes)?.scan(schema, fields)?)
    }
}
