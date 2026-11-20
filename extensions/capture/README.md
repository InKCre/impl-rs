# Capture Rust 扩展

`inkcre/capture` 0.2.0 解释共享信息库的随手收集图。其 canonical 业务契约对应 impl-py `9f75cbb` 的可选 Capture reader。

构建 `cargo build --locked -p inkcre-capture-extension`，可执行文件为 `target/debug/inkcre-capture-extension`。按运行文档制作可信 manifest，坐标为 `inkcre/capture`、`0.2.0`、`rust_stdio`、`core-rs`、协议 1，并填写 executable 的 SHA-256。启用后提供 `extensions.capture.capture.v1` 与 `extensions.capture.attachment.v1` 两个 typed Resolver。

Capture 根 JSON 保存 submission UUID、SHA-256 payload digest、带时区的采集时间、entry 和可选 origin。根的 `material:0`、`material:1` 等出边给出有序材料，`annotation` 出边给出可选附注。材料类型为 `core.text.v1` 或 Capture attachment，附注类型为 `core.text.v1`。attachment JSON 保存声明图片 MIME 和可选文件名，通过唯一 `content` 出边引用 `core.image.v1`。

Resolver 解算时检查材料序号连续、重复序号、缺失端点、类型以及附注唯一性。solved 保留 canonical JSON 字段，text 包括来源、材料和附注，label 使用既有随手收集和采集图片格式。读取独立于最初采集资料的 Peer，停用扩展保留持久资料。图片文字投影由 core image Resolver 提供。

native PostgreSQL 凭据通过受信宿主的 `INKCRE_DATABASE_URL` 传入。协议 stdout 仅承载 SDK 消息。数据入口与 receipts 属于执行采集的参与者，canonical Python reader 本身提供上述图解释能力。
