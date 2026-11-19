// This executable exercises the same AnyTLS transport package pinned by dae.
// All targets and the optional server process run on loopback; no eBPF is used.
package main

import (
	"bytes"
	"context"
	"crypto/tls"
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
	"time"

	"github.com/daeuniverse/outbound/netproxy"
	"github.com/daeuniverse/outbound/protocol"
	"github.com/daeuniverse/outbound/protocol/anytls"
)

const timeout = 3 * time.Second

type result struct {
	Name   string `json:"name"`
	Status string `json:"status"`
	Error  string `json:"error,omitempty"`
}

type direct struct{ connections atomic.Int32 }

func (d *direct) DialContext(ctx context.Context, network, address string) (netproxy.Conn, error) {
	n, err := netproxy.ParseMagicNetwork(network)
	if err != nil {
		return nil, err
	}
	c, err := (&net.Dialer{}).DialContext(ctx, n.Network, address)
	if err == nil {
		d.connections.Add(1)
	}
	return c, err
}

type harness struct{ server, password string }

func (h *harness) client() (netproxy.Dialer, *direct, func(), error) {
	next := &direct{}
	d, err := anytls.NewDialer(next, protocol.Header{
		ProxyAddress: h.server, Password: h.password, IsClient: true,
		TlsConfig: &tls.Config{ServerName: "localhost", InsecureSkipVerify: true}, // loopback fixture only
	})
	if err != nil {
		return nil, nil, nil, err
	}
	return d, next, func() { _ = d.(io.Closer).Close() }, nil
}

func dial(d netproxy.Dialer, network, address string) (netproxy.Conn, error) {
	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	defer cancel()
	c, err := d.DialContext(ctx, network, address)
	if err == nil {
		err = c.SetDeadline(time.Now().Add(timeout))
	}
	if err != nil && c != nil {
		_ = c.Close()
	}
	return c, err
}

func payload(n int) []byte {
	p := make([]byte, n)
	for i := range p {
		p[i] = byte((i*31 + 17) % 251)
	}
	return p
}

func startTCP(handler func(net.Conn)) (net.Listener, error) {
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
			go func() { defer c.Close(); _ = c.SetDeadline(time.Now().Add(2 * timeout)); handler(c) }()
		}
	}()
	return l, nil
}

func roundTrip(d netproxy.Dialer, address string, p []byte, eof bool) error {
	c, err := dial(d, "tcp", address)
	if err != nil {
		return err
	}
	defer c.Close()
	if _, err := c.Write(p); err != nil {
		return fmt.Errorf("write: %w", err)
	}
	got := make([]byte, len(p))
	if _, err := io.ReadFull(c, got); err != nil {
		return fmt.Errorf("read echo: %w", err)
	}
	if !bytes.Equal(got, p) {
		return errors.New("TCP echo data mismatch")
	}
	if eof {
		var b [1]byte
		if n, err := c.Read(b[:]); n != 0 || !errors.Is(err, io.EOF) {
			return fmt.Errorf("target EOF: n=%d err=%v", n, err)
		}
	}
	return nil
}

func (h *harness) tcpEchoEOF() error {
	p := payload(256 * 1024)
	l, err := startTCP(func(c net.Conn) {
		got := make([]byte, len(p))
		if _, err := io.ReadFull(c, got); err == nil {
			_, _ = c.Write(got)
		}
	})
	if err != nil {
		return err
	}
	defer l.Close()
	d, _, closeClient, err := h.client()
	if err != nil {
		return err
	}
	defer closeClient()
	return roundTrip(d, l.Addr().String(), p, true)
}

func (h *harness) tcpLocalFIN() error {
	closed := make(chan error, 1)
	p := payload(64 * 1024)
	l, err := startTCP(func(c net.Conn) {
		_, _ = c.Write([]byte("ready"))
		got, err := io.ReadAll(io.LimitReader(c, int64(len(p)+1)))
		if err != nil {
			closed <- fmt.Errorf("target read until FIN: %w", err)
			return
		}
		if !bytes.Equal(got, p) {
			closed <- fmt.Errorf("client FIN lost queued TCP data: got %d want %d bytes", len(got), len(p))
			return
		}
		closed <- nil
	})
	if err != nil {
		return err
	}
	defer l.Close()
	d, _, closeClient, err := h.client()
	if err != nil {
		return err
	}
	defer closeClient()
	c, err := dial(d, "tcp", l.Addr().String())
	if err != nil {
		return err
	}
	defer c.Close()
	var b [5]byte
	if _, err := io.ReadFull(c, b[:]); err != nil {
		return err
	}
	if _, err := c.Write(p); err != nil {
		return err
	}
	if err := c.Close(); err != nil {
		return err
	}
	select {
	case err := <-closed:
		return err
	case <-time.After(timeout):
		return errors.New("target did not receive EOF after client FIN")
	}
}

