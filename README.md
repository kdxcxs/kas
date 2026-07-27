# 核心理念：只有 Resource

KAS 只有一个公开的持久化原语：`Resource`。系统中所有可以被保存、查询、
授权、关联或协调的对象都是 Resource，没有与 Resource 并列的第二套对象模型。

```json
{
  "path": "/agents/planner",
  "metadata": {
    "manifest": "/manifests/agent",
    "name": "Planner",
    "state": "available",
    "[kas]": {
      "revision": 1,
      "observed": {
        "/manifests/agent/driver": {
          "driver_revision": 2,
          "resource_revision": 1
        }
      },
      "created_at": "...",
      "updated_at": "..."
    }
  },
  "spec": {},
  "status": {
    "metadata": {
      "state": "available"
    },
    "spec": {}
  }
}
```

顶层 `path` 是 Resource 的全局稳定身份；`metadata.manifest` 指向定义
该 Resource 的另一个 Resource。所有引用只保存 path，不再携带 `kind`。
`spec` 只保存业务期望；生命周期状态、revision 和 Driver 消费进度都是
平台 metadata。

SQLite 中的 `resources` 表也严格保持这一形状，只包含 `path`、`metadata`、
`spec`、`status` 四列；后三列是 JSON 文档。Manifest、Run、Link 等查询通过
JSON expression index 加速，不再为平台字段维护平行列。

## Manifest 是定义 Resource 的 Resource

Manifest 不是与 Resource 并列的原语，而是类似“类定义”的一种 Resource。
根 Manifest 自描述：

```text
/builtin/manifest
  manifest → /builtin/manifest
```

其他 Manifest 都是它的实例：

```text
/builtin/relation
  manifest → /builtin/manifest

/manifests/agent
  manifest → /builtin/manifest

/agents/planner
  manifest → /manifests/agent
```

根 Manifest 是启动时唯一需要由 KAS 内核直接信任并加载的自引用种子。它就绪
后，其他 built-in Manifest 和业务 Manifest 均通过普通 Resource 机制安装。
当前不提供 Manifest 继承；每个 Resource 的 `manifest` 指向一个精确的定义。

## Built-in 是标准库，不是新的原语

KAS 启动时自动安装一组 built-in Manifest：

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

`/builtin` 是 KAS 保留且受保护的标准库命名空间，不代表新的对象类型。
系统提供的具体 Relation 和 Role 也位于这个命名空间，例如
`/builtin/relations/run-action` 和 `/builtin/roles/admin`。业务 Manifest
仍使用 `/manifests/{name}`，业务 Resource 可以按自己的领域选择 path。

Action、Relation、Link、Driver、Run、User、ServiceAccount、Role 和 Credential
因而都只是由 built-in Manifest 定义的 Resource。例如：

```text
/manifests/message/relations/mentioned
  manifest → /builtin/relation

/messages/123/links/mentioned/planner
  manifest → /builtin/link
```

具体 Relation、Link、Role 或 Run 并不是 built-in；built-in 的是定义它们结构
和平台语义的 Manifest。Relation 和 Link 都按普通 Resource 持久化，不在
Store 中维护专用关系投影。SQLite 只保留 `resources` 和 `events` 两张表；
Driver、Run、RBAC、Package ownership 与 Credential 哈希均直接由 Resource
表达。

## 平台语义

- **Action Resource** 描述可以对某类 Resource 发起的操作。
- **Relation Resource** 使用两端允许的 Manifest path 描述 Resource 关系。
- **Link Resource** 保存 relation、source path 和 target path。
- **Driver Resource** 描述一个 singleton Driver、executable 及其管理的 Manifest。
- **Run Resource** 是一次 Action 的执行记录。
- **Package Resource** 描述已安装 artifact 的 digest、大小和媒体类型。
- **RBAC Resource** 使用 built-in Manifest 表达身份、Role 和 Binding。

Relation selector 直接按 Manifest path 工作，不再按对象 kind 工作。例如：

```json
{
  "sources": [{"manifest": "/manifests/message"}],
  "targets": [{"manifest": ["/manifests/user", "/manifests/agent"]}]
}
```

因此 `@Agent` 可以被保存为一个普通 Link Resource：Message 是 source，
Agent 是 target，`mentioned` Relation 负责约束两端的 Manifest。

