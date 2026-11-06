# InKCre Rust Implementation

先用英文思考；聊天、文档、PR 和 issue 使用自然中文。实现、修复和重构请求授权范围内必要源码修改。只有用户明确要求才能提交；保留其他人的改动。

## 定位与权威

`impl-rs` 是共享同一信息库的独立 Peer，包含可嵌入的 `core` SDK/lib 和单一可执行文件 HTTP `app`。数据库是共享持久状态的权威。`impl-py` 拥有 schema 和 migration；Rust 直接使用 PostgreSQL 的已准入 `inkcre` 协议，不经过 PostgREST，也不另建私有业务表。

扩展为当前 Peer 增加执行能力。Registry Release、部署 Extension 身份、Peer 精确分发选择与启用、进程 running 分别管理，不因 Rust 实现而产生新的中心节点。扩展 SDK 和语言无关分发契约由 `ext-reg` 拥有。

## 知识入口

- 共享产品与跨单元契约：`docs/_shared/10-prd/`、`docs/_shared/20-product-tdd/`（只读 Hub 子模块）。
- Rust 内部边界：`docs/30-unit-tdd/architecture.md`。
- 运行、配置、验收：`docs/40-deployment/runtime.md`。
- 开发命令与贡献约定：`CONTRIBUTING.md`、Cargo workspace、CI。
- 本机任务与未完成工作：`tasks/`（默认忽略，不公开提交）；任务记录不能代替长期设计文档。

## 开发约束

- 使用 Cargo workspace，锁定工具链并提交 `Cargo.lock`；遵循 rustfmt 和 Clippy。
- 当前初始化阶段不编写自动化测试。使用编译、格式、Clippy、文档检查和显式执行的关键端到端验收脚本。
- SQL 必须遵守已准入的数据库协议与权限边界；不以超级用户绕过权限，不执行 migration，不创建或删除共享数据库、角色或 schema。
- 配置文件是数据，不可作为 shell 脚本 source。日志、错误和任务证据不包含口令、JWT 或完整数据库连接串。
- 扩展加载不能依赖 Rust 的不稳定动态库 ABI；与 SDK 的兼容性必须有明确协议依据。
- 禁止安装、引入、调用或使用 LiteLLM。
- 用户决定 impl-rs 许可证留待发布前确定；本仓当前不声明许可，不自行添加。依赖和 ext-reg SDK 的既有许可证继续保留。
- 非平凡任务使用 svc-workflow、svc-specs、svc-task-packet、svc-verification。合适的独立工作按照 svc-agent-collaboration 委派。
- 提交与 GitHub 流程遵循 [组织治理](https://github.com/InKCre/.github/blob/main/GOVERNANCE.md)，结合当前用户对提交、分支与发布的明确授权执行。

若存在 `AGENTS.local.md`，在本文件后读取。
