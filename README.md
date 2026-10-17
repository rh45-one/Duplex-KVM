# Duplex-KVM (`duplex-kvm`)

**Cross-Platform Bidirectional Wired KVM, Virtual Display Streamer & 1 Gbps Network Passthrough (Debian Linux <-> Windows 11)**

---

## Single-File Quick Start (Zero Installation Needed)

Everything (Graphical Control Center, `duplexd` Daemon, `duplex-guard` 250ms Dead-Man's Switch Watchdog, CLI Controller, and 1-Click Network Setup Scripts) is compiled into **one standalone file per OS**:

| Operating System | Single-File Executable | How to Run |
| :--- | :--- | :--- |
| **Windows 11 Desktop (`192.168.137.1`)** | **[`releases/duplex-kvm-windows-x86_64.exe`](releases/duplex-kvm-windows-x86_64.exe)** *(2.4 MB)* | Download the `.exe` and **double-click it** to open the Graphical Control Center. |
| **Debian Linux Laptop (`192.168.137.2`)** | **[`releases/duplex-kvm-linux-x86_64`](releases/duplex-kvm-linux-x86_64)** *(1.7 MB)* | Run `./duplex-kvm-linux-x86_64` to open the Graphical Control Center. |

### Inside the Graphical Control Center:
1. **1-Click Setup Network:** Click **"1-Click Setup Network"** on first run to automatically configure the `192.168.137.1/24` <-> `192.168.137.2/24` 1GbE subnet, Windows NAT internet passthrough, firewall rules, and Linux `udev` / Wi-Fi-safe (`route-metric 700`) NetworkManager settings.
2. **Start Local Daemon:** Click **"Start Local Daemon"** to launch the built-in background daemon and 250ms Dead-Man's Switch watchdog directly from the same executable.
3. **Switch Operational Modes with 1 Click:**
   - **Mode 0 — Idle (Local Only + 1GbE Internet Passthrough)**
   - **Mode 1 — PC Primary (Laptop Controls Windows PC, KVM Input Only)**
   - **Mode 1+ — PC Primary (Laptop Controls PC + Extend 1 Virtual Display)**
   - **Mode 2 — Laptop Primary (Dock Windows Dual Monitors & Peripherals to Laptop)**
4. **Emergency Panic Release:** Click **"EMERGENCY PANIC / UNGRAB ALL"** or press **`Ctrl + Alt + Shift + Backspace`** (or **`Escape` 5 times in 1.5s**) on your physical keyboard to immediately release all input grabs and virtual displays.

---

## Optional Command-Line Flags (Using the Same Single File)

Both `duplex-kvm-linux-x86_64` and `duplex-kvm-windows-x86_64.exe` also accept CLI arguments if you prefer terminal control:

```bash
./duplex-kvm-linux-x86_64                  # Launch Native Desktop GUI + Web UI (127.0.0.1:48180)
./duplex-kvm-linux-x86_64 --web            # Open Web Control Center in default browser
./duplex-kvm-linux-x86_64 --setup          # Run embedded 1GbE network & udev setup script
./duplex-kvm-linux-x86_64 --daemon         # Run the background KVM/Video daemon directly
./duplex-kvm-linux-x86_64 status           # Print live link RTT, heartbeat & packet telemetry
./duplex-kvm-linux-x86_64 watch            # Live 10 Hz terminal dashboard
./duplex-kvm-linux-x86_64 mode pc          # Switch to Mode 1+ (PC Primary + Extended Display)
./duplex-kvm-linux-x86_64 mode pc --no-display # Switch to Mode 1 (PC Primary, KVM Input Only)
./duplex-kvm-linux-x86_64 mode laptop      # Switch to Mode 2 (Dock PC Dual Monitors to Laptop)
./duplex-kvm-linux-x86_64 mode idle        # Switch to Mode 0 (Idle / Local Only)
./duplex-kvm-linux-x86_64 panic            # Trigger immediate emergency ungrab
```

---

## Architecture & Workspace Crates

| Crate | Description |
| :--- | :--- |
| `crates/duplex-proto` | Zero-allocation wire framing, canonical USB HID Usage ID (`0x07`) keymap translation, `ChaCha20-Poly1305` encryption + 64-bit anti-replay window, VESA EDID 1.4 generator, video chunking/PLI, and shared-memory telemetry block (`ShmWatchdogBlock`). |
| `crates/duplex-pal` | Platform Abstraction Layer (`InputBackend`, `VirtualDisplayBackend`, `VideoPipelineBackend`, and strict DSCP QoS sockets). |
| `crates/duplex-guard` | Independent 250ms Dead-Man's Switch watchdog module monitoring local daemon and remote 50 Hz peer heartbeats via POSIX `/dev/shm` and Win32 `CreateFileMappingW`. |
| `crates/duplexd` | Symmetric peer-to-peer daemon coordinating input capture/injection, virtual display attachment, low-latency HEVC streaming (`60 Mbps` cap per monitor), and decoupled 1000 Hz hardware cursor updates. |
| `crates/duplex-gui` | Unified single-file executable (`duplex-kvm`) combining the native GTK+ 3 (Linux) and Win32 (Windows 11) GUI, embedded localhost Web UI (`127.0.0.1:48180`), daemon, watchdog, CLI, and embedded OS setup scripts. |
| `crates/duplex-ctl` | Standalone lightweight CLI controller (`status`, `watch`, `mode`, `panic`, `gui`). |
