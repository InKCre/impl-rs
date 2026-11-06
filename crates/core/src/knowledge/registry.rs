use super::{Error, Resolver, ResolverDescriptor, Result, StorageDescriptor, StorageHandler};
use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};
use uuid::Uuid;

struct Entry<T: ?Sized> {
    owner: Uuid,
    handler: Arc<T>,
}

/// Per-Core exact registrations. No handler captures this Registry's owning Core.
pub struct Registry {
    resolvers: RwLock<BTreeMap<String, Entry<dyn Resolver>>>,
    storages: RwLock<BTreeMap<String, Entry<dyn StorageHandler>>>,
}

impl Default for Registry {
    fn default() -> Self {
        let mut resolvers: BTreeMap<String, Entry<dyn Resolver>> = BTreeMap::new();
        for handler in super::builtins::resolvers() {
            resolvers.insert(
                handler.descriptor().id,
                Entry {
                    owner: Uuid::nil(),
                    handler,
                },
            );
        }
        let mut storages: BTreeMap<String, Entry<dyn StorageHandler>> = BTreeMap::new();
        for handler in super::storage::handlers() {
            storages.insert(
                handler.descriptor().id,
                Entry {
                    owner: Uuid::nil(),
                    handler,
                },
            );
        }
        Self {
            resolvers: RwLock::new(resolvers),
            storages: RwLock::new(storages),
        }
    }
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register_resolver(&self, owner: Uuid, handler: Arc<dyn Resolver>) -> Result<()> {
        let descriptor = handler.descriptor();
        if owner.is_nil() || descriptor.id.trim().is_empty() {
            return Err(Error::InvalidInput(
                "non-nil registration owner and exact Resolver ID are required",
            ));
        }
        let mut names = std::collections::BTreeSet::new();
        let common = super::common_read_methods();
        for method in &descriptor.read_methods {
            if !(method.name.starts_with("get_") || method.name.starts_with("read_"))
                || !names.insert(&method.name)
            {
                return Err(Error::InvalidInput(
                    "Resolver methods must be unique typed get_/read_ methods",
                ));
            }
            validate_schema(&method.input_schema)?;
            if let Some(standard) = common.iter().find(|standard| standard.name == method.name)
                && standard.input_schema != method.input_schema
            {
                return Err(Error::InvalidInput(
                    "a common Resolver method cannot redefine its argument contract",
                ));
            }
        }
        if let Some(draft) = &descriptor.draft {
            validate_schema(&draft.input_schema)?;
        }
        let mut entries = self
            .resolvers
            .write()
            .map_err(|_| Error::Conflict("Resolver registry is unavailable".into()))?;
        if let Some(existing) = entries.get(&descriptor.id) {
            if existing.owner == owner && Arc::ptr_eq(&existing.handler, &handler) {
                return Ok(());
            }
            return Err(Error::Conflict(format!(
                "Resolver {} is already registered",
                descriptor.id
            )));
        }
        entries.insert(descriptor.id, Entry { owner, handler });
        Ok(())
    }
    pub fn register_storage(&self, owner: Uuid, handler: Arc<dyn StorageHandler>) -> Result<()> {
        let descriptor = handler.descriptor();
        if owner.is_nil()
            || descriptor.id.trim().is_empty()
            || descriptor.writable != handler.writable().is_some()
        {
            return Err(Error::InvalidInput(
                "Storage registration must match its writable capability",
            ));
        }
        validate_schema(&descriptor.config_schema)?;
        let mut entries = self
            .storages
            .write()
            .map_err(|_| Error::Conflict("Storage registry is unavailable".into()))?;
        if let Some(existing) = entries.get(&descriptor.id) {
            if existing.owner == owner && Arc::ptr_eq(&existing.handler, &handler) {
                return Ok(());
            }
            return Err(Error::Conflict(format!(
                "Storage {} is already registered",
                descriptor.id
            )));
        }
        entries.insert(descriptor.id, Entry { owner, handler });
        Ok(())
    }
    /// Revoke only one running generation's adapters; built-ins are permanent.
    pub fn unregister_owner(&self, owner: Uuid) -> Result<()> {
        if owner.is_nil() {
            return Err(Error::InvalidInput(
                "built-in registration owner cannot be revoked",
            ));
        }
        self.resolvers
            .write()
            .map_err(|_| Error::Conflict("Resolver registry is unavailable".into()))?
            .retain(|_, entry| entry.owner != owner);
        self.storages
            .write()
            .map_err(|_| Error::Conflict("Storage registry is unavailable".into()))?
            .retain(|_, entry| entry.owner != owner);
        Ok(())
    }
    pub fn resolver(&self, id: &str) -> Result<Arc<dyn Resolver>> {
        self.resolvers
            .read()
            .map_err(|_| Error::Conflict("Resolver registry is unavailable".into()))?
            .get(id)
            .map(|entry| entry.handler.clone())
            .ok_or_else(|| Error::UnknownResolver(id.into()))
    }
    pub fn storage(&self, id: &str) -> Result<Arc<dyn StorageHandler>> {
        self.storages
            .read()
            .map_err(|_| Error::Conflict("Storage registry is unavailable".into()))?
            .get(id)
            .map(|entry| entry.handler.clone())
            .ok_or_else(|| Error::UnknownStorage(id.into()))
    }
    pub fn resolvers(&self) -> Result<Vec<ResolverDescriptor>> {
        Ok(self
            .resolvers
            .read()
            .map_err(|_| Error::Conflict("Resolver registry is unavailable".into()))?
            .values()
            .map(|entry| entry.handler.descriptor())
            .collect())
    }
    pub fn storages(&self) -> Result<Vec<StorageDescriptor>> {
        Ok(self
            .storages
            .read()
            .map_err(|_| Error::Conflict("Storage registry is unavailable".into()))?
            .values()
            .map(|entry| entry.handler.descriptor())
            .collect())
    }
}

fn validate_schema(schema: &serde_json::Value) -> Result<()> {
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .build(schema)
        .map_err(|_| Error::InvalidInput("capability input schema is invalid"))?;
    Ok(())
}
