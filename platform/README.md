# KAS Platform

This workspace contains batteries-included KAS packages. It depends on the
generic KAS crates in the repository root but is not a member of the root
workspace.

Each directory under `packages/` is an independent Manifest package root:

```text
packages/
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
`message` owns the fanout Driver that turns validated `mentioned` Links into
Agent Runs. `agent` owns the Codex Driver that executes those Runs and manages
both Agent and Session Resources.

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
`platform/dist/file.kas`, `platform/dist/message.kas`, and
`platform/dist/agent.kas`. Install Thread, Session, and File before Agent
because the Agent Driver manages Session Resources and reads File descriptors.
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

Run the complete platform flow with the real Codex CLI:

```bash
platform/tests/e2e.sh
platform/tests/e2e.sh -v
```

The E2E test requires an authenticated `codex` executable on `PATH`. Set
`KAS_CODEX_BIN` to select another real Codex executable. It starts KAS with a
temporary database, installs all five packages, uploads binary content,
verifies full and ranged downloads, creates a multi-Agent Thread with an
attached File, and verifies that only the mentioned Agent receives a Run. The
real Codex Agent downloads the attachment with its own ServiceAccount,
produces a KAS Resource from its contents, survives an Agent Driver restart,
and resumes the same Session. Agent ServiceAccounts may upload new Files and
download existing Files, but uploads are create-only and cannot overwrite an
existing File path. The test also covers File RBAC and deletion.

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
`KAS_FILE_API_URL` (default `http://127.0.0.1:3001`). A production deployment
should route both paths to KAS and the File Driver respectively.
`VITE_KAS_API_URL` and `VITE_KAS_FILE_API_URL` can select direct API bases at
build time when those endpoints permit browser cross-origin access.

The UI stores its KAS Bearer token and User path in browser-local settings. It
can create Agents and independent Threads, add multiple Agent participants,
turn `@handle` mentions into structured Links, wait for Driver-created real
Codex Runs, display the linked assistant Messages, inspect the Session for each
Thread participant, reset a Session when a fresh Codex context is needed, and
attach arbitrary files to Messages. Image, video, and audio previews are
loaded on demand with authenticated requests; all other content is available
through authenticated download.
