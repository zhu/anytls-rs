# [client] Settings 的 client 版本固定为 anytls-rs/0.1.0，与发布版本不符

<!-- anytls-client-audit: client-version-advertisement -->

## 范围与优先级

建议优先级：**P3**。本 issue 只跟踪仓库自带的 **Rust 客户端/客户端库**；不影响本次 Rust 服务端配合 Shadowrocket / dae 的发布范围。

审计源码固定为 `9424f3d83836649590a6ae452b2528e00d8b0822`。下述结论来自静态源码审查，未声称已做该场景端到端测试。

## 问题与影响

`Session::start_client` 固定发送 `client=anytls-rs/0.1.0`，而当前 Cargo 包版本为 `0.5.4-custom.2`。这影响服务端兼容性排查、按已知版本做告警/拒绝的策略和实际部署版本统计。

定位：[src/session/session.rs:1222-1234](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/session/session.rs#L1222-L1234)；[Cargo.toml:1-4](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/Cargo.toml#L1-L4)。协议要求第三方实现报告真实软件名称和版本：[docs/protocol.md:84-95](https://github.com/anytls/anytls-go/blob/fd6167acd6d73b9fa3e607659951847fbc9e6c50/docs/protocol.md#L84-L95)。

这是静态确定的元数据问题。`v=2` 是协议版本，并不应被替换成软件包版本；需要改的是 `client` 字段。

## 预期修复与验收

- 使用构建期包版本或统一 VERSION 常量生成 client 字段，custom 版本保留实际后缀。
- 编码一次初始 Settings，断言 client 与构建版本一致、v仍为2，避免修改错误的字段。
