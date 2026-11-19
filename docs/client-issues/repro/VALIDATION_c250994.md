# 发布候选依赖更新后的客户端复核

复核时间：2026-11-19。被测源码：[`c250994`](https://github.com/zhu/anytls-rs/tree/c250994c468a7e68d3aba8b2d45816e0a9d55f74)。

服务端依赖安全更新后，重新构建并运行此目录提供的两个探针二进制，两个进程都退出0。
**退出0表示已复现仍然存在的客户端问题，并不表示这些问题已修复。**
11份非UDP运行证据均仍成立；四份UDP草稿没有在这一轮重新运行，版本字段仍为静态证据。
客户端、认证、TLS helper、padding和共享Session源文件与原审计提交9424f3d一致。

本轮通过prepare.py读入上述checkout；使用该提交的依赖版本生成
[Cargo.lock.current](Cargo.lock.current)。认证探针仍使用固定的Go参考服务端和合成密码audit-only。
本轮stock客户端的系统错误为Broken pipe，前一轮为Connection reset；两次均在目标连接前失败，
仅把认证合并为一次写入的对照均成功。

## 重现

```sh
python3 prepare.py /absolute/path/to/checkout-at-c250994
cp Cargo.lock.current Cargo.lock
CARGO_TARGET_DIR=/tmp/anytls-client-audit-current cargo run --locked --bin client-audit-repros
ANYTLS_GO_SERVER=/path/to/anytls-go-server CARGO_TARGET_DIR=/tmp/anytls-client-audit-current cargo run --locked --bin go_auth
```

需要复现最初9424f3d的结果时，使用该checkout和本分支最初的Cargo.lock。探针不修改生产源码。

## 本轮实际输出

```text
TLS: self-signed localhost certificate accepted for wrong-name.example
HTTP CONNECT: queued HELLO lost; target's first bytes = WORLD
HTTP local EOF: target gets no EOF in 500ms; AnyTLS stream still active; handler blocked
SOCKS5 local EOF: target gets no EOF in 500ms; AnyTLS stream still active; handler blocked
HTTP remote EOF: shared Session processed FIN and removed stream, but local application gets no EOF in 500ms
Client Drop: all caller strong references dropped for 650ms (>3*200ms idle/heartbeat timeout), session remains live via background tasks; strong_count=4
Pool: two idle checkouts return the identical session while stream 1 is open
Padding counter: Session writes [30, 60, 90, 8]; spec/reference expect [60, 90, 8, 8] after authentication packet0
Padding update: failed to update default factory; default MD5 unchanged = 75cff2ad89aadf5e257059ee571ebe11
Wire UpdatePaddingScheme: ordered HeartResponse still uses old 30-byte scheme (new scheme would use 200/240)
SOCKS5 CMD=0x02: returns success REP=0 and actually establishes TCP CONNECT
SOCKS5 CMD=0x03: returns success REP=0 and actually establishes TCP CONNECT
SOCKS5 CMD=0x99: returns success REP=0 and actually establishes TCP CONNECT
v1 peer: sent PSH v1-ready successfully, but create_proxy_stream returns Protocol error: SYNACK timeout after 30s
TLS stall: TCP accepted and ClientHello received, but create_proxy_stream remains pending beyond 31s (SYNACK timeout never started)
All bounded client-audit probes reproduced the suspected current behavior.
Stock Rust Client -> stock anytls-go: IO error: Broken pipe (os error 32); target never connected
Control with one gathered 64-byte authentication write: SYNACK succeeds and target TCP connects
```