## Event

平台在 Resource 创建、更新或删除的同一事务中写入 `created`、`updated`
或 `deleted` Event。Event 使用全局递增 sequence，用于内部审计和可靠投递；
业务代码不能主动创建自定义 Event。Event 是平台运行记录，不是第二套公开
领域对象。Package 的可查询元数据是 Resource；tar、binary
等 artifact bytes 仍是外部内容存储。

## Link

两个 Resource 之间的有方向关系。任何 Resource 都可以作为 source 或
target，因此 Manifest、Action、Driver、User、Role 等不需要专门的 Link
端点类型。Link 自己也是 Resource，其 `manifest` 固定指向
`/builtin/link`。API 只按 Manifest schema 接受 Link；内置 Relationship Driver
异步读取 Relation、source 和 target，校验端点 selector 与 metadata，并把
结果写入 Link status。无效 Link 会进入 `invalid` 状态。

例如：

- Thread `mentions` Agent
- Driver `uses` ServiceAccount
- Subject 通过内置 `role-binding` Relation 的 Link 绑定 Role
- Run `executes` Action

## 基本关系

```text
Manifest Resource
  ├─ 定义普通 Resource 的 schema 与状态
  ├─ Package 的 resources/ 保存初始化 Action / Relation / Driver / RBAC
  └─ Driver 默认 reconcile 本 Manifest Resource，并通过 watches 关注额外 Resource

普通 Resource ──Action──> Run Resource
       │                      │
       └──────── Link Resource┘
```

这些名称描述的是 Manifest 赋予 Resource 的平台语义，不会引入新的持久化
原语或按类型分裂的 CRUD。

## Path 身份

所有公开持久对象使用不可变的绝对 `path` 作为身份和引用，不再向 API、
Driver、Link、Event 或 RBAC 暴露对象 UUID。例如：

```text
/manifests/computer
/computers/team-a/computer-01
/computers/team-a/computer-01/runs/{request-id}
/manifests/agent/service-accounts/driver
/roles/team-a/computer-reader
```

Path 创建后不能重命名，禁止空段、`.`、`..`、重复 `/` 和尾部 `/`。
`request_id`、`delivery_id` 等协议关联 ID 仍使用 UUID。

## MVP 约束

- Manifest 可以不声明 Driver，此时它是被动类型。
- Action 只有在所属 Manifest 存在 Driver 时才可执行。
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
Store 打开数据库时会自动安装随 KAS 发布的
[`builtins/`](builtins/) 中的独立 Manifest 文件。每个 built-in Manifest
都由自己的 `builtins/{name}/manifest.json` 定义，初始化 Resource 分别保存于
该包的 `resources/**/*.json`。每份目录作为一个随 binary 发布的内置 Package
安装；安装时先创建全部 Manifest 根 Resource，再创建各自初始化 Resource，
以解开根 Manifest 的自描述启动依赖。它们从首次启动起就存在，不由 Migration
写死，也不依赖管理命令注入。
`kas-admin bootstrap` 只使用其中标记为 admin 的 Role 创建首个 User、
role-binding Link 和 Credential，并把 Bearer token 输出到终端。

## Manifest 包

`POST /packages` 接收 `application/vnd.kas.manifest+tar`，不接受客户端机器上的
binary 绝对路径。tar 根目录必须包含 `manifest.json`，例如：

```text
agent.kas
├── manifest.json
├── resources/
│   ├── actions/
│   │   └── message.json
│   ├── service-accounts/
│   │   └── driver.json
│   └── driver.json
└── driver/
    └── bin/
        └── kas-agent-driver
```

`manifest.json` 只定义 Manifest 自身，不包含 `members` 或 `resources`
字段。`resources/` 可选；KAS 递归读取其中所有 `.json`，每个文件定义一个
初始化 Resource。文件目录只用于组织，Resource 身份始终来自 JSON 顶层
`path`；文件同样使用通用的 `path/metadata/spec/status` envelope。

初始化 Resource 使用相对于 Manifest path 的 `./` 路径：

```text
./actions/message
./relations/has-thread
./driver
./driver/bin/kas-agent-driver
```

