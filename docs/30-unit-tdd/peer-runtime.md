# Peer 发现与 HTTP 委派

嵌入 SDK、为业务增加远端执行入口时阅读本文。共享行为由 Hub 的 Peer 和知识能力契约拥有。Rust 使用既有 `peers` 行保存身份、配置、能力快照与租约。

`Core::peer_candidates` 按数据库时钟排除租约过期行和自己，明确目标时仅选择该 ID。HTTP 委派验证每个候选的完整快照，跳过格式错误、重复 capability ID 和未知协议。未指定目标时随机排列候选，不将某一语言实现固定为控制节点。

业务安装真实的本机 handler 后，以 `HttpCapability` 声明精确 capability、方法与相对路径。`peer_publish_http` 在行锁下读取 Peer 配置，将相对路径接到 `http_public_base_url` 并替换完整快照。未配置公开地址时发布空快照。Rust app 同时提供无前缀与 `/api/v1` 入口，部署者按实际外部路由填写 base。应用在续租前刷新快照，让用户配置变化生效。

`peer_delegate` 接受共享 HTTP v1 envelope，query 和 headers 是小写名称映射到字符串数组，body 的省略与显式 null 分别保留。方法和 URL 由提供者广告拥有。调用者提供本地 `PeerHttpOptions`，签发相同部署的 HS256 authenticated JWT，secret 不进入广告。Authorization、Host、连接及传输相关 header 由 transport 拥有。客户端禁止重定向，响应流限制为8 MiB。

只有连接建立失败，或对端返回 `InkCre-Peer-Execution: not-executed`，才允许选择下一候选。显式目标不切换其他 Peer。发送后超时、连接重置或读取失败返回 `OutcomeUnknown`，调用方必须将这种状态保留到业务层，避免重复副作用。普通 HTTP 4xx/5xx 是已经返回的业务结果，不据此重试。非 JSON body 返回协议错误。响应保留 status、多值 headers、可选 JSON body，并剔除 transport 自己的 execution header。

应用在进入 handler 前证明工作尚未开始时，才可附加上述 not-executed header，例如业务能力已经撤回。handler 开始后出现异常不能附加该信号。业务 facade 与被广告的本地 inbound 分别实现，后者直接执行本机业务，从而防止不同 Peer 互相转发。

`peer_routes` 提供 Python 兼容的 `/peers/self` 观察入口。Peer 的低层持久化 API 保持可嵌入，实际业务入口分别接入 lexical、semantic、organization 等模块。

验收使用独占开发库注册三个随机 Peer，发布固定 loopback HTTP 广告并续租。观察自身、过期及错误快照被排除，显式目标保留，成功 JSON、空 body 与显式 null 可区分。关闭 listener 时允许 failover，在 server 已接收请求后延迟响应则必须返回 OutcomeUnknown，且其他 server 未收到第二次请求。结束后删除本次精确 Peer 并停止 fixture。此流程显式执行，不随 CI 启动数据库或测试套件。
