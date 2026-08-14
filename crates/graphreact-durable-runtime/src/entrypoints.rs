use std::collections::BTreeMap;

use graphreact_core::PermissionManifest;
use graphreact_grx_parser::GrxIr;

/// Validate the runtime portion of the immutable entrypoint contract after
/// component bodies have mounted and produced their permission manifests.
pub fn validate_entrypoint_manifests(
    ir: &GrxIr,
    manifests: &BTreeMap<String, PermissionManifest>,
) -> Result<(), EntrypointValidationError> {
    for entrypoint in &ir.entrypoints {
        let manifest = manifests.get(&entrypoint.target).ok_or_else(|| {
            EntrypointValidationError::MissingManifest {
                entrypoint: entrypoint.name.clone(),
                target: entrypoint.target.clone(),
            }
        })?;
        if !manifest.accepts_external_call.iter().any(|call| {
            call.request_type == entrypoint.request && call.reply_type == entrypoint.reply
        }) {
            return Err(EntrypointValidationError::MissingExternalCall {
                entrypoint: entrypoint.name.clone(),
                target: entrypoint.target.clone(),
                request: entrypoint.request.clone(),
                reply: entrypoint.reply.clone(),
            });
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EntrypointValidationError {
    #[error("entrypoint `{entrypoint}` target `{target}` did not produce a permission manifest")]
    MissingManifest { entrypoint: String, target: String },
    #[error(
        "entrypoint `{entrypoint}` requires target `{target}` to declare accepts_external_call request `{request}` reply `{reply}`"
    )]
    MissingExternalCall {
        entrypoint: String,
        target: String,
        request: String,
        reply: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphreact_core::ExternalCall;

    fn entrypoint_ir() -> GrxIr {
        graphreact_grx_parser::ir::parse_to_ir(
            r#"
            <Graph id="agents">
              <Agent id="agent" />
              <entrypoint name="summarize" target="agent" request="prompt" reply="completion" />
            </Graph>
            "#,
        )
        .unwrap()
    }

    #[test]
    fn matching_external_call_pair_is_ready() {
        let mut manifest = PermissionManifest::default();
        manifest.accepts_external_call.push(ExternalCall {
            request_type: "prompt".into(),
            reply_type: "completion".into(),
        });
        let manifests = BTreeMap::from([("agent".into(), manifest)]);
        validate_entrypoint_manifests(&entrypoint_ir(), &manifests).unwrap();
    }

    #[test]
    fn mismatched_pair_fails_readiness() {
        let mut manifest = PermissionManifest::default();
        manifest.accepts_external_call.push(ExternalCall {
            request_type: "prompt".into(),
            reply_type: "wrong".into(),
        });
        let manifests = BTreeMap::from([("agent".into(), manifest)]);
        let error = validate_entrypoint_manifests(&entrypoint_ir(), &manifests).unwrap_err();
        assert!(matches!(
            error,
            EntrypointValidationError::MissingExternalCall { .. }
        ));
    }

    #[test]
    fn absent_manifest_fails_readiness() {
        let error = validate_entrypoint_manifests(&entrypoint_ir(), &BTreeMap::new()).unwrap_err();
        assert!(matches!(
            error,
            EntrypointValidationError::MissingManifest { .. }
        ));
    }
}
