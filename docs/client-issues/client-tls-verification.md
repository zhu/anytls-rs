# [client] CLI 默认接受任意证书，并跳过 TLS 握手签名校验

<!-- anytls-client-audit: client-tls-verification -->

## 范围与优先级

建议优先级：**P1**。本 issue 只跟踪仓库自带的 **Rust 客户端/客户端库**；不影响本次 Rust 服务端配合 Shadowrocket / dae 的发布范围。

审计源码固定为 `9424f3d83836649590a6ae452b2528e00d8b0822`。已在本地有界复现，未修改生产代码。

## 问题与影响

CLI 无条件调用标注为测试用途的 `create_client_config()`。其中 verifier 对证书链、服务器名称、有效期全部返回成功；TLS 1.2 / 1.3 的握手签名回调也直接返回成功。CLI 的 `--sni` 只改变发送的名称，并不能使校验生效。

定位：[src/bin/client.rs:121-133](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/bin/client.rs#L121-L133)；[src/util/tls.rs:102-136](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/util/tls.rs#L102-L136)；[src/util/tls.rs:157-174](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/util/tls.rs#L157-L174)。

这使客户端无法验证连接到的 AnyTLS 服务端身份。能冒充 TLS 对端的一方可以获得该层承载的数据和认证哈希。应用自身的独立 HTTPS 等加密仍有其保护，不能据此声称所有应用明文都会泄露。库调用方可以传入自行配置的 `TlsConnector`，受影响范围主要是捆绑 CLI 和使用该 helper 的调用方。

## 已执行复现

本地 TLS 服务端使用 `create_server_config()` 生成仅供 localhost 使用的自签证书。客户端使用 CLI 同款 `create_client_config()`，以 `ServerName::try_from("wrong-name.example")` 连接，TLS 握手成功并读到服务端的 `accepted` 数据。这同时验证了不受信任证书与名称不匹配仍被接受。

握手签名无条件成功来自上面两个 verifier 回调的静态证据；本次没有伪造 TLS CertificateVerify 签名的运行试验。

## 与参考实现的区别

Go 仓库的示例 CLI **同样默认 `InsecureSkipVerify: true`**，并在注释中明确仅适合示例，不应作为生产建议：[cmd/client/main.go:69-76](https://github.com/anytls/anytls-go/blob/fd6167acd6d73b9fa3e607659951847fbc9e6c50/cmd/client/main.go#L69-L76)。因此这不是“Go 示例默认安全而 Rust 不安全”的结论。Rust 此处额外直接放行握手签名，也不应被测试模式沿用。

## 预期修复与验收

- CLI 默认验证信任链、服务器名称和有效期；支持配置 CA 或证书 pin。
- 测试自签场景用显式 insecure 选项，且不把不校验证书与跳过握手签名混为一体。
- 分别覆盖可信证书成功、不受信任/错误名称/过期证书失败，以及 TLS 1.2/1.3 无效握手签名失败。

## 依赖更新后的复核

在服务端发布候选提交 [`c250994`](https://github.com/zhu/anytls-rs/tree/c250994c468a7e68d3aba8b2d45816e0a9d55f74) 更新依赖后，本项已再次运行并复现。客户端相关生产源码未改变；[补充输出与固定依赖](https://github.com/zhu/anytls-rs/blob/custom/client-issue-drafts/docs/client-issues/repro/VALIDATION_c250994.md)保留了本轮结果。
