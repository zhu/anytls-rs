// Exercise the real anytls-go session client and its pinned sing UoT codec.
package main

import (
	"anytls/proxy/padding"
	"anytls/proxy/session"
	"bytes"
	"context"
	"crypto/sha256"
	"crypto/tls"
	"encoding/binary"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	M "github.com/sagernet/sing/common/metadata"
	"github.com/sagernet/sing/common/uot"
)

const timeout = 3 * time.Second

type harness struct{ server, password string }
type result struct {
	Name   string `json:"name"`
	Status string `json:"status"`
	Error  string `json:"error,omitempty"`
}

type skipped struct{ reason string }

func (s *skipped) Error() string { return s.reason }

func (h *harness) client() (*session.Client, *atomic.Int32) {
	connections := &atomic.Int32{}
	client := session.NewClient(context.Background(), func(ctx context.Context) (net.Conn, error) {
		raw, err := (&net.Dialer{}).DialContext(ctx, "tcp", h.server)
		if err != nil {
			return nil, err
		}
		conn := tls.Client(raw, &tls.Config{ServerName: "localhost", InsecureSkipVerify: true}) // loopback fixture only
		_ = conn.SetDeadline(time.Now().Add(timeout))
		if err := conn.HandshakeContext(ctx); err != nil {
			_ = conn.Close()
			return nil, err
		}
		key := sha256.Sum256([]byte(h.password))
		padLen := 0
		if sizes := padding.DefaultPaddingFactory.Load().GenerateRecordPayloadSizes(0); len(sizes) > 0 {
			padLen = sizes[0]
		}
		auth := make([]byte, 32+2+padLen)
		copy(auth, key[:])
		binary.BigEndian.PutUint16(auth[32:], uint16(padLen))
		// Match cmd/client/myclient.go: one write includes digest, length and padding.
		if _, err := conn.Write(auth); err != nil {
			_ = conn.Close()
			return nil, err
		}
		_ = conn.SetDeadline(time.Time{})
		connections.Add(1)
		return conn, nil
	}, &padding.DefaultPaddingFactory, 30*time.Second, 30*time.Second, 1, false)
	return client, connections
}

type fragmentedConn struct {
	net.Conn
	size int
}

func (c *fragmentedConn) Write(p []byte) (int, error) {
	written := 0
	for len(p) > 0 {
		size := min(c.size, len(p))
		n, err := c.Conn.Write(p[:size])
		written += n
		p = p[n:]
		if err != nil {
			return written, err
		}
		if n != size {
			return written, io.ErrShortWrite
		}
	}
	return written, nil
}

func open(client *session.Client, destination M.Socksaddr, fragmented bool) (net.Conn, error) {
	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	defer cancel()
	conn, err := client.CreateStream(ctx)
	if err != nil {
		return nil, err
	}
	_ = conn.SetDeadline(time.Now().Add(timeout))
	if fragmented {
		conn = &fragmentedConn{Conn: conn, size: 1}
	}
	if err := M.SocksaddrSerializer.WriteAddrPort(conn, destination); err != nil {
		_ = conn.Close()
		return nil, err
	}
	return conn, nil
}

func data(size int) []byte {
	p := make([]byte, size)
	for i := range p {
		p[i] = byte((i*17 + 9) % 251)
	}
	return p
}

func tcpTarget(size int) (net.Listener, error) {
	l, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		return nil, err
	}
	go func() {
		for {
			c, err := l.Accept()
			if err != nil {
				return
			}
			go func() {
				defer c.Close()
				_ = c.SetDeadline(time.Now().Add(2 * timeout))
				p := make([]byte, size)
				if _, err := io.ReadFull(c, p); err == nil {
					_, _ = c.Write(p)
				}
			}()
		}
	}()
	return l, nil
}

