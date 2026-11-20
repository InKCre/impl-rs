//! Installed knowledge handlers have an independent process generation.

use super::{
    CALL_TIMEOUT, Error, ExtensionHost, HostState, Participation, Result, Running, adapters,
    public_error, same_selection,
};
use inkcre_extension_sdk::{DISTRIBUTION, KnowledgeStartContext};
use std::sync::atomic::Ordering;

pub(super) enum Slot {
    Ready(Box<Running>),
    Unavailable {
        version: String,
        error: Option<String>,
    },
}

impl Slot {
    fn matches(&self, selection: &Participation) -> bool {
        match self {
            Self::Ready(running) => same_selection(selection, &running.manifest),
            Self::Unavailable { version, .. } => selection.version == *version,
        }
    }

    pub(super) fn error(&self) -> Option<String> {
        match self {
            Self::Ready(_) => None,
            Self::Unavailable { error, .. } => error.clone(),
        }
    }

    pub(super) async fn stop(self) {
        if let Self::Ready(running) = self {
            running.process.stop().await;
        }
    }
}

pub(super) async fn withdraw(state: &mut HostState, name: &str) {
    if let Some(slot) = state.knowledge.remove(name) {
        slot.stop().await;
    }
}

impl ExtensionHost {
    pub(super) async fn reconcile_knowledge(
        &self,
        selections: &[Participation],
        state: &mut HostState,
    ) -> Result<()> {
        let names: Vec<_> = state.knowledge.keys().cloned().collect();
        for name in names {
            let slot = &state.knowledge[&name];
            let selection = selections.iter().find(|selection| {
                selection.name == name
                    && !selection.enabled
                    && selection.distribution.as_deref() == Some(DISTRIBUTION)
                    && slot.matches(selection)
            });
            if selection.is_none() || self.locally_disabled(&name, state).await? {
                withdraw(state, &name).await;
            } else if let Slot::Ready(running) = slot
                && !running.process.is_alive()
            {
                let version = running.manifest.version.to_string();
                let recoverable = running.process.control.restartable.load(Ordering::Acquire);
                withdraw(state, &name).await;
                if !recoverable {
                    state.knowledge.insert(
                        name,
                        Slot::Unavailable {
                            version,
                            error: Some("持久资料读取进程已退出".into()),
                        },
                    );
                }
            }
        }
        Ok(())
    }

    pub(super) async fn ensure_knowledge(&self, selection: &Participation, state: &mut HostState) {
        if selection.enabled
            || selection.distribution.as_deref() != Some(DISTRIBUTION)
            || state
                .knowledge
                .get(&selection.name)
                .is_some_and(|slot| slot.matches(selection))
        {
            return;
        }
        withdraw(state, &selection.name).await;
        let slot = match self.start_knowledge(selection).await {
            Ok(Some(running)) => Slot::Ready(Box::new(running)),
            Ok(None) => Slot::Unavailable {
                version: selection.version.clone(),
                error: None,
            },
            Err(error) => Slot::Unavailable {
                version: selection.version.clone(),
                error: Some(public_error(&error)),
            },
        };
        state.knowledge.insert(selection.name.clone(), slot);
    }

    async fn start_knowledge(&self, selection: &Participation) -> Result<Option<Running>> {
        let running = self
            .open_exact(&selection.name, &selection.version, None)
            .await?;
        let result: Result<bool> = async {
            if !self
                .core
                .participation_update_schema_in_mode(
                    &selection.name,
                    self.peer_id,
                    DISTRIBUTION,
                    &selection.version,
                    running.descriptor.config_schema.clone(),
                    false,
                )
                .await?
            {
                return Err(Error::Conflict("安装期间已改变本机精确选择".into()));
            }
            if !running.descriptor.capabilities.passive_knowledge {
                return Ok(false);
            }
            running
                .process
                .call(
                    "start_knowledge",
                    serde_json::to_value(KnowledgeStartContext {
                        peer_id: self.peer_id.to_string(),
                    })?,
                    Some(CALL_TIMEOUT),
                )
                .await?;
            self.check_knowledge_selection(&running).await?;
            adapters::publish(
                self,
                &running.manifest,
                &running.descriptor,
                &running.process,
                true,
            )
            .await?;
            self.check_knowledge_selection(&running).await?;
            Ok(true)
        }
        .await;
        match result {
            Ok(true) => Ok(Some(running)),
            incomplete => {
                running.process.stop().await;
                incomplete.map(|_| None)
            }
        }
    }

    async fn check_knowledge_selection(&self, running: &Running) -> Result<()> {
        let current = self
            .core
            .participation_get(&running.manifest.name, self.peer_id)
            .await?;
        if !current.as_ref().is_some_and(|selection| {
            !selection.enabled && same_selection(selection, &running.manifest)
        }) {
            return Err(Error::Conflict("持久资料读取启动期间已改变本机选择".into()));
        }
        Ok(())
    }
}