对象路径分别解析为 `/manifests/agent/actions/message`、
`/manifests/agent/relations/has-thread` 和 `/manifests/agent/driver`。
每个初始化 Resource 同时声明自己的 built-in Manifest；entrypoint 仍作为
包内相对文件保存。例如 Driver Resource 使用 `/builtin/driver`，Role 使用
`/builtin/role`。

API 对整个 tar 计算 SHA-256，先解压到 staging，校验完成后原子移动到：

```text
${KAS_DATA_DIR}/packages/sha256/<digest>/
```

安装命令根据 digest 创建受保护的 Package Resource：

```text
/packages/sha256/<hex>
  manifest → /builtin/package
```

其 `spec` 和 `status.spec` 保存 `digest`、`size_bytes`、`media_type`，
生命周期 state 位于各自 metadata；Resource 不保存数据目录的绝对路径。
KAS 同时创建
`/builtin/relations/package-manifest` Link：

```text
Package Resource ──package-manifest──> Manifest Resource
```

Manifest spec 因此不再重复保存 `package_digest`。运行时使用 Package 安装
事务建立的内部 ownership 投影找到 artifact；上述 Link 仍作为普通 Resource
表达同一事实，不参与控制面 bootstrap。Package Resource 只能由
`POST /packages` 创建并受保护，不能通过普通 Resource CRUD 伪造。

同一 Manifest path 和 Package 的重复安装是幂等操作；同一路径安装不同内容会
被拒绝。`POST /packages` 返回 Package Resource；安装后的 Manifest 和初始化
Resource 通过 `GET /resources` 或 `GET /resources/by-path?path=...` 查询，
没有第二套 Manifest CRUD。

带 Driver 的包安装完成后，由 API 进程内的 Supervisor 自动启动
entrypoint。Supervisor 管理 singleton、generation、临时 Credential、hello
超时、停止、崩溃重启和退避，并向进程传递 `KAS_API`、
`KAS_DRIVER_PATH`、`KAS_DRIVER_GENERATION`、`KAS_DRIVER_TOKEN`、
`KAS_MANIFEST_PATH` 和 `KAS_PACKAGE_ROOT`。

## 权限

权限规则同样以 Resource 保存到 SQLite，不从配置文件加载。User、
ServiceAccount、Role 和 Credential 分别由对应的系统 Manifest 定义；授权
关系是 `/builtin/relations/role-binding` 下的普通 Link。所有 API 默认拒绝，
`/health` 除外。

Rule 同时约束 Resource 的 Manifest、verb 和实例 path：

```json
{
  "manifests": ["/manifests/computer"],
  "verbs": ["get", "update"],
  "paths": ["/computers/team-a/**"]
}
```

Manifest 和 Path pattern 都支持精确匹配、单段 `*` 和递归 `**`；省略
`paths` 表示匹配这些 Manifest 的全部实例。List 会逐 Resource 过滤。

Manifest 包通过 `resources/` 声明自己的 ServiceAccount、Role 和 role-binding
Link。这些 Resource 随包原子安装并受保护。Driver 必须在 `spec.service_account` 中引用
一个 ServiceAccount Resource；KAS 不猜测业务权限，也不会自动生成 Role
或 Binding。Driver 每次启动会签发绑定当前 generation 和该 ServiceAccount
的短期 Credential，旧 Credential 会被撤销。

默认的 `system:admin`、`system:editor`、`system:viewer` Role 是 built-in
包中的 Resource。系统身份不依赖固定 Role path：bootstrap 按 Role spec 的
`system_role` 语义查找 admin。

调用者可以通过 `GET /auth` 查看当前 Bearer Credential 的完整授权上下文，
包括 Credential path、Subject、所有当前有效 Rule，以及 Driver Credential
的 Driver path 和 generation。该结果反映每次请求时数据库中的 Role 和
role-binding Link，不是签发 Credential 时固化的权限快照。

外部 Driver 或服务可以通过 `POST /auth/check` 判断当前 Bearer Credential
是否允许对一个确定对象执行指定 verb：

```json
{
  "manifest": "/manifests/file",
  "verb": "download",
  "path": "/files/report"
}
```

