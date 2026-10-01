package tailcatbridge

import (
	"io"
	"net"
	"testing"
	"time"
)

func TestRejectsNonTailcatAddress(t *testing.T) {
	if _, err := Start("https://not-a-tailcat.example"); err == nil {
		t.Fatal("accepted non-Tailcat address")
	}
}

func TestRejectsInvalidPort(t *testing.T) {
	if _, err := OpenForward(99, 0); err == nil {
		t.Fatal("accepted port zero")
	}
}

func TestSingleForwardClosesListenerAfterAttach(t *testing.T) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	remote, peer := net.Pipe()
	done := make(chan struct{})
	go func() { proxySingleConnection(listener, remote); close(done) }()

	local, err := net.Dial("tcp", listener.Addr().String())
	if err != nil {
		t.Fatal(err)
	}
	if _, err := local.Write([]byte("test")); err != nil {
		t.Fatal(err)
	}
	got := make([]byte, 4)
	if _, err := io.ReadFull(peer, got); err != nil {
		t.Fatal(err)
	}
	if string(got) != "test" {
		t.Fatalf("proxied bytes = %q", got)
	}
	_ = local.Close()
	_ = peer.Close()
	select {
	case <-done:
	case <-time.After(time.Second):
		t.Fatal("single-use forward did not close")
	}
	if conn, err := net.DialTimeout("tcp", listener.Addr().String(), 100*time.Millisecond); err == nil {
		_ = conn.Close()
		t.Fatal("listener accepted a second connection")
	}
}
