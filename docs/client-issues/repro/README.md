# Rust client audit reproductions

These standalone probes document unfixed Rust **client** findings. They do not modify the production crate and are not part of the Rust server release changes.

Audited remote source: `9424f3d83836649590a6ae452b2528e00d8b0822`.
Go reference source: `fd6167acd6d73b9fa3e607659951847fbc9e6c50`.
The later server-only changes through remote commit `893cca47a1cda271c733795d328162ad2ff22a29` did not change the audited client, TLS/auth, padding, or shared-session source files. Use the pinned audited checkout above to reproduce the recorded dependency versions.

## Drafts

`../issues.json` contains complete Chinese issue bodies, proposed priorities, evidence levels, source permalinks, and acceptance criteria. The same bodies are separate Markdown files. The repository had Issues disabled (GitHub API 410), so these are drafts, not published issues. The four UDP drafts are documented separately: their temporary runtime probes were not retained and are not covered by this executable harness.

## Running the bounded local probes

The probe crate uses an absolute/path dependency on an existing checkout. Generate the two private-handler wrappers before building:

```bash
python3 prepare.py /absolute/path/to/anytls-rs
CARGO_TARGET_DIR=/tmp/anytls-client-audit-target cargo run --locked --bin client-audit-repros
```

`prepare.py` copies the complete HTTP and SOCKS source files **unchanged** and appends a single public wrapper calling each private connection handler. This lets the probe queue a complete CONNECT header and initial payload before starting the handler, so the early-data test does not depend on network scheduling. All other Client, TLS, Session, Stream, and Server code comes directly from the audited library.

Every waiting operation used as a test criterion is bounded. The v1 negotiation and pre-handshake deadline probes run concurrently and intentionally take about 31 seconds; normal compilation is additional. The assertions confirm the currently observed bugs, so a fixed implementation may intentionally fail these audit probes until they are inverted into regression expectations.

Observed successful probe output:

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
```

The old software version advertisement is static-only evidence and is not represented as a runtime result here. The invalid TLS handshake-signature bypass is also source evidence; only untrusted/wrong-name certificate acceptance was tested live.

## Actual Go server authentication test

Build the reference server with a suitable Go toolchain (Go 1.24 was used):

```bash
cd /absolute/path/to/anytls-go
go build -o /tmp/anytls-go-audit-server ./cmd/server
cd /absolute/path/to/client-audit-repros
ANYTLS_GO_SERVER=/tmp/anytls-go-audit-server CARGO_TARGET_DIR=/tmp/anytls-client-audit-target cargo run --locked --bin go_auth
```

The binary starts the Go server on a loopback ephemeral port, uses only the synthetic password `audit-only`, and terminates its child server on exit. Its log is written to `go-auth-server.log` in the current directory.

Observed output:

```text
Stock Rust Client -> stock anytls-go: IO error: Connection reset by peer (os error 104); target never connected
Control with one gathered 64-byte authentication write: SYNACK succeeds and target TCP connects
```

The Go server log contains a `fallback` entry for the stock Rust attempt. The successful control uses the same Rust TLS/session implementation and target; only authentication is collected into one write. OS error wording can vary; rejection before target connection versus successful coalesced-auth control is the meaningful assertion.
