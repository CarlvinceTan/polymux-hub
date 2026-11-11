---
name: hub
description: "Read and send bridged messaging on WhatsApp, Telegram, Messenger, Instagram, LinkedIn and Discord with the stateless `hub` CLI over a private Matrix homeserver. Use Himalaya separately for email."
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
