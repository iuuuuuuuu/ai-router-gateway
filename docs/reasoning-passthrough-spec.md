# 思考（reasoning / thinking）透传实现规范

> 本文记录**为什么**网关曾经 100% 丢掉思考参数、**怎么补**、以及**哪些情况刻意不补**。

## 0. 根因：网关不是透明代理

网关收到客户端 body 后，**解析成协议无关的 IR（`ChatRequest`）**，再用上游协议的 codec **重新渲染**出上游 body
（`proxy.rs` 的 `render_upstream_request`）。因此：

- IR 里**没有建模的字段，不是「没转发」，而是被删除**。
- 修复前的 `ChatRequest` 只有 `messages/system/max_tokens/temperature/top_p/stop/stream/tools/tool_choice`，
  所以 `reasoning_effort`、`thinking`、`budget_tokens`、`enable_thinking` 全部消失在解析阶段。

**实测证据**（回显上游，把收到的 body 原样回给客户端）：

| 客户端发送 | 修复前上游收到 | 修复后上游收到 |
|---|---|---|
| `reasoning_effort=high` + `thinking={type:enabled,budget_tokens:8000}` + `extra_body.enable_thinking=true` | 只有 `keys=messages,model,stream` | `keys=enable_thinking,messages,model,reasoning_effort,stream,thinking`，三个值都在 |
| 对照：`temperature/top_p/max_tokens` | 正常出现 | 正常出现（未回归） |

响应方向同理：上游返回的 `reasoning_content` 在 `classify_upstream_frame` 里无人识别，
客户端 SSE 里既没有标记也没有字段名——**双向都是 0 透传**。

## 1. IR 层的三个新增（`server/src/protocol/`）

| 类型 | 位置 | 作用 |
|---|---|---|
| `ir::ReasoningCfg` | `ir.rs` | 请求侧：意图（`effort` / `budget_tokens` / `enabled`）+ 客户端**原始字段**（`raw`，带协议标签） |
| `ChatRequest.reasoning` | `mod.rs` | 让请求侧字段活过解析阶段 |
| `Content::Reasoning` / `StreamEvent::Reasoning` | `ir.rs` / `mod.rs` | 响应侧：思考**独立于正文**，任何渲染器都能选择drop 而不是混进正文 |

### 1.1 同协议时**原样回放**，跨协议才翻译

`ReasoningCfg::raw_for(protocol)` 只在「客户端协议 == 上游协议」时返回原文，
直接把客户端自己的 JSON 对象塞回上游 body。这是唯一不会**发明或误译**的路径。

跨协议时才翻译，且翻译规则写死并记录在案（`effort_to_budget`）：

| effort | budget_tokens |
|---|---|
| `minimal` / `low` | 4096 |
| `medium` 或无法识别的值 | 8192 |
| `high` / `xhigh` / `max` | 16384 |

这里有个**刻意的发明**：Anthropic / Gemini 都没有 effort 旋钮。如果不翻译，一个 OpenAI 客户端把
`reasoning_effort:high` 发到一个 Anthropic 上游，就会**完全没有思考**——正是本次要修的 bug。
所以宁可给一个约定俗成的预算值（不是测量值），也不让它静默失效。未知 effort 值也走 8192 而不是 0：
**不认识的取值必须仍然产生思考**。反向（budget → effort）同理：`≤4096→low`、`≤16384→medium`、更大→`high`。

## 2. 三协议字段名（parse + render 都已覆盖）

| 协议 | 请求侧读/写 | 响应侧读/写 |
|---|---|---|
| OpenAI | `reasoning_effort`、`reasoning.effort`、`thinking{budget_tokens,type}`、`enable_thinking`、`extra_body.enable_thinking` | `delta.reasoning_content`、`delta.reasoning` → `reasoning_content` |
| Anthropic | `thinking{type,budget_tokens}`、`reasoning_effort` | `thinking_delta`（流）/ `content[].type=thinking`（缓冲）→ `thinking_delta` |
| Gemini | `generationConfig.thinkingConfig{thinkingBudget,includeThoughts}`、`reasoning_effort` | `parts[].thought` → `parts[].thought=true` |

Gemini 的 `includeThoughts:true` 必须一起下发：只给 `thinkingBudget` 时 Gemini 会接受参数但**把思考剥掉返回**，
现象和「没修」完全一样。

## 3. 一个必须写清楚的坑：同一帧里既有思考又有正文

`Classified` 每个上游帧只能报一种变体。修复中曾把「思考优先」写成 `return Classified::Reasoning(...)`，
结果当上游在一个 delta 里**同时**给出 `reasoning_content` 和 `content` 时，**正文被静默删除**
（实测：`content` 只剩 ` tail`，`BOTH-TEXT` 消失，客户端还收到一个不应有的 repair 帧）。

因此新增 `Classified::ReasoningAndText { reasoning, text }` + `FrameClass::ReasoningAndText`：
**两半都往各自的目的地送**——思考进 `pre_reasoning`（提交前）/ 直接下发（提交后），
正文照常走 holdback 与循环守卫。混合帧不会成为守卫的盲区。

## 4. 刻意**不**透传的三处（诚实边界）

1. **缓冲响应的 Anthropic 思考块被丢弃**。Anthropic 在客户端把 thinking 块回放进下一轮 assistant
   消息时会校验 `signature`，而 IR 不携带签名。下发一个无签名块会把原本可用的多轮对话变成 400。
   流式路径不受影响（`thinking_delta` 不需要签名），所以实时读思考的客户端什么也没少。
2. **缓冲响应的 OpenAI 思考被丢弃**。`render_response` 不发明 `reasoning_content` 字段：
   客户端没要这个字段时给它加上，会改变它的消息解析。流式（`StreamEvent::Reasoning`）才是思考的出口。
3. **历史思考在请求侧被清空**（Anthropic 回放为 `text:""`，Gemini 保留 `thought:true`）。
   同上，无签名不能回放。

## 5. 验收（`server/tests/reasoning.rs`，12 条）

| 编号 | 断言 |
|---|---|
| r01 | OpenAI `reasoning_effort` 真的到达上游（回显上游读字节） |
| r02 | 客户端原始字段逐字回放（含 `thinking.budget_tokens`） |
| r03 | **没要思考的请求不得凭空多出字段** |
| r04 | Anthropic 20000 预算跨协议翻译成 `high` |
| r05 | OpenAI 流式思考到达且**不混入正文**（正文严格等于答案） |
| r06 | **只带思考的帧不得被丢弃** |
| r07 | Anthropic `thinking_delta` 透传 |
| r08 | Gemini `thought:true` 不被当成答案 |
| r09 | 缓冲 OpenAI：思考不混进正文 |
| r10 | 缓冲 Anthropic：思考被干净丢弃，**不产生占位空文本块** |
| r11 | 缓冲 Gemini：普通文本不被凭空标成 `thought` |
| r12 | **同帧混合：正文与思考都必须存活**，且不出现 repair 帧 |

故障注入（每条新增闸门都必须能变红，否则不算通过）：去掉上游 reasoning 分类 → r05/r06 红；
让 `insert_reasoning` 空转 → r01/r02/r04 红；去掉 `thinking_delta` 分类 → r07 红；
去掉 Gemini `thought` 判断 → r08 红；把普通文本标成 `thought` → r11 红；
回放未签名 Anthropic 思考块 → r10 红。