// Shared hub logic: provisioning access, status rows, and the connect/disconnect
// flows. Used by both the server tools (server.ts) and the CLI menu (tui.ts).
// No JSX here so it can be imported from anywhere.

import { execFileSync } from "node:child_process"
import { existsSync, readFileSync } from "node:fs"
import { homedir } from "node:os"
import { openCookieWindow } from "./cookies.ts"
import type { StyledText } from "@opentui/core"

export const MXID = process.env.MATRIX_USER_ID ?? "@user:localhost"
export const HUB_ROOT = `${homedir()}/Library/Application Support/matrix-hub`
export const BRIDGE_ROOT = `${HUB_ROOT}/bridges`
export const HOMESERVER = process.env.MATRIX_HOMESERVER ?? "http://127.0.0.1:8008"
export const TOKEN_SERVICE = "Matrix Hub Codex Token"

export type BridgeSpec = {
  platform: string
  label: string
  /** Provisioning API port, for bridges that serve one. */
  port?: number
  /** Process to look for, for bridges that have no provisioning API. */
  process?: string
  /** GitHub repo under mautrix/ the bridge is built from. */
  repo?: string
  /** Go package to build inside that repo. */
  target?: string
  /** Binary name written into the bridge's own directory. */
  binary?: string
  /** Why there is no source to build, for bridges that are not mautrix binaries. */
  note?: string
}

// Every bridge this hub can host: the ones running here, plus the rest of the
// family so the menu can list what could be added. A bridge counts as installed
// once the homeserver has its registration (bridges/registrations/<name>.yaml),
// which is what the setup steps produce; the others show as not installed.
//
// Bridges are built from source, not pulled as containers: the published macOS
// binaries link against libolm, which no longer builds against a current clang,
// and end-to-bridge encryption is off here anyway. Building with `-tags nocrypto`
// gives a self-contained binary and keeps the whole hub outside Docker.
export const BRIDGES: BridgeSpec[] = [
  { platform: "whatsapp", label: "WhatsApp", port: 29318, repo: "whatsapp", target: "./cmd/mautrix-whatsapp", binary: "mautrix-whatsapp" },
  { platform: "telegram", label: "Telegram", port: 29317, repo: "telegram", target: "./cmd/mautrix-telegram", binary: "mautrix-telegram" },
  { platform: "discord", label: "Discord", port: 29334, repo: "discord", target: ".", binary: "mautrix-discord" },
  { platform: "messenger", label: "Messenger", port: 29319, repo: "meta", target: "./cmd/mautrix-meta", binary: "mautrix-meta" },
  {
    platform: "instagram",
    label: "Instagram",
    port: 29320,
    repo: "meta",
    target: "./cmd/mautrix-instagram",
    binary: "mautrix-instagram",
    note: "Instagram is the second binary in the mautrix-meta repo.",
  },
  { platform: "linkedin", label: "LinkedIn", port: 29341, repo: "linkedin", target: "./cmd/mautrix-linkedin", binary: "mautrix-linkedin" },
  {
    platform: "imessage",
    label: "iMessage",
    process: "mautrix-imessage",
    repo: "imessage",
    note: "iMessage runs as a native macOS bridge; it is not built by this hub.",
  },
  {
    platform: "wechat",
    label: "WeChat",
    process: "matrix-bridge.mjs",
    note: "WeChat is the hub's own bridge script, not a published bridge.",
  },
  { platform: "signal", label: "Signal", port: 29328, repo: "signal", target: "./cmd/mautrix-signal", binary: "mautrix-signal" },
  { platform: "googlechat", label: "Google Chat", port: 29338, repo: "googlechat", target: ".", binary: "mautrix-googlechat" },
  { platform: "slack", label: "Slack", port: 29335, repo: "slack", target: "./cmd/mautrix-slack", binary: "mautrix-slack" },
  { platform: "bluesky", label: "Bluesky", port: 29346, repo: "bluesky", target: "./cmd/mautrix-bluesky", binary: "mautrix-bluesky" },
  { platform: "twitter", label: "X (Twitter)", port: 29327, repo: "twitter", target: "./cmd/mautrix-twitter", binary: "mautrix-twitter" },
  { platform: "reddit", label: "Reddit", port: 29336, repo: "reddit", target: "./cmd/mautrix-reddit", binary: "mautrix-reddit" },
  { platform: "gmessages", label: "Google Messages", port: 29345, repo: "gmessages", target: "./cmd/mautrix-gmessages", binary: "mautrix-gmessages" },
]

