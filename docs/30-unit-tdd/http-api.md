# HTTP API

接入 Rust app 时阅读本文。一般业务路径同时支持根路径和 `/api/v1` 前缀，均要求 `Authorization: Bearer <JWT>`。扩展管理的两套路径及 MCP 的凭据单独说明。健康检查 `/health/live` 和 `/health/ready` 无需认证。JSON 错误形如 `{"error":{"code":"…","message":"…"}}`，数据库和插件原始错误不会直接返回。

## 图数据

| 方法与路径 | 输入与结果 |
| --- | --- |
| `GET /blocks?limit=50&cursor=123` | `items/next_cursor`；ID 降序，cursor exclusive |
| `GET /blocks/recent?num=10&resolver=…` | 按创建时间倒序返回 Block 数组，可按 exact Resolver 过滤 |
| `POST /blocks` | `{resolver,content,storage?}`，201 返回 Block |
| `GET/PATCH/DELETE /blocks/{id}` | 读取、部分更新或删除；删除返回 204 |
| `GET /blocks/{id}/content` | inline 文本或已注册 Storage handler 读取的实际 bytes |
| `GET /blocks/{id}/relations` | 关联 Relation 分页 |
| `GET /relations/by_block/{id}` | 关联 Relation 数组，端点为 `from_block_id/to_block_id` |
| `POST /relations` | `{from_block_id,to_block_id,content}`，201 返回 Relation。也接受原字段 `from_/to_` |
| `GET/PATCH/DELETE /relations/{id}` | 读取、部分更新或删除 |
| `GET /relations/{id}/neighborhood` | `{focal_relation,graph}`，含 Relation 与存在的端点 |
| `POST /graph/neighborhood` | `{block_id,direction?,contents?,limit?,cursor?}` |
| `GET /blocks/{id}/neighborhood` | `direction`、可重复的 `contents`、`limit`、`cursor`；返回 `focal_block/graph/next_cursor` |
| `POST /graph/path` | `{from_block_id,to_block_id,direction?,contents?,max_hops?,max_explored_blocks?}` |
| `POST /graph/components` | `{seed_block_ids,contents,max_explored_blocks?,max_explored_relations?}`，返回种子分组、缺失 ID 和闭合证明图 |
| `POST /graph` | 扁平生产者命令 `{blocks,relations}`，原子保存并返回 `{blocks:[{local_id,id}]}` |
| `POST /entities/get` | `{entities:[{type,id}],content:"none/raw/hydrated"}`，按输入顺序返回记录或逐项错误 |

`POST /graph` 的新 Block 使用命令局部负 ID，Relation 可引用这些 ID 或既有正 ID。生产者可保存本机尚未加载的 exact Resolver。需要本机校验表示时使用 `/graph/submit`。实体批量响应在存在二进制内容时采用 `multipart/related`，首段包含 `{value,parts}`，其余各段用 Content-ID 与 JSON Pointer 对应。

`direction` 为 `in/out/both`。`contents` 按 Relation content 精确过滤，空数组代表不过滤。根路径 POST 以 `status=found/not_found/limit_reached` 表示结果，找到路径时包含闭合 `graph` 和有序 `block_path/relation_path`。旧 GET 查询与 `/api/v1` 下的 POST 保留 `outcome`、`block_ids/relation_ids`。旧 GET 的 `contents` 使用 URL 编码的 JSON 字符串数组。PATCH 省略字段保持原值，Block storage 显式 null 可清空。

普通 Block/Relation 读取不要求相应 Resolver 已加载。HTTP 创建或修改 Block 则会校验完整的新表示，要求本机 exact Resolver 和相应 Storage handler；未知 Resolver 不会退化为纯文本。`/content` 返回 `text/plain` 或 `application/octet-stream`，不返回 solved JSON。

例如创建普通文本：

```json
{"resolver":"core.text.v1","content":"一条资料"}
```

## Resolver 与 graph draft

