//! 可嵌入的 Job 执行器；App 决定是否运行、唤醒频率与并发预算。

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::{
    sync::{mpsc, watch},
    task::{JoinHandle, JoinSet},
    time::{MissedTickBehavior, timeout},
};
use uuid::Uuid;

use crate::{Core, Error, Job, JobStatus, JobType, Result};

/// One exact executable contract. Availability is checked before taking ownership.
#[async_trait]
pub trait JobHandler: Send + Sync {
    fn job_type(&self) -> JobType;
    async fn can_handle(&self, core: &Core, job: &Job) -> Result<bool>;
    async fn handle(&self, context: JobContext, job: Job) -> Result<()>;
}

#[derive(Clone)]
struct Registration {
    owner: Uuid,
    generation: Uuid,
    contract: JobType,
    handler: Arc<dyn JobHandler>,
}

#[derive(Default)]
struct RegistryState {
    entries: RwLock<BTreeMap<String, Registration>>,
    revision: AtomicU64,
}

/// Process-local handlers; their catalog rows remain deployment-owned after withdrawal.
#[derive(Clone, Default)]
pub struct JobRegistry(Arc<RegistryState>);

impl JobRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, handler: Arc<dyn JobHandler>) -> Result<()> {
        self.register_owned(Uuid::nil(), handler)
    }

    /// The same owner may replace its adapter; running old generations are cancelled.
    pub fn register_owned(&self, owner: Uuid, handler: Arc<dyn JobHandler>) -> Result<()> {
        let contract = handler.job_type();
        if contract.id.trim().is_empty()
            || contract.default_timeout_seconds <= 0
            || !contract.parameters_schema.is_object()
        {
            return Err(Error::InvalidInput("invalid handler Job contract".into()));
        }
        jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .build(&contract.parameters_schema)
            .map_err(|_| Error::InvalidInput("invalid handler parameters schema".into()))?;
        let mut entries = self.0.entries.write().map_err(|_| registry_error())?;
        if entries
            .get(&contract.id)
            .is_some_and(|entry| entry.owner != owner)
        {
            return Err(Error::Conflict(
                "another module owns this exact Job handler".into(),
            ));
        }
        entries.insert(
            contract.id.clone(),
            Registration {
                owner,
                generation: Uuid::new_v4(),
                contract,
                handler,
            },
        );
        self.0.revision.fetch_add(1, Ordering::Release);
        Ok(())
    }

    pub fn unregister_owner(&self, owner: Uuid) -> Result<usize> {
        let mut entries = self.0.entries.write().map_err(|_| registry_error())?;
        let before = entries.len();
        entries.retain(|_, entry| entry.owner != owner);
        let removed = before - entries.len();
        if removed > 0 {
            self.0.revision.fetch_add(1, Ordering::Release);
        }
        Ok(removed)
    }

    pub fn types(&self) -> Result<Vec<JobType>> {
        Ok(self
            .0
            .entries
            .read()
            .map_err(|_| registry_error())?
            .values()
            .map(|entry| entry.contract.clone())
            .collect())
    }

    pub async fn publish_catalog(&self, core: &Core) -> Result<()> {
        for contract in self.types()? {
            core.job_type_upsert(&contract).await?;
        }
        Ok(())
    }

    fn get(&self, type_: &str) -> Result<Option<Registration>> {
        Ok(self
            .0
            .entries
            .read()
            .map_err(|_| registry_error())?
            .get(type_)
            .cloned())
    }

    fn current(&self, type_: &str, generation: Uuid) -> bool {
        self.get(type_)
            .ok()
            .flatten()
            .is_some_and(|entry| entry.generation == generation)
    }
}

fn registry_error() -> Error {
    Error::Conflict("Job handler registry is unavailable".into())
}

/// Handler checkpoints are persisted immediately; cancellation has no authority to roll them back.
#[derive(Clone)]
pub struct JobContext {
    core: Core,
    job_id: i64,
    cancellation: watch::Receiver<bool>,
    cancel: watch::Sender<bool>,
}

