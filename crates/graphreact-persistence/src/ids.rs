use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! durable_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::now_v7().to_string())
            }

            pub fn parse(value: impl Into<String>) -> Result<Self, uuid::Error> {
                let value = value.into();
                Uuid::parse_str(&value)?;
                Ok(Self(value))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

durable_id!(DeploymentId);
durable_id!(RunId);
durable_id!(AttemptId);
durable_id!(MessageId);
durable_id!(EffectId);
durable_id!(TimerId);
durable_id!(ArtifactId);
durable_id!(CorrelationId);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_uuid_v7_and_round_trip() {
        let id = RunId::new();
        let parsed = Uuid::parse_str(id.as_str()).unwrap();
        assert_eq!(parsed.get_version_num(), 7);
        assert_eq!(RunId::parse(id.to_string()).unwrap(), id);
    }
}
