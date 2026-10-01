use crate::compactor::CompactError;
use slatedb::object_store::{ObjectStore, path::Path};
use std::sync::Arc;
use tracelake_core::SegmentManifest;

/// Manifest file name inside a (tenant, namespace) directory.
const MANIFEST_FILE: &str = "manifest.json";

/// Reads the manifest for one (tenant, namespace), or `None` when nothing has
/// been compacted there yet.
pub async fn read_manifest(
    store: &Arc<dyn ObjectStore>,
    base_path: &str,
    tenant_id: &str,
    namespace: &str,
) -> Result<Option<SegmentManifest>, CompactError> {
    let path = manifest_path(base_path, tenant_id, namespace);

    match store.get(&Path::from(path.as_str())).await {
        Ok(result) => {
            let bytes = result.bytes().await?;
            Ok(Some(serde_json::from_slice(&bytes)?))
        }
        Err(slatedb::object_store::Error::NotFound { .. }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Publishes a manifest with a single PUT; this is the atomic point where
/// compacted segments become visible to readers.
pub async fn publish_manifest(
    store: &Arc<dyn ObjectStore>,
    base_path: &str,
    manifest: &SegmentManifest,
) -> Result<(), CompactError> {
    let bytes = serde_json::to_vec(manifest)?;
    let path = manifest_path(base_path, &manifest.tenant_id, &manifest.namespace);

    store.put(&Path::from(path.as_str()), bytes.into()).await?;

    Ok(())
}

fn manifest_path(base_path: &str, tenant_id: &str, namespace: &str) -> String {
    format!("{base_path}/{tenant_id}/{namespace}/{MANIFEST_FILE}")
}
