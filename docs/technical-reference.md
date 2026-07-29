# KAS Core technical reference

English | [简体中文](technical-reference.zh-CN.md)

This document is the implementation-oriented companion to the
[project overview](../README.md). It describes the contracts shared by the
API, Store, Supervisor, Packages, and Drivers.

## Canonical Resource document

KAS exposes one persistent primitive:

```json
{
  "path": "/agents/reviewer",
  "metadata": {
    "manifest": "/manifests/agent",
    "state": "available",
    "[kas]": {
      "revision": 4,
      "package": "/packages/sha256/...",
      "observed": {
        "/manifests/agent/driver": {
          "driver_revision": 2,
          "resource_revision": 4
        }
      }
    }
  },
  "spec": {
    "model": "gpt-5"
  },
  "status": {
    "metadata": {
      "manifest": "/manifests/agent",
      "state": "available",
      "[kas]": {
        "revision": 4,
        "package": "/packages/sha256/...",
        "observed": {
          "/manifests/agent/driver": {
            "driver_revision": 2,
            "resource_revision": 4
          }
        }
      }
    },
    "spec": {
      "model": "gpt-5"
    }
  }
}
```

`path` is the immutable global identity. `metadata.manifest` points to the
Manifest Resource that defines the document. Root `metadata` and `spec`
describe desired state; `status.metadata` and `status.spec` describe the state
that has been implemented.

KAS-owned metadata is isolated under the reserved `"[kas]"` key. Manifest
schemas may not define field names containing `[` or `]`.

The `resources` table contains only `path`, `metadata`, `spec`, and `status`.
SQLite stores the three documents as JSON text. PostgreSQL uses native `jsonb`.
Backend-specific JSON expression indexes accelerate Manifest, Link, Run, and
other platform queries without duplicating those values into parallel columns.

## Manifests and built-ins

A Manifest is a Resource defined by `/builtin/manifest`; it is not a second
persistent primitive. The self-describing root Manifest is the only seed the
kernel must trust directly. KAS then installs the standard library under
`/builtin`:

```text
/builtin/manifest
/builtin/action
/builtin/relation
/builtin/link
/builtin/driver
/builtin/run
/builtin/user
/builtin/service-account
/builtin/role
/builtin/credential
/builtin/package
```

Action, Relation, Link, Driver, Run, User, ServiceAccount, Role, Credential,
and Package objects are ordinary Resources whose Manifest gives them platform
semantics. Business Manifests normally live below `/manifests/{name}`.

The built-in definitions are shipped as independent packages in
[`builtins/`](../builtins/). Store initialization installs them automatically;
database migrations do not hard-code their Resources. `kas-admin bootstrap`
only creates the first User, role-binding Link, and Credential using the
already installed admin Role.

## Paths

Every public persistent object uses an absolute path for identity and
references:

```text
/manifests/computer
/computers/team-a/computer-01
/manifests/agent/service-accounts/driver
/roles/team-a/computer-reader
```

Paths cannot be renamed. Empty segments, `.`, `..`, repeated slashes, and a
trailing slash are invalid. Protocol correlation values such as
`delivery_id` remain UUIDs; they are not object identities.

## Packages

`POST /packages` accepts an `application/vnd.kas.manifest+tar` archive. The
archive root contains a `manifest.json`, optional initial Resource documents,
and optional Driver artifacts:

```text
agent.kas
├── manifest.json
├── resources/
│   ├── actions/
│   ├── relations/
│   ├── roles/
│   └── driver.json
└── driver/
    └── bin/
        └── kas-agent-driver
```

`manifest.json` defines only the Manifest. Each JSON file below `resources/`
contains one normal Resource envelope. Relative paths beginning with `./` are
resolved below the installed Manifest path.

KAS validates and hashes the archive, stages it, then atomically moves it to:

```text
${KAS_DATA_DIR}/packages/sha256/<digest>/
```

Installation creates a protected `/builtin/package` Resource and a
Package-to-Manifest Link. Reinstalling the same Manifest and digest is
idempotent. Installing a new digest atomically updates the Manifest and
package-managed initial Resources, while ordinary business Resources remain
in place and reconcile against the new Package revision.

For a running Driver, the Supervisor stops the old process and starts the new
entrypoint with an incremented generation and the new package root. An old
Package remains available until all status references converge, then KAS
reclaims it.

## Relations and Links

A Relation defines valid endpoint Manifest selectors, metadata, and deletion
behavior. A Link is a `/builtin/link` Resource containing the Relation path,
source path, and target path.

Clients create and query Links through the generic Resource API. The built-in
Relationship Driver manages both Relation and Link Manifests, validates
endpoints asynchronously, advances valid Links to `available`, and applies
`unlink` or `cascade` deletion behavior. Cardinality and domain-specific
relationship balance remain the responsibility of business Drivers.

Role bindings, Driver credentials, Run targets, Actions, Packages, and other
platform mappings are represented by named Links instead of private object
types.

## Authorization

Users, ServiceAccounts, Roles, Credentials, and role-binding Links are stored
as Resources. Authorization is deny-by-default except for `/health`.

A Rule constrains Manifest, verb, and optional instance path:

