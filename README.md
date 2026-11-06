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

`crates/core` 已提供图数据、Resolver/Storage registry、typed read、事务化 graph draft、Job worker、Source 和 Cron；`crates/app` 组合这些能力。既有 [`inkcre/rss` 扩展的 Rust 分发](extensions/rss/README.md) 通过独立 `rust_stdio` executable 执行 RSS/Atom 采集、正文提取和附件保存，并以原有 canonical 图供 Resolver 读取。它采用严格 XML 解析，不复刻 Python feedparser 对损坏文档的容错。

实现边界分别见 [知识与 Storage](docs/30-unit-tdd/knowledge-runtime.md)、[Job 与 Cron](docs/30-unit-tdd/job-runtime.md)、[RSS Source](docs/30-unit-tdd/rss-extension.md) 和 [HTTP API](docs/30-unit-tdd/http-api.md)。扩展 SDK 位于 `vendor/ext-reg/runtimes/core-rs`，本机可信 manifest 管理精确 executable；Rust 公共 Registry 上传、下载尚未提供。

静态检查执行 `bash scripts/check.sh`；当前阶段不包含自动化测试。显式运行 `scripts/accept_runtime.py` 可验收知识入口、真实 RSS/Atom worker、取消恢复与 Cron，**仅允许空的独占 disposable development 数据库**，不能指向已有资料的共享部署；准备方式见运行说明。

数据库 schema 与 migration 由 `impl-py` 维护；此仓库不运行迁移。Rust 的 `/api/v1` 是初始接口，不能当作 Python 全量业务能力的等价替换。

impl-rs 的许可证由项目所有者稍后确定，当前仓库尚未设置许可证。ext-reg Rust SDK 保留其自身 AGPL-3.0-only 许可证。
