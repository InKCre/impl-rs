# Job 与 Cron 运行时

嵌入 Rust worker、注册扩展 handler 或排查任务状态时阅读本文。共享行为以 [知识能力契约](../_shared/20-product-tdd/knowledge-capability-contract.md) 为准；此处说明 Rust 执行边界。

## 领取与实际执行

`JobRegistry` 是当前进程能执行的精确 Job 类型集合。注册时校验参数 schema，worker 在集合变化后发布共享 `job_types` 元数据。撤回 handler 不删除共享 catalog、待执行命令或其他 Peer 的能力。

`JobHandler::can_handle` 在领取前判定当前 Peer 能否执行这条命令。未知或暂不可执行的命令保留 pending。领取使用 `UPDATE ... WHERE status='pending' RETURNING`，因此多个 Peer 观察到同一命令也只有一个执行器成功领取。执行开始后才记 `job.started`，最终写入成功后才记 `job.closed`。

并发预算包含资格判断及执行中的任务。pending 发现按 ID 递增并轮转游标，避免前面的其他 Peer 专属工作阻挡后续命令。`WorkerNotifier` 提供有界、尽力而为的提交后提示；通常由周期发现承担进程重启和提示丢失后的 pending 发现。明确关闭 `discover_pending` 后，嵌入者负责提示具体新 Job，不再扫描 pending。

Job 提交只依赖持久 catalog，不要求本机有 handler。Source collect/backfill 的提交与 Cron 模板同时验证 Source 类型的命令 schema；这一步不依赖本机插件正在运行。

## 状态、取消与退出

`JobContext` 提供当前 Core、Job ID、即时持久化的 state 读写和取消信号。`patch_state` 在数据库内对对象顶层键做原子合并，并且只更新 running Job。业务进度必须写入其所属 Source、图或共享扩展状态；Job state 只记录这次尝试的执行情况。

领取后的执行预算按数据库时钟计算，再用本机单调计时器限制 handler。周期观察共享 abort 标记、实际持久状态和本机 handler 注册代次。停用或替换扩展会使旧代次停止执行。取消和超时先通知 `JobContext`，给 handler 一个有限清理窗口；窗口结束后仍未返回则取消并等待实际异步任务，最后才关闭 Job。动态扩展适配器负责同步终止其子进程，不能把外部副作用留给不受管理的后台任务。

成功、失败、超时与取消分别关闭为 finished、failed、timed_out、aborted。终态写入仅作用于仍 running 的行，并保留已持久化的 checkpoint。失败摘要使用固定错误标识，避免把第三方异常中的凭据或原文写入通用状态。

`WorkerHandle::shutdown` 先拒绝新领取，再取消并收尾已有执行；App 必须在关闭扩展资源和数据库之前等待它。清理或数据库写入超过预算时返回错误。进程崩溃或终态写入失败后，行可能持续 running，继续阻止关联 Cron 叠加新 Job；Rust worker 不会自动把这种行关闭。

## 显式维护与混合 Peer 边界

共享协议没有执行器 owner 字段。仅凭 `started_at + timeout_seconds` 已经过期，无法判断另一个 Peer 的 handler 是否仍在取消清理。因此常规 Rust 发现循环不调用全局 sweeper，不通过排除本机 active、额外宽限或单边 advisory lock 声称其他执行器已经退出。

`Core::jobs_expire_overdue` 保留为显式部署维护操作。调用前必须确认所有可能受影响的执行器已经退出；它按数据库时钟关闭过期 running 行，不负责取消实际 handler。已知具体 Job 时优先确认该执行器停止，读取最新 state，再用 `job_finish` 只关闭这一条记录。维护不重新排队，也不证明业务副作用已回滚；再次尝试需要创建新的 Job。

当前 Python scanner 仍可能自动关闭全局过期行。与它混合运行时，Rust 无法阻止它在清理窗口内提前写入终态；Peer lease 过期也不能证明进程停止。这是现有跨单元协议与参与者行为的限制，不能把 Rust 本机清理顺序当成部署级保证。

## Cron 发生与并发

Cron 使用五段 UNIX schedule，分钟粒度；day-of-month 与 day-of-week 遵循 OR 语义。部署时区来自 `inkcre.configs` 的 `core.cron`，schema 为 `core.cron.config.v1`，缺省 UTC。IANA 时区由 `chrono-tz` 解析。每次只判断数据库当前分钟，错过的分钟保持错过。

`cron_materialize(id)` 在独立事务内锁住模板，使用 `FOR UPDATE SKIP LOCKED` 排除并发物化者，以数据库时间生成 UTC 分钟 occurrence。`last_scheduled_for` 避免同一发生重复生成；`last_job` 尚为 pending/running 时不叠加新 Job。命令创建和这两个进度字段在同一事务提交，失败不留下半完成发生。

schedule 在部署时区匹配，发生身份仍是 UTC 分钟。因此夏令时跳过的本地分钟不会补跑，重复的本地分钟对应两个不同 UTC 时刻，并继续受 outstanding Job 限制。

`cron_run_now` 创建独立普通 Job，不修改 Cron 的发生进度，也不要求模板 enabled。`CronScheduler` 只负责周期物化，真正执行统一进入同一 worker。停止调度不删除模板或其已创建的 Job。

## 显式运行验收

在独占开发数据库中使用临时可执行程序注册唯一 JobType 与真实 native handler，提交先于 worker 启动的 pending 命令，再由两个 worker 竞争发现。观察一次实际执行、持久 checkpoint 和终态，同时观察不可执行参数保留 pending、并发不超过配置预算。等待 handler 写入 started checkpoint 后再请求取消、撤回 owner 或关闭 worker，避免把领取前取消误当作运行中取消。

超时验收还需要观察中间状态：handler 收到取消后进入一个可见且受控的清理窗口，第二个 Rust worker 继续正常发现。在窗口内反复读取 Job，必须保持 running 且没有 `closed_at`；允许清理结束后才观察 timed_out。只读取最终 timed_out 无法判断终态是否曾提前写入。显式维护的过期关闭与这条常规执行路径分别验收。

Cron 验收使用同一个临时 JobType，验证非法 schedule/参数被拒绝，两个事务并发物化同一当前分钟只创建一条 Job。要单独区分 occurrence 去重和 outstanding 限制，可仅将自建模板的 `last_scheduled_for` 调整为前一分钟，保留其 pending `last_job`，再观察新发生仍不创建；关闭旧 Job 后观察当前发生可以创建。停用模板后调用 run-now，确认它创建独立 Job 且不改发生进度。

验收结束先停止 worker，再删除自建 Cron、Job 和 JobType，查询确认这些精确标识没有剩余记录；临时程序与构建目录随即删除。这个流程显式执行，不作为自动化测试套件或 CI 的数据库操作。