// Markers are one glyph family so every row is the same size: a filled circle
// when the bridge is up, a hollow one when it is not. Which one it is, and the
// colour, come from bridgeState.
export const CONNECTED = "●"
export const ISSUE = "○"

/** A bridge is installed once the homeserver has its registration. */
export function isInstalled(platform: string): boolean {
  return existsSync(`${BRIDGE_ROOT}/registrations/${platform}.yaml`)
}

/** States where the bridge is working on being connected rather than broken. */
const TRANSIENT_STATES = new Set(["CONNECTING", "TRANSIENT_DISCONNECT"])

/** What the row means: the TUI paints these green/red/white/dim. */
export type BridgeState = "missing" | "down" | "empty" | "partial" | "connecting" | "ok"

export function bridgeState(row: BridgeRow): BridgeState {
  if (!row.installed) return "missing"
  if (!row.reachable) return "down"
  if (row.accounts === 0) return "empty"
  if (row.up === row.accounts) return "ok"
  // Nothing connected is only reconnecting, so nothing is wrong yet.
  if (row.up + row.transient === row.accounts) return "connecting"
  return "partial"
}

/** Filled when the bridge is up, hollow when it is not. This is what the text
 *  surfaces (tool output) show, since a colour cannot travel there. */
export function markerOf(row: BridgeRow): string {
  const state = bridgeState(row)
  return state === "missing" || state === "empty" || state === "connecting" ? ISSUE : CONNECTED
}

// Listing order: bridges needing attention first, then working ones, then set-up
// bridges with nothing connected, then the ones only listed as possible. Sorting
// is stable, so bridges keep the catalogue order inside a group.
const STATE_ORDER: Record<BridgeState, number> = { partial: 0, down: 0, ok: 1, connecting: 2, empty: 3, missing: 4 }

export function sortRows(list: BridgeRow[]): BridgeRow[] {
  return [...list].sort((a, b) => STATE_ORDER[bridgeState(a)] - STATE_ORDER[bridgeState(b)])
}

const STATE_LABEL: Record<string, string> = {
  CONNECTED: "connected",
  CONNECTING: "connecting",
  TRANSIENT_DISCONNECT: "reconnecting",
  BAD_CREDENTIALS: "needs re-login",
  UNKNOWN_ERROR: "error",
}

export function stateLabel(state?: string): string {
  return STATE_LABEL[state ?? ""] ?? state ?? "?"
}

export function sharedSecret(platform: string): string {
  const lines = readFileSync(`${BRIDGE_ROOT}/${platform}/config.yaml`, "utf8").split("\n")
  // The block is top level in most bridges and nested under appservice: in
  // others (mautrix-discord), so match it at any indentation.
  const start = lines.findIndex((line) => /^\s*provisioning:\s*$/.test(line))
  if (start < 0) return ""
  const indent = lines[start].match(/^\s*/)?.[0].length ?? 0
  for (let i = start + 1; i < lines.length && i < start + 12; i++) {
    const line = lines[i]
    const match = line.match(/^\s+shared_secret:\s*(\S+)/)
    if (match) return match[1] === "generate" || match[1] === "disable" || match[1] === "null" ? "" : match[1]
    if (line.trim() !== "" && (line.match(/^\s*/)?.[0].length ?? 0) <= indent) break
  }
  return ""
}

export async function provision(platform: string, path: string, init?: RequestInit): Promise<any> {
  const port = BRIDGES.find((b) => b.platform === platform)?.port
  if (!port) throw new Error(`unknown bridge ${platform}`)
  const secret = sharedSecret(platform)
  if (!secret) throw new Error("no provisioning secret in config.yaml")
  const headers = new Headers(init?.headers ?? {})
  headers.set("Authorization", `Bearer ${secret}`)
  if (init?.body && !headers.has("Content-Type")) headers.set("Content-Type", "application/json")
  const separator = path.includes("?") ? "&" : "?"
  const response = await fetch(
    `http://127.0.0.1:${port}/_matrix/provision/v3${path}${separator}user_id=${encodeURIComponent(MXID)}`,
    { ...init, headers },
  )
  const text = await response.text()
  let data: any
  try {
    data = text ? JSON.parse(text) : {}
  } catch {
    data = { raw: text }
  }
  if (!response.ok) throw new Error(`${response.status} ${data.error ?? text}`)
  return data
}

/**
 * mautrix-discord speaks the older v1 provisioning API (ping, login/token,
 * logout, reconnect) rather than the v3 login-step flow, so it gets its own
 * calls instead of going through provision().
 */
