# KAS Forge

[English](README.md) | 简体中文

> 构建在 KAS Core 之上的 Agent-native 工程控制面。

KAS Forge 是 KAS 面向企业研发环境的产品发行版。它将软件服务、代码仓库、
部署、环境、文档、配置、数据库和运行事件表示为经过授权的 Resource 与 Link，
再把边界明确的工作交给合适的 Agent。

第一个产品闭环会刻意保持狭窄：

```text
生产事件
  -> 关联的工程上下文
  -> 受限 Agent 排查
  -> 隔离的验证环境
  -> 通过测试的分支与 Merge Request
```

Forge 当前处于产品定义阶段。本目录将独立维护其 Package、Driver、UI、部署、
文档和端到端测试；目前还不是一个可以运行的完整发行版。

## 与 KAS Core 的关系

`master` 分支只包含 KAS Core；`forge` 分支在其上增加本目录，并持续从
`master` 合并 Core 更新。Forge 不直接修改 Core，也不与 `studio` 产品分支
互相合并。

Forge 需要的通用能力必须先在 `master` 实现，再合并到本分支；工程产品专属
的集成和业务行为则始终留在 `forge/`。

## 初始范围

- 软件目录 Resource 与跨系统 Link。
- Git Provider、运行环境、可观测性和 CI Driver。
- 基于事件的 Agent 分配与最小权限 Credential。
- 隔离、可销毁的开发和验证环境。
- 可审计的代码变更、测试结果、Approval 和 Merge Request。

第一个里程碑只做到经过验证的 Merge Request；自动合并或部署到生产环境明确
不在首期范围内。
