# B 站收藏夹 Rust 读取分发

`inkcre/bilibili` 解释 Web Distribution 已写入共享图的账号、收藏夹、视频和收藏条目。读取与词法索引使用已保存的事实，支持原生身份回退标签、标题及简介文本。

```sh
cargo build --locked -p inkcre-app -p inkcre-bilibili-extension
target/debug/inkcre extension-manifest --name inkcre/bilibili --version 0.1.0 \
  target/debug/inkcre-bilibili-extension > target/debug/bilibili.extension.json
```

将 manifest 加入宿主可信清单，安装并启用精确版本。扩展配置为 `{}`。四个 Resolver 为 `extensions.bilibili.user.v1`、`extensions.bilibili.folder.v1`、`extensions.bilibili.video.v1` 和 `extensions.bilibili.membership.v1`。普通图查询保留 `owns/contains/item/author` 关系的访问方式。

记录未知字段必须省略。显式 null、额外字段、非规范原生 ID 和非法来源时间会返回内容错误。默认与 lexical 文本投影一致，由既有 lexical maintenance Job 保存索引。

`example-graph.json` 提供四类记录及完整关系的虚构示例，可在已启用该插件的开发库通过 `POST /api/v1/graph/submit` 提交。返回的局部 ID 映射可用于读取四个 Block 的 solved、text 和 label。

采集由 Web Distribution 在用户选择的浏览器中完成。Rust 分发对应 canonical Python 的读取能力，配置与运行都无需 B 站凭据。
