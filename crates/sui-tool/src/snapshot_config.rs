// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use object_store::{
    aws::AmazonS3Builder, azure::MicrosoftAzureBuilder, gcp::GoogleCloudStorageBuilder,
    local::LocalFileSystem,
};
use sui_protocol_config::Chain;
use url::Url;

use crate::storage::{HttpStorage, Storage, StorageConnectionArgs};

#[derive(clap::Args, Clone, Debug)]
#[group(required = false)]
pub struct SnapshotSourceArgs {
    /// Fetch snapshot from AWS S3. Provide the bucket name.
    /// (env: AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_DEFAULT_REGION, AWS_SNAPSHOT_ENDPOINT)
    #[arg(long, group = "source")]
    pub s3: Option<String>,

    /// Fetch snapshot from Google Cloud Storage. Provide the bucket name.
    /// (env: GCS_SNAPSHOT_SERVICE_ACCOUNT_FILE_PATH, GCS_SNAPSHOT_SERVICE_ACCOUNT_PROJECT_ID)
    #[arg(long, group = "source")]
    pub gcs: Option<String>,

    /// Fetch snapshot from Azure Blob Storage. Provide the container name.
    /// (env: AZURE_SNAPSHOT_STORAGE_ACCOUNT, AZURE_SNAPSHOT_STORAGE_ACCESS_KEY)
    #[arg(long, group = "source")]
    pub azure: Option<String>,

    /// Fetch snapshot from a generic HTTP endpoint.
    #[arg(long, group = "source")]
    pub http: Option<Url>,

    /// Fetch snapshot from local filesystem. Provide the path to the snapshot directory.
    #[arg(long = "snapshot-dir", group = "source")]
    pub snapshot_dir: Option<PathBuf>,
}

impl SnapshotSourceArgs {
    /// Build a storage backend from the provided arguments.
    pub async fn build_storage(
        &self,
        connection_args: &StorageConnectionArgs,
    ) -> anyhow::Result<Arc<dyn Storage + Send + Sync + 'static>> {
        let store: Arc<dyn Storage + Send + Sync + 'static> = if let Some(bucket) = &self.s3 {
            let mut builder = AmazonS3Builder::from_env()
                .with_client_options(connection_args.into())
                .with_bucket_name(bucket);

            if let Ok(endpoint) = std::env::var("AWS_SNAPSHOT_ENDPOINT") {
                builder = builder.with_endpoint(endpoint);
            }

