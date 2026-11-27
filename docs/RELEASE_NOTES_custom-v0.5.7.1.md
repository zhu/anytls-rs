# custom-v0.5.7.1

This release integrates upstream 0.5.7 and reports version `0.5.7-custom.1`. Building from source requires Rust 1.91 or newer.

The Rust UDP client now delivers a zero-length response and continues forwarding subsequent datagrams on the same proxy. The server retains support for custom padding and server settings up to the 65,535-byte frame limit. Keep those payloads within 4 KiB when using unmodified upstream 0.5.7 peers.

Before tagging, the integrated code passed 146 Rust tests on Linux and 144 on macOS. On Linux, all 18 interoperability cases passed with the pinned dae and anytls-go clients, including TCP session reuse, UoT v1/v2, empty datagrams and large IPv6 datagrams. Shadowrocket on iOS was outside this test run.
