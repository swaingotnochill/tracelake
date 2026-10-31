use crate::compactor::CompactError;
use slatedb::object_store::{ObjectStore, path::Path};
use std::sync::Arc;
use tracelake_core::TableSchema;

/// Schema objects live beside the table's segments, one per table.
pub(crate) fn schema_path(
    base_path: &str,
    tenant_id: &str,
    namespace: &str,
    table: &str,
) -> String {
    format!("{base_path}/{tenant_id}/{namespace}/tables/{table}/schema.json")
}

/// Reads and re-validates the registered schema for one table.
pub async fn read_schema(
    store: &Arc<dyn ObjectStore>,
    base_path: &str,
    tenant_id: &str,
    namespace: &str,
    table: &str,
) -> Result<Option<TableSchema>, CompactError> {
    let path = schema_path(base_path, tenant_id, namespace, table);

    match store.get(&Path::from(path.as_str())).await {
        Ok(result) => {
            let bytes = result.bytes().await?;
            let schema: TableSchema = serde_json::from_slice(&bytes)?;
            schema.validate_shape()?;
            Ok(Some(schema))
        }
        Err(slatedb::object_store::Error::NotFound { .. }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Registers a schema once. Re-registering the same shape is a no-op; a
/// different shape is refused because published packs were written under the
/// old fingerprint and would no longer decode.
pub async fn put_schema(
    store: &Arc<dyn ObjectStore>,
    base_path: &str,
    tenant_id: &str,
    namespace: &str,
    schema: &TableSchema,
) -> Result<(), CompactError> {
    schema.validate_shape()?;

    if let Some(existing) =
        read_schema(store, base_path, tenant_id, namespace, &schema.table).await?
    {
        if existing.fingerprint() != schema.fingerprint() {
            return Err(CompactError::SchemaConflict {
                table: schema.table.clone(),
            });
        }

        return Ok(());
    }

    let path = schema_path(base_path, tenant_id, namespace, &schema.table);
    let bytes = serde_json::to_vec(schema)?;
    store.put(&Path::from(path.as_str()), bytes.into()).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use slatedb::object_store::memory::InMemory;
    use tracelake_core::FieldType;

    fn notes_schema(extra: Option<(&str, FieldType)>) -> TableSchema {
        let mut builder = TableSchema::builder("notes")
            .field("id", FieldType::String)
            .field("body", FieldType::String);

        if let Some((name, field_type)) = extra {
            builder = builder.field(name, field_type);
        }

        builder.primary_key("id").build().expect("schema builds")
    }

    fn store() -> Arc<dyn ObjectStore> {
        Arc::new(InMemory::new())
    }

    #[tokio::test]
    async fn absent_schema_reads_as_none() {
        let store = store();

        let found = read_schema(&store, "base", "tenant-1", "default", "notes")
            .await
            .expect("read should succeed");

        assert_eq!(found, None);
    }

    #[tokio::test]
    async fn registered_schema_round_trips_through_the_store() {
        let store = store();
        let schema = notes_schema(Some(("score", FieldType::F64)));

        put_schema(&store, "base", "tenant-1", "default", &schema)
            .await
            .expect("put should succeed");
        let found = read_schema(&store, "base", "tenant-1", "default", "notes")
            .await
            .expect("read should succeed");

        assert_eq!(found, Some(schema));
    }

    #[tokio::test]
    async fn re_registering_the_same_shape_is_a_no_op_and_a_new_shape_is_refused() {
        let store = store();
        let schema = notes_schema(None);

        put_schema(&store, "base", "tenant-1", "default", &schema)
            .await
            .expect("first put should succeed");
        put_schema(&store, "base", "tenant-1", "default", &schema)
            .await
            .expect("identical put should be a no-op");

        let error = put_schema(
            &store,
            "base",
            "tenant-1",
            "default",
            &notes_schema(Some(("score", FieldType::F64))),
        )
        .await
        .expect_err("a different shape must be refused");

        assert!(matches!(error, CompactError::SchemaConflict { .. }));
    }

    #[tokio::test]
    async fn schemas_are_isolated_by_tenant_namespace_and_table() {
        let store = store();
        let schema = notes_schema(None);

        put_schema(&store, "base", "tenant-1", "default", &schema)
            .await
            .expect("put should succeed");

        for (tenant, namespace, table) in [
            ("tenant-2", "default", "notes"),
            ("tenant-1", "other", "notes"),
            ("tenant-1", "default", "other"),
        ] {
            let found = read_schema(&store, "base", tenant, namespace, table)
                .await
                .expect("read should succeed");
            assert_eq!(found, None, "{tenant}/{namespace}/{table}");
        }
    }

    #[tokio::test]
    async fn a_stored_schema_with_a_foreign_primary_key_is_rejected_on_read() {
        let store = store();
        let path = schema_path("base", "tenant-1", "default", "notes");
        let mut value = serde_json::to_value(notes_schema(None)).unwrap();
        value["fields"][0]["field_type"] = serde_json::json!("i64");
        store
            .put(
                &Path::from(path.as_str()),
                serde_json::to_vec(&value).unwrap().into(),
            )
            .await
            .unwrap();

        let error = read_schema(&store, "base", "tenant-1", "default", "notes")
            .await
            .expect_err("invalid stored schema must be refused");

        assert!(matches!(error, CompactError::Schema(_)));
    }
}
