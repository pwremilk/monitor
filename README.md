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

JavaScript 渠道在 hub 内嵌的 QuickJS（`rquickjs`）里运行，不依赖 Node，也不需要 hub
之外的任何东西。脚本必须定义 `sendMessage(message, title)`；如果还定义了
`sendEvent(event)`，则优先调用它，参数是事件对象（`event`/`title`/`node`/`message`/
`emoji`/`time`/`timestamp`，和 Webhook 的 body 同一套字段）。

脚本能用的全局对象如下，没有任何别的东西。脚本按非严格模式运行（和 Node 的 CommonJS
一致，不写 `'use strict'` 时未声明赋值不会报错）：

| 提供 | 说明 |
|---|---|
| `fetch(url, options)` | `options` 支持 `method`、`headers`、`body`；同步返回 `{status, ok, body}`，所以 `await fetch(...)` 和 `const r = fetch(...)` 都成立；失败时抛异常，可以 `try`/`catch` |
| `console.log/info/warn/error/debug` | 输出进 hub 日志 |
| `setTimeout` / `clearTimeout` / `setInterval` / `clearInterval` | 单位毫秒，回调可以带额外参数；两个 clear 等价，都接受对方返回的句柄（一个数字） |
| `atob` / `btoa` | 二进制字符串：一个字符一个字节，和浏览器一致；超出 Latin-1 的字符抛错 |
| `Buffer` | `Uint8Array` 的子类：`Buffer.from(str\|array\|BufferSource, encoding)`、`.toString(encoding)`、`Buffer.alloc`、`Buffer.concat`、`Buffer.byteLength`、`Buffer.isBuffer`；编码支持 `utf8`/`hex`/`base64`/`base64url`/`latin1`(`binary`)/`ascii`/`utf16le`(`ucs2`) |
| `crypto` | `randomUUID()`（v4）、`getRandomValues(typedArray)`（整数 TypedArray，≤65536 字节）、`createHash(alg)`、`createHmac(alg, key)`（`update` 可链式，`digest(encoding)` 不带参数返回 `Buffer`）、`subtle.digest(alg, data)`（返回 Promise，解析为 `ArrayBuffer`）；摘要支持 `sha1`/`sha224`/`sha256`/`sha384`/`sha512` |
| `process` | `env`（只读快照，已冻结）、`platform`、`arch`、`version`、`versions`、`argv` |
| `require` | 只有五个模块：`node:path`（`join`/`resolve`/`normalize`/`isAbsolute`/`basename`/`dirname`/`extname`/`sep`/`delimiter`）、`node:os`（`platform`/`arch`/`hostname`/`tmpdir`/`EOL`）、`node:util`（`format`/`inspect` 简版）、`node:crypto`（就是全局 `crypto` **本身**，不是副本）、`node:buffer`（`{ Buffer }`，就是全局 `Buffer`）；`node:` 前缀可省，`require('crypto')` 与 `require('node:crypto')` 是同一个对象。其它一律 `Error: Cannot find module '...'` |

**不提供** `fs`、`child_process`、`net`、`http`、`dgram`、`worker_threads`、`import`
（静态和动态都不行）、`XMLHttpRequest`、`WebSocket`、`process.exit` 等；用到了就是一个
普通错误，会如实报给测试按钮和日志。一句话：脚本可以对外发 HTTP，但碰不到文件、进程和
网络套接字。

### 定时器与事件循环

- 一次执行结束时会同时排空微任务队列和**到期的定时器**：先微任务，再定时器，循环往复，
  直到没有待办或预算用尽。`await` 和 `setTimeout` 都能等到结果。
- `setTimeout(fn, 0)` 在同步流程跑完之后才执行，不会插队。
- 定时器和网络等待一起受同一个 20 秒预算约束。**没清掉的 interval 会让事件循环一直不
  空闲**，于是会在预算耗尽时按渠道错误处理——脚本要自己 `clearInterval`（这也是 Node
  里没人清 interval 就不退出的同一件事，只是这里没有第二个进程可以留下）。
- CPU 死循环（`while (true) {}`）由引擎的中断处理器在约 3 秒内判失败；内存上限 64 MiB。
  两者都是渠道错误，不会拖住 hub。

脚本不编译、缺少 `sendMessage`、抛异常、超时、未配置，都按渠道错误处理（测试按钮会显示
原因，并带上脚本里的位置，例如 `notify.js:12:3`）。

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
