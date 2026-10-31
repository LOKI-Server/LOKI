use std::fmt;
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    InvalidInput,
    Unauthenticated,
    PermissionDenied,
    NotFound,
    Conflict,
    LimitExceeded,
    Unavailable,
    HostFailure,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    status: u16,
    message: String,
}
impl Error {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    pub fn bad(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }
    pub fn unauthorized() -> Self {
        Self::new(401, "Sign in with LOKI to continue.")
    }
    pub fn forbidden() -> Self {
        Self::new(403, "You do not have access to this operation.")
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(500, message)
    }
    pub fn status(&self) -> u16 {
        self.status
    }
    pub fn message(&self) -> &str {
        &self.message
    }
    pub fn kind(&self) -> ErrorKind {
        match self.status {
            400 | 422 => ErrorKind::InvalidInput,
            401 => ErrorKind::Unauthenticated,
            403 => ErrorKind::PermissionDenied,
            404 => ErrorKind::NotFound,
            409 => ErrorKind::Conflict,
            413 | 429 => ErrorKind::LimitExceeded,
            503 => ErrorKind::Unavailable,
            _ => ErrorKind::HostFailure,
        }
    }
    #[cfg(all(
        feature = "guest",
        any(target_arch = "wasm32", test, feature = "testing")
    ))]
    pub(crate) fn from_host(message: String) -> Self {
        #[derive(serde::Deserialize)]
        struct HostError {
            loki_error: u8,
            status: u16,
            message: String,
        }
        if let Ok(error) = serde_json::from_str::<HostError>(&message)
            && error.loki_error == 1
            && (400..600).contains(&error.status)
        {
            return Self::new(error.status, error.message);
        }
        Self::internal(message)
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(f)
    }
}
impl std::error::Error for Error {}
impl From<String> for Error {
    fn from(value: String) -> Self {
        Self::bad(value)
    }
}
impl From<&str> for Error {
    fn from(value: &str) -> Self {
        Self::bad(value)
    }
}
impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Self::internal(format!("Invalid host data: {value}"))
    }
}
