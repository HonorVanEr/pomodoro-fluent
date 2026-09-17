# 长期记忆交给 OpenViking —— 方案、提示词与取舍

> 结论：**不自建记忆层。** 记忆的存储、向量化、分层（L0/L1/L2）、检索、自迭代
> 全部交给 OpenViking —— 它本身就是「给 Agent 用的上下文数据库」，记忆是它的主场。
> 本项目只做一件事：**给 agent 一段提示词，让它知道什么时候读、什么时候写。**
>
> 番茄钟因此回归**纯计时器 + agent hook**：不存索引、不做检索、不做归纳、不联网，
> 与 OpenViking 之间**没有任何桥**（不导出、不推送、不互相依赖）。

---

## 1. 为什么一行提示词就够

因为 OpenViking 提供了一条「一发入魂」的写记忆命令：

```bash
ov add-memory "key insight from today's debugging session"
```

配套读侧：

```bash
ov find "上次那个登录超时我是怎么修的"      # 语义检索
ov ls viking://user/memories                # 列已有记忆
ov read  viking://user/memories/xxx         # 读全文（L2）
```

也就是说，**记忆的存储、向量化、分层、检索、自迭代全部由 OpenViking 负责**，
agent 侧只需要「会跑 bash + 知道该记什么」。这跟番茄钟 hook 的设计哲学完全一致：
不引入 MCP、不引入 SDK，agent 会跑命令就行。

顺带解决的问题：

- 自建检索层（`LIKE` 子串匹配）→ 换成目录递归 + 向量语义检索，质量不在一个量级
- 自建分层 → OpenViking 摄入时自己生成 L0/L1/L2，按需加载省 token
- 自建归纳入口 → 由 OpenViking 侧的模型做，我们不用管模型选型
- **许可证** → 从「可能要把 AGPL 组件接进 MIT 项目」变成「调用一个外部 CLI」，边界干净

---

## 2. 三步装好

### ① 装 CLI（Windows x64 有官方包）

```bash
npm i -g @openviking/cli
# 备选：curl -fsSL https://openviking.tos-cn-beijing.volces.com/cli/install.sh | bash
```

### ② 起服务端

```bash
pip install openviking
openviking-server init        # 首次
openviking-server doctor      # 校验模型配置是否可用
openviking-server             # 默认监听 1933
```

`~/.openviking/ov.conf` 里要配 **Embedding 模型（必须）** 和 VLM（可选但推荐）。
可以试试把 `provider` 设为 `openai`、`api_base` 指向内部 OpenAI 兼容端点
（`http://10.118.8.123:8317/v1`）来避开出网 —— **但前提是该端点暴露了 embedding 模型，需要先验证**。

### ③ 配 CLI 指向服务端

`~/.openviking/ovcli.conf`：

```json
{
  "url": "http://localhost:1933",
  "api_key": "",
  "output": "table",
  "timeout": 60.0
}
```

验证：

```bash
ov health        # 通 → 可以往下
ov version       # 顺手确认 CLI / 服务端版本
```

---

## 3. 给 agent 的提示词（可直接粘贴）

> 下面这段已经写进 `~/.claude/CLAUDE.md`（Claude Code 全局生效）。
> 其他宿主放到各自的规则文件即可：Codex → `~/.codex/AGENTS.md`，
> 项目级 → 仓库根的 `AGENTS.md` / `CLAUDE.md`。

```markdown
## 长期记忆（OpenViking）

长期记忆统一放在 OpenViking，不要写进对话历史、不要另建存储。

**可用性**：先 `ov health`。若命令不存在或服务不通 → 本节整体跳过，
按平常方式干活，不要因此报错或提醒用户。

**读取（需要历史上下文时才查，不要每轮都查）**
- `ov find "<关键词>"` 语义检索；或 `ov ls viking://user/memories` 浏览
- 命中后用 `ov abstract <uri>`（L0 摘要）先判断，确有必要再 `ov read <uri>` 读全文

**写入（一轮任务真正收尾、且产生了可复用结论时）**
- `ov add-memory "<一条记忆>"`，一条一个主题

