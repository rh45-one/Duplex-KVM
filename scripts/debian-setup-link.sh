#!/usr/bin/env bash
# ==============================================================================
# Duplex-KVM: Debian Linux Laptop 1GbE Static Link, uinput & Anti-Bufferbloat QoS
# ==============================================================================
# Configures Computer A (Debian Linux Laptop):
# 1. Loads `uinput` kernel module and installs `/etc/udev/rules.d/99-duplex-kvm.rules`
#    so `duplexd` and `duplex-guard` can access `/dev/uinput` and `/dev/input/event*`.
# 2. Creates a persistent NetworkManager (`nmcli`) connection profile (`duplex-kvm-1gbe`)
#    with static IPv4 192.168.137.2/24, gateway 192.168.137.1, and DNS forwarding.
# 3. Disables Energy Efficient Ethernet (EEE) on the RJ45/USB-C NIC and applies
#    an `fq_codel` root qdisc to eliminate bufferbloat and honor DSCP/SO_PRIORITY.

set -euo pipefail

DUPLEX_LOCAL_IP="192.168.137.2"
DUPLEX_GATEWAY_IP="192.168.137.1"
DUPLEX_DNS="1.1.1.1,8.8.8.8"
CONN_NAME="duplex-kvm-1gbe"

if [[ $EUID -ne 0 ]]; then
    echo "ERROR: Please run this script as root (sudo ./scripts/debian-setup-link.sh [interface])" >&2
    exit 1
fi

IFACE="${1:-}"
if [[ -z "$IFACE" ]]; then
    # Auto-detect the primary wired Ethernet interface (enp*, enx*, eth*)
    IFACE=$(ip -o link show | awk -F': ' '{print $2}' | grep -E '^(enp|enx|eth|eno)' | head -n1 || true)
fi

if [[ -z "$IFACE" ]]; then
    echo "ERROR: Could not auto-detect wired Ethernet interface. Usage: sudo $0 <eth_iface>" >&2
    exit 1
fi

echo "=== Duplex-KVM: Provisioning Debian Laptop on interface '$IFACE' ==="

# 1. Configure kernel uinput & udev permissions
echo "[1/4] Configuring /dev/uinput and /dev/input/event* udev rules..."
modprobe uinput
echo "uinput" > /etc/modules-load.d/duplex-kvm.conf

cat <<'EOF' > /etc/udev/rules.d/99-duplex-kvm.rules
KERNEL=="uinput", SUBSYSTEM=="misc", MODE="0660", GROUP="input", OPTIONS+="static_node=uinput"
KERNEL=="event*", SUBSYSTEM=="input", MODE="0660", GROUP="input"
EOF

udevadm control --reload-rules
udevadm trigger --subsystem-match=input --subsystem-match=misc

if [[ -n "${SUDO_USER:-}" ]]; then
    usermod -aG input "$SUDO_USER" || true
    echo "      Added user '$SUDO_USER' to 'input' group."
fi

# 2. Configure Static IPv4 192.168.137.2/24 + Gateway 192.168.137.1 via NetworkManager (or iproute2 fallback)
echo "[2/4] Configuring static 1GbE link ($DUPLEX_LOCAL_IP/24 -> Gateway $DUPLEX_GATEWAY_IP) on '$IFACE'..."
if command -v nmcli >/dev/null 2>&1; then
    nmcli connection delete "$CONN_NAME" >/dev/null 2>&1 || true
    nmcli connection add \
        type ethernet \
        con-name "$CONN_NAME" \
        ifname "$IFACE" \
        ipv4.method manual \
        ipv4.addresses "${DUPLEX_LOCAL_IP}/24" \
        ipv4.gateway "$DUPLEX_GATEWAY_IP" \
        ipv4.dns "$DUPLEX_DNS" \
        ipv4.route-metric 50 \
        ipv6.method ignore \
        connection.autoconnect yes \
        connection.autoconnect-priority 100
    nmcli connection up "$CONN_NAME" || true
else
    ip link set dev "$IFACE" up
    ip addr flush dev "$IFACE"
    ip addr add "${DUPLEX_LOCAL_IP}/24" dev "$IFACE"
    ip route replace default via "$DUPLEX_GATEWAY_IP" dev "$IFACE" metric 50
fi

# 3. Disable Energy Efficient Ethernet (EEE) to prevent PHY wake latency spikes
echo "[3/4] Disabling Energy Efficient Ethernet (EEE) on '$IFACE'..."
if command -v ethtool >/dev/null 2>&1; then
    ethtool --set-eee "$IFACE" eee off 2>/dev/null || true
    ethtool -K "$IFACE" gro on gso on tso on 2>/dev/null || true
fi

# 4. Configure Anti-Bufferbloat fq_codel qdisc on the 1GbE interface
echo "[4/4] Installing low-latency fq_codel qdisc on '$IFACE'..."
if command -v tc >/dev/null 2>&1; then
    tc qdisc replace dev "$IFACE" root fq_codel quantum 300 limit 1024 noecn 2>/dev/null || true
fi

echo "=== Debian Duplex-KVM Setup Complete! ($DUPLEX_LOCAL_IP -> $DUPLEX_GATEWAY_IP) ==="
