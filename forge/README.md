# KAS Forge

English | [简体中文](README.zh-CN.md)

> An Agent-native engineering control plane built on KAS Core.

KAS Forge is the engineering product distribution of KAS. It will connect
software services, repositories, deployments, environments, documentation,
configuration, databases, and operational events as authorized Resources and
Links, then dispatch bounded work to the appropriate Agents.

Forge begins with one deliberately small but complete capability loop:

```text
User runs a scoped Agent
  -> Agent discovers a missing capability
  -> Agent builds and submits a validated .kas Package
  -> User reviews and approves the request
  -> KAS installs the Package as new Resources
```

The Agent may inspect KAS and work in its assigned repository, but its
ServiceAccount cannot install Packages. The Package Request service validates
the archive before it reaches the review queue, records requester and approver
as Links, and performs installation using the approving user's credential.
This creates an auditable boundary without preventing Agents from proposing the
next capability Forge needs.

This directory is the exclusive home for Forge Packages, Drivers, UI,
deployment, documentation, and end-to-end tests.

## Try it

Prerequisites: Rust, Node.js, `jq`, `curl`, and an authenticated Codex CLI.

```bash
./forge/scripts/preview.sh
```

The script builds KAS and both Forge Packages, starts a temporary Core API,
provisions a real Codex Agent with a scoped ServiceAccount, submits an example
Package Request as that Agent, and starts the UI at
`http://127.0.0.1:5173`. It prints the one-time preview URL, database path, and
log directory. Press Ctrl-C to stop it.

Run the complete boundary test with:

```bash
./forge/tests/e2e.sh
```

## Included Packages

- `agent`: provisions one scoped ServiceAccount and runtime Role per Agent and
  executes tasks through the locally authenticated Codex CLI.
- `package-request`: validates submitted `.kas` archives, exposes the approval
  API, records the decision trail, and installs approved Packages.

## Relationship to KAS Core

The `core` branch contains only KAS Core. The `forge` branch adds this
directory and merges Core changes from `core`. Forge does not modify Core
directly and does not merge from or into the `studio` product branch.

Generic capabilities required by Forge must first be implemented on `core`,
then merged into this branch. Product-specific integrations and behavior stay
under `forge/`. The `master` integration branch merges Forge together with
Core and Studio for a complete checkout.

## Next engineering scope

- Software catalog Resources and cross-system Links.
- Git provider, runtime, observability, and CI Drivers.
- Event-driven Agent assignment with least-privilege credentials.
- Isolated, disposable development and verification environments.
- Auditable changes, test results, approvals, and merge requests.

The next milestone connects the current controlled-extension loop to software
catalog, repository, runtime, observability, and CI Resources. It stops at a
tested merge request; autonomous production merge or deployment remains outside
the initial scope.