**值得记**
- 用户的偏好与决策依据（谁要求的、为什么、有什么约束）
- 踩过的坑与最终解法（现象 → 根因 → 有效做法）
- 项目约定（构建/部署命令、目录规范、代码风格、内部端点）
- 工具与接口的用法要点（哪个端点、哪个参数、什么坑）

**不要记**
- 一次性命令输出、临时路径、时间戳
- 密钥、token、凭据、内部敏感信息
- 流水账（"改了 3 个文件"）
- 代码或 README 里已经写清楚、`ov find` 本来就能查到的东西

**写法**
- 写结论，不写过程：「迁移用 X 而不是 Y」而不是「我先试了 Y 发现不行」
- 带上适用条件：哪个项目 / 哪个版本 / 什么场景
- 中文，一条 1–3 句

**禁止**
- 不要把 OpenViking 的内容整段贴回对话，只取需要的那一两句
- 记忆里没有的就直说没有，不要编造
```

---

## 4. agent 侧的完整命令面

| 命令 | 用途 |
|---|---|
| `ov health` | 可用性检查（提示词的开关就靠它） |
| `ov add-memory "<文本>"` | 写一条记忆 |
| `ov find "<查询>"` | 语义检索（带 score） |
| `ov ls viking://user/memories` | 列记忆 |
| `ov abstract <uri>` | 读 L0，~100 token，省着用 |
| `ov overview <uri>` | 读 L1，~2000 token |
| `ov read <uri>` | 读 L2 全文 |
| `ov add-resource <路径\|URL> --wait --reason "..."` | 把文档/代码库喂进去 |
| `ov grep "<正则>"` | 内容正则匹配 |
| `ov glob "**/*.md" --uri viking://resources` | 文件匹配 |
| `ov session new` → `add-message` → `session commit` | 会话式记忆（自动抽取） |
| `ov -o json <cmd>` | JSON 输出，给脚本消费 |

> 命令集以 `ov --help` 为准。`add-memory` 在 CLI 0.4.x 的 npm 包说明里有，
> 但官方文档站的部分页面还没列出来 —— 装完先跑一次 `ov --help` 确认。

---

## 5. 提示词覆盖不到的边界

提示词驱动的是**agent 自己的会话记忆**：你说了什么、它做了什么、结论是什么。
它天然记不住两类东西：

| 记不住的 | 原因 |
|---|---|
| **时长** | 会话没有明确的起止；「45 分钟」是番茄钟才知道的 |
| **没跟 agent 交互的工作** | 读代码、开会、手工调试、翻文档 —— 根本没进会话 |

所以要先回答一个问题：

- **只要「agent 记忆」**（偏好 / 踩坑 / 项目约定）→ **提示词就够了，番茄钟完全不参与。**
  它回去当纯计时器 + agent hook；本项目里跟「记录 / 记忆」有关的那套东西已经**全部砍掉**。
- **日报 / 周报 / 月报**（最初的需求）→ **本项目已不做**。2026-09-16 起，番茄钟不再记录
  工作流水（`reports.js` / 报告抽屉 / 窗口采样 / git 证据全部删除），日报周报改为
  **完全交给 agent 自己的记忆服务**：让 agent 把「今天干了什么」用 `ov add-memory` 记进去，
  再让它按需检索成稿。番茄钟只提供时长——那是它唯一不可替代的东西。

> **不做「专注结束自动写记忆」的开关**：那会让番茄钟反过来依赖 OpenViking，
> 它没启动时就得处理超时 / 失败。番茄钟对 OpenViking 的依赖数**永远是 0**。

---

## 6. 已砍掉的东西（2026-09-16 落地）

按「记忆交给 OpenViking」这个前提，下面这些已经**真的删掉**了：

| 文件 / 能力 | 处理 |
|---|---|
| `store.js`（整层 SQLite 索引 / 检索 / 统计） | **已删除文件** |
| `bin/worklog.js`（分层目录导出 CLI） | **已删除文件** |
| `docs/openviking-integration.md` / `docs/work-report-design.md` | **已删除**，有效内容并入本文 |
| **`reports.js`（整个 JSONL 事实账本）** | **已删除文件** |
| **`activity.js`（前台窗口采样）** | **已删除文件** |
| **`evidence.js`（git 代码证据）** | **已删除文件** |
| **`renderer/report.js`（报告抽屉）** | **已删除文件**，连同标题栏报告按钮、设置里的「工作记录」区块 |
| `records-YYYY.jsonl` 之类的本地流水 | 不再写入 |

