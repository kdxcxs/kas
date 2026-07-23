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

平台自动记录的对象生命周期变化。

Resource、Link 和 Run 被创建、更新或删除时，平台会在同一事务中写入对应的
`created`、`updated` 或 `deleted` Event。Event 使用全局递增 cursor，
用于查询历史和断线后继续 Watch；业务代码不能主动创建自定义 Event。

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
                    └─ 通过 Link 关联输入、输出和其他对象

Resource、Link、Run 的变化
  └─ 由平台自动记录为 Event
```

这些概念只构成通用底层。具体业务对象和业务流程后续再定义。

## MVP 约束

- Manifest 可以是被动类型，此时 `driver` 为空且不会创建 Driver。
- 声明了 Action 的 Manifest 必须拥有一个稳定的 singleton Driver。
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

## 更新、关系与事件

Resource spec 可以通过 `PATCH /resources/{id}` 更新，请求必须携带
`expected_revision`。更新成功后 revision 递增；旧 revision 会收到冲突响应。
archive、restore 等业务状态保留在各自 Resource spec 中，不是平台字段。

Link API 支持创建、读取、按 source/relation/target 过滤和删除。Event
是 Watch 使用的内部持久化日志，只能由平台随业务对象写入自动产生，
不提供业务创建接口。

## Driver WebSocket

Driver 使用 `/drivers/{id}/connect?generation=N` 建立带 Bearer Token 的
WebSocket，不再依赖 claim 轮询。控制面主动推送 reconcile、Run 和 stop，
Driver 在同一连接上返回 ack，并将所有业务写操作统一放进一条 `mutation`
消息。每次投递都持久化；
同 generation 断线会重放，generation 更新会完成旧投递并把未完成 Run
重新排队。

reconcile 的 mutation 包含 `update_resource_status`；Run 的 mutation
可以包含有确定 UUID 的 Resource、Resource 更新和 Link 操作，
并以 `complete_run` 结束。KAS 返回 `mutation_result`，只有 `committed`
才表示整组写入成功。KAS 先按 Driver ServiceAccount 的精细 RBAC 验证，
再在一个 SQLite 事务中同时提交全部操作和完成 delivery；任一操作失败时
整组回滚。跨 Manifest fanout 需要显式给 Driver ServiceAccount
绑定诸如 `resources/message:create` 和 `links:create` 的 Role。

现有 REST 写接口继续保留；统一的 `mutation` 入口只存在于 Driver
WebSocket 协议中，不提供对应的 HTTP endpoint。

## Watch

Watch 直接复用 Driver 的 `/drivers/{driver_id}/connect` WebSocket，
不提供面向普通客户端的独立连接。Driver 使用 `watch`、`unwatch`，
控制面使用 `watch_ready`、`created`、`updated`、`deleted`、
`watch_closed` 和 `error`。消息类型直接表达生命周期变化，不再额外包含
`change` 或 `operation`。

Watch 不发送 snapshot。Driver 使用 `hello` 中的 Event cursor 建立 Watch，
并在重连后从最后处理的 cursor 继续。权限使用
`resources/{manifest}:watch`、`links:watch` 和 `runs:watch`。

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
  → Driver 通过 WebSocket ready 并接收 Run
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

入口只需要构造具体 Driver 并调用 `DriverRuntime::run()`。Runtime 会持续维护
WebSocket、执行 reconciliation、接收 Run 和上报结果，不会在完成一条 Run
后退出。