```json
{
  "manifests": ["/manifests/computer"],
  "verbs": ["get", "update"],
  "paths": ["/computers/team-a/**"]
}
```

Manifest and path patterns support exact matches, `*`, and recursive `**`.
List operations filter every returned Resource.

Packages declare Driver ServiceAccounts, Roles, and role-binding Links in
their initial Resources. A Driver explicitly references its ServiceAccount;
KAS does not infer business permissions. Driver Credentials are bound to the
Driver generation and protected Driver-to-Credential Link. They become invalid
when the Driver stops, restarts, or loses that Link.

`GET /auth` returns the caller's Credential, Subject, and effective Rules.
`POST /auth/check` answers whether that same Credential may perform one
Manifest, verb, and path operation. External Driver APIs can therefore reuse
KAS authorization without receiving access to another principal's secrets.

## Reconciliation

Root `metadata` and `spec` are desired; the corresponding documents under
`status` are current. Any owner-visible difference requires reconciliation.
Changing desired state advances `metadata["[kas]"].revision`.

Each matching Driver has an observation entry:

- desired `metadata["[kas]"].observed` records the Driver and Resource
  revisions it must consume;
- `status.metadata["[kas]"].observed` records the revisions it has completed.

A Driver declares the Manifests it `manages` and optional additional Resource
patterns it `watches`. One Driver may manage several Manifests, but a Manifest
has at most one owner Driver. Owner Drivers converge status; watch-only Drivers
advance only their own observation.

KAS computes affected Driver/Resource pairs when Resources, Manifests, or
Driver watch rules change. It keeps ready and in-flight work in process memory
and reconstructs it from observation differences after restart, so no
persistent delivery fanout table is required.

Deleting a Resource first changes desired state to `deleted`. Drivers reconcile
that state, the Relationship Driver applies Link deletion rules, and KAS
removes the row only after every matching Driver has consumed the latest
revision. There is no force-delete or tombstone in the current model.

Resource transactions also append internal `created`, `updated`, and `deleted`
Events. Events are an audit log, not a business Event API or Driver queue.

## Driver lifecycle and protocol

A Driver Resource describes a singleton executable and its managed Manifests.
The Supervisor manages process start, stop, generation, credentials, hello
timeout, crash restart, and backoff.

The process receives:

```text
KAS_API
KAS_DRIVER_PATH
KAS_DRIVER_GENERATION
KAS_DRIVER_TOKEN
KAS_MANIFEST_PATH
KAS_PACKAGE_ROOT
```

It connects to `/drivers/connect?path=...&generation=...` over an authenticated
WebSocket. KAS pushes one-Resource reconciliation deliveries, Runs, and stop
messages. The Driver sends acknowledgements, mutations,
`reconcile_complete`, Run completion, and heartbeats on the same connection.

A Driver first commits any required mutation, then sends
`reconcile_complete`. Only the explicit completion advances its observation.
Delivery IDs are stable across retries. A lost connection or expired
in-flight lease causes redelivery; an API restart recreates unfinished work
from Resource observations and Run state.

WebSocket mutations support:

```text
create_resource
update_resource
delete_resource
update_resource_status
complete_run
```

All operations in a mutation are authorized against the Driver
ServiceAccount and committed atomically. The control protocol itself is
authorized by Driver identity, generation, Credential, and in-flight delivery,
so a Manifest does not need to grant generic protocol privileges.

The reusable `kas-driver` runtime owns the connection loop and concurrency.
A concrete Driver implements reconciliation and execution behavior, then calls
`DriverRuntime::run()`.

## Storage and processes

Run migrations explicitly before starting the API:

```bash
cargo run -p kas-migrate
cargo run -p kas-admin -- bootstrap admin
cargo run -p kas-api
```

SQLite uses `${KAS_DATA_DIR}/kas.db` by default. Set the same
`KAS_DATABASE=postgresql://...` value for all three commands to use PostgreSQL.
`KAS_DATABASE_POOL_SIZE` controls the connection pool and defaults to 16.

The API never performs schema migration implicitly; it refuses to start when
the database is not ready.

## Repository boundaries

The `core` branch owns generic files in the repository root, including
`crates/`, `apps/`, `builtins/`, tests, benchmarks, and these documents.

The `master` branch adds the batteries-included product exclusively under
`platform/`. Product code depends on Core; Core never depends on Platform.
Core changes are committed on `core` and then merged into `master`.

Install the repository hooks with:

```bash
scripts/install-git-hooks.sh
```

The hooks reject Core commits that contain `platform/**`, reject direct
master-only edits outside `platform/**`, and verify that `master` contains the
latest Core history with an identical non-Platform tree.

## Validation

Run the Core tests:

```bash
cargo test --workspace
tests/e2e.sh
```

With Docker available, the same black-box flow can validate native PostgreSQL:

```bash
tests/e2e-postgres.sh
```

The independent end-to-end benchmark starts a real API and Driver processes,
installs generated Packages through HTTP, and reconciles through WebSocket:

```bash
./benchmarks/kas-benchmark/run.sh smoke
```

Sweep and limit profiles cover Resource, Manifest, and Driver counts, payload
size, field count, nesting depth, watch fanout, concurrency, and Driver delay.
Results are written to `benchmark-results/`.
