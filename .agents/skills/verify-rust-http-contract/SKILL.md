---
name: verify-rust-http-contract
description: 验证 InKCre Rust Peer 的实时 OpenAPI、inkcre-cli --schema 与真实 HTTP 持久化是否一致。修改公开请求类型、路由或 CLI Schema 投影时使用。
---

# 验证 Rust HTTP 契约

用当前候选构建的 Peer 和 `inkcre-cli`。从 [贡献指南](../../../CONTRIBUTING.md#独占开发数据库) 初始化当前专用工作树的 PostgreSQL。执行 `agent-build cargo build --locked -p inkcre-app -p inkcre-cli`。需要验收 Worker、Cron 调度或扩展冷恢复时，运行[已有端到端入口](../../../docs/40-deployment/runtime.md#显式端到端验收)。

## 启动和诊断

在仓库根运行以下 Bash 命令。若 18761 已被占用，换用一个空闲的 loopback 端口。保留 `$proof` 下的非敏感证据。

```bash
set -euo pipefail
umask 077
proof="$(mktemp -d "${TMPDIR:-/tmp}/inkcre-http-proof.XXXXXX")"
address=127.0.0.1:18761
python3 scripts/dev_database.py exec -- target/debug/inkcre db-check |
  jq -e '.environment == "development"'
python3 scripts/dev_database.py exec -- target/debug/inkcre serve \
  --bind "$address" --state-dir "$proof/peer" --no-worker --no-cron >"$proof/peer.log" 2>&1 &
peer_pid=$!
trap 'kill "$peer_pid" 2>/dev/null || true; wait "$peer_pid" 2>/dev/null || true; rm -f "$proof/connections.json"' EXIT
cli=(target/debug/inkcre-cli --json --connections-file "$proof/connections.json")
python3 scripts/dev_database.py exec -- python3 -c \
  'import json, os, sys; print(json.dumps({"base_url": sys.argv[1], "jwt_secret": os.environ["JWT_SECRET"]}))' \
  "http://$address" | "${cli[@]}" connection set probe --input -
```

等待 Peer 监听后，确认 `lsof -nP -a -p "$peer_pid" -iTCP:18761 -sTCP:LISTEN` 显示刚启动的进程。执行 `"${cli[@]}" connection check`，就绪应为真，且 `authenticated_read` 应返回当前 Peer 记录。需要定位失败时查看私密的 `peer.log`。

## 用真实 CLI 执行一次变更

```bash
"${cli[@]}" graph submit --input-json \
  '{"blocks":[{"id":-1,"resolver":"core.text.v1","content":"before"}]}' >"$proof/created.json"
id="$(jq -er '.blocks[0].id' "$proof/created.json")"
"${cli[@]}" update "block:$id" --schema >"$proof/patch-schema.json"
jq -e '((.required // []) | length == 0) and
  .properties.resolver.type == "string" and .additionalProperties == false' "$proof/patch-schema.json"
"${cli[@]}" update "block:$id" --input-json '{"content":"after","storage":null}' >"$proof/updated.json"
"${cli[@]}" get "block:$id" >"$proof/read-back.json"
jq -e '.[0] | .type == "block" and .resolver == "core.text.v1" and .content == "after"' "$proof/read-back.json"
```

修改其他资源时，先保存实时 `GET /openapi.json` 的相关操作，再运行对应 CLI 的 `--schema` 和实际请求。选一个最能区分错误实现的输入，对照接受或拒绝的结果，再从独立读取确认持久化。PATCH 优先核对省略字段是否保留原值、显式 `null` 是否只在允许处生效，以及派生 Schema 是否带入创建默认值。Source 和 Cron 的动态参数另由对应的类型目录验证。影响兼容性时，由维护者决定是否保留旧写法。

退出当前 Bash 运行段会停止本次 Peer 并删掉含密钥的连接文件。按贡献指南销毁本次新建的专属数据库，保留筛选后的 Schema、CLI 输出和候选 SHA。分享前检查证据中没有连接文件、口令、JWT 或业务数据。
