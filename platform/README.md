# KAS Platform

This workspace contains batteries-included KAS packages. It depends on the
generic KAS crates in the repository root but is not a member of the root
workspace.

Each directory under `packages/` is an independent Manifest package root:

```text
packages/
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
both Agent and Session Resources. Agent-to-Skill assignment uses a `uses`
Link. The built-in KAS operating context is itself the `/skills/kas` Skill and
is assigned to every Agent. The fixed Agent prompt contains only enough
bootstrap context to identify KAS, the Agent's ServiceAccount, and its API
environment variables; the complete operating instructions live in `$kas`.

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
`platform/dist/message.kas`, and `platform/dist/agent.kas`. Install Thread,
Session, File, and Skill before Agent because the Agent Driver manages Session
Resources and reads Skill and File descriptors.
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

Run the complete platform flow with the real Codex CLI:

```bash
platform/tests/e2e.sh
platform/tests/e2e.sh -v
```

The E2E test requires an authenticated `codex` executable on `PATH`. Set
`KAS_CODEX_BIN` to select another real Codex executable. It starts KAS with a
temporary database, installs all six packages, uploads binary content,
verifies full and ranged downloads, creates a multi-Agent Thread with an
attached File, and verifies that only the mentioned Agent receives a Run. The
real Codex Agent loads an assigned Skill, downloads the attachment with its
own ServiceAccount, and produces KAS Resources from both inputs. The test then
replaces the Skill bundle without changing the Skill or bundle Link identity,
restarts the Agent Driver, and verifies that the resumed Session uses the new
bundle. It also rejects a ZIP containing a symbolic link. Agent
ServiceAccounts may upload new Files and download existing Files, but uploads
are create-only and cannot overwrite an existing File path. The test also
covers File and Skill RBAC and deletion.

## Frontend

`frontend/` is a standalone Svelte project with its own package manifest and
lockfile. It is deliberately not part of the Rust workspace.

Start KAS, then run the frontend development server:

```bash
cd platform/frontend
npm install
KAS_API_URL=http://127.0.0.1:3000 npm run dev
```

The development server proxies `/api` to `KAS_API_URL`, so KAS does not need
cross-origin configuration. It also proxies `/files-api` to
`KAS_FILE_API_URL` (default `http://127.0.0.1:3001`) and `/skills-api` to
`KAS_SKILL_API_URL` (default `http://127.0.0.1:3002`). A production deployment
should route the three paths to KAS, the File Driver, and the Skill Driver.
`VITE_KAS_API_URL`, `VITE_KAS_FILE_API_URL`, and `VITE_KAS_SKILL_API_URL` can
select direct API bases at build time when those endpoints permit browser
cross-origin access.

The UI stores its KAS Bearer token and User path in browser-local settings. It
can create Agents and independent Threads, add multiple Agent participants,
turn `@handle` mentions into structured Links, wait for Driver-created real
Codex Runs, display the linked assistant Messages, inspect the Session for each
Thread participant, reset a Session when a fresh Codex context is needed, and
attach arbitrary files to Messages. Image, video, and audio previews are
loaded on demand with authenticated requests; all other content is available
through authenticated download. The Skill page imports or replaces Skill ZIP
bundles and manages Agent assignments.
