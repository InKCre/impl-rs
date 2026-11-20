# CalDAV / CardDAV Rust 分发

执行 `cargo build --manifest-path extensions/caldav/Cargo.toml` 构建该分发。

该进程以 `inkcre/caldav` 的 `rust_stdio` 分发运行，使用 app 的 HTTP ingress 和原生 PostgreSQL 凭据。在 workspace 根目录运行 `cargo build --locked -p inkcre-app -p inkcre-caldav-extension`。app 负责登记 manifest、选择版本和启动进程。

## 连接与认证

在扩展配置设置随机的 32–128 字符 `password`，用户名固定为 `inkcre`。DAV 客户端使用 HTTP Basic；每个请求读取当前共享配置，清空密码立即撤销访问。Host 将该协议凭据原样交给扩展，不用 Peer JWT 代验。

`GET /caldav/connection` 则要求 Peer JWT，公布 `caldav.connection.v1` 能力，返回 `server_url` 和 `username`，不返回密码。服务地址来自本 Peer 的 `http_public_base_url`，未设置时返回 409。DAV 根路径为 `/caldav/`，个人主体为 `/caldav/inkcre/`。首次认证访问会创建 calendar、tasks、contacts 三个默认集合，分别提供事件、任务和联系人。

反向代理须保留 OPTIONS、PROPFIND、REPORT、MKCOL、MKCALENDAR、PROPPATCH、MOVE、GET、HEAD、PUT、DELETE，以及 Depth、If-Match、If-None-Match、Destination 和 Overwrite 头。请求体最多 16 MiB，单个资源最多 8 MiB。修改或删除已有资料、移动资料均要求匹配当前 ETag 或 `If-Match: *`；集合整体 PUT 覆盖禁用。

支持属性发现与修改、calendar/addressbook query、multiget、sync-collection、日程重复规则的时间范围查询、calendar-data expand、组件/属性选择，以及 free-busy-query。重复规则由有预算限制的 RFC 5545 执行器处理，支持 RRULE、RDATE、EXDATE、覆盖实例及 IANA TZID。超过计算预算或遇到无法解算的自定义时区时明确失败。同步令牌对应当前内容，发生变化后旧令牌过期，客户端须重新枚举；没有独立增量历史。

与基线 Radicale 一样，calendar-data 的 limit-recurrence-set/limit-freebusy-set 选择器保留完整内容；不单独解算 REPORT 内的自定义时区定义。手机平台和第三方 DAV 客户端的实际兼容性尚待独立验收，不能以 loopback 协议旅程代替。

## 共享图与停用行为

持久模型与固定 Python 基线共用六个 exact Resolver：`extensions.caldav.{collection,calendar,contact}.v1` 和 `.v2`。集合保存 `{path, props, published}`，`member` 关系控制发布；v2 资源根保存 `{href, remainder}`，其中只保留由原生格式独占的内容。

TEL、EMAIL、URL、NOTE、DESCRIPTION、PHOTO、LOGO、SOUND、ATTACH 和 RELATED-TO 的值由独立 Block 与 `caldav:property:` 关系承载。关系保存组件、alarm、属性名、参数、group、位置和表示方式。普通文字使用 `core.text.v1`；嵌入字节使用既有 PostgreSQL storage `-4` 并依 MIME 选择 Resolver。唯一匹配同集合 UID 的 RELATED-TO 引用直接指向该资源，否则保留为独立文字值。其他 Peer 对这些语义值的修改会进入下一次 DAV GET、ETag 和同步视图。

每次操作在一个数据库事务中读取 DAV 图；写操作使用与 Python 相同的 `extensions.caldav.v1` advisory lock，并锁定根与依赖值，成功响应前原子提交。修改资料复用根 ID；替换语义值产生新 Block，旧值不会被覆盖或回收。DAV DELETE 仅撤下 `member` 或将集合标为 `published=false`，保留对象、语义值和外部关系。

活跃启动原子提升已有 v1 资源到 v2，保留根 ID 和外部引用。停用后通过 SDK `passive_knowledge` 保留已安装 Resolver 的读取能力；此模式只初始化数据库，不迁移 v1、不挂载 HTTP。协议访问及连接能力随停用撤回。备份和恢复沿用共享数据库。

实现按 schema、content-line codec、graph transaction、protocol、properties、report、temporal、projection 和 resolver 分工。显式执行的本地 loopback 旅程已验证协议认证、v1 迁移、图语义值与二进制、条件更新、查询/同步、重复日程、移动保 ID、删除撤发布和停用读取；未新增自动化测试套件。