async function discordCall(path: string, init?: RequestInit): Promise<any> {
  const secret = sharedSecret("discord")
  if (!secret) throw new Error("no provisioning secret in config.yaml")
  const port = BRIDGES.find((spec) => spec.platform === "discord")?.port ?? 29334
  const separator = path.includes("?") ? "&" : "?"
  const headers = new Headers(init?.headers ?? {})
  headers.set("Authorization", `Bearer ${secret}`)
  if (init?.body && !headers.has("Content-Type")) headers.set("Content-Type", "application/json")
  const response = await fetch(
    `http://127.0.0.1:${port}/_matrix/provision${path}${separator}user_id=${encodeURIComponent(MXID)}`,
    { ...init, headers },
  )
  const text = await response.text()
  const data = text ? JSON.parse(text) : {}
  if (!response.ok) throw new Error(`${response.status} ${data.error ?? text}`)
  return data
}

export const discordStatus = () => discordCall("/v1/ping")
export const discordLogin = (token: string) =>
  discordCall("/v1/login/token", { method: "POST", body: JSON.stringify({ token }) })
export const discordLogout = () => discordCall("/v1/logout", { method: "POST" })
export const discordReconnect = () => discordCall("/v1/reconnect", { method: "POST" })

export type BridgeRow = {
  platform: string
  label: string
  account: string
  note: string
  /** Accounts currently connected, accounts known, and the ones that are only
   *  reconnecting, so callers never have to read state out of the glyph. */
  up: number
  accounts: number
  transient: number
  /** Set up on this hub, and answered when asked. */
  installed: boolean
  reachable: boolean
  info?: any
}

// Some bridges report a longer name than the platform needs — mautrix's
// Messenger says "Facebook Messenger" — so these are the names to show instead.
const LABEL_OVERRIDE: Record<string, string> = { messenger: "Messenger", instagram: "Instagram" }

/** A bridge that is either not set up here or not answering. */
function absentRow(spec: BridgeSpec, installed: boolean, note: string): BridgeRow {
  return {
    platform: spec.platform,
    label: LABEL_OVERRIDE[spec.platform] ?? spec.label,
    account: "—",
    note,
    up: 0,
    accounts: 0,
    transient: 0,
    installed,
    reachable: false,
  }
}

function rowFor(spec: BridgeSpec, info: any): BridgeRow {
  const logins: any[] = info?.logins ?? []
  const pretty = LABEL_OVERRIDE[spec.platform] ?? info?.network?.displayname ?? spec.label
  if (logins.length === 0) {
    return {
      platform: spec.platform,
      label: pretty,
      account: "no account",
      note: "not connected",
      up: 0,
      accounts: 0,
      transient: 0,
      installed: true,
      reachable: true,
      info,
    }
  }
  const states = logins.map((login) => login?.state?.state_event)
  const connected = states.filter((state) => state === "CONNECTED").length
  const reconnecting = states.filter((state) => TRANSIENT_STATES.has(state)).length
  const account = logins.map((login) => login?.name ?? login?.id).join(", ")
  const reason = logins.find((login) => login?.state?.state_event !== "CONNECTED")?.state?.message
  const note =
    connected === states.length
      ? "connected"
      : `${connected}/${states.length} connected · ${
          reason ??
          states
            .filter((state) => state !== "CONNECTED")
            .map((state) => stateLabel(state))
            .join(", ")
        }`
  return {
    platform: spec.platform,
    label: pretty,
    account,
    note,
    up: connected,
    accounts: states.length,
    transient: reconnecting,
    installed: true,
    reachable: true,
    info,
  }
}

/** Native and hand-rolled bridges have no provisioning API to ask, so the running
 *  process is the signal. */
function processRunning(pattern: string): boolean {
  try {
    execFileSync("/usr/bin/pgrep", ["-f", pattern], { stdio: ["ignore", "pipe", "ignore"] })
    return true
  } catch {
    return false
  }
}

/** Whether anything is listening on a bridge port. */
async function portAnswers(port: number): Promise<boolean> {
  try {
    await fetch(`http://127.0.0.1:${port}/`, { signal: AbortSignal.timeout(1500) })
    return true
  } catch {
    return false
  }
}

