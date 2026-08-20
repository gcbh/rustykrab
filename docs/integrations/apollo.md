# Apollo ↔ RustyKrab API contract

The wire contract between the RustyKrab gateway and the **Apollo iOS app**
(`gcbh/apollo-ios`). The conversation/message DTOs in
`crates/rustykrab-gateway/src/routes.rs` implement the **Implemented** parts
of this document; the sections marked **Planned** are specified by
`docs/plans/apollo-ios-and-credential-guard.md` and are normative for the
Phase 1 server work.

Conventions, all endpoints:

- Base URL: `https://<mac-hostname>.<tailnet>.ts.net` (Tailscale-served HTTPS
  in front of the loopback-only gateway at `127.0.0.1:3000`).
- JSON bodies, camelCase field names, timestamps as **epoch milliseconds**.
- Auth: `Authorization: Bearer <token>` on every `/api/*` route except
  `/api/health` (and, once pairing lands, `POST /api/pair`). The token is
  today the single `RUSTYKRAB_AUTH_TOKEN`; after Phase 1 it may equally be a
  per-device token issued by pairing.
- Errors are bare HTTP status codes (no error body): `401` unauthenticated,
  `404` not found, `409` conflict, `413` too large, `429` rate limited.
- Requests are rate-limited and origin-checked by gateway middleware;
  responses carry strict security headers.
- **`Origin` is mandatory on every `/api/*` request** (not just browser
  ones): the middleware rejects a missing `Origin` on sensitive paths with
  `403`, and today only loopback origins are allowed —
  `OriginPolicy::default()` starts with an empty allowlist. Apollo sends
  `Origin: <scheme>://<host>[:port]` of its base URL, so **Phase 1 server
  work must make the allowlist configurable** (the tailnet hostname needs
  to be in it) or every app request past `/api/health` gets a `403`.

## Health — Implemented

```
GET /api/health → 200 "ok"        (no auth)
```

## Conversations — Implemented

```
GET    /api/conversations               → [Conversation]
POST   /api/conversations {title?}      → Conversation
GET    /api/conversations/{id}          → Conversation
DELETE /api/conversations/{id}          → 204
```

```jsonc
// Conversation
{ "id": "uuid", "title": "string|null", "createdAt": 1704067200000, "updatedAt": 1704153600000 }
```

## Messages — Implemented

```
GET  /api/conversations/{id}/messages   → [Message]
POST /api/conversations/{id}/messages   {content} → Message | {message, apps}
POST /api/conversations/{id}/messages/stream {content} → SSE stream
```

```jsonc
// Message
{
  "id": "uuid",
  "conversationId": "uuid",
  "role": "user" | "assistant" | "system",   // internal `tool` role is coerced to `assistant`
  "content": "string",                        // tool calls render as "[tool_call:…]" text
  "createdAt": 1704067200000
}
```

- `content` in the send body is plain text, max **100 KB** (`413` beyond).
- The non-stream response is a bare `Message` today; the envelope form
  `{message, apps}` appears only if the agent produced app specs (it
  currently never does).
- Transcript replays (`GET …/messages`) include system/tool turns; clients
  typically filter to `user`/`assistant` before rendering.

### SSE stream events

Apollo must handle three event types and ignore everything else:

| event | data | meaning |
|-------|------|---------|
| `text` | `{"type":"text","delta":"…"}` | incremental assistant text |
| `done` | `{"type":"done","message":Message,"apps":[…]?}` | terminal; carries the full assistant message (`apps` omitted when empty) |
| `error` | `{"type":"error","message":"The agent is unavailable.","delta":"…"}` | terminal failure; the stream closes after it |

Frames the gateway also emits for its own WebChat UI — `tool_start`,
`tool_end`, `tool_error`, `tool_heartbeat`, `thinking`,
`user_message_queued`, and a bare `{"type":"done"}` progress frame — are
safe no-ops for Apollo (it may optionally surface tool names as a status
line). Keep-alive comment pings arrive every 15 s. If the connection drops
mid-stream with no `done`, Apollo synthesises an "agent unavailable" state
and refetches the transcript.

## Secrets — Implemented today, semantics change in Phase 1

Implemented today:

```
GET    /api/secrets          → {names: [string], keychain_available: bool}
POST   /api/secrets          {name, value, dest?: "store"|"keychain", service?, account?} → 204   // silently upserts
DELETE /api/secrets/{name}   → 204
```

- `value` max **64 KB**, non-empty. Secret **values are never returned** by
  any endpoint.
- `name`: 1–256 chars of `[A-Za-z0-9_.-]`.

**Planned (Phase 1) changes** — the create/overwrite split:

```
GET  /api/secrets   → {secrets: [{name, createdAt, updatedAt, version}], keychainAvailable}
POST /api/secrets   {name, value, overwrite?: false, dest?, service?, account?}
                    → 204, or 409 when the name exists and overwrite != true
```

Clients must send `overwrite: true` only after an explicit user confirmation
dialog — that flag *is* the user's approval for user-initiated overwrites.

## Credential change requests — Planned (Phase 1)

The agent cannot overwrite or delete an existing credential; its attempt
files a request the user resolves here.

```
GET  /api/credential-requests?status=pending → [ChangeRequest]
POST /api/credential-requests/{id}/approve   → 204   (409 if stale/expired)
POST /api/credential-requests/{id}/deny      → 204
```

```jsonc
// ChangeRequest — never includes the proposed value
{
  "id": "uuid",
  "name": "notion_api_token",
  "action": "update" | "delete",
  "reason": "string|null",            // agent-supplied justification
  "conversationId": "uuid|null",
  "status": "pending" | "approved" | "denied" | "expired",
  "createdAt": 1704067200000
}
```

- Requests expire after 7 days; a newer request for the same name supersedes
  the older one.
- `approve` applies the stored change server-side and is refused with `409`
  if the target secret changed after the request was filed.

## Device pairing — Planned (Phase 1)

```
POST   /api/pair {code, deviceName}  → {deviceId, deviceToken}   (no auth; rate-limited; code single-use, 5-min TTL)
GET    /api/devices                  → [{id, name, createdAt, lastSeenAt}]
DELETE /api/devices/{id}             → 204   (revocation)
POST   /api/devices/{id}/push-token {token} → 204   (Phase 3, APNs)
```

The pairing code is minted on the Mac (`rustykrab pair` CLI subcommand or the
WebChat "Pair device" button) and displayed as a QR payload:

```json
{"url": "https://<mac>.<tailnet>.ts.net", "code": "XXXXXXXX"}
```

`deviceToken` is shown exactly once and stored hashed server-side. It is
accepted anywhere the master bearer token is, and identifies the device in
approval audit records.

## Auth token rotation — Implemented

```
POST /api/logout → 204
```

Rotates the **master** token (printed to the server's stderr); does not touch
device tokens.
