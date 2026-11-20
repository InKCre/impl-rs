use crate::{Failure, Input};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    pub base_url: String,
    pub jwt_secret: String,
}
#[derive(Default, Deserialize, Serialize)]
pub struct Store {
    pub default: Option<String>,
    #[serde(default)]
    pub connections: BTreeMap<String, Connection>,
}
#[derive(Subcommand)]
pub enum Command {
    List,
    Get {
        name: String,
    },
    Set {
        name: String,
        #[command(flatten)]
        input: Input,
    },
    Use {
        name: String,
    },
    Delete {
        name: String,
    },
    Check,
}
pub fn path(override_path: Option<PathBuf>) -> Result<PathBuf, Failure> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let path = override_path
        .or_else(|| {
            home.as_ref()
                .map(|p| p.join(".inkcre/cli/connections.json"))
        })
        .ok_or_else(|| Failure::usage("需要 --connections-file"))?;
    if path == Path::new("~") {
        return home.ok_or_else(|| Failure::usage("无法展开主目录"));
    }
    if let Ok(relative) = path.strip_prefix("~/") {
        return home
            .map(|p| p.join(relative))
            .ok_or_else(|| Failure::usage("无法展开主目录"));
    }
    Ok(path)
}
pub fn load(path: &Path) -> Result<Store, Failure> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| Failure::usage("连接文件格式无效")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(_) => Err(Failure::usage("无法读取连接文件")),
    }
}
fn save(path: &Path, store: &Store) -> Result<(), Failure> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(|_| Failure::usage("无法创建连接目录"))?;
    let temporary = parent.join(format!(".connections-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&serde_json::to_vec(store)?)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok::<_, Box<dyn std::error::Error>>(())
    })();
    let _ = std::fs::remove_file(temporary);
    result.map_err(|_| Failure::usage("无法原子保存连接文件"))
}
pub async fn execute(
    path: PathBuf,
    command: Command,
    selected: Option<String>,
) -> Result<Value, Failure> {
    let mut store = load(&path)?;
    match command {
        Command::List => Ok(
            json!({"default":store.default,"connections":store.connections.iter().map(|(name,c)|(name,json!({"base_url":c.base_url}))).collect::<BTreeMap<_,_>>()}),
        ),
        Command::Get { name } => serde_json::to_value(
            store
                .connections
                .get(&name)
                .ok_or_else(|| Failure::usage("连接不存在"))?,
        )
        .map_err(|_| Failure::usage("无法表示连接")),
        Command::Set { name, input } => {
            if input.schema {
                return Ok(
                    json!({"type":"object","additionalProperties":false,"required":["base_url","jwt_secret"],"properties":{"base_url":{"type":"string","format":"uri"},"jwt_secret":{"type":"string","minLength":1}}}),
                );
            }
            if name.trim().is_empty() {
                return Err(Failure::usage("连接名不能为空"));
            }
            let connection: Connection = serde_json::from_value(input.read()?)
                .map_err(|_| Failure::usage("连接输入需要 base_url/jwt_secret"))?;
            crate::http::Client::new(&connection)?;
            let result = json!({"name":name,"base_url":connection.base_url});
            store.connections.insert(name.clone(), connection);
            if store.default.is_none() {
                store.default = Some(name);
            }
            save(&path, &store)?;
            Ok(result)
        }
        Command::Use { name } => {
            if !store.connections.contains_key(&name) {
                return Err(Failure::usage("连接不存在"));
            }
            store.default = Some(name);
            save(&path, &store)?;
            Ok(json!({"default":store.default}))
        }
        Command::Delete { name } => {
            if store.connections.remove(&name).is_none() {
                return Err(Failure::usage("连接不存在"));
            }
            if store.default.as_ref() == Some(&name) {
                store.default = None;
            }
            save(&path, &store)?;
            Ok(Value::Null)
        }
        Command::Check => {
            let name = selected
                .or(store.default)
                .ok_or_else(|| Failure::usage("请选择命名连接"))?;
            let client = crate::http::Client::new(
                store
                    .connections
                    .get(&name)
                    .ok_or_else(|| Failure::usage("连接不存在"))?,
            )?;
            let ready = match client.ready().await {
                Ok(value) => value,
                Err(error) => json!({"error":error.as_value()}),
            };
            let authenticated = match client.send("GET", "/peers/self", None, &[]).await {
                Ok(value) => value,
                Err(error) => json!({"error":error.as_value()}),
            };
            Ok(json!({"readyz":ready,"authenticated_read":authenticated}))
        }
    }
}
