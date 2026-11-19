# [client] 会话 padding 计数从 0 开始，重复使用认证策略并整体错位

<!-- anytls-client-audit: client-padding-counter -->

## 范围与优先级

建议优先级：**P2**。本 issue 只跟踪仓库自带的 **Rust 客户端/客户端库**；不影响本次 Rust 服务端配合 Shadowrocket / dae 的发布范围。

审计源码固定为 `9424f3d83836649590a6ae452b2528e00d8b0822`。已在本地有界复现，未修改生产代码。

## 问题与影响

认证已使用 packet0 策略，但 `Session::new_client` 把 `pkt_counter` 初始化为 0，写入路径使用 `fetch_add(1)` 的返回旧值直接选择策略。第一笔 Session 写入于是再次使用 key0，后续策略全部晚一包，stop 边界也随之错位。

定位：[src/util/auth.rs:65-77](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/util/auth.rs#L65-L77)；[src/session/session.rs:163-166](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/session/session.rs#L163-L166)；[src/session/session.rs:1107-1128](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/session/session.rs#L1107-L1128)。

协议将 padding0 定义为认证，并要求 Session 的包1包含 Settings + 首个 SYN + 目标地址：[docs/protocol.md:140-153](https://github.com/anytls/anytls-go/blob/fd6167acd6d73b9fa3e607659951847fbc9e6c50/docs/protocol.md#L140-L153)。Go 会话使用 `Add(1)` 的新值，因此从1开始：[proxy/session/session.go:430-435](https://github.com/anytls/anytls-go/blob/fd6167acd6d73b9fa3e607659951847fbc9e6c50/proxy/session/session.go#L430-L435)。

## 已执行复现

使用可记录每次 write 长度的 AsyncWrite，以及固定策略：

```text
stop=3
0=30-30
1=60-60
2=90-90
```

连续四次向客户端 Session 写入 1 字节数据的 PSH（原始帧8字节），实际写入长度是 **[30, 60, 90, 8]**；按认证已占 packet0 的定义应为 **[60, 90, 8, 8]**。

这测量的是向传输层提交的 write 长度，并未将每个 AsyncWrite 调用等同于任意网络条件下的物理 TCP 包。计数差异由固定策略直接验证。

## 预期修复与验收

- 第一笔 Session 写入使用策略1，认证只使用策略0，stop 边界与参考一致。
- 加入固定无随机策略的计数测试，包含初始缓冲 Settings/SYN/目标地址的首次 flush。
- 不把这项修复与 UpdatePaddingScheme 的持有/更新问题混成一个不可单独验证的变化。

## 依赖更新后的复核

在服务端发布候选提交 [`c250994`](https://github.com/zhu/anytls-rs/tree/c250994c468a7e68d3aba8b2d45816e0a9d55f74) 更新依赖后，本项已再次运行并复现。客户端相关生产源码未改变；[补充输出与固定依赖](https://github.com/zhu/anytls-rs/blob/custom/client-issue-drafts/docs/client-issues/repro/VALIDATION_c250994.md)保留了本轮结果。