export async function rows(): Promise<BridgeRow[]> {
  return Promise.all(
    BRIDGES.map(async (spec) => {
      if (!isInstalled(spec.platform)) return absentRow(spec, false, "not installed")
      if (spec.process) {
        const running = processRunning(spec.process)
        return {
          ...absentRow(spec, true, running ? "running" : "not running"),
          account: "bridge",
          up: running ? 1 : 0,
          accounts: 1,
          reachable: running,
        }
      }
      if (spec.platform === "discord") {
        try {
          const ping = await discordStatus()
          const status = ping?.Discord ?? {}
          const loggedIn = Boolean(status.logged_in)
          const connected = Boolean(status.connected)
          return {
            ...absentRow(spec, true, connected ? "connected" : loggedIn ? "reconnecting" : "not logged in"),
            account: loggedIn ? String(status.id ?? "discord") : "no account",
            up: connected ? 1 : 0,
            accounts: loggedIn ? 1 : 0,
            transient: loggedIn && !connected ? 1 : 0,
            reachable: true,
            info: ping,
          }
        } catch (error) {
          const message = (error as Error).message.replace(/^\d+\s*/, "").trim()
          return absentRow(spec, true, message.slice(0, 60))
        }
      }
      try {
        return rowFor(spec, await provision(spec.platform, "/whoami"))
      } catch (error) {
        const message = (error as Error).message.replace(/^\d+\s*/, "").trim()
        // A bridge can be up without a provisioning API (Discord's config has
        // none), so its port answering still counts as reachable.
        if (spec.port && /no provisioning secret|unknown bridge|404|not found|disabled/i.test(message)) {
          const up = await portAnswers(spec.port)
          return { ...absentRow(spec, true, up ? "running · no provisioning API" : "not answering"), reachable: up }
        }
        return absentRow(spec, true, message.slice(0, 60))
      }
    }),
  )
}

export function summaryLine(row: BridgeRow): string {
  const extra = bridgeState(row) !== "ok" && row.note ? `  ·  ${row.note}` : ""
  return `${markerOf(row)} ${row.label.padEnd(20)} ${row.account}${extra}`
}

// ---- Matrix room helpers (unread) -------------------------------------------

export function readMatrixToken(): string {
  return execFileSync("/usr/bin/security", ["find-generic-password", "-a", MXID, "-s", TOKEN_SERVICE, "-w"], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "ignore"],
  }).trim()
}

export async function matrix(path: string, init?: RequestInit): Promise<any> {
  const headers = new Headers(init?.headers ?? {})
  if (!headers.has("Authorization")) headers.set("Authorization", `Bearer ${readMatrixToken()}`)
  const response = await fetch(`${HOMESERVER}${path}`, { ...init, headers })
  const text = await response.text()
  if (!response.ok) throw new Error(`Matrix ${response.status}`)
  return text ? JSON.parse(text) : {}
}

export async function unreadLines(): Promise<string[]> {
  const result = await matrix("/_matrix/client/v3/notifications?limit=100")
  const counts = new Map<string, number>()
  for (const notification of result.notifications ?? []) {
    const event = notification.event ?? {}
    if (notification.read !== false || event.type !== "m.room.message" || event.sender === MXID) continue
    counts.set(notification.room_id, (counts.get(notification.room_id) ?? 0) + 1)
  }
  return Promise.all(
    [...counts.entries()].map(async ([room, count]) => {
      let name = room
      try {
        const state = await matrix(`/_matrix/client/v3/rooms/${encodeURIComponent(room)}/state/m.room.name`)
        if (state?.name) name = state.name
      } catch {}
      return `${String(count).padStart(2)}  ${name}`
    }),
  )
}

/** The newest event id in a room, or "" when the room has no events yet. */
async function latestEventId(roomId: string): Promise<string> {
  const result = await matrix(`/_matrix/client/v3/rooms/${encodeURIComponent(roomId)}/messages?dir=b&limit=1`)
  return result.chunk?.[0]?.event_id ?? ""
}

/**
 * Mark one conversation read, up to an event. With no event id this reads to the
 * newest message, which is what "mark this conversation read" means. Both markers
 * are set: `m.read` carries the receipt to other clients, `m.fully_read` is the
 * one that actually clears the room's unread state in the user's own client.
 * Returns the event that was marked, so the caller can report it.
 */
export async function markRoomRead(roomId: string, eventId?: string): Promise<string> {
  const target = eventId?.trim() || (await latestEventId(roomId))
  if (!target) throw new Error(`no event to mark read in ${roomId}`)
  await matrix(`/_matrix/client/v3/rooms/${encodeURIComponent(roomId)}/read_markers`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ "m.read": target, "m.fully_read": target }),
  })
  return target
}

