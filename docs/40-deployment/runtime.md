# 运行与验收

配置、启动或验收 Rust Peer 时阅读本文。接口参数见 [HTTP API](../30-unit-tdd/http-api.md)，扩展协议由 `vendor/ext-reg/runtimes/core-rs/README.md` 维护。

## 构建与启动

```sh
git submodule update --init --recursive
cargo build --locked --workspace
cp .env.example .env
```

填入 `.env` 的 native `DATABASE_URL` 和至少 32 字节的 `JWT_SECRET`。dotenv 只作为数据解析。运行 `target/debug/inkcre db-check` 完成只读协议准入，再执行 `target/debug/inkcre serve`。生产构建使用 `cargo build --release --locked -p inkcre-app`，HTTP 应用是 `target/release/inkcre` 单一可执行文件；插件是另行交付的 executable。

数据库必须先由 impl-py 初始化到已准入协议。Rust 不运行 migration、不创建角色。native 登录角色是 `authenticated` 成员；HTTP JWT 独立校验共享 claims，数据库口令不能替代 HTTP Bearer token。`inkcre token` 使用配置的 secret 签发 token，默认有效期 3600 秒；`INKCRE_TOKEN_LIFETIME_SECONDS` 只改变签发默认值。

| 配置 | 默认及作用 |
| --- | --- |
| `DATABASE_URL` | 必填；直连 PostgreSQL 的 native 运行主体，保留部署要求的 TLS 参数 |
| `JWT_SECRET` | 必填；与同部署其他 HTTP Peer 使用相同签名 secret |
| `INKCRE_BIND` | `127.0.0.1:8000`；HTTP 监听地址 |
| `INKCRE_PEER_ID` | 可选；明确指定稳定 UUID |
| `INKCRE_STATE_DIR` | `.runtime`；默认 Peer UUID 与本机插件产物、停用状态 |
| `INKCRE_PEER_NAME` | `Rust Peer`；仅首次创建，不覆盖用户改名 |
| `INKCRE_PEER_LEASE_SECONDS` | `60`；数据库租约 TTL，范围 3–3600 秒 |
| `INKCRE_DATABASE_MAX_CONNECTIONS` | `10`；连接池上限，范围 1–100 |
| `INKCRE_REQUEST_TIMEOUT_SECONDS` | `30`；HTTP 请求超时，必须为正整数 |
| `INKCRE_EXTENSION_MANIFESTS` | 可选；用逗号分隔的可信本地 manifest 路径 |
| `RUST_LOG` | 日志过滤；普通运行不应开启请求参数或 SQL 参数输出 |

完整 CLI 参数见 `inkcre --help`、`inkcre serve --help`。`--env-file <路径>` 显式选择 dotenv 文件。运行目录中的状态文件需要随实例保留；丢失默认 UUID 会产生新的 Peer 身份。

## 插件生命周期

启动配置决定可信 manifest catalog。HTTP 只能按 catalog 中的名称和精确版本安装，不能提交任意可执行路径。manifest 包含 `rust_stdio`、`core-rs` SDK 要求、协议版本、相对 executable 路径及 SHA-256。安装复制本地产物、校验摘要并保存当前 Peer 选择，不自动启用。

enable 保存意图后启动插件，initialize 获得该精确分发的 config schema，start 完成后复核选择。配置更新使用当前 Peer 精确 schema，不合并其他分发 schema；运行实例通过停用再启用读取新配置。disable 停止本机实例并同步共享意图；数据库失败时本机停用状态保留，通过 `locally_disabled` 区分尚未同步的共享 enabled。周期 reconcile 处理其他 Peer 修改的启用、移除或版本选择。

SIGTERM/ctrl-c 使 app 停止接收新请求、收尾插件并清自己的 lease。退出保留已保存的 enabled，重启只恢复本地已有的精确选择，不追随 latest。插件调用超时后的副作用可能已经发生；调用者不能自动重试有副作用的请求。

## 显式端到端验收

构建 app 和文本扩展示例后，可对已有准入数据库执行：

```sh
python3 scripts/acceptance.py --extension-binary target/debug/inkcre-note-extension
```

脚本从环境读取 `DATABASE_URL/JWT_SECRET`，也可用 `--config-json <私密文件>` 读取包含这两个键的 JSON。用户授权的 xiaoland/core-py 自部署可以作为验收目标；秘密只在进程环境中传递，输出不包含连接串。脚本启动仅监听 loopback 的临时 HTTP Peer，观察认证、图数据、方向与路径、二进制存储、Job 状态，以及插件通过 core SDK 保存资料、app 重启后的恢复。

验收只清理本次创建的 Block、Relation、blob、Job、JobType 与临时 Peer 参与，不重建 schema 或修改既有 Source/Cron。共享 Extension 身份按正常卸载语义保留，不因删除一个 Peer 的参与而删除其 config/state。运行失败日志位于忽略目录 `.runtime/acceptance/last-failure.log`。

此阶段没有自动化测试套件。`bash scripts/check.sh` 执行格式、编译、Clippy 和 rustdoc。静态检查通过不能代替真实数据库与插件验收。

Rust Registry 远端发布/下载、AI/Organization 和既有 Python 第一方扩展移植尚未包含。当前 `/api/v1` 为 Rust 初始 API，不能直接替换依赖 Python 完整 API 的消费者。
