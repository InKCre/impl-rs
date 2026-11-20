# MCP Sink

当客户端使用已启用的 `core.mcp.v1` 实例时，连接 `/sinks/{id}/mcp`，携带该实例配置的 PAT Bearer。鉴权每次从共享 Sink 行检查当前 Peer 的 enable intent。普通 Peer JWT 只有与实例 PAT 相同时才可通过该入口。

传输提供 2025-11-25 的无状态 JSON 响应形式。POST 承载 JSON-RPC 请求，通知返回202，GET不建立 SSE 会话。工具发现提供 metadata、query、open/read、graph expansion/path、Resolver method discovery/invocation 八项能力。`resources/read` 使用稳定 Block 或 Resolver URI 重新读取当前资料，资源选择器失效会返回错误。

metadata 与条件查询使用 Core 的列白名单、ID 升序和一项前瞻。有界 cursor 将工具、集合或过滤器与最后 ID 一起编码，跨范围复用被拒绝。查询工具保持 lexical 和 semantic 各自的证据。大文字结果通过 live URI 读取。原始 Storage bytes 和 Resolver 结果中的嵌套 bytes 都生成 blob URI，读取时以 base64 返回。

`skills/list` 和 `skills/get` 提供静态 `use-inkcre`，`resources/read` 返回 Markdown 及其稳定 URI。逐项失败字段和工具结果中的 ResourceLink 格式仍需继续对齐。SQL metadata 操作位于 Core discovery，HTTP transport 通过该接口读取。

协议依据为 MCP 2025-11-25 Streamable HTTP 规范。后续协议升级需要与 Python当前SDK所支持版本协商共同推进。

[传输规范](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports) 允许 POST 返回单个 JSON 响应，服务端可选择省略协议会话。该实现采用这一形式，依赖 Axum 处理HTTP生命周期。

本轮已用真实 native PostgreSQL、随机 Peer 和独有 Sink 验证 PAT 与 Peer JWT 隔离、初始化、八工具列表、cursor翻页及跨集合拒绝、混合成功/失败的 Block 读取、资源更新后重新读取。临时 Sink、Block 和 Peer 在 finally 精确删除。显式验收日志保存在本机 `.runtime/mcp-result.log`，不由CI自动创建数据库。
