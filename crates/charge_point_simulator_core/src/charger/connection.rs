use serde::{Deserialize, Serialize};

/// How this connection authenticates to the CSMS. Only HTTP Basic Auth exists today
/// (OCPP Security Profile 1/2); modeled as an enum so TLS client-cert profiles (3) can be
/// added later without breaking existing [`ConnectionProfile`]s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SecurityProfile {
    Basic { password: String },
}

/// Rejected when a Basic Auth password exceeds [`SecurityProfile::MAX_BASIC_PASSWORD_BYTES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasswordTooLong;

impl std::fmt::Display for PasswordTooLong {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "password exceeds the {} byte limit",
            SecurityProfile::MAX_BASIC_PASSWORD_BYTES
        )
    }
}

impl std::error::Error for PasswordTooLong {}

impl SecurityProfile {
    /// The OCPP Basic Auth password limit.
    pub const MAX_BASIC_PASSWORD_BYTES: usize = 40;

    pub fn basic(password: impl Into<String>) -> Result<Self, PasswordTooLong> {
        let password = password.into();
        if password.len() > Self::MAX_BASIC_PASSWORD_BYTES {
            return Err(PasswordTooLong);
        }
        Ok(Self::Basic { password })
    }
}

/// Everything needed to dial a specific CSMS as a specific identity - kept separate from
/// [`super::ChargerConfig`] so the same charger hardware definition can be reused against
/// different CSMS endpoints/credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub csms_url: String,
    pub ocpp_identity: String,
    pub security: SecurityProfile,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_at_the_limit_is_accepted() {
        let password = "a".repeat(SecurityProfile::MAX_BASIC_PASSWORD_BYTES);
        assert!(SecurityProfile::basic(password).is_ok());
    }

    #[test]
    fn a_password_over_the_limit_is_rejected() {
        let password = "a".repeat(SecurityProfile::MAX_BASIC_PASSWORD_BYTES + 1);
        assert_eq!(SecurityProfile::basic(password), Err(PasswordTooLong));
    }

    #[test]
    fn an_empty_password_is_accepted() {
        assert!(SecurityProfile::basic("").is_ok());
    }
}
