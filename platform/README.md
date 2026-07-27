# KAS Platform

This workspace contains batteries-included KAS packages. It depends on the
generic KAS crates in the repository root but is not a member of the root
workspace.

Each directory under `packages/` is an independent Manifest package root:

```text
packages/
├── approval-result/
│   └── manifest.json
├── skill/
│   ├── manifest.json
│   ├── assets/kas/
│   ├── resources/
│   └── driver/
├── file/
│   ├── manifest.json
│   ├── resources/
│   │   ├── drivers/
│   │   ├── relations/
│   │   ├── role-bindings/
│   │   ├── roles/
│   │   └── service-accounts/
│   └── driver/
│       ├── Cargo.toml
│       └── src/
├── session/
│   ├── manifest.json
│   └── resources/
│       └── relations/
├── thread/
│   ├── manifest.json
│   └── resources/
│       └── relations/
├── message/
│   ├── manifest.json
│   ├── resources/
│   │   ├── drivers/
│   │   ├── relations/
│   │   ├── role-bindings/
│   │   ├── roles/
│   │   └── service-accounts/
│   └── driver/
│       ├── Cargo.toml
│       └── src/
├── approval/
│   ├── manifest.json
│   ├── resources/
│   └── driver/
└── agent/
    ├── manifest.json
    ├── resources/
    │   ├── actions/
    │   ├── drivers/
    │   ├── relations/
    │   ├── role-bindings/
    │   ├── roles/
    │   └── service-accounts/
    └── driver/
        ├── Cargo.toml
        └── src/
```

`manifest.json` defines only the package Manifest. Each Action, Relation,
Driver, ServiceAccount, Role, and RoleBinding installed with the package is an
ordinary Resource stored as one JSON file under `resources/`. `thread` is
data-only. `session` defines the persistent Thread-Agent session record.
`file` stores immutable File descriptors in KAS while its singleton Driver
owns the binary content API and storage. Binary content never passes through
the KAS API. A `File --attached-to--> Resource` Link represents attachments.
`skill` stores stable Skill Resources while each Skill's current ZIP bundle is
an immutable File selected by its `bundle` Link. Replacing a bundle updates the
existing Link target; it does not replace the Skill or Link Resource.
`message` owns the fanout Driver that turns validated `mentioned` Links into
Agent Runs. `agent` owns the Codex Driver that executes those Runs and manages
both Agent and Session Resources. The Agent publishes its own assistant Message
and required Links through the scoped KAS API; the Driver validates that reply
and never converts Codex's final terminal output into a Message.
Agent-to-Skill assignment uses a `uses`
Link. The built-in KAS operating context is itself the `/skills/kas` Skill and
is assigned to every Agent. The fixed Agent prompt contains only enough
bootstrap context to identify KAS, the Agent's ServiceAccount, and its API
environment variables; the complete operating instructions live in `$kas`.
`approval` lets an Agent request one exact operation that its normal
ServiceAccount cannot perform. A User may approve or reject the request. On
approval, the Driver verifies that the deciding User may perform the exact
operation and executes it with that request's User credential. Request,
Decision, and Result are independent Resources whose paths belong to their
principal namespaces: `/approvals{requester}/requests/{uuid}`,
`/approvals{approver}/decisions/{uuid}`, and
`/approvals{requester}/results/{uuid}`. Named Links record `requested-by`,
`decides`, `decided-by`, `result-of`, and `produced-by`; no shared request ID
or per-request Role and RoleBinding is required. A successful operation creates
an immutable `/manifests/approval-result` Resource containing the sanitized API
response. Plaintext credentials are never stored in KAS Resources.

Threads are independent Resources under `/threads/{id}`. Their `participants`
Links may reference multiple Agents, but only Agents referenced by a Message
`mentioned` Link receive a Run. Message membership uses `message-thread`;
the old convention where a root Message doubled as a Thread no longer exists.

Each `(Thread, Agent)` pair gets at most one Session Resource at
`/threads/{thread}/sessions/{agent}`. The first mention starts a persistent
Codex CLI session and records the `thread.started.thread_id`; later mentions
use `codex exec resume`. The Session cursor advances to the latest assistant
Message, so a resumed Agent receives only Thread Messages created since its
previous turn. Different Agents in the same Thread have isolated Sessions.

Build installable `.kas` archives:

```bash
platform/scripts/build-packages.sh
```

This writes `platform/dist/thread.kas`, `platform/dist/session.kas`,
`platform/dist/file.kas`, `platform/dist/skill.kas`,
`platform/dist/message.kas`, `platform/dist/approval-result.kas`,
`platform/dist/approval.kas`, and `platform/dist/agent.kas`. Install Approval
Result before Approval. Install Thread, Session, File, and Skill before Agent
because the Agent Driver manages Session Resources and reads Skill and File
descriptors.
The Message package contains the singleton fanout Driver. The Agent package
contains a singleton Driver that invokes the `codex` executable available in
its environment. Set `KAS_CODEX_BIN` to override its path and
`KAS_CODEX_HOME` to select the persistent Codex session store.

The File Driver binds `KAS_FILE_ADDRESS` (default `127.0.0.1:3001`) and stores
content under `KAS_DATA_DIR/file-driver/blobs`. Clients upload multipart
`content` to `POST /files` and download with
`GET /files/content?path=/files/...`; both endpoints accept normal KAS Bearer
Credentials. The Driver forwards that Credential to KAS `/auth/check` using
the `upload` or `download` verb. `KAS_FILE_MAX_BYTES` sets the upload limit
(default 1 GiB). Downloads support HEAD and HTTP byte ranges.

The Skill Driver binds `KAS_SKILL_ADDRESS` (default `127.0.0.1:3002`).
Create a Skill with multipart field `bundle` at
`POST /skills?path=/skills/{id}` and replace its bundle at
`PATCH /skills?path=/skills/{id}&expected_revision={revision}`. Both endpoints
authorize the caller through KAS and upload bundle bytes through the File API.
The Driver validates every ZIP before creating Skill state and validates it
again before materializing it for an Agent. Bundles require a root
`SKILL.md`; absolute or traversing paths, duplicate entries, symbolic links,
and other non-file entries are rejected. Expanded size and entry counts are
also bounded.

The Approval Driver binds `KAS_APPROVAL_ADDRESS` (default
`127.0.0.1:3003`). Agents submit requests to `POST /approvals`; authenticated
Users decide them with `POST /approvals/decide`. Approval never expands an
Agent's standing permissions. The Driver checks the current User credential
against the exact create, update, delete, get, or bounded list operation before
executing it. An unauthorized Decision is retained as `invalid` and leaves the
Request pending; a later authorized Decision may still claim it. Concurrent or
late valid Decisions become `superseded`. Successful responses are stored as
Result Resources after removing platform-only `[kas]` bookkeeping fields and
are discovered through `result-of` and `produced-by` Links.

Run the complete platform flow with the real Codex CLI:

```bash
platform/tests/e2e.sh
platform/tests/e2e.sh -v
```

The E2E test requires an authenticated `codex` executable on `PATH`. Set
`KAS_CODEX_BIN` to select another real Codex executable. It starts KAS with a
temporary database, installs all eight packages, uploads binary content,
installs the FrontendPlugin package and Registry plugin, verifies full and
ranged downloads, creates a multi-Agent Thread with an
attached File, and verifies that only the mentioned Agent receives a Run. The
real Codex Agent loads an assigned Skill, downloads the attachment with its
own ServiceAccount, and produces KAS Resources from both inputs. The test then
replaces the Skill bundle without changing the Skill or bundle Link identity,
restarts the Agent Driver, and verifies that the resumed Session uses the new
bundle. It also rejects a ZIP containing a symbolic link. Agent
ServiceAccounts may upload new Files and download existing Files, but uploads
are create-only and cannot overwrite an existing File path. The test also
covers File and Skill RBAC and deletion. It also proves that an Agent cannot
perform a privileged write directly, can submit it for User approval, and that
the operation executes only when the deciding User has the requested
permission. It covers invalid, rejected, successful, and duplicate Decisions,
Link-based discovery, and requester namespace isolation.

## Frontend

`packages/frontend/` is a normal KAS Package with a singleton Rust Driver.
The Package contains the built Svelte host, the FrontendPlugin Manifest,
Driver RBAC, the HTTP Gateway, and the plugin runtime. Build all Platform
packages with:

```bash
platform/scripts/build-packages.sh
```