// ---- actions ----------------------------------------------------------------

/** Single-line, width-limited text for dialogs: they clip, they do not wrap. */
export function clip(value: string, width: number): string {
  const text = value.replace(/\s+/g, " ").trim()
  return text.length > width ? `${text.slice(0, width - 1)}…` : text
}

/**
 * Terminal QR rendering, injected by the TUI half of the plugin so the server
 * half never depends on the encoder (see qr.ts). mautrix sends the raw payload
 * for a QR step, so the art has to be produced on this side.
 */
export type RenderQR = (data: string) => Promise<{ art?: string | StyledText; cells?: number; error?: string }>

/** A dialog body, plus how many cells wide the code is, so the caller can pick a
 *  dialog size that fits it. */
type DialogBody = { message: string | StyledText; cells: number }

/** What a display_and_wait step asks the user to look at: the thing itself, with
 *  no prose — the dialog title already says which bridge this is. */
async function displayBody(display: any, renderQR?: RenderQR): Promise<DialogBody> {
  const type = display?.type
  if (type === "qr") {
    const { art, cells, error } = display.data && renderQR ? await renderQR(display.data) : {}
    if (art) return { message: art, cells: cells ?? 0 }
    const lines: string[] = []
    if (!renderQR) lines.push("(no QR renderer wired)")
    if (display.image_url) lines.push(display.image_url)
    if (display.data) lines.push(display.data)
    if (error) lines.push(`(no terminal QR: ${clip(error, 60)})`)
    return { message: lines.join("\n") || "Nothing to display.", cells: 0 }
  }
  if (type === "emoji" && display.data) return { message: display.data, cells: 0 }
  if (type === "code" && display.data) return { message: display.data, cells: 0 }
  return { message: "Waiting for the login to finish…", cells: 0 }
}

/** Names of the fields a step asks for. Bridges do not all shape these the same
 *  (the meta bridge sends its own wording instead of the documented objects), so
 *  anything unnameable is left out rather than printed as "undefined". */
function fieldLabels(fields: any[]): string {
  return fields
    .map((field) => {
      if (typeof field === "string") return field
      const name = field?.name ?? field?.id
      if (typeof name !== "string" || name === "") return undefined
      return typeof field?.type === "string" && field.type !== "cookie" ? `${name} (${field.type})` : name
    })
    .filter((label): label is string => Boolean(label))
    .join(", ")
}

type CookiesOutcome = { kind: "step"; step: any } | { kind: "failed"; reason: string }

/**
 * Runs a cookies step through one browser window: opens it on the URL the bridge
 * gave, waits for the login, posts the cookies. The window is never reloaded or
 * reopened — a rejected submission restarts the login and the same window keeps
 * waiting — so the only way out is escaping the dialog.
 */
async function cookiesLogin(ctx: any, platform: string, flowId: string, step: any, params: any): Promise<CookiesOutcome> {
  const url = typeof params.url === "string" ? params.url : ""
  const hidden = params.hidden === true
  const fields: unknown[] = Array.isArray(params.fields) ? params.fields : []
  const names = fields
    .map((field: any) => (typeof field === "string" ? field : field?.name ?? field?.id))
    .filter((name): name is string => typeof name === "string" && name !== "")

  const window = await openCookieWindow(url, {
    headless: hidden,
    app: !hidden,
    user_agent: typeof params.user_agent === "string" ? params.user_agent : undefined,
  })
  if (!window) return { kind: "failed", reason: "no Chromium browser is available to log in with" }

  let current = step
  let currentParams = params
  try {
    for (;;) {
      const controller = new AbortController()
      const waiting = window.wait(
        {
          url,
          extract_js: typeof currentParams.extract_js === "string" ? currentParams.extract_js : undefined,
          wait_for_url_pattern:
            typeof currentParams.wait_for_url_pattern === "string" ? currentParams.wait_for_url_pattern : undefined,
          names: names.length > 0 ? names : undefined,
        },
        { signal: controller.signal },
      )
      const dialog = ctx.ui.dialog.alert({
        title: `Connect ${platform}`,
        message: hidden ? "Reading cookies…" : "Opening window…",
      })
      const outcome = await Promise.race([waiting.then(() => "done" as const), dialog.then(() => "cancel" as const)])
      if (outcome === "cancel") {
        controller.abort()
        void waiting.catch(() => {})
        ctx.ui.dialog.clear()
        return { kind: "failed", reason: "cancelled" }
      }
      ctx.ui.dialog.clear()

      const values = await waiting
      if (!values) {
        // The window is gone, so there is nothing left to wait on. No new window:
        // that would throw away whatever the user was doing in it.
        return { kind: "failed", reason: "the login window was closed before the login was read" }
      }

      try {
        const next = await provision(
          platform,
          `/login/step/${encodeURIComponent(current.login_id)}/${encodeURIComponent(current.step_id)}/cookies?txn_id=${encodeURIComponent(current.txn_id ?? "")}`,
          { method: "POST", body: JSON.stringify(values) },
        )
        return { kind: "step", step: next }
      } catch {
        // The bridge discarded that login process, so a new one starts — in the
        // same window, which is pointed at the new step's URL if it differs.
        const restarted = await provision(platform, `/login/start/${encodeURIComponent(flowId)}`, {
          method: "POST",
        }).catch(() => undefined)
        if (!restarted) return { kind: "failed", reason: "the bridge would not start a new login" }
        current = restarted
        currentParams = restarted.cookies ?? currentParams
        const nextUrl = typeof currentParams.url === "string" ? currentParams.url : url
        if (nextUrl !== url) await window.navigate(nextUrl)
      }
    }
  } finally {
    window.close()
  }
}

