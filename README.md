# monitor

## 特性

- 实时监控：秒级实时数据展示
- 轻量高效：Rust 语言构建，低资源占用，极简高效
- 自托管：完全掌控数据隐私，部署简单
- 通知：节点离线、恢复时推送到 Webhook / Telegram / Bark / Server酱 / JavaScript 脚本

## 通知

在后台「设置 → 通知」里配置渠道、模板和宽限期，配置完可以直接点「发送测试通知」
验证一次。

支持的渠道：

| 渠道 | 配置 |
|---|---|
| Webhook | 地址、GET/POST（默认 POST）、自定义请求头（JSON 对象）、可选 Basic 认证 |
| Telegram | Bot Token、Chat ID、可选 API 地址（默认 `https://api.telegram.org/bot`） |
| Bark | 设备 Key、可选服务器地址（默认 `https://api.day.app`）、可选推送级别 |
| Server酱 | SendKey、可选接口地址（默认 `https://sctapi.ftqq.com`，发送走 `{地址}/{SendKey}.send`） |
| JavaScript | 一段脚本，见下 |

JavaScript 渠道在 hub 内嵌的 `boa_engine` 里运行，不依赖 Node，也不需要 hub 之外的
任何东西。脚本必须定义 `sendMessage(message, title)`；如果还定义了
`sendEvent(event)`，则优先调用它，参数是事件对象（`event`/`title`/`node`/`message`/
`emoji`/`time`/`timestamp`，和 Webhook 的 body 同一套字段）。

脚本能用的全局对象只有两个，没有任何别的东西：

| 提供 | 说明 |
|---|---|
| `fetch(url, options)` | `options` 支持 `method`、`headers`、`body`；同步返回 `{status, ok, body}`，所以 `await fetch(...)` 和 `const r = fetch(...)` 都成立 |
| `console.log/warn/error` | 输出进 hub 日志 |

**不提供** `require`、`fs`、`crypto`、`process`、`setTimeout`、`import` 等；用到了就是
一个普通错误，会如实报给测试按钮和日志。脚本有确定性上限（引擎的循环与递归上限，以及
一次执行的总时间预算），`while (true) {}` 会被丢弃为一次渠道失败，不会拖住 hub。脚本
不编译、缺少 `sendMessage`、抛异常、超时，都按渠道错误处理（测试按钮会显示原因）。

行为：

- 节点断开后先等「离线宽限期」（默认 300 秒，可设 0 表示立即通知）；期间重连视为
  抖动，不发任何通知。
- 宽限期结束后仍在离线，才发一条离线通知；节点之后重新上线，再发一条上线通知
  （可在面板关掉）。
- 节点首次连接不算「上线」，hub 每次重启都会重置这份内存状态。
- 模板占位符：`{{event}}`、`{{node}}`、`{{message}}`、`{{time}}`、`{{emoji}}`；
  未识别的占位符原样保留，方便自己发现自己写错了。
- 发送失败重试 3 次，之后只记一条 warn 日志；通知失败不会影响节点连接和其它功能。
- 脚本里可以写 `async function sendMessage(...)` 并用 `await`：微任务队列会在同一次
  执行里跑完，而返回的 Promise 一直没有结果会被当作失败，而不是当作成功。
- 密钥（Webhook 地址/密码、Telegram Token、Bark Key、Server酱 SendKey）只写不可读：
  面板能设置，但不能读回原文，只能看到「已设置」。因此留空表示沿用已存的值。

## 组成

| 仓库 | 说明 |
|---|---|
| [monitor](https://github.com/monitor-probe/monitor) | hub：后台、API、公开页宿主 |
| [agent](https://github.com/monitor-probe/agent) | Linux agent |
| [monitor-theme-default](https://github.com/monitor-probe/monitor-theme-default) | 内置默认主题 |

```
agent (Linux)  ──WebSocket / JSON-RPC 2.0──▶  hub (axum + SQLite)  ──▶  后台 + 状态页
                                                             └─────▶  Webhook / Telegram / Bark / Server酱
```
