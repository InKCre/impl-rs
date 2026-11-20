# Lexical 检索

嵌入 SDK 使用 `Core::lexical_retrieve`。HTTP 的 `POST /lexical-retrieval` 接受 `query`、缺省20且范围1–20的 `limit`，以及可选 `filters`。过滤项为 `resolver: {eq: exact_id}` 和 `storage: {eq: integer}`，同时存在时使用 AND，显式 null predicate 被拒绝。空白 query 被拒绝。

检索消费 canonical `inkcre.block_lexical_records`，要求记录更新时间至少为 Block 更新时间。证据按 label_exact、label_substring、text_substring、terms 排序，同类按 PostgreSQL simple 文本检索分数降序，最终按 Block ID 升序。LIKE 的百分号、下划线与反斜杠按字面匹配。label 权重 A、text 权重 D，检索结果包含 Block、label、最多280字符的 excerpt、evidence 与 rank。

`Core::lexical_maintain(options, rebuild)` 扫描缺失或过期 projection，按 Block ID 递增分页。预算由 max_records、scan_page_size、diagnostic_limit 控制，缺省100、100、20。rebuild 以启动时数据库时间作 cutoff，把之前已有的记录也列为候选。每个 Block 通过当前进程 Resolver 的 label 和 lexical context text 投影，materialize_missing 为 true。未知 Resolver、缺少 lexical text 或空 label 记 unavailable，其他错误记 failed，诊断只记录固定标识。每批在一个事务内更新 projection，失败计入该批的 failed。

普通 worker 接入 `lexical::job_handlers()`，两个精确 Job ID 为 `core.feature_retrieval.lexical.maintain.v1` 和 `core.feature_retrieval.lexical.rebuild.v1`，缺省预算900与1800秒。参数为 `{options:{...}}`，catalog schema 固定 canonical profile 的 JSON。执行报告持久化到本次 Job state。Block 删除通过既有 FK 级联清理 projection。

Bilibili 等扩展只需注册可解释既有 Block 的 Resolver，提供 get_label 与 get_text(context=lexical) 即可由同一维护器投影。维护器不拥有扩展的业务资料和采集游标。

显式验收在独占 development 数据库执行 `python3 scripts/accept_lexical.py --config-json <私密native配置>`。脚本启动临时 HTTP Peer 和真实 worker，检查参数准入、维护/重建 Job、过期投影排除、字面 LIKE 与过滤，并按自建 ID 清理。它会启动本机 worker，运行环境须由该验收独占。
