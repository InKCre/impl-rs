# RSS Source 与 Rust 分发

修改 Source 执行框架、RSS 采集或读取逻辑时阅读本文。跨语言的来源身份、持久信息和能力边界由 [知识能力契约](../_shared/20-product-tdd/knowledge-capability-contract.md) 拥有；既有 RSS 业务契约来自 `impl-py` 的主分支 `extensions/rss`。

## Source 与执行能力

`core::source` 管理共享 `sources_types` catalog、Source 实例与配置、state patch、延迟来源锚点和 collect/backfill Job。创建、更新、提交命令依据持久 catalog 的 JSON Schema 准入，不要求提交者加载插件。Job 和 Cron 共用 Source 命令校验，backfill schema 为 null 时明确拒绝。

`SourceRegistry` 保存当前进程具备的 handler，按 owner 撤销。`SourceRuntime` 向普通 Job worker 提供两个 exact Job handler，领取前检查对应 Source handler 的本机可执行性；它不另建 RSS worker、重试队列或私有任务状态。动态插件停用时撤回本机能力，并取消执行中的 RPC。worker 等待实际进程回收后关闭本次 Job；重新启用可以领取新提交的命令，不重放已中止的 Job。catalog、Source state 和已采集资料继续存在。

Source 可拥有 `core.source.v1` 锚点，内容仅为 `id/type/nickname`。锚点由显式请求延迟创建，改昵称时同步已存在的锚点；删除 Source 配置不删除锚点和资料。内建 Resolver 可独立解释该锚点，不捕获某个 Core 实例。

## 同一 RSS 业务身份

独立进程采用 extension SDK 的 typed `source_collect` 与 `resolver_read`，使用 `inkcre-core` 直连 PostgreSQL。通用 `invoke` 仅提供显式正文、附件 materialization 命令。下面这些持久标识不能因分发语言变化而修改：

| 用途 | 标识 |
| --- | --- |
| Extension Name | `inkcre/rss` |
| RSS Source | `extensions.rss.rss.Source` |
| Atom Source | `extensions.rss.atom.Source` |
| Feed Resolver | `extensions.rss.feed.v1` |
| Item Resolver | `extensions.rss.feed_item.v1` |
| Enclosure Resolver | `extensions.rss.enclosure.v1` |

Canonical JSON 保留 Python 的字段与 null/default 语义，协议元数据与下载内容分别保存。Item 指向 Feed 的关系为 `feed`，指向附件元数据的关系为 `enclosure`，指向文章文本的关系为 `full_text`；附件元数据通过 `content` 指向 Storage 支持的语义内容 Block。Rust 不引入第二份 RSS 数据格式或 Source 采集台账。

## 连续性与事务

Feed 身份依次采用本机来源标识、声明的 self URL、配置 URL，并始终包含 Source 实例 ID。Item 身份依次采用原生 ID 及其 `rss_guid/atom_id` 类型、alternate URL。绝对 URL 保留原始拼写，协议规定的相对地址按 effective URL 与 `xml:base` 还原。Atom ID 和 permalink GUID 遵循 Python feedparser 的相对地址规则；不透明 GUID 和 RSS 1.0 的 `rdf:about` 保留原值。

相同 exact identity 复用根 Block；已有多个 exact root 时报告完整性错误。无身份 Item 按配置丢弃或创建，不使用内容摘要作身份。已具来源时间的无身份 Item 可以用先前成功快照的 observed-at 水位线减少重复；没有时间的无身份 Item 每次内容快照均可能新增。

条件请求的 `etag/last_modified` 仅在 `snapshot_configured_url` 与当前配置 URL 相同时复用。`last_successful_contentful_snapshot_observed_at` 仅在 `snapshot_feed_block_id` 与当前 Feed 根相同时用于准入，URL 或 graph identity 变化不会错误继承旧范围。304 只更新同一请求范围的 validators，不推进内容水位线。

网络请求和解析完成后，一个短事务锁住 Source，复核原配置与 state，再串行对账 Feed、全部主 Item/附件元数据以及新 cursor。并发执行中较旧的请求若发现 checkpoint 已变化会失败，不覆盖新状态。主图和 cursor 要么共同提交，要么共同回滚；state 使用顶层 patch 保留扩展不拥有的字段。

正文提取和附件下载在主事务之后执行。这些可重建的投影失败会记录有界诊断，不撤销已接纳的原始信息。取消或崩溃可能留下已提交的主图和 cursor；后续显式 materialization 或新采集可以恢复缺失投影，不能以 Job 终态推断来源没有产生任何副作用。

## 内容与读取

文章页面通过有界 HTTP 请求取得，再由 Readability 提取正文，保存为 `core.text.v1` 子 Block。页面整体 HTML 投影不冒充文章提取。并发 materialization 在父 Item 锁下复核 alternate URL，并保持最多一个 `full_text` 投影。

附件元数据保留协议声明 MIME、长度、URL 和 title。RSS 的语义候选顺序是声明 MIME、响应 MIME、文件名、字节证据；Atom 先响应 MIME，再声明 MIME。候选必须匹配本机已注册 Resolver；仍无匹配时保留原始字节并使用真正的 `core.file.v1`。字节、语义子 Block 与 `content` 关系在同一数据库事务内落地，不把二进制字节塞入元数据 JSON。

Feed Resolver 返回 canonical feed；Item Resolver 返回 root 和图拥有的 Feed、Enclosure、Full Text 引用；Enclosure Resolver 返回元数据和语义子 Block 引用。默认 Item text 优先已有正文，lexical text 使用 feed 自带的内容。可选 materialization 失败时仍可读取已保存的原始事实。typed read 与普通图读取分别管理，关闭 RSS 插件不会删除内容。

当前解析器使用严格 XML 与 feed-rs，支持 RSS 和 Atom，接受编码声明和相对地址。它不复刻 feedparser 对损坏 XML 的容错恢复；无法把原始协议条目与解析条目一一对应时拒绝快照，避免把 GUID 或附件分配给错误 Item。支持的解析器与正文提取器不保证输出逐字等同于 Python 库；跨语言保持相同持久结构、身份规则和业务状态范围。stdio 每帧仍受 SDK 的 1 MiB 限制。
