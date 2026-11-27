use std::time::{Duration, SystemTime};

pub const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
    expires: Option<SystemTime>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("session_token", &self.session_token.is_some())
            .field("expires", &self.expires)
            .finish_non_exhaustive()
    }
}

impl Credentials {
    pub fn new(
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
        session_token: Option<String>,
        expires: Option<SystemTime>,
    ) -> Self {
        let credentials = Self {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
            session_token: session_token.filter(|token| !token.is_empty()),
            expires,
        };
        assert!(!credentials.access_key_id.is_empty());
        assert!(!credentials.secret_access_key.is_empty());
        credentials
    }

    pub fn access_key_id(&self) -> &str {
        &self.access_key_id
    }

    pub fn secret_access_key(&self) -> &str {
        &self.secret_access_key
    }

    pub fn session_token(&self) -> Option<&str> {
        self.session_token.as_deref()
    }

    pub const fn expires(&self) -> Option<SystemTime> {
        self.expires
    }

    pub(crate) fn is_fresh(&self, now: SystemTime) -> bool {
        self.expires.is_none_or(|expires| {
            expires
                .duration_since(now)
                .is_ok_and(|left| left > REFRESH_MARGIN)
        })
    }
}
