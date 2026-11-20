//! Lifecycle-bound dispatch for HTTP routes declared by a loaded Extension.

use super::{CALL_TIMEOUT, Error, ExtensionHost, HostState, Process, Result, same_selection};
use crate::peer::HttpCapability;
use inkcre_extension_sdk::Descriptor;
use std::{collections::HashSet, sync::Arc};

pub use inkcre_extension_sdk::{
    HttpAuth, HttpBody, HttpHeader, HttpRequest, HttpResponse, MAX_HTTP_BODY_BYTES,
};

/// A request admitted by the descriptor of an active process generation.
pub struct HttpDispatch {
    pub auth: HttpAuth,
    name: String,
    method: String,
    path: String,
    process: Arc<Process>,
}

impl ExtensionHost {
    /// Locate a route without reading its body or releasing credentials to the plugin.
    pub async fn admit_http(&self, method: &str, path: &str) -> Option<HttpDispatch> {
        let state = self.inner.lock().await;
        state.running.iter().find_map(|(name, running)| {
            if !running.process.is_alive() {
                return None;
            }
            let ingress = running.descriptor.capabilities.http_ingress.as_ref()?;
            let route = ingress.admit(method, path)?;
            Some(HttpDispatch {
                auth: route.auth,
                name: name.clone(),
                method: method.into(),
                path: path.into(),
                process: running.process.clone(),
            })
        })
    }

    /// Forward after the embedding transport has applied the admitted authentication policy.
    pub async fn dispatch_http(
        &self,
        dispatch: HttpDispatch,
        request: HttpRequest,
    ) -> Result<HttpResponse> {
        if request.method != dispatch.method || request.path != dispatch.path {
            return Err(Error::Invalid(
                "HTTP request differs from the admitted route".into(),
            ));
        }
        request.validate()?;
        let participation = self
            .core
            .participation_get(&dispatch.name, self.peer_id)
            .await?;
        {
            let state = self.inner.lock().await;
            let active = state
                .running
                .get(&dispatch.name)
                .filter(|running| {
                    Arc::ptr_eq(&running.process, &dispatch.process) && running.process.is_alive()
                })
                .ok_or(Error::NotFound)?;
            if !participation
                .as_ref()
                .is_some_and(|row| row.enabled && same_selection(row, &active.manifest))
            {
                return Err(Error::NotFound);
            }
        }
        let value = dispatch
            .process
            .call(
                "http_request",
                serde_json::to_value(request)?,
                Some(CALL_TIMEOUT),
            )
            .await?;
        let response: HttpResponse = serde_json::from_value(value)?;
        response.validate()?;
        Ok(response)
    }

    pub async fn http_inbounds(&self) -> Vec<HttpCapability> {
        let state = self.inner.lock().await;
        let mut snapshot = Vec::new();
        for running in state
            .running
            .values()
            .filter(|running| running.process.is_alive())
        {
            if let Some(ingress) = &running.descriptor.capabilities.http_ingress {
                snapshot.extend(ingress.peer_inbounds.iter().map(|inbound| HttpCapability {
                    id: inbound.capability.clone(),
                    method: inbound.method.clone(),
                    path: inbound.path.clone(),
                }));
            }
        }
        snapshot.sort_by(|left, right| left.id.cmp(&right.id));
        snapshot
    }
}

pub(super) fn check_activation(
    host: &ExtensionHost,
    state: &HostState,
    descriptor: &Descriptor,
) -> Result<()> {
    let Some(ingress) = &descriptor.capabilities.http_ingress else {
        return Ok(());
    };
    ingress.validate_mount(&descriptor.name)?;
    let (_, name) = descriptor.name.split_once('/').ok_or(Error::NotFound)?;
    if host.reserved_http_mounts.contains(name) {
        return Err(Error::Conflict(
            "Extension HTTP mount is reserved by the application".into(),
        ));
    }
    let proposed: HashSet<_> = ingress
        .peer_inbounds
        .iter()
        .map(|inbound| inbound.capability.as_str())
        .collect();
    if proposed.iter().any(|id| id.starts_with("core.")) {
        return Err(Error::Conflict(
            "Extension Peer HTTP capabilities cannot claim the core namespace".into(),
        ));
    }
    for running in state.running.values() {
        if !running.process.is_alive() {
            continue;
        }
        if let Some(other) = &running.descriptor.capabilities.http_ingress
            && (running
                .descriptor
                .name
                .split_once('/')
                .is_some_and(|(_, running)| running == name)
                || other
                    .peer_inbounds
                    .iter()
                    .any(|inbound| proposed.contains(inbound.capability.as_str())))
        {
            return Err(Error::Conflict(
                "Extension HTTP mount or Peer capability is already active".into(),
            ));
        }
    }
    Ok(())
}
