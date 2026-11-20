# Twitter Rust 分发

`inkcre-twitter-extension` 实现 `inkcre/twitter` 的 `rust_stdio` 分发：共享原生 archive、四个 Resolver、Bookmark collect/backfill、OAuth setup，以及 x-monkey 0.2.0 HTTP 协议。业务合同沿用 `impl-py` 的 `9f75cbb`。official provider 使用 Rust HTTP 客户端；Twikit 模式由 Rust 调用管理员显式配置的 Python provider。持久图、Source 与 Job 操作直接连接共享 PostgreSQL，不创建 Peer 或执行 migration。

```sh
cargo build --locked -p inkcre-app -p inkcre-twitter-extension
target/debug/inkcre extension-manifest --name inkcre/twitter --version 0.1.0 \
  target/debug/inkcre-twitter-extension > target/debug/twitter.extension.json
```

将 manifest 与 executable 放在同一受信目录，通过 `INKCRE_EXTENSION_MANIFESTS` 提供本机分发，再选择精确版本并启用。停用撤回 Source 和 HTTP 执行入口，已安装的知识进程仍可解释持久资料；该进程只初始化读取资源，不执行 OAuth reconciliation、采集或 cookie 保存。

## 共享资料

| 能力 | ID |
| --- | --- |
| Bookmark Source | `extensions.twitter.bookmark.Source` |
| Post Resolver | `extensions.twitter.tweet.v2` |
| User Resolver | `extensions.twitter.user.v1` |
| Media Resolver | `extensions.twitter.media.v1` |
| Legacy tweet Resolver | `extensions.twitter.tweet.v1` |

Post、User、Media 使用严格稀疏 JSON：未知字段省略，禁止显式 null，原生 ID 为正十进制字符串。`extensions.twitter.reference.v1` JSON 关系表达 author、按出现顺序编号的 media、reply、quote 和 repost。缺失实体保存为 `{id}` placeholder，后续捕获补齐原 Block；legacy tweet.v1 身份可原位升级为 tweet.v2。无关关系与既有 Block 保留。

所有写入持有与 Python 分发一致的 `extensions.twitter.reconcile` 事务级 advisory lock。嵌套对象逐字段合并，数组替换；counter 按各自 `countsObservedAt` 的毫秒时间拒绝较旧覆盖，并保留原始时间精度。Block 与关系使用批量 SQL，减少远程 PostgreSQL 往返。official 和 Twikit 收藏都采用 supplement 策略，保留已经捕获的正文，继续补充其他事实；x-monkey submit 使用 replace 策略。

四个 Resolver 提供 canonical solved JSON、标签和文本。legacy tweet 的相关附件使用 SDK typed Resolver value 保留嵌套 bytes；canonical `POST /blocks/{id}/resolver/methods/get_solved_content` 会把二进制作为 multipart part 返回。兼容 `GET /blocks/{id}/solved` 保持 JSON 投影。普通作者 JSON 内部形似 bytes 标签的对象仍是 JSON。

## 收藏 Source

Source 配置为空对象，provider 使用整个扩展的当前配置。

- collect：`{"full":false,"result_limit":40}`。保存当前一页中位于 `latest_tweet_id` 之前的主帖及其依赖，然后更新普通 marker。
- `full=true`：保存整页，将下一页 token 放入 Job state 的 `page`，保持普通 marker；每次操作处理一页。
- backfill：`{"skip":0,"limit":200,"page_size":50}`。有界遍历，报告 scanned/saved/pages/exhausted/next_page，拒绝跨页重复 ID、重复 token和空 continuation page，保持普通 marker。

collect 沿用当前收藏顺序的 marker 合同；需要重新扫描时使用 full 或 backfill。每页提交前重新校验当前 Source 与 provider 身份。失败的持久化不推进该页 marker；backfill 已成功提交的较早页继续保留。

