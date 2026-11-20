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
inkcre-cli config replace semantic_retrieval --schema-id core.semantic_retrieval.config.v1 \
  --input-json '{"default_profile":3}'
inkcre-cli agent create --schema
inkcre-cli query '总结资料' --sink 3 --timeout-seconds 120
inkcre-cli extension select inkcre/rss --distribution rust_stdio --version 0.1.0
```

连接输入结构为 `{base_url,jwt_secret}`，HTTP base 支持反向代理路径。客户端每次请求签发短期 shared Peer JWT。`connection check` 分别观察公共就绪和受保护 Peer 读取。客户端禁用重定向和隐式重试，HTTP响应上限32MiB。用法错误退出2，HTTP/传输错误退出1，批量查询出现逐项错误时保留成功项并退出1。

JSON输入可通过 `--input`、兼容别名 `--input-file` 或 `--input-json` 提供，文件名 `-` 从标准输入读取。`--schema` 查询当前服务的 `/openapi.json` 并投射该操作的 JSON输入结构，扩展配置使用接入Peer公布的schema。`connection set --schema` 可离线使用。`config replace` 的输入是value本身，命令根据 `--schema-id` 封装PUT请求。Source collect/backfill 的输入是config本身，超时通过query提交。`job wait --for` 接受正数秒或s/m/h后缀，并限制整个观察过程的耗时。

默认输出缩进JSON，`--json`输出完整单行JSON。`--table --fields id,name` 显示制表符分隔记录，`--fields` 也可筛选JSON对象或数组记录的顶层字段。二进制响应及multipart中的bytes保存为原始文件，JSON中使用 `{file}` 引用。作者原有的 `{bytes:[...]}` JSON保持原值。`--output-dir` 在指定目录下创建独占结果目录，保存结果入口和二进制文件。Unix目录权限0700，文件权限0600。

当前命令包括 get/update/delete、recall、graph submit/path/neighborhood，以及 job/cron/source/peer/config/extension/agent/sink/ai/embedding-profile/resolver 的基础管理。另提供 `query --sink`、`agent tools`、`ai models`、`config schemas`、Sink配置替换、扩展select/unselect/status/config管理。扩展坐标使用 `namespace/name`，`--peer`选择远端参与者，`extension install NAME VERSION` 是精确rust_stdio选择的别名。每个资源的 `--help` 展示操作，不适用的资源操作返回用法错误。

`resolver methods` 接受 `block:<id>` 或 exact Resolver ID，`resolver invoke --method` 调用对应读取，并可通过 `--schema` 获取方法参数。`graph components` 按输入关系类型返回连通分量。

仍待对齐的Python CLI行为包括Organization入口、部分metadata命令分页参数、服务端422字段位置映射、自动长输出预览，以及部分schema的owner子合同投射。当前CLI还需配合服务实际发布的路径使用，资源list/get保持远端响应结构。完整命令兼容性仍需逐项验收。
