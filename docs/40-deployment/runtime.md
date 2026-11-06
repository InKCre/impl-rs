# 运行与验收

配置、启动或验收 Rust Peer 时阅读本文。接口参数见 [HTTP API](../30-unit-tdd/http-api.md)，执行语义见 [Job 与 Cron](../30-unit-tdd/job-runtime.md)，扩展协议由 [独立 SDK](../../vendor/ext-reg/runtimes/core-rs/README.md) 维护。

## 构建与启动

```sh
git submodule update --init --recursive
cargo build --locked --workspace
cp .env.example .env
```

填入 `.env` 的 native `DATABASE_URL` 和至少 32 字节的 `JWT_SECRET`。dotenv 只作为数据解析。运行 `target/debug/inkcre db-check` 完成只读协议准入，再按所需执行范围启动 `target/debug/inkcre serve`。生产构建使用 `cargo build --release --locked -p inkcre-app`，HTTP 应用是 `target/release/inkcre` 单一可执行文件；插件是另行交付的 executable。

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
| `INKCRE_NO_WORKER` | 默认 false；true 时不启动本机 Job worker，保留提交和协议操作入口 |
| `INKCRE_NO_WORKER_DISCOVERY` | 默认 false；true 时不扫描共享 pending 队列，只处理具体 Job ID 的本机尽力提示 |
| `INKCRE_WORKER_CONCURRENCY` | `4`；本机资格判断及执行的并发预算，范围 1–256 |
| `INKCRE_NO_CRON` | 默认 false；true 时不启动自动 Cron loop，保留按 ID 的显式 run/materialize 入口 |
| `INKCRE_TOKEN_LIFETIME_SECONDS` | `3600`；`token` 命令签发 JWT 的默认有效期 |
| `RUST_LOG` | 日志过滤；普通运行不应开启请求参数或 SQL 参数输出 |

完整 CLI 参数见 `inkcre --help`、`inkcre serve --help`。`--env-file <路径>` 显式选择 dotenv 文件。运行目录中的状态文件需要随实例保留；丢失默认 UUID 会产生新的 Peer 身份。

## 共享库中的执行范围

`serve` 默认启动 worker 和 Cron loop。worker 发现共享 pending 队列中的本机候选，原子领取后执行；Cron loop 每 10 秒检查共享模板，在当前到期分钟创建普通 Job。Cron 物化不要求本机具备该 Job 的 handler，所以仅关闭 worker 仍可能为其他 Peer 产生工作。

首次接入共享库、只需 HTTP 与插件读取时可运行：

```sh
target/debug/inkcre serve --no-worker --no-cron
```

这些开关只改变当前进程，不停用共享 Source、Cron 或其他 Peer。`--no-worker` 不阻止 HTTP 提交 Job；其他具备能力的 Peer 仍可领取。`--no-cron` 不阻止手动 `POST /crons/{id}/run` 或 `/materialize`。

`--no-worker-discovery` 保留 worker，但只接收本进程 HTTP 提交 Job、Source 命令、显式 Cron 操作等产生的具体 ID 提示。提示是有界、尽力而为的内存消息；进程重启前、其他 Peer 或自动 Cron loop 创建的 pending 工作不会被它补扫，提示丢失或本机暂不可执行时也没有持久重试。需要恢复这些工作时启用 pending discovery 或由其他合适的 Peer 发现。它不是按提交者隔离共享队列的权限机制。

HTTP 请求默认有 30 秒预算，业务请求体上限为 8 MiB。Source/Job 的长任务在提交返回后由 worker 按 Job timeout 执行，不受该 HTTP 请求时限或通用插件调用的 20 秒限制；具体 Resolver、Storage 与通用 invoke 请求仍有各自的调用边界。

## 插件生命周期

启动配置决定可信 manifest catalog。HTTP 只能按 catalog 中的名称和精确版本安装，不能提交任意可执行路径。manifest 包含 `rust_stdio`、`core-rs` SDK 要求、协议版本、相对 executable 路径及 SHA-256。安装复制本地产物、校验摘要并保存当前 Peer 选择，不自动启用。

enable 保存意图后启动插件，initialize 获得该精确分发的 config schema 和 typed capability descriptor，start 完成后注册本机 Resolver、Storage、Source 或 Job adapter，并复核选择。共享 catalog 中有类型不代表当前 Peer 已加载 handler。配置更新使用当前 Peer 精确 schema，不合并其他分发 schema；运行实例通过停用再启用读取新配置。disable 撤回 adapter、取消在途调用并停止实际子进程；数据库失败时本机停用状态保留，通过 `locally_disabled` 区分尚未同步的共享 enabled。周期 reconcile 处理其他 Peer 修改的启用、移除或版本选择。

