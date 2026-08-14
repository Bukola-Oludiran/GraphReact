use std::path::PathBuf;

use graphreact_core::DurabilityMode;
use graphreact_persistence::DeploymentId;

/// Unvalidated durability settings collected from command-line flags and the
/// process environment. Credential values are deliberately absent: S3 clients
/// must use their standard credential chain or workload identity.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RawDurabilityConfig {
    pub mode: DurabilityMode,
    pub deployment_id: Option<String>,
    pub durability_dir: Option<PathBuf>,
    pub replica_id: Option<String>,
    pub database_url: Option<String>,
    pub s3_bucket: Option<String>,
    pub s3_endpoint: Option<String>,
    pub s3_region: Option<String>,
    pub s3_prefix: Option<String>,
}

impl std::fmt::Debug for RawDurabilityConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RawDurabilityConfig")
            .field("mode", &self.mode)
            .field("deployment_id", &self.deployment_id)
            .field("durability_dir", &self.durability_dir)
            .field("replica_id", &self.replica_id)
            .field(
                "database_url",
                &self.database_url.as_ref().map(|_| "[REDACTED]"),
            )
            .field("s3_bucket", &self.s3_bucket)
            .field("s3_endpoint", &self.s3_endpoint)
            .field("s3_region", &self.s3_region)
            .field("s3_prefix", &self.s3_prefix)
            .finish()
    }
}

impl RawDurabilityConfig {
    /// Fill storage settings from the supported Phase A environment
    /// variables, preserving explicit values already supplied by the caller.
    #[must_use]
    pub fn with_environment(mut self) -> Self {
        self.database_url = self
            .database_url
            .or_else(|| non_empty_env("GRX_PERSISTENCE_DATABASE_URL"));
        self.s3_bucket = self
            .s3_bucket
            .or_else(|| non_empty_env("GRX_PERSISTENCE_S3_BUCKET"));
        self.s3_endpoint = self
            .s3_endpoint
            .or_else(|| non_empty_env("GRX_PERSISTENCE_S3_ENDPOINT"));
        self.s3_region = self
            .s3_region
            .or_else(|| non_empty_env("GRX_PERSISTENCE_S3_REGION"));
        self.s3_prefix = self
            .s3_prefix
            .or_else(|| non_empty_env("GRX_PERSISTENCE_S3_PREFIX"));
        self
    }

