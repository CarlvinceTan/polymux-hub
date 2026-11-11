// The hub command line: the six messaging operations plus bridge maintenance.
// Every command is a single short-lived process that reads current state and
// exits, so nothing has to stay running except Tuwunel and the bridges.
//
// Connecting a new bridge is not here: that is the bridge's own login flow.
import { readFileSync } from "node:fs"
import { BRIDGES, isInstalled, rows, sortRows, summaryLine } from "./hub.ts"
import { checkUpdate, installBridge, updateBridge } from "./install.ts"
import { formatLink, install as installSkills, PACK, status as skillStatus, uninstall as uninstallSkills } from "./skills.ts"
import { connections, markRead, messages, rooms, send, unread } from "./ops.ts"

const args = process.argv.slice(2)
const command = args[0] ?? "status"
const progress = (message: string) => console.log(`  ${message}`)
const wantsJSON = args.includes("--json")
const positional = args.slice(1).filter((arg) => !arg.startsWith("--"))

/** Reads `--flag value` from the arguments, ignoring position. */
function flag(name: string): string | undefined {
  const index = args.indexOf(`--${name}`)
  return index >= 0 ? args[index + 1] : undefined
}

function emit(text: string, data: unknown): void {
  console.log(wantsJSON ? JSON.stringify(data, null, 2) : text)
}

function usage(): void {
  console.log(
    [
      "usage: hub <command> [options]",
      "",
      "Messaging (stateless, one process per call):",
      "  connections [--json]                 platform and account status",
      "  unread [--json]                      unread conversations, never marking them read",
      "  rooms [query] [--json]               joined conversations, optional name filter",
      "  messages <room_id> [--limit N] [--json]   recent window, newest last",
      "  mark-read <room_id> --requested-by <ref> [--event-id ID]",
      "                                       mark one conversation read; requires the user's",
      "                                       explicit request, recorded in --requested-by",
      "  send <room_id> --approved-by <ref> [--reply-to ID] [--text TEXT]",
      "                                       send exact text (stdin when --text is absent);",
      "                                       requires the user's approval, recorded in --approved-by",
      "",
      "Bridge maintenance:",
      "  status                               what is installed and whether it is up",
      "  check                                which bridges have a newer mautrix release",
      "  update [platform]                    rebuild and restart bridges that are behind",
      "  install platform                     build a bridge that is not installed yet",
      "  skills status|install|uninstall      where the agent skill pack is linked",
      "",
      `platforms: ${BRIDGES.map((spec) => spec.platform).join(", ")}`,
    ].join("\n"),
  )
}

switch (command) {
  case "connections": {
    const report = await connections()
    emit(
      [`connected ${report.connected}/${report.total}`, ...report.bridges.map((row) => summaryLine(row))].join("\n"),
      report,
    )
    break
  }

  case "unread": {
    const list = await unread()
    emit(
      list.length ? list.map((room) => `${String(room.count).padStart(2)}  ${room.name}\t${room.room_id}`).join("\n") : "Nothing unread.",
      list,
    )
    break
  }

  case "rooms": {
    const list = await rooms(positional[0])
    emit(list.map((room) => `${room.name}\t${room.room_id}`).join("\n") || "No rooms matched.", list)
    break
  }

  case "messages": {
    const roomID = positional[0]
    if (!roomID) {
      usage()
      process.exit(1)
    }
    const limit = Number(flag("limit") ?? "20")
    const list = await messages(roomID, Number.isFinite(limit) ? limit : 20)
    emit(
      list.length
        ? list.map((message) => `${message.timestamp}  ${message.event_id}  ${message.sender}  ${message.body}`).join("\n")
        : "No messages.",
      list,
    )
    break
  }

  case "mark-read": {
    const roomID = positional[0]
    const reference = flag("requested-by")
    if (!roomID || !reference) {
      usage()
      process.exit(1)
    }
    const result = await markRead(roomID, flag("event-id"), true, reference)
    emit(`marked ${result.room_id} read up to ${result.event_id}`, result)
    break
  }

  case "send": {
    const roomID = positional[0]
    const reference = flag("approved-by")
    if (!roomID || !reference) {
      usage()
      process.exit(1)
    }
    // The body comes from stdin unless --text is given, so quoting never has to
    // survive the shell.
    const inline = flag("text")
    const text = inline ?? readFileSync(0, "utf8").replace(/\n$/, "")
    if (!text) {
      console.error("hub: refusing to send an empty message")
      process.exit(1)
    }
    const result = await send(roomID, text, true, reference, flag("reply-to"))
    emit(`sent to ${result.room_id} (event ${result.event_id})`, result)
    break
  }

  case "status": {
    const list = sortRows(await rows())
    for (const row of list) console.log(summaryLine(row))
    console.log(`${list.filter((row) => row.up > 0).length}/${list.length} connected`)
    break
  }

  case "check": {
    for (const spec of BRIDGES) {
      if (!spec.repo || !isInstalled(spec.platform)) continue
      const status = await checkUpdate(spec).catch((error) => ({ behind: false, reason: String(error) }))
      console.log(
        `${spec.label}: ${
          status.behind ? `behind, ${status.current} -> ${status.latest}` : status.reason ? `check failed (${status.reason})` : "up to date"
        }`,
      )
    }
    break
  }

  case "update": {
    const only = positional[0]
    let changed = 0
    for (const spec of BRIDGES) {
      if (only && spec.platform !== only) continue
      if (!spec.repo || !isInstalled(spec.platform)) continue
      const status = await checkUpdate(spec).catch((error) => ({ behind: false, reason: String(error) }))
      if (!status.behind) {
        console.log(`${spec.label}: ${status.reason ? `check failed (${status.reason})` : "up to date"}`)
        continue
      }
      const result = await updateBridge(spec, progress)
      console.log(`${spec.label}: ${result.ok ? result.summary : `FAILED: ${result.error}`}`)
      if (result.ok) changed += 1
    }
    console.log(`\n${changed} bridge(s) updated.`)
    break
  }

  case "install": {
    const platform = positional[0]
    const spec = BRIDGES.find((candidate) => candidate.platform === platform)
    if (!spec) {
      usage()
      process.exit(1)
    }
    const result = await installBridge(spec, progress)
    console.log(`${spec.label}: ${result.ok ? result.summary : `FAILED: ${result.error}`}`)
    if (!result.ok) process.exit(1)
    break
  }

  case "skills": {
    const sub = args[1] ?? "status"
    if (!["status", "install", "update", "uninstall"].includes(sub)) {
      usage()
      process.exit(1)
    }
    console.log(`Local skill pack: ${PACK}`)
    if (sub === "status") {
      for (const link of skillStatus()) console.log(formatLink(link))
      break
    }
    const outcomes = sub === "uninstall" ? uninstallSkills() : installSkills()
    for (const outcome of outcomes) {
      console.log(`${formatLink(outcome.link)}${outcome.note ? ` (${outcome.note})` : ""}`)
    }
    if (outcomes.some((outcome) => !outcome.ok)) process.exit(1)
    break
  }

  default:
    usage()
    process.exit(1)
}
