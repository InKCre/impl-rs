# Organization 图语义与 Agent 执行

组合 Organization 操作时使用 `Core` 的显式业务方法。持久描述符采用七个既有 `core.organization.behavior.*.v1` Resolver，文本、标签和判断契约来自固定 Python 基线。候选通过 `candidate for` 关系指向描述符。

`organization_relation` 支持 `supersedes`、`refines`、`supports`、`challenges`、`duplicates assertion`。两个端点必须不同且已存在。supersedes 和 refines 在写入前检查反向路径，重复断言以较小 ID 指向较大 ID。相同关系再次提交返回既有 ID 与 `created=false`。

`organization_synthesis` 要求非空文字和至少两个不同来源。复用条件包含文字及完整来源集合，来源以 `synthesis` 指向合成文字。指定前一合成时核对其多来源基础，并添加 `edited` 关系。`organization_anchor` 使用 `has mention` 与 `refers to` 保存具体片段，优先复用已有完整路径。每个操作在一个原生 PostgreSQL 事务内提交。

`register_tools` 把七个 Organization 写入工具登记到调用者持有的 Agent Runtime。工具失败区分可反馈给模型的输入/业务错误和数据库故障。`organization_ruminate` 读取已配置 Agent，建立包含焦点文字、直接关系和 drafting 能力的首条消息，并等待实际 Turn 结束。达到模型调用预算时返回显式错误。调用者取消该 future 会取消模型与工具执行。

Supersession Resolver 公开有界 `read_lineage`，遍历 supersedes 连通历史并检测环。结果被截断或含环时，current frontier 为空。

显式 rumination Job 使用 `core.organization.rumination.explicit.v1`，领取前检查配置的 Agent 能力，执行与取消由共享 worker 管理。

七种自动 Job 使用 canonical exact ID、参数 schema 和 1800 秒默认预算。max_seeds 缺省 10，范围 3–100。每种行为依据候选、近期信息或行为相关关系变化、随机信息三个类别选种，先保留每个非空类别的位置，再按类别优先级填充与去重。描述符不会进入近期或随机类别。supersession/refinement 使用 edited 信号，evidence stance 使用近期关系，synthesis 还追踪被变化来源影响的已有合成。

每个 seed 在数据库读取结束后调用独立 Agent Turn。seed 已删除或达到模型调用预算时记录诊断并继续其他 seed，配置、数据库与执行基础故障使本次 Job 失败。非 rumination 判断读取最多 20 条直接关系，关闭缺失信息物化并携带该行为完整判断契约。

原生数据库验收已覆盖关系幂等、环拒绝、lineage、合成及 anchor 复用。HTTP 与真实 worker 验收覆盖显式 rumination、自动 refinement、配置选择、bounded seeds 和 Agent 预算耗尽后的正常收尾。其余六种自动行为复用同一执行器，并对照固定 Python 的元数据和分类策略核对。通用 drafting 工具绑定与媒体解释编排仍需继续完成。