接口始终以 `200 OK` 返回授权判断；权限不足表示为 `"allowed": false`，而不是
请求失败。响应同时包含 Credential path 和 Subject，调用方不需要再请求一次
`GET /auth` 才能识别操作者。无效或失效的 Credential 仍返回 `401`，格式错误的
Manifest、path 或 verb 返回 `400`。这两个接口只检查请求自身携带的 Credential，
不能查询其他 Credential 的权限。

## 更新、关系与事件

根级 `metadata` 和 `spec` 是期望文档，`status.metadata` 和 `status.spec`
是当前已实现文档。两侧使用完全相同的结构，Manifest 的 `resource_schema`
同时校验 `spec` 和 `status.spec`；创建时未显式提供 `status.spec`，KAS 会
用根级 `spec` 初始化它。
Manifest 可以声明扩展状态，并通过 `default_state` 和 `initial_state` 分别
指定新建 Resource 的期望状态和初始状态；KAS 固定提供 `pending`、
`available`、`deleted`，不得在 `states` 中重复声明。`resource_schema` 只描述
业务 spec 字段，`state` 由 KAS 单独校验并位于 metadata。根级
`metadata.state` 或 `spec` 改变时 `metadata["[kas]"].revision` 递增；
status 和消费进度更新不会推进 revision。

所有平台维护的 metadata 都放在保留字段 `"[kas]"` 中；Manifest 和 Resource
业务字段的名称不得包含 `[` 或 `]`。例如，一个同时被两个 Driver 关注的
Resource 会呈现为：

```json
{
  "path": "/agents/reviewer",
  "metadata": {
    "manifest": "/manifests/agent",
    "name": "reviewer",
    "state": "available",
    "[kas]": {
      "revision": 4,
      "observed": {
        "/manifests/agent/driver": {
          "driver_revision": 2,
          "resource_revision": 4
        },
        "/manifests/audit/driver": {
          "driver_revision": 1,
          "resource_revision": 4
        }
      },
      "created_at": "2026-07-26T00:00:00Z",
      "updated_at": "2026-07-26T00:05:00Z"
    }
  },
  "spec": {"model": "gpt-5"},
  "status": {
    "metadata": {
      "manifest": "/manifests/agent",
      "name": "reviewer",
      "state": "available",
      "[kas]": {
        "revision": 4,
        "observed": {
          "/manifests/agent/driver": {
            "driver_revision": 2,
            "resource_revision": 4
          },
          "/manifests/audit/driver": {
            "driver_revision": 1,
            "resource_revision": 3
          }
        },
        "created_at": "2026-07-26T00:00:00Z",
        "updated_at": "2026-07-26T00:05:00Z"
      }
    },
    "spec": {"model": "gpt-5"}
  }
}
```

根级 `metadata["[kas]"].observed` 是每个匹配 Driver 应消费到的目标版本；
`status.metadata["[kas]"].observed` 是已实际完成的版本。上例中 Agent Driver
已经完成，Audit Driver 仍落后一个 Resource revision。负责该 Manifest 的
owner Driver 在自己的消费版本落后，或根级 metadata/spec 与 status
metadata/spec 任一字段不同时收到 reconcile；只 watch 的 Driver 仅比较自己
的 observed 条目。

Driver spec 使用 `manages` 声明由该 singleton 负责推进 status 的 Manifest。
每个 Manifest 最多只能映射到一个 Driver，但一个 Driver 可以管理多个
Manifest；未显式声明时，Package 展开器会填入该 Driver 所属的 Manifest。
`watches` 用于声明只需额外观测的 Resource：

```json
{
  "manages": [
    "/builtin/relation",
    "/builtin/link"
  ],
  "watches": [
    {
      "manifest": "/builtin/link",
      "paths": ["/manifests/message/relations/recipient/links/**"]
    },
    {
      "manifest": "/manifests/integration-*",
      "paths": ["/resources/integrations/**"]
    }
  ]
}
```

Manifest 和 path pattern 支持精确匹配、段内 `*`、单段 `*` 和递归 `**`。
watch 不理解 Relation；需要只消费某类 Link 时，应使用普通 path 分区，或者
由 Driver 读取 `spec.relation` 后自行过滤。KAS 为每个匹配的
Driver/Resource 组合独立投递；完成后把消费版本写入
`status.metadata["[kas]"].observed[driver_path]`。Driver 定义 revision 或
Resource revision 任一变化都会重新投递。新 Driver 会回扫已有 Resource；
新 Manifest 注册后，已有通配符 watch 会立即覆盖包内初始化 Resource。
消费进度是唯一事实来源；KAS 直接扫描 observation 差异生成工作，不维护
持久化 reconcile queue。

