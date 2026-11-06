//! 可信本地 executable 扩展宿主。持久 selection/intent 和进程 running 分别管理。

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex as SyncMutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use inkcre_extension_sdk::{
    Descriptor, InvokeParams, Manifest, PROTOCOL_VERSION, Request, Response, StartContext,
    read_frame, validate_name, write_frame,
};
use semver::Version;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, watch},
    time::{Instant, timeout, timeout_at},
};
use uuid::Uuid;

use crate::{Core, Participation};
use crate::{
    source::{SourceRegistry, SourceRuntime},
    worker::JobRegistry,
};

mod adapters;

const CALL_TIMEOUT: Duration = Duration::from_secs(20);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_EXECUTABLE_BYTES: u64 = 128 * 1024 * 1024;
const MANIFEST_FILENAME: &str = "manifest.json";
const EXECUTABLE_FILENAME: &str = if cfg!(windows) {
    "extension.exe"
} else {
    "extension"
};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("扩展输入无效：{0}")]
    Invalid(String),
    #[error("扩展或本地精确产物不存在")]
    NotFound,
    #[error("扩展状态冲突：{0}")]
    Conflict(String),
    #[error("扩展运行失败：{0}")]
    Runtime(String),
    #[error("扩展请求排队超时，尚未发送")]
    QueueTimeout,
    #[error("扩展返回业务错误（{0}）")]
    Remote(String),
    #[error("扩展文件操作失败")]
    Io(#[from] std::io::Error),
    #[error("扩展协议处理失败")]
    Protocol(#[from] inkcre_extension_sdk::Error),
    #[error("扩展 JSON 无效")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Core(#[from] crate::Error),
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtensionStatus {
    pub name: String,
    pub peer_id: Uuid,
    pub distribution: Option<String>,
    pub version: Option<String>,
    pub enabled: bool,
    pub config_schema: Option<Value>,
    pub supported_distributions: [&'static str; 1],
    pub loaded_version: Option<String>,
    pub running: bool,
    pub locally_disabled: bool,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct ExtensionHost {
    core: Core,
    peer_id: Uuid,
    manifest_dir: PathBuf,
    database_url: Option<Arc<str>>,
    jobs: Option<JobRegistry>,
    sources: Option<SourceRuntime>,
    inner: Arc<Mutex<HostState>>,
}

#[derive(Default)]
struct HostState {
    running: HashMap<String, Running>,
    errors: HashMap<String, String>,
    failed_versions: HashMap<String, String>,
    local_stops: HashSet<String>,
}

struct Running {
    manifest: Manifest,
    descriptor: Descriptor,
    process: Arc<Process>,
}

struct Process {
    control: Arc<ProcessControl>,
    io: Mutex<ProtocolIo>,
}

struct ProtocolIo {
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_id: u64,
}

struct ProcessControl {
    child: SyncMutex<Child>,
    accepting: AtomicBool,
    publication: SyncMutex<Option<Publication>>,
    stop_signal: watch::Sender<bool>,
    restartable: AtomicBool,
}

/// 只记录本次启动的注册身份；registry 中的 adapter 只持 Weak<Process>。
struct Publication {
    owner: Uuid,
    knowledge: Arc<crate::knowledge::Registry>,
    jobs: Option<JobRegistry>,
    sources: Option<SourceRegistry>,
}

impl Publication {
    fn withdraw(self) {
        let _ = self.knowledge.unregister_owner(self.owner);
        if let Some(jobs) = self.jobs {
            let _ = jobs.unregister_owner(self.owner);
        }
        if let Some(sources) = self.sources {
            let _ = sources.unregister_owner(self.owner);
        }
    }
}

impl ProcessControl {
    fn cancel(&self) {
        self.restartable.store(true, Ordering::Release);
        self.fail();
    }

    fn fail(&self) {
        self.accepting.store(false, Ordering::Release);
        let publication = self
            .publication
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(publication) = publication {
            publication.withdraw();
        }
        let _ = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .start_kill();
    }

    fn reaped(&self) -> std::io::Result<bool> {
        Ok(self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .try_wait()?
            .is_some())
    }
}

impl Drop for ProcessControl {
    fn drop(&mut self) {
        self.fail();
    }
}

/// future 被取消时撤销业务能力并立即结束实际子进程；不留下失配的协议流。
struct KillOnCancel {
    control: Arc<ProcessControl>,
    completed: bool,
}

impl Drop for KillOnCancel {
    fn drop(&mut self) {
        if !self.completed {
            self.control.cancel();
        }
    }
}

impl Process {
    async fn spawn(executable: &Path, database_url: Option<&str>) -> Result<Self> {
        let mut command = Command::new(executable);
        command
            .current_dir(
                executable
                    .parent()
                    .ok_or_else(|| Error::Invalid("无执行目录".into()))?,
            )
            .env_clear();
        if let Some(database_url) = database_url {
            command.env("INKCRE_DATABASE_URL", database_url);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| Error::Runtime("stdin 不可用".into()))?;
        let output = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| Error::Runtime("stdout 不可用".into()))?,
        );
        Ok(Self {
            control: Arc::new(ProcessControl {
                child: SyncMutex::new(child),
                accepting: AtomicBool::new(true),
                publication: SyncMutex::new(None),
                stop_signal: watch::channel(false).0,
                restartable: AtomicBool::new(false),
            }),
            io: Mutex::new(ProtocolIo {
                input,
                output,
                next_id: 1,
            }),
        })
    }

    fn is_alive(&self) -> bool {
        if !self.control.accepting.load(Ordering::Acquire) {
            return false;
        }
        if self.control.reaped().unwrap_or(true) {
            self.control.fail();
            return false;
        }
        true
    }

    async fn call(&self, method: &str, params: Value, duration: Option<Duration>) -> Result<Value> {
        let deadline = duration.map(|duration| Instant::now() + duration);
        if !self.is_alive() {
            return Err(Error::Runtime("扩展进程已不可用".into()));
        }
        // 排队尚未取得协议流，超时只撤销这个等待者，不触发进程取消 guard。
        let mut io = match deadline {
            Some(deadline) => timeout_at(deadline, self.io.lock())
                .await
                .map_err(|_| Error::QueueTimeout)?,
            None => self.io.lock().await,
        };
        let remaining = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        if remaining.is_some_and(|remaining| remaining.is_zero()) {
            return Err(Error::QueueTimeout);
        }
        if !self.is_alive() {
            return Err(Error::Runtime("扩展进程已不可用".into()));
        }
        self.exchange(&mut io, method, params, remaining).await
    }

    async fn try_call(&self, method: &str, params: Value) -> Result<Option<Value>> {
        if !self.is_alive() {
            return Err(Error::Runtime("扩展进程已不可用".into()));
        }
        let Ok(mut io) = self.io.try_lock() else {
            return Ok(None);
        };
        self.exchange(&mut io, method, params, Some(CALL_TIMEOUT))
            .await
            .map(Some)
    }

    fn is_idle(&self) -> bool {
        self.is_alive() && self.io.try_lock().is_ok()
    }

    async fn stopped(&self) {
        let mut signal = self.control.stop_signal.subscribe();
        while !*signal.borrow_and_update() {
            if signal.changed().await.is_err() {
                return;
            }
        }
    }

    async fn exchange(
        &self,
        io: &mut ProtocolIo,
        method: &str,
        params: Value,
        duration: Option<Duration>,
    ) -> Result<Value> {
        let id = io.next_id;
        io.next_id = io
            .next_id
            .checked_add(1)
            .ok_or_else(|| Error::Runtime("请求 ID 耗尽".into()))?;
        let request = Request {
            protocol_version: PROTOCOL_VERSION,
            id,
            method: method.into(),
            params,
        };
        let mut cancellation = KillOnCancel {
            control: self.control.clone(),
            completed: false,
        };
        let operation = async {
            write_frame(&mut io.input, &request).await?;
            let frame = read_frame(&mut io.output)
                .await?
                .ok_or_else(|| Error::Runtime("扩展提前退出".into()))?;
            let raw: Value = serde_json::from_slice(&frame)?;
            // 显式区分成功的 null 和缺失 result；serde Option 单独无法表达此差异。
            let object = raw
                .as_object()
                .ok_or_else(|| Error::Runtime("响应必须是对象".into()))?;
            if object.contains_key("result") == object.contains_key("error") {
                return Err(Error::Runtime("响应必须恰有 result 或 error".into()));
            }
            if object.get("error").is_some_and(|error| !error.is_object()) {
                return Err(Error::Runtime("响应 error 必须是对象".into()));
            }
            let response: Response = serde_json::from_value(raw)?;
            if response.id != id || response.protocol_version != PROTOCOL_VERSION {
                return Err(Error::Runtime("响应身份或协议版本不匹配".into()));
            }
            match response.error {
                Some(error) => Err(Error::Remote(error.code)),
                None => Ok(response.result.unwrap_or(Value::Null)),
            }
        };
        let result = if let Some(duration) = duration {
            match timeout(duration, operation).await {
                Ok(result) => result,
                Err(_) => Err(Error::Runtime("扩展请求超时".into())),
            }
        } else {
            operation.await
        };
        if result
            .as_ref()
            .is_err_and(|error| !matches!(error, Error::Remote(_)))
        {
            self.control.fail();
        }
        cancellation.completed = true;
        result
    }

    async fn terminate(&self) {
        self.control.fail();
        let _ = timeout(STOP_TIMEOUT, async {
            while !self.control.reaped().unwrap_or(true) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
    }

    async fn stop(&self) {
        self.control.stop_signal.send_replace(true);
        self.control.accepting.store(false, Ordering::Release);
        let publication = self
            .control
            .publication
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(publication) = publication {
            publication.withdraw();
        }
        // 长任务持有 RPC 锁时不等待它自然结束；撤销能力后直接终止进程。
        if let Ok(mut io) = self.io.try_lock() {
            let _ = self
                .exchange(&mut io, "stop", Value::Null, Some(STOP_TIMEOUT))
                .await;
        }
        self.terminate().await;
    }
}

impl ExtensionHost {
    pub fn new(core: Core, peer_id: Uuid, manifest_dir: PathBuf) -> Self {
        Self {
            core,
            peer_id,
            manifest_dir,
            database_url: None,
            jobs: None,
            sources: None,
            inner: Arc::new(Mutex::new(HostState::default())),
        }
    }

    /// 将执行端的已准入连接能力显式传给可信插件。不会写入manifest或状态响应。
    pub fn with_database_url(mut self, database_url: String) -> Self {
        self.database_url = Some(database_url.into());
        self
    }

    pub fn with_jobs(mut self, jobs: JobRegistry) -> Self {
        self.jobs = Some(jobs);
        self
    }

    pub fn with_sources(mut self, sources: SourceRuntime) -> Self {
        self.sources = Some(sources);
        self
    }

    fn installed_dir(&self, name: &str, version: &str) -> Result<PathBuf> {
        validate_name(name)?;
        let parsed = Version::parse(version).map_err(|_| Error::Invalid("版本无效".into()))?;
        if !parsed.build.is_empty() {
            return Err(Error::Invalid("版本不能包含 build metadata".into()));
        }
        Ok(self.manifest_dir.join(name).join(version))
    }

    /// 仅由本机明确配置的可信路径调用；不把任意 HTTP 路径变为执行权限。
    /// 安装只保存当前 Peer 的精确 selection，不自动启用。
    pub async fn install(
        &self,
        manifest_path: PathBuf,
        expected_name: &str,
        expected_version: &str,
    ) -> Result<ExtensionStatus> {
        let mut state = self.inner.lock().await;
        let bytes = read_bounded(&manifest_path, MAX_MANIFEST_BYTES).await?;
        let mut manifest: Manifest = serde_json::from_slice(&bytes)?;
        manifest.validate()?;
        if manifest.name != expected_name || manifest.version.to_string() != expected_version {
            return Err(Error::Conflict(
                "manifest 与请求的精确安装坐标不一致".into(),
            ));
        }
        let selected = self
            .core
            .participation_get(&manifest.name, self.peer_id)
            .await?;
        if selected.as_ref().is_some_and(|p| {
            p.enabled
                && (p.distribution.as_deref() != Some(inkcre_extension_sdk::DISTRIBUTION)
                    || p.version != manifest.version.to_string())
        }) {
            return Err(Error::Conflict("请先停用再更换精确版本".into()));
        }
        let source_dir = tokio::fs::canonicalize(
            manifest_path
                .parent()
                .ok_or_else(|| Error::Invalid("manifest 需要父目录".into()))?,
        )
        .await?;
        let source = tokio::fs::canonicalize(source_dir.join(&manifest.executable)).await?;
        if !source.starts_with(&source_dir) || !tokio::fs::metadata(&source).await?.is_file() {
            return Err(Error::Invalid("executable 必须位于 manifest 目录内".into()));
        }
        let destination = self.installed_dir(&manifest.name, &manifest.version.to_string())?;
        manifest.executable = EXECUTABLE_FILENAME.into();
        if tokio::fs::try_exists(&destination).await? {
            let installed = self
                .read_installed(&manifest.name, &manifest.version.to_string())
                .await?;
            if installed != manifest {
                return Err(Error::Conflict("已安装坐标不可覆盖；请使用新版本".into()));
            }
        } else {
            tokio::fs::create_dir_all(&self.manifest_dir).await?;
            let temporary = self
                .manifest_dir
                .join(format!(".install-{}", Uuid::new_v4()));
            tokio::fs::create_dir(&temporary).await?;
            let result: Result<()> = async {
                let binary = temporary.join(EXECUTABLE_FILENAME);
                if tokio::fs::metadata(&source).await?.len() > MAX_EXECUTABLE_BYTES {
                    return Err(Error::Invalid("executable 超过 128 MiB".into()));
                }
                tokio::fs::copy(&source, &binary).await?;
                verify_digest(&binary, &manifest.sha256).await?;
                tokio::fs::write(
                    temporary.join(MANIFEST_FILENAME),
                    serde_json::to_vec_pretty(&manifest)?,
                )
                .await?;
                tokio::fs::create_dir_all(
                    destination
                        .parent()
                        .ok_or_else(|| Error::Invalid("安装目录无效".into()))?,
                )
                .await?;
                tokio::fs::rename(&temporary, &destination).await?;
                Ok(())
            }
            .await;
            if result.is_err() {
                let _ = tokio::fs::remove_dir_all(&temporary).await;
            }
            result?;
        }
        self.core.extension_ensure(&manifest.name).await?;
        self.core
            .participation_select(
                &manifest.name,
                self.peer_id,
                inkcre_extension_sdk::DISTRIBUTION,
                &manifest.version.to_string(),
                None,
            )
            .await?;
        state.errors.remove(&manifest.name);
        state.failed_versions.remove(&manifest.name);
        self.status_locked(&manifest.name, &mut state).await
    }

    async fn read_installed(&self, name: &str, version: &str) -> Result<Manifest> {
        let directory = self.installed_dir(name, version)?;
        let bytes = read_bounded(&directory.join(MANIFEST_FILENAME), MAX_MANIFEST_BYTES)
            .await
            .map_err(|error| match error {
                Error::Io(ref io) if io.kind() == std::io::ErrorKind::NotFound => Error::NotFound,
                _ => error,
            })?;
        let manifest: Manifest = serde_json::from_slice(&bytes)?;
        manifest.validate()?;
        if manifest.name != name
            || manifest.version.to_string() != version
            || manifest.executable != EXECUTABLE_FILENAME
        {
            return Err(Error::Conflict("本地产物与精确选择不一致".into()));
        }
        verify_digest(&directory.join(&manifest.executable), &manifest.sha256).await?;
        Ok(manifest)
    }

    pub async fn enable(&self, name: &str) -> Result<ExtensionStatus> {
        validate_name(name)?;
        let mut state = self.inner.lock().await;
        self.core
            .participation_set_enabled(name, self.peer_id, true)
            .await?;
        self.clear_local_stop(name, &mut state).await?;
        let attempted = self.core.participation_get(name, self.peer_id).await?;
        if let Err(error) = self.start_locked(name, &mut state).await {
            state.errors.insert(name.into(), public_error(&error));
            if let Some(selected) = attempted {
                state.failed_versions.insert(name.into(), selected.version);
            }
            return Err(error);
        }
        self.status_locked(name, &mut state).await
    }

    async fn start_locked(&self, name: &str, state: &mut HostState) -> Result<()> {
        if self.locally_disabled(name, state).await? {
            return Err(Error::Conflict("本机扩展已停用，请显式启用".into()));
        }
        let selected = self
            .core
            .participation_get(name, self.peer_id)
            .await?
            .ok_or(Error::NotFound)?;
        if !selected.enabled
            || selected.distribution.as_deref() != Some(inkcre_extension_sdk::DISTRIBUTION)
        {
            return Err(Error::Conflict(
                "当前 Peer 未启用 rust_stdio 精确选择".into(),
            ));
        }
        if let Some(running) = state.running.get(name) {
            if running.process.is_alive() {
                if running.manifest.version.to_string() == selected.version {
                    return Ok(());
                }
                return Err(Error::Conflict("另一个版本仍在运行".into()));
            }
            state.running.remove(name);
        }
        let manifest = self.read_installed(name, &selected.version).await?;
        let process = Arc::new(
            Process::spawn(
                &self
                    .installed_dir(name, &selected.version)?
                    .join(&manifest.executable),
                self.database_url.as_deref(),
            )
            .await?,
        );
        let result: Result<Descriptor> = async {
            let descriptor: Descriptor = serde_json::from_value(
                process
                    .call(
                        "initialize",
                        json!({"host_sdk_version":inkcre_extension_sdk::SDK_VERSION}),
                        Some(CALL_TIMEOUT),
                    )
                    .await?,
            )?;
            if descriptor.name != manifest.name
                || descriptor.version != manifest.version
                || descriptor.protocol_version != PROTOCOL_VERSION
                || !manifest.host_sdk_version.matches(&descriptor.sdk_version)
            {
                return Err(Error::Conflict("扩展 handshake 与 manifest 不一致".into()));
            }
            if !descriptor.config_schema.is_object() {
                return Err(Error::Invalid("config schema 必须为对象".into()));
            }
            descriptor.capabilities.validate()?;
            // schema 描述已验证的精确产物，不能依赖成功 start 才能编辑必填配置。
            if !self
                .core
                .participation_update_schema(
                    name,
                    self.peer_id,
                    inkcre_extension_sdk::DISTRIBUTION,
                    &selected.version,
                    descriptor.config_schema.clone(),
                )
                .await?
            {
                return Err(Error::Conflict("初始化期间精确选择或启用意图已改变".into()));
            }
            let shared = self
                .core
                .extension_get(name)
                .await?
                .ok_or(Error::NotFound)?;
            crate::models::validate_json(&descriptor.config_schema, &shared.config)?;
            let context = StartContext {
                peer_id: self.peer_id.to_string(),
                config: shared.config,
                state: shared.state,
            };
            process
                .call("start", serde_json::to_value(context)?, Some(CALL_TIMEOUT))
                .await?;
            let current = self
                .core
                .participation_get(name, self.peer_id)
                .await?
                .ok_or_else(|| Error::Conflict("启动期间参与记录已移除".into()))?;
            if !same_selection(&current, &manifest) || !current.enabled {
                return Err(Error::Conflict(
                    "启动期间 selection 或 enabled 已改变".into(),
                ));
            }
            adapters::publish(self, &manifest, &descriptor, &process).await?;
            let current = self
                .core
                .participation_get(name, self.peer_id)
                .await?
                .ok_or_else(|| Error::Conflict("能力发布期间参与记录已移除".into()))?;
            if !same_selection(&current, &manifest) || !current.enabled {
                return Err(Error::Conflict(
                    "能力发布期间 selection 或 enabled 已改变".into(),
                ));
            }
            Ok(descriptor)
        }
        .await;
        match result {
            Ok(descriptor) => {
                state.errors.remove(name);
                state.failed_versions.remove(name);
                state.running.insert(
                    name.into(),
                    Running {
                        manifest,
                        descriptor,
                        process,
                    },
                );
                Ok(())
            }
            Err(error) => {
                process.stop().await;
                Err(error)
            }
        }
    }

    pub async fn disable(&self, name: &str) -> Result<ExtensionStatus> {
        validate_name(name)?;
        let mut state = self.inner.lock().await;
        self.stop_locally(name, &mut state).await?;
        if let Err(error) = self
            .core
            .participation_set_enabled(name, self.peer_id, false)
            .await
        {
            state
                .errors
                .insert(name.into(), "本机已停止，共享停用意图尚未提交".into());
            return Err(error.into());
        }
        self.clear_local_stop(name, &mut state).await?;
        state.errors.remove(name);
        state.failed_versions.remove(name);
        self.status_locked(name, &mut state).await
    }

    pub async fn unselect(&self, name: &str) -> Result<ExtensionStatus> {
        validate_name(name)?;
        let mut state = self.inner.lock().await;
        self.stop_locally(name, &mut state).await?;
        self.core
            .participation_set_enabled(name, self.peer_id, false)
            .await?;
        self.core.participation_delete(name, self.peer_id).await?;
        self.clear_local_stop(name, &mut state).await?;
        state.errors.remove(name);
        state.failed_versions.remove(name);
        self.status_locked(name, &mut state).await
    }

    pub async fn invoke(&self, name: &str, method: &str, params: Value) -> Result<Value> {
        validate_name(name)?;
        let mut state = self.inner.lock().await;
        self.reconcile_locked(&mut state).await?;
        let running = state
            .running
            .get(name)
            .ok_or_else(|| Error::Conflict("扩展尚未运行".into()))?;
        if !running
            .descriptor
            .methods
            .iter()
            .any(|known| known == method)
        {
            return Err(Error::Invalid("扩展未声明此方法".into()));
        }
        let process = running.process.clone();
        let loaded_version = running.manifest.version.to_string();
        drop(state);
        let result = process
            .call(
                "invoke",
                serde_json::to_value(InvokeParams {
                    method: method.into(),
                    params,
                })?,
                Some(CALL_TIMEOUT),
            )
            .await;
        if let Err(error) = &result
            && !matches!(error, Error::Remote(_) | Error::QueueTimeout)
        {
            let mut state = self.inner.lock().await;
            if state
                .running
                .get(name)
                .is_some_and(|running| Arc::ptr_eq(&running.process, &process))
            {
                state.running.remove(name);
                process.terminate().await;
                state.errors.insert(name.into(), public_error(error));
                if let Some(selected) = self.core.participation_get(name, self.peer_id).await?
                    && selected.version == loaded_version
                    && selected.distribution.as_deref() == Some(inkcre_extension_sdk::DISTRIBUTION)
                {
                    state.failed_versions.insert(name.into(), selected.version);
                }
            }
        }
        result
    }

    /// 冷启动仅消费已有 enabled intent；不会在恢复过程中写回 true。
    pub async fn restore(&self) -> Result<Vec<ExtensionStatus>> {
        let mut state = self.inner.lock().await;
        let selected = self.core.participations(self.peer_id).await?;
        for participation in selected.iter().filter(|p| {
            p.enabled && p.distribution.as_deref() == Some(inkcre_extension_sdk::DISTRIBUTION)
        }) {
            if let Err(error) = self.start_locked(&participation.name, &mut state).await {
                state
                    .errors
                    .insert(participation.name.clone(), public_error(&error));
                state
                    .failed_versions
                    .insert(participation.name.clone(), participation.version.clone());
            }
        }
        self.list_locked(&mut state).await
    }

    pub async fn list(&self) -> Result<Vec<ExtensionStatus>> {
        let mut state = self.inner.lock().await;
        self.list_locked(&mut state).await
    }

    async fn list_locked(&self, state: &mut HostState) -> Result<Vec<ExtensionStatus>> {
        self.reconcile_locked(state).await?;
        let selected = self.core.participations(self.peer_id).await?;
        let mut result = Vec::with_capacity(selected.len());
        for participation in selected {
            result.push(self.status_locked(&participation.name, state).await?);
        }
        Ok(result)
    }

    /// 可在 Peer 心跳中调用，撤销远端已移除/停用/换版本的迟到运行实例。
    pub async fn reconcile(&self) -> Result<()> {
        let mut state = self.inner.lock().await;
        self.reconcile_locked(&mut state).await
    }

    async fn reconcile_locked(&self, state: &mut HostState) -> Result<()> {
        let names: Vec<_> = state.running.keys().cloned().collect();
        for name in names {
            let selected = self.core.participation_get(&name, self.peer_id).await?;
            let locally_disabled = self.locally_disabled(&name, state).await?;
            let running = state.running.get(&name).ok_or(Error::NotFound)?;
            let exited = !running.process.is_alive();
            let recoverable_stop = running.process.control.restartable.load(Ordering::Acquire);
            let failed_version = selected
                .as_ref()
                .filter(|selected| !recoverable_stop && same_selection(selected, &running.manifest))
                .map(|selected| selected.version.clone());
            let stale = locally_disabled
                || !selected
                    .as_ref()
                    .is_some_and(|p| p.enabled && same_selection(p, &running.manifest));
            if exited || stale {
                if let Some(running) = state.running.remove(&name) {
                    running.process.stop().await;
                }
                if exited && recoverable_stop {
                    state.errors.remove(&name);
                    state.failed_versions.remove(&name);
                } else if exited {
                    state.errors.insert(name.clone(), "扩展进程已退出".into());
                    if let Some(version) = failed_version {
                        state.failed_versions.insert(name, version);
                    }
                }
            }
        }
        for selected in self.core.participations(self.peer_id).await? {
            if self.locally_disabled(&selected.name, state).await? {
                if selected.enabled {
                    if self
                        .core
                        .participation_set_enabled(&selected.name, self.peer_id, false)
                        .await
                        .is_err()
                    {
                        state.errors.insert(
                            selected.name.clone(),
                            "本机已停止，共享停用意图尚未提交".into(),
                        );
                    } else {
                        self.clear_local_stop(&selected.name, state).await?;
                        state.errors.remove(&selected.name);
                        state.failed_versions.remove(&selected.name);
                    }
                } else {
                    self.clear_local_stop(&selected.name, state).await?;
                    state.errors.remove(&selected.name);
                    state.failed_versions.remove(&selected.name);
                }
                continue;
            }
            if !selected.enabled {
                state.errors.remove(&selected.name);
                state.failed_versions.remove(&selected.name);
            } else if selected.distribution.as_deref() == Some(inkcre_extension_sdk::DISTRIBUTION)
                && !state.running.contains_key(&selected.name)
                && state.failed_versions.get(&selected.name) != Some(&selected.version)
                && let Err(error) = self.start_locked(&selected.name, state).await
            {
                state
                    .errors
                    .insert(selected.name.clone(), public_error(&error));
                state
                    .failed_versions
                    .insert(selected.name, selected.version);
            }
        }
        Ok(())
    }

    async fn status_locked(&self, name: &str, state: &mut HostState) -> Result<ExtensionStatus> {
        let selected = self.core.participation_get(name, self.peer_id).await?;
        let locally_disabled = self.locally_disabled(name, state).await?;
        let running = state
            .running
            .get(name)
            .filter(|running| running.process.is_alive());
        Ok(ExtensionStatus {
            name: name.into(),
            peer_id: self.peer_id,
            distribution: selected.as_ref().and_then(|p| p.distribution.clone()),
            version: selected.as_ref().map(|p| p.version.clone()),
            enabled: selected.as_ref().is_some_and(|p| p.enabled),
            config_schema: selected.and_then(|p| p.config_schema),
            supported_distributions: [inkcre_extension_sdk::DISTRIBUTION],
            loaded_version: running.map(|r| r.manifest.version.to_string()),
            running: running.is_some(),
            locally_disabled,
            error: state.errors.get(name).cloned(),
        })
    }

    /// 退出仅撤销运行实例，保留可恢复的 enabled intent 与共享业务数据。
    pub async fn shutdown(&self) {
        let mut state = self.inner.lock().await;
        for (_, running) in state.running.drain() {
            running.process.stop().await;
        }
    }

    fn stop_marker(&self, name: &str) -> PathBuf {
        self.manifest_dir
            .join(".disabled")
            .join(self.peer_id.to_string())
            .join(name)
    }

    async fn locally_disabled(&self, name: &str, state: &HostState) -> Result<bool> {
        Ok(state.local_stops.contains(name)
            || tokio::fs::try_exists(self.stop_marker(name)).await?)
    }

    async fn clear_local_stop(&self, name: &str, state: &mut HostState) -> Result<()> {
        match tokio::fs::remove_file(self.stop_marker(name)).await {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        state.local_stops.remove(name);
        Ok(())
    }

    async fn stop_locally(&self, name: &str, state: &mut HostState) -> Result<()> {
        state.local_stops.insert(name.into());
        let marker = self.stop_marker(name);
        let persisted: Result<()> = async {
            tokio::fs::create_dir_all(
                marker
                    .parent()
                    .ok_or_else(|| Error::Invalid("本机停用路径无效".into()))?,
            )
            .await?;
            tokio::fs::write(&marker, b"disabled\n").await?;
            Ok(())
        }
        .await;
        // 即便本机磁盘或共享连接失败，也先停止已经运行的原生进程。
        if let Some(running) = state.running.remove(name) {
            running.process.stop().await;
        }
        persisted
    }
}

fn same_selection(participation: &Participation, manifest: &Manifest) -> bool {
    participation.distribution.as_deref() == Some(inkcre_extension_sdk::DISTRIBUTION)
        && participation.version == manifest.version.to_string()
}

fn public_error(error: &Error) -> String {
    match error {
        Error::Remote(_) => "扩展返回业务错误".into(),
        _ => error.to_string(),
    }
}

async fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = tokio::fs::File::open(path).await?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).await?;
    if bytes.len() as u64 > limit {
        return Err(Error::Invalid("文件超过大小上限".into()));
    }
    Ok(bytes)
}

async fn verify_digest(path: &Path, expected: &str) -> Result<()> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let length = file.read(&mut buffer).await?;
        if length == 0 {
            break;
        }
        total += length as u64;
        if total > MAX_EXECUTABLE_BYTES {
            return Err(Error::Invalid("executable 超过大小上限".into()));
        }
        digest.update(&buffer[..length]);
    }
    if format!("{:x}", digest.finalize()) != expected {
        return Err(Error::Invalid("executable SHA-256 不匹配".into()));
    }
    Ok(())
}
