# KAS Platform

This workspace contains batteries-included KAS packages. It depends on the
generic KAS crates in the repository root but is not a member of the root
workspace.

Each directory under `packages/` is an independent Manifest package root:

```text
packages/
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
`platform/dist/message.kas`, and `platform/dist/agent.kas`. Install Thread and
Session before Agent because the Agent Driver manages the Session Manifest.
The Message package contains the singleton fanout Driver. The Agent package
contains a singleton Driver that invokes the `codex` executable available in
its environment. Set `KAS_CODEX_BIN` to override its path and
`KAS_CODEX_HOME` to select the persistent Codex session store.

Run the complete platform flow with the real Codex CLI:

```bash
platform/tests/e2e.sh
platform/tests/e2e.sh -v
```

The E2E test requires an authenticated `codex` executable on `PATH`. Set
`KAS_CODEX_BIN` to select another real Codex executable. It starts KAS with a
temporary database, installs all four packages, creates a multi-Agent Thread
and a user Message, verifies that only the mentioned Agent receives a Run,
invokes that Agent through real Codex, restarts the Agent Driver, resumes the
same Codex Session, and verifies two-turn memory plus the assistant Messages
and Links.

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
cross-origin configuration. A production deployment should place `/api`
behind the same-origin reverse proxy. `VITE_KAS_API_URL` can select another
API base at build time when that endpoint permits browser cross-origin access.

The UI stores its KAS Bearer token and User path in browser-local settings. It
can create Agents and independent Threads, add multiple Agent participants,
turn `@handle` mentions into structured Links, wait for Driver-created real
Codex Runs, display the linked assistant Messages, inspect the Session for each
Thread participant, and reset a Session when a fresh Codex context is needed.