| 方法与路径 | 输入与结果 |
| --- | --- |
| `GET /resolvers?limit=…&cursor=…` | 本机已加载的 Resolver，返回 `{resolvers,next_cursor}` |
| `GET /resolvers/{resolver}/methods` | 该 exact Resolver 的公共及自定义方法、参数 schema 和分页游标 |
| `GET /blocks/{id}/resolver/methods` | 按 Block 的 exact Resolver 发现方法，支持相同分页 |
| `POST /blocks/{id}/resolver/methods/{method}` | 参数对象缺省 `{}`。按结果类型返回 JSON、原始 bytes 或 `multipart/related` |
| `GET /blocks/{id}/read-methods` | 根据 Block 当前 Resolver 查询方法，不解算内容 |
| `GET /blocks/{id}/solved` | Resolver-owned JSON；支持 `refresh`、`materialize_missing` query |
| `GET /blocks/{id}/text` | JSON string 或 null；另支持 `context=default/lexical` |
| `GET /blocks/{id}/label` | JSON string；支持 `refresh` |
| `POST /blocks/{id}/invoke/{method}` | body 为方法参数对象，按声明的 input schema 校验 |
| `POST /graph/draft` | `{resolver,input}`，生成未持久化的 GraphDraft |
| `POST /graph/submit` | GraphDraft，201 返回 `{root,blocks,relations}` 的持久 ID 映射 |

读取参数缺省为 `refresh=false`、`materialize_missing=true`、`context=default`。`refresh` 刷新本次 reader 的内容或关系缓存，不要求重新采集或 AI 处理；`materialize_missing` 允许具体 Resolver 填补缺失派生信息，因此读取可能产生该能力声明的副作用。只查看已保存投影时使用 `?materialize_missing=false`。支持但没有文字结果返回 null；作者写入的空字符串返回 `""`；未知或不支持的能力明确报错。

公共方法为 `get_raw_content`、`get_solved_content`、`get_text`、`get_label`、`get_relations` 和 `get_transfer_url`。通过 invoke 调用时 body 仍直接使用该方法的参数对象，例如 `{"materialize_missing":false}`；`get_raw_content` 返回带 `kind/content` 的 JSON 表示，而 `/content` 返回实际响应体。额外只读方法使用 `get_` 或 `read_` 前缀。

`/api/v1` 的 Resolver 发现接口保留旧数组格式。旧 `/invoke/{method}` 和 `/solved` 保留 JSON 投影。新的 `/resolver/methods/{method}` 保留顶层或嵌套的二进制类型，multipart 采用与 `/entities/get` 相同的 JSON Pointer 约定。

GraphDraft 形如 `{"root":-1,"blocks":[{"id":-1,"resolver":"core.text.v1","content":"资料"}],"relations":[]}`。新 Block ID 为命令局部负整数，Relation 可引用这些 ID 或既有正 ID；提交会校验全部新表示，在同一事务内映射 ID 并写图。每次最多 1000 个新 Block、5000 条 Relation。draft 本身不保存任何图数据，具体 Resolver 必须声明 drafting 能力。读取与事务细节见 [知识运行时](knowledge-runtime.md)。

## Storage

| 方法与路径 | 输入与结果 |
| --- | --- |
| `GET /storages` | 已有 storage 配置；不创建私有存储表 |
| `GET /storage-types` | 本机注册的 Storage descriptor，包含 config schema 和 writable 标志 |
| `POST /storages` | `{type,nickname?,config?}`，201 返回新 Storage；需本机对应 handler |
| `POST /storages/{id}/objects` | 原始 bytes，201 返回 `{pointer}` |
| `GET /storages/{id}/objects?pointer=…` | 读取 opaque pointer 对应的原始 bytes |
| `PUT /storages/{id}/objects?pointer=…` | body 为原始 bytes，更新返回 204 |
| `DELETE /storages/{id}/objects?pointer=…` | 删除对象，返回 204 |
| `POST /storage-blobs` | 原始 bytes，201 返回 `{id}` |
| `GET/PUT/DELETE /storage-blobs/{uuid}` | 原始 bytes 读写，更新/删除返回 204 |

`pointer` 由 handler 拥有，query 中须完整 URL 编码。内建 `postgresql_binary` 可读写，`http` 只读 HTTP(S)；对只读 Storage 写入会明确报错。`/storage-blobs` 是 PostgreSQL blob 的低层入口；Storage object 接口返回的 pointer 可以作为 Block 的 `content`，并在 `storage` 中记录其 Storage ID。内建负 Storage ID 合法，不能当作 graph draft 的局部 ID 解释。

## Peer 与 Job

| 方法与路径 | 输入与结果 |
| --- | --- |
| `GET /self` | 当前 Peer、应用版本与支持的分发格式 |
| `GET /peers`、`GET/DELETE /peers/{uuid}` | Peer 分页、读取、移除 |
| `GET/POST /job-types` | catalog 分页或发布精确 JobType schema |
| `GET/DELETE /job-types/{id}` | 读取或移除 catalog 项 |
| `GET/POST /jobs` | 分页或提交 `{type,parameters,timeout_seconds?}`，提交返回 201 |
| `GET/DELETE /jobs/{id}` | 读取或删除未运行的记录 |
| `POST /jobs/{id}/claim` | 原子领取 pending Job，调用者需具备执行能力 |
| `POST /jobs/{id}/finish` | `{status,state}`；只关闭 running Job |
| `POST /jobs/{id}/abort` | pending 立即中止，running 保存 abort_requested |

