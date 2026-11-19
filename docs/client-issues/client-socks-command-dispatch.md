# [client/socks5] 忽略 CMD，把 BIND、UDP_ASSOCIATE 和未知命令都当作 TCP CONNECT

<!-- anytls-client-audit: client-socks-command-dispatch -->

## 范围与优先级

建议优先级：**P2**。本 issue 只跟踪仓库自带的 **Rust 客户端/客户端库**；不影响本次 Rust 服务端配合 Shadowrocket / dae 的发布范围。

审计源码固定为 `9424f3d83836649590a6ae452b2528e00d8b0822`。已在本地有界复现，未修改生产代码。

## 问题与影响

请求解析返回 `(destination, cmd)`，handler 却绑定为 `_cmd`，然后无条件创建普通 TCP proxy stream 并返回成功。尚未实现某个 SOCKS 命令本身可以接受，但成功执行完全不同的命令不正确。

定位：[src/client/socks5.rs:93-110](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/socks5.rs#L93-L110)；[src/client/socks5.rs:394-448](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/socks5.rs#L394-L448)；[src/client/socks5.rs:147-151](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/socks5.rs#L147-L151)。RFC1928 的命令和回复定义见 [RFC1928 §4/§6](https://www.rfc-editor.org/rfc/rfc1928.html)，不支持命令有专门的 REP=0x07。

## 已执行复现

完成 `05 01 00` / `05 00` 认证协商后，分别发送：

```text
05 02 00 01 7f 00 00 01 <port>  # BIND
05 03 00 01 7f 00 00 01 <port>  # UDP ASSOCIATE
05 99 00 01 7f 00 00 01 <port>  # 未定义命令
```

三次都收到 `REP=0x00`，且本地 target listener 实际接受到 TCP CONNECT。使用真实 Rust AnyTLS 服务端和 Client，私有 SOCKS handler 原样复制并仅添加测试 wrapper。

## 预期修复与验收

- 按 CMD 分支处理；至少只对已实现的 CONNECT 执行现有逻辑。
- BIND / UDP ASSOCIATE 未实现时返回命令不支持并关闭请求，不建立目标 TCP 连接；未知值同样拒绝。
- 若补充 UDP ASSOCIATE，应实现其独立 UDP socket、控制连接生命周期和返回 BND 地址语义，不能用当前固定目标 UDP helper 冒充该协议。
- 表驱动验证每种 CMD、无效 RSV、成功/失败回复，并断言不支持请求不产生出站。

## 依赖更新后的复核

在服务端发布候选提交 [`c250994`](https://github.com/zhu/anytls-rs/tree/c250994c468a7e68d3aba8b2d45816e0a9d55f74) 更新依赖后，本项已再次运行并复现。客户端相关生产源码未改变；[补充输出与固定依赖](https://github.com/zhu/anytls-rs/blob/custom/client-issue-drafts/docs/client-issues/repro/VALIDATION_c250994.md)保留了本轮结果。