func (h *harness) tcp(fragmented bool, count int) error {
	p := data(257)
	l, err := tcpTarget(len(p))
	if err != nil {
		return err
	}
	defer l.Close()
	client, connections := h.client()
	defer client.Close()
	for i := 0; i < count; i++ {
		c, err := open(client, M.ParseSocksaddr(l.Addr().String()), fragmented)
		if err != nil {
			return err
		}
		if _, err := c.Write(p); err != nil {
			_ = c.Close()
			return err
		}
		got := make([]byte, len(p))
		_, err = io.ReadFull(c, got)
		if err != nil {
			_ = c.Close()
			return fmt.Errorf("stream %d payload: %w", i+1, err)
		}
		if !bytes.Equal(got, p) {
			_ = c.Close()
			return errors.New("TCP data mismatch")
		}
		var b [1]byte
		n, err := c.Read(b[:])
		_ = c.Close()
		// anytls-go reports remote FIN as net.ErrClosed, rather than io.EOF.
		if n != 0 || (!errors.Is(err, io.EOF) && !errors.Is(err, net.ErrClosed)) {
			return fmt.Errorf("stream %d target EOF: n=%d err=%v", i+1, n, err)
		}
	}
	if n := connections.Load(); n != 1 {
		return fmt.Errorf("session reuse opened %d TLS connections, want one", n)
	}
	return nil
}

func udpTarget(ipv6 bool) (*net.UDPConn, error) {
	network, address := "udp4", "127.0.0.1:0"
	if ipv6 {
		network, address = "udp6", "[::1]:0"
	}
	a, err := net.ResolveUDPAddr(network, address)
	if err != nil {
		return nil, err
	}
	c, err := net.ListenUDP(network, a)
	if err != nil {
		return nil, err
	}
	go func() {
		p := make([]byte, 65535)
		for {
			n, from, err := c.ReadFromUDP(p)
			if err != nil {
				return
			}
			_, _ = c.WriteToUDP(p[:n], from)
		}
	}()
	return c, nil
}

func (h *harness) udp(version uint8, connected, fragmented, mixed bool) error {
	sizes := []int{17, 0, 60000, 31}
	if fragmented {
		sizes = []int{17, 0, 31}
	}
	return h.udpSized(version, connected, fragmented, mixed, sizes)
}

func (h *harness) udpSized(version uint8, connected, fragmented, mixed bool, sizes []int) error {
	first, err := udpTarget(false)
	if err != nil {
		return err
	}
	defer first.Close()
	targets := []*net.UDPConn{first}
	if mixed {
		second, err := udpTarget(true)
		if err != nil {
			return err
		}
		defer second.Close()
		targets = append(targets, second)
	}
	client, _ := h.client()
	defer client.Close()
	stream, err := open(client, uot.RequestDestination(version), fragmented)
	if err != nil {
		return err
	}
	defer stream.Close()
	if !fragmented {
		// The reference Stream.Write does not split oversized frame payloads.
		// Split client writes so the max-size UDP test isolates server framing.
		stream = &fragmentedConn{Conn: stream, size: 32768}
	}
	request := uot.Request{IsConnect: connected, Destination: M.SocksaddrFromNet(first.LocalAddr())}
	if version == 2 {
		if err := uot.WriteRequest(stream, request); err != nil {
			return err
		}
	}
	pc := uot.NewConn(stream, request)
	for i, size := range sizes {
		target := targets[i%len(targets)]
		p := data(size)
		if _, err := pc.WriteTo(p, target.LocalAddr()); err != nil {
			return fmt.Errorf("packet %d write: %w", i, err)
		}
		got := make([]byte, 65535)
		n, from, err := pc.ReadFrom(got)
		if err != nil {
			return fmt.Errorf("packet %d size %d read: %w", i, size, err)
		}
		if !bytes.Equal(got[:n], p) {
			return fmt.Errorf("packet %d data mismatch: got %d want %d", i, n, size)
		}
		if from.String() != target.LocalAddr().String() {
			return fmt.Errorf("packet %d source %s want %s", i, from, target.LocalAddr())
		}
	}
	return nil
}

func (h *harness) udpMaxIPv6() error {
	// Some kernels cap UDP below the IPv6 protocol maximum. Probe the host
	// independently, so their EMSGSIZE is not misreported as a server failure.
	target, err := udpTarget(true)
	if err != nil {
		return err
	}
	defer target.Close()
	conn, err := net.DialUDP("udp6", nil, target.LocalAddr().(*net.UDPAddr))
	if err != nil {
		return err
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(timeout))
	p := data(65527)
	if _, err := conn.Write(p); err != nil {
		if errors.Is(err, syscall.EMSGSIZE) {
			return &skipped{reason: "host kernel rejects 65527-byte IPv6 UDP datagram with EMSGSIZE; frame splitting remains covered by Rust session tests"}
		}
		return fmt.Errorf("direct IPv6 maximum-size probe: %w", err)
	}
	got := make([]byte, 65535)
	n, err := conn.Read(got)
	if err != nil {
		return fmt.Errorf("direct IPv6 maximum-size probe read: %w", err)
	}
	if !bytes.Equal(got[:n], p) {
		return errors.New("direct IPv6 maximum-size probe data mismatch")
	}
	return h.udpSized(2, false, false, true, []int{17, 65527})
}

