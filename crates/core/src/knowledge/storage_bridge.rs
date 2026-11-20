//! Optional fallback for a Storage handler owned by another process on the same Peer.

use inkcre_extension_sdk::{HostStorageOperation, HostStorageValue, host_storage};

use super::{Error, Result};

pub(super) async fn call(
    operation: HostStorageOperation,
    unavailable: Error,
) -> Result<HostStorageValue> {
    match host_storage(operation).await {
        None => Err(unavailable),
        Some(Ok(value)) => Ok(value),
        Some(Err(error)) => Err(match error.code.as_str() {
            "storage_unavailable" => unavailable,
            "storage_not_found" => crate::Error::NotFound("storage").into(),
            "storage_unsupported" | "storage_read_only" => {
                Error::Unsupported("host Storage operation".into())
            }
            "storage_busy" => Error::Conflict("host Storage provider is busy".into()),
            "storage_invalid" | "storage_too_large" => {
                Error::InvalidInput("host Storage input failed")
            }
            _ => Error::Unsupported("host Storage operation failed".into()),
        }),
    }
}
