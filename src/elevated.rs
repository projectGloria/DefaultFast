//! Elevated helper (HKLM repairs). Implemented in the repair milestone.
use crate::{backup::Op, error::{Error, Result}};

pub fn restore_machine(_ops: &[Op]) -> Result<()> {
    Err(Error::Invalid("Restoring system-wide (HKLM) values isn't available in this build".into()))
}
