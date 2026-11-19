# [client] 认证分成多次 TLS 写入，无法连接 anytls-go 参考服务端

<!-- anytls-client-audit: client-auth-fragmentation -->

## 范围与优先级

建议优先级：**P1**。本 issue 只跟踪仓库自带的 **Rust 客户端/客户端库**；不影响本次 Rust 服务端配合 Shadowrocket / dae 的发布范围。

审计源码固定为 `9424f3d83836649590a6ae452b2528e00d8b0822`。已在本地有界复现，未修改生产代码。

## 问题与影响

`send_authentication()` 分别写入 32 字节密码哈希、2 字节 padding 长度、padding 本体。使用当前 tokio-rustls 时，这会拆开认证数据；Go 参考服务端第一次 TLS Read 未取得完整认证，因此进入 fallback 并关闭连接。密码正确也无法建立普通 TCP 代理，UDP 同样无法通过这一阶段。

定位：[src/util/auth.rs:57-85](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/util/auth.rs#L57-L85)；调用点：[src/client/client.rs:271-304](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/client.rs#L271-L304)。

协议明确要求认证的 packet0 不分包：[docs/protocol.md:140-144](https://github.com/anytls/anytls-go/blob/fd6167acd6d73b9fa3e607659951847fbc9e6c50/docs/protocol.md#L140-L144)。Go 客户端先拼好认证再一次写出：[cmd/client/myclient.go:48-61](https://github.com/anytls/anytls-go/blob/fd6167acd6d73b9fa3e607659951847fbc9e6c50/cmd/client/myclient.go#L48-L61)；Go 服务端一次读取后在同一 buffer 中检查哈希、长度和 padding：[cmd/server/inbound_tcp.go:30-59](https://github.com/anytls/anytls-go/blob/fd6167acd6d73b9fa3e607659951847fbc9e6c50/cmd/server/inbound_tcp.go#L30-L59)。

## 已执行复现

1. 从上述参考提交构建 `go build -o anytls-go-server ./cmd/server`，启动 `LOG_LEVEL=debug ./anytls-go-server -l 127.0.0.1:<port> -p audit-only`。
2. 在另一个本地端口启动 TCP listener，调用当前 Rust `Client::new(..., PaddingFactory::default())`，随后 `create_proxy_stream(("127.0.0.1", target_port))`。
3. 3 秒外层截止时间内实际得到 `IO error: Connection reset by peer (os error 104)`；目标没有收到 TCP 连接，Go 日志显示 `fallback`。
4. 对照只把认证改为 `hash || 30u16.to_be_bytes() || [0; 30]` 的 **一次 64 字节 `write_all`**，其余 Rust TLS / Session / SYN / 目标地址代码不变：成功收到 SYNACK，目标 listener 接受连接。

这是实际 Go 参考服务端的对照试验，不是仅以 Rust 服务端模拟 Go 的行为。错误文本可能随系统时序变化，认证阶段被断开及对照成功才是判据。

## 预期修复与验收

- 拼接完整认证头和 padding0 后统一写入 TLS，并在最后 flush；不把要求错误表述为“必须同一个 TCP 包”。
- 用真实 Go 参考服务端验证默认 padding0、零长度 padding0 均能认证和建立目标连接。
- 回归覆盖认证数据跨底层 TCP 分段但同一 TLS 认证记录的情况，避免把网络分段误判为认证协议错误。

## 依赖更新后的复核

在服务端发布候选提交 [`c250994`](https://github.com/zhu/anytls-rs/tree/c250994c468a7e68d3aba8b2d45816e0a9d55f74) 更新依赖后，本项已再次运行并复现。客户端相关生产源码未改变；[补充输出与固定依赖](https://github.com/zhu/anytls-rs/blob/custom/client-issue-drafts/docs/client-issues/repro/VALIDATION_c250994.md)保留了本轮结果。
