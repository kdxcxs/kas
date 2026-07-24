# 基础概念

KAS 先只保留几个最基础的概念：

## Resource

系统里可以被描述、保存和引用的对象。

例如一台机器、一个 Agent、一段对话或一个文件，后续都可以是不同类型的 Resource。

## Manifest

一类 Resource 的完整包，用来声明 Resource schema、Action、Relation，以及可选的
singleton Driver 和它的 executable。

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
`created`、`updated` 或 `deleted` Event。Event 使用全局递增 sequence，
用于审计和查询历史；业务代码不能主动创建自定义 Event。

## Link

两个对象之间的有方向关系。任何具有稳定 Path 的持久对象都可以作为 source
或 target，包括 Manifest、Action、Relation、Resource、Driver、Run、Link、
User、ServiceAccount、Role、RoleBinding 和 Credential。Event、Delivery
和 request 等追加记录或运行时对象不属于 Link 端点。

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

## Path 身份

所有公开持久对象使用不可变的绝对 `path` 作为身份和引用，不再向 API、
Driver、Link、Event 或 RBAC 暴露对象 UUID。例如：

```text
/manifests/computer
/computers/team-a/computer-01
/computers/team-a/computer-01/runs/{request-id}
/service-accounts/team-a/agent-01
/roles/team-a/computer-reader
```

Path 创建后不能重命名，禁止空段、`.`、`..`、重复 `/` 和尾部 `/`。
`request_id`、`delivery_id` 等协议关联 ID 仍使用 UUID。

## MVP 约束

- Manifest 可以是被动类型，此时 `driver` 为空且不会创建 Driver。
- 声明了 Action 的 Manifest 必须拥有一个稳定的 singleton Driver。
- 同一 Manifest 的所有 Resource 共享这个 Driver 进程。
- Driver 重启不会创建新的逻辑 Driver，但会递增 `generation`。
- Run 通过受保护的 Link 关联 Resource、Action 和 Driver，并记录执行
  generation；旧进程不能完成新一代 Run。

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

## 分支与目录职责

`core` 是 KAS 核心分支，只维护通用平台能力和可复用实现。本 README 中列出的
根目录、`crates/` 和 `apps/` 均由 `core` 维护，不包含任何预置业务 Manifest、
业务 Driver 或完整产品功能。

`master` 是 batteries-included 的完整平台分支。它以 `core` 为基础，但所有
仅属于完整平台的内容必须放在独立的 `platform/` 目录：

```text
platform/
├── Cargo.toml
├── Cargo.lock
├── manifests/
├── drivers/
├── apps/
├── deploy/
└── README.md
```

`platform/` 使用独立 Rust workspace，并通过 path dependency 引用
`../crates/...` 中的核心 crate；不得把平台专属 package 加入根
`Cargo.toml` workspace。平台专属文档、配置、部署文件和测试也应保留在
`platform/` 内。

核心改动只在 `core` 上完成，再由 `core` 合并到 `master`。`master` 不直接
修改或复制核心实现；如果完整平台发现核心缺陷或需要通用能力，应先在
`core` 修复或实现。`core` 不得依赖 `platform/`。通过这一单向依赖和目录
所有权约定，持续降低 `core → master` 合并时的冲突。

启动顺序：

```bash
cargo run -p kas-migrate
cargo run -p kas-admin -- bootstrap admin
cargo run -p kas-api
```

`kas-api` 不会自动修改数据库结构。如果数据库尚未迁移，它会直接拒绝启动。
Store 打开数据库时会自动安装随 KAS 发布的 `builtins/core` 和 `builtins/auth`
Manifest 包，因此核心 Relation 和默认 Role 从首次启动起就存在，不由
Migration 写入，也不依赖管理命令注入。`kas-admin bootstrap` 只使用 auth
built-in 中标记为 admin 的 Role 创建首个管理员及其 Binding，并把 Bearer
token 输出到终端。

## Manifest 包

`POST /manifests` 接收 `application/vnd.kas.manifest+tar`，不接受客户端机器上的
binary 绝对路径。tar 根目录必须包含 `manifest.json`，例如：

```text
agent.kas
├── manifest.json
└── driver/
    └── bin/
        └── kas-agent-driver
```

Manifest 内成员使用 `./` 相对路径：

```text
./actions/message
./relations/has-thread
./driver
./driver/bin/kas-agent-driver
```

对象路径分别解析为 `/manifests/agent/actions/message`、
`/manifests/agent/relations/has-thread` 和 `/manifests/agent/driver`。
entrypoint 仍作为包内相对文件保存。

API 对整个 tar 计算 SHA-256，先解压到 staging，校验完成后原子移动到：

```text
${KAS_DATA_DIR}/packages/sha256/<digest>/
```

