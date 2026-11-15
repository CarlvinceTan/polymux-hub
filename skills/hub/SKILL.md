---
name: hub
description: "Read, search and send bridged messaging on WhatsApp, Telegram, Messenger, Instagram, LinkedIn and Discord with the `hub` CLI over a private Matrix homeserver. Search finds messages across every account by words or meaning, including photos, voice notes and videos. Use when the user mentions something someone sent or said in a chat (\"the address Sam sent\", \"that photo of the lease\", \"what time did they say\"), before asking them for details that are likely in their messages, and to read, reply to or catch up on conversations. Use Himalaya separately for email."
---

# Hub

Messaging runs through the private Matrix homeserver, and the `hub` CLI is a
stateless wrapper over it: every command is one short-lived process that talks
to Tuwunel on localhost and exits. Email belongs to the Himalaya CLI skill.

## Route

```sh
hub connections                 # platform and account status
hub unread                      # unread conversations, without marking them read
hub rooms [query]               # find conversations; resolve the exact room ID
hub messages <room_id>          # bounded recent window, newest last (add --limit N)
hub search WORDS...             # every conversation, every account: exact words first, then meaning
hub media <event_id>            # save a result's photo, video or voice note to look at
hub index [--status]            # fill or refresh the local search index
hub mark-read <room_id> --requested-by "where the user asked"
hub send <room_id> --approved-by "where the user approved"
```

Add `--json` to any read command for structured output. `messages` prints
`timestamp  event_id  sender  body`, so the event id is there when a reply is
needed. For `send`, the body is read from stdin unless `--text` is given, which
keeps quoting and newlines out of the shell:

```sh
printf '%s' "on my way" | hub send '!room:id' --approved-by "user said send it"
```

## Search

`hub search` looks through every conversation on every bridged account at once,
including messages from long ago, without opening any of them as read:

```sh
hub search when is the dinner booking          # a question in plain words
hub search "flight number" --room Mum --days 30
hub search photo of the apartment lease         # finds photos and videos by what they show
```

Reach for it whenever an answer is probably in the user's messages (a booking,
an address, a time, a link, a photo someone shared) instead of asking them; a
plain-words description of a photo or video finds it by what it shows.

Messages containing every word come first; then messages that mean the same
(marked `≈`), embedded with EmbeddingGemma 2 so photos, voice notes and videos are
found by what is in them, not only by their captions. Results show time,
conversation, sender and text, with `[image]`, `[audio]` or `[video]` for media.
The user's own messages show as `me` (`"from_me": true` in `--json`), and in a
private chat the other person shows as the chat's name. Media results end with
`(hub media <event_id>)`: run it to save the file under `~/.cache/hub/media`, then
open the path with the read tool to check what it shows before answering (a
video also gets a `frames` sheet, one image of eight frames). Use
`hub messages <room_id>` around a result for the surrounding conversation.

The index is local (`~/.local/share/hub/index.db`) and needs the local embedding
server on `127.0.0.1:8277` for meaning and media (`HUB_EMBED_URL` to change it;
without it, search uses exact words only). Each search copies in recent messages
first; `hub index` does a full pass, and the first one copies all history.

## Authority

Reading preserves unread state. Never mark read as a side effect or tidy-up.
`mark-read` and `send` require the user's explicit approval and refuse without
`--requested-by`/`--approved-by`, but those flags record a decision the user
already made; they do not create it. A request naming the platform/account, the
exact recipient and the final text, and saying to send, is sufficient; otherwise
show the final message and wait. Never retry an uncertain send before checking
what landed with `hub messages`.

Keep messages short and match the user's style for the recipient. Report only
observed results: `send` confirms the homeserver accepted the event, not that
the other platform delivered it.

## Bridge maintenance

The same CLI carries the maintenance commands: `hub status`, `hub check`,
`hub update [platform]`, `hub install <platform>`, `hub skills status`. Connecting
an account uses the bridge's own login flow.
