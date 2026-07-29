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

## 快速运行

安装较新的 Rust 工具链后，在仓库根目录依次运行：

```bash
cargo run -p kas-migrate
cargo run -p kas-admin -- bootstrap admin
cargo run -p kas-api
```

创建管理员时会输出 Bearer token。随后 KAS 会监听
`http://127.0.0.1:3000`，并将 SQLite 数据库保存在 `.data/kas.db`。
可以在另一个终端确认 API 已就绪：

```bash
curl http://127.0.0.1:3000/health
```

PostgreSQL、配置项、Package 安装和 Driver 开发等内容见
[Core 技术参考](docs/technical-reference.zh-CN.md)。如果需要完整产品与 Web
界面，请使用
[KAS Platform](https://github.com/kdxcxs/kas/blob/master/platform/README.zh-CN.md)。

## 为什么是 KAS

> 无论参与者是人还是 Agent，所有操作归根结底都是与 Resource 交互。

创建、读取、修改、删除、共享文件、调用 Agent、授予权限和批准操作，在产品
层面看起来是完全不同的功能；但它们本质上都在读取、改变、关联或操作某个
可以被描述、保存和引用的对象。执行本身也可以表示为 Resource：Action 描述
可以做什么，Run 则记录一次具体调用。

就像计算机中形态各异的程序最终都归结为对内存的操作，KAS 的出发点是：
应用中的一切最终都归结为对 Resource 的操作。因此，真正重要的问题不是
“这个功能应该再建立一个什么特殊子系统”，而是：

- 这个 Resource 长什么样，代表什么？
- 它与其他 Resource 之间有什么关系？

只要把这两件事描述清楚，许多上层能力就可以自然建立在同一套基础之上：

- **资源共享：** 人和 Agent 通过具有稳定地址的同一批 Resource 协作，不需要
  在各个功能内部维护彼此不可见的数据副本。
- **权限与审计：** 读取、修改、建立关系和执行操作都经过同一套权限模型，
  并留下可以检查的记录，从而实现更精细的 Agent 控制和行为审计。
- **控制与编排：** Link 可以表达参与者、依赖、输入、输出、所有权和顺序，
  Driver 据此协调复杂的 Agent 行为与工作流。
- **非侵入式扩展：** Link 可以在不修改原 Resource 的情况下，为它增加新的
  描述和关系。
- **动态扩展词汇：** Manifest 可以在运行时引入新的 Resource 类型。只要拥有
  相应权限，Agent 自己也可以定义和创建新类型，而不必等待修改 KAS 内核。

因此，KAS 不需要分别为聊天、任务、身份、权限、工作流和插件建立不同的底层。
它们只是同一个小型、可组合控制面上的不同 Resource 定义与关系。

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
