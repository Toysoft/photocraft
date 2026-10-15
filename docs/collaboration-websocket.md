# PhotoCraft Collaboration WebSocket

The collaboration relay is started with:

```bash
photocraft-cli collab-server --bind 0.0.0.0:7879 --control-token-file /run/photocraft/collab.token
```

For production, terminate TLS at a reverse proxy and keep the token out of command history.
The relay does not transmit pixel buffers. Clients remain responsible for applying engine commands.

## Authentication

The first text frame must be:

```json
{"type":"auth","token":"...","project":"campaign-42","user":"Mona"}
```

The server replies with `auth.ok` and a `room.snapshot` containing the room revision, current
participants, and the bounded event log.

## Client messages

- `presence`: `{ "type":"presence", "cursor":[x,y], "layer":12, "status":"editing" }`
- `propose`: `{ "type":"propose", "command":"filter.blur.gaussianBlur", "params":{}, "summary":"...", "baseRevision":4 }`
- `review`: `{ "type":"review", "request":7, "accepted":true }`
- `command`: `{ "type":"command", "command":"layer.new.layer", "params":{}, "baseRevision":4 }`
- `ping`: `{ "type":"ping" }`

Accepted commands increment the authoritative room revision and are broadcast as a `command`
event. A stale `baseRevision` receives `conflict` and is not broadcast, so clients must refresh or
rebase before retrying. Review requests are broadcast as `change.proposed` and decisions as
`change.reviewed`.

## Desktop client

The native egui client is integrated in the **History › Live collaboration** card. Expand
**Network connection**, enter the WebSocket URL and the same token used by the server, then press
**Connect to server**. The UI keeps network I/O on a worker thread, applies incoming `command`
events through `Session::execute_remote`, and sends locally executed engine commands with the
current `baseRevision`. Incoming commands are not echoed back to the room.

The transport intentionally does not execute arbitrary commands on the server or serialize full
documents. Clients remain responsible for applying commands and saving documents normally. This
keeps the engine as the single source of truth for document semantics and avoids sending pixel
buffers through the collaboration daemon.
