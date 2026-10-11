use core::fmt;
use core::str::FromStr;

use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// Why a canonical revision string was rejected.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RevisionParseError {
    /// The input was empty.
    #[error("a revision string cannot be empty")]
    Empty,
    /// A nonzero revision had a leading zero.
    #[error("a revision string cannot contain leading zeroes")]
    LeadingZero,
    /// The input contained a non-decimal byte.
    #[error("a revision must contain only ASCII decimal digits")]
    InvalidDecimal,
    /// The decimal value did not fit in `u64`.
    #[error("the revision exceeds u64")]
    Overflow,
}

fn parse_revision(value: &str) -> Result<u64, RevisionParseError> {
    if value.is_empty() {
        return Err(RevisionParseError::Empty);
    }
    if value.len() > 1 && value.starts_with('0') {
        return Err(RevisionParseError::LeadingZero);
    }
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(RevisionParseError::InvalidDecimal);
    }
    value.parse().map_err(|_| RevisionParseError::Overflow)
}

/// Common operations supported by every canonical revision domain.
pub trait CanonicalRevision: Copy + Eq + Ord {
    /// Returns the underlying monotonic value.
    fn get(self) -> u64;

    /// Returns the next revision, or `None` at `u64::MAX`.
    fn checked_next(self) -> Option<Self>;
}

macro_rules! revision_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        #[repr(transparent)]
        pub struct $name(u64);

        impl $name {
            /// Creates a revision from its numeric value.
            #[must_use]
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            /// Returns the underlying monotonic value.
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }

            /// Returns the next revision, or `None` at `u64::MAX`.
            #[must_use]
            pub const fn checked_next(self) -> Option<Self> {
                match self.0.checked_add(1) {
                    Some(value) => Some(Self(value)),
                    None => None,
                }
            }
        }

        impl CanonicalRevision for $name {
            fn get(self) -> u64 {
                self.get()
            }

            fn checked_next(self) -> Option<Self> {
                self.checked_next()
            }
        }

        impl From<u64> for $name {
            fn from(value: u64) -> Self {
                Self::new(value)
            }
        }

        impl From<$name> for u64 {
            fn from(value: $name) -> Self {
                value.get()
            }
        }

        impl FromStr for $name {
            type Err = RevisionParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                parse_revision(value).map(Self)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                struct RevisionVisitor;

                impl Visitor<'_> for RevisionVisitor {
                    type Value = $name;

                    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                        formatter.write_str("a canonical u64 decimal string")
                    }

                    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
                    where
                        E: serde::de::Error,
                    {
                        value.parse().map_err(E::custom)
                    }
                }

                deserializer.deserialize_str(RevisionVisitor)
            }
        }
    };
}

revision_type!(
    /// Persistent revision of authority-owned room state.
    RoomRevision
);
revision_type!(
    /// Revision of online presence within one server generation.
    PresenceRevision
);
revision_type!(
    /// Revision of a member's complete source catalog within one run.
    CatalogRevision
);
revision_type!(
    /// Persistent revision of member-owned configuration.
    MemberConfigRevision
);
revision_type!(
    /// Persistent revision of capability grants for one room and member.
    GrantRevision
);
revision_type!(
    /// Revision of a complete direct subscription set.
    SubscriptionRevision
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revisions_use_canonical_decimal_strings() {
        let revision = RoomRevision::new(41);
        assert_eq!(serde_json::to_string(&revision).unwrap(), "\"41\"");
        assert_eq!(
            serde_json::from_str::<RoomRevision>("\"41\"").unwrap(),
            revision
        );
        assert!(serde_json::from_str::<RoomRevision>("41").is_err());
        assert!(serde_json::from_str::<RoomRevision>("\"041\"").is_err());
        assert!(serde_json::from_str::<RoomRevision>("\"-1\"").is_err());
        assert!(serde_json::from_str::<RoomRevision>("\"18446744073709551616\"").is_err());
    }

    #[test]
    fn revision_domains_are_checked() {
        assert_eq!(
            CatalogRevision::new(7).checked_next(),
            Some(CatalogRevision::new(8))
        );
        assert_eq!(GrantRevision::new(u64::MAX).checked_next(), None);
    }
}
