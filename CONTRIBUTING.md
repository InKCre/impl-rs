# 开发与贡献

先阅读 `AGENTS.md` 和共享设计，再修改实现。`core` 拥有可复用的业务与数据库访问能力；`app` 负责配置、HTTP 传输、认证和进程生命周期。HTTP handler 不直接持有另一套业务 SQL。

工具链固定为 Rust 1.93.0。先执行 `git submodule update --init --recursive`；`docs/_shared` 是共享契约，`vendor/ext-reg` 提供独立的 Rust 扩展 SDK。修改 SDK 时进入子模块的功能分支，先交付其源码，再更新本仓库子模块指针。

本初始化阶段不添加自动化测试。完整静态检查执行 `bash scripts/check.sh`，包括格式、编译、Clippy 和 rustdoc，也检查独立 SDK。CI 调用同一入口。关键端到端验收显式执行，不能对共享部署进行破坏性初始化。依赖变化后使用 `cargo generate-lockfile` 更新锁文件，确认后保留，不在正常检查时隐式更新。

功能开发使用 `feat/` 或 `hotfix/` 分支。仅在用户明确要求后提交，提交格式为 `feat|fix|ref|docs|chore(scope): 中文摘要`，正文按需列出关键变化。PR、issue 和文档使用中文。组织 [贡献指南](https://github.com/InKCre/.github/blob/main/CONTRIBUTING.md) 和 [验证政策](https://github.com/InKCre/.github/blob/main/TESTING.md) 继续适用；本阶段用户明确要求不编写自动化测试。

扩展 SDK 的通用契约在 `ext-reg` 维护。共享 Hub 文档通过 Hub 仓库修改并更新子模块指针，不直接编辑 `docs/_shared`。

`tasks/` 保存本机暂态计划与验收证据，默认忽略；交付所需的长期边界进入上述设计、运行文档和可执行配置。
