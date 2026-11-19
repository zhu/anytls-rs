# anytls-go reference interoperability fixture

This fixture imports the real `anytls-go` session client at commit
`fd6167acd6d73b9fa3e607659951847fbc9e6c50`, and its `sing` v0.5.1 UoT codec.
The TLS authentication wrapper follows the reference example's
`cmd/client/myclient.go`. It requires Go 1.24 or newer.

```sh
go run -buildvcs=false . -server-binary /absolute/path/to/anytls-server
```

All traffic stays on loopback. The fixture starts and cleans up its own Rust
server and target sockets; self-signed TLS verification is disabled only for this
local fixture. IPv6 loopback must be enabled for the mixed-family cases.
`-server 127.0.0.1:8443 -password ...` can target an existing local
test server instead. Each case emits JSON; any failure produces a nonzero exit.

Coverage includes byte-by-byte SOCKS address fragmentation across AnyTLS PSH
frames, payload integrity, target close propagation, reference session reuse,
UoT v2 connected and unconnected modes, legacy UoT v1, mixed IPv4/IPv6 targets,
empty UDP packets, 60 KB datagrams, and fragmented UoT request/per-packet headers.
A maximum-size 65,527-byte IPv6 datagram produces a 65,548-byte UoT reply and
verifies that the Rust server splits it across AnyTLS frames. Client writes are
split at 32 KiB because the reference client's own `Stream.Write` does not split
oversized frame payloads; this keeps that case focused on the server under test.
If a direct loopback probe returns `EMSGSIZE` for 65,527 bytes, that maximum-size
case emits an explicit `skip`; the other IPv6 checks still run. The Rust session
tests separately verify frame splitting without depending on a kernel UDP limit.

Legacy mode and connected mode use the real `sing` codec but differ from the
sample anytls-go binary's default: the example's UDP path chooses v2 unconnected
mode. Neither this fixture nor the dae fixture exercises a real Shadowrocket
installation.
