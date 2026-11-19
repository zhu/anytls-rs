# dae transport interoperability fixture

This fixture executes dae's real AnyTLS transport against a Rust server binary.
It imports `github.com/daeuniverse/outbound/protocol/anytls` using the exact
replacement dependency selected by dae commit
`e35410568009a7e1b49090aaba265c9992fc6b46` (2026-11-18), also pinned by the
stable `v2.1.2` tag (`460cec77f4365abfd9ea6fae04b66466dd4861cb`):

- Outbound implementation: `github.com/olicesx/outbound`
- Commit: `cc86ced2e683b91387f2cde7cfcadcb5ac2adbc3`
- Minimum Go version: 1.24

Run from this directory:

```sh
go run -buildvcs=false . -server-binary /absolute/path/to/anytls-server
```

The fixture starts its own Rust server and TCP/UDP targets on loopback, uses a
test-only password, and stops the server when finished. Certificate verification
is disabled only for that loopback fixture's self-signed certificate. To exercise
an existing local test server, supply `-server 127.0.0.1:8443 -password ...` instead.

IPv6 loopback must be enabled for the IPv6 and dual-stack hostname checks.
Each check emits a JSON result. A nonzero exit status means at least one check
failed. Cases cover large TCP payloads and target EOF, client FIN immediately
after a 64 KiB write (including full delivery before target EOF), sequential
session reuse, idle heartbeat before reuse, connection refusal, hostname cache
port separation, and UDP v2 connected mode (IPv4, IPv6, empty and 60 KB datagrams,
fragmentation across AnyTLS PSH frames, and dae's batch writer).

This does not run dae's kernel/eBPF datapath or an iOS Shadowrocket app. It verifies
the exact AnyTLS transport dependency used by the inspected dae revision.
Legacy UoT and v2 unconnected mode require the separate reference-client checks.
