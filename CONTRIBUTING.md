# 开发与贡献

先阅读 `AGENTS.md` 和共享设计，再修改实现。`core` 拥有可复用的业务与数据库访问能力；`app` 负责配置、HTTP 传输、认证和进程生命周期。HTTP handler 不直接持有另一套业务 SQL。

工具链固定为 Rust 1.93.0。先执行 `git submodule update --init --recursive`；`docs/_shared` 是共享契约，`vendor/ext-reg` 提供独立的 Rust 扩展 SDK。修改 SDK 时进入子模块的功能分支，先交付其源码，再更新本仓库子模块指针。

本初始化阶段不添加自动化测试。完整静态检查执行 `bash scripts/check.sh`，包括格式、编译、Clippy 和 rustdoc，也检查独立 SDK。CI 调用同一入口。关键端到端验收显式执行，不能对共享部署进行破坏性初始化。依赖变化后使用 `cargo generate-lockfile` 更新锁文件，确认后保留，不在正常检查时隐式更新。

初始化阶段用户允许直接提交并推送 `main`；需要隔离时使用 `feat/` 或 `hotfix/` 分支。提交只包含当前任务改动，格式为 `feat|fix|ref|docs|chore(scope): 中文摘要`，正文按需列出关键变化。PR、issue 和文档使用中文。组织 [贡献指南](https://github.com/InKCre/.github/blob/main/CONTRIBUTING.md) 和 [验证政策](https://github.com/InKCre/.github/blob/main/TESTING.md) 继续适用；本阶段用户明确要求不编写自动化测试。

扩展 SDK 的通用契约在 `ext-reg` 维护。共享 Hub 文档通过 Hub 仓库修改并更新子模块指针，不直接编辑 `docs/_shared`。

`tasks/` 保存本机暂态计划与验收证据，默认忽略；交付所需的长期边界进入上述设计、运行文档和可执行配置。

## 独占开发数据库

本机准备 PostgreSQL 17 或更新版本及匹配的 pgvector，并准备 canonical impl-py checkout，在该 checkout 按其贡献说明用 PDM 安装依赖。Rust 的开发入口调用 Python 所有的数据库生命周期命令，schema 和 migration 随该 checkout 的 revision 确定。

```sh
python3 scripts/dev_database.py init --impl-py /路径/impl-py
cargo build --locked --workspace
python3 scripts/accept_runtime.py --config-json .runtime/dev-database/native-peer.private.json
```

默认使用 PATH 中的 PostgreSQL 工具和 impl-py 的 `pdm run python`。有多个 PostgreSQL 版本时通过 `--postgres-bin /路径/postgresql/bin` 选择带匹配 pgvector 的版本。复用已安装依赖的 Python 虚拟环境时可传 `--python /路径/venv/bin/python`。

该入口为当前 Rust checkout 创建独占 cluster，绑定随机 loopback 端口，初始化 development profile，并保存 native `inkcre_core` 连接配置。应用直接使用 PostgreSQL。新库不启动 Python worker 或 Cron。密码、JWT secret 和诊断日志只保存在权限受限的 `.runtime/dev-database/`，成功输出包含配置文件路径和 Python source revision。

```sh
python3 scripts/dev_database.py status
python3 scripts/dev_database.py stop
python3 scripts/dev_database.py start
python3 scripts/dev_database.py destroy
```

stop/start 保留资料，destroy 停止并删除当前 checkout 所属的整个开发 cluster。验收期间让所有使用者共用同一生命周期，完成验收后再 destroy。初始化失败会停止自建 PostgreSQL并保留私密诊断，修复依赖后执行 destroy/init 重试。运行其他共享环境的配置与数据独立管理。

开发时用 `exec` 将 native 连接与签名参数传给子进程，凭据无需进入命令行或 shell 历史。

```sh
python3 scripts/dev_database.py exec -- target/debug/inkcre db-check
python3 scripts/dev_database.py exec -- target/debug/inkcre serve
```
