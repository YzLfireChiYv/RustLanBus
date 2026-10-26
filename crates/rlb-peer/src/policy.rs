//! Resolved input-role choices. User rule matching lives in the application.

use crate::{Intent, MAX_SOURCES, PeerError, SourceKind};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceIntent {
    pub kind: SourceKind,
    pub intent: Intent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourcePolicy {
    pub default: Intent,
    pub overrides: Vec<SourceIntent>,
}

impl SourcePolicy {
    #[must_use]
    pub fn all(intent: Intent) -> Self {
        Self {
            default: intent,
            overrides: Vec::new(),
        }
    }

    #[must_use]
    pub fn for_kind(&self, kind: SourceKind) -> Intent {
        self.overrides
            .iter()
            .find(|choice| choice.kind == kind)
            .map(|choice| choice.intent)
            .unwrap_or(self.default)
    }

    pub(crate) fn validate(&self) -> Result<(), PeerError> {
        if self.overrides.len() > MAX_SOURCES
            || self.overrides.iter().enumerate().any(|(index, choice)| {
                self.overrides[..index]
                    .iter()
                    .any(|prior| prior.kind == choice.kind)
            })
        {
            return Err(PeerError::MalformedControl);
        }
        Ok(())
    }

    /// A control envelope permits discovery of future publications. Actual
    /// audio still requires the exact current source grant at both boundaries.
    pub(crate) fn envelope(&self) -> Intent {
        Intent {
            send: self.default.send || self.overrides.iter().any(|choice| choice.intent.send),
            receive: self.default.receive
                || self.overrides.iter().any(|choice| choice.intent.receive),
        }
    }
}