impl JobContext {
    pub fn core(&self) -> &Core {
        &self.core
    }
    pub fn job_id(&self) -> i64 {
        self.job_id
    }
    pub async fn state(&self) -> Result<Value> {
        self.core.job_state(self.job_id).await
    }
    pub async fn set_state(&self, state: Value) -> Result<()> {
        self.core.job_set_state(self.job_id, &state).await
    }
    pub async fn patch_state(&self, patch: Value) -> Result<()> {
        self.core.job_patch_state(self.job_id, &patch).await
    }
    pub fn is_cancelled(&self) -> bool {
        *self.cancellation.borrow()
    }
    /// Withdraw local execution capability without rewriting the shared abort intent.
    pub fn cancel(&self) {
        let _ = self.cancel.send(true);
    }
    pub async fn cancelled(&self) {
        let mut cancellation = self.cancellation.clone();
        while !*cancellation.borrow_and_update() {
            if cancellation.changed().await.is_err() {
                return;
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub concurrency: usize,
    pub poll_interval: Duration,
    pub abort_poll_interval: Duration,
    pub discovery_batch: usize,
    pub shutdown_grace: Duration,
    pub cancellation_grace: Duration,
    /// Disable broad discovery when an embedding application explicitly admits only hints.
    pub discover_pending: bool,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            concurrency: 4,
            poll_interval: Duration::from_secs(1),
            abort_poll_interval: Duration::from_millis(250),
            discovery_batch: 100,
            shutdown_grace: Duration::from_secs(10),
            cancellation_grace: Duration::from_secs(1),
            discover_pending: true,
        }
    }
}

pub struct Worker {
    core: Core,
    registry: JobRegistry,
    config: WorkerConfig,
}

pub struct WorkerHandle {
    accepting: Arc<AtomicBool>,
    stop: watch::Sender<bool>,
    wake: Arc<tokio::sync::Notify>,
    hints: mpsc::Sender<i64>,
    task: Option<JoinHandle<Result<()>>>,
}

/// A cloneable post-commit notification port without shutdown ownership.
#[derive(Clone)]
pub struct WorkerNotifier {
    accepting: Arc<AtomicBool>,
    wake: Arc<tokio::sync::Notify>,
    hints: mpsc::Sender<i64>,
}

impl WorkerNotifier {
    pub fn wake(&self) {
        self.wake.notify_one();
    }
    pub fn notify_job(&self, id: i64) -> bool {
        self.accepting.load(Ordering::Acquire) && self.hints.try_send(id).is_ok()
    }
}

impl WorkerHandle {
    pub fn notifier(&self) -> WorkerNotifier {
        WorkerNotifier {
            accepting: self.accepting.clone(),
            wake: self.wake.clone(),
            hints: self.hints.clone(),
        }
    }
    pub fn wake(&self) {
        self.wake.notify_one();
    }
    /// Post-commit best-effort hint. Periodic discovery remains the normal recovery path.
    pub fn notify_job(&self, id: i64) -> bool {
        self.accepting.load(Ordering::Acquire) && self.hints.try_send(id).is_ok()
    }
    pub async fn shutdown(mut self) -> Result<()> {
        self.accepting.store(false, Ordering::Release);
        let _ = self.stop.send(true);
        match self.task.take() {
            Some(task) => task
                .await
                .map_err(|_| Error::Conflict("Job worker task failed".into()))?,
            None => Ok(()),
        }
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.accepting.store(false, Ordering::Release);
        let _ = self.stop.send(true);
    }
}

impl Worker {
    pub fn new(core: Core, registry: JobRegistry, config: WorkerConfig) -> Result<Self> {
        if config.concurrency == 0
            || config.concurrency > 256
            || config.discovery_batch == 0
            || config.discovery_batch > 1000
            || config.poll_interval.is_zero()
            || config.abort_poll_interval.is_zero()
            || config.shutdown_grace.is_zero()
        {
            return Err(Error::InvalidInput("invalid Job worker budget".into()));
        }
        Ok(Self {
            core,
            registry,
            config,
        })
    }

    pub fn start(self) -> WorkerHandle {
        let accepting = Arc::new(AtomicBool::new(true));
        let (stop, receiver) = watch::channel(false);
        let (hints, hint_receiver) = mpsc::channel(self.config.discovery_batch);
        let wake = Arc::new(tokio::sync::Notify::new());
        let task = tokio::spawn(self.run(receiver, accepting.clone(), wake.clone(), hint_receiver));
        WorkerHandle {
            accepting,
            stop,
            wake,
            hints,
            task: Some(task),
        }
    }