            builder.build().map(Arc::new)?
        } else if let Some(bucket) = &self.gcs {
            GoogleCloudStorageBuilder::from_env()
                .with_client_options(connection_args.into())
                .with_bucket_name(bucket)
                .build()
                .map(Arc::new)?
        } else if let Some(container) = &self.azure {
            MicrosoftAzureBuilder::from_env()
                .with_client_options(connection_args.into())
                .with_container_name(container)
                .build()
                .map(Arc::new)?
        } else if let Some(endpoint) = &self.http {
            HttpStorage::new(endpoint.clone(), connection_args).map(Arc::new)?
        } else if let Some(path) = &self.snapshot_dir {
            LocalFileSystem::new_with_prefix(path).map(Arc::new)?
        } else {
            bail!("No snapshot source provided");
        };

        Ok(store)
    }

    /// Infer default bucket/endpoint based on network if none is specified.
    pub fn with_defaults(self, network: Chain, no_sign_request: bool) -> Self {
        if self.s3.is_some()
            || self.gcs.is_some()
            || self.azure.is_some()
            || self.http.is_some()
            || self.snapshot_dir.is_some()
        {
            return self;
        }

        // Apply defaults based on network
        if no_sign_request {
            let endpoint = match network {
                Chain::Mainnet => Some("https://formal-snapshot.mainnet.sui.io".to_string()),
                Chain::Testnet => Some("https://formal-snapshot.testnet.sui.io".to_string()),
                _ => None,
            };
            if let Some(url) = endpoint.and_then(|s| Url::parse(&s).ok()) {
                Self {
                    http: Some(url),
                    ..self
                }
            } else {
                self
            }
        } else {
            self
        }
    }

    /// Infer default DB snapshot bucket/endpoint based on network.
    pub fn with_db_defaults(self, network: Chain) -> Self {
        if self.s3.is_some()
            || self.gcs.is_some()
            || self.azure.is_some()
            || self.http.is_some()
            || self.snapshot_dir.is_some()
        {
            return self;
        }

        let bucket = match network {
            Chain::Mainnet => {
                std::env::var("MAINNET_DB_SIGNED_BUCKET")
                    .unwrap_or_else(|_| "mysten-mainnet-snapshots".to_string())
            }
            Chain::Testnet => {
                std::env::var("TESTNET_DB_SIGNED_BUCKET")
                    .unwrap_or_else(|_| "mysten-testnet-snapshots".to_string())
            }
            _ => return self,
        };

        Self {
            s3: Some(bucket),
            ..self
        }
    }

    /// Infer default formal snapshot bucket based on network.
    pub fn with_formal_defaults(self, network: Chain, no_sign_request: bool) -> Self {
        if self.s3.is_some()
            || self.gcs.is_some()
            || self.azure.is_some()
            || self.http.is_some()
            || self.snapshot_dir.is_some()
        {
            return self;
        }

        if no_sign_request {
            let endpoint = match network {
                Chain::Mainnet => Some("https://formal-snapshot.mainnet.sui.io".to_string()),
                Chain::Testnet => Some("https://formal-snapshot.testnet.sui.io".to_string()),
                _ => None,
            };
            if let Some(url) = endpoint.and_then(|s| Url::parse(&s).ok()) {
                return Self {
                    http: Some(url),
                    ..self
                };
            }
        }

        let bucket = match (network, no_sign_request) {
            (Chain::Mainnet, false) => std::env::var("MAINNET_FORMAL_SIGNED_BUCKET")
                .unwrap_or_else(|_| "mysten-mainnet-formal".to_string()),
            (Chain::Testnet, false) => std::env::var("TESTNET_FORMAL_SIGNED_BUCKET")
                .unwrap_or_else(|_| "mysten-testnet-formal".to_string()),
            _ => return self,
        };

        Self {
            s3: Some(bucket),
            ..self
        }
    }

    /// Build an ObjectStoreConfig for use with legacy download functions.
    /// This maintains compatibility with StateSnapshotReaderV1 and other infrastructure.
    pub fn to_object_store_config(&self) -> anyhow::Result<sui_config::object_storage_config::ObjectStoreConfig> {
        use sui_config::object_storage_config::{ObjectStoreConfig, ObjectStoreType};

        if let Some(bucket) = &self.s3 {
            Ok(ObjectStoreConfig {
                object_store: Some(ObjectStoreType::S3),
                bucket: Some(bucket.clone()),
                aws_access_key_id: std::env::var("AWS_SNAPSHOT_ACCESS_KEY_ID").ok(),
                aws_secret_access_key: std::env::var("AWS_SNAPSHOT_SECRET_ACCESS_KEY").ok(),
                aws_region: std::env::var("AWS_SNAPSHOT_REGION").ok(),
                aws_endpoint: std::env::var("AWS_SNAPSHOT_ENDPOINT").ok(),
                aws_virtual_hosted_style_request: std::env::var("AWS_SNAPSHOT_VIRTUAL_HOSTED_REQUESTS")
                    .ok()
                    .and_then(|b| b.parse().ok())
                    .unwrap_or(false),
                object_store_connection_limit: 200,
                ..Default::default()
            })
        } else if let Some(bucket) = &self.gcs {
            Ok(ObjectStoreConfig {
                object_store: Some(ObjectStoreType::GCS),
                bucket: Some(bucket.clone()),
                google_service_account: std::env::var("GCS_SNAPSHOT_SERVICE_ACCOUNT_FILE_PATH").ok(),
                google_project_id: std::env::var("GCS_SNAPSHOT_SERVICE_ACCOUNT_PROJECT_ID").ok(),
                object_store_connection_limit: 200,
                ..Default::default()
            })
        } else if let Some(container) = &self.azure {
            Ok(ObjectStoreConfig {
                object_store: Some(ObjectStoreType::Azure),
                bucket: Some(container.clone()),
                azure_storage_account: std::env::var("AZURE_SNAPSHOT_STORAGE_ACCOUNT").ok(),
                azure_storage_access_key: std::env::var("AZURE_SNAPSHOT_STORAGE_ACCESS_KEY").ok(),
                object_store_connection_limit: 200,
                ..Default::default()
            })
        } else if let Some(path) = &self.snapshot_dir {
            Ok(ObjectStoreConfig {
                object_store: Some(ObjectStoreType::File),
                directory: Some(path.clone()),
                ..Default::default()
            })
        } else if let Some(url) = &self.http {
            // HTTP storage for formal snapshots uses S3-compatible interface
            Ok(ObjectStoreConfig {
                object_store: Some(ObjectStoreType::S3),
                aws_endpoint: Some(url.to_string()),
                aws_virtual_hosted_style_request: true,
                object_store_connection_limit: 200,
                no_sign_request: true,
                ..Default::default()
            })
        } else {
            bail!("No snapshot source configured")
        }
    }
}

/// Validate that a snapshot for the given epoch exists and is complete.
pub async fn validate_snapshot(
    storage: &Arc<dyn Storage + Send + Sync>,
    epoch: u64,
) -> anyhow::Result<()> {
    let success_marker = format!("epoch_{}/_SUCCESS", epoch);

    storage
        .get(success_marker.clone().into())
        .await
        .context(format!(
            "Snapshot for epoch {} is not complete (missing {})",
            epoch, success_marker
        ))?;

    Ok(())
}
