# Bilibili 读取分发

实现或维护 Bilibili 共享图的读取时使用本文。业务基准为 impl-py `9f75cbb` 的 `extensions/bilibili`，原生图结构与 Web Distribution 共用。Rust 扩展使用独立 executable，通过 typed Resolver 接口提供 solved、text 与 label。

四类记录均使用 sparse object。未知字段通过省略表达。MID、收藏夹 ID 与 aid 是无前导零的正十进制字符串，BVID 保持 `BV` 加十位字母数字。来源时间要求带 `Z` 的有效 UTC RFC3339 字符串。所有读取都验证持久表示，历史损坏记录明确返回内容错误。

User 标签为名称或 `UID <mid>`，Folder 为标题或 `收藏夹 <id>`，Video 为标题、BVID 或 `av<aid>`。Membership 根据 availability 输出失效或普通收藏条目标记。Folder 和 Video 文本将非空简介追加到标签之后。Lexical 与默认文本遵循同一规则。

扩展读取普通图事实，关系仍由 Core 图入口访问。现有 lexical retrieval 维护 Job 消费 Resolver 投影，扩展没有独立索引或缓存。二进制构建与 manifest 生成见 [扩展使用说明](../../extensions/bilibili/README.md)。
