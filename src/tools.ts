// Six messaging tools over the private Matrix homeserver. The structured work
// lives in ops.ts; this file only adapts it to the MCP text surface and keeps
// the approval gates in one place.
import type { ToolDefinition } from "./mcp.ts"
import { type BridgeRow, summaryLine } from "./hub.ts"
import { connections, markRead, messages, rooms, send, unread } from "./ops.ts"

const emptyInput = { type: "object", properties: {}, additionalProperties: false } as const

export const INSTRUCTIONS = [
  "The user's bridged messaging (WhatsApp, Telegram, Messenger, Instagram, LinkedIn, Discord) over a private Matrix",
  "homeserver. Reading mutates nothing. Sending and marking read need the",
  "user's explicit approval of the exact conversation or message, and marking read is never a side effect of reading.",
].join(" ")

export const tools: ToolDefinition[] = [
  {
    name: "connections",
    description:
      "Status of each bridged platform (WhatsApp, Telegram, Messenger, Instagram, LinkedIn, Discord), with the accounts connected to them. Safe, read-only.",
    inputSchema: emptyInput,
    run: async () => {
      const report = await connections()
      const lines = [`connected ${report.connected}/${report.total}`, ...report.bridges.map((row) => summaryLine(row as BridgeRow))]
      return lines.join("\n")
    },
  },

  {
    name: "unread",
    description: "Unread messages across all joined Matrix conversations, grouped by room. Does not mark anything read.",
    inputSchema: emptyInput,
    run: async () => {
      const list = await unread()
      return list.length ? list.map((room) => `${String(room.count).padStart(2)}  ${room.name}`).join("\n") : "Nothing unread."
    },
  },

  {
    name: "rooms",
    description: "List joined Matrix conversations (bridged chats included), optionally filtered by name.",
    inputSchema: {
      type: "object",
      properties: { query: { type: "string", description: "Case-insensitive name filter" } },
      additionalProperties: false,
    },
    run: async ({ query }: { query?: string }) => {
      const list = await rooms(query)
      return list.map((room) => `${room.name}\t${room.room_id}`).join("\n") || "No rooms matched."
    },
  },

  {
    name: "messages",
    description: "Read the most recent messages of one Matrix conversation. Bounded window; never the whole history.",
    inputSchema: {
      type: "object",
      properties: {
        room_id: { type: "string", description: "Exact Matrix room ID" },
        limit: { type: "number", description: "How many events to return (default 20, max 100)" },
      },
      required: ["room_id"],
      additionalProperties: false,
    },
    run: async ({ room_id, limit }: { room_id: string; limit?: number }) => {
      const list = await messages(room_id, limit)
      return list.length
        ? list.map((message) => `${message.timestamp}  ${message.sender}  ${message.body}`).join("\n")
        : "No messages."
    },
  },

  {
    name: "mark_read",
    description:
      "Mark one Matrix conversation read, clearing its unread state for the user. This changes state the user will see, so call it ONLY when the user explicitly asked for that conversation to be marked read; pass explicitly_requested_by_user=true and a short request_reference. Never call it on inference, as a tidy-up, or because you read the room.",
    inputSchema: {
      type: "object",
      properties: {
        room_id: { type: "string", description: "Exact Matrix room ID from hub_rooms" },
        event_id: { type: "string", description: "Optional event to read up to; defaults to the newest message" },
        explicitly_requested_by_user: {
          type: "boolean",
          description: "true only when the user explicitly asked for this conversation to be marked read",
        },
        request_reference: { type: "string", description: "Short reference to where the user asked for it" },
      },
      required: ["room_id", "explicitly_requested_by_user", "request_reference"],
      additionalProperties: false,
    },
    run: async ({
      room_id,
      event_id,
      explicitly_requested_by_user,
      request_reference,
    }: {
      room_id: string
      event_id?: string
      explicitly_requested_by_user: boolean
      request_reference: string
    }) => {
      const result = await markRead(room_id, event_id, explicitly_requested_by_user, request_reference)
      return `marked ${result.room_id} read up to ${result.event_id}`
    },
  },

  {
    name: "send",
    description:
      "Send one message to one exact Matrix conversation. Requires explicit user approval of the exact recipient and payload; pass approved_exact_payload=true and a short approval_reference. Never send on context alone.",
    inputSchema: {
      type: "object",
      properties: {
        room_id: { type: "string", description: "Exact Matrix room ID" },
        text: { type: "string", description: "Exact final message body" },
        approved_exact_payload: { type: "boolean", description: "true only after the user approved this exact recipient and payload" },
        approval_reference: { type: "string", description: "Short reference to where the user approved it" },
        reply_to_event_id: { type: "string", description: "Optional event ID to reply to" },
      },
      required: ["room_id", "text", "approved_exact_payload", "approval_reference"],
      additionalProperties: false,
    },
    run: async ({
      room_id,
      text,
      approved_exact_payload,
      approval_reference,
      reply_to_event_id,
    }: {
      room_id: string
      text: string
      approved_exact_payload: boolean
      approval_reference: string
      reply_to_event_id?: string
    }) => {
      const result = await send(room_id, text, approved_exact_payload, approval_reference, reply_to_event_id)
      return `sent to ${result.room_id} (event ${result.event_id})`
    },
  },
]
