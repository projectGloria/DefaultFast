//! One error type for every fallible operation. Messages are written for the toast: plain
//! language first, the technical detail after.
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{op} {path} failed: {source}")]
    Registry {
        op: &'static str,
        path: String,
        #[source]
        source: windows::core::Error,
    },
    #[error("DefaultFast only changes file-association keys; refused to write {0}")]
    Forbidden(String),
    #[error("{0}")]
    Invalid(String),
    #[error("The admin prompt was cancelled, so the system-wide setting was not changed")]
    ElevationCancelled,
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Os(#[from] windows::core::Error),
}

impl Error {
    /// ACCESS_DENIED from a registry call: either an ACL (UserChoice deny ACE) or the UCPD
    /// driver. Both mean "use Settings", never "seize ownership".
    pub fn is_access_denied(&self) -> bool {
        const E_ACCESSDENIED: i32 = 0x8007_0005u32 as i32;
        match self {
            Error::Registry { source, .. } | Error::Os(source) => source.code().0 == E_ACCESSDENIED,
            _ => false,
        }
    }
    /// Whether the UI should offer "Open Settings" for this failure.
    pub fn wants_settings(&self) -> bool {
        self.is_access_denied()
    }
}

pub type Result<T> = std::result::Result<T, Error>;
