use std::{net::SocketAddr, path::PathBuf};

use clap::{Args, Parser, Subcommand};
use uuid::Uuid;

#[derive(Parser)]
#[command(name = "inkcre", version, about = "InKCre Rust HTTP Peer")]
pub struct Cli {
    /// 以 dotenv 数据格式加载配置；不执行 shell 内容
    #[arg(long, global = true)]
    pub env_file: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// 启动 HTTP Peer；不会运行数据库 migration
    Serve(ServeArgs),
    /// 只读检查 native 数据库协议准入
    DbCheck(DatabaseArgs),
    /// 签发共享 Peer JWT 并写入标准输出
    Token(TokenArgs),
}

#[derive(Args)]
pub struct DatabaseArgs {
    #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
    pub database_url: String,
    #[arg(long, env = "INKCRE_DATABASE_MAX_CONNECTIONS", default_value_t = 10)]
    pub max_connections: u32,
}

#[derive(Args)]
pub struct ServeArgs {
    #[command(flatten)]
    pub database: DatabaseArgs,
    #[arg(long, env = "JWT_SECRET", hide_env_values = true)]
    pub jwt_secret: String,
    #[arg(long, env = "INKCRE_BIND", default_value = "127.0.0.1:8000")]
    pub bind: SocketAddr,
    /// 不指定时，在 state-dir 中创建并复用 UUID
    #[arg(long, env = "INKCRE_PEER_ID")]
    pub peer_id: Option<Uuid>,
    /// 仅用于首次注册；不覆盖已有 Peer 显示名
    #[arg(long, env = "INKCRE_PEER_NAME", default_value = "Rust Peer")]
    pub peer_name: String,
    #[arg(long, env = "INKCRE_STATE_DIR", default_value = ".runtime")]
    pub state_dir: PathBuf,
    /// 可信本地扩展 manifest；HTTP 只能安装此目录清单中的名称
    #[arg(long, env = "INKCRE_EXTENSION_MANIFESTS", value_delimiter = ',')]
    pub extension_manifest: Vec<PathBuf>,
    #[arg(long, env = "INKCRE_PEER_LEASE_SECONDS", default_value_t = 60)]
    pub lease_seconds: u32,
    #[arg(long, env = "INKCRE_REQUEST_TIMEOUT_SECONDS", default_value_t = 30)]
    pub request_timeout_seconds: u64,
}

#[derive(Args)]
pub struct TokenArgs {
    #[arg(long, env = "JWT_SECRET", hide_env_values = true)]
    pub jwt_secret: String,
    #[arg(long, env = "INKCRE_TOKEN_LIFETIME_SECONDS", default_value_t = 3600)]
    pub lifetime_seconds: u64,
}

pub fn load_env() -> Result<(), String> {
    // 在 clap 读取 env 前处理全局参数；显式文件出错不得静默回退。
    let mut args = std::env::args_os().skip(1);
    let mut explicit = None;
    while let Some(arg) = args.next() {
        if arg == "--env-file" {
            explicit = args.next().map(PathBuf::from);
            break;
        }
        if let Some(value) = arg.to_str().and_then(|v| v.strip_prefix("--env-file=")) {
            explicit = Some(PathBuf::from(value));
            break;
        }
    }
    if let Some(path) = explicit {
        dotenvy::from_path(path).map_err(|_| "无法读取 dotenv 配置文件".to_owned())?;
    } else if let Err(error) = dotenvy::dotenv()
        && !error.not_found()
    {
        return Err("无法解析 .env 配置文件".to_owned());
    }
    Ok(())
}

pub fn validate_secret(secret: &str) -> Result<(), String> {
    if secret.len() < 32 {
        return Err("JWT_SECRET 至少需要 32 字节".to_owned());
    }
    Ok(())
}

pub fn stable_peer_id(args: &ServeArgs) -> Result<Uuid, String> {
    if let Some(id) = args.peer_id {
        return Ok(id);
    }
    std::fs::create_dir_all(&args.state_dir).map_err(|_| "无法创建运行状态目录")?;
    let path = args.state_dir.join("peer-id");
    match std::fs::read_to_string(&path) {
        Ok(text) => Uuid::parse_str(text.trim()).map_err(|_| "已保存的 Peer UUID 无效".to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            use std::io::Write;
            let id = Uuid::new_v4();
            let result = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path);
            match result {
                Ok(mut file) => {
                    file.write_all(id.to_string().as_bytes())
                        .map_err(|_| "无法保存 Peer UUID")?;
                    Ok(id)
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let text = std::fs::read_to_string(path).map_err(|_| "无法读取 Peer UUID")?;
                    Uuid::parse_str(text.trim()).map_err(|_| "已保存的 Peer UUID 无效".to_owned())
                }
                Err(_) => Err("无法保存 Peer UUID".to_owned()),
            }
        }
        Err(_) => Err("无法读取 Peer UUID".to_owned()),
    }
}
