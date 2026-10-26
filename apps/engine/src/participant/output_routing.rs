//! Persistent output intent. Endpoint availability never changes the intent.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::config::{SourceKind, validate_optional_endpoint};

/// `Main` follows the selected main output; `Endpoint` retains an exact device.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputTarget {
    Main,
    Endpoint(String),
}

pub fn default_output_targets() -> Vec<OutputTarget> {
    vec![OutputTarget::Main]
}

/// An empty list deliberately routes this source to no output devices.
pub fn validate_output_targets(targets: &[OutputTarget]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for target in targets {
        if !seen.insert(target) {
            return Err("An output target must not be listed more than once.".into());
        }
        if let OutputTarget::Endpoint(endpoint) = target {
            validate_optional_endpoint(Some(endpoint))?;
        }
    }
    Ok(())
}

/// Derived per-source routing projection, separate from transport consent.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceOutputTargets {
    pub kind: SourceKind,
    pub targets: Vec<OutputTarget>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn output_intent_roundtrips_without_resolving_main_to_a_device() {
        let targets = vec![
            OutputTarget::Main,
            OutputTarget::Endpoint("headphones".into()),
        ];
        validate_output_targets(&targets).unwrap();
        let encoded = serde_json::to_value(&targets).unwrap();
        assert_eq!(encoded, json!(["main", {"endpoint": "headphones"}]));
        assert_eq!(
            serde_json::from_value::<Vec<OutputTarget>>(encoded).unwrap(),
            targets
        );
        assert_eq!(default_output_targets(), vec![OutputTarget::Main]);
        validate_output_targets(&[]).unwrap();
    }

    #[test]
    fn duplicate_and_invalid_output_targets_are_rejected() {
        assert!(validate_output_targets(&[OutputTarget::Main, OutputTarget::Main]).is_err());
        assert!(
            validate_output_targets(&[
                OutputTarget::Endpoint("headphones".into()),
                OutputTarget::Endpoint("headphones".into()),
            ])
            .is_err()
        );
        for invalid in [
            "".to_owned(),
            " ".to_owned(),
            " x".to_owned(),
            "x\n".to_owned(),
            "x".repeat(513),
        ] {
            assert!(validate_output_targets(&[OutputTarget::Endpoint(invalid)]).is_err());
        }
        validate_output_targets(&[OutputTarget::Endpoint("x".repeat(512))]).unwrap();
    }
}
