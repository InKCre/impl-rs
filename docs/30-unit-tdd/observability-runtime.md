# PostgreSQL 可观测性运行时

查询部署日志或嵌入事件记录时阅读本文。共享 `inkcre.observability.backend` 配置使用 `inkcre.observability.backend.v1`，默认选用 PostgreSQL。运行时在启动时固定该选择，配置变化在下次启动生效。配置选择 Loki 而本机缺少连接时，status 保留所选 backend 并报告不可用。

`observability::Runtime::start` 为当前 Core 建立事件出口。Core clone 共享 weak binding，独立 Core 保持各自的记录与错误状态。`Core::record_event` 写入已命名事件与元数据，Worker 在成功领取后产生 job.started，在终态持久化成功后产生 job.closed。事件包括 Job ID、状态和 operation，记录时补充当前 Peer 身份。

记录队列容量为 1024，每批最多写入 100 条。emit 同步返回接纳结果，队列满或参数无效时增加 dropped。写入使用独立短事务，失败批次增加 dropped 并保留固定诊断。正常关闭拒绝新记录并等待最多 5 秒排空，超预算终止 writer，并计入剩余记录。业务执行收尾完成后关闭观测运行时，再关闭数据库。

POST /observability 受 Peer JWT 保护，支持 status、query、ingest。单请求最多 256 KiB，最多 4 个请求并发，请求预算 15 秒。查询另有 4 个并发槽及 10 秒预算。ingest 每批最多 100 条，body、ID、severity、时间与 scalar metadata 均按共享格式准入，参数错误统一返回简短诊断。

query 必须指定有时区的 since/until，窗口最多 31 天，采用左闭右开区间。单页 1–500 条，以 timestamp、数据库 ID 倒序排列，lookahead 决定 truncated。Job、trace、Peer、operation、kind 与最低 severity 可组合，text 按 literal ILIKE 匹配。历史日志保留旧 nested extras，并以 pg.<ID> 表示缺少公开记录 ID 的行。

expected_backend 与实际启动选择不一致返回 409。观测容量不足返回 429，输入无效返回 422，超大请求返回 413，不可用 backend 返回 503。SQL 与驱动错误不会作为日志内容回传。运行时查询失败会报告错误状态，调用者可与空结果区分。

已验收 PostgreSQL 记录和查询、HTTP 过滤与界限、历史数据兼容及真实 Worker 生命周期事件。OTLP、Loki 客户端与通用 tracing 日志桥接由后续传输接入承担。
