# custom-v0.5.4.2 — Rust server interoperability fixes

This custom release targets a Rust server used with dae and other AnyTLS clients.
Its binary/Cargo version is `0.5.4-custom.2`; the tag follows this fork's existing
`custom-v0.5.4.N` convention. Base: `9de6b200a6818b5f19e4a3e8af141ca4652396dd`.

## Fixes

- Cache DNS IPs independently of destination ports, bound the cache, try multiple TCP addresses, and fall back on immediate local UDP address errors without retrying successfully sent datagrams.
- Complete stream FIN/EOF/error cleanup and wake pending readers, writers, and SYNACK waiters; drain TCP bytes queued before peer FIN for up to one second instead of discarding the tail.
- Cancel session transport work reliably, avoid recursive writer locking on errors, and make interrupted close retryable.
- Preserve in-progress SYNACK writes across stream FIN; bound control writes to five seconds and close a session if an aborted write may have left a partial frame.
- Supervise connection and stream tasks; bound TLS/authentication time and live connection count.
- Support UoT v2 connected/unconnected modes and legacy v1, with correct address encodings, dual-stack sockets,
  empty datagrams, truncated-packet detection, and connected peer filtering.
- Fragment large stream writes into legal AnyTLS PSH frames; ignore empty PSH and reject oversized control frames.
- Enforce Settings before SYN and nonzero increasing stream IDs; match UoT magic domains exactly.
- Recompute certificate expiry at the current time, independently of file watching, and reject expired or not-yet-valid certificate renewals without replacing the working configuration.

## Compatibility verification

The release workflow runs Rust regression tests and two pinned real-client fixtures:

- dae v2.1.2/main's actual outbound transport, revision `cc86ced2e683`.
- anytls-go reference session client, revision `fd6167acd6d7`, with `sing v0.5.1` UoT codecs.

Checks cover TCP EOF/FIN, session reuse and heartbeat, DNS port isolation, fragmented headers,
UoT v1/v2, IPv4/IPv6, empty packets, and a maximum-size IPv6 UDP packet spanning AnyTLS frames.
They run on loopback and do not test dae's eBPF datapath or an actual iOS device.
**Shadowrocket has not been tested on a physical iOS device.** See
`docs/SERVER_COMPATIBILITY.md` in the archive or
[the repository documentation](https://github.com/zhu/anytls-rs/blob/custom-v0.5.4.2/docs/SERVER_COMPATIBILITY.md).

## Upgrade

Replace the server binary with the matching archive's `anytls-server` and restart your service.
Existing listen/password/certificate arguments remain valid. New optional flags:

- `--max-connections 1024`: maximum simultaneous TLS sessions, including handshakes.
- `--handshake-timeout 15`: combined TLS and AnyTLS authentication deadline, in seconds.

These do not impose a traffic idle timeout on authenticated streams. Increase the connection limit
for larger deployments. Server `-I/-T/-M` hints do not override the inspected dae client's own pool policy.
The bundled Rust client is not the focus of this release's interoperability validation.
