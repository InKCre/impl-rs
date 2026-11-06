# 文本保存扩展示例

本例展示原生扩展同时使用独立 extension SDK 和 core 数据库 SDK。宿主启动插件后，`capture_text` 接受 `{"text":"内容"}` 并保存普通 `core.text.v1` Block；`get_block` 接受 `{"id":123}` 并读取持久 Block。关闭插件不会删除资料。

```sh
cargo build --locked -p inkcre-note-extension
```

可执行文件在 `target/debug/inkcre-note-extension`。正式使用时将文件复制到本机受信目录，并按 `vendor/ext-reg/runtimes/core-rs/README.md` 创建包含摘要的 manifest。显式运行 `scripts/acceptance.py --extension-binary target/debug/inkcre-note-extension` 会临时准备 manifest，并完整经过 HTTP 安装、调用、重启恢复和本次图数据清理。

插件只复用宿主 Peer，不创建额外 Peer。宿主通过专用环境变量 `INKCRE_DATABASE_URL` 提供 native 连接；凭据不进入共享 Extension config。原生插件是可信代码，进程隔离用于生命周期管理。
