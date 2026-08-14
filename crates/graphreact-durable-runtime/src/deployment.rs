use std::collections::BTreeMap;
use std::fmt::Write as _;

use graphreact_grx_parser::GrxIr;
use graphreact_persistence::{
    canonical_json_bytes, DeploymentBundle, DeploymentId, PersistenceEngine,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::DurableRuntimeError;

#[derive(Debug, Clone, Default)]
pub struct BundleInputs {
    pub module_lockfiles: BTreeMap<String, Vec<u8>>,
    pub external_bodies: BTreeMap<String, Vec<u8>>,
    pub generated_go_artifacts: BTreeMap<String, Vec<u8>>,
}

#[derive(Serialize)]
struct BundlePayload<'a> {
    format_version: u16,
    ir: &'a GrxIr,
    module_lockfiles: &'a BTreeMap<String, Vec<u8>>,
    external_bodies: &'a BTreeMap<String, Vec<u8>>,
    generated_go_artifacts: &'a BTreeMap<String, Vec<u8>>,
}

pub fn build_deployment_bundle(
    deployment_id: DeploymentId,
    ir: &GrxIr,
    inputs: &BundleInputs,
) -> Result<DeploymentBundle, DurableRuntimeError> {
    let payload_value = serde_json::to_value(BundlePayload {
        format_version: 1,
        ir,
        module_lockfiles: &inputs.module_lockfiles,
        external_bodies: &inputs.external_bodies,
        generated_go_artifacts: &inputs.generated_go_artifacts,
    })
    .map_err(|error| DurableRuntimeError::InvalidMessage(error.to_string()))?;
    let payload = canonical_json_bytes(&payload_value);
    let ir_value = serde_json::to_value(ir)
        .map_err(|error| DurableRuntimeError::InvalidMessage(error.to_string()))?;
    let manifest = serde_json::json!({
        "format_version": 1,
        "ir_sha256": hex(Sha256::digest(canonical_json_bytes(&ir_value))),
        "payload_sha256": hex(Sha256::digest(&payload)),
        "lockfiles": digest_map(&inputs.module_lockfiles),
        "external_bodies": digest_map(&inputs.external_bodies),
        "generated_go_artifacts": digest_map(&inputs.generated_go_artifacts),
    });
    Ok(DeploymentBundle::new(deployment_id, manifest, payload))
}

pub async fn install_deployment_bundle(
    engine: &dyn PersistenceEngine,
    bundle: &DeploymentBundle,
) -> Result<(), DurableRuntimeError> {
    if !bundle.verify_digest()
        || bundle.deployment_id != *engine.deployment_id()
        || bundle.digest != engine.deployment_digest()
    {
        return Err(DurableRuntimeError::DeploymentBundleMismatch);
    }
    if let Some(existing) = engine.read_bundle().await? {
        if existing.digest != bundle.digest || !existing.verify_digest() {
            return Err(DurableRuntimeError::DeploymentBundleMismatch);
        }
        return Ok(());
    }
    engine.put_bundle(bundle).await?;
    Ok(())
}

pub async fn require_deployment_bundle(
    engine: &dyn PersistenceEngine,
) -> Result<DeploymentBundle, DurableRuntimeError> {
    let bundle = engine
        .read_bundle()
        .await?
        .ok_or(DurableRuntimeError::DeploymentBundleMissing)?;
    if bundle.deployment_id != *engine.deployment_id()
        || bundle.digest != engine.deployment_digest()
        || !bundle.verify_digest()
    {
        return Err(DurableRuntimeError::DeploymentBundleMismatch);
    }
    Ok(bundle)
}

fn digest_map(values: &BTreeMap<String, Vec<u8>>) -> BTreeMap<String, String> {
    values
        .iter()
        .map(|(name, bytes)| (name.clone(), hex(Sha256::digest(bytes))))
        .collect()
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .fold(String::new(), |mut output, byte| {
            let _ = write!(output, "{byte:02x}");
            output
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphreact_persistence::{LocalConfig, LocalPersistence};

    #[test]
    fn bundle_is_deterministic_and_required_for_readiness() {
        let ir = graphreact_grx_parser::ir::parse_to_ir(
            r#"<Graph id="agents"><Agent id="agent" /></Graph>"#,
        )
        .unwrap();
        let deployment_id = DeploymentId::new();
        let mut inputs = BundleInputs::default();
        inputs
            .module_lockfiles
            .insert("package-lock.json".into(), b"lock".to_vec());
        inputs
            .external_bodies
            .insert("agent.js".into(), b"export default {}".to_vec());
        let first = build_deployment_bundle(deployment_id.clone(), &ir, &inputs).unwrap();
        let second = build_deployment_bundle(deployment_id.clone(), &ir, &inputs).unwrap();
        assert_eq!(first.digest, second.digest);

        let directory = tempfile::tempdir().unwrap();
        let store = LocalPersistence::open(LocalConfig::new(
            directory.path(),
            deployment_id,
            first.digest,
        ))
        .unwrap();
        let missing = futures::executor::block_on(require_deployment_bundle(&store)).unwrap_err();
        assert!(matches!(
            missing,
            DurableRuntimeError::DeploymentBundleMissing
        ));
        futures::executor::block_on(install_deployment_bundle(&store, &first)).unwrap();
        let restored = futures::executor::block_on(require_deployment_bundle(&store)).unwrap();
        assert_eq!(restored.digest, first.digest);
        assert_eq!(restored.payload, first.payload);
    }
}