Driver 和 Run 也是 Resource，但它们由 KAS 的 built-in Manifest 赋予控制面
状态机。Driver 的根级 `metadata.state` 表示期望启动或停止，
`status.metadata.state` 表示当前进程状态；generation 保存在
`metadata["[kas]"].generation`，PID 和 heartbeat 只属于 Supervisor 的进程
内状态。Run 的 Driver generation、开始时间、结束时间和结果均写入 Run
Resource，并同步到 status.spec。它们不会错误地交给“Driver Manifest 的
Driver”做自我协调。

Resource spec 可以通过 `PATCH /resources/by-path?path=...` 更新，请求必须携带
`expected_revision`，并可同时提交新的 `metadata.state`。更新成功后
`metadata["[kas]"].revision` 递增；旧 revision 会收到冲突响应。
archive、restore 等仍是 Manifest 自己定义的业务状态。

`DELETE /resources/by-path?path=...&expected_revision=N` 不会绕过 Driver：
KAS 先把 `metadata.state` 改为 `deleted`，所属 Driver reconcile 后把
`status.metadata.state` 改为 `deleted`。内置 Relationship Driver 会先按 Relation
删除策略清理 Link 或请求级联删除；所有匹配 Driver 都消费当前 revision 后，
KAS 删除 Resource 和相关运行数据，不保留 tombstone，也暂不提供 force
delete；原 path 随后可以重新使用。

Action、Relation、Driver、ServiceAccount 和 Role 都拥有 Manifest 下的独立
Path。Manifest 安装时，KAS 根据 built-in Relation 的语义 role 自动创建
Manifest 到初始化 Resource、Driver 到 ServiceAccount 的受保护 Link；RBAC
授权本身直接由 Subject 到 Role 的 role-binding Link 表达。创建 Run 时，KAS 同样创建 Run 到目标
Resource、Action 和 Driver 的受保护 Link。Resource 的类型身份直接由
envelope 中不可变的 `manifest` path 表达，不再重复创建类型 Link。

系统初始化逻辑按 Relation 的语义 role 创建普通 Link Resource，不把具体
Relation path 写死在业务逻辑中。Driver 凭据、RBAC、Run 和 Package 启动所需
的映射直接从 Resource spec 与 Link 推导，不依赖 Relationship Driver
启动完成，从而避免 bootstrap 循环。

Relation 只声明允许的端点、metadata schema 和删除策略，不声明数量约束，
也不承担 Driver 触发语义。`/builtin/link` 包提供一个 singleton
Relationship Driver，同时管理 Relation 和 Link 两个 Manifest，并 watch
所有 Resource；它负责 Relation status、端点校验、`unlink`/`cascade` 和
Link status。业务上的数量与关系平衡仍由相应业务 Driver 使用普通 mutation
维护。

Link 不再拥有单独的 CRUD。客户端使用通用 `/resources` 创建、读取、更新和
删除 Link Resource，并使用
`GET /resources?manifest=/builtin/link` 列出 Link。API 创建成功只表示
Resource 已持久化；调用方应以 `status.metadata.state == "available"` 判断
Link 已通过内置 Driver 校验。

Event 是平台维护的内部持久化审计日志，只能随业务对象事务自动产生，
不提供业务创建接口，也不参与 Driver 工作投递。

## Driver WebSocket

Driver 使用 `/drivers/connect?path=...&generation=N` 建立带 Bearer Token 的
WebSocket，不再依赖 claim 轮询。控制面主动推送 reconcile、Run 和 stop，
Driver 在同一连接上返回 ack，并将所有业务写操作统一放进一条 `mutation`
消息。in-flight delivery 仅存在于当前 API 进程内；断线或服务重启后，KAS
重新扫描尚未收敛的 observation 与 queued/running Run 并生成新 delivery，
因此不需要持久化投递副本。

