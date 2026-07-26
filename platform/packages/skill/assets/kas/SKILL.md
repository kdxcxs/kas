---
name: kas
description: Use KAS Resources, Links, authentication, Messages, Threads, and Files.
---

# KAS platform

KAS is the Resource management and reconciliation platform hosting this Agent.
Every persistent object is a Resource selected by its `metadata.manifest` path.

The current runtime provides:

- `$KAS_API`: KAS REST API base.
- `$KAS_FILE_API`: authenticated File content API base.
- `$KAS_APPROVAL_API`: authenticated elevation request API base.
- `$KAS_TOKEN`: this Agent ServiceAccount Credential.
- `$KAS_AGENT_PATH`: this Agent Resource path.
- `$KAS_SERVICE_ACCOUNT_PATH`: this Agent ServiceAccount path.
- `$KAS_THREAD_PATH`: the current Thread Resource path.

Use `Authorization: Bearer $KAS_TOKEN` for both KAS and File API requests.
Operations remain restricted by this ServiceAccount's RBAC permissions.

## Request user approval

When a required KAS operation is forbidden, do not ask for or handle a User
token. Submit the exact immutable operation to the Approval API:

```bash
curl -sS \
  -H "Authorization: Bearer $KAS_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "reason": "Explain why this exact mutation is required",
    "expires_in_seconds": 900,
    "operation": {
      "verb": "update",
      "path": "/resources/example",
      "update": {
        "expected_revision": 4,
        "spec": {"field": "new value"}
      }
    }
  }' \
  "$KAS_APPROVAL_API/approvals"
```

Supported operation verbs are `get`, `list`, `create`, `update`, and `delete`.
For `get`, provide `path`. For `list`, provide `manifest` and optionally
`path_prefix` and `limit` (1–1000). For `create`, provide `resource`; for
`update`, provide `path` and `update`; for `delete`, provide `path` and
`expected_revision`.

Tell the user that approval is pending and include the returned Approval path.
The Request is stored below
`/approvals{requester-path}/requests/{uuid}`. List your own approval namespace
and `/builtin/link` Resources below that namespace to follow it. A Decision has
its own `/approvals{approver-path}/decisions/{uuid}` path and connects to the
Request through the `decides` Relation. A successful Result has its own
`/approvals{requester-path}/results/{uuid}` path and connects to the Request
through `result-of`; `produced-by` connects it to the Decision. Its
`spec.response` contains the HTTP status, content type, and sanitized response
body. Platform `[kas]` bookkeeping fields are omitted, but Resource `metadata`,
`spec`, and `status` business data are retained.
Never retry the privileged operation directly or place credentials in an
Approval.

Read this Agent:

```bash
curl -sS -G \
  -H "Authorization: Bearer $KAS_TOKEN" \
  --data-urlencode "path=$KAS_AGENT_PATH" \
  "$KAS_API/resources/by-path"
```

Read the current Thread by replacing `$KAS_AGENT_PATH` with
`$KAS_THREAD_PATH` in the request above.

List Resources of one Manifest:

```bash
curl -sS -G \
  -H "Authorization: Bearer $KAS_TOKEN" \
  --data-urlencode "manifest=/manifests/message" \
  "$KAS_API/resources"
```

## Reply to the current Message

When KAS runs you for a Message, `$KAS_MESSAGE_PATH` is the Message that
mentioned you and `$KAS_REPLY_PATH` is the exact path where you must publish
your reply. Your final terminal response is not forwarded to the Thread.

Before finishing the run, create one assistant Message at `$KAS_REPLY_PATH`,
then create all three required Links:

- `authored-by`: reply → `$KAS_AGENT_PATH`
- `replies-to`: reply → `$KAS_MESSAGE_PATH`
- `message-thread`: reply → `$KAS_THREAD_PATH`

Use the following shape, replacing `<reply>` with the response for the user:

```bash
reply_body='<reply>'
curl -fsS \
  -H "Authorization: Bearer $KAS_TOKEN" \
  -H "Content-Type: application/json" \
  -d "$(jq -n --arg path "$KAS_REPLY_PATH" --arg body "$reply_body" '{
    metadata: {
      path: $path,
      manifest: "/manifests/message",
      name: "assistant-reply"
    },
    spec: {role: "assistant", body: $body}
  }')" \
  "$KAS_API/resources"

create_reply_link() {
  relation_name="$1"
  relation_path="$2"
  target_path="$3"
  curl -fsS \
    -H "Authorization: Bearer $KAS_TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(jq -n \
      --arg path "$KAS_REPLY_PATH/links/$relation_name" \
      --arg relation "$relation_path" \
      --arg source "$KAS_REPLY_PATH" \
      --arg target "$target_path" '{
        metadata: {
          path: $path,
          manifest: "/builtin/link",
          name: ($path | split("/") | last)
        },
        spec: {
          relation: $relation,
          source: $source,
          target: $target,
          metadata: {}
        }
      }')" \
    "$KAS_API/resources"
}

create_reply_link \
  authored-by \
  /manifests/message/relations/authored-by \
  "$KAS_AGENT_PATH"
create_reply_link \
  replies-to \
  /manifests/message/relations/replies-to \
  "$KAS_MESSAGE_PATH"
create_reply_link \
  message-thread \
  /manifests/message/relations/message-thread \
  "$KAS_THREAD_PATH"
```

The Agent Driver validates this Message and its Links but never creates the
reply from your final Codex output. If any required write fails, fix it before
ending the run.

Upload a new immutable File:

```bash
curl -sS \
  -H "Authorization: Bearer $KAS_TOKEN" \
  -F "content=@<local-path>" \
  "$KAS_FILE_API/files?path=/files/<new-unique-path>"
```

Download File content:

```bash
curl -sS -G \
  -H "Authorization: Bearer $KAS_TOKEN" \
  --data-urlencode "path=/files/example" \
  "$KAS_FILE_API/files/content" \
  -o <output-path>
```

Never print, persist, or place `$KAS_TOKEN` in a Resource, Message, Link, Skill,
or file.
