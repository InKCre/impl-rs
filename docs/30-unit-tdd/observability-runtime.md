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

Loki 和 Relay 的关闭会撤销本机连接并取消在途网络 future。Loki 保留既有的有界排空过程，OTLP relay 仅对显式请求转发。应用自身的固定 metadata span、event 和 metric 使用下述 Trace API 导出。通用 tracing subscriber 不会把任意日志或字段桥接到外部 backend。

Loki/OTLP 已通过独占本地 PostgreSQL 加 loopback HTTP 接收端验收，覆盖私有认证、默认内容过滤及显式放行、Loki 查询与 lookahead、三种 OTLP 信号、无效 protobuf、媒体与体积边界、上游失败和禁用重定向。无效 Loki 私有配置保持所选 backend 不可用，独立启用的 OTLP relay 仍可工作。PostgreSQL 既有 HTTP 旅程已回归通过。

## 原生 Trace API 与因果传播

`Runtime::start_telemetry` 从本实例已准入的 Relay 连接初始化标准 OTLP protobuf 导出器。Resource 包含 core-rs、版本、本实例 UUID、Peer 和 deployment UUID；不会运行环境 resource detector。未启用或没有可用信号时，不注册全局 provider，也不改变业务 readiness。HTTP middleware 只在至少一个信号可用时建立 http.server scope。

SDK 调用者通过 `Core::operation`、`client_operation`、`server_operation` 和 `linked_operation` 建立固定名称 scope。`run` 根据 Result 分类 success/error，`scope` 允许业务显式设定 Outcome；已进入 scope 的 future 被取消时记录 cancelled。错误类型、状态和准入属性可以显式设置，API 不读取错误消息、Debug、请求内容或堆栈。`Core::record_event` 同时发送本机记录和可选 OTLP event log。`record_ai_usage` 将已知非负整数与未知字段分别计数。

上下文属于当前 Runtime，并经 Tokio task-local 在 scope 内传递。`Core::trace_scope` 的显式快照可以带入新 spawned task；不同 Core 实例不会共享上下文。scope 在构造异步状态前装箱内部 future，避免多层 AI/Agent scope 放大默认 Tokio 栈。传播使用标准 W3C parser，仅接受不超过 512 字节的 traceparent/tracestate，不传播 baggage。只有 traces 信号可用时才创建有效 span。

HTTP 只记录方法、route 模板和响应状态。Job 的所有事务提交入口在没有显式 carrier 时捕获当前 scope；Cron 物化因此也保存自身 span carrier。Worker 领取成功后建立独立 job.execute trace，通过 link 指向提交时的 span，handler task 显式继承执行 scope。执行结束与 handler 取消不改写已持久化的 Job 业务效果。

AI 的 ai.chat/ai.embed scope 记录目录模型标识、方言、能力类型和数值 usage。OpenAI 与 Alibaba SSE 响应仅抽取明确的 token 数值；自定义 Dialect 可以覆写 usage-aware 方法，旧适配器保留默认未知 usage 的兼容性。Agent 记录 agent.turn、agent.step、agent.tool，包含会话、轮次、模型调用次数与终止类型。provider 生成的 tool-call ID 仅以 SHA-256 摘要进入运营 span。Prompt、system message、tool schema/arguments/results、模型输出、provider key 与连接 URL 均不进入这些 span。

Trace 与 event-log 各有 256 条等待队列，批次最多 256，最长 5 秒触发导出。失败批次不重试，不影响调用者。累计 metrics 每 30 秒导出，最多保留 2000 组 instrument/label；duration 使用固定 14 个秒级边界，计数器保留整数精度。metadata 最多 64 个字段，字符串限制 256 字符，数组限制 32 项，并按显式白名单过滤。记录关闭拒绝新输入，三信号并发排空，每个任务最多等待 32 秒；未完成记录计入 exporter dropped，再撤销 Relay 私有连接并关闭数据库。

已以真实 HTTP、原生 PostgreSQL、独立 Worker 和 loopback protobuf 接收端验证：远端 parent、持久 Job carrier、独立 execution link、handler/event 关联、实例隔离、取消 scope、三信号排空和 2 万事件压力下的有界丢弃。真实 Agent 两轮模型调用及 tool、AI chat/embed usage 和 Cron 物化均通过；检查导出 wire 不含 fixture 的业务内容、凭据或原始 tool-call ID。未引入自动化测试套件或数据库 migration。

Peer HTTP 每个候选的实际发送独立建立 peer.http CLIENT span。传输层用当前 scope 的 traceparent/tracestate 替换 payload 中同名 header，远端 http.server 直接接续该客户端 span。失败切换仍只依据连接前失败或明确 not-executed，观测字段区分 not_executed、unknown、responded；HTTP 5xx 记录错误 outcome，保留已经响应的业务结果。认证、URL、query、请求与响应 body 不进入 span。

AI 的 provider 数字字段另外支持 gen_ai.usage.cache_read.input_tokens 与 gen_ai.usage.reasoning.output_tokens，限制为非负、非 bool、int64 范围内的整数。gen_ai.response.finish_reasons 最多 16 项，仅保留 stop/length/tool_calls/content_filter/function_call，其他值转为 other。SSE 终态 usage-only chunk 替换前面的 totals，保留前面已收到的结束原因；不会重复累加分块 totals。

Agent retrieve 工具仅在对应分支成功后发 inkcre.retrieval.candidates，内容为检索方法、实体 ID 和候选数量。get_entities 发 persisted 的 inkcre.entity.read，Resolver invoke 的成功项发 resolved 的同名事件。错误项和 describe 不产生“已读取”的事件；这些事件不复制查询文本、实体内容、Resolver 参数或返回值。点位位于相应 Agent 适配层，与 canonical Python 的业务边界一致。

上述增强已以独立 Peer 入站和出站、伪造 payload carrier、not-executed/unknown/500 接收端、五工具 Agent、JSON 和 Alibaba SSE 模型响应联合验收。固定规范字段、远端 span 父子关系和外发 wire 内容均已检查。
