# Rust REST CLI

`inkcre-cli` 是独立 HTTP consumer。命名连接采用 Python CLI 的 JSON 文件格式，默认路径为 `~/.inkcre/cli/connections.json`，可由 `--connections-file` 或 `INKCRE_CLI_CONNECTIONS_FILE` 指定。写入使用同目录临时文件及原子替换，Unix文件权限为0600。连接列表展示地址，`connection get` 按明确命令读取完整连接，包括 secret。

```sh
cargo build --locked -p inkcre-cli
inkcre-cli connection set local --input /私密目录/connection.json
inkcre-cli --connection local connection check
inkcre-cli --json recall '资料' --mode lexical
inkcre-cli --output-dir ./结果 get block:42 relation:8 --content hydrated
inkcre-cli source collect 4 --timeout-seconds 120 --input-json '{}'
inkcre-cli job wait 9 --for 30s
inkcre-cli organization ruminate 42 --timeout-seconds 120
inkcre-cli config replace semantic_retrieval --schema-id core.semantic_retrieval.config.v1 \
  --input-json '{"default_profile":3}'
inkcre-cli agent create --schema
inkcre-cli query '总结资料' --sink 3 --timeout-seconds 120
inkcre-cli extension select inkcre/rss --distribution rust_stdio --version 0.1.0
```

连接输入结构为 `{base_url,jwt_secret}`，HTTP base 支持反向代理路径。客户端每次请求签发短期 shared Peer JWT。`connection check` 分别观察公共就绪和受保护 Peer 读取。客户端禁用重定向和隐式重试，HTTP 响应上限 32 MiB。用法错误退出 2，HTTP 或传输错误退出 1。HTTP 错误的 JSON 包含 `detail` 和 `http_status`。422 输入位置按命令实际接受的 JSON 字段和参数显示，批量查询出现逐项错误时保留成功项并退出 1。

JSON 输入可通过 `--input`、兼容别名 `--input-file` 或 `--input-json` 提供，文件名 `-` 从标准输入读取。`--schema` 读取服务端的 `/openapi.json` 或负责该输入的类型目录。Job、Cron、Source 与 Sink 的嵌套参数使用所选类型的合同。命令行已提供的字段从 JSON 输入结构移除，并保留嵌套合同的本地引用。`connection set --schema` 可离线使用。`config replace` 根据 `--schema-id` 封装 value。`job wait --for` 用正数秒或 s/m/h 后缀指定整个观察预算。

默认输出原始字符串或缩进 JSON。显示内容超过 8,000 字符时，终端输出前 2,000 字符和完整入口路径。`--json` 输出完整单行 JSON。`--output-dir` 指定导出目录，在独占子目录保存完整入口和原始附件。入口 JSON 内的附件路径相对于入口文件，便于整体移动。二进制响应通过 `{file}` 引用，作者原有的 `{bytes:[...]}` JSON 保持原值。Unix 目录权限为 0700，文件权限为 0600。`--table --fields id,name` 显示制表符分隔记录，`--fields` 也可筛选 JSON 对象或数组记录的顶层字段。

当前命令包括 get/update/delete、recall、graph submit/path/neighborhood，以及 job/cron/source/peer/config/extension/agent/sink/ai/embedding-profile/resolver 的基础管理。另提供 `organization ruminate BLOCK_ID`（提交 explicit rumination Job）、`query --sink`、`agent tools`、`ai models`、`config schemas`、Sink配置替换、扩展select/unselect/status/config管理。扩展坐标使用 `namespace/name`，`--peer`选择远端参与者，`extension install NAME VERSION` 是精确rust_stdio选择的别名。每个资源的 `--help` 展示操作，不适用的资源操作返回用法错误。

`resolver methods` 接受 `block:<id>` 或 exact Resolver ID，`resolver invoke --method` 调用对应读取，并可通过 `--schema` 获取方法参数。`graph components` 按输入关系类型返回连通分量。

`recall` 支持semantic的 `--min-score`、重复 `--entity-type`，同一mode只请求一次；每项结果平铺保留远端字段。`graph neighborhood` 支持Block方向、重复 `--content`、limit/cursor，Relation拒绝这些遍历参数。metadata的types/schemas/tools/models支持limit/cursor。
