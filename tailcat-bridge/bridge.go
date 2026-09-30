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

	"github.com/tailscale/tailcat"
)

type clientRef struct {
	client *tailcat.Client
	address string
	refs    int
}

type forward struct {
	listener net.Listener
	cancel   context.CancelFunc
}

var state = struct {
	sync.Mutex
	nextHandle  int64
	nextForward int64
	clients     map[int64]*clientRef
	byAddress   map[string]int64
	forwards    map[int64]*forward
}{nextHandle: 1, nextForward: 1, clients: map[int64]*clientRef{}, byAddress: map[string]int64{}, forwards: map[int64]*forward{}}

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

// OpenForward binds a loopback-only ephemeral listener. Each accepted local
// connection gets its own Tailcat TCP stream, which permits russh terminal,
// SFTP and forwarding connections to share one Tailcat client safely.
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
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil { return 0, err }
	ctx, cancel := context.WithCancel(context.Background())
	state.Lock()
	fid := state.nextForward
	state.nextForward++
	state.forwards[fid] = &forward{listener: ln, cancel: cancel}
	state.Unlock()
	go acceptLoop(ctx, ln, ref.client, uint16(remotePort))
	return ln.Addr().(*net.TCPAddr).Port, nil
}

func acceptLoop(ctx context.Context, ln net.Listener, client *tailcat.Client, port uint16) {
	for {
		local, err := ln.Accept()
		if err != nil { return }
		go func() {
			defer local.Close()
			remote, err := client.DialTCPPort(ctx, port)
			if err != nil { return }
			defer remote.Close()
			// Close either half when the opposite end exits. This keeps failed
			// Tailcat dials/cancellations from leaving a local SSH socket stuck.
			done := make(chan struct{}, 2)
			go func() { _, _ = io.Copy(remote, local); done <- struct{}{} }()
			go func() { _, _ = io.Copy(local, remote); done <- struct{}{} }()
			<-done
		}()
	}
}

// StopForward immediately unblocks Accept and cancels in-flight dials.
func StopForward(id int64) error {
	state.Lock()
	f := state.forwards[id]
	delete(state.forwards, id)
	state.Unlock()
	if f == nil { return nil }
	f.cancel()
	return f.listener.Close()
}

// Stop drops one logical user of a shared client. The underlying Tailcat
// engine is closed only after the last user has disconnected.
func Stop(handle int64) error {
	state.Lock()
	ref := state.clients[handle]
	if ref == nil { state.Unlock(); return nil }
	ref.refs--
	if ref.refs > 0 { state.Unlock(); return nil }
	delete(state.clients, handle)
	delete(state.byAddress, ref.address)
	state.Unlock()
	return ref.client.Close()
}
