# Mail Rust 分发

`inkcre-mail-extension` 实现 `inkcre/mail` 的 `rust_stdio` 分发，直接连接共享 PostgreSQL。Source、Resolver、Mailbox UID locator 和 newsletter 图语法沿用 [impl-py 9f75cbb 的 Mail 合同](https://github.com/InKCre/impl-py/blob/9f75cbb/docs/30-unit-tdd/mail-extension.md)。

| 能力 | ID |
| --- | --- |
| Source | `extensions.mail.source.Source` |
| 邮件、邮箱、地址、标记、MIME 元数据 | `extensions.mail.email.v1`、`extensions.mail.mailbox.v1`、`extensions.mail.email_address.v1`、`extensions.mail.flag.v1`、`extensions.mail.mime_part.v1` |
| 刊物、newsletter HTML、newsletter text | `extensions.mail.mailing_list.v1`、`extensions.mail.newsletter_html.v1`、`extensions.mail.newsletter_text.v1` |
| 延迟附件的 Peer inbound | `extensions.mail.mime_part.materialize.v1`：`POST /mail/mime-parts/materialize` |

在仓库根目录构建；app 和插件应使用同一 SDK 版本生成 manifest：

```sh
cargo build --locked -p inkcre-mail-extension
cargo build --locked -p inkcre-app
target/debug/inkcre extension-manifest --name inkcre/mail --version 0.1.0 \
  target/debug/inkcre-mail-extension > target/debug/mail.extension.json
```

通过 `INKCRE_EXTENSION_MANIFESTS` 提供本机受信 manifest，选择精确版本并启用。宿主向插件授予 native 数据库连接；插件不创建 Peer，不运行迁移。Linux 的 native TLS 使用系统 OpenSSL，macOS 使用系统 TLS。标准进程变量 `SSL_CERT_FILE` 可追加 PEM CA bundle；它属于受信 executable 的部署配置，不是 Source 的认证放宽选项。证书链及 hostname 验证始终启用。宿主会清空子进程环境，自定义 CA 可由受信启动 wrapper 显式提供。

Source 配置示例：

```json
{
  "protocol": "imap",
  "parameters": {
    "host": "imap.example.org",
    "port": 993,
    "security": "tls",
    "username": "reader@example.org",
    "password": "application-password"
  },
  "ordinary_mark_as_seen": true,
  "backfill_mark_as_seen": false,
  "synchronize_deletions": false
}
```

支持 `tls`、`starttls`、`plain`。默认排除 `\Drafts`、`\Junk`、`\Trash`，Source 可设置 `excluded_mailboxes.names/special_uses`。省略时，首次使用会把扩展的 `default_excluded_mailboxes` 写入 Source 配置。Source state 只保存不含密码的 access binding 与各 Mailbox Block 的 `uid_validity/last_uid/highest_modseq`。修改 host、port、security 或 username 需要新建 Source；可轮换密码。

普通 collect 从 Source 创建时间开始，按远端 INTERNALDATE 做精确过滤，并把 UIDNEXT frontier 纳入初始进度。显式 backfill 使用 `{"since":"2020-01-01","before":"2021-01-01"}` 的半开区间，不推进普通 cursor，默认不设 `\Seen`。普通采集在图持久化成功之后才设已读。支持 modified UTF-7 邮箱名、OBJECTID 的 Mailbox rename 连续性和 source-scoped EMAILID；CONDSTORE 同步 flag 变化，QRESYNC 提供 VANISHED locator。只有 `synchronize_deletions=true` 才删除消失的 occurrence 与该 Mailbox 的 tags。UIDVALIDITY 更换会清除失效 locator，保留已采集 Email 及内容。

身份依次匹配精确 occurrence、Source 内 EMAILID、保守的 Message-ID 候选。附件邮件不会仅凭 Message-ID 合并两个仍有效的远端副本。正文、MIME 元数据、参与者及 occurrence 保持分离；参与者地址复用全局 Block，flag vocabulary 属于 Mailbox，缺失的 reply/reference 目标使用可补全 Email stub。HTML 的 cid/content-location 引用连接对应 MIME 元数据。

采集只请求 `BODY.PEEK[HEADER]`、BODYSTRUCTURE 和正文 part；附件与内嵌图片保留元数据。显式读取 MIME part，或提交 `POST /mail/mime-parts/materialize {"block":123}`，才沿 MIME → Email → Mailbox → Source 派生 locator，核对实时 UIDVALIDITY 并下载对应 part。网络请求在事务外运行，保存前重新校验来源、配置和元数据。已有 `content` child 会复用。目标 Storage 依次取 Source.storage、`core.source.default_storage`、默认 `-4`；不会静默替换作者选定的 Storage。通过 `Core::knowledge()` 使用内建 Storage 或宿主提供的跨扩展 Storage fallback。

有效 List-ID，或 Beehiiv 的 `X-Beehiiv-Type` 与刊物 URL，产生 MailingList `publishes` Email。newsletter body 的 authored content 保留原文，读取时才清除隐藏预览、跟踪像素、明确的订阅操作和已知 analytics 参数；签名链接保留原始参数。阅读投影在 multipart/alternative 中优先选择 HTML，Email lexical 投影只包含主题，正文分别索引。读取不加载外部图片，也不执行退订。

连接超时15秒，socket 读写超时30秒。framing 逐条限制 line 为64KiB、literal 为64MiB、response 为65MiB；结构深度和 part 数有界。IMAP grammar 与 MIME/header/transfer decoding 分别由 `imap-proto`、`mailparse` 负责。独立 framing 不依赖解析器认识所有可选扩展，连接或 framing 错误会使会话失效，不会继续消费错误 tagged completion。错误和 Job diagnostics 使用固定消息，避免传播密码或服务器原始响应。

本目录没有自动化测试套件。静态检查与专用原生 PostgreSQL 验收库上的显式旅程：

```sh
cargo fmt --manifest-path extensions/mail/Cargo.toml --check
cargo clippy --manifest-path extensions/mail/Cargo.toml --all-targets --locked -- -D warnings
python3 extensions/mail/acceptance.py --config-json /私密目录/native-peer.json \
  --app target/debug/inkcre --extension target/debug/inkcre-mail-extension
```

验收脚本启动 loopback IMAP、TLS 和 STARTTLS 服务及真正 app Worker，生成一次性 CA，检查 horizon/frontier、backfill、flags、VANISHED、epoch 切换、图关系、newsletter、附件原始字节与绑定更换拒绝，并清理本次创建的 Source、Job、图与 PostgreSQL blob。需要本机 `openssl`。它只在明确执行时运行。

backfill 的跨字段日期次序会在 Worker 运行时校验，宿主目前只按 JSON Schema 做 admission。Mail 的 parent/reference 展开完整 SolvedEmail，遇到访问栈内的环时以 canonical Email root 终止；单次投影最多512个节点、16层，超限明确失败。SDK 0.1.3 的 typed Resolver wire 保留嵌套 MIME 内容中的 bytes，旧 JSON 接口仍按 JSON 投影。
