import { useEffect, useState } from "react"

export type Metrics = {
  uptime: number
  cpu: number
  load: [number, number, number]
  mem_total: number
  mem_used: number
  swap_total: number
  swap_used: number
  disk_total: number
  disk_used: number
  net_rx: number
  net_tx: number
  total_rx: number
  total_tx: number
  month_rx: number
  month_tx: number
  tcp: number
  udp: number
  procs: number
}

export type Node = {
  id: number
  name: string
  sort: number
  public: boolean
  online: boolean
  last_seen: number
  metrics: Metrics | null
  os: string
  kernel: string
  arch: string
  virt: string
  cpu_name: string
  cpu_cores: number
  mem_total: number
  swap_total: number
  disk_total: number
  agent_version: string
  price: number
  currency: string
  billing_cycle: string
  expires_at: string | null
  traffic_limit: number
  traffic_mode: string
  traffic_reset_day: number
  total_rx: number
  total_tx: number
  month_rx: number
  month_tx: number
  month_start: string
  /** Panel only. */
  hostname?: string
  /** ISO 3166-1 alpha-2, derived from the address the agent connects from. */
  country: string
  ip?: string
  ipv4?: string
  ipv6?: string
  remark?: string
  /** Panel only. Empty for nodes created before the hub retained a copy. */
  token?: string
}

export type PingTask = { id: number; name: string; target: string; interval: number; nodes: number[] }

/** Form snapshots must never overwrite fields the user did not edit. */
export function changes<T extends object>(initial: T, values: Partial<T>): Partial<T> {
  return Object.fromEntries(Object.entries(values).filter(([key, value]) => value !== initial[key as keyof T])) as Partial<T>
}

/**
 * 通知配置里只写不可读的键，和服务端 `notify::SECRETS` 一一对应。
 *
 * 面板拿不到密钥原文，只有一个 `<key>_set` 布尔。把空字符串发回去等于把已存
 * 的密钥清空，而面板根本不知道原来有没有。
 */
export const NOTIFY_SECRETS = [
  "notify_webhook_url",
  "notify_webhook_password",
  "notify_telegram_token",
  "notify_bark_key",
  "notify_serverchan_key",
] as const

/**
 * 「通知」卡片的回传体：服务端 `/api/settings` 给的可读键原样带回（默认值已由
 * 服务端填好），密钥只在操作者重新输入时带上。
 *
 * 单独一个纯函数，是因为这里的每一条默认值都必须和服务端读取时的一致：面板漏
 * 传一个键，保存就静默地什么也没写；带上一个空字符串，就把已存的密钥清掉了。
 */
export function notifyPatch(s: Record<string, string | boolean>): Record<string, string> {
  const patch: Record<string, string> = {
    // 两个开关的默认方向不同，和服务端一致：通知默认关（新装一个 hub 不该自己开始
    // 往外发消息），上线通知默认开。缺键时按默认值走，而不是按「不是 off 就是 on」。
    notify_enabled: s.notify_enabled === "on" ? "on" : "off",
    notify_notify_on_online: s.notify_notify_on_online === "off" ? "off" : "on",
    notify_provider: String(s.notify_provider || "none"),
    notify_grace_seconds: String(s.notify_grace_seconds || "300"),
    notify_template: String(s.notify_template ?? ""),
    notify_webhook_method: String(s.notify_webhook_method || "POST"),
    notify_webhook_headers: String(s.notify_webhook_headers ?? ""),
    notify_webhook_username: String(s.notify_webhook_username ?? ""),
    notify_telegram_chat: String(s.notify_telegram_chat ?? ""),
    notify_telegram_endpoint: String(s.notify_telegram_endpoint ?? ""),
    notify_bark_url: String(s.notify_bark_url ?? ""),
    notify_bark_level: String(s.notify_bark_level ?? ""),
  }
  for (const key of NOTIFY_SECRETS) {
    const value = String(s[key] ?? "")
    // 只有空白也算没填：操作者把输入框清成空格，不该被当成一把新密钥存进去。
    if (value.trim()) patch[key] = value
  }
  return patch
}

export const GIB = 1024 ** 3

/**
 * The traffic fields as a `TrafficPatch`: GB entered by hand, bytes on the wire,
 * and only the counters actually given a value.
 *
 * An emptied field means the counter is left unchanged rather than set to zero.
 * The patch is entirely `Option` and `set_traffic` COALESCEs, so omitting the key
 * expresses that; sending 0 would clear a lifetime total, the one figure that may
 * never decrease and that nothing can recompute. Zeroing deliberately remains one
 * keystroke away.
 */
export function trafficCorrection(
  pristine: Record<string, string>,
  typed: Record<string, string>,
): Record<string, number> {
  return Object.fromEntries(
    Object.entries(changes(pristine, typed))
      .filter(([, value]) => String(value).trim() !== "")
      .map(([key, value]) => [key, Math.round(Number(value) * GIB)]),
  )
}