原生 executable 是可信代码，进程隔离承担生命周期管理。宿主以 `INKCRE_DATABASE_URL` 向受信插件提供 native 数据库能力；凭据不保存到 manifest 或共享扩展 config。SDK 每帧上限为 1 MiB。

[`inkcre/rss`](../../extensions/rss/README.md) 使用同一 Extension Name、Source 类型与 canonical 图契约提供 RSS/Atom 采集、正文提取、附件保存和三个 Resolver。构建、manifest 坐标与配置见扩展 README；backfill 明确不支持。它使用严格 XML 解析，拒绝损坏或不能对齐条目的文档，不保证 Python feedparser 的容错恢复行为或提取文本逐字相同。游标范围及可恢复投影见 [RSS 设计](../30-unit-tdd/rss-extension.md)。

SIGTERM/ctrl-c 使 app 停止接收新请求、停止 Cron 生产和 worker 新领取，收尾执行后再停止插件、清理本机 lease 和连接池。退出保留已保存的 enabled，重启只恢复本地已有的精确选择，不追随 latest。插件 abort/timeout 会回收实际子进程；仍启用的精确分发可自动恢复，但原 Job 不重放。调用超时或取消前业务副作用可能已经发生，调用者不能自动重试有副作用的请求。

## 遗留 running Job 的维护

进程崩溃、清理超预算或终态写入失败后，Job 可能持续 running，关联 Cron 也会因 outstanding Job 而停止叠加发生。Rust worker 不在日常发现中自动关闭全局过期行，重启不自动重试这些命令。

维护时先确认实际执行器和子进程已经停止，读取最新持久 checkpoint，再通过 `job_finish` 或 `POST /jobs/{id}/finish` 关闭已知单条 Job，并保留其最新 state。SDK 的 `jobs_expire_overdue` 是部署级显式维护入口，执行前须确认所有可能受影响的执行器已经退出；它不取消 handler，也不回滚副作用。当前没有对应的全局 HTTP sweeper 或维护 CLI。

共享协议缺少执行器 owner 字段，超时与 Peer lease 失效都不能证明实际进程已经停止。当前 Python scanner 仍可能自动关闭全局过期 Job；混合运行时，它可能在 Rust 取消清理完成前写入终态。该部署级限制和恢复要求见 [Job 运行时](../30-unit-tdd/job-runtime.md#显式维护与混合-peer-边界)。

## 显式端到端验收

关键运行时旅程需要**空的、独占且可丢弃的 development 数据库**。先通过 impl-py 的数据库生命周期工具准备并准入 development 环境，再构建 workspace：

```sh
cargo build --locked --workspace
python3 scripts/accept_runtime.py --config-json /私密目录/development.json
```

私密 JSON 仅需 `DATABASE_URL/JWT_SECRET`；两个脚本也可从进程环境读取这两项，配置不可作为 shell 脚本 source。`accept_runtime.py` 检查数据库 admission 的 environment 必须为 `development`，并要求 Block、Source、Job、Cron 为空。不能指向已有资料的 xiaoland 自部署、共享 preview 或生产库，也不能只改配置名来冒充独占开发环境。

该脚本在 loopback 启动临时 HTTP Peer 和 RSS/Atom feed server，实际走 typed draft/事务提交、Storage hydration、RSS 正文与附件、条件请求、Atom、运行中 abort、插件精确恢复及自动 Cron 物化。它依赖独占空库前提枚举并清理验收图、Source、Cron 与 Job；不是任意共享库的精确清理工具。它不初始化或重建 schema，不创建角色。失败日志为 `.runtime/acceptance/last-runtime-failure.log`。

基础协议与文本示例插件仍可单独验收：

```sh
python3 scripts/acceptance.py --config-json /私密目录/native.json \
  --extension-binary target/debug/inkcre-note-extension
```

基础脚本默认关闭 worker 与 Cron，按本次精确 ID 清理图、blob、手工 Job 和临时 Peer 参与，观察认证、图导航、PostgreSQL bytes、插件保存和重启。共享 Extension 身份按正常卸载语义保留。它不覆盖自动 worker、Source 或 Cron；其失败日志为 `.runtime/acceptance/last-failure.log`。

此阶段没有自动化测试套件。`bash scripts/check.sh` 执行格式、编译、Clippy 和 rustdoc。静态检查通过不能代替真实数据库与插件验收。

知识读取的语义和事务边界见 [知识运行时](../30-unit-tdd/knowledge-runtime.md)。Rust Registry 远端发布/下载、AI/Organization/Agent/Sink、检索与其他第一方扩展分发尚未包含。当前 `/api/v1` 不能直接替换依赖 Python 完整 API 的消费者。