**结果**：番茄钟回归**纯计时器 + agent hook**。它只做三件事——
计时、托盘 / 悬浮窗体验、把 agent 的确认 / 权限 / 通知弹出来。
**一个字的工作流水都不记。** 要「记工作」，请让 agent 自己用 `ov add-memory` 写进记忆服务。

**代价说明**：本地不再有任何工作记录可查。要查历史，只能靠 agent 的记忆服务（`ov find`）。
这是有意的取舍——与其维护一份比专业记忆服务弱得多的自建流水，不如干脆只留
「番茄钟唯一不可替代」的那部分：**在你不说话的时候，它也知道你什么时候在专注、专注了多久。**

---

## 7. 附：与 OpenViking 打通的评估记录（为什么最终不打通）

这一节是决策留档，记录曾经评估过什么、为什么都没做。

### 7.1 两者各自是什么

| | 番茄钟（本项目） | OpenViking（火山引擎开源） |
|---|---|---|
| 定位 | 纯计时器 + agent 交互弹窗 | 面向 Agent 的**上下文数据库** |
| 数据来源 | 番茄计时；agent hook 仅用于弹窗，不落流水 | 文档、网页、会话提交 |
| 存储 | **无**（不自建任何记录层） | 虚拟文件系统 `viking://`（RAGFS） |
| 检索 | 不自建（无此能力） | 目录递归检索 + 向量语义检索 |
| 依赖 | 零依赖、零联网、零 API Key | 必须有 Embedding 模型，推荐再配 VLM |
| 许可证 | MIT | AGPL-3.0（另有资料写 Apache-2.0，对外发布前需自行核实） |

OpenViking 官方列出的痛点之一是「记忆只记录交互，缺少 Agent 任务相关的经验记忆」，
番茄钟恰好是它的补集：不依赖对话、带时间与时长、是第三方视角的被动传感器。
但它补上的只是边角 —— 上面的提示词方案已经把「偏好 / 踩坑 / 约定」这类
真正需要长期记忆的部分覆盖了，硬接一层收益有限。

### 7.2 评估过的三档方案（结论：都不做）

| 档位 | 做法 | 结论 |
|---|---|---|
| **A. 单向导出** | 番茄钟导出分层目录 → `ov add-resource` 摄入 | **曾实现，已撤销**。多一个 CLI、多一层产物要维护；后来连「本地流水」本身也一并砍掉了 |
| **B. 双向** | 开专注时反查 OpenViking，把相关记忆显示在窗口 | **不做**。番茄钟要拿 API Key、处理服务不可用、新增 UI 位，复杂度与收益不成正比 |
| **C. 只做形状对齐** | 导出目录当人工可读工作日志 | 随 A 一起撤掉了 |

保留的只有第 5 节那条：**给 agent 一段提示词**。它成本接近 0，却拿到了 OpenViking 的
核心价值（语义检索 + 跨会话记忆 + 分层投喂）。

### 7.3 真要自己装时，这些坑还在

| 坑 | 说明 |
|---|---|
| **Embedding 模型是硬门槛** | OpenViking 必须配 Embedding（L0/L1 也读它），只配 VLM 不够。想用内部 OpenAI 兼容端点（如 `http://10.118.8.123:8317/v1`）需先确认它暴露了 embedding 模型，否则只能走火山引擎 / OpenAI 官方。 |
| **许可证** | 主流口径是 AGPL-3.0，对「本地自用」无影响；但**不要把 OpenViking 代码 bundle 进这个 MIT 仓库**。 |
| **鉴权分两级** | 服务端开启鉴权后，`root_key` 调 `add_resource` / `find` 这类 tenant 数据 API 会 `PERMISSION_DENIED`，要用 `user_key`（或 admin key）。 |
| **CLI 命令名以 `ov --help` 为准** | `add-memory` 在 CLI 0.4.x 的 npm 包说明里有、官方文档站部分页面还没列出来，装完先跑一次确认。 |
