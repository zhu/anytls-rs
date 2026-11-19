# Custom 服务端兼容性与验证

## 版本与范围

`custom-v0.5.4.2` 基于 `zhu/anytls-rs` 的 `9de6b200a6818b5f19e4a3e8af141ca4652396dd`。
Cargo 与二进制版本使用合法的 SemVer `0.5.4-custom.2`；GitHub tag 沿用这个 fork 的 `custom-v0.5.4.N` 规则。

本次验证针对 **Rust 服务端 + dae / AnyTLS Go 客户端传输层**。发布包仍包含 Rust 客户端，
但 Rust 客户端与 Go 服务端的完整互通、HTTP 代理行为不属于本次验证范围。

| 客户端 | 验证方式 | 覆盖与边界 |
| --- | --- | --- |
| dae v2.1.2 / 已检查的 main | 导入 dae 实际依赖的 AnyTLS transport，在回环网络连接 Rust 服务端 | TCP 完整性、EOF、FIN、TLS 会话复用、空闲心跳、拒绝连接、DNS 端口隔离、UDP v2 connect 模式；不包含 eBPF 数据路径 |
| anytls-go | 使用参考项目的 session.Client 与 sing 的 UoT 编解码器 | 分片 SOCKS 地址、TCP、会话复用、UoT v1 / v2、connect / 非 connect 模式、IPv4 / IPv6、零长度与大 UDP 包 |
| Shadowrocket（iOS） | 对照公开协议和 UoT 格式补齐服务端行为 | 没有 iOS 实机验证；不能据此承诺特定 App 版本、网络切换或后台恢复表现 |

### 固定的客户端源码

- dae stable：[v2.1.2 go.mod](https://github.com/daeuniverse/dae/blob/460cec77f4365abfd9ea6fae04b66466dd4861cb/go.mod)。
- dae main：[e35410568009a7e1b49090aaba265c9992fc6b46 go.mod](https://github.com/daeuniverse/dae/blob/e35410568009a7e1b49090aaba265c9992fc6b46/go.mod)。
- 两者实际选择的 outbound：[cc86ced2e683b91387f2cde7cfcadcb5ac2adbc3](https://github.com/olicesx/outbound/tree/cc86ced2e683b91387f2cde7cfcadcb5ac2adbc3/protocol/anytls)。
- AnyTLS 参考：[fd6167acd6d73b9fa3e607659951847fbc9e6c50](https://github.com/anytls/anytls-go/tree/fd6167acd6d73b9fa3e607659951847fbc9e6c50)，UoT codec 为 `github.com/sagernet/sing v0.5.1`。
- UoT 公开格式：[sing-box 文档](https://sing-box.sagernet.org/configuration/shared/udp-over-tcp/)。

## 修复的行为

- DNS 只缓存 IP，访问同一域名的不同端口不会串用端口；TCP 会尝试解析出的多个地址。
  UDP 对本地绑定、连接或首次发送的失败尝试后续地址；已成功发送的数据报不会因缺少响应而重发。
- 目标 EOF、连接失败与远端 FIN 都进入一致的流收尾路径；正确清理会话表、通知等待方并避免 FIN 回声。
- Session 关闭能够中断阻塞的读写；写错误不会在持有 writer 锁时再次申请同一把锁。
- 流级 FIN 不会截断已经开始写出的 SYNACK。控制帧写入最多等待 5 秒；若超时或意外取消
  可能留下不完整帧，关闭整个 Session，禁止其他流继续使用损坏的帧序列。
- 会话任务、回调任务与代理任务具有明确的生命周期，关闭连接后不再因后台任务持有引用而残留。
- 只把精确的 `sp.v2.udp-over-tcp.arpa` 和 `sp.udp-over-tcp.arpa` 当作 UoT 请求。
- UoT v2 connect 模式使用初始 SOCKS 地址；非 connect 模式和 v1 使用逐包 UoT 地址。
  两套地址类型编号分别为 `1/3/4` 和 `0/1/2`，不能混用。
- UDP 支持 IPv4、IPv6、同一非 connect association 内混合地址族和零长度数据报。
  connect 模式只接收目标来源的响应；截断包与正常流结束会被区分。
- 超过 65,535 字节的应用字节流被拆成多个合法 PSH 帧；UoT 包不需要与 PSH 帧边界对齐。
  空 PSH 不再被解释成字节流 EOF；超长控制帧会报错而不是截断长度字段。
- 服务端要求 Settings 先于 SYN，SID 非零并严格递增；合法 SID 跳号仍然允许。
- 证书状态按当前时钟计算，过期后的第一秒即可被识别；证书监控独立于文件监听开关。
  热重载拒绝已过期或尚未生效的新证书，保留当前有效配置。

## 运行配置

原来的监听地址、密码、证书参数仍可使用。新增两个可选参数：

| 参数 | 默认值 | 含义 |
| --- | --- | --- |
| `--max-connections` | `1024` | 同时存在的 TLS 会话总数，包含握手中的连接 |
| `--handshake-timeout` | `15` 秒 | TLS 握手与 AnyTLS 密码认证合计的期限 |

这些限制不对已认证的 TCP 业务施加空闲超时。连接规模更大时，可按服务器资源调高连接数。
超限连接会被关闭，已有会话继续运行。

生产环境应提供与客户端 SNI 匹配、受客户端信任的证书。互通测试使用回环网络自签证书，
仅测试程序关闭证书校验；这不是客户端生产配置建议。

服务端 `-I/-T/-M` 是发给客户端的建议值。检查到的 dae transport 会读取并丢弃
ServerSettings 的内容，因此这些参数不会改变该 dae 版本的连接池策略。
AnyTLS FIN 表示整个流关闭，不提供 TCP 半关闭语义。收到流级 FIN 后，服务端最多用 1 秒
排空已接收的 TCP 尾部数据；目标持续不读取时终止剩余转发，避免流永久残留。整个 TLS 会话
关闭会立即取消代理任务。

## 重现验证

需要 Rust stable 和 Go 1.24 或更新版本。各测试只访问本机回环目标；首次构建会下载固定依赖。

```sh
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
cargo build --locked --bin anytls-server
SERVER_BINARY="$(pwd)/target/debug/anytls-server"
(cd tests/interop/dae && go run -mod=readonly -buildvcs=false . -server-binary "$SERVER_BINARY")
(cd tests/interop/reference && go run -mod=readonly -buildvcs=false . -server-binary "$SERVER_BINARY")
```

两个 Go fixture 各自启动并清理 Rust 服务端与 TCP/UDP 目标。任一断言失败都会返回非零状态。
IPv6 测试需要本机启用 IPv6 回环。测试细节与固定版本见各 fixture 的 README。

Release 工作流在所有验证和七个平台的构建成功后发布 tag、压缩包与 SHA256 文件。
手动运行 Release 时填写完整 tag（例如 `custom-v0.5.4.2`）；版本不匹配或已有 tag 指向另一提交时会拒绝发布。
