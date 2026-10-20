#!/usr/bin/env bash
# ==============================================================================
# Duplex-KVM: Debian Linux Laptop 1GbE Static Link, uinput & Anti-Bufferbloat QoS
# ==============================================================================
# Configures Computer A (Debian Linux Laptop):
# 1. Loads `uinput` kernel module and installs `/etc/udev/rules.d/99-duplex-kvm.rules`
#    so `duplexd` and `duplex-guard` can access `/dev/uinput` and `/dev/input/event*`.
# 2. Creates a manually selected NetworkManager profile (`duplex-kvm-1gbe`).
#    Normal Ethernet DHCP and Wi-Fi profiles are preserved. No default route or DNS
#    is installed unless --passthrough is explicitly requested.
# 3. Disables Energy Efficient Ethernet (EEE) on the RJ45/USB-C NIC and applies
#    an `fq_codel` root qdisc to eliminate bufferbloat and honor DSCP/SO_PRIORITY.

set -euo pipefail

DUPLEX_LOCAL_IP="192.168.137.2"
DUPLEX_GATEWAY_IP="192.168.137.1"
DUPLEX_DNS="1.1.1.1,8.8.8.8"
CONN_NAME="duplex-kvm-1gbe"
CONNECT=false
PASSTHROUGH=false
NETWORK_ONLY=false
IFACE=""

usage() {
    echo "Usage: sudo $0 [interface] [--connect] [--passthrough] [--network-only]"
    echo "Default: prepare a manual KVM profile; leave active networking unchanged."
    echo "--connect: activate the direct PC link (only when plugged into the PC)."
    echo "--passthrough: allow PC internet as a low-priority fallback on that link."
    echo "--network-only: repair/create the profile without changing input permissions or QoS."
}

for arg in "$@"; do
    case "$arg" in
        --connect) CONNECT=true ;;
        --passthrough) PASSTHROUGH=true ;;
        --network-only) NETWORK_ONLY=true ;;
        --help|-h) usage; exit 0 ;;
        -*) usage >&2; exit 1 ;;
        *)
            if [[ -n "$IFACE" ]]; then usage >&2; exit 1; fi
            IFACE="$arg"
            ;;
    esac
done

if [[ $EUID -ne 0 ]]; then
    echo "ERROR: Please run this script as root (sudo ./scripts/debian-setup-link.sh [interface])" >&2
    exit 1
fi

if ! command -v nmcli >/dev/null 2>&1; then
    echo "ERROR: NetworkManager (nmcli) is required; refusing to replace system routes." >&2
    exit 1
fi

if [[ -z "$IFACE" ]]; then
    mapfile -t wired_devices < <(nmcli -t -f DEVICE,TYPE device status | awk -F: '$2 == "ethernet" {print $1}')
    if [[ ${#wired_devices[@]} -eq 1 ]]; then
        IFACE="${wired_devices[0]}"
    fi
fi

if [[ -z "$IFACE" ]]; then
    echo "ERROR: Specify the PC-facing Ethernet interface (none or multiple detected)." >&2
    exit 1
fi

if [[ $(nmcli -g GENERAL.TYPE device show "$IFACE") != ethernet ]]; then
    echo "ERROR: '$IFACE' is not a NetworkManager Ethernet device." >&2
    exit 1
fi

echo "=== Duplex-KVM: Provisioning Debian Laptop on interface '$IFACE' ==="

# 1. Configure kernel uinput & udev permissions
if ! $NETWORK_ONLY; then
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
fi

# 2. A route metric cannot distinguish a PC from a router on the same port.
# Never autoconnect the static profile: otherwise it wins before DHCP is tried.
# Modify in place so upgrading an active legacy profile does not drop the link.
echo "[2/4] Preparing manual KVM profile on '$IFACE' (normal networking preserved)..."
if ! nmcli -g connection.uuid connection show "$CONN_NAME" >/dev/null 2>&1; then
    nmcli connection add type ethernet con-name "$CONN_NAME" ifname "$IFACE" \
        connection.autoconnect no ipv4.method manual ipv4.addresses "${DUPLEX_LOCAL_IP}/24" \
        ipv4.never-default yes ipv6.method disabled
fi

gateway=""
dns=""
never_default=yes
if $PASSTHROUGH; then
    gateway="$DUPLEX_GATEWAY_IP"
    dns="$DUPLEX_DNS"
    never_default=no
fi
nmcli connection modify "$CONN_NAME" \
    connection.interface-name "$IFACE" \
    connection.autoconnect no \
    connection.autoconnect-priority -999 \
    ipv4.method manual \
    ipv4.addresses "${DUPLEX_LOCAL_IP}/24" \
    ipv4.gateway "$gateway" \
    ipv4.dns "$dns" \
    ipv4.routes "" \
    ipv4.never-default "$never_default" \
    ipv4.dns-priority 2000 \
    ipv4.route-metric 2000 \
    ipv6.method disabled \
    ipv6.never-default yes

if $CONNECT; then
    nmcli --wait 20 connection up "$CONN_NAME" ifname "$IFACE"
else
    echo "Active connection unchanged. Reconnect the cable to use normal Ethernet DHCP."
    echo "For a direct PC link: sudo $0 '$IFACE' --network-only --connect"
    echo "For PC internet too: add --passthrough to that command."
fi

# 3. Disable Energy Efficient Ethernet (EEE) to prevent PHY wake latency spikes
if ! $NETWORK_ONLY; then
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

    # Native-resolution video arrives in multi-megabyte bursts; the default
    # 208 KiB socket-buffer ceiling would silently clamp the daemon's 4 MiB
    # video buffers and drop datagrams whenever its receive thread is late.
    echo "Raising socket buffer limits for video (persistent)..."
    if printf '%s\n' '# Duplex-KVM: allow 4 MiB video socket buffers.' \
        'net.core.rmem_max = 8388608' 'net.core.wmem_max = 8388608' \
        > /etc/sysctl.d/90-duplex-kvm.conf; then
        sysctl -q -p /etc/sysctl.d/90-duplex-kvm.conf || true
    else
        echo "Warning: could not write /etc/sysctl.d/90-duplex-kvm.conf; video may drop packets." >&2
    fi
fi

echo "=== Debian Duplex-KVM Setup Complete! (manual profile: $CONN_NAME) ==="