func (h *harness) tcpReuse(heartbeat bool) error {
	p := payload(17)
	l, err := startTCP(func(c net.Conn) {
		got := make([]byte, len(p))
		if _, err := io.ReadFull(c, got); err == nil {
			_, _ = c.Write(got)
		}
	})
	if err != nil {
		return err
	}
	defer l.Close()
	d, next, closeClient, err := h.client()
	if err != nil {
		return err
	}
	defer closeClient()
	for i := 0; i < 5; i++ {
		if err := roundTrip(d, l.Addr().String(), p, true); err != nil {
			return fmt.Errorf("stream %d: %w", i+1, err)
		}
		if heartbeat && i == 0 {
			time.Sleep(3100 * time.Millisecond)
		} else {
			time.Sleep(20 * time.Millisecond)
		}
	}
	if n := next.connections.Load(); n != 1 {
		return fmt.Errorf("expected one reused TLS session, opened %d", n)
	}
	return nil
}

func (h *harness) tcpRefusal() error {
	l, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		return err
	}
	addr := l.Addr().String()
	_ = l.Close()
	d, next, closeClient, err := h.client()
	if err != nil {
		return err
	}
	defer closeClient()
	c, err := dial(d, "tcp", addr)
	if err != nil {
		return err
	}
	defer c.Close()
	var b [1]byte
	n, err := c.Read(b[:])
	if n != 0 || (!errors.Is(err, net.ErrClosed) && !errors.Is(err, io.EOF)) {
		return fmt.Errorf("expected immediate stream close on connection refusal: n=%d err=%v", n, err)
	}
	_ = c.Close()
	time.Sleep(20 * time.Millisecond)
	// A destination refusal must close that stream while preserving its session.
	p := payload(17)
	listener, err := startTCP(func(c net.Conn) {
		got := make([]byte, len(p))
		if _, err := io.ReadFull(c, got); err == nil {
			_, _ = c.Write(got)
		}
	})
	if err != nil {
		return err
	}
	defer listener.Close()
	if err := roundTrip(d, listener.Addr().String(), p, true); err != nil {
		return fmt.Errorf("stream after refusal: %w", err)
	}
	if n := next.connections.Load(); n != 1 {
		return fmt.Errorf("destination refusal lost TLS session: opened %d connections", n)
	}
	return nil
}

func (h *harness) tcpDNSPorts() error {
	// A dual-stack listener keeps this test independent of localhost A/AAAA order.
	var listeners []net.Listener
	defer func() {
		for _, l := range listeners {
			_ = l.Close()
		}
	}()
	d, _, closeClient, err := h.client()
	if err != nil {
		return err
	}
	defer closeClient()
	for i := 0; i < 2; i++ {
		l, err := net.Listen("tcp", "[::]:0")
		if err != nil {
			return err
		}
		listeners = append(listeners, l)
		banner := byte('A' + i)
		go func(l net.Listener, banner byte) {
			for {
				c, err := l.Accept()
				if err != nil {
					return
				}
				_, _ = c.Write([]byte{banner})
				_ = c.Close()
			}
		}(l, banner)
		_, port, _ := net.SplitHostPort(l.Addr().String())
		c, err := dial(d, "tcp", net.JoinHostPort("localhost", port))
		if err != nil {
			return err
		}
		var got [1]byte
		_, err = io.ReadFull(c, got[:])
		_ = c.Close()
		if err != nil {
			return fmt.Errorf("localhost port %s: %w", port, err)
		}
		if got[0] != banner {
			return fmt.Errorf("hostname cache reused wrong port: got banner %q, want %q for port %s", got[0], banner, port)
		}
	}
	return nil
}

