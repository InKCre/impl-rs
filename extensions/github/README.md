# GitHub Rust 分发

`inkcre-github-extension` 实现 `inkcre/github` 的 `rust_stdio` 分发，直接连接共享 PostgreSQL，使用既有 Source、Job 和图协议。业务身份与 Python 分发共用：

| 能力 | ID |
| --- | --- |
| Stars Source | `extensions.github.stars.Source` |
| Account Resolver | `extensions.github.account.v1` |
| Repository Resolver | `extensions.github.repository.v1` |
| List Resolver | `extensions.github.list.v1` |

```sh
cargo build --locked -p inkcre-app -p inkcre-github-extension
target/debug/inkcre extension-manifest --name inkcre/github --version 0.1.0 \
  target/debug/inkcre-github-extension > target/debug/github.extension.json
```

将 manifest 和 executable 一起复制到受信目录，通过 `INKCRE_EXTENSION_MANIFESTS` 提供本机可安装分发，再选择精确版本并启用。宿主只向插件注入数据库连接；插件不创建 Peer，不执行 schema migration。具体分发格式见 [Rust SDK](../../vendor/ext-reg/runtimes/core-rs/README.md)。

创建 Source 使用配置 `{"github_token":"…"}`；令牌需要能读取当前账户的 Stars、Lists 及希望同步的仓库。令牌仅用于 GitHub Bearer 请求，不出现在图内容、错误或 Job 报告中。`POST /sources/{id}/collect` 接受空 JSON 对象，创建 `core.source.collect.v1` Job，由启用了该分发的 worker 执行；不支持 backfill。

GraphQL 查询沿用 GitHub 官方的 [User、UserList 与游标分页接口](https://docs.github.com/en/graphql/reference/users)。每次采集先取得完整的 Stars 和 Lists 快照，包括分页列表成员和只存在于 List 的仓库，再在一个数据库事务中更新图与 Source state。Repository owner 使用 Account Block；关系分别为 Source `collects` Account、Account `stars` Repository、Account `owns` List、List `contains` Repository，以及 Repository owner `owns` Repository。三种内容均可通过 Resolver 读取 canonical JSON、标签和文本。

身份以 `(resolver, node_id)` 查找，重复身份和损坏的既有 GitHub 内容会阻止写入。再次采集复用 Block；更新内容、清理重复和失效的受管关系，并保留已有 Block 与其他关系。Source state 绑定首次成功采集的 GitHub Account，换账户需要新建 Source。事务内重新校验 Source 配置，采集中修改配置会使本次写入失败。并发 Rust 采集使用事务级 advisory lock 串行更新 GitHub 身份，Source 行锁保护账户 state。

HTTP 请求逐次限制为 30 秒、16 MiB，拒绝重定向、GraphQL 错误和不前进的游标。生产 endpoint 默认为 `https://api.github.com/graphql`。`INKCRE_GITHUB_GRAPHQL_URL` 仅供受信执行环境覆盖 endpoint；它不是 Source 配置字段。宿主会清空子进程环境，手工端到端验收可使用本机受信包装 executable 设置 loopback endpoint，再执行真实插件。

本目录不新增自动化测试。开发时运行 `cargo fmt --check`、定向 `cargo clippy`，并使用临时 loopback GraphQL 服务与 scoped PostgreSQL 数据执行显式 HTTP 验收。Registry 上的公开 Rust artifact 仍由 Registry 发布流程单独管理。