/**
 * Discord's connect flow. mautrix-discord is the one bridge still on the legacy
 * v1 provisioning API, which has no login-flow endpoints at all: the bridge
 * wants a user token.
 *
 * So instead of asking for that token, this opens the real Discord web app in the
 * throwaway Chromium window the cookie bridges already use, waits for the login
 * to land, and reads the session token out of the page. A pasted token is only
 * the fallback, for when no browser is installed or the page will not give one up.
 */
const DISCORD_LOGIN_URL = "https://discord.com/login"

/** Reads the logged-in Discord session. The web app keeps the token in
 *  localStorage, and newer builds moved it behind the module that mints it, so
 *  both are tried. Returns {} while there is nothing to read yet. */
const DISCORD_TOKEN_JS = `(() => {
  try {
    const raw = localStorage.getItem("token")
    if (raw) {
      try {
        const parsed = JSON.parse(raw)
        if (typeof parsed === "string" && parsed) return { token: parsed }
      } catch {
        if (raw) return { token: raw }
      }
    }
  } catch {}
  try {
    let found = ""
    window.webpackChunkdiscord_app.push([["hub"], {}, (module) => {
      for (const candidate of Object.values(module.c)) {
        const token = candidate && candidate.exports && candidate.exports.default && candidate.exports.default.getToken
          ? candidate.exports.default.getToken()
          : undefined
        if (typeof token === "string" && token) { found = token; break }
      }
    }])
    if (found) return { token: found }
  } catch {}
  return {}
})()`

async function finishDiscordLogin(ctx: any, token: string): Promise<void> {
  try {
    await discordLogin(token)
    await ctx.ui.dialog.alert({ title: "Connect Discord", message: "Connected." })
  } catch (error) {
    await ctx.ui.dialog.alert({ title: "Connect Discord", message: `Failed: ${(error as Error).message}` })
  }
}

/** The fallback: paste a token, for a host with no browser to log in with. */
async function startDiscordTokenLogin(ctx: any): Promise<void> {
  const token = await ctx.ui.dialog.prompt({
    title: "Connect Discord",
    description: "Discord user token",
    placeholder: "paste the token",
  })
  if (token === undefined) return
  if (!token.trim()) {
    await ctx.ui.dialog.alert({ title: "Connect Discord", message: "No token given." })
    return
  }
  await finishDiscordLogin(ctx, token.trim())
}

