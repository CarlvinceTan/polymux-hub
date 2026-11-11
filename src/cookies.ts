// Cookie harvest for bridges whose login wants a browser session. TUI-only.
//
// The bridge hands over the login URL, optional extraction JS, and the URL it
// expects after a successful login. This opens one Chromium app window on that
// URL with a throwaway profile and drives it over the DevTools protocol: it reads
// the whole cookie jar (httpOnly cookies included, which document.cookie cannot
// see) plus whatever the extraction JS returns, and keeps polling until the login
// lands. The window is reused across attempts and only closed at the end.

import { spawn, type ChildProcess } from "node:child_process"
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"

const BROWSERS = [
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  "/Applications/Chromium.app/Contents/MacOS/Chromium",
  "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
  "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
]

/** Everything document.cookie exposes, as a JSON object. */
const COOKIE_EXPRESSION = `JSON.stringify(Object.fromEntries(document.cookie.split("; ").filter(Boolean).map((pair) => { const at = pair.indexOf("="); return [pair.slice(0, at), pair.slice(at + 1)] })))`

export type HarvestParams = {
  url: string
  extract_js?: string
  wait_for_url_pattern?: string
  /** Cookie names the step asks for, when it names them. */
  names?: string[]
  /**
   * A header to lift off the page's own traffic, for logins whose credential is
   * not in a cookie or the DOM. Discord is the case: its web client signs every
   * API request with the session token in Authorization, which is the only place
   * that token is reliably readable.
   */
  capture?: { header: string; url_contains: string; as: string }
}

export type HarvestOptions = {
  signal?: AbortSignal
  timeoutMs?: number
  /** Used by tests. The real flow needs a window the user can log in with. */
  headless?: boolean
  /** Open as a chromeless app window rather than a browser tab. */
  app?: boolean
  user_agent?: string
}

export type CookieWindow = {
  /** Polls the page until it yields values. Resolves undefined when the window is
   *  gone or the wait runs out. */
  wait(params: HarvestParams, options?: { signal?: AbortSignal; timeoutMs?: number }): Promise<Record<string, string> | undefined>
  /** Loads another URL in the same window, for a restarted login step. */
  navigate(url: string): Promise<void>
  close(): void
}

export function browserPath(): string | undefined {
  return BROWSERS.find((path) => existsSync(path))
}

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms))

async function getJson<T>(url: string): Promise<T | undefined> {
  try {
    const response = await fetch(url, { signal: AbortSignal.timeout(2000) })
    if (!response.ok) return undefined
    return (await response.json()) as T
  } catch {
    return undefined
  }
}

/** Port Chrome picked, which it writes into the profile once it is listening. */
async function waitForPort(profile: string, timeoutMs = 15000): Promise<number | undefined> {
  const file = join(profile, "DevToolsActivePort")
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    try {
      const port = Number(readFileSync(file, "utf8").split("\n")[0])
      if (Number.isInteger(port) && port > 0) return port
    } catch {}
    await sleep(150)
  }
  return undefined
}

type Target = { id: string; type?: string; url: string; webSocketDebuggerUrl: string }

async function waitForPage(port: number, timeoutMs = 15000): Promise<Target | undefined> {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    const targets = await getJson<Target[]>(`http://127.0.0.1:${port}/json/list`)
    // Chrome lists component extensions too, and their background pages come
    // first; only real tabs carry the page we loaded.
    const page = targets?.find(
      (target) =>
        target.webSocketDebuggerUrl &&
        (target.type ?? "page") === "page" &&
        !target.url.startsWith("chrome-extension://") &&
        !target.url.startsWith("devtools://"),
    )
    if (page) return page
    await sleep(200)
  }
  return undefined
}

