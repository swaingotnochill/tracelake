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
    crate::validate_location(tenant_id, namespace, Some(table))?;
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

/// Registers a table schema and returns the version the registry now holds.
///
/// An identical shape returns the existing schema. A compatible shape is
/// stored as the next version, so packs written under earlier versions stay
/// readable. An incompatible shape is refused.
pub async fn put_schema(
    store: &Arc<dyn ObjectStore>,
    base_path: &str,
    tenant_id: &str,
    namespace: &str,
    schema: &TableSchema,
) -> Result<TableSchema, CompactError> {
    schema.validate_shape()?;
    crate::validate_location(tenant_id, namespace, Some(&schema.table))?;

    let next = match read_schema(store, base_path, tenant_id, namespace, &schema.table).await? {
        None => {
            let mut first = schema.clone();
            first.version = 1;
            first
        }
        Some(existing) => {
            if existing.same_shape(schema) {
                return Ok(existing);
            }

            existing.evolve(schema)?;
            let mut next = schema.clone();
            next.version = existing.version + 1;
            next
        }
    };

    let path = schema_path(base_path, tenant_id, namespace, &next.table);
    let bytes = serde_json::to_vec(&next)?;
    store.put(&Path::from(path.as_str()), bytes.into()).await?;

    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use slatedb::object_store::memory::InMemory;
    use tracelake_core::{EvolutionError, FieldType};

    fn notes_schema() -> TableSchema {
        TableSchema::builder("notes")
            .field("id", FieldType::String)
            .field("body", FieldType::String)
            .primary_key("id")
            .build()
            .expect("schema builds")
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
    async fn the_first_registration_is_version_one() {
        let store = store();

        let registered = put_schema(&store, "base", "tenant-1", "default", &notes_schema())
            .await
            .expect("put should succeed");
        let found = read_schema(&store, "base", "tenant-1", "default", "notes")
            .await
            .expect("read should succeed");

        assert_eq!(registered.version, 1);
        assert_eq!(found, Some(registered));
    }

    #[tokio::test]
    async fn an_identical_shape_is_a_no_op() {
        let store = store();
        let first = put_schema(&store, "base", "tenant-1", "default", &notes_schema())
            .await
            .expect("first put should succeed");
        let second = put_schema(&store, "base", "tenant-1", "default", &notes_schema())
            .await
            .expect("identical put should be a no-op");

        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn an_added_optional_field_becomes_the_next_version() {
        let store = store();
        put_schema(&store, "base", "tenant-1", "default", &notes_schema())
            .await
            .expect("first put should succeed");

        let evolved = TableSchema::builder("notes")
            .field("id", FieldType::String)
            .field("body", FieldType::String)
            .field("author", FieldType::String)
            .primary_key("id")
            .build()
            .unwrap();
        let registered = put_schema(&store, "base", "tenant-1", "default", &evolved)
            .await
            .expect("a compatible shape should evolve");

        assert_eq!(registered.version, 2);
        assert_eq!(
            read_schema(&store, "base", "tenant-1", "default", "notes")
                .await
                .unwrap(),
            Some(registered)
        );
    }

    #[tokio::test]
    async fn a_dropped_field_is_allowed() {
        let store = store();
        let with_body = notes_schema();
        put_schema(&store, "base", "tenant-1", "default", &with_body)
            .await
            .unwrap();

        let without_body = TableSchema::builder("notes")
            .field("id", FieldType::String)
            .primary_key("id")
            .build()
            .unwrap();
        let registered = put_schema(&store, "base", "tenant-1", "default", &without_body)
            .await
            .expect("dropping a field should evolve");

        assert_eq!(registered.version, 2);
        assert!(registered.field("body").is_none());
    }

    #[tokio::test]
    async fn a_type_change_and_a_bare_required_field_are_refused() {
        let store = store();
        put_schema(&store, "base", "tenant-1", "default", &notes_schema())
            .await
            .unwrap();

        let retyped = TableSchema::builder("notes")
            .field("id", FieldType::String)
            .field("body", FieldType::I64)
            .primary_key("id")
            .build()
            .unwrap();
        let error = put_schema(&store, "base", "tenant-1", "default", &retyped)
            .await
            .expect_err("a type change must be refused");
        assert!(matches!(
            error,
            CompactError::Evolution(EvolutionError::TypeChanged { .. })
        ));

        let added_required = TableSchema::builder("notes")
            .field("id", FieldType::String)
            .field("body", FieldType::String)
            .required_field("author", FieldType::String)
            .primary_key("id")
            .build()
            .unwrap();
        let error = put_schema(&store, "base", "tenant-1", "default", &added_required)
            .await
            .expect_err("a required field without a default must be refused");
        assert!(matches!(
            error,
            CompactError::Evolution(EvolutionError::RequiredWithoutDefault { .. })
        ));
    }

    #[tokio::test]
    async fn a_required_field_with_a_default_evolves() {
        let store = store();
        put_schema(&store, "base", "tenant-1", "default", &notes_schema())
            .await
            .unwrap();

        let with_default = TableSchema::builder("notes")
            .field("id", FieldType::String)
            .field("body", FieldType::String)
            .required_field_with_default("author", FieldType::String, json!("unknown"))
            .primary_key("id")
            .build()
            .unwrap();
        let registered = put_schema(&store, "base", "tenant-1", "default", &with_default)
            .await
            .expect("a defaulted required field should evolve");

        assert_eq!(registered.version, 2);
        assert_eq!(
            registered
                .field("author")
                .and_then(|field| field.default.clone()),
            Some(json!("unknown"))
        );
    }

    #[tokio::test]
    async fn schemas_are_isolated_by_tenant_namespace_and_table() {
        let store = store();
        put_schema(&store, "base", "tenant-1", "default", &notes_schema())
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
        let mut value = serde_json::to_value(notes_schema()).unwrap();
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
