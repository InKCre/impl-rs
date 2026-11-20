# Sink 与 Agent Query

创建应用入口或排查 Query 交付时阅读本文。Sink 类型保存在 `sink_types`，实例保存在 `sinks`。实例的 `enabled` 是一组 Peer ID。Rust 在行锁下更新这组 ID，防止两个 Peer 的并发操作覆盖彼此。删除要求所有 Peer 都已停用。

Agent Query Sink 使用 `core.agent-query.v1`。它的配置只包含 Agent ID。HTTP 请求在当前 Peer 启用该 Sink 时创建 `core.sink.agent-query.v1` Job，并返回 Job 地址。Worker 按当前启用状态、模型和本机工具检查领取资格。给 Agent 定义启用 `submit_query_result` 后，Agent 可以把答案与引用交给 Job。

结果保存在 `jobs.state.result`。同一执行中最后一次成功提交获采用，提交必须来自已经闭合的 Assistant 和 ToolResult 消息对。正常结束后将终止原因写入 `jobs.state.termination`，缺少结果会使 Job 失败。取消时尽力保留已完成的结果，业务效果是否已经提交以各自的持久化记录为准。

Sink 配置读取使用共享数据库权限。Peer 管理路由使用部署 JWT，MCP 客户端使用实例的 PAT。访问时复核当前 Peer 是否 enabled，以便停用后停止接收新的查询。
