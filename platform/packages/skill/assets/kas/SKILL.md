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
- `$KAS_TOKEN`: this Agent ServiceAccount Credential.
- `$KAS_AGENT_PATH`: this Agent Resource path.
- `$KAS_SERVICE_ACCOUNT_PATH`: this Agent ServiceAccount path.
- `$KAS_THREAD_PATH`: the current Thread Resource path.

Use `Authorization: Bearer $KAS_TOKEN` for both KAS and File API requests.
Operations remain restricted by this ServiceAccount's RBAC permissions.

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
