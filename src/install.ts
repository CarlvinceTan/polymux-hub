// Installing a bridge into the hub, natively.
//
// There is no container runtime in this hub: a bridge is a Go binary built from
// its mautrix repo with `-tags nocrypto` (the published macOS binaries link
// libolm, which no longer compiles against a current clang, and end-to-bridge
// encryption is off here), run by launchd beside Tuwunel.
//
// The steps: build the binary, let it write its config, point that config at the
// local Tuwunel and the bridge's own port, generate the registration, copy it
// where Tuwunel reads appservices from, install a launchd unit, then restart
// Tuwunel so it picks the registration up. Everything a bridge owns is written
// under its own directory; every file the hub edits is backed up first.

import { spawn, type ChildProcess } from "node:child_process"
import { createConnection } from "node:net"
import { existsSync, copyFileSync, mkdirSync, readFileSync, writeFileSync } from "node:fs"
import { homedir, tmpdir } from "node:os"
import { join, resolve, dirname } from "node:path"
import { fileURLToPath } from "node:url"
import { BRIDGE_ROOT, BRIDGES, HOMESERVER, HUB_ROOT, MXID, isInstalled, type BridgeSpec } from "./hub.ts"

export type Progress = (message: string) => void
/** Plain shape on purpose: the plugin is compiled with whatever settings the
 *  host uses, and optional fields behave the same in all of them. */
export type InstallResult = { ok: boolean; summary?: string; error?: string; finished: string[] }

const REGISTRATIONS = join(BRIDGE_ROOT, "registrations")
const LAUNCH_AGENTS = join(homedir(), "Library", "LaunchAgents")
const TUWUNEL_LABEL = "com.carlvince.matrix-hub.tuwunel"
const LOGCAP_LABEL = "com.carlvince.matrix-hub.logcap"
const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..")
const LOGCAP_SOURCE = join(ROOT, "bin", "hub-log-cap")
const LOGCAP_SCRIPT = join(HUB_ROOT, "bin", "hub-log-cap")
const buildRoot = join(tmpdir(), "matrix-hub-build")

const tail = (text: string, lines = 2) =>
  text
    .trim()
    .split("\n")
    .filter((line) => line.trim() !== "")
    .slice(-lines)
    .join(" ")
    .slice(0, 200)

/** Async on purpose: a bridge build takes minutes and must not freeze the TUI. */
function run(command: string, args: string[], options: { cwd?: string; timeoutMs?: number } = {}): Promise<string> {
  return new Promise((resolve, reject) => {
    let stdout = ""
    let stderr = ""
    let timedOut = false
    let child: ChildProcess | undefined
    try {
      child = spawn(command, args, { cwd: options.cwd ?? HUB_ROOT, env: { ...process.env } })
    } catch (error) {
      reject(error)
      return
    }
    const timer = setTimeout(
      () => {
        timedOut = true
        try {
          child?.kill()
        } catch {}
      },
      options.timeoutMs ?? 10 * 60 * 1000,
    )
    child.stdout?.on("data", (chunk) => (stdout += String(chunk)))
    child.stderr?.on("data", (chunk) => (stderr += String(chunk)))
    child.on("error", (error) => {
      clearTimeout(timer)
      reject(error)
    })
    child.on("close", (code) => {
      clearTimeout(timer)
      if (timedOut) return reject(new Error(`${command} ${args[0] ?? ""} timed out`))
      if (code !== 0) return reject(new Error(`${command} ${args[0] ?? ""} failed: ${tail(stderr || stdout) || `exit ${code}`}`))
      resolve(stdout)
    })
  })
}

/** Same naming the hub already uses for its own backups. */
function backup(file: string): string {
  const stamp = new Date().toISOString().replace(/[-:]|\.\d+/g, "").replace("T", "-").slice(0, 15)
  const target = `${file}.bak-${stamp}`
  copyFileSync(file, target)
  return target
}

