# whatsapp-sync

A permanent WhatsApp archive in PostgreSQL, and a read-only MCP server over it.

`whatsapp-client` is a receive-only WhatsApp linked device written from scratch in Rust: it
holds the Noise transport, the Signal sessions and the group sender keys itself, decrypts
what arrives, writes it to Postgres, and acknowledges it only once it is stored.
`whatsapp-mcp` serves that archive to an MCP client. Both binaries ship in one image of
about the size of the binaries alone.

The client can adopt an existing [WAHA](https://waha.devlike.pro) / Baileys session on its
first start (`WHATSAPP_IMPORT_FROM`), so moving off WAHA needs no re-pairing.

## What it is

| binary | role | database role |
|---|---|---|
| `whatsapp-client` | the linked device: decrypts, archives, acks, transcribes voice notes | `whatsapp_ingest` (DDL + writes) |
| `whatsapp-mcp` | serves the archive over MCP | `whatsapp_mcp` (read-only except aliases) |

- **The client is the only writer.** It applies the migrations; the MCP asserts the schema
  it was built against and refuses to start on drift.
- **Archive, then save the Signal state, then acknowledge.** A crash between the steps means
  WhatsApp re-delivers, and the upserts absorb it.
- **The device's keys live in schema `device`**, which the MCP role cannot read, and the MCP
  binary does not link the client at all (`tests/archive/isolation.rs`).
- **Rows are WAHA-compatible.** Message ids, chat ids and authors follow WAHA's own
  `buildMessageId` and its chat list, so an archive started under WAHA carries on unbroken.
- **Edits, deletions and reactions** are applied to the archived message: edits become
  revisions (including the ones current phones seal with the original message's secret),
  deletions mark the row, reactions are kept per person.

## Tools

`chats`, `messages`, `message`, `revisions`, `search`, `people`, `status`, `set-alias`.
Times render as ISO 8601 in UTC. Names resolve **alias → address-book name → pushName →
phone number → id**; `set-alias` records what someone is actually called.

Names are pinned by a test: MCPHub groups allow-list tools by name, so a rename silently
empties a curated tool set.

## Media

Metadata only (mime type, filename, duration, caption), rendered as `[image/jpeg "beach.jpg"]`.
**Voice notes** are fetched from WhatsApp's CDN, decrypted, and sent to a whisper.cpp server
(`WHATSAPP_WHISPER_URL`, its `/inference` endpoint, started with `--convert`); the text is
searchable with message bodies. If whisper or the CDN cannot be reached the note stays
pending and is retried; only audio that was read and refused is marked failed.

## Deploying

The image is published to `ghcr.io/rgryta/whatsapp-sync` on every version bump
(`tag.yml` → `image.yml`). `deploy/compose.yaml` shows the two services.

1. Provision the database:

   ```bash
   psql -h <postgres> -U postgres \
     -v ingest_pw="<ingest password>" -v mcp_pw="<mcp password>" \
     -f deploy/provision-whatsapp-db.sql
   ```

2. Put the secrets in `.env` beside the compose file (see `.env.example`).
3. Moving off WAHA: stop WAHA first — **WAHA and `whatsapp-client` are the same device and
   must never run at once** — keep a copy of its session folder, then start the client with
   that folder mounted read-only as `WHATSAPP_IMPORT_FROM`. Its log says `adopted the device`
   and `logged in`. The setting is ignored once a device is stored.

## Limitations

- **No history sync.** The client archives what arrives while it runs; WhatsApp re-delivers
  what queued while it was down. Older history is whatever was archived before.
- **Names do not refresh.** Group subjects and contact names already archived stay; chats
  first seen by the client have none until app-state sync is added. "Delete for me" is an
  app-state change too, so it is not seen.
- **No media contents.** Attachments are named, not fetched; voice notes are transcribed.
- **Receive-only.** It never sends a message.

## Licence

`crates/client` (the WhatsApp device, including the Signal code vendored from libsignal) is
AGPL-3.0-only. Everything else is MIT.
