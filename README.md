# Duplex-KVM (`duplex-kvm`)

Bidirectional wired KVM, virtual display streamer, and 1 Gbps network passthrough between Debian Linux and Windows 11.

> [!WARNING]
> Project status: Alpha / Beta. This software is in active development and is not yet fully stable. Expect bugs and rough edges while input hooks, display driver integrations, and real-time video streaming continue to be tested across different hardware configurations.

## Single-file quick start

The graphical control center, `duplexd` daemon, `duplex-guard` 250 ms watchdog, CLI controller, and network setup scripts compile into one standalone executable for each operating system:

Mode 1+ requires an upstream Windows virtual display driver. Linux video requires the desktop session packages listed below.

| Operating system | Single-file executable | How to run |
| :--- | :--- | :--- |
| Windows 11 desktop (`192.168.137.1`) | [`releases/duplex-kvm-windows-x86_64.exe`](releases/duplex-kvm-windows-x86_64.exe) | Download and double-click the `.exe` to open the control center. |
| Debian Linux laptop (`192.168.137.2`) | [`releases/duplex-kvm-linux-x86_64`](releases/duplex-kvm-linux-x86_64) | Run `./duplex-kvm-linux-x86_64` to open the control center. |

### Using the control center

1. Setup network: Click "1-Click Setup Network" on first run to configure the `192.168.137.1/24` <-> `192.168.137.2/24` link, Windows NAT and firewall rules, and Linux input permissions. On Linux, this creates a manual `duplex-kvm-1gbe` profile without an internet gateway or DNS override. Select this profile in NetworkManager only when connected to the PC; regular router Ethernet continues using DHCP. Setup leaves active connections unchanged.
2. Start daemon: Click "Start Local Daemon" to launch the background daemon and 250 ms watchdog.
3. Switch modes through the UI or global keyboard shortcuts:
   - `Ctrl + Alt + P` (or `Ctrl + Win/Super + P`): Cycle to the next mode (`Mode 0 -> Mode 1 -> Mode 1+ -> Mode 2 -> Mode 0`). This works globally on Windows 11 and Debian Linux even when input is grabbed.
   - `Ctrl + Alt + 0`: Mode 0 (Idle: local input only, with optional 1GbE internet passthrough).
   - `Ctrl + Alt + 1`: Mode 1 (PC primary: laptop controls the Windows PC with keyboard, mouse, and two-finger trackpad scrolling).
   - `Ctrl + Alt + 2`: Mode 1+ (PC primary with extended display: laptop controls the PC and streams an extended Windows desktop to the laptop).
   - `Ctrl + Alt + 3`: Mode 2 (Laptop primary: laptop uses the Windows PC dual monitors and peripherals).
4. Emergency ungrab: Click "Emergency Panic / Ungrab All" or press `Ctrl + Alt + Shift + Backspace` (or press `Escape` five times within 1.5 seconds) to release all input grabs and virtual displays.

### Pair the computers before starting the daemon

The daemon requires one private pairing key shared by both computers.
From the native app, open the web control center, then "Pair computers":

1. On one computer, click "Generate a key on this computer", then "Copy key".
2. Transfer the key privately to the other computer, paste it into "Pairing key", and click "Save key".
3. Check that the fingerprints match, then start the daemon on both computers.

The key is saved in your configuration directory as `link.key`, so later launches can be started directly. Linux saves the file with permissions 0600; Windows stores it under `%APPDATA%\Duplex-KVM`. Stop the daemon before changing its key. The daemon rejects default or empty keys.

Existing `DUPLEX_PSK_HEX` environment overrides take precedence over saved keys. To use saved pairing instead, clear the override and reopen the application. If you use an older executable without the pairing form, generate a key once with `openssl rand -hex 32` and start both apps with that value:

```bash
export DUPLEX_PSK_HEX='<your 64 hexadecimal digits>'
./duplex-kvm-linux-x86_64
```

