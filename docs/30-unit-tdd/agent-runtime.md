# Agent 定义与执行

嵌入 Agent 或排查一次工具调用时阅读本文。Agent 定义保存在共享的 `agents` 表，模型、系统提示、精确 Tool ID 和每轮模型调用预算随定义一起编辑。定义管理不要求本机具备该工具或模型执行能力。PATCH 在行锁中合并完整值并验证，只有 tool_choice 接受显式 null。

`agent::Runtime` 保存进程内工具注册，每个工具提供稳定 JSON Schema 和异步 handler。创建 Thread 时读取定义快照并绑定工具。已经创建的 Thread 保留快照，后续定义修改作用于新 Thread。Thread 默认使用进程内 whole-thread backend，与 Python 当前默认 backend 的生命周期一致。嵌入者通过 Runtime::with_persistence 注入 ThreadPersistence，实现 create/read/append/discard_trailing_incomplete_tool_calls 四个原子边界。ThreadState 序列化保留模型、工具 schema、选择、预算和完整消息，单次 append 必须在持久后端整体提交。Thread::refresh 从该权威重新读取。

`create_thread` 创建快照，`run` 创建快照并执行初始 turn。`Thread::turn` 接受 canonical User message，至多一个 turn 同时运行。调用者可取消该 future，取消会停止模型请求并取消尚在执行的工具任务。工具已经提交的业务效果继续存在。Assistant 和完整 ToolResult batch 一起保存，取消未完成 batch 后的下一轮不会把未闭合的 ToolCall 传给模型。

每轮以模型调用数限制循环。无 ToolCall 的回复结束为 completed，完整执行最后一批工具后用尽预算则结束为 max_model_calls。工具在 batch 中并发执行，结果按模型原有次序恢复。未知工具、参数 schema 错误和 handler 的可操作错误成为 is_error ToolResult，通用执行失败使用固定诊断。

HTTP 提供 Agent CRUD 与本机 Tool metadata/schema 发现。Agent 的执行由拥有业务输入和副作用的 Organization、Sink 或嵌入调用者驱动。

`register_builtin_tools` 注册 retrieve、get_entities、resolver、find_path 与 get_entity_neighborhood、get_connected_components。检索按各自 owner 保留分支排名，传给模型的候选只包含实体引用。实体读取保持请求顺序和缺失 null，Resolver 工具按调用逐项返回结果与错误。图工具使用 bounded 读取并保留探索预算耗尽的结果。

连通分量按输入 seed 顺序去重，沿指定精确 Relation content 做无向遍历。全调用共享 Block 与 Relation 预算，结果同时返回已观察的 seed 分组、闭合 spanning proof、缺失 seed 和 truncated 标志。预算耗尽后尚未探索的有效 seed 保持单项分组，调用者以 truncated 判断分组是否完整。