The Driver serves the host and acts as a small same-origin reverse proxy.
`/api/*` is an intrinsic route to the same KAS control plane used by the
Driver protocol. Every independently served HTTP API is configured as a
`/manifests/proxy` Resource in KAS. The Frontend Driver reconciles those
Resources into its live longest-prefix route table, so route changes require
neither environment configuration nor a process restart. File is the
foundational external route; Skill and Approval remain compatibility Proxy
Resources while those Drivers still expose specialized HTTP operations.

The connection form exchanges a KAS Credential for an opaque in-memory
Gateway Session. Only an `HttpOnly`, `SameSite=Strict` session cookie remains
in the browser; the Gateway forwards each operation with the current User's
Credential rather than its Driver ServiceAccount.

The UI stores the User path and API base in browser-local settings; it does
not persist the exchanged Bearer Credential. It can create Agents and independent Threads, add multiple Agent participants,
turn `@handle` mentions into structured Links, wait for Driver-created real
Codex Runs, display the linked assistant Messages, inspect the Session for each
Thread participant, reset a Session when a fresh Codex context is needed, and
attach arbitrary files to Messages. Image, video, and audio previews are
loaded on demand with authenticated requests; all other content is available
through authenticated download. The Skill page imports or replaces Skill ZIP
bundles and manages Agent assignments. The Approval page presents the exact
requested operation and its reason, lets a User approve or reject it, and
shows the resulting decision audit record.

### Frontend plugins

Platform extensions can contribute entries to the Workspace or Resources
sidebar without changing Core or rebuilding the host UI. A FrontendPlugin is
an ordinary Resource whose ZIP bundle is an immutable File connected by the
package-defined `./relations/bundle` Link. Build and install one with:

```bash
platform/scripts/build-frontend-plugin.sh \
  platform/plugins/registry \
  /tmp/registry.zip

platform/scripts/install-frontend-plugin.sh \
  /tmp/registry.zip \
  /frontend-plugins/registry \
  registry \
  index.html \
  Objects \
  '◇' \
  50 \
  /objects
```

The Frontend Driver watches the plugin, bundle Link, and File; validates and
extracts the ZIP into a digest-addressed cache; and serves the entrypoint and
relative assets below `/plugins/{slug}/`. The host loads that URL in a
sandboxed iframe without exposing the User Credential. A `postMessage` bridge
provides plugin context plus Resource, Link, authorization, approved Gateway
API, and navigation operations; every operation is executed by the host with
the current User's normal KAS permissions. Static JavaScript and CSS assets
are CORS-readable so an opaque-origin sandbox can load modules, but entrypoints
and all data operations remain authenticated.

`platform/plugins/registry/` implements the generic Objects registry.
Threads, Agents, Skills, and Approvals are also installed FrontendPlugin
Resources and render through the same iframe runtime. Their bundle reuses the
complete Svelte management UI, including File, Skill, Approval, and navigation
operations. Chat and the current Thread context remain part of the minimal
host shell.

## Docker

Build the complete Platform image from the repository root:

```bash
docker build -f platform/Dockerfile -t kas-platform .
```

Run it with a named volume so the SQLite database, installed packages, File
blobs, plugins, and Codex sessions survive container replacement:

```bash
docker run --name kas-platform \
  -p 5173:5173 \
  -p 3000:3000 \
  -v kas-data:/var/lib/kas \
  -e OPENAI_API_KEY \
  kas-platform
```

The multi-stage build compiles Core and every Platform Driver in release mode,
builds the Svelte host, creates the `.kas` archives, bundles the built-in
iframe plugins, and installs the Codex CLI. The runtime image runs as the
unprivileged `kas` User under `tini`.

On a new volume, the entrypoint migrates the database, bootstraps an admin,
starts KAS, installs all packages, creates the three Proxy Resources, and
installs Threads, Agents, Skills, Approvals, and Objects as FrontendPlugin
Resources. The initial admin token is printed once in the container logs:

```bash
docker logs kas-platform
```

Open `http://localhost:5173/` and connect with that token. The control-plane
API is available on port `3000`; File, Skill, and Approval APIs normally remain
behind the Frontend Gateway. Set `KAS_ADMIN_NAME` before the first start to
choose another bootstrap User name. `CODEX_VERSION` is a build argument when a
specific Codex CLI release is required:

```bash
docker build -f platform/Dockerfile \
  --build-arg CODEX_VERSION=latest \
  -t kas-platform .
```
