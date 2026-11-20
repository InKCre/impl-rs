# Telegram Rust 分发

`inkcre-telegram-extension` 是 `inkcre/telegram` 的独立 `rust_stdio` 分发，复用 Source、Job、Storage 与原生 PostgreSQL。新采集只保存普通文本、HTML 和附件 metadata；已发布的 `extensions.telegram.message.v1` 内容仍可读取。

```sh
cargo build --locked -p inkcre-app -p inkcre-telegram-extension
target/debug/inkcre extension-manifest --name inkcre/telegram --version 0.1.0 \
  target/debug/inkcre-telegram-extension > target/debug/telegram.extension.json
```

将 manifest 和 executable 放入同一受信目录，通过 `INKCRE_EXTENSION_MANIFESTS` 提供本机分发，然后选择精确版本并启用。宿主注入数据库连接，插件直连既有 PostgreSQL，不执行 schema migration。

Source 类型是 `extensions.telegram.source.Source`，配置为 `{"bot_token":"…","bound_user_id":123,"download_attachments":false}`。只接收指定 Telegram 用户私发给该 bot 的消息；群、频道、其他用户和非 message update 不保存内容。一个 bot 只配置一个 Source，因为 Telegram 为每个 bot 提供一个全局 update queue；此约束由部署者维护。Source 不创建计划，collect 可以由用户提交 Job 或配置 Cron。

首次采集固定 bot identity，之后更换 bot 会失败。每个 update 的内容和游标在同一数据库事务中提交；重放不重复创建内容，失败的 primary write 不提前确认 update。Telegram 游标使用已提交 update ID + 1，并在每页最多 100 条的短轮询中继续拉取。UTF-16 entities 转为普通 `core.html.v1`，无 entities 的文本保存为 `core.text.v1`，转发仅保存消息所含内容。

附件使用 `extensions.telegram.attachment.v1`，保留文件身份、类型、尺寸与可用 metadata；照片选最大分辨率。Source anchor 通过 `collects` 指向附件，附件通过 `caption` 指向普通文本或 HTML。默认不下载文件，读取 solved/text/label 也不发起网络请求。

显式 `POST /telegram/attachments/materialize` 使用 Peer JWT 鉴权，并发布 `extensions.telegram.attachment.materialize.v1` Peer HTTP capability。兼容 generic invoke `materialize_attachment`；两者接受严格参数 `{"block":123}` 并返回原生内容 Block。它验证附件唯一 live Source 与 pinned bot、下载文件，再锁定 metadata 重新校验访问上下文，原子保存 Storage bytes、内容 Block 与唯一 `content` 关系。重复请求返回已保存内容。Storage 顺序为 Source 的 `storage`、`core.source` 部署配置的 `default_storage`、默认 PostgreSQL 二进制存储 `-4`。只读 Storage 会拒绝物化。Resolver 依次依据检测 MIME、声明 MIME、文件名和附件类型选择。

`download_attachments=true` 在 metadata 提交之后调用同一物化操作。下载失败保留 metadata 和游标，Job 报告 `partial`，用户可稍后重试。成功保存发送 👍；`/start`、不支持的内容、部分下载失败和主写入失败使用简短 bot 回复。通知失败只写入至多 50 条安全诊断，不撤销资料。

实现遵循 [Telegram Bot API](https://core.telegram.org/bots/api)。HTTP 请求逐次限制 30 秒，拒绝重定向；JSON 响应上限 16 MiB，附件上限 64 MiB。生产 endpoint 为 `https://api.telegram.org/`；受信执行环境可用 `INKCRE_TELEGRAM_API_BASE_URL` 覆盖为 loopback fixture，Source 配置不暴露此项。宿主会清空子进程环境，验收可通过受信包装 executable 设置变量后执行真实插件。

本目录不新增自动化测试。使用 fmt、定向 clippy，以及显式 loopback Bot API → HTTP Source Job → scoped PostgreSQL 验收。