/** Installation commands require a TLS origin with a domain, never an IP. */
export function provisioningSite(site: string): string {
  try {
    const u = new URL(site)
    return u.protocol === "https:" && !u.hostname.startsWith("[") && !/^\d+\.\d+\.\d+\.\d+$/.test(u.hostname)
      && u.hostname !== "localhost" && !u.hostname.endsWith(".localhost") && !u.username && !u.password
      && u.pathname === "/" && !u.search && !u.hash ? u.origin : ""
  } catch {
    return ""
  }
}

export class ApiError extends Error {
  status: number
  constructor(status: number, message: string) {
    super(message)
    this.status = status
  }
}

export async function api<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`/api${path}`, {
    ...init,
    headers: init?.body ? { "content-type": "application/json", ...init?.headers } : init?.headers,
  })
  if (!res.ok) throw new ApiError(res.status, (await res.text()) || res.statusText)
  return res.status === 204 ? (undefined as T) : res.json()
}

/**
 * 4 MiB: the only size a reverse proxy must pass, whatever the file behind it
 * weighs. The hub accepts up to 8 MiB per request, so this can change without
 * touching the server or negotiating first.
 */
const CHUNK = 4 * 1024 * 1024

/**
 * Uploads a file one chunk at a time. There is no upload id: the hub tracks an
 * upload by the length of what it has already written, so a chunk states only
 * where it begins. The last one carries the result.
 */
export async function upload<T>(
  path: string,
  file: File,
  onProgress?: (sent: number) => void,
  signal?: AbortSignal,
): Promise<T> {
  if (file.size === 0) throw new ApiError(400, "文件是空的")
  let last: Response | null = null
  for (let offset = 0; offset < file.size; offset += CHUNK) {
    // A chunk boundary is a genuine stopping point: the hub applies nothing until
    // the last piece lands, and `offset = 0` truncates whatever an abandoned
    // attempt left behind, so aborting here leaves the state unchanged.
    if (signal?.aborted) throw new DOMException("aborted", "AbortError")
    const res = await fetch(`/api${path}?offset=${offset}&total=${file.size}`, {
      method: "POST",
      headers: { "content-type": "application/octet-stream" },
      body: file.slice(offset, offset + CHUNK),
      signal,
    })
    if (!res.ok) {
      // A 413 never reached the hub: the proxy in front answered, and only its
      // own logs record it. The message names the setting responsible.
      throw new ApiError(
        res.status,
        res.status === 413
          ? "反向代理拒收了 4 MiB 的分片，把 nginx 的 client_max_body_size 调到 8m"
          : (await res.text()) || res.statusText,
      )
    }
    last = res
    onProgress?.(Math.min(offset + CHUNK, file.size))
  }
  return last!.json()
}

/**
 * Live node list. Uses the WebSocket the hub pushes every two seconds, falling
 * back to polling if it cannot be established.
 */
export function useNodes() {
  const [nodes, setNodes] = useState<Node[] | null>(null)
  // null until a frame reports it. The panel treats an explicit false as the
  // session no longer being an admin one, so an unanswered first fetch must not
  // read as that; see App.tsx.
  const [admin, setAdmin] = useState<boolean | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [reload, setReload] = useState(0)

  useEffect(() => {
    let socket: WebSocket | null = null
    let poll: ReturnType<typeof setInterval> | null = null
    let retry: ReturnType<typeof setTimeout> | null = null
    let closed = false

    const fetchOnce = () =>
      api<{ nodes: Node[]; admin: boolean }>("/nodes")
        .then((d) => {
          setNodes(d.nodes)
          setAdmin(d.admin)
          setError(null)
        })
        .catch((e: Error) => {
          setError(e.message)
          // With the public page switched off, a revoked session receives a 401
          // here and on the stream, so the frame that would report admin=false
          // never arrives and the panel would retain the list it already had.
          if (e instanceof ApiError && e.status === 401) setAdmin(false)
        })

    fetchOnce()

    const url = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/api/ws`
    // A hub restart closes every stream. Without reconnecting, a page that
    // outlives a deploy would remain on the fallback poll for the rest of its
    // life, refreshing at a fifth of the live rate with no indication.
    const connect = () => {
      try {
        socket = new WebSocket(url)
      } catch {
        poll ??= setInterval(fetchOnce, 5000)
        return
      }
      socket.onmessage = (event) => {
        const frame = JSON.parse(event.data)
        setNodes(frame.nodes)
        setAdmin(frame.admin)
        setError(null)
        // The stream has returned; the poll was only covering for it.
        if (poll) {
          clearInterval(poll)
          poll = null
        }
      }
      socket.onerror = () => socket?.close()
      socket.onclose = () => {
        if (closed) return
        poll ??= setInterval(fetchOnce, 5000)
        retry = setTimeout(connect, 5000)
      }
    }
    connect()

    return () => {
      closed = true
      socket?.close()
      if (poll) clearInterval(poll)
      if (retry) clearTimeout(retry)
    }
  }, [reload])

  return { nodes, admin, error, refresh: () => setReload((n) => n + 1) }
}
