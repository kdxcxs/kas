# KAS Core technical reference

English | [简体中文](technical-reference.zh-CN.md)

This document is the implementation-oriented companion to the
[project overview](../README.md). It describes the contracts shared by the
API, Store, Supervisor, Packages, and Drivers.

## Canonical Resource document

KAS exposes one persistent primitive:

```json
{
  "path": "/packages/acme/agent/resources/reviewer",
  "metadata": {
    "manifest": "/packages/acme/agent/manifest",
    "state": "available",
    "[kas]": {
      "revision": 4,
      "package": "/packages/acme/agent",
      "package_revision": 3,
      "observed": {
        "/packages/acme/agent/driver": {
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
      "manifest": "/packages/acme/agent/manifest",
      "state": "available",
      "[kas]": {
        "revision": 4,
        "package": "/packages/acme/agent",
        "package_revision": 3,
        "observed": {
          "/packages/acme/agent/driver": {
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
SQLite stores the three documents as JSON text. JSON expression indexes
accelerate Manifest, Link, Run, and other platform queries without duplicating
those values into parallel columns.

## Manifests and built-ins

A Manifest is a Resource defined by `/packages/kas/manifest/manifest`; it is not a second
persistent primitive. The self-describing root Manifest is the only seed the
kernel must trust directly. KAS then installs the standard library as isolated
Packages under `/packages/kas`:

```text
/packages/kas/manifest/manifest
/packages/kas/action/manifest
/packages/kas/relation/manifest
/packages/kas/link/manifest
/packages/kas/driver/manifest
/packages/kas/run/manifest
/packages/kas/user/manifest
/packages/kas/service-account/manifest
/packages/kas/role/manifest
/packages/kas/credential/manifest
/packages/kas/package/manifest
```

Action, Relation, Link, Driver, Run, User, ServiceAccount, Role, Credential,
and Package objects are ordinary Resources whose Manifest gives them platform
semantics. Every installable Package has one Manifest at its stable
`/packages/{publisher}/{package}/manifest` location.

The built-in definitions are shipped as independent packages in
[`builtins/`](../builtins/). Store initialization installs them automatically;
database migrations do not hard-code their Resources. `kas-admin bootstrap`
only creates the first User, role-binding Link, and Credential using the
already installed admin Role.

## Paths

Every public persistent object uses an absolute path for identity and
references:

```text
/packages/acme/computer
/packages/acme/computer/manifest
/packages/acme/computer/resources/computer-01
/packages/acme/computer/service-accounts/driver
/packages/acme/computer/roles/reader
```

Paths cannot be renamed. A persisted path is canonical: every segment is 1 to
128 bytes, starts and ends with a lowercase ASCII letter or digit, and contains
only lowercase ASCII letters, digits, and internal `-` characters. The complete
path is at most 1024 bytes. Empty segments, uppercase and Unicode characters,
spaces, percent-encoded aliases, `.`, `..`, repeated slashes, and a trailing
slash are invalid. Protocol correlation values such as `delivery_id` remain
UUIDs; they are not object identities.

`*` and `**` are complete wildcard segments and are legal only in patterns;
partial forms such as `/packages/acme/integration-*/manifest` are invalid. A
Manifest's `paths` field defines where its instances may be created inside its
Package sandbox:

```json
{
  "path": "/packages/acme/agent/manifest",
  "paths": ["./resources/*", "./resources/groups/*"]
}
```

Installation resolves those patterns to
`/packages/acme/agent/resources/*` and
`/packages/acme/agent/resources/groups/*`. Non-platform Packages cannot declare
an absolute Manifest path pattern or escape their Package Root. Trusted
`/packages/kas/**` Packages are the only exception because their platform
Manifests define cross-Package types such as Action, Link, Role, and Run.

Creation must satisfy the Package boundary, the Manifest path patterns, and
the caller's RBAC path rules. A Package Root must already exist before generic
CRUD may create descendants. A path containing a `credentials` segment can
only be populated through Credential APIs.

Path hierarchy establishes identity, ownership, and authorization scope; it
is not an implicit Link. Prefix nesting does not synthesize relationships or
make ordinary deletion cascade. Package-only `./...` notation is resolved and
validated before installation and is never persisted or exposed by the API.

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

`manifest.json` defines only the Manifest. Its path must be exactly
`/packages/{publisher}/{package}/manifest`; KAS derives the stable Package Root
by removing `/manifest`. Each JSON file below `resources/` contains one normal
Resource envelope and its top-level `path` must use `./...`. Package-relative
paths and patterns resolve from the Package Root, while `.` in a Manifest
selector means the Package's own Manifest. Absolute paths remain valid only as
cross-Package references.

For `/packages/acme/agent`, examples resolve as follows:

```text
./resources/reviewer       -> /packages/acme/agent/resources/reviewer
./actions/run              -> /packages/acme/agent/actions/run
./roles/driver             -> /packages/acme/agent/roles/driver
./driver                   -> /packages/acme/agent/driver
. in a Manifest selector   -> /packages/acme/agent/manifest
```

Driver `entrypoint` is different: `./driver/bin/kas-agent-driver` is a file
inside the tar artifact, not a Resource path.

KAS validates and hashes the archive, stages it, then atomically moves it to:

```text
${KAS_DATA_DIR}/packages/sha256/<digest>/
```

Installation creates the protected Package Resource at the Package Root and a
Package-to-Manifest Link. The Package Path is stable; its `spec.digest` points
to the content-addressed artifact directory. Reinstalling the same digest is
idempotent. Installing a new digest increments the Package Resource revision,
atomically updates the Manifest and package-managed initial Resources, and
keeps ordinary business Resources in place.

For business Resources, `metadata["[kas]"].package` stores the stable Package
Path and `package_revision` stores the desired Package revision. The owning
Driver completes migration by advancing the corresponding fields in
`status.metadata["[kas]"]`.

For a running Driver, the Supervisor stops the old process and starts the new
entrypoint with an incremented generation and the new content-addressed
artifact directory.

## Relations and Links

A Relation defines valid endpoint Manifest selectors, metadata, and deletion
behavior. A Link is a `/packages/kas/link/manifest` Resource containing the Relation path,
source path, and target path.

Clients create and query Links through the generic Resource API. The built-in
Relationship Driver manages both Relation and Link Manifests, validates
endpoints asynchronously, advances valid Links to `available`, and applies
`unlink` or `cascade` deletion behavior. Cardinality and domain-specific
relationship balance remain the responsibility of business Drivers.

The Relationship Driver does not watch or list the complete Resource registry.
Source, target, and Relation indexes let the Store advance only affected Link
revisions; those unreconciled Links are then delivered through the normal
observation queue. Each delivery reads only its three referenced Resources.

Role bindings, Driver credentials, Run targets, Actions, Packages, and other
platform mappings are represented by named Links instead of private object
types.

## Authorization

Users, ServiceAccounts, Roles, Credentials, and role-binding Links are stored
as Resources. Authorization is deny-by-default except for `/health`.

A Rule constrains Manifest, verb, and optional instance path:

```json
{
  "manifests": ["/packages/acme/computer/manifest"],
  "verbs": ["get", "update"],
  "paths": ["/packages/acme/computer/resources/team-a/**"]
}
```

Manifest and path patterns support exact matches, `*`, and recursive `**`.
List operations filter every returned Resource.

Packages declare Driver ServiceAccounts, Roles, and role-binding Links in
their initial Resources. A Driver explicitly references its ServiceAccount;
KAS does not infer business permissions. Driver Credentials are bound to the
Driver generation and protected Driver-to-Credential Link. They become invalid
when the Driver stops, restarts, or loses that Link.

Because RoleBinding Links are themselves part of the authorization boundary,
the Store validates and activates them transactionally. Authorization ignores
bindings whose desired state is `deleted` or whose status is not `available`;
`pending` and `invalid` bindings never grant permissions.

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

SQLite uses `${KAS_DATA_DIR}/kas.db` by default. `KAS_DATABASE` can override
that file path, and `KAS_DATABASE_POOL_SIZE` controls the connection pool.

The API never performs schema migration implicitly; it refuses to start when
the database is not ready.

## Repository boundaries

The `master` branch owns KAS Core: the generic files in the repository root,
including `crates/`, `apps/`, `builtins/`, tests, benchmarks, and these
documents. It never contains product directories.

The `studio` branch adds KAS Studio exclusively under `studio/`. The `forge`
branch adds KAS Forge exclusively under `forge/`. Both products depend on Core;
Core never depends on either product. Core changes are committed on `master`
and then merged independently into both product branches. Product branches do
not merge into each other.

Install the repository hooks with:

```bash
scripts/install-git-hooks.sh
```

The hooks reject product files on `master`, reject direct product-branch edits
outside that product's directory, and verify that each product branch contains
the latest `master` history with an identical non-product tree.

## Validation

Run the Core tests:

```bash
cargo test --workspace
tests/e2e.sh
```

The independent end-to-end benchmark starts a real API and Driver processes,
installs generated Packages through HTTP, and reconciles through WebSocket:

```bash
./benchmarks/kas-benchmark/run.sh smoke
```

Sweep and limit profiles cover Resource, Manifest, and Driver counts, payload
size, field count, nesting depth, watch fanout, concurrency, and Driver delay.
Results are written to `benchmark-results/`.
