# Agent Note: 测试不许继承主机时区

Status: implemented

## Problem

CI 首次上线时，`server-tests`（`windows-latest`，默认 UTC）在 `store::tests::since_from_days_is_an_exact_rfc3339_boundary` 上变红：

```
left:  "2026-10-15T04:00:00.000Z"        ← UTC runner 上算出来的
right: "2026-10-15T12:00:00.000+08:00"   ← 测试里写死的期望值
```

根因不是产品缺陷，而是**测试把主机时区写进了断言**。`since_from_days` 按传入 `now` 的时区渲染截止时刻 —— 这是对的。但测试这样构造：

```rust
let now = parse_from_rfc3339("2026-10-16T12:00:00.000+08:00")
    .with_timezone(&chrono::Local);        // 换成主机时区
assert_eq!(one, "2026-10-15T12:00:00.000+08:00");  // 又断言 +08:00 字面量
```

`.with_timezone(&Local)` 在 +08:00 机器上等于没变，在 UTC runner 上把时刻换成了 UTC，于是渲染出 `Z` 后缀。断言测的是**主机时区**，不是函数。

这类缺陷的严重性在于**本机永远绿**：开发机时区是 +08:00，怎么写都过。它是 CI 存在的直接理由之一，却也是 CI 独有的失效面。

## Decision

**测试不得用 `chrono::Local` 构造被测值，也不得把主机时区当作期望值的一部分。时区在断言里显式钉死。**

`since_from_days` 泛型化到任意时区：

```rust
pub fn since_from_days<Z: chrono::TimeZone>(days: Option<u32>, now: chrono::DateTime<Z>) -> Option<String>
```

生产调用点（`admin.rs` 三处 `chrono::Local::now()`）不受影响：泛型参数由调用处推断，行为与改动前逐字节相同。

测试用 `FixedOffset` 钉 +08:00，并**额外加一条 UTC 断言**证明函数渲染的是传入时区而不是硬编码的偏移：

```rust
let plus8 = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
let now = parse_from_rfc3339("2026-10-16T12:00:00.000+08:00").unwrap().with_timezone(&plus8);
assert_eq!(since_from_days(Some(1), now).unwrap(), "2026-10-15T12:00:00.000+08:00");

// 同一时刻用 UTC 表示：2026-10-16T04:00:00+08:00 == 2026-10-15T20:00:00Z
let utc_now = parse_from_rfc3339("2026-10-16T04:00:00.000+08:00").unwrap().with_timezone(&chrono::Utc);
assert_eq!(since_from_days(Some(1), utc_now).unwrap(), "2026-10-14T20:00:00.000Z");
```

**为什么另两个窗口测试（`a_days_window_includes_recent_rows_and_excludes_old_ones`、`a_days_window_also_filters_the_log_pages_and_their_total`）也要钉偏移**：它们插入的行带着 `+08:00` 时间戳，而 `build_filters` 把 `ts` 当**字符串**比较（这是刻意的，为了吃 `idx_rl_ts` 索引）。若截止时刻用另一个时区渲染，比较的就是两套日历。当前用例的日期相隔 1 天与 45 天，8 小时偏移翻不翻盘是碰运气 —— 钉死之后不靠运气。

**这条约束的适用范围**：任何跨主机环境运行的测试。`chrono::Local`、`std::env::temp_dir()` 之外的宿主状态同理，但本地时钟是最容易漏的一个。

## Alternatives considered

- **在 CI 上设置 `TZ=Asia/Shanghai`，让 runner 与开发机一致。** 最强理由是最省事：一行 `env:` 就能让现有断言全部成立，不必改代码与测试。否掉的原因有两个：一是**那等于把缺陷藏起来** —— 测试仍然依赖主机时区，换个开发者、换台机器、或者 runner 镜像改了默认时区，问题原样复现；二是仓库若要支持其他时区的使用者，这种测试会持续误报。门禁应当对宿主状态不敏感，而不是要求宿主迁就它。
- **保留 `Local`，把断言改成「与 `Local` 渲染出的同一偏移」的比较（即断言两边都用 `Local`）。** 最强理由是它仍能验证「恰好往前 1 天」这个核心语义，且不改生产函数签名。否掉的原因是它把期望值与被测值绑在同一个时区来源上，**时区渲染 bug 就测不出来了** —— 如果函数错误地忽略传入时区、改用 `Local`，这条断言照样绿。独立的期望值才是有意义的期望值。
- **不改函数签名，测试里直接构造 `FixedOffset` 的 `DateTime` 再转 `Local`。** 最强理由是零生产代码改动。否掉的原因是转过 `Local` 之后偏移信息就丢了，正是问题本身；不转又过不了 `DateTime<Local>` 的类型检查。

## Consequences

- **收益**：测试从「依赖主机时区」变成「对主机时区不敏感」。测试模块内 `chrono::Local` 的出现次数为 **0**，这一点是可机械核对的。
- **收益**：新增的 UTC 断言堵住了一个此前没有防线的退化路径 —— 把 `since_from_days` 改成硬编码 `+08:00`（或忽略传入时区）现在会立刻变红。
- **代价**：`since_from_days` 多了一个泛型参数，签名比原来复杂。调用点无需改动，但读签名的人要理解泛型的用意。
- **代价**：测试里的时间戳字面量与 `FixedOffset::east_opt(8 * 3600)` 在三个测试里重复出现，可以抽一个辅助函数；当前保留重复是为了让每个测试自解释。
- **已知上限**：这只治了「测试依赖时区」这一种。**生产代码本身仍按主机时区渲染时间戳并落库**，字符串比较依赖「本机 UTC 偏移不变」这个前提（已在 `store.rs` 的 `build_filters` 文档里写明）。跨时区迁移数据或机器改时区仍会破坏时间窗口 —— 那是产品侧未解的问题，不是本次要修的。

## Verification

- `cd server && cargo test --release --locked` → **202 passed / 0 failed / 0 ignored**，exit 0。
- 故障注入证明新断言有效：把 UTC 那条期望值改回 `+08:00`（模拟函数忽略传入时区），`store::tests::since_from_days` 变红（`0 passed; 1 failed`）；复原后变绿。
- 结构性证据：测试模块内 `chrono::Local` 出现 0 次（`awk 'NR>956' src/store.rs | grep -c "chrono::Local"` → 0）。
- GitHub Actions 实跑（run 39018679276）：`server-tests` 在 `windows-latest`（UTC）上全绿。
- **一次失败的验证方法**：最初试图用 `TZ=UTC` 在本机复现，但探针实测表明 Windows 上 `chrono::Local` 忽略 `TZ` 环境变量，仍返回 `+08:00` —— 那次「UTC 下通过」是无效证据，已弃用，改以上面的结构性证据与 CI 实跑为准。
