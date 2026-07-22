# 基础概念

KAS 先只保留几个最基础的概念：

## Resource

系统里可以被描述、保存和引用的对象。

例如一台机器、一个 Agent、一段对话或一个文件，后续都可以是不同类型的 Resource。

## Manifest

一类 Resource 的声明，用来说明这类 Resource 有哪些数据、可以执行哪些 Action，以及由哪个 Driver 负责。

## Action

可以对 Resource 发起的一次操作。

Action 只描述“要做什么”，实际执行由 Driver 负责。

## Driver

负责管理某类 Resource，并执行它的 Action。

Driver 是系统与真实运行环境之间的适配层。

## Run

一次 Action 的实际执行记录。

它保存输入、执行状态、结果和错误，便于查询一次操作最终发生了什么。

## Event

Run 执行过程中产生的事实记录。

例如开始执行、产生中间结果、完成或失败。

## Link

两个对象之间的有方向关系。

例如：

- Resource `contains` Resource
- Resource `uses` Resource
- Run `produces` Resource
- Resource `derived_from` Resource

## 基本关系

```text
Manifest
  ├─ 定义 Action
  └─ 由 Driver 管理

Resource
  └─ 发起 Action → Run
                    ├─ 产生 Event
                    └─ 通过 Link 关联输入、输出和其他对象
```

这些概念只构成通用底层。具体业务对象和业务流程后续再定义。

## MVP 约束

- 每个 Manifest 在创建时同时创建一个稳定的 singleton Driver。
- 同一 Manifest 的所有 Resource 共享这个 Driver 进程。
- Driver 重启不会创建新的逻辑 Driver，但会递增 `generation`。
- Run 记录执行它的 Driver 和 generation，旧进程不能完成新一代 Run。

Driver 生命周期：

```text
Stopped → Starting → Ready → Stopping → Stopped
                    └──────→ Failed → Starting
```

## Monorepo

```text
crates/kas-core    核心数据结构
crates/kas-auth    数据库驱动的认证与 RBAC 模型
crates/kas-store   SQLite 持久化
crates/kas-driver  Driver 通用接口与持续运行的 Runtime
apps/kas-admin     初始管理员工具
apps/kas-migrate   独立数据库 Migration
apps/kas-api       最小控制面 API
apps/kas-test-driver 可执行的端到端测试 Driver
```

启动顺序：

```bash
cargo run -p kas-migrate
cargo run -p kas-admin -- bootstrap admin
cargo run -p kas-api
```

`kas-api` 不会自动修改数据库结构。如果数据库尚未迁移，它会直接拒绝启动。
`kas-admin bootstrap` 只允许执行一次，并把初始管理员 Bearer token 输出到终端。

## 权限

权限规则直接保存在 SQLite 中，不从配置文件加载。当前模型包含 User、ServiceAccount、Role、RoleBinding 和 Credential。所有 API 默认拒绝，`/health` 除外。

内置的 `system:admin`、`system:editor`、`system:viewer`、`system:driver` Role 以及系统自动创建的 Binding 不允许通过 API 修改或删除。用户创建的 Role 和 RoleBinding 可以由管理员维护。

每个 Driver 自动拥有一个 ServiceAccount。Driver 每次启动都需要签发绑定当前 generation 的短期 token，旧 token 会被撤销。

## 测试

运行全部测试：

```bash
cargo test --workspace
```

端到端测试会创建临时数据库并启动真实 HTTP 服务，然后使用 `kas-test-driver` 完成以下流程：

```text
创建 Manifest 和 Resource
  → 启动 singleton Driver
  → 创建 Run
  → Driver ready 并领取 Run
  → Driver reconcile Resource 并上报 status
  → Driver 读取 Resource 并执行 echo Action
  → Driver 上报结果后继续运行
  → 创建并完成第二条 Run
  → 通过 API 查询并验证最终状态
```

`kas-test-driver` 也可以单独运行。控制面将 Driver 切换到 `starting` 后，把对应信息传给进程：

```bash
KAS_API=http://127.0.0.1:3000 \
KAS_DRIVER_ID=<driver-id> \
KAS_DRIVER_GENERATION=<generation> \
KAS_DRIVER_TOKEN=<driver-token> \
cargo run -p kas-test-driver
```

入口只需要构造具体 Driver 并调用 `DriverRuntime::run()`。Runtime 会持续执行 reconciliation、领取 Run 和上报结果，不会在完成一条 Run 后退出。
