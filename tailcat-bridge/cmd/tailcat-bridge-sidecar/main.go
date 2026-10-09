// tailcat-bridge-sidecar is Submarine's private desktop Tailcat helper.
// It is bundled with the application and speaks only on loopback; users never
// install or invoke it themselves.
package main

import (
	"bufio"
	"encoding/base64"
	"flag"
	"fmt"
	"io"
	"net"
	"os"
	"strconv"
	"strings"
	"sync"

	tailcatbridge "github.com/SinaXhpm/Submarine/tailcat-bridge"
)

var listen = flag.String("listen", "127.0.0.1:38492", "private loopback control address")
var exitOnStdinClose = flag.Bool("exit-on-stdin-close", false, "exit when the parent application closes stdin")

func main() {
	flag.Parse()
	if *exitOnStdinClose {
		go func() {
			_, _ = io.Copy(io.Discard, os.Stdin)
			os.Exit(0)
		}()
	}
	ln, err := net.Listen("tcp", *listen)
	if err != nil {
		// Never include a request/address in process output.
		fmt.Println("tailcat bridge could not bind its private control socket")
		return
	}
	defer ln.Close()
	var clients sync.Map // map[string]int64
	for {
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		go handle(conn, &clients)
	}
}

func handle(conn net.Conn, clients *sync.Map) {
	defer conn.Close()
	line, err := bufio.NewReader(conn).ReadString('\n')
	if err != nil {
		return
	}
	fields := strings.Fields(line)
	if len(fields) != 3 || fields[0] != "OPEN" {
		fmt.Fprintln(conn, "ERR BAD_REQUEST")
		return
	}
	decoded, err := base64.RawURLEncoding.DecodeString(fields[1])
	if err != nil {
		fmt.Fprintln(conn, "ERR INVALID_ADDRESS")
		return
	}
	address := strings.TrimSpace(string(decoded))
	if !strings.HasPrefix(address, "tc") {
		fmt.Fprintln(conn, "ERR INVALID_ADDRESS")
		return
	}
	port, err := strconv.Atoi(fields[2])
	if err != nil || port < 1 || port > 65535 {
		fmt.Fprintln(conn, "ERR INVALID_PORT")
		return
	}
	value, err := clientFor(clients, address)
	if err != nil {
		fmt.Fprintln(conn, "ERR CLIENT_START_FAILED")
		return
	}
	localPort, err := tailcatbridge.OpenForward(value.(int64), port)
	if err != nil {
		fmt.Fprintln(conn, "ERR FORWARD_OPEN_FAILED")
		return
	}
	fmt.Fprintf(conn, "OK %d\n", localPort)
}

func clientFor(clients *sync.Map, address string) (any, error) {
	if handle, ok := clients.Load(address); ok {
		return handle, nil
	}
	handle, err := tailcatbridge.Start(address)
	if err != nil {
		return nil, err
	}
	actual, loaded := clients.LoadOrStore(address, handle)
	if loaded {
		_ = tailcatbridge.Stop(handle)
	}
	return actual, nil
}
