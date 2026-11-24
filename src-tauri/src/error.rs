use serde::{Serialize, Serializer};
use std::fmt;

/// Error type shared by all modules. It serializes to a plain string so the
/// UI can show it directly.
#[derive(Debug)]
pub struct AppError(pub String);

pub type Result<T> = std::result::Result<T, AppError>;

impl AppError {
    pub fn new(msg: impl Into<String>) -> Self {
        AppError(msg.into())
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError(e.to_string())
    }
}

impl From<zip::result::ZipError> for AppError {
    fn from(e: zip::result::ZipError) -> Self {
        AppError(format!("zip error: {e}"))
    }
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        AppError(format!("json error: {e}"))
    }
}

impl From<walkdir::Error> for AppError {
    fn from(e: walkdir::Error) -> Self {
        AppError(e.to_string())
    }
}

impl Serialize for AppError {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

/// Adds context ("while doing X: ...") to any error.
pub trait Context<T> {
    fn context(self, msg: impl AsRef<str>) -> Result<T>;
}

impl<T, E: fmt::Display> Context<T> for std::result::Result<T, E> {
    fn context(self, msg: impl AsRef<str>) -> Result<T> {
        self.map_err(|e| AppError(format!("{}: {e}", msg.as_ref())))
    }
}
