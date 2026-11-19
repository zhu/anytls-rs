# [client] UpdatePaddingScheme 在默认初始化后永远失败，后续会话也不继承配置

<!-- anytls-client-audit: client-padding-update -->

## 范围与优先级

建议优先级：**P2**。本 issue 只跟踪仓库自带的 **Rust 客户端/客户端库**；不影响本次 Rust 服务端配合 Shadowrocket / dae 的发布范围。

审计源码固定为 `9424f3d83836649590a6ae452b2528e00d8b0822`。已在本地有界复现，未修改生产代码。

## 问题与影响

`PaddingFactory::default()` 使用进程全局 `OnceLock::get_or_init`。`update_default()` 却试图再次 `OnceLock::set`；默认工厂初始化后 set 必然失败。CLI 在连接前就调用 default，因此收到服务器合法的 `UpdatePaddingScheme` 只记录 warning，继续使用旧策略。

定位：[src/padding/factory.rs:25-26](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/padding/factory.rs#L25-L26)；[src/padding/factory.rs:55-71](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/padding/factory.rs#L55-L71)；[src/session/session.rs:787-807](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/session/session.rs#L787-L807)；[src/bin/client.rs:135-136](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/bin/client.rs#L135-L136)。

另一个所有权问题是 Client 保存不可更新的 `Arc<PaddingFactory>`，新会话和下一次认证继续 clone 原值：[src/client/client.rs:287-304](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/client.rs#L287-L304)。仅把 OnceLock 换成全局可变锁仍不足以保证以后会话更新或多服务器隔离。

协议要求按 Client 保存服务端下发的策略，并让该 Client 后续的新会话使用它：[docs/protocol.md:111-130](https://github.com/anytls/anytls-go/blob/fd6167acd6d73b9fa3e607659951847fbc9e6c50/docs/protocol.md#L111-L130)。这影响策略轮换及流量形态，不只是日志显示。

## 已执行复现

最小确定性根因：

```rust
let before = PaddingFactory::default();
assert!(PaddingFactory::update_default(b"stop=2\n0=10-10\n1=123-123").is_err());
assert_eq!(PaddingFactory::default().md5(), before.md5());
```

实际输出：`failed to update default factory`，MD5 仍为 `75cff2ad89aadf5e257059ee571ebe11`。

另外通过 duplex 给客户端 Session 顺序发送 `UpdatePaddingScheme` 和 `HeartRequest` 作为处理屏障；旧策略为固定 30 字节，新策略为 200/240 字节。随后 HeartResponse 写入仍为 30 字节，确认接收处理路径确实继续使用旧策略。

## 预期修复与验收

- 在 Client 级共享可更新的 padding 状态；认证、现有 Session、后续 Session 的读取与更新规则明确。
- 更新成功时后续新会话的 padding-md5 与实际策略一致，padding0 也使用更新结果。
- 测试默认初始化后多次合法更新、非法更新保留旧值、重新连接继承更新，以及两个连接不同服务器的 Client 不相互覆盖。
- Go 代码自身也存在进程全局 factory 的历史设计；本项 Client 级隔离要求依据协议文档，不声称 Go 示例已经实现这一隔离。
