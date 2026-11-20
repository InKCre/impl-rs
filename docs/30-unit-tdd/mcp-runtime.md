# MCP Sink

当客户端使用已启用的 `core.mcp.v1` 实例时，连接 `/sinks/{id}/mcp`，携带该实例配置的 PAT Bearer。鉴权每次从共享 Sink 行检查当前 Peer 的 enable intent。普通 Peer JWT 只有与实例 PAT 相同时才可通过该入口。

传输提供 2025-11-25 的无状态 JSON 响应形式。POST 承载 JSON-RPC 请求，通知返回202，GET不建立 SSE 会话。工具发现提供 metadata、query、open/read、graph expansion/path、Resolver method discovery/invocation 八项能力。`resources/read` 使用稳定 Block 或 Resolver URI 重新读取当前资料，资源选择器失效会返回错误。

metadata 与条件查询使用 Core 的列白名单、ID 升序和一项前瞻。有界 cursor 将工具、集合或过滤器与最后 ID 一起编码，跨范围复用被拒绝。查询工具保持 lexical 和 semantic 各自的证据。结果按请求顺序保留逐项关联：open/expand返回entity，read返回block，Resolver invocation返回index/block/method。部分失败保留同批成功项与各自not_found、invalid_arguments、unavailable或failed；只有整次工具参数无效时将isError置为true。resolver_methods按Resolver聚合Block，单列missing Block或未注册Resolver的errors；同时提供blocks时不另查resolvers参数。

read_blocks与invoke_resolver_methods同时返回structuredContent和MCP原生content。最多64 KiB的非二进制值使用EmbeddedResource，字符串是text/plain、对象是application/json；文字阈值按实际UTF-8 bytes计算，JSON按Python的空格分隔格式计算。大文字与顶层bytes使用ResourceLink，size为原始交付bytes；嵌套bytes在结构化值中留下resource/size及已知mime_type，并另附对应ResourceLink，不将二进制塞进TextContent的JSON字符串。只要存在成功内容块，失败信息留在structuredContent；没有内容块时用TextContent承载结构化结果。

稳定URI分为Block content和Resolver method两组，各自提供text/blob模板。selector保留对象key与数组index的类型，使用URL-safe Base64 JSON，不把嵌套bytes的JSON Pointer误当URI路径。resources/read重新执行当前资料读取和精确选择，text模板返回text/plain，blob模板返回application/octet-stream与标准Base64；不存在的选择器明确失败。它不是运行内缓存，也不由URI授予PAT之外的权限。

`skills/list` 和 `skills/get` 提供静态 `use-inkcre`，`resources/read` 返回 Markdown 及其稳定 URI。open/expand与found path只提供Block/Relation预览，不混入原始Block.content；路径状态和关联字段使用status、block_path、relation_path。SQL metadata 操作位于 Core discovery，HTTP transport 通过该接口读取。

协议依据为 MCP 2025-11-25 Streamable HTTP 规范。后续协议升级需要与 Python当前SDK所支持版本协商共同推进。

[传输规范](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports) 允许 POST 返回单个 JSON 响应，服务端可选择省略协议会话。该实现采用这一形式，依赖 Axum 处理HTTP生命周期。

已用真实app、native PostgreSQL、随机Peer与独有Sink验证PAT/JWT隔离、初始化、八工具列表、cursor翻页及跨范围拒绝、64 KiB Unicode边界、原生MCP内容块、顶层/嵌套bytes URI往返、逐项失败关联、Resolver方法分组与调用分类、图预览及资源更新后的live重读。临时Sink、Block、Relation、blob与Peer均在finally按本次ID精确清理。显式验收结果保存在本机 `.runtime/mcp-output-result.log`，不新增自动化测试，也不由CI创建数据库。