`hello.driver`、`reconcile.resource` 以及 Run 投递中的 `run`、`resource`、
`action` 都使用同一个 Resource envelope。Driver 的异步
`reconcile(&Resource)` 直接返回 mutation；它不需要判断另一套 ObjectKind，
Link 也只是 manifest 为 `/builtin/link` 的 Resource。

Mutation 只保留 `create_resource`、`update_resource`、`delete_resource`、
`update_resource_status` 和 `complete_run`。Driver 因而可以在同一事务中创建
任意获授权的 Resource，包括 ServiceAccount、Role 或 Link。
空 mutation 表示当前 Driver 已经消费该 Resource revision；KAS 原子完成
delivery，并写入该 Driver 自己的
`status.metadata["[kas]"].observed` 条目。如果处理期间 Resource 或 Driver
revision 已推进，新版本仍会继续排队。Run mutation 必须
以该 Run 的 `complete_run` 结束。KAS 返回 `mutation_result`，只有
`committed` 才表示整组写入和消费确认成功。

Driver 的 `hello`、ack、reconcile 状态回写和 Run 完成属于控制协议，由绑定
generation 的 Credential、Driver 身份、in-flight delivery 和目标对象共同
授权，不要求 Manifest 猜测这些基础协议权限。mutation 中额外的业务写操作
仍按 Driver ServiceAccount 的精细 RBAC 验证，并和 delivery 完成在一个
SQLite 事务中提交；任一操作失败时整组回滚。跨 Manifest fanout 需要显式给
Driver ServiceAccount 绑定目标 Manifest、verb 和 path 对应的 Role。

现有 REST 写接口继续保留；统一的 `mutation` 入口只存在于 Driver
WebSocket 协议中，不提供对应的 HTTP endpoint。

Driver 不订阅 Event。需要 Driver 处理的业务变化必须显式表示为 Resource
revision，并由所属 Manifest 或 Driver `watches` 选中。工作项直接由 Resource
metadata 中的期望/实际 observation 差异生成，不依赖 Event sequence 或
额外队列表。

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
  → 验证自描述根 Manifest 和系统 Manifest Resource 已存在
  → 验证内置 Relationship Driver 的单一真实进程 running
  → 把编译后的 kas-test-driver 打进 .kas tar
  → POST /packages 安装 Manifest 包
  → 验证 Package Resource 及 Package → Manifest Link
  → 验证 Manifest 初始化 Resource 和 RBAC/Driver 关系的受保护 Link
  → Supervisor 启动真实 binary
  → Driver 通过 WebSocket hello
  → 验证新 Driver 回扫并消费注册前已有的 User Resource
  → 注册新 Manifest，验证已有通配符 watch 消费其初始化 Resource
  → 使用通用 API 创建 Link，并由内置 Relationship Driver 校验为 available
  → 使用通用 API 创建 Run Resource
  → 验证 Run 到 Resource/Action/Driver 的系统 Link
  → Driver 执行 echo 并完成 Run
  → DELETE Resource，经 Driver reconcile 后验证硬删除及 path 可复用
  → 创建 User/Role/role-binding Link/Credential Resource 并验证 RBAC
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

### 端到端性能测试

`benchmarks/kas-benchmark` 是独立的黑盒 Benchmark crate。它启动真实
`kas-api`、内置 Driver 和动态生成的 singleton Driver 进程，通过正式 HTTP
接口安装 Package 和创建 Resource，并通过正式 WebSocket 协议完成 reconcile；
测试代码不会直接访问 SQLite。

运行短 smoke：

```bash
./benchmarks/kas-benchmark/run.sh smoke
```

运行多维度扫描或自动寻找首次违反 SLO 的规模：

```bash
./benchmarks/kas-benchmark/run.sh sweep \
  --profile benchmarks/kas-benchmark/profiles/scale.json

./benchmarks/kas-benchmark/run.sh find-limit \
  --profile benchmarks/kas-benchmark/profiles/limit.json \
  --dimension resources
```

结果写入 `benchmark-results/`，包含请求与进程采样、JSON 汇总、Markdown
报告、测试配置和服务日志。可扫描 Resource、Manifest、Driver、Resource
大小、spec 字段数量、嵌套深度、watch fanout、并发度和 Driver 处理延迟。
