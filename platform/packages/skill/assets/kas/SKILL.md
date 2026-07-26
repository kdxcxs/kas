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

Create a Message:

```bash
curl -sS \
  -H "Authorization: Bearer $KAS_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"metadata":{"path":"/messages/example","manifest":"/manifests/message","name":"example"},"spec":{"role":"system","body":"example"}}' \
  "$KAS_API/resources"
```

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
