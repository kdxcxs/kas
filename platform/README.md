# KAS Platform

This workspace contains batteries-included KAS packages. It depends on the
generic KAS crates in the repository root but is not a member of the root
workspace.

Each directory under `packages/` is an independent Manifest package root:

```text
packages/
├── message/
│   └── manifest.json
└── agent/
    ├── manifest.json
    └── driver/
        ├── Cargo.toml
        └── src/
```

Build installable `.kas` archives:

```bash
platform/scripts/build-packages.sh
```

This writes `platform/dist/message.kas` and `platform/dist/agent.kas`. The
Agent package contains a singleton Driver that invokes the `codex` executable
available in its environment. Set `KAS_CODEX_BIN` to override its path.

Run the complete platform flow with the real Codex CLI:

```bash
platform/tests/e2e.sh
platform/tests/e2e.sh -v
```

The E2E test requires an authenticated `codex` executable on `PATH`. Set
`KAS_CODEX_BIN` to select another real Codex executable. It starts KAS with a
temporary database, installs both packages, creates an Agent and a user
Message, invokes the Agent Action through Codex, and verifies the assistant
Message and its Links.

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
can create Agents, start Message threads, enqueue the Agent `message` Action,
wait for the real Codex Run, and display the linked assistant Message.