数据库只保存 `sha256:<digest>` 和相对 entrypoint，不保存数据目录的绝对路径。
同一 Manifest path 和 digest 的重复安装是幂等操作；同一路径安装不同内容会被拒绝。

带 Driver 的 Manifest 安装完成后，由 API 进程内的 Supervisor 自动启动
entrypoint。Supervisor 管理 singleton、generation、临时 Credential、ready
超时、停止、崩溃重启和退避，并向进程传递 `KAS_API`、
`KAS_DRIVER_PATH`、`KAS_DRIVER_GENERATION`、`KAS_DRIVER_TOKEN`、
`KAS_MANIFEST_PATH` 和 `KAS_PACKAGE_ROOT`。

## 权限

权限规则直接保存在 SQLite 中，不从配置文件加载。当前模型包含 User、
ServiceAccount、Role、RoleBinding 和 Credential。所有 API 默认拒绝，
`/health` 除外。

Rule 同时约束资源类型、verb 和实例 path：

```json
{
  "resources": ["resources/computer"],
  "verbs": ["get", "patch"],
  "paths": ["/computers/team-a/**"]
}
```

Path pattern 支持精确匹配、单段 `*` 和递归 `**`；省略 `paths` 表示该类型的
全部实例。List 会逐对象过滤。创建或修改 Role 时，除非拥有
`roles:escalate`，其
resource、verb 和 path 都不能超过调用者现有权限；绑定 Role 同样要求调用者
拥有该 Role 的权限或 `bind` 权限。

Manifest 可以在 `rbac` 中声明自己的 ServiceAccount、Role 和 RoleBinding。
这些对象随 Manifest 原子安装，不能作为普通独立对象修改或删除。Driver 必须
通过 `service_account` 显式引用其中一个 ServiceAccount；KAS 不猜测业务权限，
也不会为 Driver 自动生成 Role 或 Binding。Driver 每次启动会签发绑定当前
generation 和该 ServiceAccount 的短期 token，旧 token 会被撤销。

默认的 `system:admin`、`system:editor`、`system:viewer` Role 由 auth built-in
Manifest 声明。系统身份不依赖固定对象 path：bootstrap 按 Role 的
`system_role` 语义查找 admin。

## 更新、关系与事件

Resource 使用同一份 Manifest schema 描述 `spec`（期望的完整状态）和
`status`（Driver 已实现的完整状态）。Manifest 可以声明扩展状态，并通过
`default_state` 和 `initial_state` 分别指定新建 Resource 的期望状态和初始
状态；KAS 固定保留 `pending`、`available`、`deleted`。只要 `spec != status`，
KAS 就会把对象持续交给该 Manifest 的 singleton Driver reconcile；revision
只用于 spec 的并发控制，不再兼任状态或重试标记。

Resource spec 可以通过 `PATCH /resources/by-path?path=...` 更新，请求必须携带
`expected_revision`。更新成功后 revision 递增；旧 revision 会收到冲突响应。
archive、restore 等仍是 Manifest 自己定义的业务状态。

`DELETE /resources/by-path?path=...&expected_revision=N` 不会绕过 Driver：
KAS 先把 `spec.state` 改为 `deleted`，Driver reconcile 后把
`status.state` 改为 `deleted`。两者都到达 `deleted` 后，KAS 删除 Resource、
其 Link 和相关运行数据，不保留 tombstone，也暂不提供 force delete；原 path
随后可以重新使用。

Action、Relation、Driver、ServiceAccount、Role 和 RoleBinding 都拥有
Manifest 下的独立 Path。Manifest 安装时，KAS 根据 core/auth built-in Relation
的语义 role 自动创建 Manifest 到所有成员、Driver 到 ServiceAccount、
RoleBinding 到 Role 和 Subject 的受保护 Link。创建 Resource 和 Run 时，KAS
同样自动创建 Resource 到 Manifest、Run 到 Resource/Action/Driver 的归属
Link；客户端只提交直接引用，不需要也不能伪造这些平台关系。

实现按 Relation 的语义 role 查找关系，不把 built-in Relation 的具体 path
写死在业务逻辑中。数据库可以维护由 Link 自动生成的内部投影索引，但这些
索引不属于公开 API。

Relation 使用明确的 `one_to_one`、`one_to_many`、`many_to_one` 或
`many_to_many` 类型。首版 `ensure` 只支持 `one_to_one`：当 KAS 发现符合
selector 的对象缺少关系时，会创建一个待处理 Link，其 source 或 target
可以暂时为空（但不能同时为空），`spec.state=available`、
`status.state=pending`。这个 Link 由“声明该 Relation 的 Manifest”的 Driver
reconcile，而不是由任一端点对象的 Driver 猜测所有权。Driver 可以在同一次
mutation 中创建缺失对象、补齐端点并推进 Link status。