    async fn run(
        self,
        mut stop: watch::Receiver<bool>,
        accepting: Arc<AtomicBool>,
        wake: Arc<tokio::sync::Notify>,
        mut hints: mpsc::Receiver<i64>,
    ) -> Result<()> {
        let mut tick = tokio::time::interval(self.config.poll_interval);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut tasks: JoinSet<(i64, Result<bool>)> = JoinSet::new();
        let mut active = HashSet::new();
        let mut task_jobs: HashMap<tokio::task::Id, i64> = HashMap::new();
        let mut cursor = None;
        let mut published_revision = None;
        loop {
            enum Event {
                Stop,
                Scan,
                Hint(i64),
                Completed,
            }
            let event = tokio::select! {
                biased;
                _=stop.changed()=>Event::Stop,
                completed=tasks.join_next_with_id(),if !tasks.is_empty()=> {
                    match completed {
                        Some(Ok((task_id,(id,result))))=> {
                            task_jobs.remove(&task_id);active.remove(&id);
                            if result.is_err() {tracing::warn!(job_id=id,"Job execution or closure failed; inspect running state and executor before explicit maintenance");}
                        }
                        Some(Err(error))=> {
                            if let Some(id)=task_jobs.remove(&error.id()) {active.remove(&id);tracing::warn!(job_id=id,"Job execution task failed; unfinished running state requires explicit maintenance after the executor stops");}
                        }
                        None=>{}
                    }
                    Event::Completed
                },
                hint=hints.recv(),if active.len()<self.config.concurrency=>match hint {Some(id)=>Event::Hint(id),None=>Event::Stop},
                _=wake.notified()=>Event::Scan,
                _=tick.tick()=>Event::Scan,
            };
            let hinted = match event {
                Event::Stop => break,
                Event::Completed => continue,
                Event::Hint(id) => Some(id),
                Event::Scan => None,
            };
            if !accepting.load(Ordering::Acquire) {
                break;
            }
            let revision = self.registry.0.revision.load(Ordering::Acquire);
            if published_revision != Some(revision) {
                let publish = tokio::select! { _=stop.changed()=>break, result=self.registry.publish_catalog(&self.core)=>result };
                if publish.is_err() {
                    tracing::warn!("Job handler catalog publication failed");
                    continue;
                }
                published_revision = Some(revision);
            }
            let capacity = self.config.concurrency - active.len();
            if capacity == 0 {
                continue;
            }
            let ids = if let Some(id) = hinted {
                vec![id]
            } else if self.config.discover_pending {
                let types = self
                    .registry
                    .types()?
                    .into_iter()
                    .map(|contract| contract.id)
                    .collect::<Vec<_>>();
                let pending = tokio::select! {
                    _=stop.changed()=>break,
                    result=self.core.jobs_pending_for(
                        &types,
                        cursor,
                        self.config.discovery_batch.min(capacity) as i64,
                    )=>result,
                };
                match pending {
                    Ok(rows) => {
                        cursor = rows.last().map(|job| job.id);
                        rows.into_iter().map(|job| job.id).collect()
                    }
                    Err(_) => {
                        tracing::warn!("Job pending discovery failed");
                        continue;
                    }
                }
            } else {
                Vec::new()
            };
            for id in ids {
                if active.len() >= self.config.concurrency {
                    break;
                }
                if !active.insert(id) {
                    continue;
                }
                let core = self.core.clone();
                let registry = self.registry.clone();
                let config = self.config.clone();
                let stop = stop.clone();
                let accepting = accepting.clone();
                let abort = tasks.spawn(async move {
                    (
                        id,
                        execute(core, registry, config, id, stop, accepting).await,
                    )
                });
                task_jobs.insert(abort.id(), id);
            }
        }
        accepting.store(false, Ordering::Release);
        // Each execution observes the same stop channel, cancels its handler and closes independently.
        let drained = timeout(self.config.shutdown_grace, async {
            let mut failed = false;
            while let Some(result) = tasks.join_next().await {
                if !matches!(result, Ok((_, Ok(_)))) {
                    failed = true;
                }
            }
            failed
        })
        .await;
        if drained.is_err() {
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
            return Err(Error::Conflict(
                "Job shutdown budget elapsed; unfinished running rows require explicit maintenance after executors stop".into(),
            ));
        }
        if matches!(drained, Ok(true)) {
            return Err(Error::Conflict(
                "one or more Job closures failed; unfinished running rows require explicit maintenance after executors stop".into(),
            ));
        }
        Ok(())
    }
}

struct HandlerTask(JoinHandle<Result<()>>);
impl Drop for HandlerTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn execute(
    core: Core,
    registry: JobRegistry,
    config: WorkerConfig,
    id: i64,
    mut stop: watch::Receiver<bool>,
    accepting: Arc<AtomicBool>,
) -> Result<bool> {
    let candidate = core.job_get(id).await?;
    let Some(candidate) = candidate.filter(|job| job.status == JobStatus::Pending) else {
        return Ok(false);
    };
    let Some(registration) = registry.get(&candidate.type_)? else {
        return Ok(false);
    };
    let invalid = crate::models::validate_json(
        &registration.contract.parameters_schema,
        &candidate.parameters,
    )
    .is_err();
    if !invalid {
        let eligible = tokio::select! {
            _=stop.changed()=>return Ok(false),
            result=timeout(Duration::from_secs(10),registration.handler.can_handle(&core,&candidate))=>result.map_err(|_|Error::Conflict("Job eligibility check exceeded its budget".into()))??,
        };
        if !eligible {
            return Ok(false);
        }
    }
    if !accepting.load(Ordering::Acquire)
        || !registry.current(&candidate.type_, registration.generation)
    {
        return Ok(false);
    }
    let Some(claimed) = core.job_claim(id).await? else {
        return Ok(false);
    };
    tracing::info!(event="job.started",job_id=id,job_type=%claimed.type_,"Job started");
    if invalid {
        close(
            &core,
            id,
            JobStatus::Failed,
            Some("invalid_persisted_parameters"),
        )
        .await?;
        return Ok(true);
    }
    if !accepting.load(Ordering::Acquire)
        || candidate.parameters != claimed.parameters
        || candidate.type_ != claimed.type_
    {
        close(&core, id, JobStatus::Aborted, None).await?;
        return Ok(true);
    }
    let initial_control = control(&core, id).await?;
    let remaining = initial_control
        .remaining
        .unwrap_or(f64::from(claimed.timeout_seconds))
        .max(0.0);
    if remaining == 0.0 {
        close(&core, id, JobStatus::TimedOut, None).await?;
        return Ok(true);
    }
    if initial_control.status != JobStatus::Running || initial_control.abort_requested {
        close(&core, id, JobStatus::Aborted, None).await?;
        return Ok(true);
    }
    let (cancel, cancellation) = watch::channel(false);
    let context = JobContext {
        core: core.clone(),
        job_id: id,
        cancellation,
        cancel: cancel.clone(),
    };
    let handler = registration.handler.clone();
    let handler_context = context.clone();
    let handler_job = claimed.clone();
    let mut task = HandlerTask(tokio::spawn(async move {
        handler.handle(handler_context, handler_job).await
    }));
    let deadline = tokio::time::sleep(Duration::from_secs_f64(remaining));
    tokio::pin!(deadline);
    let mut tick = tokio::time::interval(config.abort_poll_interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let (status, error) = loop {
        tokio::select! {
            biased;
            _=stop.changed()=>break (JobStatus::Aborted,None),
            _=&mut deadline=>break (JobStatus::TimedOut,None),
            _=context.cancelled()=>break (JobStatus::Aborted,None),
            _=tick.tick()=> {
                if !registry.current(&claimed.type_,registration.generation) {break (JobStatus::Aborted,None);}
                let observation=tokio::select! {
                    _=stop.changed()=>break (JobStatus::Aborted,None),
                    _=&mut deadline=>break (JobStatus::TimedOut,None),
                    result=timeout(Duration::from_secs(3),control(&core,id))=>result,
                };
                match observation {
                    Ok(Ok(control)) if control.status!=JobStatus::Running=>break (if control.status.terminal(){control.status}else{JobStatus::Aborted},None),
                    Ok(Ok(control)) if control.abort_requested=>break (JobStatus::Aborted,None),
                    Ok(Ok(_))=>{},
                    _=>tracing::warn!(job_id=id,"Job cancellation observation unavailable"),
                }
            },
            result=&mut task.0=>break if !registry.current(&claimed.type_,registration.generation) {
                (JobStatus::Aborted,None)
            } else {match result {
                Ok(Ok(()))=>(JobStatus::Finished,None),
                _=>(JobStatus::Failed,Some("handler_failed")),
            }},
        }
    };
    if !task.0.is_finished() {
        let _ = cancel.send(true);
        if timeout(config.cancellation_grace, &mut task.0)
            .await
            .is_err()
        {
            task.0.abort();
            let _ = (&mut task.0).await;
        }
    }
    close(&core, id, status, error).await?;
    Ok(true)
}

#[derive(sqlx::FromRow)]
struct Control {
    status: JobStatus,
    abort_requested: bool,
    remaining: Option<f64>,
}
async fn control(core: &Core, id: i64) -> Result<Control> {
    sqlx::query_as("SELECT status::text AS status,abort_requested,GREATEST(EXTRACT(EPOCH FROM (started_at+timeout_seconds*interval '1 second'-statement_timestamp())),0)::double precision AS remaining FROM inkcre.jobs WHERE id=$1")
        .bind(id).fetch_optional(core.pool()).await?.ok_or(Error::NotFound("job"))
}

async fn close(core: &Core, id: i64, status: JobStatus, error: Option<&str>) -> Result<()> {
    let patch = error
        .map(|error| json!({"error":error}))
        .unwrap_or_else(|| json!({}));
    let closed=timeout(Duration::from_secs(5),sqlx::query("UPDATE inkcre.jobs SET status=$2::text::inkcre.jobstatus,state=state || $3::jsonb WHERE id=$1 AND status='running'")
        .bind(id).bind(status).bind(patch).execute(core.pool())).await
        .map_err(|_|Error::Conflict("Job terminal persistence exceeded its budget".into()))??;
    if closed.rows_affected() > 0 {
        tracing::info!(event="job.closed",job_id=id,status=?status,"Job closed");
    }
    Ok(())
}
