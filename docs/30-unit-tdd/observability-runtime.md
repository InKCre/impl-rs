# 可观测性运行时

查询部署日志或嵌入事件记录时阅读本文。共享 `inkcre.observability.backend` 配置使用 `inkcre.observability.backend.v1`，默认选用 PostgreSQL。运行时在启动时固定该选择，配置变化在下次启动生效。配置选择 Loki 而本机缺少连接时，status 保留所选 backend 并报告不可用。

`observability::Runtime::start` 为当前 Core 建立事件出口。Core clone 共享 weak binding，独立 Core 保持各自的记录与错误状态。`Core::record_event` 写入已命名事件与元数据，Worker 在成功领取后产生 job.started，在终态持久化成功后产生 job.closed。事件包括 Job ID、状态和 operation，记录时补充当前 Peer 身份。

记录队列容量为 1024，每批最多写入 100 条。emit 同步返回接纳结果，队列满或参数无效时增加 dropped。写入使用独立短事务，失败批次增加 dropped 并保留固定诊断。正常关闭拒绝新记录并等待最多 5 秒排空，超预算终止 writer，并计入剩余记录。业务执行收尾完成后关闭观测运行时，再关闭数据库。

POST /observability 受 Peer JWT 保护，支持 status、query、ingest。单请求最多 256 KiB，最多 4 个请求并发，请求预算 15 秒。查询另有 4 个并发槽及 10 秒预算。ingest 每批最多 100 条，body、ID、severity、时间与 scalar metadata 均按共享格式准入，参数错误统一返回简短诊断。

query 必须指定有时区的 since/until，窗口最多 31 天，采用左闭右开区间。单页 1–500 条，以 timestamp、数据库 ID 倒序排列，lookahead 决定 truncated。Job、trace、Peer、operation、kind 与最低 severity 可组合，text 按 literal ILIKE 匹配。历史日志保留旧 nested extras，并以 pg.<ID> 表示缺少公开记录 ID 的行。

expected_backend 与实际启动选择不一致返回 409。观测容量不足返回 429，输入无效返回 422，超大请求返回 413，不可用 backend 返回 503。SQL 与驱动错误不会作为日志内容回传。运行时查询失败会报告错误状态，调用者可与空结果区分。

已验收 PostgreSQL 记录和查询、HTTP 过滤与界限、历史数据兼容及真实 Worker 生命周期事件。通用 tracing 日志桥接由后续 Trace API 接入承担。

## Loki 与 OTLP 连接

选用 Loki 时，运行时同时读取共享 `inkcre.observability.v1` 的 deployment_id，并从本机 `OBSRV__LOKI_URL`、`OBSRV__LOKI_HEADERS` 取得私有连接。headers 使用 JSON 字符串对象。URL 接受 HTTP(S) 与基路径，认证使用 header。配置无效或连接缺失时 status 继续显示 Loki，recording/queryable 为 false。

默认对 Loki 仅发送固定事件名称和准入 metadata，普通日志 body 变为 application.log，未准入属性被移除。明确设置 `OBSRV__EXTERNAL_CONTENT_ENABLED=true` 后才允许原始 body 和属性送往外部 backend。Loki 查询采用 deployment stream selector、向后时间范围和 lookahead，body 文本先进行正则转义。响应最多 8 MiB，查询失败仍返回 backend 不可用。

`OBSRV__TELEMETRY_ENABLED=true` 另外启用 HTTP OTLP relay。共享 otlp_http_endpoints 提供 traces、logs、metrics 的完整目的地，本机 OTEL_EXPORTER_OTLP_<SIGNAL>_ENDPOINT 可以覆盖它们。认证使用 OTEL_EXPORTER_OTLP_HEADERS 或单个 SIGNAL_HEADERS，格式为逗号分隔的 key=percent-encoded-value。单个信号的 TIMEOUT 覆盖 OTEL_EXPORTER_OTLP_TIMEOUT，缺省 10 秒，范围大于 0 且不超过 30 秒。配置错误按信号隔离。

POST /telemetry/v1/{signal} 接受未压缩的 application/x-protobuf，复用 Peer JWT，最多 256 KiB 和 4 个并发请求。Relay 解码对应的标准 OTLP request，向已配置目的地转发，并校验标准 response。发送方无法指定目的地或转发认证 header。上游凭据只来自本机配置。重定向保持关闭。无可用信号返回 503，格式无效返回 400，媒体类型或压缩不支持返回 415，上游失败返回固定的 502。

Loki 和 Relay 的关闭会撤销本机连接并取消在途网络 future。Loki 保留既有的有界排空过程，OTLP relay 仅对显式请求转发。应用自身的自动 span/metric exporter 与通用 tracing 桥接另由 Trace API 接入。

Loki/OTLP 已通过独占本地 PostgreSQL 加 loopback HTTP 接收端验收，覆盖私有认证、默认内容过滤及显式放行、Loki 查询与 lookahead、三种 OTLP 信号、无效 protobuf、媒体与体积边界、上游失败和禁用重定向。无效 Loki 私有配置保持所选 backend 不可用，独立启用的 OTLP relay 仍可工作。PostgreSQL 既有 HTTP 旅程已回归通过。
