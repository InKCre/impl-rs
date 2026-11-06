# HTTP API

接入 Rust app 时阅读本文。下表业务路径均相对 `/api/v1`，携带 `Authorization: Bearer <JWT>`；健康检查 `/health/live` 和 `/health/ready` 无需认证。JSON 错误形如 `{"error":{"code":"…","message":"…"}}`，数据库和插件原始错误不会直接返回。接口覆盖 Rust 当前能力，不承诺 Python 的全量 API 兼容。

## 图数据

| 方法与路径 | 输入与结果 |
| --- | --- |
| `GET /blocks?limit=50&cursor=123` | `items/next_cursor`；ID 降序，cursor exclusive |
| `POST /blocks` | `{resolver,content,storage?}`，201 返回 Block |
| `GET/PATCH/DELETE /blocks/{id}` | 读取、部分更新或删除；删除返回 204 |
| `GET /blocks/{id}/content` | inline 文本或已注册 Storage handler 读取的实际 bytes |
| `GET /blocks/{id}/relations` | 关联 Relation 分页 |
| `POST /relations` | `{from_,to_,content}`，201 返回 Relation |
| `GET/PATCH/DELETE /relations/{id}` | 读取、部分更新或删除 |
| `GET /relations/{id}/neighborhood` | Relation 和其两个端点组成的闭合图 |
| `POST /graph/neighborhood` | `{block_id,direction?,contents?,limit?,cursor?}` |
| `GET/POST /graph/path` | source、target、direction、contents、max_hops、max_explored_blocks；POST 使用 JSON body |

`direction` 为 `in/out/both`；`contents` 是精确 Relation content 过滤，空数组代表不过滤。GET path 的 contents 是经 URL 编码的 JSON 字符串数组，可完整表达含逗号的内容。路径结果包含 `outcome=found/not_found/limit_reached`、闭合 `graph` 与有序 `block_ids/relation_ids`，不能把预算耗尽当作没有路径。PATCH 省略字段保持原值；Block storage 显式 null 清空，其余非空字段不接受 null。

普通 Block/Relation 读取不要求相应 Resolver 已加载。HTTP 创建或修改 Block 则会校验完整的新表示，要求本机 exact Resolver 和相应 Storage handler；未知 Resolver 不会退化为纯文本。`/content` 返回 `text/plain` 或 `application/octet-stream`，不返回 solved JSON。

例如创建普通文本：

```json
{"resolver":"core.text.v1","content":"一条资料"}
```

## Resolver 与 graph draft

| 方法与路径 | 输入与结果 |
| --- | --- |
| `GET /resolvers` | 本机已加载的 Resolver descriptor 数组 |
| `GET /resolvers/{resolver}/methods` | 该 exact Resolver 的公共及自定义只读方法与参数 schema |
| `GET /blocks/{id}/read-methods` | 根据 Block 当前 Resolver 查询方法，不解算内容 |
| `GET /blocks/{id}/solved` | Resolver-owned JSON；支持 `refresh`、`materialize_missing` query |
| `GET /blocks/{id}/text` | JSON string 或 null；另支持 `context=default/lexical` |
| `GET /blocks/{id}/label` | JSON string；支持 `refresh` |
| `POST /blocks/{id}/invoke/{method}` | body 为方法参数对象，按声明的 input schema 校验 |
| `POST /graph/draft` | `{resolver,input}`，生成未持久化的 GraphDraft |
| `POST /graph/submit` | GraphDraft，201 返回 `{root,blocks,relations}` 的持久 ID 映射 |

读取参数缺省为 `refresh=false`、`materialize_missing=true`、`context=default`。`refresh` 刷新本次 reader 的内容或关系缓存，不要求重新采集或 AI 处理；`materialize_missing` 允许具体 Resolver 填补缺失派生信息，因此读取可能产生该能力声明的副作用。只查看已保存投影时使用 `?materialize_missing=false`。支持但没有文字结果返回 null；作者写入的空字符串返回 `""`；未知或不支持的能力明确报错。

公共方法为 `get_raw_content`、`get_solved_content`、`get_text`、`get_label`、`get_relations` 和 `get_transfer_url`。通过 invoke 调用时 body 仍直接使用该方法的参数对象，例如 `{"materialize_missing":false}`；`get_raw_content` 返回带 `kind/content` 的 JSON 表示，而 `/content` 返回实际响应体。额外只读方法使用 `get_` 或 `read_` 前缀。

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
| `POST /sources/{id}/collect` | `{config?,timeout_seconds?}`，202 返回 `core.source.collect.v1` Job |
| `POST /sources/{id}/backfill` | 同上，提交 `core.source.backfill.v1`；该类型无 backfill schema 时返回 501 |
| `GET/POST /crons` | 模板分页，或创建 `{schedule,enabled?,job_type,job_parameters?,job_timeout_seconds?}`；创建返回 201 |
| `GET/PATCH/DELETE /crons/{id}` | 读取、修改或删除模板；删除返回 204 |
| `POST /crons/{id}/run` | 202 返回独立普通 Job，忽略 enabled，不改变发生进度 |
| `POST /crons/{id}/materialize` | 按当前到期分钟物化一次，返回 Job 或 null |
| `GET/PUT /cron/config` | 部署级 `{timezone}`，默认 UTC；写入影响共享 Cron 的时区解释 |

Source PATCH 支持 `nickname/config/storage`，省略保持原值，nickname/storage 显式 null 清空；config 整对象替换且不接受 null。`storage` 必须选择可写类型；state 与 cursor 由实际 Source handler 拥有。删除 Source 配置保留来源锚点与已采集资料。catalog 存在不表示本机具备 Source handler，插件停用也不会删除 catalog。

RSS Source ID 为 `extensions.rss.rss.Source`，Atom 为 `extensions.rss.atom.Source`，最小 config 为 `{"feed_url":"https://example.org/feed.xml"}`。需先安装并启用可信的 `inkcre/rss` Rust 分发，本机 worker 才具有对应能力。collect 的 config 可以为空对象；这两个 Source 不支持 backfill。具体业务语义见 [RSS 分发](rss-extension.md)。

Cron 默认 enabled，schedule 使用五段 UNIX 格式、分钟粒度，时区接受 IANA 名称。PATCH 支持创建字段，`job_timeout_seconds=null` 恢复 JobType 默认值。materialize 遵守 enabled、当前发生去重及 `last_job` pending/running 的叠加限制；删除模板不删除已生成的 Job。自动 loop 与显式入口都只生产普通 Job，执行统一交给 worker；详细语义见 [Job 与 Cron](job-runtime.md)。

分页一般返回 `items/next_cursor`，limit 默认 50、范围 1–1000，cursor exclusive。Source 按 ID 降序，Cron 按 ID 升序，SourceType 按 ID 字典序升序。

## 本机扩展

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

Extension name 是 `namespace/name`，路径中需要将斜杠编码为 `%2F`。配置更改不自动重启插件。Rust 公共 Registry 安装尚不提供；catalog 来自 `--extension-manifest` / `INKCRE_EXTENSION_MANIFESTS` 指定的本机可信 manifest。启用后按 capability descriptor 注册 typed adapter；停用会撤回本机能力并取消在途调用，普通持久记录仍可读取。构建和摘要安装方式见 [运行说明](../40-deployment/runtime.md#插件生命周期)。
