# 知识与 Storage 运行时

实现 Resolver、Storage handler 或读取资料时阅读本文。跨单元语义由 [知识能力契约](../_shared/20-product-tdd/knowledge-capability-contract.md) 拥有；Rust API 以 `crates/core/src/knowledge/` 为准。

## 权威与读取边界

`Core` 保留 raw Block/Relation CRUD；普通持久记录读取不要求相应 Resolver 已加载，也不重新执行业务输入校验。`Core::knowledge()` 返回同一 Core 实例共享的运行时 registry。不同 Core 实例的注册、撤销和缓存相互独立。

读取分为三层：

- raw record 是数据库行，`content` 可能是 Storage pointer。
- actual content 是 inline text 或 Storage 读取的实际 bytes；Storage 不解释 MIME、文件名和资料含义。
- solved content 是 exact Resolver 解算的使用投影；text 和 label 也由该 Resolver 拥有。

未知 Resolver、未知 Storage handler、没有该方法、不支持该能力、支持但没有有意义结果，以及作者明确写入的空字符串分别表达。`get_text` 的 `None` 不等于 `Some("")`；未知类型不会回退为纯文本。

`ResolverContext` 代表一次 Block 行快照，并持有本实例的 actual、solved 和直接 Relation 缓存。需要观察 Block 行的新 resolver/content/storage 时，创建新的 reader。`refresh` 重新读取当前 pointer 后的字节或当前 Relations，并替换相应本地缓存；它不授予 AI、重新采集或覆盖已有派生内容的权限。`materialize_missing` 单独表达允许填补缺失派生信息。内建文本解码器没有 materialization 副作用。

## 注册与扩展适配

`Resolver` 和 `StorageHandler` 是可注册的 async traits，handler 不捕获所属 Core。执行所需的 Core、Block 和关系上下文由每次调用传入。`Registry` 以 exact contract ID 选择 handler，同一 Arc 的重复注册幂等，不同 handler 抢占同一 ID 明确失败。

扩展适配器使用当前 running generation 的 UUID 作为 owner。撤销 owner 只移除这一代的 adapter，不能删除内建 handler 或另一个 running generation。宿主撤销后仍存活的调用也不能借旧 adapter 重新启动已停止的插件；进程句柄和调用收尾由扩展宿主拥有。

typed read 方法拥有名称、说明和 JSON input schema。公共方法包括 `get_raw_content`、`get_solved_content`、`get_text`、`get_label`、`get_relations` 和 `get_transfer_url`；额外只读方法使用 `get_` 或 `read_` 前缀。公共方法的参数合同不能由某个插件重定义。调用边界先校验参数，再执行具体方法。

直接关系的方向相对当前 Block：incoming 以当前 Block 为 target，outgoing 以当前 Block 为 source。它不隐含递归。Resolver context 的直接读取有明确数量边界，超过边界返回错误，不静默截断为一个看似完整的图。

## 内建能力

本阶段内建语义 Resolvers 为 `core.text.v1`、`core.markdown.v1`、`core.html.v1` 和 `core.file.v1`，并可读取 `core.source.v1` 的 Source anchor。text 与 Markdown 解算和文字投影保留源文；HTML solved 保留源文，文字投影保留段落、代码、表格单元和链接目标，不执行脚本或请求资源。通用 HTML 文字投影不是文章正文提取。

`core.file.v1` 要求 storage-backed bytes，解算结果包含原始 bytes、byte size 和可选的字节签名 media type。普通文字投影明确不支持，lexical context 只返回文件的有界字节事实，label 为 `file`。字节签名检测是 Resolver-owned evidence，不会自动选择另一种 Resolver；没有 PDF、图像或音视频解码器时，Source 仍需明确决定是否按通用 file 保存。

字节文本有 Unicode BOM 时按 BOM 严格解码，否则要求 UTF-8。HTML 额外识别有界前缀中的 meta charset 声明；无法识别的编码或无效字节明确失败。HTML label 优先 title，再取标题元素。媒体容器类型没有因此自动实现；media-type helper 仅返回当前已安装的 exact Resolver，不猜测通用 file fallback。

`JsonResolver` 是供扩展 canonical JSON 合同复用的 codec。扩展仍须给出自己的 exact ID 和 input schema；本阶段没有新增 `core.json.v1`。新输入按 schema 校验，普通解算只恢复 JSON 表示。具有图关联、文本和 label 语义的扩展应实现自己的 Resolver，而不是让通用 codec 推断业务含义。

## Storage 与事务

`postgresql_binary` 支持 byte C/R/U/D；pointer grammar 由 handler 拥有。其读写可接受 caller 的 PostgreSQL connection，因此 blob 与图行可以处在同一事务中。修改 bytes 不改写持有 pointer 的 Block，也不承诺使其他 Peer 的缓存失效。

`http` 只读 opaque HTTP(S) pointer，遵守 timeout、redirect 和 byte limit 配置。它返回 bytes，不根据响应 header 选择 Resolver，不将远端失败或超量响应伪装为空内容。

`WritableStorageHandler` 与只读 Storage capability 分开。外部进程或外部服务上的字节写入有自己的副作用边界，SQL rollback 不能回滚这些外部效果。调用者必须按具体 handler 的契约安排失败清理，不声称存在跨进程事务。

impl-py 的 artifact 初始化内建 Storage catalog。Rust 不用语言投影不同的 schema 覆盖这些内建行；扩展贡献的 Storage type 可以由相应运行时发布。

## 内容准入与 graph draft

`Knowledge::create_block` 和 draft 提交是业务内容接受边界，要求 exact Resolver 并调用其内容准入能力。原始 Core CRUD 仍是已准入数据库协议的低层操作，不冒充业务验证。

`GraphDraft` 是有 root 的 authoring command，与读取的 `GraphModel` 分开。新 Block 使用命令局部负 ID；Relation 可以引用本命令局部 ID 或既有正 ID。零端点、重复局部 ID 和未声明引用都在写入前拒绝。提交在一个事务中校验新表示、映射局部 ID，并写 Block/Relation；caller 可以把 PostgreSQL byte 写入组合进同一事务。

Resolver drafting 只产生 authoring command，不自动持久化。提交和外部 materialization 的权限与效果必须在调用边界明确，不能从普通读取或 `refresh` 推导出来。