type lockedBuffer struct {
	mu sync.Mutex
	bytes.Buffer
}

func (b *lockedBuffer) Write(p []byte) (int, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.Buffer.Write(p)
}
func (b *lockedBuffer) String() string { b.mu.Lock(); defer b.mu.Unlock(); return b.Buffer.String() }

func spawnServer(binary, password string) (string, func(), *lockedBuffer, error) {
	l, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		return "", nil, nil, err
	}
	addr := l.Addr().String()
	_ = l.Close()
	path, err := filepath.Abs(binary)
	if err != nil {
		return "", nil, nil, err
	}
	cmd := exec.Command(path, "--listen", addr, "--password", password, "--log-level", "warn")
	logs := &lockedBuffer{}
	cmd.Stdout, cmd.Stderr = logs, logs
	if err := cmd.Start(); err != nil {
		return "", nil, nil, err
	}
	done := make(chan error, 1)
	go func() { done <- cmd.Wait() }()
	cleanup := func() { _ = cmd.Process.Kill(); <-done }
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		select {
		case err := <-done:
			return "", nil, logs, fmt.Errorf("server exited: %v\n%s", err, logs.String())
		default:
		}
		c, err := net.DialTimeout("tcp", addr, 100*time.Millisecond)
		if err == nil {
			_ = c.Close()
			return addr, cleanup, logs, nil
		}
		time.Sleep(20 * time.Millisecond)
	}
	cleanup()
	return "", nil, logs, fmt.Errorf("server startup timed out\n%s", logs.String())
}

func main() { os.Exit(run()) }

func run() int {
	server := flag.String("server", "", "existing loopback Rust AnyTLS server address")
	binary := flag.String("server-binary", "", "start a Rust AnyTLS server executable for this run")
	password := flag.String("password", "reference-local-interop-only", "test server password")
	flag.Parse()
	var logs *lockedBuffer
	if *binary != "" {
		addr, cleanup, childLogs, err := spawnServer(*binary, *password)
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			return 2
		}
		defer cleanup()
		*server, logs = addr, childLogs
	}
	if *server == "" {
		fmt.Fprintln(os.Stderr, "provide -server-binary or -server")
		return 2
	}
	host, _, err := net.SplitHostPort(*server)
	if err != nil || net.ParseIP(host) == nil || !net.ParseIP(host).IsLoopback() {
		fmt.Fprintln(os.Stderr, "server must be a loopback IP address")
		return 2
	}
	h := &harness{server: *server, password: *password}
	tests := []struct {
		name string
		run  func() error
	}{
		{"tcp_fragmented_socks_address_payload_and_eof", func() error { return h.tcp(true, 1) }},
		{"tcp_reference_session_reuse", func() error { return h.tcp(false, 4) }},
		{"udp_v2_connected_zero_and_large", func() error { return h.udp(2, true, false, false) }},
		{"udp_v2_unconnected_mixed_ipv4_ipv6", func() error { return h.udp(2, false, false, true) }},
		{"udp_v2_unconnected_fragmented_headers", func() error { return h.udp(2, false, true, true) }},
		{"udp_v2_unconnected_max_ipv6_frame_boundary", h.udpMaxIPv6},
		{"udp_v1_legacy_mixed_ipv4_ipv6", func() error { return h.udp(1, false, false, true) }},
	}
	failures := 0
	for _, test := range tests {
		r := result{Name: test.name, Status: "pass"}
		if err := test.run(); err != nil {
			r.Error = err.Error()
			var skip *skipped
			if errors.As(err, &skip) {
				r.Status = "skip"
			} else {
				r.Status = "fail"
				failures++
			}
		}
		_ = json.NewEncoder(os.Stdout).Encode(r)
	}
	if failures != 0 && logs != nil {
		fmt.Fprintln(os.Stderr, logs.String())
	}
	if failures != 0 {
		return 1
	}
	return 0
}
