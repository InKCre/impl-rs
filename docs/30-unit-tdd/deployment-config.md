# 部署配置

修改部署级配置时使用 `deployment_config` SDK。`ConfigRegistry` 拥有本机确切 schema ID 的规范化函数和可发现 JSON Schema。默认提供 Cron 时区、Source materialization 的默认 Storage 和 semantic retrieval 的默认 Profile。嵌入者可添加自定义 schema，重复 ID 明确拒绝。

读取和分页返回原始持久值，旧值或当前 Peer 不认识的 schema 仍可查看。写入前使用确切 schema 完整规范化，填入默认值。PUT 用冲突插入结果决定 201/200，PATCH 在行锁内读取、浅合并并重新校验完整值。失败回滚，保留已有配置。schema 变更通过 PUT 进行。

HTTP 暴露 `/configs` 和 `/config-schemas`，支持升序 exclusive cursor。PATCH 不认识的持久 schema 返回 409，PUT 不认识的新 schema 返回 422。Source 的默认 Storage 只作为共享配置保存，实际可写类型和 handler 可用性在 materialization 时判断。
