# 本机 Twikit 运行时

启用 `backend=twikit` 的执行端阅读本文。Rust负责Page、canonical Batch与共享图事务；
内嵌Python桥只调用固定Twikit `2.3.3` Client、维护本机cookie并取得provider快照。
版本依据冻结Python `9f75cbb` 的pdm.lock与[Twikit 2.3.3发布](https://pypi.org/project/twikit/2.3.3/)。
运行时由操作者显式安装，不随采集请求安装依赖，也不探测本机其他账户目录。

先准备专用Python环境（本轮使用Python3.12）：

```bash
uv venv --python 3.12 /absolute/path/twikit-runtime
uv pip install --python /absolute/path/twikit-runtime/bin/python \
  -r extensions/twitter/src/twikit/requirements.txt
```

设置app的 `INKCRE_TWITTER_TWIKIT_RUNTIME` 为绝对JSON文件路径。宿主只把该路径
授予已启用的 `inkcre/twitter` 进程，停用后的知识读取进程不获得它。文件仅接受：

```json
{
  "python_executable": "/absolute/path/twikit-runtime/bin/python",
  "cookies_file": "/absolute/path/private/twitter/twikit_cookies.json"
}
```

解释器保留配置中的venv路径执行，允许正常venv符号链接，不因解析实际二进制位置
而丢失site-packages。解释器须为可执行普通文件。配置文件最多16 KiB；Unix下配置
与解释器均不能由group/other写入，配置建议0600。账号、密码、TOTP、language与proxy
继续来自部署Twitter配置，它们通过stdin传递，不进入命令行或日志。Python桥不获得
数据库连接能力。

cookie文件须为可写普通JSON文件，禁止符号链接；已有文件最多4 MiB，Unix权限必须
0600。它采用Twikit的string→string cookie字典格式，不读取浏览器profile。首次使用
只在指定位置创建目录，新目录为0700；其直接父目录不能由group/other写入。已有
cookies通过Twikit.login的cookies_file复用，成功登录、成功page和正常close均以0600
临时文件原子替换保存。关闭只结束HTTP连接，不调用平台logout。

## 调用与边界

桥先以无凭据hello检查依赖版本，随后接收配置。每个provider会话在首次请求时
调用Client.login，使用email、原始username、password、可选totp_secret及本机cookies。
Client.user核对实际账户；配置username比较时去掉一个可选`@`并忽略大小写，登录
入参本身保留。提供的TOTP可用于普通2FA；交互input、CAPTCHA solver与自动unlock
均未启用，UI metrics执行关闭。平台要求交互验证或解锁时明确返回操作者处理，
不会收集更多真实账号或尝试绕过挑战。

每次bookmarks直接调用 `get_bookmarks(count=limit,cursor=page)`，保留当前页primary
顺序与重复项，引用的quote/repost只进入Batch，不混入primary。next来自Result的
next_cursor，不自动翻页，也不以请求count截断已返回页。媒体、引用、HTML实体、
UTC秒时间与观察时刻按冻结provider映射为Rust canonical模型。最后由原有Batch
schema检查，错误不会变成空捕获。

`guard_in`在数据库行共享锁下复核Twikit相关配置指纹，并确认本机runtime路径配置
未变；采集期间改账号、密码、语言、proxy、backend或runtime目标会拒绝落库。
cookie保持本机生命周期，不写共享Extension.state。每个Source run由Provider关闭
自己的桥；后续run从同一cookie文件恢复，不保留脱离执行生命周期的全局Python进程。

请求帧最多64 KiB，响应帧最多16 MiB；单次桥调用120秒。传输按id和version复核，
所有库异常只映射固定code。close幂等，失败也回收child。独立stdin EOF监测在网络
请求尚未结束时取消操作；Rust父进程被整体强杀后，Python最多约2秒兜底退出。

| 情况                                          | Rust错误         |
| --------------------------------------------- | ---------------- |
| 缺本机配置、依赖版本不符、解释器/cookie不可用 | twitter_conflict |
| 认证失效或账户身份不同                        | twitter_conflict |
| 平台验证/解锁挑战                             | twitter_conflict |
| 限流、网络、provider响应或传输失败            | provider_error   |
| 输入页参数越界                                | invalid_input    |

本轮以固定依赖、合成账户和loopback provider验证登录参数、真实Twikit cookie格式
复用、带@用户名、当前页/next、quote/repost/media映射、配置guard、挑战错误净化、
private权限、重复close与杀死Rust父进程后的桥退出。没有使用真实平台账号，也没有
把这类验收接入自动化测试套件。