async function startDiscordLogin(ctx: any): Promise<void> {
  const window = await openCookieWindow(DISCORD_LOGIN_URL, { app: true })
  if (!window) return startDiscordTokenLogin(ctx)

  const controller = new AbortController()
  try {
    // Discord lands on /channels/@me once signed in, and from then on the page
    // can be read. Two sources, whichever lands first: the Authorization header
    // the web client signs its own API calls with, and the token the page keeps
    // for itself. Both have to be readable before the value counts.
    const waiting = window.wait(
      {
        url: DISCORD_LOGIN_URL,
        capture: { header: "Authorization", url_contains: "discord.com/api", as: "token" },
        extract_js: DISCORD_TOKEN_JS,
        wait_for_url_pattern: "discord\\.com/channels",
      },
      { signal: controller.signal, timeoutMs: 5 * 60 * 1000 },
    )
    const dialog = ctx.ui.dialog.alert({
      title: "Connect Discord",
      message: "Log in to Discord in the window that just opened.\nThe hub reads the session as soon as you are signed in.",
    })
    const raced = await Promise.race([
      waiting.then((values) => ({ kind: "values" as const, values })),
      dialog.then(() => ({ kind: "dismissed" as const })),
    ])
    if (raced.kind === "dismissed") {
      controller.abort()
      void waiting.catch(() => {})
      ctx.ui.dialog.clear()
      return
    }
    // The login landed, or the window gave up: either way its job is done, so it
    // closes now rather than waiting behind the result dialog. The finally below
    // is the backstop for the paths that throw.
    window.close()
    ctx.ui.dialog.clear()
    if (!raced.values?.token) {
      // No session came out of the window, so offer the token instead.
      await startDiscordTokenLogin(ctx)
      return
    }
    await finishDiscordLogin(ctx, raced.values.token)
  } finally {
    window.close()
  }
}

export async function startLogin(ctx: any, platform: string, renderQR?: RenderQR): Promise<void> {
  // Discord has no v3 login flows to drive, so it gets its own one-step flow.
  if (platform === "discord") return startDiscordLogin(ctx)

  // A QR needs more room than the other steps, and once the user has dismissed a
  // code dialog the outcome is reported without opening another one.
  let dismissed = false
  const finish = async (message: string) => {
    ctx.ui.dialog.set({ size: "medium", centered: false })
    if (dismissed) {
      ctx.ui.toast.show({ title: `Connect ${platform}`, message })
      return
    }
    await ctx.ui.dialog.alert({ title: `Connect ${platform}`, message })
  }
  try {
    const info = await provision(platform, "/whoami")
    const flows: any[] = info?.login_flows ?? []
    if (flows.length === 0) {
      await ctx.ui.dialog.alert({ title: platform, message: "This bridge exposes no login flows." })
      return
    }
    const flowId =
      flows.length === 1
        ? flows[0].id
        : await ctx.ui.dialog.select({
            title: `Connect ${info?.network?.displayname ?? platform}`,
            options: flows.map((flow) => ({ title: flow.name ?? flow.id, value: flow.id, description: clip(flow.description ?? "", 60) })),
          })
    if (!flowId) return

    let step = await provision(platform, `/login/start/${encodeURIComponent(flowId)}`, { method: "POST" })
    // The dialog currently on screen, kept across iterations so a refresh can
    // replace it in place instead of closing and reopening it.
    let onScreen: { key: string; closed: Promise<"back" | "hidden"> } | undefined
    for (let guard = 0; guard < 25; guard++) {
      if (step?.type === "complete") {
        await finish("Connected.")
        return
      }
      if (step?.type === "display_and_wait") {
        const display = step.display_and_wait ?? {}
        // The wait starts as the code appears: the step endpoint blocks until the
        // bridge has something new (a rotated QR, or completion), so nothing asks
        // the user to confirm the step. Escape and the Back button both leave the
        // login, the way every other step backs out; ok just hides the code.
        const key = `${display.type}:${display.data ?? display.image_url ?? ""}`
        const waiting = provision(
          platform,
          `/login/step/${encodeURIComponent(step.login_id)}/${encodeURIComponent(step.step_id)}/display_and_wait?txn_id=${encodeURIComponent(step.txn_id ?? "")}`,
          { method: "POST" },
        )
        let outcome: "progress" | "back" | "hidden" = "progress"
        if (!dismissed) {
          // A refresh replaces the dialog in place, so it does not blink away and
          // back; a payload already on screen is left alone.
          if (onScreen?.key !== key) {
            const { message, cells } = await displayBody(display, renderQR)
            const dialog = ctx.ui.dialog.confirm({
              title: `Connect ${platform}`,
              message: message as unknown as string,
              label: { confirm: "ok", cancel: "Back" },
            })
            // Dialog widths are fixed (medium 60, large 88, xlarge 116) with two
            // cells of padding each side, so a code picks the smallest that fits
            // it instead of always asking for the widest.
            const size = cells === 0 ? "medium" : cells <= 56 ? "medium" : cells <= 84 ? "large" : "xlarge"
            ctx.ui.dialog.set({ size, centered: cells > 0 })
            onScreen = {
              key,
              // Escape and Back both leave the login and go back, the way every
              // other step does; ok only hides the code and lets the wait carry
              // on in the background.
              closed: dialog.then((choice) => (choice === true ? ("hidden" as const) : ("back" as const))),
            }
          }
          outcome = await Promise.race([waiting.then(() => "progress" as const), onScreen.closed])
        }
        if (outcome === "back") {
          void waiting.catch(() => {})
          onScreen = undefined
          await provision(platform, `/login/cancel/${encodeURIComponent(step.login_id)}`, { method: "POST" }).catch(
            () => {},
          )
          ctx.ui.dialog.set({ size: "medium", centered: false })
          ctx.ui.dialog.clear()
          return
        }
        if (outcome === "hidden") {
          dismissed = true
          onScreen = undefined
        }
        step = await waiting
        continue
      }
      if (step?.type === "cookies") {
        // Cookie steps want a browser session's cookies. The bridge hands over the
        // login URL, so this opens a window on it and watches the page until the
        // cookies are there, retrying until the login actually takes.
        const outcome = await cookiesLogin(ctx, platform, flowId, step, step.cookies ?? {})
        if (outcome.kind === "failed") {
          // Cancelling is not a failure worth reporting.
          if (outcome.reason !== "cancelled") await finish(`Could not log in: ${outcome.reason}.`)
          return
        }
        step = outcome.step
        continue
      }

      if (step?.type === "user_input") {
        const body: Record<string, string> = {}
        for (const field of step.user_input?.fields ?? []) {
          const options: string[] = field.options ?? []
          const value =
            field.type === "select" && options.length > 0
              ? await ctx.ui.dialog.select({
                  title: `${platform}: ${field.name}`,
                  options: options.map((option) => ({ title: option, value: option })),
                  current: field.default_value,
                })
              : await ctx.ui.dialog.prompt({
                  title: `${platform}: ${field.name}`,
                  description: field.description,
                  placeholder: clip(field.description ?? field.type, 60),
                  value: field.default_value,
                })
          if (value === undefined) return
          body[field.id] = value
        }
        step = await provision(
          platform,
          `/login/step/${encodeURIComponent(step.login_id)}/${encodeURIComponent(step.step_id)}/user_input?txn_id=${encodeURIComponent(step.txn_id ?? "")}`,
          { method: "POST", body: JSON.stringify(body) },
        )
        continue
      }
      await finish(`Unsupported login step: ${step?.type}`)
      return
    }
    await finish("Login did not finish; giving up.")
  } catch (error) {
    await finish(`Failed: ${(error as Error).message}`)
  }
}