/** Sets a scalar inside a top-level block, leaving the rest of the file alone. */
export function setBlockValue(text: string, block: string, key: string, value: string, indent = "    "): string {
  const lines = text.split("\n")
  const start = lines.findIndex((line) => line === `${block}:` || line.startsWith(`${block}:`))
  if (start < 0) return `${text.replace(/\s*$/, "")}\n\n${block}:\n${indent}${key}: ${value}\n`
  for (let index = start + 1; index < lines.length; index++) {
    const line = lines[index]
    if (line.trim() !== "" && !line.startsWith(" ") && !line.startsWith("#")) break
    if (new RegExp(`^${indent}${key}:(\\s|$)`).test(line)) {
      lines[index] = `${indent}${key}: ${value}`
      return lines.join("\n")
    }
  }
  lines.splice(start + 1, 0, `${indent}${key}: ${value}`)
  return lines.join("\n")
}

/** Replaces a nested block's body, e.g. bridge.permissions. */
export function setBlockBody(text: string, block: string, key: string, body: string[], indent = "    "): string {
  const lines = text.split("\n")
  const start = lines.findIndex((line) => line === `${block}:` || line.startsWith(`${block}:`))
  if (start < 0) return `${text.replace(/\s*$/, "")}\n\n${block}:\n${indent}${key}:\n${body.map((line) => `${indent}    ${line}`).join("\n")}\n`
  const at = lines.findIndex((line, index) => index > start && new RegExp(`^${indent}${key}:(\\s|$)`).test(line))
  if (at < 0) {
    lines.splice(start + 1, 0, `${indent}${key}:`, ...body.map((line) => `${indent}    ${line}`))
    return lines.join("\n")
  }
  let end = at + 1
  while (end < lines.length && (lines[end].startsWith(indent + " ") || lines[end].trim() === "")) end++
  lines.splice(at, end - at, `${indent}${key}:`, ...body.map((line) => `${indent}    ${line}`))
  return lines.join("\n")
}

/** Sets a top-level scalar, used for the registration's url. */
export function setTopValue(text: string, key: string, value: string): string {
  const lines = text.split("\n")
  const at = lines.findIndex((line) => new RegExp(`^${key}:(\\s|$)`).test(line))
  if (at < 0) return `${text.replace(/\s*$/, "")}\n${key}: ${value}\n`
  lines[at] = `${key}: ${value}`
  return lines.join("\n")
}

function randomSecret(): string {
  const bytes = new Uint8Array(32)
  crypto.getRandomValues(bytes)
  return Array.from(bytes, (byte) => byte.toString(36).padStart(2, "0")).join("").slice(0, 48)
}

/** Whether anything is listening on the port, including bridges outside the hub. */
function portInUse(port: number): Promise<boolean> {
  return new Promise((resolve) => {
    const socket = createConnection({ host: "127.0.0.1", port })
    const done = (inUse: boolean) => {
      socket.destroy()
      resolve(inUse)
    }
    socket.once("connect", () => done(true))
    socket.once("error", () => done(false))
    socket.setTimeout(400, () => done(false))
  })
}

/** Free port for the appservice: the catalogue's default when it is unused, and
 *  otherwise the first free one. */
async function pickPort(spec: BridgeSpec): Promise<number> {
  const taken = new Set(BRIDGES.map((bridge) => bridge.port).filter((port): port is number => typeof port === "number"))
  taken.delete(spec.port ?? -1)
  const usable = async (port: number) => !taken.has(port) && !(await portInUse(port))
  if (spec.port && (await usable(spec.port))) return spec.port
  for (let port = 29350; port < 29450; port++) {
    if (await usable(port)) return port
  }
  throw new Error("no free port for the bridge")
}

/** The newest release tag of a bridge repo. */
async function latestTag(repo: string): Promise<string | undefined> {
  try {
    const response = await fetch(`https://api.github.com/repos/mautrix/${repo}/releases/latest`, {
      headers: { Accept: "application/vnd.github+json", "User-Agent": "opencode-hub" },
    })
    if (!response.ok) return undefined
    const data = (await response.json()) as { tag_name?: string }
    return data.tag_name
  } catch {
    return undefined
  }
}

