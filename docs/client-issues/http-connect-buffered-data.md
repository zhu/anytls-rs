# [client/http] CONNECT 头后预读到的隧道字节被静默丢弃

<!-- anytls-client-audit: http-connect-buffered-data -->

## 范围与优先级

建议优先级：**P2**。本 issue 只跟踪仓库自带的 **Rust 客户端/客户端库**；不影响本次 Rust 服务端配合 Shadowrocket / dae 的发布范围。

审计源码固定为 `9424f3d83836649590a6ae452b2528e00d8b0822`。已在本地有界复现，未修改生产代码。

## 问题与影响

`read_http_header` 会返回 `header` 和同一次 socket read 得到的 `remaining`。handler 把 remaining 存入 `request.body`，但 CONNECT 分支只回 200；只有普通 HTTP 分支才把 body 写入 AnyTLS。后续中继直接从 socket 继续读，已消费的 remaining 永久丢失。

定位：[src/client/http_proxy.rs:54-89](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/http_proxy.rs#L54-L89)；[src/client/http_proxy.rs:157-175](https://github.com/zhu/anytls-rs/blob/9424f3d83836649590a6ae452b2528e00d8b0822/src/client/http_proxy.rs#L157-L175)。

这影响把 CONNECT 头和首段隧道数据一起发送的 optimistic CONNECT 客户端，可导致首个 TLS ClientHello 或其他握手消息不完整。这里的隧道数据不是 CONNECT 请求正文；问题在于前端已读走这些字节却成功回 200 后静默丢弃。

## 已执行复现

1. 在启动 handler 前，用本地 TCP connection 一次写入 `CONNECT 127.0.0.1:<target> HTTP/1.1\r\nHost: ...\r\n\r\nHELLO`，保证头和早期字节已排队。
2. 经真实 Rust AnyTLS 服务端建立到本地 target 的代理，收到 HTTP 200 后另发 `WORLD`。
3. 目标实际首先读到 **WORLD**，HELLO 丢失。

私有 HTTP handler 原样复制并仅追加 wrapper 以控制启动时序，其余路径调用原仓库库代码。结果没有依赖“发送一次恰好被网络拆成几次 read”的概率。

## 预期修复与验收

- 将 header parser 的剩余缓冲作为隧道输入前缀，按原顺序、仅一次转交，再继续从 socket 读取。
- 明确并一致处理 optimistic CONNECT；如果选择不支持，应明确拒绝，不能成功建立隧道后静默丢字节。
- 测试头与数据合并、头跨多次 read、剩余数据跨 1024 字节读边界、无剩余数据；目标完整收到输入。

## 依赖更新后的复核

在服务端发布候选提交 [`c250994`](https://github.com/zhu/anytls-rs/tree/c250994c468a7e68d3aba8b2d45816e0a9d55f74) 更新依赖后，本项已再次运行并复现。客户端相关生产源码未改变；[补充输出与固定依赖](https://github.com/zhu/anytls-rs/blob/custom/client-issue-drafts/docs/client-issues/repro/VALIDATION_c250994.md)保留了本轮结果。
