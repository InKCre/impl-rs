# Rust Peer 内部设计

需要理解 Rust SDK、HTTP 应用和扩展责任时阅读本文。跨单元语义以 [拓扑](../_shared/20-product-tdd/unit-topology.md)、[系统状态与权威](../_shared/20-product-tdd/system-state-and-authority.md) 和 [知识能力契约](../_shared/20-product-tdd/knowledge-capability-contract.md) 为准。

## 已确定边界

`core` 是 Rust SDK/lib，负责数据库协议准入、持久化操作和 Peer 能力。它不依赖 HTTP server 或进程环境变量。`app` 是单一 HTTP 可执行文件，组合 core、认证、配置、日志、扩展运行时和优雅退出。

PostgreSQL 的已准入 `inkcre` schema 既承载持久信息，也承载部署和 Peer 状态。Rust 直连数据库，并以数据库提供的协议兼容声明决定是否允许启动；Python migration 进程仍是唯一 schema 变更入口。Rust 不复制 migration 或通过 PostgREST 间接访问。

扩展身份、config 和 state 属于共享部署，精确分发选择与 enabled 属于 Peer，running 属于进程。扩展载入与 SDK 选择必须保留这些权威边界。不同语言的同一扩展共享 Extension ID 和业务数据契约。

## 当前实现范围

core 提供图数据 CRUD、存储字节、有界图导航、Peer 身份与租约、知识能力 registry、graph draft、Job worker、Source 和 Cron。Job 提交依据共享 catalog 的参数 schema 准入，不要求提交者具备本地执行能力；本机 handler 的资格判断与原子领取分别管理。持久时间使用数据库时钟，不另造 worker ownership 或 heartbeat 字段。

app 以 `/api/v1` 暴露这些能力，接口参数见 [HTTP API](http-api.md)。当前已移植既有 RSS/Atom 扩展；这不代表完整复刻 Python 的业务能力或全部 API。

| 能力 | 当前实现边界 |
| --- | --- |
| 知识持久化 | 原始 Block/Relation CRUD、组合事务、闭合图邻域、有界最短路径、Resolver drafting 与带局部 ID 映射的事务提交 |
| Storage | 可注册只读/可写 handler；PostgreSQL bytes C/R/U/D 与 HTTP(S) 有界读取；外部 handler 的副作用不具备跨进程 SQL 原子性 |
| 知识解释 | exact Resolver registry、typed read、solved/text/label、inline/Storage hydration；内建 text、Markdown、HTML、file 与 Source anchor；尚无 PDF、图像或音视频解码器 |
| Peer | 稳定身份、保留用户配置的注册、能力 snapshot 与租约；跨 Peer capability discovery、HTTP delegation 和 outcome-unknown 协议尚未实现 |
| Job | catalog/schema 准入、handler registry、资格判断、pending 发现、原子 claim、并发预算、持久 checkpoint、执行中取消与终态收尾；不自动重试 |
| 定时与采集 | Source catalog/config/state/anchor、collect/backfill 框架、五段 Cron 与时区；RSS/Atom 实现真实采集、正文和附件，RSS backfill 明确不支持 |
| 检索与其他第一方扩展 | lexical/semantic/hybrid retrieval、其他 Source/Sink 分发尚未实现 |
| AI 与业务运行时 | AI providers/models/dialects、Organization、Agent、Sink 及其工具链尚未实现 |
| 运维与接口 | stderr/HTTP tracing、serve/db-check/token；完整观测查询与业务 CLI 尚未实现 |

知识与 Storage 的事务、读取和缓存边界见 [知识运行时](knowledge-runtime.md)；Job 执行与 Cron 发生见 [任务运行时](job-runtime.md)；RSS 的 canonical 图、游标与 materialization 见 [RSS 分发](rss-extension.md)。扩展通用方法不会自动成为业务 handler，必须通过 SDK capability descriptor 声明并注册 typed adapter。

app 默认运行 Peer 续租、插件 reconcile、Job worker 与 Cron loop。worker 会发现共享 pending 队列中的本机候选，Cron loop 会为部署级模板创建普通 Job；连接共享库前应按运行目的选择 `--no-worker`、`--no-worker-discovery` 和 `--no-cron`。它们只影响当前进程，不停用其他 Peer 或删除持久配置。

Rust worker 取消执行并等待本机收尾后写终态，但进程崩溃或关闭写入失败会留下 running 行，需要确认实际执行器停止后显式维护。当前 Python scanner 仍可能自动关闭全局过期行，混合部署不具备跨执行器的统一收尾保证，具体限制见任务运行时。

数据库准入使用 `extension-distribution-open-v1`。native runtime 使用具备 `authenticated` 能力的独立登录角色；HTTP JWT 则校验 HS256、role、issuer、audience、iat 和 exp，不设置 token lifetime 上限。

## 插件边界

`rust_stdio` 分发是本机可信 executable。宿主通过版本化 stdio JSON 消息初始化、启动、调用和关闭插件，stdout 只承载协议。独立进程允许单一 app binary 动态增加插件，避免将 Rust 编译器 ABI 当作长期兼容协议；代价是进程开销和显式序列化。

扩展 SDK 在 `ext-reg/runtimes/core-rs` 独立维护，本仓通过固定版本子模块消费，避免复制协议类型。宿主从启动配置明确允许的本地 manifest 安装精确产物，校验身份、semver、协议、SDK 条件及摘要。HTTP 请求不能引入任意本机 executable 路径；Rust 公共 Registry 上传与下载尚未实现。

保存 selection/enabled 后应用运行变化；插件启动返回后再次确认精确选择与启用意图。停止 Peer 或插件撤回本机运行能力并收尾在途任务，持久配置、业务 state 和已保存资料继续存在。插件 abort/timeout 使实际子进程被回收；仍被选择且 enabled 的精确分发可由 reconcile 恢复，原 Job 不重放。

原生插件复用宿主 Peer。显式受信的插件通过 `INKCRE_DATABASE_URL` 接收 native 数据库能力，可另行采用 core SDK；SDK 本身不反向依赖 core，也不把凭据保存到 manifest 或共享 config。[文本保存示例](../../examples/note-extension/README.md) 演示通用调用，[RSS/Atom 分发](../../extensions/rss/README.md) 演示同一扩展身份下的真实业务能力。