```powershell
$env:DUPLEX_PSK_HEX = '<your 64 hexadecimal digits>'
.\duplex-kvm-windows-x86_64.exe
```

Opening the control center does not start the daemon. Click "Start Local Daemon" when ready, or pass `--autostart`. Pairing protects input and control messages. The UDP video stream is unencrypted and intended for the direct wired cable. Internet sharing does not require pairing or the daemon.

### Windows Defender detection

The original bundled Windows EXE was reported as `Trojan:Win32/Wacatac.C!ml` while downloading. Source inspection identified features that commonly trigger heuristic alerts (keyboard hooks, input injection, screen capture, and an elevated launcher), but does not confirm Microsoft's specific trigger or certify the executable as safe. See the [investigation and validation limits](docs/windows-defender-investigation.md).

Windows setup now requests UAC through the Windows Shell API and runs network configuration from the elevated helper without writing temporary scripts or requesting `ExecutionPolicy Bypass`. Release CI scans the final EXE with updated Defender intelligence and refuses to publish if that scan fails or cannot run. Releases include the scan report and SHA256 checksums. Local cross-builds are not scanned automatically.

If Defender still flags an exact build, leave it quarantined and submit that file as a software developer to [Microsoft Security Intelligence](https://www.microsoft.com/en-us/wdsi/filesubmission). Code signing with a trusted certificate is still needed for publisher identity; this project currently has no signing certificate configured.

## Linux networking: normal internet and the PC link

The static KVM profile is configured not to connect automatically, because the same Ethernet port may be plugged into a router that requires DHCP. Route metrics alone cannot distinguish between these two networks. Setup preserves existing wired and Wi-Fi profiles.

To configure or repair the KVM profile without interrupting active connections or modifying input permissions:

```bash
sudo bash scripts/debian-setup-link.sh enp2s0 --network-only
```

Replace `enp2s0` with your Ethernet device name. When plugged into a router, select your normal wired profile in the network menu (for example, `Wired connection 1`). This obtains an address, gateway, and DNS through DHCP. Wi-Fi connections are left alone; the KVM profile does not manage Wi-Fi hardware.

When connected directly to the PC, activate the KVM profile:

```bash
sudo bash scripts/debian-setup-link.sh enp2s0 --network-only --connect
```

To route the laptop's internet traffic through the PC as well, enable passthrough:

```bash
sudo bash scripts/debian-setup-link.sh enp2s0 --network-only --connect --passthrough
```

Windows setup uses static IP configuration and NAT; it does not run a DHCP server. When connecting to the PC, select `duplex-kvm-1gbe` in the laptop network menu. Selecting a DHCP profile will wait indefinitely for an address lease the PC does not offer. For the link, the laptop uses `192.168.137.2/24`, with gateway `192.168.137.1` and DNS assigned when passthrough is active. The web control center includes a "Connect PC link" button that activates the saved profile without altering its gateway or DNS settings. Click it again after reconnecting the cable if NetworkManager defaults back to a DHCP profile.

If the peer is unreachable, the web control center reports missing IP configuration, loss of physical carrier on Linux, or missing authenticated heartbeats. On Windows, run `Get-NetAdapter -Physical` to verify that the USB Ethernet adapter is listed separately from your main network adapter. If it is missing, reconnect the adapter to another USB port and verify its driver in Device Manager. Firewall rules and static IP assignments cannot establish communication if Windows has not recognized the network hardware.

Active modes require a recent authenticated peer before input is grabbed. "Restart daemon" in the interface restarts the background daemon while Idle; switch to Idle before restarting. The previous daemon log is saved to `daemon.previous.log`.

Passthrough assigns route metric and DNS priority `2000`, keeping it lower priority than typical standard connections while respecting custom connection priorities. The profile remains manual with passthrough enabled. Select your normal wired profile when plugging back into a router. Running setup without `--passthrough` removes the PC gateway and DNS entries. Running setup without `--connect` updates settings without changing the active link until reconnected. Without NetworkManager, setup exits without modifying system routes.

## Optional command-line flags

Both `duplex-kvm-linux-x86_64` and `duplex-kvm-windows-x86_64.exe` accept command-line arguments for terminal control:

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

## Architecture and workspace crates

| Crate | Description |
| :--- | :--- |
| `crates/duplex-proto` | Zero-allocation wire framing, canonical USB HID Usage ID (`0x07`) keymap translation, `ChaCha20-Poly1305` encryption + 64-bit anti-replay window, VESA EDID 1.4 generator, `DPXT` tile codec (`tile_codec.rs`) + video chunking/PLI, and shared-memory telemetry block (`ShmWatchdogBlock`). |
| `crates/duplex-pal` | Platform Abstraction Layer (`InputBackend`, `VirtualDisplayBackend`, `VideoPipelineBackend`, and strict DSCP QoS sockets). |
| `crates/duplex-guard` | Independent 250ms Dead-Man's Switch watchdog module monitoring local daemon and remote 50 Hz peer heartbeats via POSIX `/dev/shm` and Win32 `CreateFileMappingW`. |
| `crates/duplexd` | Symmetric peer-to-peer daemon coordinating input capture/injection, virtual display attachment, and screen streaming over dedicated paced video threads. |
| `crates/duplex-gui` | Unified single-file executable (`duplex-kvm`) combining the native GTK+ 3 (Linux) and Win32 (Windows 11) GUI, embedded localhost Web UI (`127.0.0.1:48180`), daemon, watchdog, CLI, and embedded OS setup scripts. |
| `crates/duplex-ctl` | Standalone lightweight CLI controller (`status`, `watch`, `mode`, `panic`, `gui`). |

## Video runtime and project validation

Desktop video on Linux requires Python GI, GTK 4 (GTK 3 works as a fallback), Cairo, GStreamer, and PipeWire:

```bash
sudo apt-get install python3-gi python3-cairo python3-gi-cairo gir1.2-gtk-4.0 gir1.2-gtk-3.0 gir1.2-gstreamer-1.0 gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-pipewire
```

Run the application in the logged-in desktop session. Mode 2 creates two virtual outputs through Mutter or a desktop portal offering virtual sources; portal consent may be required. X11 needs usable virtual connectors.

Each display is sent at its native resolution (up to 7680×4320) and up to 60 frames per second (`DUPLEX_VIDEO_MAX_FPS` changes the cap). The DPXT codec splits the screen into 64-pixel tiles and sends only those that changed: flat areas as a single colour, text and interface graphics losslessly (LZ4), and photos or video as JPEG while in motion. When motion stops, JPEG tiles are refreshed so static text settles into exact pixels. Lost datagrams are detected and repaired with a refresh, and every tile is also re-sent in the background. Encoding runs on a below-normal-priority thread pool while network transmission runs on dedicated paced threads, keeping input and heartbeats responsive.

Capture and viewer pipelines:
- Linux capture uses Mutter `RecordVirtual` (or the desktop portal / X11) through a GStreamer helper that transfers frames in shared memory.
- Windows capture uses DXGI Desktop Duplication with an automatic GDI fallback (for example, while a UAC prompt is displayed), drawing the cursor directly into the frame.
- Linux viewer uses a fullscreen GTK 4 window that maps decoded frames from shared memory and renders at device pixels, including fractional scales such as 125%.
- Windows viewer uses one borderless window per monitor, repainting changed regions 1:1 or with aspect-preserving scaling.

Measured on the development laptop (sender side, Mutter capture in a headless GNOME session): a 4K desktop with typing or scrolling text typically streams at 52 to 56 frames per second (about 1 Mbit/s for typing and 115 to 125 Mbit/s for scrolling); full-screen 4K photo motion runs at about 34 frames per second (~250 Mbit/s); 1080p content reaches close to the 60 fps cap. Video transmission is paced to at most 700 Mbit/s (`DUPLEX_VIDEO_MBPS`), leaving bandwidth on the gigabit link. Optional settings: `DUPLEX_VIDEO_LOSSLESS=1` (disables lossy compression), `DUPLEX_VIDEO_JPEG_QUALITY` (30 to 100, default 90), and `DUPLEX_VIDEO_CHROMA=444` (full-color JPEG).

The codec is negotiated between peers; a peer that predates DPXT receives the older native-resolution bands. On Linux, run the setup script to allow 4 MiB video socket buffers in the kernel. The GNOME sender aligns newly created virtual outputs and selects supported display scales that keep the laptop display accessible.

For Mode 1+, install the [VirtualDrivers Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver/releases) on Windows using its upstream installer or control utility. Check that its configured resolutions include the laptop display mode (for example, 1920×1200 at 60 Hz). Duplex activates this virtual adapter to the right of physical monitors and captures it by adapter identity. The project does not include an IddCx driver and does not fall back to physical monitors when the driver is missing. Status and errors appear in the web control center and daemon log. Windows driver activation and detachment require validation on physical hardware.

Diagnostics for the GUI-launched daemon and watchdog are saved to `logs/daemon.log` in `~/.config/duplex-kvm` (or `$XDG_CONFIG_HOME/duplex-kvm`) on Linux and `%APPDATA%\Duplex-KVM` on Windows. Starting a new daemon preserves the previous log as `daemon.previous.log`.

Private-release updates can be configured under "Software & updates" in the web control center. Supply a GitHub token with repository read permissions, then check or install updates there or through the command line:

```bash
./duplex-kvm-linux-x86_64 config set-repo owner/repository
./duplex-kvm-linux-x86_64 update
./duplex-kvm-linux-x86_64 update install
```

Tokens can also be supplied through `DUPLEX_GITHUB_TOKEN` or `GITHUB_TOKEN`. Automatic checks run after startup and every four hours while the control center is active; automatic installation is opt-in. Release assets must include `SHA256SUMS.txt`. Restart the application after installation.

## Building from source

Prebuilt binaries are included in [`releases/`](releases/), so building from source is optional.

### Native build on Debian or Ubuntu Linux
```bash
sudo apt-get install build-essential pkg-config libgtk-3-dev
cargo build --release --bin duplex-kvm
# Output: target/release/duplex-kvm
```

### Native build on Windows 11
Install [Rust (`rustup`)](https://rustup.rs/) with the standard MSVC toolchain, then run:
```powershell
cargo build --release --bin duplex-kvm
# Output: target\release\duplex-kvm.exe
```

### Cross-compiling the Windows executable from Linux
```bash
sudo apt-get install gcc-mingw-w64-x86-64-posix
rustup target add x86_64-pc-windows-gnu
cargo build --release --bin duplex-kvm --target x86_64-pc-windows-gnu
# Output: target/x86_64-pc-windows-gnu/release/duplex-kvm.exe
```

## Documentation index

| Document | Contents |
| :--- | :--- |
| [`docs/architecture.md`](docs/architecture.md) | Technical reference covering network topology, QoS, modules, threading, wire protocols (`ChaCha20-Poly1305`, `DPXT` tile codec, shared-memory IPC), watchdog safety, and environment variables. |
| [`AGENTS.md`](AGENTS.md) | Developer guide covering build commands, test suites, and architectural invariants. |
| [`docs/project-status.md`](docs/project-status.md) | Validation log, reproducible test commands, and hardware verification checklist. |
| [`docs/gui-design.md`](docs/gui-design.md) | Native GTK 3, Win32, and web control center typography, color hierarchy, and layout specification. |
| [`docs/windows-defender-investigation.md`](docs/windows-defender-investigation.md) | Defender alert investigation, elevated setup hardening, and CI scan configuration. |
