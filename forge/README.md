# KAS Forge

English | [简体中文](README.zh-CN.md)

> An Agent-native engineering control plane built on KAS Core.

KAS Forge is the engineering product distribution of KAS. It will connect
software services, repositories, deployments, environments, documentation,
configuration, databases, and operational events as authorized Resources and
Links, then dispatch bounded work to the appropriate Agents.

The first product loop is intentionally narrow:

```text
production event
  -> related engineering context
  -> scoped Agent investigation
  -> isolated validation environment
  -> tested branch and merge request
```

Forge is at the product-definition stage. This directory is the exclusive home
for its Packages, Drivers, UI, deployment, documentation, and end-to-end tests;
it does not yet contain a runnable distribution.

## Relationship to KAS Core

The `master` branch contains only KAS Core. The `forge` branch adds this
directory and merges Core changes from `master`. Forge does not modify Core
directly and does not merge from or into the `studio` product branch.

Generic capabilities required by Forge must first be implemented on `master`,
then merged into this branch. Product-specific integrations and behavior stay
under `forge/`.

## Initial scope

- Software catalog Resources and cross-system Links.
- Git provider, runtime, observability, and CI Drivers.
- Event-driven Agent assignment with least-privilege credentials.
- Isolated, disposable development and verification environments.
- Auditable changes, test results, approvals, and merge requests.

The initial milestone stops at a tested merge request. Autonomous production
merge or deployment is explicitly outside the first scope.
