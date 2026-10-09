//! Feed tokens: what the game sends as `Authorization: Bearer <Token>`. The relay keeps only a
//! digest of each one in its routing table and looks the presented token up by digest.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// Minted tokens start with this, so one is easy to tell from a Warcon `wkf_` token in a config.
pub const PREFIX: &str = "cws_";

/// A new feed token: the prefix and 32 random bytes, base64url (47 characters in all).
pub fn mint() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the operating system's random source failed");
    format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
}

pub fn digest(token: &[u8]) -> [u8; 32] {
    Sha256::digest(token).into()
}

/// The token in an `Authorization: Bearer <token>` header, or None for any other shape.
pub fn bearer(header: &[u8]) -> Option<&[u8]> {
    let header = header.trim_ascii();
    let (scheme, rest) = header.split_at_checked(6)?;
    if !scheme.eq_ignore_ascii_case(b"bearer") || !rest.first()?.is_ascii_whitespace() {
        return None;
    }
    let token = rest.trim_ascii();
    (!token.is_empty() && !token.iter().any(u8::is_ascii_whitespace)).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_tokens_are_distinct_and_shaped() {
        let a = mint();
        let b = mint();
        assert_ne!(a, b);
        assert!(a.starts_with(PREFIX));
        assert_eq!(a.len(), PREFIX.len() + 43);
        assert!(
            a[PREFIX.len()..]
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        );
    }

    #[test]
    fn bearer_reads_the_token() {
        assert_eq!(bearer(b"Bearer abc"), Some(&b"abc"[..]));
        assert_eq!(bearer(b"bearer   abc  "), Some(&b"abc"[..]));
        assert_eq!(bearer(b"BEARER\tabc"), Some(&b"abc"[..]));
        assert_eq!(bearer(b"Bearer"), None);
        assert_eq!(bearer(b"Bearer "), None);
        assert_eq!(bearer(b"Bearerabc"), None);
        assert_eq!(bearer(b"Basic abc"), None);
        assert_eq!(bearer(b"Bearer a b"), None);
        assert_eq!(bearer(b""), None);
    }
}
