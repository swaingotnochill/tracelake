//! Object store selection. Local disk is the default; S3 and GCS are selected
//! through `TRACELAKE_STORE` and configured from the provider's own environment.

use object_store::ObjectStore;
use object_store::aws::AmazonS3Builder;
use object_store::gcp::GoogleCloudStorageBuilder;
use object_store::local::LocalFileSystem;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreKind {
    Local,
    S3,
    Gcs,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("TRACELAKE_STORE must be local, s3, or gcs, not '{value}'")]
    UnknownKind { value: String },

    #[error("{name} is required when TRACELAKE_STORE={kind}")]
    MissingSetting {
        name: &'static str,
        kind: &'static str,
    },

    #[error("object store configuration failed: {0}")]
    Store(#[from] object_store::Error),

    #[error("data directory failed: {0}")]
    Io(#[from] std::io::Error),
}

impl StoreKind {
    fn parse(value: &str) -> Result<Self, StoreError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "local" => Ok(StoreKind::Local),
            "s3" => Ok(StoreKind::S3),
            "gcs" => Ok(StoreKind::Gcs),
            other => Err(StoreError::UnknownKind {
                value: other.to_string(),
            }),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            StoreKind::Local => "local",
            StoreKind::S3 => "s3",
            StoreKind::Gcs => "gcs",
        }
    }
}

#[derive(Debug, Clone)]
pub struct StoreSettings {
    pub kind: StoreKind,
    pub bucket: Option<String>,
    pub region: Option<String>,
    pub endpoint: Option<String>,
    pub data_dir: PathBuf,
    pub prefix: String,
}