    pub fn validate(
        self,
        watch_enabled: bool,
    ) -> Result<ValidatedDurabilityConfig, DurabilityConfigError> {
        if watch_enabled && self.mode != DurabilityMode::Off {
            return Err(DurabilityConfigError::WatchNotSupported);
        }

        match self.mode {
            DurabilityMode::Off => Ok(ValidatedDurabilityConfig::Off),
            DurabilityMode::Local => {
                let deployment_id = parse_deployment_id(self.deployment_id)?;
                let directory = self
                    .durability_dir
                    .ok_or(DurabilityConfigError::Missing("--durability-dir"))?;
                Ok(ValidatedDurabilityConfig::Local {
                    deployment_id,
                    directory,
                })
            }
            DurabilityMode::Cluster => {
                let deployment_id = parse_deployment_id(self.deployment_id)?;
                let replica_id = required_non_empty(self.replica_id, "--replica-id")?;
                let database_url =
                    required_non_empty(self.database_url, "GRX_PERSISTENCE_DATABASE_URL")?;
                let bucket = required_non_empty(self.s3_bucket, "GRX_PERSISTENCE_S3_BUCKET")?;
                Ok(ValidatedDurabilityConfig::Cluster {
                    deployment_id,
                    replica_id,
                    database_url,
                    object_store: ClusterObjectStoreConfig {
                        bucket,
                        endpoint: normalized(self.s3_endpoint),
                        region: normalized(self.s3_region),
                        prefix: normalized(self.s3_prefix),
                    },
                })
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum ValidatedDurabilityConfig {
    Off,
    Local {
        deployment_id: DeploymentId,
        directory: PathBuf,
    },
    Cluster {
        deployment_id: DeploymentId,
        replica_id: String,
        database_url: String,
        object_store: ClusterObjectStoreConfig,
    },
}

impl std::fmt::Debug for ValidatedDurabilityConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => formatter.write_str("Off"),
            Self::Local {
                deployment_id,
                directory,
            } => formatter
                .debug_struct("Local")
                .field("deployment_id", deployment_id)
                .field("directory", directory)
                .finish(),
            Self::Cluster {
                deployment_id,
                replica_id,
                object_store,
                ..
            } => formatter
                .debug_struct("Cluster")
                .field("deployment_id", deployment_id)
                .field("replica_id", replica_id)
                .field("database_url", &"[REDACTED]")
                .field("object_store", object_store)
                .finish(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterObjectStoreConfig {
    pub bucket: String,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub prefix: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DurabilityConfigError {
    #[error("durability cannot be combined with --watch; durable deployments are immutable")]
    WatchNotSupported,
    #[error("missing required durability setting: {0}")]
    Missing(&'static str),
    #[error("--deployment-id must be a valid UUID: {0}")]
    InvalidDeploymentId(String),
}

fn parse_deployment_id(value: Option<String>) -> Result<DeploymentId, DurabilityConfigError> {
    let value = required_non_empty(value, "--deployment-id")?;
    DeploymentId::parse(&value).map_err(|_| DurabilityConfigError::InvalidDeploymentId(value))
}

fn required_non_empty(
    value: Option<String>,
    name: &'static str,
) -> Result<String, DurabilityConfigError> {
    normalized(value).ok_or(DurabilityConfigError::Missing(name))
}

fn normalized(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    })
}

fn non_empty_env(name: &str) -> Option<String> {
    normalized(std::env::var(name).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deployment_id() -> String {
        DeploymentId::new().to_string()
    }

    #[test]
    fn off_is_the_compatibility_default() {
        assert_eq!(
            RawDurabilityConfig::default().validate(false).unwrap(),
            ValidatedDurabilityConfig::Off
        );
    }

    #[test]
    fn local_requires_identity_and_directory() {
        let error = RawDurabilityConfig {
            mode: DurabilityMode::Local,
            deployment_id: Some(deployment_id()),
            ..RawDurabilityConfig::default()
        }
        .validate(false)
        .unwrap_err();
        assert_eq!(error, DurabilityConfigError::Missing("--durability-dir"));
    }

    #[test]
    fn durable_watch_is_rejected() {
        let error = RawDurabilityConfig {
            mode: DurabilityMode::Local,
            ..RawDurabilityConfig::default()
        }
        .validate(true)
        .unwrap_err();
        assert_eq!(error, DurabilityConfigError::WatchNotSupported);
    }

    #[test]
    fn cluster_requires_storage_and_replica_settings() {
        let error = RawDurabilityConfig {
            mode: DurabilityMode::Cluster,
            deployment_id: Some(deployment_id()),
            replica_id: Some("replica-a".to_owned()),
            ..RawDurabilityConfig::default()
        }
        .validate(false)
        .unwrap_err();
        assert_eq!(
            error,
            DurabilityConfigError::Missing("GRX_PERSISTENCE_DATABASE_URL")
        );
    }

    #[test]
    fn cluster_debug_output_redacts_database_url() {
        let config = RawDurabilityConfig {
            mode: DurabilityMode::Cluster,
            deployment_id: Some(deployment_id()),
            replica_id: Some("replica-a".to_owned()),
            database_url: Some("postgres://secret".to_owned()),
            s3_bucket: Some("durability".to_owned()),
            ..RawDurabilityConfig::default()
        }
        .validate(false)
        .unwrap();
        let debug = format!("{config:?}");
        assert!(!debug.contains("postgres://secret"));
        assert!(debug.contains("[REDACTED]"));
    }
}