/** The tag a bridge's binary was built from, if the hub installed it. */
function builtTag(platform: string): string | undefined {
  try {
    return readFileSync(join(BRIDGE_ROOT, platform, ".bridge-version"), "utf8").trim() || undefined
  } catch {
    return undefined
  }
}

/**
 * Builds one bridge from source and installs the binary in its own directory.
 * `-tags nocrypto` is deliberate: the hub runs unencrypted rooms and the pure-Go
 * build has no libolm dependency, so the binary stands alone.
 */
async function buildBridge(spec: BridgeSpec, dir: string, tag: string, progress: Progress): Promise<void> {
  const repo = spec.repo
  if (!repo || !spec.target || !spec.binary) throw new Error(`no mautrix source is known for ${spec.label}`)
  const source = join(buildRoot, repo)
  mkdirSync(buildRoot, { recursive: true })
  if (existsSync(join(source, ".git"))) {
    progress(`Fetching ${spec.label} ${tag}…`)
    await run("git", ["fetch", "--depth", "1", "origin", "tag", tag], { cwd: source, timeoutMs: 5 * 60 * 1000 }).catch(
      async () => {
        await run("git", ["fetch", "--depth", "1", "origin"], { cwd: source, timeoutMs: 5 * 60 * 1000 })
      },
    )
    await run("git", ["checkout", "--force", tag], { cwd: source, timeoutMs: 2 * 60 * 1000 })
  } else {
    progress(`Cloning ${spec.label} ${tag}…`)
    await run("git", ["clone", "--depth", "1", "--branch", tag, `https://github.com/mautrix/${repo}.git`, source], {
      timeoutMs: 10 * 60 * 1000,
    })
  }
  progress(`Building ${spec.label} (this takes a minute)…`)
  const output = join(dir, spec.binary)
  try {
    await run("go", ["build", "-tags", "nocrypto", "-o", output, spec.target], { cwd: source, timeoutMs: 20 * 60 * 1000 })
  } catch (error) {
    // The legacy repos build from the root package and have no cmd/ layout.
    if (spec.target === ".") throw error
    await run("go", ["build", "-tags", "nocrypto", "-o", output, "."], { cwd: source, timeoutMs: 20 * 60 * 1000 })
  }
}

/** The launchd unit that keeps a bridge running. */
function bridgePlist(spec: BridgeSpec, dir: string): string {
  const binary = join(dir, spec.binary ?? spec.platform)
  return `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.carlvince.matrix-hub.${spec.platform}</string>
    <key>ProgramArguments</key>
    <array>
        <string>${binary}</string>
        <string>-c</string>
        <string>${join(dir, "config.yaml")}</string>
        <string>-r</string>
        <string>${join(dir, "registration.yaml")}</string>
    </array>
    <key>WorkingDirectory</key>
    <string>${dir}</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>StandardOutPath</key>
    <string>${join(dir, "runtime.log")}</string>
    <key>StandardErrorPath</key>
    <string>${join(dir, "runtime.log")}</string>
</dict>
</plist>
`
}

/** Installs the unit and starts (or restarts) the bridge under launchd. */
async function startBridge(spec: BridgeSpec, dir: string): Promise<void> {
  const label = `com.carlvince.matrix-hub.${spec.platform}`
  const plist = join(LAUNCH_AGENTS, `${label}.plist`)
  mkdirSync(LAUNCH_AGENTS, { recursive: true })
  writeFileSync(plist, bridgePlist(spec, dir))
  const domain = `gui/${process.getuid?.() ?? 501}`
  await run("launchctl", ["bootout", domain, plist]).catch(() => "")
  await run("launchctl", ["bootstrap", domain, plist])
  await ensureLogCap()
}

