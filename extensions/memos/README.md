# Memos Rust 分发

本包以 `inkcre/memos` 身份实现 `impl-py` 基线 `9f75cbb` 的 memo-family 共享图契约与
Memos `0.29.1` 产品访问子集。`inkcre-core` 直接连接已准入 PostgreSQL，HTTP ingress
由所在 Peer 按精确 SDK 描述挂载，持久内容与插件进程分别管理。

`src/schema.rs` 拥有 canonical DTO，`src/graph.rs` 拥有图关系、事务和 attachment
ownership，`src/wire.rs` 映射产品字段与 updateMask，`src/ingress.rs` 拥有协议路由、
PAT、分页和 HTTP 结果；`src/main.rs` 只接 SDK 生命周期和 Resolver。

## 启用与连接

宿主通过可信本地 manifest 或 Registry 精确版本安装 executable，然后选择
`rust_stdio` 并启用。SDK 接受 `INKCRE_DATABASE_URL` 作为宿主明确授予的原生数据库
能力。扩展配置为完整对象：

```json
{ "personal_access_token": "memos_pat_替换为32位ASCII字母数字" }
```

token 可为 null；配置不可含未知字段。每次产品请求从共享 Extension.config 读取
当前 token 并验证格式、固定长度和 bytes，撤销/替换不需要重启。`/memos/connection`
只接受 Peer JWT，读取当前 Peer 的 `http_public_base_url` 并返回 `server_url`；缺少
地址时返回409。产品探测 `/memos/api/v1/instance/profile` 公开，其余产品与文件
入口使用 Memos PAT。Host 按声明保留这三个认证边界。

已实现的0.29.1访问表面：

- `GET /memos/api/v1/instance/profile`、`auth/me`、`users/inkcre/settings/GENERAL`。
- `POST/GET /memos/api/v1/memos`；`PATCH/DELETE /memos/api/v1/memos/{id}`。
- `POST/GET /memos/api/v1/memos/{id}/comments`。
- `POST/GET /memos/api/v1/attachments`；`DELETE /memos/api/v1/attachments/{id}`。
- `GET /memos/file/attachments/{id}/{filename}`。

产品查询要求 `filter=creator == "users/inkcre"`，分页按创建时间及 Block ID 降序，
明确区分 NORMAL/ARCHIVED。opaque page token 绑定查询状态、generation与cursor；
评论token绑定父Memo。updateMask采用明确字段选择、缺省重置与显式null拒绝。
评论继承父Memo的visibility，普通memo与评论分别分页。

## 共享图与字节

Memo root使用 `extensions.memos.memo.v1`，inline JSON包含body、可空UTC时间、
archived、visibility、pinned。Attachment metadata使用
`extensions.memos.attachment.v2`，inline JSON包含filename/media_type/size/created_at。
Memo到attachment的关系为连续、从零开始的 `attachment:{position}`；metadata到
语义内容块为 `content`，评论到父Memo为 `parent`，普通引用为 `reference`。

二进制内容使用既有 `postgresql_binary` Storage `-4`，pointer遵守原生Storage协议。
metadata、语义块、blob与关系在一个事务内提交；附件列表更新按稳定ID顺序锁定，
拒绝重复身份或跨Memo争用。按media type选择已注册Resolver，未知类型使用
`core.file.v1`。文件最多32 MiB decoded bytes；产品JSON envelope最多45 MiB，SDK
HTTP帧最多64 MiB。下载要求精确filename，返回原始bytes。

删除Memo先提交root删除，再尽力清理独占评论与attachment。普通reference不赋予
删除权，其他content owner引用的语义块/blob保留。替换附件列表会删除已移出的
独占component；共享内容的实际存储机制继续由Storage handler负责。

Resolver支持memo和attachment的solve/text/label。配置启用与解码能力分别管理：
`passive_knowledge` 允许已安装但停用的分发只连接Core来读取旧资料；它不挂载HTTP、
不取得协议执行资格。停止仅关闭本次数据库连接，保留共享资料。

## 构建与显式验收

在仓库根目录检查和构建：

```bash
cargo fmt -p inkcre-memos-extension --check
cargo clippy -p inkcre-memos-extension --all-targets --locked -- -D warnings
cargo build --locked -p inkcre-app -p inkcre-memos-extension
```

关键验收需要独占、可丢弃的 PostgreSQL development 环境，以及 Python 3 和 psycopg 3。
脚本创建随机 Peer 和验收图，最后按记录的 ID 清理并恢复原先的 Memos 配置。私密 JSON
包含 `DATABASE_URL` 和 `JWT_SECRET`。

```bash
uv run --no-project --with 'psycopg[binary]>=3,<4' python \
  extensions/memos/scripts/accept_memos.py \
  --config-json /absolute/path/native-peer.private.json \
  --binary /absolute/path/inkcre \
  --extension-binary target/debug/inkcre-memos-extension
```

脚本显式验证HTTP→SDK子进程→原生PostgreSQL的PAT/JWT、connection、canonical图、
UTC、Resolver文字、updateMask、keyset分页、评论、32 MiB bytes和摘要、独占清理、
共享内容保留、实时撤销及停用后的 HTTP 撤回与资料读取。
