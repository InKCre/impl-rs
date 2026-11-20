# AI 模型发现与执行

嵌入模型调用或排查模型不可用时阅读本文。共享目录由 PostgreSQL 的 `ai_dialects`、`ai_providers` 和 `ai_models` 承载。读取模型目录只查询模型记录，无需本机注册相应 dialect。HTTP 模型接口返回模型能力与启用状态，provider 的配置和凭据始终留在执行侧。

`inkcre_core::ai::Ai` 允许调用者通过共享模型 ID 执行 embedding 与 chat。一次查询装载启用的 model/provider 配对，随后释放数据库连接。模型声明与本机 adapter 都支持所需的模态和特性时才发出请求。`can_execute` 只检查这些静态条件，远端连通性以实际调用结果为准。独立嵌入者可以注册共享目录中已有确切 ID 的自定义 adapter。

内建 adapter 保持 `core.openai-compatible.v1` 和 `core.alibaba-model-studio.v1` 的共享身份。Embedding 按 provider 返回的 index 还原数组顺序，验证维度及有限非零向量。Chat 保存 canonical message 和 tool call 结构，tool result 必须与相邻请求的 ID 集合精确一致。Alibaba adapter 聚合 SSE 文本及按 index 分片的 tool calls。Chat 返回模型发出的请求，历史、工具执行及图资料由调用者管理。

Provider 请求使用 10 秒连接预算、120 秒请求预算和 32 MiB 响应上限。媒体内联上限为每项 7 MiB。超过上限时，所选 dialect 和该模态允许 URL 才可使用调用者提供的 transfer URL。OpenAI 兼容的原生音频字段需要内联 MP3/WAV。SDK 关闭 provider 请求的重定向和隐式重试，错误不包含远端响应、配置值或消息。
