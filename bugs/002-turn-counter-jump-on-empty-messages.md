# Bug 002: `process()` turn_id jumps when messages is empty

**状态**: 已修复  
**文件**: `src/eval/mod.rs:128-142`  
**严重度**: 中等（数据错误）

## 原因

`turn_counter += 1` 和 `jsonl_ids.push()` 在 `current_messages.is_empty()` 检查**之前**执行。空 record 提前 return 后，Turn 没入栈但计数器已经跳号。

```rust
// BUG 版本
pub fn process(...) {
    self.turn_counter += 1;           // ← 已经 +1
    self.jsonl_ids.push(jsonl_id);    // ← 已经记录

    let current_messages = parse_request_messages(request_body);
    if current_messages.is_empty() {
        return;                       // ← 提前 return，Turn 没 push
    }
    // ...
    self.turns.push(turn);
}
```

连锁影响在 `extract_metrics`：

```rust
// 只统计最后 turn 的文本
if turn.turn_id == view.turns.len() as u64 {
    final_text_len += content.len();
    has_final_text = true;
}
```

假设 3 个记录，第 2 个 messages 为空：
- turn_id: 1, 3, 4（跳了 2）
- turns.len(): 3
- 最后一轮 turn_id=4，4 != 3 → **最后一轮文本永远不被统计**

## 修复

把计数操作移到空检查之后：

```rust
pub fn process(...) {
    // 先解析，避免空 messages 导致计数器跳号
    let current_messages = parse_request_messages(request_body);
    if current_messages.is_empty() {
        return;                       // 什么都不做就 return
    }

    self.turn_counter += 1;           // 确认有效再计数
    self.jsonl_ids.push(jsonl_id);
    // ...
}
```