/** Values the page reported: an object, or a JSON string that parses to one. */
function parseValues(raw: unknown): Record<string, string> {
  if (raw == null) return {}
  let value: unknown = raw
  if (typeof value === "string") {
    const text = value.trim()
    if (!text.startsWith("{")) return {}
    try {
      value = JSON.parse(text)
    } catch {
      return {}
    }
  }
  if (typeof value !== "object" || Array.isArray(value)) return {}
  return Object.fromEntries(
    Object.entries(value as Record<string, unknown>)
      .filter(([, item]) => item !== undefined && item !== null && item !== "")
      .map(([name, item]) => [name, String(item)]),
  )
}

/**
 * Opens the window. Returns undefined when no browser is installed or the window
 * never came up.
 */
export async function openCookieWindow(url: string, options: HarvestOptions = {}): Promise<CookieWindow | undefined> {
  const browser = browserPath()
  if (!browser) return undefined

  const profile = mkdtempSync(join(tmpdir(), "hub-cookies-"))
  // An app window has no tabs or address bar, so the login reads as part of the
  // tool rather than a browser session. Headless steps get no window at all.
  const target = options.headless ? url : options.app ? `--app=${url}` : url
  const args = [
    `--user-data-dir=${profile}`,
    "--no-first-run",
    "--no-default-browser-check",
    "--remote-debugging-port=0",
    ...(options.headless ? ["--headless=new"] : []),
    ...(!options.headless && options.app ? ["--window-size=1020,840"] : []),
    ...(options.user_agent ? [`--user-agent=${options.user_agent}`] : []),
    target,
  ]

  let child: ChildProcess | undefined
  let socket: WebSocket | undefined
  let closed = false
  const shutdown = () => {
    if (closed) return
    closed = true
    try {
      socket?.close()
    } catch {}
    try {
      child?.kill()
    } catch {}
    // Chrome writes locks inside the profile; failing to remove it is harmless.
    try {
      rmSync(profile, { recursive: true, force: true })
    } catch {}
  }

  try {
    child = spawn(browser, args, { stdio: ["ignore", "ignore", "ignore"] })
    const port = await waitForPort(profile)
    if (!port) {
      shutdown()
      return undefined
    }
    const page = await waitForPage(port)
    if (!page) {
      shutdown()
      return undefined
    }

    socket = new WebSocket(page.webSocketDebuggerUrl)
    await new Promise<void>((resolve, reject) => {
      if (!socket) return reject(new Error("no socket"))
      socket.onopen = () => resolve()
      socket.onerror = () => reject(new Error("DevTools socket failed"))
      socket.onclose = () => {
        closed = true
      }
    })

    let nextId = 1
    const send = (method: string, params: unknown, timeoutMs: number) =>
      new Promise<any>((resolve) => {
        const id = nextId++
        // A pending promise from the page (extract_js may not resolve until the
        // fields exist) must not stall the poll, and a dead window must not hang
        // it either, so every call gives up early and the next tick tries again.
        const timer = setTimeout(() => {
          socket?.removeEventListener("message", onMessage)
          resolve(undefined)
        }, timeoutMs)
        function onMessage(event: MessageEvent) {
          const data = JSON.parse(String(event.data)) as { id: number; result?: any }
          if (data.id !== id) return
          clearTimeout(timer)
          socket?.removeEventListener("message", onMessage)
          resolve(data.result)
        }
        socket?.addEventListener("message", onMessage)
        socket?.send(JSON.stringify({ id, method, params }))
      })

    const evaluate = (expression: string) =>
      send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true }, 3000).then(
        (result) => result?.result?.value as unknown,
      )

    // Headers lifted off the page's own requests, by the name the step asked for.
    // Network is enabled once per window; the listener ignores the responses the
    // send() helper is waiting on, which all carry an id.
    let capture: HarvestParams["capture"]
    const captured = new Map<string, string>()
    socket.addEventListener("message", (event) => {
      if (!capture) return
      let data: any
      try {
        data = JSON.parse(String(event.data))
      } catch {
        return
      }
      if (data.method !== "Network.requestWillBeSent") return
      const request = data.params?.request ?? {}
      if (typeof request.url !== "string" || !request.url.includes(capture.url_contains)) return
      const headers: Record<string, string> = request.headers ?? {}
      const wanted = capture.header.toLowerCase()
      for (const [name, value] of Object.entries(headers)) {
        if (name.toLowerCase() === wanted && typeof value === "string" && value) {
          captured.set(capture.as, value)
          return
        }
      }
    })
    await send("Network.enable", {}, 3000)

    /** Cookies for the site, host-only and domain-wide alike, including the
     *  httpOnly ones document.cookie cannot see. */
    const jar = async (pageUrl: string): Promise<Record<string, string>> => {
      const urls = new Set<string>()
      try {
        const parsed = new URL(pageUrl)
        urls.add(parsed.origin + "/")
        if (parsed.hostname.startsWith("www.")) urls.add(`${parsed.protocol}//${parsed.hostname.slice(4)}/`)
      } catch {}
      const result = await send("Network.getCookies", { urls: [...urls] }, 3000)
      const cookies: Array<{ name?: string; value?: string }> = result?.cookies ?? []
      return Object.fromEntries(
        cookies
          .filter((cookie) => cookie.name && cookie.value !== undefined && cookie.value !== "")
          .map((cookie) => [String(cookie.name), String(cookie.value)]),
      )
    }

    return {
      async wait(params, waitOptions = {}) {
        capture = params.capture
        const deadline = Date.now() + (waitOptions.timeoutMs ?? 10 * 60 * 1000)
        const pattern = params.wait_for_url_pattern ? new RegExp(params.wait_for_url_pattern, "i") : undefined
        while (Date.now() < deadline) {
          if (closed || waitOptions.signal?.aborted) return undefined
          const href = await evaluate("location.href")
          if (closed) return undefined
          const cookieJar = await jar(params.url)
          const visible = parseValues(await evaluate(COOKIE_EXPRESSION))
          const extracted = params.extract_js ? parseValues(await evaluate(params.extract_js)) : {}
          const lifted = Object.fromEntries(captured)
          const all = { ...cookieJar, ...visible, ...extracted, ...lifted }
          // A login page already holds cookies (csrftoken, mid, ig_did…), so a
          // non-empty set means nothing on its own: the step's own signals are what
          // say the login actually landed. Submitting too early is what leaves an
          // account reconnecting forever.
          //
          // A step may name several sources (a page value and a lifted header);
          // any one of them landing is enough, but at least one must.
          const named = params.names && params.names.length > 0
          const haveNamed = !named || params.names!.every((name) => name in all)
          const sources = [
            params.capture ? params.capture.as in lifted : undefined,
            params.extract_js ? Object.keys(extracted).length > 0 : undefined,
          ].filter((ok) => ok !== undefined)
          const sourceOk = sources.length === 0 || sources.some(Boolean)
          const landed = !pattern || (typeof href === "string" && pattern.test(href))
          if (haveNamed && sourceOk && landed && Object.keys(all).length > 0) {
            // Send exactly the cookies the step named, and the whole set otherwise.
            return named
              ? Object.fromEntries(Object.entries(all).filter(([name]) => params.names!.includes(name)))
              : all
          }
          await sleep(800)
        }
        return undefined
      },
      async navigate(next) {
        await send("Page.navigate", { url: next }, 3000)
      },
      close: shutdown,
    }
  } catch {
    shutdown()
    return undefined
  }
}

/** One-shot helper: open, wait once, close. Used by tests. */
export async function harvestCookies(
  params: HarvestParams,
  options: HarvestOptions = {},
): Promise<Record<string, string> | undefined> {
  const window = await openCookieWindow(params.url, options)
  if (!window) return undefined
  try {
    return await window.wait(params, { signal: options.signal, timeoutMs: options.timeoutMs })
  } finally {
    window.close()
  }
}