JobType 输入为 `{id,description,parameters_schema,default_timeout_seconds}`。Job 提交还可携带 `submission_traceparent/submission_tracestate`；分页支持 `type/status` 过滤。提交依据共享 catalog 验证参数，不因 Rust 本机缺少 handler 而拒绝；Source exact Job 另验证该 Source 的命令 config schema。

worker 默认发现并执行本机有资格领取的 pending 工作；未知或暂不可执行的命令继续 pending。HTTP 提交成功只确认持久化，不承诺当前 Peer 执行或最终成功。claim/finish 是低层执行协议操作，调用者承担实际执行和收尾责任。abort 是尽力取消，running Job 要等执行器实际退出后才能关闭；进程崩溃的 running 行须在确认执行器停止后显式维护。运行开关及混合 Python scanner 的边界见 [运行说明](../40-deployment/runtime.md)。

## Source 与 Cron

| 方法与路径 | 输入与结果 |
| --- | --- |
| `GET /source-types`、`GET /source-types/{id}` | 共享 Source catalog，包含 config、collect、可选 backfill schema |
| `GET/POST /sources` | 实例分页，或提交 `{type,nickname?,config?,storage?}`；创建返回 201 |
| `GET/PATCH/DELETE /sources/{id}` | 读取、更新配置或删除实例；删除返回 204 |
| `POST /sources/{id}/anchor` | 延迟创建或同步来源锚点，返回 `core.source.v1` Block |
| `POST /sources/{id}/collect` | body 为采集配置对象，缺省 `{}`。可加 `?timeout_seconds=…`，201 返回 `core.source.collect.v1` Job |
| `POST /sources/{id}/backfill` | body 为回填配置对象，同样支持超时参数。该类型无 backfill schema 时返回 422 |
| `GET/POST /crons` | 模板分页，或创建 `{schedule,enabled?,job_type,job_parameters?,job_timeout_seconds?}`；创建返回 201 |
| `GET/PATCH/DELETE /crons/{id}` | 读取、修改或删除模板；删除返回 204 |
| `POST /crons/{id}/run` | 201 返回独立普通 Job，忽略 enabled，保留发生进度。`/api/v1` 返回 202 |
| `POST /crons/{id}/materialize` | 按当前到期分钟物化一次，返回 Job 或 null |
| `GET/PUT /cron/config` | 部署级 `{timezone}`，默认 UTC；写入影响共享 Cron 的时区解释 |

Source PATCH 支持 `nickname/config/storage`，省略保持原值，nickname/storage 显式 null 清空；config 整对象替换且不接受 null。`storage` 必须选择可写类型；state 与 cursor 由实际 Source handler 拥有。删除 Source 配置保留来源锚点与已采集资料。catalog 存在不表示本机具备 Source handler，插件停用也不会删除 catalog。

`/api/v1` 下的 collect 与 backfill 接收 `{config?,timeout_seconds?}`，成功返回 202。该版本的 Source 缺少 backfill 能力时返回 501。

RSS Source ID 为 `extensions.rss.rss.Source`，Atom 为 `extensions.rss.atom.Source`，最小 config 为 `{"feed_url":"https://example.org/feed.xml"}`。需先安装并启用可信的 `inkcre/rss` Rust 分发，本机 worker 才具有对应能力。collect 的 config 可以为空对象；这两个 Source 不支持 backfill。具体业务语义见 [RSS 分发](rss-extension.md)。

Cron 默认 enabled，schedule 使用五段 UNIX 格式、分钟粒度，时区接受 IANA 名称。PATCH 支持创建字段，`job_timeout_seconds=null` 恢复 JobType 默认值。materialize 遵守 enabled、当前发生去重及 `last_job` pending/running 的叠加限制；删除模板不删除已生成的 Job。自动 loop 与显式入口都只生产普通 Job，执行统一交给 worker；详细语义见 [Job 与 Cron](job-runtime.md)。

根路径的 Source、SourceType 和 Cron 集合分别返回 `sources`、`source_types` 和 `crons`，并提供 `next_cursor`。`/api/v1` 保留 `items/next_cursor`。Source 按 ID 降序，Cron 按 ID 升序，SourceType 按 ID 字典序升序。

## 检索与配置

