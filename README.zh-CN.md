# KAS

[English](README.md) | 简体中文

![KAS — Kas Agent System](docs/assets/kas-banner.png)

> 用一套 Resource 模型，把领域对象、关系、权限和后台协调统一到同一个控制面。

KAS 是一个面向 Resource 的应用控制面。你只需要描述系统中“应该存在什么”，
KAS 负责保存这些对象、检查权限、记录关系，并把需要处理的变化交给对应的
Driver。

它适合构建 Agent 平台、自动化控制面、集成中心，以及任何需要让多个后台能力
围绕共享对象持续协作的系统。仓库中的 `core` 分支提供通用内核；
`master` 分支还包含一个可直接使用的
[KAS Platform](https://github.com/kdxcxs/kas/blob/master/platform/README.zh-CN.md)。

![KAS 控制面协调 Resource 与 Driver](docs/assets/core-control-plane.png)

> **控制面：** Manifest 定义 Resource 结构。客户端向 KAS 提交期望的
> Resource；KAS 完成持久化、选择受影响的 singleton Driver，并记录 Driver
> 返回的当前状态。任意 Resource 之间都可以通过 Link 连接。

## 为什么是 KAS

许多系统会分别为任务、用户、权限、关系、后台作业和插件建立不同的数据模型，
最后还要额外解决它们之间的同步问题。KAS 把这些内容都表示为 Resource：

- 它们使用相同的路径进行寻址；
- 通过 Manifest 获得结构和语义；
- 通过 Link 建立显式关系；
- 通过同一套 RBAC 授权；
- 由 Driver 持续把期望状态变成实际状态。

这让平台能力可以作为 Package 安装和更新，而不是不断向内核加入新的特殊类型。

## 最小心智模型

### Resource

KAS 中唯一的持久化原语。Agent、Message、Role、Driver，甚至 Manifest 自己，
最终都是 Resource。每个 Resource 都有稳定的 `path`，以及期望数据和当前状态：

```json
{
  "path": "/agents/planner",
  "metadata": {
    "manifest": "/manifests/agent",
    "state": "available"
  },
  "spec": {
    "model": "gpt-5"
  },
  "status": {
    "metadata": {
      "state": "available"
    },
    "spec": {
      "model": "gpt-5"
    }
  }
}
```

### Manifest

定义一类 Resource 的结构、状态和可用能力，类似一份可以被平台理解的“类定义”。
Manifest 本身也是 Resource，因此新的领域类型可以动态安装，不需要修改 KAS
内核。

### Driver

负责让 Resource 的当前状态追上期望状态。一个 Driver 可以管理一种或多种
Manifest，也可以关注其他 Resource；KAS 只投递真正需要处理的变化。每个
Manifest 共享一个 singleton Driver 进程，而不是为每个实例启动一个进程。

### Relation 与 Link

Relation 定义什么样的关系是合法的，Link 是一条具体关系。Link 可以连接任意
两个 Resource，例如：

![通过具名 Link 连接的 Resource](docs/assets/resource-links.png)

> **具名 Link 示例：** `Thread → Agent`（`participants`）·
> `Message → Agent`（`mentioned`）· `Agent → Skill`（`uses`）·
> `Driver → Role`（`role-binding`）。

### Action 与 Run

Action 描述 Resource 可以执行的操作，Run 表示一次具体执行。两者仍然是
Resource，因此执行记录可以沿用相同的查询、权限和关系模型。

### Package

Package 是 KAS 的交付单元，可以一起安装 Manifest、初始化 Resources 和
Driver。一个功能由此能够自带自己的数据定义、关系、权限与运行逻辑。

## KAS 如何工作

![KAS Reconcile 闭环](docs/assets/reconciliation-loop.png)

> **Reconcile 闭环：**（1）客户端修改 Resource 的期望文档；（2）KAS
> 鉴权并持久化；（3）KAS 向每个受影响的 Driver 推送这一个 Resource；
> （4）Driver 提交 mutation 并明确完成；（5）KAS 推进 status，直至其与
> 期望文档一致。

KAS Core 关注的是这条通用闭环，不内置具体业务。完整的 Agent、Thread、
Message、File、Skill、Approval 和可插拔前端由 KAS Platform 以普通 Package
提供。

## Core 与 Platform

| 项目 | 作用 |
| --- | --- |
| **KAS Core** | Resource API、Manifest、Package、RBAC、Link、Driver Runtime、SQLite/PostgreSQL 存储 |
| **KAS Platform** | 基于 Core 构建的开箱即用多 Agent 协作产品和 Web UI |

核心代码位于根目录的 `crates/`、`apps/`、`builtins/`；Platform 的产品能力
独立维护在 `platform/`，因此 Core 可以持续合并进完整平台，而不与业务包互相
缠绕。

## 继续阅读

- [文档索引](docs/README.zh-CN.md)
- [Core 技术参考](docs/technical-reference.zh-CN.md)
- [KAS Platform 介绍](https://github.com/kdxcxs/kas/blob/master/platform/README.zh-CN.md)
