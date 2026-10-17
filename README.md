# Duplex-KVM (`duplex-kvm`)

**Cross-Platform Bidirectional Wired KVM, Virtual Display Streamer & 1 Gbps Network Passthrough (Debian Linux <-> Windows 11)**

`duplex-kvm` is a symmetric peer-to-peer daemon (`duplexd`), hardware watchdog (`duplex-guard`), CLI controller (`duplex-ctl`), and graphical control center (`duplex-gui`) connecting a **Debian Linux Laptop (`192.168.137.2`)** and a **Windows 11 Desktop PC (`192.168.137.1`)** over a direct 1 Gbps Cat6 Ethernet cable.

---

## Workspace Crates

| Crate | Description |
| :--- | :--- |
| `crates/duplex-proto` | Zero-allocation wire framing, canonical USB HID Usage ID (`0x07`) keymap translation, `ChaCha20-Poly1305` encryption + 64-bit anti-replay window, VESA EDID 1.4 generator, video chunking/PLI, and shared-memory telemetry block (`ShmWatchdogBlock`). |
| `crates/duplex-pal` | Platform Abstraction Layer (`InputBackend`, `VirtualDisplayBackend`, `VideoPipelineBackend`, and strict DSCP QoS sockets). |
| `crates/duplex-guard` | Independent 250ms Dead-Man's Switch watchdog process monitoring local daemon and remote 50 Hz peer heartbeats via POSIX `/dev/shm` and Win32 `CreateFileMappingW`. |
| `crates/duplexd` | Symmetric peer-to-peer daemon coordinating input capture/injection, virtual display attachment, low-latency HEVC streaming (`60 Mbps` cap per monitor), and decoupled 1000 Hz hardware cursor updates. |
| `crates/duplex-ctl` | Command-line switcher (`status`, `watch`, `mode`, `panic`, `gui`). |
| `crates/duplex-gui` | Native GTK+ 3 (Linux Wayland/X11) and Win32 (Windows 11) desktop graphical control center + embedded localhost Web UI (`http://127.0.0.1:48180`). |

---

## Operational Modes

1. **`MODE_0_IDLE` (Local Only + 1GbE Internet Passthrough)**
   - Both machines use their own physical displays and peripherals independently.
   - All virtual displays are detached and all input grabs/hooks are released.
2. **`MODE_1_PC_PRIMARY` (Debian Laptop Controls Windows PC + Optional Extended Display)**
   - **Input (`Linux -> Windows`):** Laptop grabs built-in keyboard/trackpad via `evdev` (`EVIOCGRAB`) and streams encrypted USB HID batches (`UDP 48100`, DSCP `EF 0xB8`) to Windows `SendInput`.
   - **Display (`Windows -> Linux`, optional `--extend`):** Windows spawns 1 virtual display matching the Laptop screen, captures via DXGI Desktop Duplication, hardware-encodes HEVC via NVENC/AMF (`UDP 48102`, DSCP `AF41 0x88`), and streams to a borderless fullscreen `VA-API` viewer on Debian.
3. **`MODE_2_LAPTOP_PRIMARY` (Windows Desktop Dual Monitors & Peripherals Docked to Debian Laptop)**
   - **Input (`Windows -> Linux`):** Windows hooks & suppresses physical USB keyboard/mouse (`SetWindowsHookExW` + `WM_INPUT`) and streams encrypted USB HID packets to Linux `/dev/uinput`.
   - **Display (`Linux -> Windows`):** Debian creates 2 virtual outputs matching the Windows Desktop's 2 physical monitors, captures via PipeWire/`VA-API` (`vaapih265enc`), and streams two low-latency HEVC streams (`UDP 48102-48103`) to two borderless fullscreen `D3D11VA` viewer windows on Windows.

---

## Safety & Anti-Lockout Guardrails

- **Hardware Panic Escape Hatch:** Press **`Ctrl + Alt + Shift + Backspace`** OR press **`Escape` 5 times within 1.5 seconds** on either machine to immediately release all input grabs/hooks, emit synthetic `KEY_UP` events for all modifiers, destroy fullscreen viewer windows, detach virtual displays, and transition to `MODE_0_IDLE`.
- **250ms Dead-Man's Switch (`duplex-guard`):** Automatically reclaims input/display state within 250ms if `duplexd` hangs or the Ethernet cable is unplugged.

---

## 1. One-Time Network & Link Setup

### On Windows 11 Desktop (Run PowerShell as Administrator)
```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\windows-setup-passthrough.ps1
```
*(Configures static IP `192.168.137.1/24` on the USB 1GbE adapter, sets up NAT internet passthrough from your WAN NIC, disables USB selective suspend / EEE, and scopes Firewall rules for ports `48100-48103` strictly to `192.168.137.1`.)*

### On Debian Linux Laptop
```bash
sudo ./scripts/debian-setup-link.sh
```
*(Configures `/dev/uinput` and `/dev/input` udev permissions and creates the `duplex-kvm-1gbe` NetworkManager profile with static IP `192.168.137.2/24` and `route-metric 700` so Wi-Fi remains prioritized for internet while unplugged.)*

---

## 2. Build & Run

### Build Release Binaries
```bash
# Native Linux release build:
cargo build --release

# Or cross-compile Windows 11 .exe binaries from Debian:
cargo build --release --target x86_64-pc-windows-gnu
```

### Start the Daemon (`duplexd`)
- **Windows 11 Desktop:**
  ```powershell
  .\target\release\duplexd.exe --role pc
  ```
- **Debian Linux Laptop:**
  ```bash
  sudo ./target/release/duplexd --role laptop
  ```

### Launch the Graphical Interface (`duplex-gui`)
```bash
# Native Desktop GUI (GTK3 on Linux, Win32 on Windows) + background Web UI on 127.0.0.1:48180
./target/release/duplex-gui

# Or launch the Web UI in your browser:
./target/release/duplex-gui --web
```

### Or Switch Modes via CLI (`duplex-ctl`)
```bash
./target/release/duplex-ctl status
./target/release/duplex-ctl watch
./target/release/duplex-ctl mode pc --no-display   # Mode 1 (KVM input only)
./target/release/duplex-ctl mode pc --extend       # Mode 1+ (KVM + Extended Display)
./target/release/duplex-ctl mode laptop            # Mode 2 (Dock PC Dual Monitors & Input to Laptop)
./target/release/duplex-ctl mode idle              # Mode 0 (Idle / Local Only)
./target/release/duplex-ctl panic                  # Emergency Panic Release
```
