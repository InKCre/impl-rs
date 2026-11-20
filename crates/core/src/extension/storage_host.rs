//! Lease-bound Storage calls from an extension operation to this Core.

use inkcre_extension_sdk::{
    HostStorageCall, HostStorageOperation, HostStorageReply, HostStorageValue,
    MAX_STORAGE_CONTENT_BYTES, PROTOCOL_VERSION, RpcError, StorageBytes,
};

use crate::{Core, knowledge};

tokio::task_local! {
    static BORROWED_STORAGE: ();
}

/// A borrowed handler cannot queue for another extension's in-flight operation.
/// The other extension might itself be waiting on this operation.
pub(super) fn is_borrowed_storage() -> bool {
    BORROWED_STORAGE.try_with(|_| ()).is_ok()
}

pub(super) async fn respond(core: &Core, call: HostStorageCall) -> HostStorageReply {
    let result = BORROWED_STORAGE
        .scope((), execute(core, call.operation))
        .await
        .map_err(public_error);
    HostStorageReply {
        protocol_version: PROTOCOL_VERSION,
        host_reply: call.host_call,
        result,
    }
}

async fn execute(
    core: &Core,
    operation: HostStorageOperation,
) -> knowledge::Result<HostStorageValue> {
    let knowledge = core.knowledge();
    match operation {
        HostStorageOperation::Read { storage, pointer } => {
            let record = core
                .storage_get(storage)
                .await?
                .ok_or(crate::Error::NotFound("storage"))?;
            let bytes = knowledge.read_storage(&record, &pointer, None).await?;
            if bytes.len() > MAX_STORAGE_CONTENT_BYTES {
                return Err(knowledge::Error::InvalidInput(
                    "Storage content exceeds bridge limit",
                ));
            }
            Ok(HostStorageValue::Bytes(StorageBytes(bytes)))
        }
        HostStorageOperation::Create { storage, data } => {
            let mut connection = core.pool().acquire().await?;
            let pointer = knowledge
                .storage_create(&mut connection, storage, &data.0)
                .await?;
            Ok(HostStorageValue::Pointer(pointer))
        }
        HostStorageOperation::Update {
            storage,
            pointer,
            data,
        } => {
            let mut connection = core.pool().acquire().await?;
            let exists = knowledge
                .storage_update(&mut connection, storage, &pointer, &data.0)
                .await?;
            Ok(HostStorageValue::Exists(exists))
        }
        HostStorageOperation::Delete { storage, pointer } => {
            let mut connection = core.pool().acquire().await?;
            let exists = knowledge
                .storage_delete(&mut connection, storage, &pointer)
                .await?;
            Ok(HostStorageValue::Exists(exists))
        }
    }
}

fn public_error(error: knowledge::Error) -> RpcError {
    let code = match error {
        knowledge::Error::UnknownStorage(_) => "storage_unavailable",
        knowledge::Error::Unsupported(_) => "storage_unsupported",
        knowledge::Error::Core(crate::Error::NotFound(_)) => "storage_not_found",
        knowledge::Error::Core(crate::Error::Conflict(_)) => "storage_busy",
        knowledge::Error::InvalidInput(_) => "storage_invalid",
        _ => "storage_failed",
    };
    RpcError::new(code, "host Storage operation failed")
}
