/// <reference types="node" />
import assert from "node:assert/strict"
import { changes, GIB, NOTIFY_SECRETS, notifyPatch, provisioningSite, trafficCorrection } from "./api.ts"

assert.deepEqual(changes({ public: true, price: 5 }, { price: 20 }), { price: 20 })
assert.deepEqual(changes({ total_rx: "100", month_tx: "2" }, { total_rx: "100", month_tx: "3" }), { month_tx: "3" })
assert.deepEqual(changes({ expires_at: "2030-01-01" as string | null }, { expires_at: null }), { expires_at: null })
assert.equal(provisioningSite("https://monitor.example.com:8443/"), "https://monitor.example.com:8443")
for (const site of ["http://monitor.example.com", "https://127.0.0.1", "https://[::1]", "https://2130706433", "https://0x7f000001", "https://localhost", "https://user@monitor.example.com", "https://monitor.example.com/path"]) {
  assert.equal(provisioningSite(site), "", site)
}
// An emptied traffic field means the counter is not to be corrected. Sent as 0
// it would clear a lifetime total, which must never decrease.
const shown = { total_rx: "1.5", total_tx: "2", month_rx: "0.25", month_tx: "1" }
assert.deepEqual(trafficCorrection(shown, { ...shown, total_rx: "" }), {})
assert.deepEqual(trafficCorrection(shown, { ...shown, total_rx: "   " }), {})
assert.deepEqual(trafficCorrection(shown, { ...shown, total_rx: "0" }), { total_rx: 0 })
assert.deepEqual(trafficCorrection(shown, { ...shown, total_tx: "3" }), { total_tx: 3 * GIB })
assert.deepEqual(trafficCorrection(shown, shown), {})

// 通知卡片回传的每一个可读键，缺一个服务端就收不到它，填错一个默认值就把操作者
// 的选择覆盖成别的东西。
assert.deepEqual(
  Object.keys(notifyPatch({})).sort(),
  [
    "notify_bark_level",
    "notify_bark_url",
    "notify_enabled",
    "notify_grace_seconds",
    "notify_javascript_script",
    "notify_notify_on_online",
    "notify_provider",
    "notify_serverchan_endpoint",
    "notify_telegram_chat",
    "notify_telegram_endpoint",
    "notify_template",
    "notify_webhook_headers",
    "notify_webhook_method",
    "notify_webhook_username",
  ],
)
const defaults = notifyPatch({})
assert.equal(defaults.notify_provider, "none")
assert.equal(defaults.notify_enabled, "off")
assert.equal(defaults.notify_grace_seconds, "300")
assert.equal(defaults.notify_webhook_method, "POST")
// 密钥留空表示不改，绝不回传空字符串：那会把已存的密钥清掉。
assert.ok(!("notify_bark_key" in notifyPatch({ notify_bark_key: "" })))
assert.equal(notifyPatch({ notify_bark_key: "  " }).notify_bark_key, undefined)
assert.equal(notifyPatch({ notify_bark_key: "device" }).notify_bark_key, "device")
// 一个都没填时，五个密钥一个都不在回传体里。
for (const key of NOTIFY_SECRETS) assert.ok(!(key in notifyPatch({})), key)
// 表单里的值原样带回，包括被改成非默认值的那几个。
const edited = notifyPatch({ notify_provider: "bark", notify_enabled: "on", notify_grace_seconds: "0", notify_bark_level: "critical" })
assert.equal(edited.notify_enabled, "on")
assert.equal(edited.notify_grace_seconds, "0")
assert.equal(edited.notify_bark_level, "critical")
// 操作者刚敲进去的密钥由面板单独保管：保存成功后清空，下一次保存不该再把上一把
// 密钥传一遍——面板手里只有「已设置」这一个布尔，它无从判断那还是不是同一把。
assert.equal(notifyPatch({}, { notify_telegram_token: "123:ABC" }).notify_telegram_token, "123:ABC")
assert.equal(notifyPatch({}, { notify_telegram_token: "  " }).notify_telegram_token, undefined)
assert.equal(notifyPatch({ notify_bark_key: "old" }, {}).notify_bark_key, "old")
assert.equal(notifyPatch({ notify_bark_key: "old" }, { notify_bark_key: "new" }).notify_bark_key, "new")
// URL 也是密钥：一次保存后它同样只留在 hub 那边。
assert.equal(notifyPatch({}, { notify_webhook_url: "https://hook.example.com/x" }).notify_webhook_url, "https://hook.example.com/x")
console.log("partial edits, traffic corrections, provisioning checks and notification patches passed")
