// Package tailcatbridge is intentionally small: it exposes Tailcat TCP
// streams as loopback listeners for Submarine's Rust SSH client. It does not
// create a TUN device, request VPN permission, or route device traffic.
package tailcatbridge

import (
	"context"
	"fmt"
	"io"
	"net"
	"strings"
	"sync"
	"time"

	"github.com/tailscale/tailcat"
)

type clientRef struct {
	client  *tailcat.Client
	address string
	refs    int
}

var state = struct {
	sync.Mutex
	nextHandle int64
	clients    map[int64]*clientRef
	byAddress  map[string]int64
}{nextHandle: 1, clients: map[int64]*clientRef{}, byAddress: map[string]int64{}}

// Start validates address and returns a reusable client handle. Identical
// addresses share one Tailcat WireGuard/magicsock client until every handle is
// stopped. Never log address: it can contain a WireGuard preshared key.
func Start(address string) (int64, error) {
	address = strings.TrimSpace(address)
	if !strings.HasPrefix(address, "tc") {
		return 0, fmt.Errorf("Tailcat address must start with tc")
	}
	if _, err := tailcat.ParseAddr(tailcat.Addr(address)); err != nil {
		return 0, fmt.Errorf("invalid Tailcat address")
	}
	state.Lock()
	defer state.Unlock()
	if existing, ok := state.byAddress[address]; ok {
		state.clients[existing].refs++
		return existing, nil
	}
	h := state.nextHandle
	state.nextHandle++
	state.clients[h] = &clientRef{client: tailcat.NewClient(tailcat.Addr(address)), address: address, refs: 1}
	state.byAddress[address] = h
	return h, nil
}

// OpenForward first establishes the Tailcat TCP stream, then binds a
// loopback-only ephemeral listener. This makes a failed Tailcat dial visible
// to the caller instead of looking like a successful SSH connection that is
// immediately reset. Each forward is deliberately single-use: every SSH,
// SFTP, monitor, or forwarding connection opens its own forward while sharing
// the Tailcat client. The listener closes after its first accepted connection
// (or after a short attachment deadline), so it cannot leak.
func OpenForward(handle int64, remotePort int) (int, error) {
	if remotePort < 1 || remotePort > 65535 {
		return 0, fmt.Errorf("invalid remote port")
	}
	state.Lock()
	ref := state.clients[handle]
	state.Unlock()
	if ref == nil {
		return 0, fmt.Errorf("Tailcat client is not running")
	}
	// DialTCPPort uses its context only while establishing the Tailcat
	// connection. Keep the returned net.Conn independent of a cancellation
	// scope so the SSH stream remains valid after this function returns.
	remote, err := ref.client.DialTCPPort(context.Background(), uint16(remotePort))
	if err != nil {
		return 0, fmt.Errorf("Tailcat TCP dial failed: %w", err)
	}
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		_ = remote.Close()
		return 0, err
	}
	go proxySingleConnection(ln, remote)
	return ln.Addr().(*net.TCPAddr).Port, nil
}

func proxySingleConnection(ln net.Listener, remote net.Conn) {
	defer ln.Close()
	defer remote.Close()
	if tcp, ok := ln.(*net.TCPListener); ok {
		_ = tcp.SetDeadline(time.Now().Add(15 * time.Second))
	}
	local, err := ln.Accept()
	if err != nil {
		return
	}
	defer local.Close()
	// Close either half when the opposite end exits. This keeps failed
	// Tailcat dials/cancellations from leaving a local SSH socket stuck.
	done := make(chan struct{}, 2)
	go func() { _, _ = io.Copy(remote, local); done <- struct{}{} }()
	go func() { _, _ = io.Copy(local, remote); done <- struct{}{} }()
	<-done
}

// Stop drops one logical user of a shared client. The underlying Tailcat
// engine is closed only after the last user has disconnected.
func Stop(handle int64) error {
	state.Lock()
	ref := state.clients[handle]
	if ref == nil {
		state.Unlock()
		return nil
	}
	ref.refs--
	if ref.refs > 0 {
		state.Unlock()
		return nil
	}
	delete(state.clients, handle)
	delete(state.byAddress, ref.address)
	state.Unlock()
	return ref.client.Close()
}