impl StoreSettings {
    /// Reads `TRACELAKE_STORE` (default `local`), plus `TRACELAKE_BUCKET`,
    /// `TRACELAKE_REGION`, `TRACELAKE_ENDPOINT`, `TRACELAKE_DATA_DIR`, and
    /// `TRACELAKE_PREFIX`.
    pub fn from_env() -> Result<Self, StoreError> {
        let kind = StoreKind::parse(
            &std::env::var("TRACELAKE_STORE").unwrap_or_else(|_| "local".to_string()),
        )?;

        Self::from_values(
            kind,
            std::env::var("TRACELAKE_BUCKET").ok(),
            std::env::var("TRACELAKE_REGION").ok(),
            std::env::var("TRACELAKE_ENDPOINT").ok(),
            std::env::var("TRACELAKE_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("./tracelake-data")),
            std::env::var("TRACELAKE_PREFIX").unwrap_or_else(|_| "tracelake".to_string()),
        )
    }

    pub fn from_values(
        kind: StoreKind,
        bucket: Option<String>,
        region: Option<String>,
        endpoint: Option<String>,
        data_dir: PathBuf,
        prefix: String,
    ) -> Result<Self, StoreError> {
        if kind != StoreKind::Local && bucket.as_deref().is_none_or(str::is_empty) {
            return Err(StoreError::MissingSetting {
                name: "TRACELAKE_BUCKET",
                kind: kind.as_str(),
            });
        }

        Ok(Self {
            kind,
            bucket,
            region,
            endpoint,
            data_dir,
            prefix,
        })
    }

    /// Hot-store path inside the selected store.
    pub fn hot_path(&self) -> String {
        match self.kind {
            StoreKind::Local => "hot".to_string(),
            StoreKind::S3 | StoreKind::Gcs => self.scoped("hot"),
        }
    }

    /// Segment path inside the selected store.
    pub fn segment_path(&self) -> String {
        match self.kind {
            StoreKind::Local => "segments".to_string(),
            StoreKind::S3 | StoreKind::Gcs => self.scoped("segments"),
        }
    }

    /// Opens the selected store. Local creates its directory; remote stores use
    /// the provider credential chain.
    pub fn open(&self) -> Result<Arc<dyn ObjectStore>, StoreError> {
        match self.kind {
            StoreKind::Local => {
                std::fs::create_dir_all(&self.data_dir)?;
                let store = LocalFileSystem::new_with_prefix(&self.data_dir)?;
                Ok(Arc::new(store))
            }
            StoreKind::S3 => {
                let bucket = self.bucket()?;
                let mut builder = AmazonS3Builder::from_env().with_bucket_name(bucket);

                if let Some(region) = &self.region {
                    builder = builder.with_region(region);
                }
                if let Some(endpoint) = &self.endpoint {
                    builder = builder
                        .with_endpoint(endpoint)
                        .with_allow_http(endpoint.starts_with("http://"))
                        .with_virtual_hosted_style_request(false);
                }

                Ok(Arc::new(builder.build()?))
            }
            StoreKind::Gcs => {
                let bucket = self.bucket()?;
                Ok(Arc::new(
                    GoogleCloudStorageBuilder::from_env()
                        .with_bucket_name(bucket)
                        .build()?,
                ))
            }
        }
    }

    pub fn describe(&self) -> String {
        match self.kind {
            StoreKind::Local => format!("local:{}", self.data_dir.display()),
            StoreKind::S3 => format!(
                "s3:{} prefix:{} region:{}",
                self.bucket.as_deref().unwrap_or_default(),
                self.prefix,
                self.region.as_deref().unwrap_or("provider default")
            ),
            StoreKind::Gcs => format!(
                "gcs:{} prefix:{}",
                self.bucket.as_deref().unwrap_or_default(),
                self.prefix
            ),
        }
    }

    fn bucket(&self) -> Result<String, StoreError> {
        self.bucket.clone().ok_or(StoreError::MissingSetting {
            name: "TRACELAKE_BUCKET",
            kind: self.kind.as_str(),
        })
    }

    fn scoped(&self, leaf: &str) -> String {
        let prefix = self.prefix.trim_matches('/');
        if prefix.is_empty() {
            leaf.to_string()
        } else {
            format!("{prefix}/{leaf}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::path::Path as ObjectPath;

    fn settings(kind: StoreKind, bucket: Option<&str>, prefix: &str) -> StoreSettings {
        StoreSettings::from_values(
            kind,
            bucket.map(str::to_string),
            Some("us-east-1".to_string()),
            None,
            PathBuf::from("./unused"),
            prefix.to_string(),
        )
        .expect("settings are valid")
    }

    #[test]
    fn local_is_the_default_layout() {
        let settings = settings(StoreKind::Local, None, "ignored");

        assert_eq!(settings.hot_path(), "hot");
        assert_eq!(settings.segment_path(), "segments");
    }

    #[test]
    fn remote_stores_scope_paths_under_the_prefix() {
        for kind in [StoreKind::S3, StoreKind::Gcs] {
            let settings = settings(kind, Some("bucket"), "tracelake");
            assert_eq!(settings.hot_path(), "tracelake/hot");
            assert_eq!(settings.segment_path(), "tracelake/segments");
        }
    }

    #[test]
    fn an_empty_prefix_keeps_paths_flat() {
        let settings = settings(StoreKind::S3, Some("bucket"), "");

        assert_eq!(settings.hot_path(), "hot");
        assert_eq!(settings.segment_path(), "segments");
    }

    #[test]
    fn s3_and_gcs_require_a_bucket() {
        for kind in [StoreKind::S3, StoreKind::Gcs] {
            let error = StoreSettings::from_values(
                kind,
                None,
                None,
                None,
                PathBuf::from("./unused"),
                "tracelake".to_string(),
            )
            .expect_err("a remote store needs a bucket");

            assert!(matches!(
                error,
                StoreError::MissingSetting {
                    name: "TRACELAKE_BUCKET",
                    ..
                }
            ));
        }
    }

    #[test]
    fn an_unknown_store_kind_is_rejected() {
        let error = StoreKind::parse("azure").expect_err("azure is not selectable");

        assert!(matches!(error, StoreError::UnknownKind { ref value } if value == "azure"));
    }

    #[test]
    fn store_kinds_parse_case_insensitively() {
        assert_eq!(StoreKind::parse("LOCAL").ok(), Some(StoreKind::Local));
        assert_eq!(StoreKind::parse(" s3 ").ok(), Some(StoreKind::S3));
        assert_eq!(StoreKind::parse("Gcs").ok(), Some(StoreKind::Gcs));
    }

    #[tokio::test]
    async fn a_local_store_round_trips_an_object() {
        let dir = tempfile::tempdir().expect("temp dir");
        let settings = StoreSettings::from_values(
            StoreKind::Local,
            None,
            None,
            None,
            dir.path().to_path_buf(),
            "ignored".to_string(),
        )
        .expect("settings are valid");

        let store = settings.open().expect("local store opens");
        store
            .put(&ObjectPath::from("hot/key"), "value".into())
            .await
            .expect("object writes");
        let read = store
            .get(&ObjectPath::from("hot/key"))
            .await
            .expect("object reads")
            .bytes()
            .await
            .expect("bytes read");

        assert_eq!(&read[..], b"value");
    }
}
