# Learn English

`inkcre/learn-english` 的 Rust stdio 分发使用 `extensions.learn_english.lexical.v1` 读写共享词条。`text` 保存原文，`pos` 可省略，缺省为空数组。`/graph/draft` 生成一个词条节点，可带入关系后提交。文本投影保留原文，标题按共享格式返回 `lexical item <…>`。已有同一 Resolver 的词条可以直接读取。

使用 `cargo build --locked -p inkcre-learn-english-extension` 构建。生成清单、选择并启用扩展的步骤见 [运行说明](../../docs/40-deployment/runtime.md#插件生命周期)。