official provider 每次访问都校验 OAuth authorization ID 与 App fingerprint。刷新 token 采用数据库 CAS，拒绝覆盖较新的授权；401 标记 reconnect_required。接口依据 [X Bookmarks API](https://docs.x.com/x-api/posts/bookmarks/introduction)。

## OAuth setup

设置与账户是整个扩展的共享状态，OAuth transaction 绑定发起 Peer 的公开 callback URL。

| 方法 | 路由 | 认证 |
| --- | --- | --- |
| GET | `/twitter/setup` | Peer JWT |
| PUT | `/twitter/setup/oauth-app` | Peer JWT |
| POST | `/twitter/setup/oauth-transactions` | Peer JWT |
| POST | `/twitter/setup/oauth-transaction` | Peer JWT |
| DELETE | `/twitter/setup/account` | Peer JWT |
| GET | `/twitter/auth/callback` | 公开 callback，校验一次性 state |

OAuth App 输入为 `client_id`、`client_secret`，更换有 live setup 的 App 需要 `confirm_account_reset=true`。发起 transaction 返回 authorize_url，使用 S256 PKCE；查询输入为 `{"transaction_id":"…"}`。transaction 十分钟有效，后来的 setup 会终止较早的 pending/exchanging transaction，最多保留八条记录。callback 先原子 claim，再交换 token、读取当前用户，最后复核 transaction 与 App 身份。终态清除 provider state 和 PKCE verifier；未知、过期、重放与已 supersede 的 callback 不安装账户。pending transaction 可跨进程重启继续。

公开 callback URL 与 x-monkey URL 保留 `http_public_base_url` 的路径前缀。App 变更、断开账户和直接配置变更后的 reconciliation 会失效旧授权材料。token 和 App secret 不进入图、Job report 或错误文本。

## x-monkey

| 方法 | 路由 | 合同 |
| --- | --- | --- |
| POST | `/twitter/x-monkey/session` | 普通 Peer JWT、same-origin、application/json，建立浏览器会话 |
| GET | `/twitter/x-monkey/account` | 公开连接页面，可显示当前会话 |
| POST | `/twitter/x-monkey/disconnect` | same-origin 顶层导航，清除此浏览器会话 |
| GET | `/twitter/x-monkey/api/v1/session` | 返回当前浏览器会话状态 |
| POST | `/twitter/x-monkey/api/v1/archive/query` | archive cookie、来源及 client header 校验 |
| POST | `/twitter/x-monkey/api/v1/archive/submit` | 同上，原子写入共享 archive |

Cookie 名为 `inkcre_twitter_archive`，使用 Secure、HttpOnly、SameSite=None，Path 包含公开 URL 前缀。宿主负责 JWT 签发、验签、半寿命续期和清除；插件不持有部署 JWT_SECRET，也不接收原始 cookie 或 Peer Authorization。archive 会话不能用于普通 Peer API，普通 Peer JWT 只能经 session 入口建立浏览器会话；既有 `session_use=twitter_archive` cookie 保持兼容。签发时长由宿主的 `TWITTER_X_MONKEY_JWT_LIFETIME_SECONDS` 配置。

query/submit 要求 `Content-Type: application/json` 与 `X-Tweet-DB-Client: x-monkey`。Origin 允许 Core 自身、`https://x.com`、合法浏览器扩展来源，以及没有 Origin 的扩展请求；Origin 只是来源过滤，cookie 才是协议凭据。query 每类最多 1000 个 ID，保持请求顺序和重复 ID，区分 found/missing/failed。submit 每类最多 1000 条，严格校验 wire DTO 后复用同一 native graph transaction，返回逐记录状态和 summary。GET session 拒绝 no-cors 请求。连接页只在页面内存中使用用户输入的密钥，通过 WebCrypto 签发短时 Peer 凭据，并设置 nonce CSP、no-store 和 no-referrer。

## Twikit 本机运行时

将扩展配置的 `backend` 设为 `twikit`，填写 `email`、`username`、`password`，可选 `totp_secret`、`api_language` 和 `proxy`。本机解释器需要预装 `Twikit==2.3.3`。Rust 分发不自动安装 Python 或依赖包。

管理员通过宿主环境指定 `INKCRE_TWITTER_TWIKIT_RUNTIME`，宿主仅向 `inkcre/twitter` 转发该本机运行时配置路径。解释器和私密 cookie 路径、权限要求及部署步骤见 [Twikit 运行时说明](src/twikit/README.md)。这些路径不进入共享 Source 配置。

登录字段和运行时配置在采集前后、提交事务内复核；一次遍历期间账户身份固定。按配置 username 校验实际登录账号，比较时允许可选的 `@` 前缀。

桥进程通过受限 stdio 传递凭据，屏蔽交互输入和 provider 原始日志，不把凭据放进 argv。平台 challenge、认证和 rate limit 返回明确错误；没有安装 CAPTCHA solver。操作结束显式关闭并保存 cookie，失败和父进程退出会回收桥进程。受控 fixture 已验证本机协议、转换和状态边界；未使用真实 X 账户登录或验证平台在线可用性。

## 开发与验收

字段 schema 从 canonical Python 模型生成，共享 `$defs` 去重。在具有 canonical PDM 依赖的环境中运行：

```sh
pdm run python /path/to/impl-rs/extensions/twitter/scripts/update-schemas.py /path/to/impl-py
```

生成过程只加载独立 schema，不导入 app 或访问 provider。补充校验包括严格整数、UTC 时间、range 顺序和省略未知字段。

显式协议验收脚本 `scripts/accept-archive.py` 不加入 CI 或测试框架。它需要明确提供私密配置文件，启动临时 Peer，执行真实 HTTP 与 native PostgreSQL，然后恢复扩展 config/state 并清理本次实体。恢复快照写入 `.runtime/twitter-archive-restore.private.json`（0600），成功恢复后删除；异常中断时保留供恢复。

```sh
python extensions/twitter/scripts/accept-archive.py --config-json /private/native-peer.json \
  --app-binary target/debug/inkcre --extension-binary target/debug/inkcre-twitter-extension
```

脚本复用现有 `scripts/acceptance.py` 启动器，需要 Python 环境安装 `psycopg`。另外已使用受控 HTTP provider 与原生 Job 验证 official collect/backfill、刷新 CAS、OAuth callback 和授权竞争；使用 SDK 0.1.3 宿主验证 legacy 内嵌媒体 multipart 与 passive 读取。生产 default endpoint 为 `https://api.x.com/2/`；本机受信 fixture wrapper 可覆盖 `INKCRE_TWITTER_API_BASE_URL` 与 `INKCRE_TWITTER_AUTHORIZE_URL`。本目录不新增自动化测试套件。