func startUDP(network, address string) (*net.UDPConn, error) {
	a, err := net.ResolveUDPAddr(network, address)
	if err != nil {
		return nil, err
	}
	c, err := net.ListenUDP(network, a)
	if err != nil {
		return nil, err
	}
	go func() {
		buf := make([]byte, 65535)
		for {
			n, from, err := c.ReadFromUDP(buf)
			if err != nil {
				return
			}
			_, _ = c.WriteToUDP(buf[:n], from)
		}
	}()
	return c, nil
}

func (h *harness) udp(ipv6 bool, sizes []int, batch bool) error {
	network, addr := "udp4", "127.0.0.1:0"
	if ipv6 {
		network, addr = "udp6", "[::1]:0"
	}
	u, err := startUDP(network, addr)
	if err != nil {
		return err
	}
	defer u.Close()
	d, _, closeClient, err := h.client()
	if err != nil {
		return err
	}
	defer closeClient()
	c, err := dial(d, "udp", u.LocalAddr().String())
	if err != nil {
		return err
	}
	defer c.Close()
	pc := c.(netproxy.PacketConn)
	packets := make([][]byte, len(sizes))
	for i, size := range sizes {
		packets[i] = payload(size)
	}
	if batch {
		items := make([]netproxy.BatchItem, len(packets))
		for i, p := range packets {
			items[i] = netproxy.BatchItem{Data: p, Addr: u.LocalAddr().String()}
		}
		n, err := pc.(netproxy.PacketBatchWriter).WriteBatch(items)
		if err != nil || n != len(items) {
			return fmt.Errorf("UDP batch write n=%d err=%v", n, err)
		}
	}
	for i, p := range packets {
		if !batch {
			n, err := pc.WriteTo(p, u.LocalAddr().String())
			if err != nil || n != len(p) {
				return fmt.Errorf("UDP write packet %d size %d: n=%d err=%v", i, len(p), n, err)
			}
		}
		got := make([]byte, 65535)
		n, from, err := pc.ReadFrom(got)
		if err != nil {
			return fmt.Errorf("UDP read packet %d size %d: %w", i, len(p), err)
		}
		if !bytes.Equal(got[:n], p) {
			return fmt.Errorf("UDP data mismatch packet %d: got %d want %d bytes", i, n, len(p))
		}
		if from.String() != u.LocalAddr().String() {
			return fmt.Errorf("UDP source %s want %s", from, u.LocalAddr())
		}
	}
	return nil
}

// lockedBuffer permits failure diagnostics while the child process writes logs.
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
	password := flag.String("password", "dae-local-interop-only", "test server password")
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
		{"tcp_large_payload_and_target_eof", h.tcpEchoEOF},
		{"tcp_client_fin_closes_upstream", h.tcpLocalFIN},
		{"tcp_sequential_session_reuse", func() error { return h.tcpReuse(false) }},
		{"tcp_idle_heartbeat_reuse", func() error { return h.tcpReuse(true) }},
		{"tcp_connection_refusal", h.tcpRefusal},
		{"tcp_dns_same_hostname_different_ports", h.tcpDNSPorts},
		{"udp_v2_connect_ipv4", func() error { return h.udp(false, []int{31, 17}, false) }},
		{"udp_v2_connect_zero_datagram", func() error { return h.udp(false, []int{0, 31, 0, 7}, false) }},
		{"udp_v2_connect_large_fragmented_datagram", func() error { return h.udp(false, []int{60000, 31}, false) }},
		{"udp_v2_connect_ipv6", func() error { return h.udp(true, []int{31, 0, 60000}, false) }},
		{"udp_v2_connect_batch", func() error { return h.udp(false, []int{31, 0, 60000, 17}, true) }},
	}
	failures := 0
	for _, test := range tests {
		r := result{Name: test.name, Status: "pass"}
		if err := test.run(); err != nil {
			r.Status = "fail"
			r.Error = err.Error()
			failures++
		}
		_ = json.NewEncoder(os.Stdout).Encode(r)
	}
	if failures != 0 && logs != nil {
		fmt.Fprintln(os.Stderr, logs.String())
	}
	// Exit only after running the child's deferred cleanup.
	if failures != 0 {
		return 1
	}
	return 0
}
