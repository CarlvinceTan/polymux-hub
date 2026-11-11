// The six messaging operations as structured data, shared by the CLI and the
// MCP server. Reading preserves unread state; marking read and sending are
// gated by explicit-approval arguments, not by anything the caller can infer.
import { MXID, bridgeState, markRoomRead, matrix, rows, sortRows, type BridgeRow, type BridgeState } from "./hub.ts"

export type Connection = {
  platform: string
  label: string
  account: string
  note: string
  up: number
  accounts: number
  transient: number
  installed: boolean
  reachable: boolean
  state: BridgeState
}

export type UnreadRoom = { room_id: string; name: string; count: number }
export type Room = { room_id: string; name: string }
export type Message = { event_id: string; timestamp: string; sender: string; body: string }

async function roomName(roomID: string): Promise<string> {
  try {
    const state = await matrix(`/_matrix/client/v3/rooms/${encodeURIComponent(roomID)}/state/m.room.name`)
    return state?.name ?? roomID
  } catch {
    return roomID
  }
}

export async function connections(): Promise<{ connected: number; total: number; bridges: Connection[] }> {
  const list: BridgeRow[] = sortRows(await rows())
  return {
    connected: list.filter((row) => row.up > 0).length,
    total: list.length,
    bridges: list.map((row) => ({
      platform: row.platform,
      label: row.label,
      account: row.account,
      note: row.note,
      up: row.up,
      accounts: row.accounts,
      transient: row.transient,
      installed: row.installed,
      reachable: row.reachable,
      state: bridgeState(row),
    })),
  }
}

export async function unread(): Promise<UnreadRoom[]> {
  const result = await matrix("/_matrix/client/v3/notifications?limit=100")
  const counts = new Map<string, number>()
  for (const notification of result.notifications ?? []) {
    const event = notification.event ?? {}
    if (notification.read !== false || event.type !== "m.room.message" || event.sender === MXID) continue
    counts.set(notification.room_id, (counts.get(notification.room_id) ?? 0) + 1)
  }
  return Promise.all(
    [...counts.entries()].map(async ([room_id, count]) => ({ room_id, name: await roomName(room_id), count })),
  )
}

export async function rooms(query?: string): Promise<Room[]> {
  const joined = await matrix("/_matrix/client/v3/joined_rooms")
  const ids: string[] = joined.joined_rooms ?? []
  const names = await Promise.all(ids.slice(0, 120).map(async (id) => ({ room_id: id, name: await roomName(id) })))
  const needle = (query ?? "").trim().toLowerCase()
  return needle ? names.filter((room) => room.name.toLowerCase().includes(needle)) : names
}

export async function messages(roomID: string, limit?: number): Promise<Message[]> {
  const size = Math.min(Math.max(1, limit ?? 20), 100)
  const result = await matrix(`/_matrix/client/v3/rooms/${encodeURIComponent(roomID)}/messages?dir=b&limit=${size}`)
  return (result.chunk ?? [])
    .filter((event: any) => event.type === "m.room.message")
    .map((event: any) => ({
      event_id: event.event_id,
      timestamp: new Date(event.origin_server_ts).toISOString(),
      sender: event.sender,
      body: event.content?.body ?? "",
    }))
    .reverse()
}

/** Refuses unless the user explicitly asked for this conversation to be read. */
export async function markRead(
  roomID: string,
  eventID: string | undefined,
  explicitlyRequestedByUser: boolean,
  requestReference: string,
): Promise<{ room_id: string; event_id: string }> {
  if (explicitlyRequestedByUser !== true || !requestReference?.trim()) {
    throw new Error("refused: the user must have explicitly asked for this conversation to be read")
  }
  return { room_id: roomID, event_id: await markRoomRead(roomID, eventID) }
}

/** Refuses unless the exact recipient and payload were approved. */
export async function send(
  roomID: string,
  text: string,
  approvedExactPayload: boolean,
  approvalReference: string,
  replyToEventID?: string,
): Promise<{ room_id: string; event_id: string }> {
  if (approvedExactPayload !== true || !approvalReference?.trim()) {
    throw new Error("refused: exact recipient/payload approval is required")
  }
  const body: Record<string, unknown> = { msgtype: "m.text", body: text }
  if (replyToEventID) body["m.relates_to"] = { "m.in_reply_to": { event_id: replyToEventID } }
  const txn = `${Date.now()}-${Math.random().toString(36).slice(2)}`
  const result = await matrix(`/_matrix/client/v3/rooms/${encodeURIComponent(roomID)}/send/m.room.message/${txn}`, {
    method: "PUT",
    body: JSON.stringify(body),
    headers: { "Content-Type": "application/json" },
  })
  return { room_id: roomID, event_id: result.event_id }
}