/**
 * The launchd timer that trims bridge logs. launchd writes each bridge's
 * stdout and stderr straight to runtime.log and never rotates it, so a bridge
 * stuck reconnecting can fill the disk: the log-cap script keeps only a tail of
 * any log over the cap. Set up beside the bridges, so it survives reinstalls.
 */
function logCapPlist(): string {
  return `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${LOGCAP_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>/bin/sh</string>
        <string>${LOGCAP_SCRIPT}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>StartInterval</key>
    <integer>60</integer>
    <key>ProcessType</key>
    <string>Background</string>
    <key>StandardOutPath</key>
    <string>${join(HUB_ROOT, "logs", "log-cap.log")}</string>
    <key>StandardErrorPath</key>
    <string>${join(HUB_ROOT, "logs", "log-cap.log")}</string>
</dict>
</plist>
`
}

/** Installs the log-cap timer. A failure here must not fail a bridge install. */
async function ensureLogCap(): Promise<void> {
  try {
    mkdirSync(join(HUB_ROOT, "bin"), { recursive: true })
    mkdirSync(join(HUB_ROOT, "logs"), { recursive: true })
    copyFileSync(LOGCAP_SOURCE, LOGCAP_SCRIPT)
    const plist = join(LAUNCH_AGENTS, `${LOGCAP_LABEL}.plist`)
    writeFileSync(plist, logCapPlist())
    const domain = `gui/${process.getuid?.() ?? 501}`
    await run("launchctl", ["bootout", domain, plist]).catch(() => "")
    await run("launchctl", ["bootstrap", domain, plist]).catch(() => "")
  } catch {}
}

/** Tuwunel reads appservice_dir at startup, so a new registration needs a boot. */
async function restartHomeserver(): Promise<void> {
  const domain = `gui/${process.getuid?.() ?? 501}`
  await run("launchctl", ["kickstart", "-k", `${domain}/${TUWUNEL_LABEL}`])
}

/** Homeserver settings and permissions, taken from a bridge's own config. */
function hubSettings(): { address: string; domain: string; permissions: string[] } {
  const installed = BRIDGES.find((spec) => isInstalled(spec.platform))
  const config = installed ? readFileSync(join(BRIDGE_ROOT, installed.platform, "config.yaml"), "utf8") : ""
  const address = config.match(/^ {4}address: (\S+)/m)?.[1] ?? HOMESERVER
  const domain = config.match(/^ {4}domain: (\S+)/m)?.[1] ?? MXID.split(":").slice(1).join(":")
  return {
    address,
    domain,
    permissions: [`"*": relay`, `"${domain}": user`, `"${MXID}": admin`],
  }
}

export type UpdateStatus = {
  platform: string
  label: string
  current?: string
  latest?: string
  behind: boolean
  reason?: string
}

/** Whether a bridge is behind the newest mautrix release. */
export async function checkUpdate(spec: BridgeSpec): Promise<UpdateStatus> {
  const status: UpdateStatus = { platform: spec.platform, label: spec.label, behind: false }
  if (!spec.repo) return { ...status, reason: spec.note ?? "no source to update" }
  const current = builtTag(spec.platform)
  const latest = await latestTag(spec.repo)
  if (!latest) return { ...status, current, reason: "could not read the releases" }
  if (!current) return { ...status, current, latest, behind: false, reason: "not built by the hub" }
  return { ...status, current, latest, behind: current !== latest }
}

/** Rebuilds a bridge at the newest release and restarts it. */
export async function updateBridge(spec: BridgeSpec, progress: Progress): Promise<InstallResult> {
  const finished: string[] = []
  try {
    if (!spec.repo) throw new Error(spec.note ?? `no source is known for ${spec.label}`)
    const latest = await latestTag(spec.repo)
    if (!latest) throw new Error(`could not read mautrix/${spec.repo} from GitHub`)
    const current = builtTag(spec.platform)
    if (current === latest) return { ok: true, summary: `${spec.label} is already up to date (${latest}).`, finished }

    const dir = join(BRIDGE_ROOT, spec.platform)
    await buildBridge(spec, dir, latest, progress)
    writeFileSync(join(dir, ".bridge-version"), `${latest}\n`)
    finished.push("built")
    progress(`Restarting ${spec.label}…`)
    await startBridge(spec, dir)
    finished.push("restarted")
    return {
      ok: true,
      summary: `${spec.label} updated to ${latest}${current ? ` (was ${current})` : ""}.`,
      finished,
    }
  } catch (error) {
    return { ok: false, error: (error as Error).message, finished }
  }
}

