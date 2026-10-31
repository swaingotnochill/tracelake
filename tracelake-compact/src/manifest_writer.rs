use crate::compactor::CompactError;
use slatedb::object_store::{ObjectStore, path::Path};
use std::sync::Arc;
use tracelake_core::SegmentManifest;

/// Manifest file name inside a (tenant, namespace) directory or a table
/// directory.
const MANIFEST_FILE: &str = "manifest.json";

/// Reads the manifest for one (tenant, namespace), or `None` when nothing has
/// been compacted there yet.
pub async fn read_manifest(
    store: &Arc<dyn ObjectStore>,
    base_path: &str,
    tenant_id: &str,
    namespace: &str,
) -> Result<Option<SegmentManifest>, CompactError> {
    crate::validate_location(tenant_id, namespace, None)?;
    read_manifest_at(store, &manifest_path(base_path, tenant_id, namespace, None)).await
}

/// Reads the manifest for one (tenant, namespace, table).
pub async fn read_record_manifest(
    store: &Arc<dyn ObjectStore>,
    base_path: &str,
    tenant_id: &str,
    namespace: &str,
    table: &str,
) -> Result<Option<SegmentManifest>, CompactError> {
    crate::validate_location(tenant_id, namespace, Some(table))?;
    read_manifest_at(
        store,
        &manifest_path(base_path, tenant_id, namespace, Some(table)),
    )
    .await
}

async fn read_manifest_at(
    store: &Arc<dyn ObjectStore>,
    path: &str,
) -> Result<Option<SegmentManifest>, CompactError> {
    match store.get(&Path::from(path)).await {
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
    crate::validate_location(
        &manifest.tenant_id,
        &manifest.namespace,
        manifest.table.as_deref(),
    )?;
    let bytes = serde_json::to_vec(manifest)?;
    let path = manifest_path(
        base_path,
        &manifest.tenant_id,
        &manifest.namespace,
        manifest.table.as_deref(),
    );

    store.put(&Path::from(path.as_str()), bytes.into()).await?;

    Ok(())
}

fn manifest_path(base_path: &str, tenant_id: &str, namespace: &str, table: Option<&str>) -> String {
    match table {
        Some(table) => {
            format!("{base_path}/{tenant_id}/{namespace}/tables/{table}/{MANIFEST_FILE}")
        }
        None => format!("{base_path}/{tenant_id}/{namespace}/{MANIFEST_FILE}"),
    }
}
