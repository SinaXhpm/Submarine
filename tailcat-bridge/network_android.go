//go:build android

package tailcatbridge

import (
	"encoding/json"
	"fmt"
	"net"
	"strings"
	"sync"

	"tailscale.com/net/netmon"
)

// Android app UIDs cannot read the route netlink socket. Kotlin obtains the
// permitted LinkProperties data from ConnectivityManager and supplies it here.
// This is process-local network metadata only; it creates neither a VPN nor a
// TUN interface.
type androidNetworkPayload struct {
	Interfaces       []androidNetworkInterface `json:"interfaces"`
	DefaultInterface string                    `json:"defaultInterface"`
}

type androidNetworkInterface struct {
	Name      string   `json:"name"`
	Addresses []string `json:"addresses"`
	MTU       int      `json:"mtu"`
}

var androidNetworkSnapshot struct {
	sync.RWMutex
	interfaces []netmon.Interface
	err        error
}

func init() {
	netmon.RegisterInterfaceGetter(func() ([]netmon.Interface, error) {
		androidNetworkSnapshot.RLock()
		defer androidNetworkSnapshot.RUnlock()
		if androidNetworkSnapshot.err != nil {
			return nil, androidNetworkSnapshot.err
		}
		if len(androidNetworkSnapshot.interfaces) == 0 {
			return nil, fmt.Errorf("Android network state is unavailable")
		}
		interfaces := make([]netmon.Interface, len(androidNetworkSnapshot.interfaces))
		copy(interfaces, androidNetworkSnapshot.interfaces)
		return interfaces, nil
	})
}

// UpdateNetworkState is called from Kotlin before starting a Tailcat client.
// It accepts only ConnectivityManager LinkProperties fields needed by netmon.
func UpdateNetworkState(value string) error {
	var state androidNetworkPayload
	if err := json.Unmarshal([]byte(value), &state); err != nil {
		return fmt.Errorf("invalid Android network state: %w", err)
	}
	interfaces := make([]netmon.Interface, 0, len(state.Interfaces))
	for index, input := range state.Interfaces {
		if strings.TrimSpace(input.Name) == "" {
			continue
		}
		addresses := make([]net.Addr, 0, len(input.Addresses))
		for _, raw := range input.Addresses {
			ip, network, err := net.ParseCIDR(strings.TrimSpace(raw))
			if err != nil {
				continue
			}
			addresses = append(addresses, &net.IPNet{IP: ip, Mask: network.Mask})
		}
		if len(addresses) == 0 {
			continue
		}
		interfaces = append(interfaces, netmon.Interface{Interface: &net.Interface{
			Index: index + 1,
			Name:  input.Name,
			MTU:   input.MTU,
			Flags: net.FlagUp | net.FlagRunning,
		}, AltAddrs: addresses})
	}
	if len(interfaces) == 0 {
		return fmt.Errorf("Android network state contains no usable interfaces")
	}
	androidNetworkSnapshot.Lock()
	androidNetworkSnapshot.interfaces = interfaces
	androidNetworkSnapshot.err = nil
	androidNetworkSnapshot.Unlock()
	netmon.UpdateLastKnownDefaultRouteInterface(strings.TrimSpace(state.DefaultInterface))
	return nil
}
