mod connection;
mod http;
mod output;
mod resource;
use resource::resource;

use clap::{Args, Parser, Subcommand};
use http::{Client, Failure};
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "InKCre REST 客户端")]
struct Cli {
    #[arg(long, global = true, env = "INKCRE_CLI_CONNECTION")]
    connection: Option<String>,
    #[arg(long, global = true, env = "INKCRE_CLI_CONNECTIONS_FILE")]
    connections_file: Option<PathBuf>,
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, global = true)]
    output_dir: Option<PathBuf>,
    #[arg(long, global = true, conflicts_with = "json")]
    table: bool,
    #[arg(long, global = true, value_delimiter = ',')]
    fields: Vec<String>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Connection {
        #[command(subcommand)]
        command: connection::Command,
    },
    Get {
        references: Vec<String>,
        #[arg(long, default_value="raw", value_parser=["none","raw","hydrated"])]
        content: String,
    },
    Update {
        reference: String,
        #[command(flatten)]
        input: Input,
    },
    Delete {
        reference: String,
    },
    Query {
        question: Option<String>,
        #[arg(long = "sink", required = true)]
        sink_id: i64,
        #[arg(long)]
        timeout_seconds: Option<u32>,
        #[arg(long)]
        schema: bool,
    },
    Recall {
        query: String,
        #[arg(long, required=true, value_parser=["lexical","semantic"])]
        mode: Vec<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        profile: Option<i64>,
        #[arg(long)]
        peer: Option<String>,
    },
    Graph {
        #[command(subcommand)]
        command: Graph,
    },
    Job(Resource),
    Cron(Resource),
    Source(Resource),
    Peer(Resource),
    Config(Resource),
    Extension(Resource),
    Agent(Resource),
    Sink(Resource),
    Ai(Resource),
    EmbeddingProfile(Resource),
    Resolver(Resource),
}
#[derive(Args, Default)]
struct Input {
    #[arg(long)]
    schema: bool,
    #[arg(long, conflicts_with = "input_file")]
    input_json: Option<String>,
    #[arg(long = "input", alias = "input-file")]
    input_file: Option<PathBuf>,
}
impl Input {
    fn read(&self) -> Result<Value, Failure> {
        let text = match (&self.input_json, &self.input_file) {
            (Some(text), _) => text.clone(),
            (_, Some(path)) if path.as_os_str() == "-" => std::io::read_to_string(std::io::stdin())
                .map_err(|_| Failure::usage("无法读取标准输入"))?,
            (_, Some(path)) => {
                std::fs::read_to_string(path).map_err(|_| Failure::usage("无法读取输入文件"))?
            }
            _ => "{}".into(),
        };
        serde_json::from_str(&text).map_err(|_| Failure::usage("输入必须为 JSON"))
    }
}
#[derive(Args)]
struct Resource {
    #[command(subcommand)]
    action: Action,
}
#[derive(Subcommand)]
enum Action {
    List {
        #[arg(long)]
        limit: Option<u32>,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, name = "type")]
        type_id: Option<String>,
        #[arg(long)]
        status: Option<String>,
    },
    Get {
        id: String,
    },
    Create {
        #[arg(long, name = "type")]
        type_id: Option<String>,
        #[arg(long)]
        job_type: Option<String>,
        #[command(flatten)]
        input: Input,
    },
    Update {
        id: String,
        #[arg(long)]
        job_type: Option<String>,
        #[command(flatten)]
        input: Input,
    },
    Set {
        id: String,
        #[command(flatten)]
        input: Input,
    },
    Delete {
        id: String,
    },
    Types {
        id: Option<String>,
    },
    Schemas {
        id: Option<String>,
    },
    Tools {
        id: Option<String>,
    },
    Models {
        id: Option<String>,
    },
    Replace {
        id: String,
        #[arg(long)]
        schema_id: Option<String>,
        #[command(flatten)]
        input: Input,
    },
    Config {
        #[command(subcommand)]
        command: ConfigAction,
    },
    Abort {
        id: i64,
    },
    Wait {
        id: i64,
        #[arg(long = "for", default_value = "30s", value_parser=parse_duration)]
        seconds: f64,
    },
    Collect {
        id: i64,
        #[arg(long)]
        timeout_seconds: Option<u32>,
        #[command(flatten)]
        input: Input,
    },
    Backfill {
        id: i64,
        #[arg(long)]
        timeout_seconds: Option<u32>,
        #[command(flatten)]
        input: Input,
    },
    Run {
        id: i64,
    },
    Enable {
        id: String,
        #[arg(long)]
        peer: Option<String>,
    },
    Disable {
        id: String,
        #[arg(long)]
        peer: Option<String>,
    },
    Status {
        id: Option<String>,
    },
    Select {
        id: String,
        #[arg(long, required = true)]
        distribution: String,
        #[arg(long, required = true)]
        version: String,
        #[arg(long)]
        peer: Option<String>,
    },
    Unselect {
        id: String,
        #[arg(long)]
        peer: Option<String>,
    },
    Install {
        name: String,
        version: String,
    },
}
#[derive(Subcommand)]
enum ConfigAction {
    Get {
        id: String,
    },
    Replace {
        id: String,
        #[command(flatten)]
        input: Input,
    },
    Update {
        id: String,
        #[command(flatten)]
        input: Input,
    },
}
#[derive(Subcommand)]
enum Graph {
    Submit {
        #[command(flatten)]
        input: Input,
    },
    Path {
        #[arg(long = "from")]
        from: i32,
        #[arg(long = "to")]
        to: i32,
        #[command(flatten)]
        input: Input,
    },
    Neighborhood {
        reference: String,
    },
}
fn entity(reference: &str) -> Result<(&str, i64), Failure> {
    let (kind, id) = reference
        .split_once(':')
        .ok_or_else(|| Failure::usage("引用为 block:42 或 relation:8"))?;
    if !matches!(kind, "block" | "relation") {
        return Err(Failure::usage("未知实体类型"));
    }
    Ok((
        kind,
        id.parse()
            .map_err(|_| Failure::usage("实体 ID 必须为整数"))?,
    ))
}
fn segment(value: &str) -> Result<String, Failure> {
    if value.is_empty() || matches!(value, "." | "..") {
        return Err(Failure::usage("无效资源标识"));
    }
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    Ok(encoded)
}
async fn run(cli: Cli) -> Result<Value, Failure> {
    let path = connection::path(cli.connections_file)?;
    if let Command::Connection { command } = cli.command {
        return connection::execute(path, command, cli.connection).await;
    }
    let config = connection::load(&path)?;
    let name = cli
        .connection
        .or(config.default)
        .ok_or_else(|| Failure::usage("请选择命名连接"))?;
    let selected = config
        .connections
        .get(&name)
        .ok_or_else(|| Failure::usage("连接不存在"))?;
    let client = Client::new(selected)?;
    match cli.command {
        Command::Query {
            question,
            sink_id,
            timeout_seconds,
            schema,
        } => {
            let path = format!("/sinks/{sink_id}/query");
            if schema {
                return client.schema("POST", &path).await;
            }
            let question = question.ok_or_else(|| Failure::usage("需要 QUESTION 或 --schema"))?;
            client
                .send(
                    "POST",
                    &path,
                    Some(json!({"query":question,"timeout_seconds":timeout_seconds})),
                    &[],
                )
                .await
        }
        Command::Get {
            references,
            content,
        } => {
            let entities = references
                .iter()
                .map(|r| entity(r).map(|(kind, id)| json!({"type":kind,"id":id})))
                .collect::<Result<Vec<_>, _>>()?;
            client
                .send(
                    "POST",
                    "/entities/get",
                    Some(json!({"entities":entities,"content":content})),
                    &[],
                )
                .await
        }
        Command::Update { reference, input } => {
            let (kind, id) = entity(&reference)?;
            client
                .send("PATCH", &format!("/{kind}s/{id}"), Some(input.read()?), &[])
                .await
        }
        Command::Delete { reference } => {
            let (kind, id) = entity(&reference)?;
            client
                .send("DELETE", &format!("/{kind}s/{id}"), None, &[])
                .await
        }
        Command::Recall {
            query,
            mode,
            limit,
            profile,
            peer,
        } => {
            let mut results = Vec::new();
            for mode in mode {
                let body = if mode == "semantic" {
                    json!({"query":query,"profile":profile,"options":{"limit":limit}})
                } else {
                    json!({"query":query,"limit":limit})
                };
                let result = client
                    .send(
                        "POST",
                        &format!("/retrieval/{mode}"),
                        Some(body),
                        &[("route_to_peer", peer.clone())],
                    )
                    .await;
                results.push(match result {
                    Ok(result) => json!({"mode":mode,"result":result}),
                    Err(error) => json!({"mode":mode,"error":error.detail}),
                });
            }
            Ok(json!(results))
        }
        Command::Graph { command } => match command {
            Graph::Submit { input } => {
                if input.schema {
                    return client.schema("POST", "/graph").await;
                }
                client
                    .send("POST", "/graph", Some(input.read()?), &[])
                    .await
            }
            Graph::Path { from, to, input } => {
                if input.schema {
                    return client.schema("POST", "/graph/path").await;
                }
                let mut body = input.read()?;
                let object = body
                    .as_object_mut()
                    .ok_or_else(|| Failure::usage("路径输入必须为对象"))?;
                object.insert("from_block_id".into(), json!(from));
                object.insert("to_block_id".into(), json!(to));
                client.send("POST", "/graph/path", Some(body), &[]).await
            }
            Graph::Neighborhood { reference } => {
                let (kind, id) = entity(&reference)?;
                client
                    .send("GET", &format!("/{kind}s/{id}/neighborhood"), None, &[])
                    .await
            }
        },
        Command::Job(r) => resource(&client, "job", r).await,
        Command::Cron(r) => resource(&client, "cron", r).await,
        Command::Source(r) => resource(&client, "source", r).await,
        Command::Peer(r) => resource(&client, "peer", r).await,
        Command::Config(r) => resource(&client, "config", r).await,
        Command::Extension(r) => resource(&client, "extension", r).await,
        Command::Agent(r) => resource(&client, "agent", r).await,
        Command::Sink(r) => resource(&client, "sink", r).await,
        Command::Ai(r) => resource(&client, "ai", r).await,
        Command::EmbeddingProfile(r) => resource(&client, "embedding-profile", r).await,
        Command::Resolver(r) => resource(&client, "resolver", r).await,
        Command::Connection { .. } => unreachable!(),
    }
}
#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let checking = matches!(
        &cli.command,
        Command::Connection {
            command: connection::Command::Check
        }
    );
    let compact = cli.json;
    let directory = cli.output_dir.clone();
    let table = cli.table;
    let fields = cli.fields.clone();
    let result = match run(cli).await {
        Ok(value) => {
            let connection_failed = checking
                && (value["readyz"].get("error").is_some()
                    || value["readyz"]["ready"] != true
                    || value["authenticated_read"].get("error").is_some());
            let failed = connection_failed
                || value
                    .as_array()
                    .is_some_and(|rows| rows.iter().any(|row| row.get("error").is_some()));
            output::emit(value, compact, directory, table, fields).and_then(|_| {
                if failed {
                    Err(Failure {
                        code: 1,
                        detail: json!("结果中存在逐项错误"),
                    })
                } else {
                    Ok(())
                }
            })
        }
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        eprintln!("{}", json!({"detail":error.detail}));
        std::process::exit(error.code);
    }
}
fn parse_duration(value: &str) -> Result<f64, String> {
    let (number, scale) = match value.as_bytes().last() {
        Some(b's') => (&value[..value.len() - 1], 1.0),
        Some(b'm') => (&value[..value.len() - 1], 60.0),
        Some(b'h') => (&value[..value.len() - 1], 3600.0),
        _ => (value, 1.0),
    };
    let seconds = number
        .parse::<f64>()
        .map_err(|_| "使用正数秒或 30s / 2m / 1h".to_owned())?
        * scale;
    if !seconds.is_finite() || seconds <= 0.0 || seconds > u32::MAX as f64 {
        return Err("观察时间超出范围".into());
    }
    Ok(seconds)
}
