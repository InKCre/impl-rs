# RSS / Atom Rust 分发

`inkcre-rss-extension` 是既有 `inkcre/rss` 扩展的 `rust_stdio` 分发，复用独立 extension SDK 和 `inkcre-core`。宿主动态安装并启动独立进程后，扩展注册 RSS、Atom Source 与三个 RSS Resolver；HTTP app 不内置 RSS 采集代码。业务身份、配置、JSON 内容、图关系与 Python 分发共用。

```sh
cargo build --locked -p inkcre-app -p inkcre-rss-extension
target/debug/inkcre extension-manifest --name inkcre/rss --version 0.1.0 \
  target/debug/inkcre-rss-extension > target/debug/rss.extension.json
```

生成的 manifest 与 executable 位于同一目录。部署时将两者一起复制到受信目录。坐标和摘要规则见 [SDK manifest 格式](../../vendor/ext-reg/runtimes/core-rs/README.md)。

将 manifest 加入宿主的 `INKCRE_EXTENSION_MANIFESTS`，通过扩展接口安装精确版本并启用。插件通过宿主传入的 `INKCRE_DATABASE_URL` 直连既有 PostgreSQL；它不创建自己的 Peer，也不修改 schema。

创建 Source 时，RSS 使用 `extensions.rss.rss.Source`，Atom 使用 `extensions.rss.atom.Source`。最小配置为 `{"feed_url":"https://example.org/feed.xml"}`。默认抓取可提取的文章正文，不下载附件；可设置 `fetch_full_text`、`download_enclosures`、请求超时和三类字节上限。提交 `core.source.collect.v1` Job，参数为 `{"source":123,"config":{}}`，由有对应本机能力的 worker 领取执行。RSS 不支持 backfill。

单次命令可以覆盖 `fetch_full_text`、`download_enclosures`、`target_storage_id`。附件存储依次选择本次命令的显式值、Source 的 `storage`、兼容旧配置的 `target_storage_id`（默认 PostgreSQL 二进制存储 `-4`）。保存 Source 配置不执行采集；重复提交会重新读取共享的当前配置与游标。

启用扩展后，`POST /rss/enclosures/materialize` 接受 `{"enclosure_block_ids":[12,34],"target_storage_id":-4}`。它按输入顺序报告已有、已保存或失败的条目，单条失败不会丢弃其他条目的结果。未指定目标 Storage 时使用内建 PostgreSQL bytes。

持久化、条件请求及读取方式见 [RSS 内部设计](../../docs/30-unit-tdd/rss-extension.md)。可使用本机可信产物或 Registry 已发布的精确 Rust 分发安装，见 [运行说明](../../docs/40-deployment/runtime.md#插件生命周期)。
