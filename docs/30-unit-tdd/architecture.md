# Rust Peer 内部设计

需要理解 Rust SDK、HTTP 应用和扩展责任时阅读本文。跨单元语义以 [拓扑](../_shared/20-product-tdd/unit-topology.md)、[系统状态与权威](../_shared/20-product-tdd/system-state-and-authority.md) 和 [知识能力契约](../_shared/20-product-tdd/knowledge-capability-contract.md) 为准。

## 已确定边界

`core` 是 Rust SDK/lib，负责数据库协议准入、持久化操作和 Peer 能力。它不依赖 HTTP server 或进程环境变量。`app` 是单一 HTTP 可执行文件，组合 core、认证、配置、日志、扩展运行时和优雅退出。

PostgreSQL 的已准入 `inkcre` schema 既承载持久信息，也承载部署和 Peer 状态。Rust 直连数据库，并以数据库提供的协议兼容声明决定是否允许启动；Python migration 进程仍是唯一 schema 变更入口。Rust 不复制 migration 或通过 PostgREST 间接访问。

扩展身份、config 和 state 属于共享部署，精确分发选择与 enabled 属于 Peer，running 属于进程。扩展载入与 SDK 选择必须保留这些权威边界。不同语言的同一扩展共享 Extension ID 和业务数据契约。

## 初始实现范围

初始 core 提供图数据 CRUD、存储字节、有界图导航、Peer 身份与租约、Job 提交与状态操作，以及扩展参与管理。Job 提交依据共享 catalog 的参数 schema 准入，不要求提交者具备本地执行能力。原子领取和超时使用数据库约束及数据库时钟，不另造 worker ownership 或 heartbeat 字段。

app 以 `/api/v1` 暴露这些能力；它是 Rust 初始接口，不承诺与 Python 全部业务 API 等同。AI、Organization、既有第一方 Source/Sink 的语言移植不因基础协议能力完成而自动成立。

| 能力 | 当前实现边界 |
| --- | --- |
| 知识持久化 | 原始 Block/Relation CRUD、组合事务、闭合图邻域与有界最短路径；高层图提交、Resolver drafting 与 materialization 尚未实现 |
| Storage | inline 与 PostgreSQL blob 的实际内容读取；可注册 handler 与 HTTP storage 尚未实现 |
| Peer | 稳定身份、保留用户配置的注册、能力 snapshot 与租约；capability discovery、HTTP delegation 和 outcome-unknown 协议尚未实现 |
| Job | catalog、参数 schema 准入、提交、原子 claim、终态、abort flag 与 timeout SQL；handler registry、worker、自动发现和执行中取消尚未实现 |
| 定时与采集 | Source collect/backfill/state/cursor 和 Cron runtime 尚未实现 |
| 知识解释与应用 | Resolver registry、typed read、solved/text/label、媒体处理、lexical/semantic retrieval 尚未实现 |
| AI 与业务运行时 | AI providers/models/dialects、Organization、Agent、Sink 及其工具链尚未实现 |
| 运维与接口 | stderr/HTTP tracing、serve/db-check/token；完整观测查询与业务 CLI 尚未实现 |

app 的后台循环只负责 Peer 续租与插件 reconcile。存在 Job 数据接口不表示本机已经执行 Job；同样，插件可直接使用 core 读写信息库，也不会使通用方法自动成为 Source、Resolver、Storage 或 Job handler。`example/notes` 是机制与数据往返示例，尚未移植既有第一方扩展。

数据库准入使用 `extension-distribution-open-v1`。native runtime 使用具备 `authenticated` 能力的独立登录角色；HTTP JWT 则校验 HS256、role、issuer、audience、iat 和 exp，不设置 token lifetime 上限。

## 插件边界

`rust_stdio` 分发是本机可信 executable。宿主通过版本化 stdio JSON 消息初始化、启动、调用和关闭插件，stdout 只承载协议。独立进程允许单一 app binary 动态增加插件，避免将 Rust 编译器 ABI 当作长期兼容协议；代价是进程开销和显式序列化。

扩展 SDK 在 `ext-reg/runtimes/core-rs` 独立维护，本仓通过固定版本子模块消费，避免复制协议类型。初始实现从启动配置明确允许的本地 manifest 安装精确产物，校验身份、semver、协议、SDK 条件及摘要。HTTP 请求不能引入任意本机 executable 路径。Registry 目前仅交付 Python/MF；Rust 公共上传、下载与发布不在本地运行完成的声明中。

保存 selection/enabled 后应用运行变化；插件启动返回后再次确认精确选择与启用意图。停止 Peer 或插件只撤回运行事实，持久配置、业务 state 和已保存资料继续存在。

原生插件复用宿主 Peer。显式受信的插件通过 `INKCRE_DATABASE_URL` 接收 native 数据库能力，可另行采用 core SDK；SDK 本身不反向依赖 core，也不把凭据保存到 manifest 或共享 config。`examples/note-extension` 演示实际文本保存与读取。
