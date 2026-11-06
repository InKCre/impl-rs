use async_trait::async_trait;
use inkcre_core::{BlockInput, Core};
use inkcre_extension_sdk::{
    Descriptor, Extension, PROTOCOL_VERSION, RpcError, SDK_VERSION, StartContext, serve,
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Default)]
struct Notes {
    core: Option<Core>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureText {
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BlockIdentity {
    id: i32,
}

fn invalid_parameters() -> RpcError {
    RpcError::new("invalid_parameters", "参数不符合方法要求")
}

fn database_error(_: inkcre_core::Error) -> RpcError {
    RpcError::new("database_error", "数据库操作失败")
}

#[async_trait]
impl Extension for Notes {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            name: "example/notes".into(),
            version: "0.1.0".parse().expect("static extension version"),
            sdk_version: SDK_VERSION.parse().expect("static SDK version"),
            protocol_version: PROTOCOL_VERSION,
            config_schema: json!({"type":"object", "additionalProperties":false}),
            methods: vec!["capture_text".into(), "get_block".into()],
        }
    }

    async fn start(&mut self, _context: StartContext) -> Result<Value, RpcError> {
        let url = std::env::var("INKCRE_DATABASE_URL")
            .map_err(|_| RpcError::new("missing_database", "宿主未提供 native 数据库连接"))?;
        self.core = Some(Core::connect(&url, 2).await.map_err(database_error)?);
        Ok(json!({"started":true}))
    }

    async fn invoke(&mut self, method: &str, parameters: Value) -> Result<Value, RpcError> {
        let core = self
            .core
            .as_ref()
            .ok_or_else(|| RpcError::new("not_started", "扩展尚未启动"))?;
        let block = match method {
            "capture_text" => {
                let input: CaptureText =
                    serde_json::from_value(parameters).map_err(|_| invalid_parameters())?;
                core.block_create(BlockInput {
                    resolver: "core.text.v1".into(),
                    content: input.text,
                    storage: None,
                })
                .await
                .map_err(database_error)?
            }
            "get_block" => {
                let input: BlockIdentity =
                    serde_json::from_value(parameters).map_err(|_| invalid_parameters())?;
                core.block_get(input.id)
                    .await
                    .map_err(database_error)?
                    .ok_or_else(|| RpcError::new("not_found", "Block 不存在"))?
            }
            _ => return Err(RpcError::new("unknown_method", "方法不存在")),
        };
        serde_json::to_value(block).map_err(|_| RpcError::new("invalid_result", "无法序列化 Block"))
    }

    async fn stop(&mut self) -> Result<(), RpcError> {
        if let Some(core) = self.core.take() {
            core.close().await;
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), inkcre_extension_sdk::Error> {
    serve(Notes::default()).await
}
