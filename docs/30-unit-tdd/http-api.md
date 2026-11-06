# HTTP API

接入初始 Rust app 时阅读本文。所有业务入口位于 `/api/v1`，携带 `Authorization: Bearer <JWT>`；健康检查 `/health/live` 和 `/health/ready` 无需认证。JSON 错误包含 code 与 message，数据库和插件原始错误不会直接返回。

## 图数据

| 方法与路径 | 输入与结果 |
| --- | --- |
| `GET /blocks?limit=50&cursor=123` | `items/next_cursor`；ID 降序，cursor exclusive |
| `POST /blocks` | `{resolver,content,storage?}`，201 返回 Block |
| `GET/PATCH/DELETE /blocks/{id}` | 读取、部分更新或删除；删除返回 204 |
| `GET /blocks/{id}/content` | inline 文本或 PostgreSQL storage 的实际 bytes |
| `GET /blocks/{id}/relations` | 关联 Relation 分页 |
| `POST /relations` | `{from_,to_,content}`，201 返回 Relation |
| `GET/PATCH/DELETE /relations/{id}` | 读取、部分更新或删除 |
| `GET /relations/{id}/neighborhood` | Relation 和其两个端点组成的闭合图 |
| `POST /graph/neighborhood` | `{block_id,direction?,contents?,limit?,cursor?}` |
| `GET/POST /graph/path` | source、target、direction、contents、max_hops、max_explored_blocks；POST 使用 JSON body |

`direction` 为 `in/out/both`；`contents` 是精确 Relation content 过滤，空数组代表不过滤。GET path 的 contents 是经 URL 编码的 JSON 字符串数组，可完整表达含逗号的内容。路径结果包含 `outcome=found/not_found/limit_reached`、闭合 `graph` 与有序 `block_ids/relation_ids`，不能把预算耗尽当作没有路径。PATCH 省略字段保持原值；Block storage 显式 null 清空，其余非空字段不接受 null。

例如创建普通文本：

```json
{"resolver":"core.text.v1","content":"一条资料"}
```

## 存储、Peer 与 Job

| 方法与路径 | 输入与结果 |
| --- | --- |
| `GET /storages` | 已有 storage 配置；不创建私有存储表 |
| `POST /storage-blobs` | 原始 bytes，201 返回 `{id}` |
| `GET/PUT/DELETE /storage-blobs/{uuid}` | 原始 bytes 读写，更新/删除返回 204 |
| `GET /self` | 当前 Peer、应用版本与支持的分发格式 |
| `GET /peers`、`GET/DELETE /peers/{uuid}` | Peer 分页、读取、移除 |
| `GET/POST /job-types` | catalog 分页或发布精确 JobType schema |
| `GET/DELETE /job-types/{id}` | 读取或移除 catalog 项 |
| `GET/POST /jobs` | 分页或提交 `{type,parameters,timeout_seconds?}`，提交返回 201 |
| `GET/DELETE /jobs/{id}` | 读取或删除未运行的记录 |
| `POST /jobs/{id}/claim` | 原子领取 pending Job，调用者需具备执行能力 |
| `POST /jobs/{id}/finish` | `{status,state}`；只关闭 running Job |
| `POST /jobs/{id}/abort` | pending 立即中止，running 保存 abort_requested |

JobType 输入为 `{id,description,parameters_schema,default_timeout_seconds}`。提交依据共享 catalog 验证参数，不因 Rust 本机缺少 handler 而拒绝。提供 Job 协议操作不等于本机实现了 Python 的所有 worker。

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

Extension name 是 `namespace/name`，路径中需要将斜杠编码为 `%2F`。配置更改不自动重启插件。Rust 公共 Registry 安装尚不提供；catalog 来自本机可信 manifest。