export async function disconnect(ctx: any, platform: string, current?: BridgeRow): Promise<void> {
  const label = current?.label ?? platform
  // Discord's only session is the one v1 login, so logging out is one call.
  if (platform === "discord") {
    const confirmed = await ctx.ui.dialog.confirm({
      title: `Disconnect ${label}`,
      message: "Log the Discord account out of the bridge?",
      label: { confirm: "Log out", cancel: "Cancel" },
    })
    if (!confirmed) return
    try {
      await discordLogout()
      await ctx.ui.dialog.alert({ title: label, message: "Logged out." })
    } catch (error) {
      await ctx.ui.dialog.alert({ title: label, message: `Failed: ${(error as Error).message}` })
    }
    return
  }

  const info = current?.info ?? (await provision(platform, "/whoami").catch(() => null))
  const logins: any[] = info?.logins ?? []
  if (logins.length === 0) {
    await ctx.ui.dialog.alert({ title: label, message: "Nothing connected." })
    return
  }
  const chosen =
    logins.length === 1
      ? logins[0].id
      : await ctx.ui.dialog.select({
          title: `Disconnect ${label}`,
          options: logins.map((login) => ({
            title: `${login.state?.state_event === "CONNECTED" ? CONNECTED : ISSUE} ${login.name ?? login.id}`,
            value: login.id,
            description: stateLabel(login?.state?.state_event),
          })),
        })
  if (!chosen) return
  const name = logins.find((login) => login.id === chosen)?.name ?? chosen
  const confirmed = await ctx.ui.dialog.confirm({
    title: `Disconnect ${label}`,
    message: `Log out ${name}?`,
    label: { confirm: "Log out", cancel: "Cancel" },
  })
  if (!confirmed) return
  try {
    await provision(platform, `/logout/${encodeURIComponent(chosen)}`, { method: "POST" })
    await ctx.ui.dialog.alert({ title: label, message: `Logged out ${name}.` })
  } catch (error) {
    await ctx.ui.dialog.alert({ title: label, message: `Failed: ${(error as Error).message}` })
  }
}
