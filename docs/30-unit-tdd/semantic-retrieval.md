# 语义检索

Rust 的 semantic 模块使用共享 embedding_profiles、block_embeddings 与 relation_embeddings。默认 profile 读取 semantic_retrieval 部署配置中的 default_profile。每次查询只在所选 profile 的向量空间内计算 cosine，按 score、实体类型和 ID 排序。resolver/storage 过滤作用于 Block，带有效过滤时省略 Relation 候选。

读取排除早于 profile、实体或 Relation 端点更新时间的向量，以及维度不符和零范数向量。查询向量通过 AI 执行层取得，模型选择和 dimensions 来自 profile。读取不会更新图或索引。

维护任务按 ID 分页扫描 Block 后扫描 Relation，使用 Resolver text 或端点 label 与关系内容产生文本。批量 embedding 在数据库作用域外执行，同批投影在短事务内写回。维护跳过新鲜记录，rebuild 以数据库调用开始时间为界重建此前记录。报告包含成功、不可用和失败计数与有界诊断。投影失败保留既有图资料，embedding 失败停止当前维护调用。

两个 Job ID 为 core.semantic_retrieval.maintain.v1 和 core.semantic_retrieval.rebuild.v1。固定 POST /semantic-retrieval 执行本地检索，ordinary facade 的 Peer 委派由应用路由组合。embedding-profiles 列表和详情直接读取共享目录。
