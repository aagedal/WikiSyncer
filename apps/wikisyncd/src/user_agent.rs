//! Shared application User-Agent configuration for every production HTTP client.

use std::env::{self, VarError};
use std::error::Error;
use std::fmt;

/// Environment variable used to identify the operator in outbound requests.
///
/// The value is sent to configured MediaWiki and dump hosts, so it must contain
/// contact information suitable for disclosure to those services, never a secret.
pub const OPERATOR_CONTACT_ENV: &str = "WIKISYNC_OPERATOR_CONTACT";

/// Largest accepted configured operator contact in bytes.
pub const MAX_OPERATOR_CONTACT_BYTES: usize = 256;

const MAX_APPLICATION_USER_AGENT_BYTES: usize = 512;

/// Returns the bounded application User-Agent used by production network clients.
///
/// [`OPERATOR_CONTACT_ENV`] may contain an ASCII email address, URL, or similarly
/// concise public contact. When it is absent, the project repository is used as a
/// safe contact. Invalid configured values fail closed and are never copied into the
/// returned error.
pub fn application_user_agent() -> Result<String, UserAgentConfigError> {
    match env::var(OPERATOR_CONTACT_ENV) {
        Ok(contact) => build_application_user_agent(Some(&contact)),
        Err(VarError::NotPresent) => build_application_user_agent(None),
        Err(VarError::NotUnicode(_)) => Err(UserAgentConfigError::InvalidOperatorContact),
    }
}

fn build_application_user_agent(
    configured_contact: Option<&str>,
) -> Result<String, UserAgentConfigError> {
    let contact = configured_contact.unwrap_or(env!("CARGO_PKG_REPOSITORY"));
    if contact.is_empty()
        || contact.len() > MAX_OPERATOR_CONTACT_BYTES
        || contact.trim() != contact
        || !contact.is_ascii()
        || contact
            .bytes()
            .any(|byte| !(b' '..=b'~').contains(&byte) || matches!(byte, b'(' | b')' | b'\\'))
    {
        return Err(UserAgentConfigError::InvalidOperatorContact);
    }

    let user_agent = format!("WikiSyncer/{} ({contact})", env!("CARGO_PKG_VERSION"));
    if user_agent.len() > MAX_APPLICATION_USER_AGENT_BYTES {
        return Err(UserAgentConfigError::UserAgentTooLong);
    }
    Ok(user_agent)
}

/// Failure to construct the bounded application User-Agent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserAgentConfigError {
    /// The configured value is empty, non-ASCII, malformed, or outside its bound.
    InvalidOperatorContact,
    /// The complete application User-Agent exceeded its defensive bound.
    UserAgentTooLong,
}

impl fmt::Display for UserAgentConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOperatorContact => formatter.write_str(
                "configured operator contact must be 1-256 visible ASCII bytes without surrounding whitespace, parentheses, or backslashes",
            ),
            Self::UserAgentTooLong => {
                formatter.write_str("configured application User-Agent exceeds its size bound")
            }
        }
    }
}

impl Error for UserAgentConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_identifies_the_application_and_safe_public_contact() {
        let user_agent = build_application_user_agent(None).expect("default User-Agent");
        assert_eq!(
            user_agent,
            format!(
                "WikiSyncer/{} ({})",
                env!("CARGO_PKG_VERSION"),
                env!("CARGO_PKG_REPOSITORY")
            )
        );
        assert!(user_agent.len() <= MAX_APPLICATION_USER_AGENT_BYTES);
    }

    #[test]
    fn configured_public_contact_is_applied_exactly_once() {
        let contact = "mailto:operator@example.invalid";
        let user_agent =
            build_application_user_agent(Some(contact)).expect("configured User-Agent");
        assert_eq!(
            user_agent,
            format!("WikiSyncer/{} ({contact})", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn malformed_oversized_and_non_ascii_contacts_are_rejected_without_disclosure() {
        let secret = "token-SHOULD-NOT-LEAK";
        for invalid in [
            "",
            " leading@example.invalid",
            "trailing@example.invalid ",
            "operator@example.invalid\r\nX-Secret: injected",
            "operator(comment)@example.invalid",
            "operator\\name@example.invalid",
            "drift@example.invalid-æ",
        ] {
            let error = build_application_user_agent(Some(invalid)).expect_err("invalid contact");
            assert_eq!(error, UserAgentConfigError::InvalidOperatorContact);
            if !invalid.is_empty() {
                assert!(!error.to_string().contains(invalid));
            }
        }

        let oversized = format!("{secret}{}", "x".repeat(MAX_OPERATOR_CONTACT_BYTES));
        let error = build_application_user_agent(Some(&oversized)).expect_err("oversized contact");
        assert!(!error.to_string().contains(secret));
        assert!(!error.to_string().contains(&oversized));
    }
}
