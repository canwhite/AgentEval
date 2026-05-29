# `&mut` 解引用 `*` 通俗理解

`*` 就是**拆包裹**：

```rust
let mut x = 42;
let r = &mut x;   // r 是一个"包裹"，里面装着 x 的地址
*r = 10;          // *r = 拆开包裹，摸到里面真的 x，把它改成 10
// 现在 x == 10
```

## 在 proxy.rs 里的实际场景

```rust
*response.status_mut() = status;
*response.headers_mut() = resp_headers;
```

拆开理解：

```rust
// status_mut() 返回 &mut StatusCode，是一个"装着 StatusCode 地址的包裹"
// * 拆开包裹，拿到里面真实的 StatusCode
// = status 把上游的真实状态码写进去，覆盖默认的 200
```

```rust
let 包裹: &mut StatusCode = response.status_mut();
*包裹 = status;
```

不加 `*` 编译报错：

```
error: mismatched types
  expected `&mut StatusCode`
  found `StatusCode`
```

左边是"包裹类型"，右边是"值类型"，类型对不上。`*` 告诉编译器：别管包裹，我要改**包裹里面的东西**。

## 规律：什么时候要 `*`，什么时候不用

**要 `*`**：方法返回 `&mut T`，你直接 `=` 赋值。

```rust
let mut v = vec![1, 2, 3];
let first = &mut v[0];
*first = 99;  // 通过 &mut 引用修改值
```

```rust
let mut map = HashMap::new();
map.insert("a", 1);
*map.get_mut("a").unwrap() = 10;  // &mut 引用，要拆开
```

**不用 `*`**：调用的是**方法**，Rust 自动帮你拆。

```rust
v.push(4);                          // push 是方法，自动解引用
map.insert("b", 2);                 // insert 是方法
response.headers_mut().insert("x-custom", "hello");  // insert 内部自己搞
```

总结：**`.` 调方法时 Rust 自动拆，`=` 赋值时必须手动 `*`。**
