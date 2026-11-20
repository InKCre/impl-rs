# Local Files Rust 分发

`inkcre/local-files` 0.3.0 解释既有本地文件图，提供 `extensions.local_files.file.v1` 和 `.v2` 两个 Resolver。metadata 中的 Peer 文件 URL 是来源身份。正文从共享图的 `body` 关系读取。

v1 要求正文为 inline `core.text.v1`。v2 允许 inline text、HTML 和 Markdown，并复用相应核心 Resolver 的文本解释。正文缺失、重复关系、已声明 revision 却没有正文、错误 Resolver 或 storage-backed 正文都会报告图完整性错误。原件失效时仍返回最后保存的正文，并保留位置、提取失败和 DOCX 主文档范围提示。

```sh
cargo build --locked -p inkcre-local-files-extension -p inkcre-app
inkcre extension-manifest --name inkcre/local-files --version 0.3.0 \
  target/debug/inkcre-local-files-extension > target/debug/local-files.json
```

把 manifest 加入宿主的可信 catalog 后安装并启用。扩展使用宿主提供的 native PostgreSQL 连接读取共享正文。配置为空对象。