Link 自身同样具有 `spec`、`status` 和 `revision`，也进入持久化 reconcile
队列。普通 Link 通常以 `available` 创建；部分端点 Link 只允许由上述 ensured
one-to-one Relation 使用。`on_source_delete` 可以选择 `unlink` 或 `cascade`。

Link API 支持创建、读取、按 source/relation_path/target 过滤和删除。创建 Link
除了需要 `links:create` 和具体 Relation 的 `relations:use`，还必须对 source
和 target 对应的对象类型及 Path 拥有 `link` verb。执行 Run 还需要具体 Action
的 `actions:invoke`。

`GET /resources/by-path?path=...&include=relations` 会在 Resource 字段之外返回
一层双向 `links` 和调用者有权读取的 `related` 对象，不进行递归展开。

Event 是平台维护的内部持久化审计日志，只能随业务对象事务自动产生，
不提供业务创建接口，也不参与 Driver 工作投递。

## Driver WebSocket

Driver 使用 `/drivers/connect?path=...&generation=N` 建立带 Bearer Token 的
WebSocket，不再依赖 claim 轮询。控制面主动推送 reconcile、Run 和 stop，
Driver 在同一连接上返回 ack，并将所有业务写操作统一放进一条 `mutation`
消息。每次投递都持久化；
同 generation 断线会重放，generation 更新会完成旧投递并把未完成 Run
重新排队。

reconcile 投递的 `object` 可以是 Resource 或 Link。Driver 的
`reconcile(&ReconcileObject)` 直接返回一组 mutation：Resource 通常使用
`update_resource_status`，Link 通常使用 `update_link`，也可以在同一事务中
创建或删除 Resource、Link、ServiceAccount。空 mutation 会让 KAS 重新检查
spec/status，并在仍不一致时再次排队；临时失败不会把对象永久标记为失败。
Run 的 mutation 可以包含有确定 path 的 Resource、Resource 更新和 Link 操作，
并以 `complete_run` 结束。KAS 返回 `mutation_result`，只有 `committed`
才表示整组写入成功。

Driver 的 `ready`、ack、reconcile 状态回写和 Run 完成属于控制协议，由绑定
generation 的 Credential、Driver 身份、in-flight delivery 和目标对象共同
授权，不要求 Manifest 猜测这些基础协议权限。mutation 中额外的业务写操作
仍按 Driver ServiceAccount 的精细 RBAC 验证，并和 delivery 完成在一个
SQLite 事务中提交；任一操作失败时整组回滚。跨 Manifest fanout 需要显式给
Driver ServiceAccount 绑定诸如 `resources/message:create` 和
`links:create` 的 Role。

现有 REST 写接口继续保留；统一的 `mutation` 入口只存在于 Driver
WebSocket 协议中，不提供对应的 HTTP endpoint。

Driver 不订阅 Event。需要 Driver 处理的业务变化必须显式表示为
`Resource.spec/status` 或 `Link.spec/status` 的差异；缺失关系由 Relation
的 `ensure` 创建待处理 Link。这样工作项由持久化 reconcile queue 投递，
不会依赖临时订阅、连接状态或 Event sequence。

## 测试

运行全部测试：

```bash
cargo test --workspace
```

真实进程级端到端测试已独立为脚本，不再放在 Rust 进程内模拟：

```bash
tests/e2e.sh
```

脚本使用临时数据库和数据目录完成：

```text
启动 migration、admin 和真实 kas-api
  → 验证 core/auth built-in Manifest 在启动时已存在
  → 把编译后的 kas-test-driver 打进 .kas tar
  → POST 安装 Manifest 包
  → 验证 Manifest/RBAC/Driver 的受保护关系已自动创建
  → Supervisor 启动真实 binary
  → Driver 通过 WebSocket ready
  → 创建 Resource，并由 KAS 自动关联 Manifest
  → 查询 Resource、Links 和关联对象
  → 创建 Run，并由 KAS 自动关联 Resource/Action/Driver
  → Driver 执行 echo 并完成 Run
  → 验证受保护的 System Links
  → DELETE Resource，经 Driver reconcile 后验证硬删除及 path 可复用
  → 停止 Driver 并确认子进程退出
```

`kas-test-driver` 也可以单独运行。正常情况下这些变量由 Supervisor 自动传入：

```bash
KAS_API=http://127.0.0.1:3000 \
KAS_DRIVER_PATH=<driver-path> \
KAS_DRIVER_GENERATION=<generation> \
KAS_DRIVER_TOKEN=<driver-token> \
cargo run -p kas-test-driver
```

入口只需要构造具体 Driver 并调用 `DriverRuntime::run()`。Runtime 会持续维护
WebSocket、执行 reconciliation、接收 Run 和上报结果，不会在完成一条 Run
后退出。