| 方法与路径 | 输入与结果 |
| --- | --- |
| `POST /lexical-retrieval`、`POST /semantic-retrieval` | 本机执行各自查询与排名 |
| `POST /retrieval/lexical`、`POST /retrieval/semantic` | 同一查询体。可通过 `route_to_peer` 指定另一个具备相应能力的 Peer |
| `GET /embedding-profiles`、`GET /embedding-profiles/{id}` | 共享 embedding profile 的分页和单项读取 |
| `GET /configs`、`GET/PUT/PATCH/DELETE /configs/{key}` | 部署级配置。PUT 使用 `{schema,value}`，PATCH 合并顶层字段并验证完整状态 |
| `GET /config-schemas`、`GET /config-schemas/{id}` | 本机注册的配置 schema 和对应输入结构 |

查询与维护参数分别见 [词法检索](lexical-retrieval.md)、[语义检索](semantic-retrieval.md) 和 [部署配置](deployment-config.md)。

## AI 模型

| 方法与路径 | 输入与结果 |
| --- | --- |
| `GET /ai/models` | 按 ID 升序，`limit` 可选，`cursor` exclusive，返回 `{models,next_cursor}` |
| `GET /ai/models/{id}` | 共享模型记录，包括声明的能力和启用状态 |

模型目录只返回模型元数据。执行使用 [AI SDK](ai-runtime.md)，调用方通过已配置模型选择相应 provider 与 dialect。

## Agent 与 Sink

| 方法与路径 | 输入与结果 |
| --- | --- |
| `GET/POST /agents`、`GET/PATCH/DELETE /agents/{id}` | 共享 Agent 定义的查询与变更 |
| `GET /agent-tools`、`GET /agent-tools/{id}` | 本机 Agent 工具发现与参数 schema |
| `POST /organization/ruminate` | 在本机执行显式 rumination，使用配置的 Agent |
| `GET /sink-types`、`GET /sink-types/{id}` | 共享 Sink 类型和配置 schema |
| `GET/POST /sinks`、`GET/DELETE /sinks/{id}` | 持久 Sink 实例 |
| `PUT /sinks/{id}/config` | 替换完整配置 |
| `POST /sinks/{id}/enable`、`POST /sinks/{id}/disable` | 写入当前 Peer 的启用意图 |
| `POST /sinks/{id}/query` | 启用的 Agent Query Sink 接受 `{query,timeout_seconds?}`，202 返回普通 Job |
| `POST /sinks/{id}/mcp` | 经 Sink PAT 认证的 MCP JSON-RPC |

运行与配置见 [Agent](agent-runtime.md)、[Organization](organization-runtime.md)、[Sink](sink-runtime.md) 和 [MCP](mcp-runtime.md)。

## 扩展管理

根路径下 `GET /extensions` 返回共享记录分页，`GET /extensions/{namespace}/{name}` 读取单条记录。`POST` 到单条记录的 `/selection` 保存 `{distribution,version}` 的精确选择，`DELETE` 撤销。启用和停用通过同一坐标的 `/enable` 与 `/disable`。这些操作可用 `route_to_peer` 指定执行者。固定本机入口 `POST /extension-management/v2` 接收同一协议的命令，并返回执行 Peer 和扩展状态。

本机旧接口保留在 `/api/v1`。下表中的路径须加该前缀。

| 方法与路径 | 输入与结果 |
| --- | --- |
| `GET /extensions/catalog` | 启动配置白名单的精确坐标 |
| `POST /extensions/install` | `{name,version}`，保存当前 Peer 的本地精确产物选择 |
| `GET /extensions` | selection、enabled、loaded_version、running、locally_disabled 和错误 |
| `POST /extensions/{name}/enable` | 保存启用意图并尝试运行 |
| `POST /extensions/{name}/disable` | 本机停止并同步停用意图 |
| `POST /extensions/{name}/invoke` | `{method,parameters}`，返回插件业务 JSON |
| `GET/PATCH /extensions/{name}/config` | 明确配置入口；管理列表不包含业务 state |
| `DELETE /extensions/{name}/participation` | 移除本 Peer 选择，保留部署共享身份与业务资料 |

旧接口的 Extension name 必须将斜杠编码为 `%2F`。其 catalog 来源于 `--extension-manifest` 或 `INKCRE_EXTENSION_MANIFESTS` 指定的本机可信 manifest。根路径的精确选择可从 Registry 获取本平台 Rust 产物。配置更改在下一次启动生效。启用后按 capability descriptor 注册 typed adapter，停用会撤回本机能力并取消在途调用。构建和安装方式见 [运行说明](../40-deployment/runtime.md#插件生命周期)。