/**
 * Installs one bridge: build it, generate its config and registration, point
 * them at this hub, hand the registration to Tuwunel, then start the bridge.
 * Nothing is started unless the config edits produced both files.
 */
export async function installBridge(spec: BridgeSpec, progress: Progress): Promise<InstallResult> {
  const finished: string[] = []
  try {
    if (isInstalled(spec.platform)) throw new Error(`${spec.label} is already installed`)
    if (!spec.repo || !spec.target || !spec.binary) throw new Error(`no mautrix source is known for ${spec.label}`)

    const port = await pickPort(spec)
    const dir = join(BRIDGE_ROOT, spec.platform)
    const hub = hubSettings()
    mkdirSync(dir, { recursive: true })

    const tag = (await latestTag(spec.repo)) ?? "main"
    await buildBridge(spec, dir, tag, progress)
    finished.push("built")
    const binary = join(dir, spec.binary)

    progress("Writing the bridge config…")
    const example = await run(binary, ["-e"]).catch(() => "")
    if (example.trim().startsWith("homeserver")) {
      writeFileSync(join(dir, "config.yaml"), example)
    } else if (!existsSync(join(dir, "config.yaml"))) {
      // Legacy repos ship example-config.yaml instead of an -e flag.
      copyFileSync(join(buildRoot, spec.repo, "example-config.yaml"), join(dir, "config.yaml"))
    }
    finished.push("config written")

    progress("Pointing the config at this hub…")
    let config = readFileSync(join(dir, "config.yaml"), "utf8")
    config = setBlockValue(config, "homeserver", "address", hub.address)
    config = setBlockValue(config, "homeserver", "domain", hub.domain)
    config = setBlockValue(config, "appservice", "address", `http://127.0.0.1:${port}`)
    config = setBlockValue(config, "appservice", "hostname", "127.0.0.1")
    config = setBlockValue(config, "appservice", "port", String(port))
    config = setBlockValue(config, "provisioning", "shared_secret", randomSecret())
    config = setBlockValue(config, "provisioning", "allow_matrix_auth", "true")
    config = setBlockValue(config, "encryption", "allow", "false")
    config = setBlockBody(config, "bridge", "permissions", hub.permissions)
    writeFileSync(join(dir, "config.yaml"), config)
    finished.push("config patched")

    progress("Generating the appservice registration…")
    await run(binary, ["-g", "-c", join(dir, "config.yaml"), "-r", join(dir, "registration.yaml")]).catch(async () => {
      if (!existsSync(join(dir, "registration.yaml"))) throw new Error("the bridge did not generate a registration")
    })
    const registration = setTopValue(readFileSync(join(dir, "registration.yaml"), "utf8"), "url", `http://127.0.0.1:${port}`)
    writeFileSync(join(dir, "registration.yaml"), registration)
    mkdirSync(REGISTRATIONS, { recursive: true })
    copyFileSync(join(dir, "registration.yaml"), join(REGISTRATIONS, `${spec.platform}.yaml`))
    finished.push("registration linked")

    progress("Starting the bridge…")
    await startBridge(spec, dir)
    finished.push("started")

    progress("Restarting the homeserver for the new registration…")
    await restartHomeserver()
    finished.push("homeserver restarted")
    writeFileSync(join(dir, ".bridge-version"), `${tag}\n`)

    return { ok: true, summary: `${spec.label} is installed on port ${port} and registered with Tuwunel.`, finished }
  } catch (error) {
    return { ok: false, error: (error as Error).message, finished }
  }
}
