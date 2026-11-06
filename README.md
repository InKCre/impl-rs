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

`crates/core` 提供数据库与图数据 SDK，`crates/app` 提供 HTTP 入口和命令行。扩展通过 `rust_stdio` executable 动态加载，SDK 位于 `vendor/ext-reg/runtimes/core-rs`。静态检查执行 `bash scripts/check.sh`；当前阶段不包含自动化测试。

数据库 schema 与 migration 由 `impl-py` 维护；此仓库不运行迁移。Rust 的 `/api/v1` 是初始接口，不能当作 Python 全量业务能力的等价替换。

impl-rs 的许可证由项目所有者稍后确定，当前仓库尚未设置许可证。ext-reg Rust SDK 保留其自身 AGPL-3.0-only 许可证。
