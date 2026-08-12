# KAS Forge

[English](README.md) | 简体中文

> 构建在 KAS Core 之上的 Agent-native 工程控制面。

KAS Forge 是 KAS 面向企业研发环境的产品发行版。它将软件服务、代码仓库、
部署、环境、文档、配置、数据库和运行事件表示为经过授权的 Resource 与 Link，
再把边界明确的工作交给合适的 Agent。

Forge 从一个刻意保持精简、但可完整运行的能力闭环开始：

```text
用户运行受限 Agent
  -> Agent 发现缺失的能力
  -> Agent 构建并提交经过校验的 .kas Package
  -> 用户检查并批准申请
  -> KAS 将 Package 安装为新的 Resource
```

Agent 可以读取 KAS、在分配的代码库中工作，但它的 ServiceAccount 无权安装
Package。Package Request 服务会在进入审批队列前校验归档，通过 Link 记录申请者
和审批者，并使用审批用户自己的 Credential 完成安装。这样既保留了清晰、可审计
的权限边界，也不妨碍 Agent 持续提出 Forge 所需的新能力。

本目录独立维护 Forge 的 Package、Driver、UI、部署、文档和端到端测试。

## 运行预览

需要安装 Rust、Node.js、`jq`、`curl`，并确保本机 Codex CLI 已登录。

```bash
./forge/scripts/preview.sh
```

脚本会构建 KAS 和两个 Forge Package，启动临时 Core API，为真实 Codex Agent
创建受限 ServiceAccount，以 Agent 身份提交一个示例 Package Request，然后在
`http://127.0.0.1:5173` 启动前端。一次性预览地址、数据库路径和日志目录都会直接
打印出来；按 Ctrl-C 停止。

运行完整权限边界测试：

```bash
./forge/tests/e2e.sh
```

## 已包含的 Package

- `agent`：为每个 Agent 创建独立 ServiceAccount 与运行时 Role，并使用本机已登录
  的 Codex CLI 执行任务。
- `package-request`：校验 `.kas` 归档、提供审批 API、记录决策链，并安装获批 Package。

## 与 KAS Core 的关系

`master` 分支只包含 KAS Core；`forge` 分支在其上增加本目录，并持续从
`master` 合并 Core 更新。Forge 不直接修改 Core，也不与 `studio` 产品分支
互相合并。

Forge 需要的通用能力必须先在 `master` 实现，再合并到本分支；工程产品专属
的集成和业务行为则始终留在 `forge/`。

## 下一阶段范围

- 软件目录 Resource 与跨系统 Link。
- Git Provider、运行环境、可观测性和 CI Driver。
- 基于事件的 Agent 分配与最小权限 Credential。
- 隔离、可销毁的开发和验证环境。
- 可审计的代码变更、测试结果、Approval 和 Merge Request。

下一阶段会把当前的“受控自扩展”闭环连接到软件目录、代码库、运行环境、可观测性
和 CI Resource，最终做到经过验证的 Merge Request；自动合并或部署到生产环境
依然不在首期范围内。
