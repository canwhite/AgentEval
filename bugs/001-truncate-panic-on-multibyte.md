# Bug 001: `truncate` panic on multi-byte characters

**状态**: 已修复  
**文件**: `src/grader/prompt.rs:97-104`  
**严重度**: 严重（panic）

## 原因

`&str[..n]` 按**字节**索引，`n` 落在多字节字符（中文、emoji 等）中间时会 panic。

```rust
// BUG 版本
fn truncate(s: &str, max_len: usize) -> String {
    let s = s.trim();
    if s.len() <= max_len {          // .len() 返回字节数！
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])  // 字节切片，可能切在字符中间 → panic
    }
}
```

触发的具体场景：
```
s = "你好世界" (12 字节, 4 字符)
max_len = 5  char

s.len() = 12 > 5, 走 else 分支
&s[..5] → 字节 0-4，但 "你" 占字节 0-2，"好" 占字节 3-5
字节 5 在 "好" 的中间 → PANIC
```

## 修复

改用 `char_indices()` 按**字符边界**定位：

```rust
fn truncate(s: &str, max_len: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let end = s
            .char_indices()
            .take(max_len)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        format!("{}...", &s[..end])      // end 一定在字符边界上
    }
}
```

`char_indices()` 返回 `(字节偏移, 字符)` 迭代器，`take(max_len)` 取前 N 个字符，最后一个的 `i + c.len_utf8()` 就是安全切片的字节位置。
