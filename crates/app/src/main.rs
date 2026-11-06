mod auth;
mod config;
mod error;
mod routes;

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use clap::Parser;
use inkcre_core::{Core, extension::ExtensionHost};
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::watch};

use config::{Cli, Command, ServeArgs};

#[tokio::main]
async fn main() -> std::process::ExitCode {
    if let Err(error) = run().await {
        eprintln!("{error}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

async fn run() -> Result<(), String> {
    config::load_env()?;
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,sqlx=warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    match cli.command {
        Command::Token(args) => {
            println!("{}", auth::issue(&args.jwt_secret, args.lifetime_seconds)?);
            Ok(())
        }
        Command::DbCheck(args) => {
            let core = Core::connect(&args.database_url, args.max_connections)
                .await
                .map_err(|error| error.to_string())?;
            let admission = core.admission().await.map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::to_string(&admission).map_err(|_| "无法序列化准入结果")?
            );
            core.close().await;
            Ok(())
        }
        Command::Serve(args) => serve(args).await,
    }
}

async fn serve(args: ServeArgs) -> Result<(), String> {
    config::validate_secret(&args.jwt_secret)?;
    if !(3..=3600).contains(&args.lease_seconds) {
        return Err("Peer lease 必须为 3..=3600 秒".to_owned());
    }
    if args.request_timeout_seconds == 0 {
        return Err("HTTP 超时必须为正整数秒".to_owned());
    }
    let peer_id = config::stable_peer_id(&args)?;
    let catalog = trusted_catalog(&args.extension_manifest)?;
    let listener = TcpListener::bind(args.bind)
        .await
        .map_err(|_| "无法监听 HTTP 地址")?;
    let core = Core::connect(&args.database.database_url, args.database.max_connections)
        .await
        .map_err(|error| error.to_string())?;
    core.peer_register(
        peer_id,
        &args.peer_name,
        env!("CARGO_PKG_VERSION"),
        &json!({"type":"object","properties":{}}),
    )
    .await
    .map_err(|error| error.to_string())?;
    core.peer_publish_capabilities(peer_id, &[])
        .await
        .map_err(|error| error.to_string())?;
    core.peer_renew_lease(peer_id, args.lease_seconds as i32)
        .await
        .map_err(|error| error.to_string())?;
    let host = ExtensionHost::new(core.clone(), peer_id, args.state_dir.join("extensions"))
        .with_database_url(args.database.database_url.clone());
    let lease_valid = Arc::new(AtomicBool::new(true));
    let state = routes::AppState {
        core: core.clone(),
        host: host.clone(),
        peer_id,
        secret: Arc::from(args.jwt_secret),
        lease_valid: lease_valid.clone(),
        catalog: Arc::new(catalog),
        timeout: Duration::from_secs(args.request_timeout_seconds),
    };
    let (stop_tx, mut stop_rx) = watch::channel(false);
    let mut reconcile_stop = stop_rx.clone();
    let reconcile_host = host.clone();
    let reconcile_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.tick().await;
        loop {
            tokio::select! {
                _=reconcile_stop.changed()=>break,
                _=interval.tick()=> {
                    let result=tokio::select! {
                        _=reconcile_stop.changed()=>break,
                        result=reconcile_host.reconcile()=>result,
                    };
                    if result.is_err() {tracing::warn!("Extension reconciliation failed");}
                }
            }
        }
    });
    let lease_core = core.clone();
    let lease_alive = lease_valid.clone();
    let lease_seconds = args.lease_seconds;
    let lease_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(u64::from(lease_seconds / 3)));
        interval.tick().await;
        loop {
            tokio::select! {
                _=stop_rx.changed()=>break,
                _=interval.tick()=> {
                    let valid=tokio::select! {
                        _=stop_rx.changed()=>break,
                        result=lease_core.peer_renew_lease(peer_id,lease_seconds as i32)=>result.is_ok(),
                    };
                    lease_alive.store(valid,Ordering::Relaxed);
                    if !valid {tracing::warn!("Peer lease renewal failed");}
                }
            }
        }
    });
    // 扩展的初始化可能等待外部 I/O；恢复期间也要续租并允许正常退出。
    let restored = tokio::select! {
        result=host.restore()=>Some(result),
        _=shutdown_signal()=>None,
    };
    match restored {
        Some(Ok(status)) => {
            tracing::info!(peer_id=%peer_id,restored=status.len(),"Peer extensions restored")
        }
        stopped => {
            let _ = stop_tx.send(true);
            let _ = lease_task.await;
            let _ = reconcile_task.await;
            host.shutdown().await;
            let _ = core.peer_clear_lease(peer_id).await;
            core.close().await;
            return if stopped.is_none() {
                Ok(())
            } else {
                Err("扩展恢复失败；请检查数据库与本地产物".to_owned())
            };
        }
    }
    tracing::info!(address=%args.bind,peer_id=%peer_id,"HTTP Peer listening");
    let signal_tx = stop_tx.clone();
    let result = axum::serve(listener, routes::router(state))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            let _ = signal_tx.send(true);
        })
        .await;
    let _ = stop_tx.send(true);
    lease_valid.store(false, Ordering::Relaxed);
    let _ = lease_task.await;
    let _ = reconcile_task.await;
    host.shutdown().await;
    let cleared = core.peer_clear_lease(peer_id).await;
    core.close().await;
    if cleared.is_err() {
        tracing::warn!("Peer lease cleanup failed; lease will expire");
    }
    result.map_err(|_| "HTTP server 已停止并返回错误".to_owned())
}

fn trusted_catalog(paths: &[PathBuf]) -> Result<BTreeMap<(String, String), PathBuf>, String> {
    let mut catalog = BTreeMap::new();
    for path in paths {
        let absolute = path
            .canonicalize()
            .map_err(|_| "无法读取可信扩展 manifest")?;
        let bytes = std::fs::read(&absolute).map_err(|_| "无法读取可信扩展 manifest")?;
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| "可信扩展 manifest 不是有效 JSON")?;
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or("扩展 manifest 缺少 name")?;
        let version = value
            .get("version")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or("扩展 manifest 缺少 version")?;
        if catalog
            .insert((name.to_owned(), version.to_owned()), absolute)
            .is_some()
        {
            return Err("可信扩展清单有重复的 name/version".to_owned());
        }
    }
    Ok(catalog)
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        else {
            let _ = tokio::signal::ctrl_c().await;
            return;
        };
        tokio::select! {_=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{}}
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
