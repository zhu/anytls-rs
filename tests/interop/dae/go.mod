module anytls-rs-dae-interop

go 1.24

require github.com/daeuniverse/outbound v0.0.0-sticky-ip.0.20260911162531-896954f65c52

require (
	github.com/andybalholm/brotli v1.0.6 // indirect
	github.com/eknkc/basex v1.0.1 // indirect
	github.com/go-task/slim-sprig/v3 v3.0.0 // indirect
	github.com/google/pprof v0.0.0-20250208200701-d0013a598941 // indirect
	github.com/klauspost/compress v1.17.4 // indirect
	github.com/olicesx/quic-go v0.0.0-20260910141758-62d80bbebb5b // indirect
	github.com/onsi/ginkgo/v2 v2.22.2 // indirect
	github.com/refraction-networking/utls v1.8.2 // indirect
	go.uber.org/mock v0.5.0 // indirect
	golang.org/x/crypto v0.36.0 // indirect
	golang.org/x/exp v0.0.0-20250207012021-f9890c6ad9f3 // indirect
	golang.org/x/mod v0.23.0 // indirect
	golang.org/x/net v0.38.0 // indirect
	golang.org/x/sync v0.12.0 // indirect
	golang.org/x/sys v0.31.0 // indirect
	golang.org/x/tools v0.29.0 // indirect
)

// Match dae e35410568009a7e1b49090aaba265c9992fc6b46 exactly.
replace github.com/daeuniverse/outbound => github.com/olicesx/outbound v0.0.0-sticky-ip.0.20260918090140-cc86ced2e683
