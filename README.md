# InKCre Rust

InKCre 的 Rust Peer 实现，提供可嵌入的 core SDK/lib 和单一可执行文件 HTTP app。Rust 与 Python、TypeScript 等实现共享同一信息库，直接连接 PostgreSQL。

开发前先阅读 [贡献约定](CONTRIBUTING.md)、[内部设计与能力边界](docs/30-unit-tdd/architecture.md) 和 [运行说明](docs/40-deployment/runtime.md)。

Rust 工具链固定为 1.93.0。共享产品契约与独立扩展 SDK 以子模块固定：

```sh
git submodule update --init --recursive
cargo build --locked -p inkcre-app
cp .env.example .env
```

填入 `.env` 的 `DATABASE_URL` 和 `JWT_SECRET` 后，执行 `target/debug/inkcre db-check` 查看只读准入结果，再用 `target/debug/inkcre serve` 启动。默认监听 `127.0.0.1:8000`，受保护接口使用 `Authorization: Bearer <JWT>`；`inkcre token` 可签发共享 Peer 凭据。

`serve` 默认启动 Job worker 和 Cron 调度，可能领取共享队列中本机能执行的命令，并为共享的到期 Cron 创建 Job。只接入 HTTP、暂不参与后台工作时使用 `inkcre serve --no-worker --no-cron`；开关的范围及恢复后果见 [运行说明](docs/40-deployment/runtime.md)。

`crates/core` 提供图数据、Resolver 与 Storage、词法与向量检索、AI、Agent、资料整理、Job worker、Source 和 Cron。`crates/app` 组合这些能力，提供 HTTP、OpenAPI 和 MCP 入口。独立 [`inkcre-cli`](crates/cli/README.md) 使用 HTTP 连接 Rust 或 Python Peer。

第一方扩展位于 [`extensions/`](extensions)。宿主从可信本地 manifest 或支持 `rust_stdio` 的 Registry 精确选择产物。部署、打包和分发发布见 [运行说明](docs/40-deployment/runtime.md)，开发扩展使用 [Rust 扩展 SDK](vendor/ext-reg/runtimes/core-rs/README.md)。

静态检查执行 `bash scripts/check.sh`。运行时验收可显式执行 `scripts/accept_runtime.py` 可验收知识入口、真实 RSS/Atom worker、取消恢复与 Cron，**仅允许空的独占 disposable development 数据库**，不能指向已有资料的共享部署；准备方式见运行说明。

数据库 schema 和 migrations 由 `impl-py` 维护。跨实现客户端使用根路径的 [HTTP API](docs/30-unit-tdd/http-api.md)，早期 Rust 客户端可继续使用 `/api/v1` 接口。

impl-rs 的许可证由项目所有者稍后确定，当前仓库尚未设置许可证。ext-reg Rust SDK 保留其自身 AGPL-3.0-only 许可证。
