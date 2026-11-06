# RSS / Atom Rust 分发

`inkcre-rss-extension` 是既有 `inkcre/rss` 扩展的 `rust_stdio` 分发，复用独立 extension SDK 和 `inkcre-core`。宿主动态安装并启动独立进程后，扩展注册 RSS、Atom Source 与三个 RSS Resolver；HTTP app 不内置 RSS 采集代码。业务身份、配置、JSON 内容、图关系与 Python 分发共用。

```sh
cargo build --locked -p inkcre-rss-extension
```

可执行文件为 `target/debug/inkcre-rss-extension`。把 executable 复制到受信目录，依据 [SDK manifest 格式](../../vendor/ext-reg/runtimes/core-rs/README.md) 填写以下坐标与实际 SHA-256：

```json
{
  "schema_version": 1,
  "name": "inkcre/rss",
  "version": "0.1.0",
  "distribution": "rust_stdio",
  "host_sdk": "core-rs",
  "host_sdk_version": "^0.1.0",
  "protocol_version": 1,
  "executable": "inkcre-rss-extension",
  "sha256": "替换为该可执行文件的64位小写SHA256"
}
```

将 manifest 加入宿主的 `INKCRE_EXTENSION_MANIFESTS`，通过扩展接口安装精确版本并启用。插件通过宿主传入的 `INKCRE_DATABASE_URL` 直连既有 PostgreSQL；它不创建自己的 Peer，也不修改 schema。

创建 Source 时，RSS 使用 `extensions.rss.rss.Source`，Atom 使用 `extensions.rss.atom.Source`。最小配置为 `{"feed_url":"https://example.org/feed.xml"}`。默认抓取可提取的文章正文，不下载附件；可设置 `fetch_full_text`、`download_enclosures`、请求超时和三类字节上限。提交 `core.source.collect.v1` Job，参数为 `{"source":123,"config":{}}`，由有对应本机能力的 worker 领取执行。RSS 不支持 backfill。

单次命令可以覆盖 `fetch_full_text`、`download_enclosures`、`target_storage_id`。附件存储依次选择本次命令的显式值、Source 的 `storage`、兼容旧配置的 `target_storage_id`（默认 PostgreSQL 二进制存储 `-4`）。保存 Source 配置不执行采集；重复提交会重新读取共享的当前配置与游标。

持久化、条件请求、恢复与读取语义见 [RSS 内部设计](../../docs/30-unit-tdd/rss-extension.md)。Rust 公共 Registry 上传与下载尚未提供；此目录交付本机可信 executable 分发。
