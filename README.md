# Duplex-KVM (`duplex-kvm`)

**Cross-Platform Bidirectional Wired KVM, Virtual Display Streamer & 1 Gbps Network Passthrough (Debian Linux <-> Windows 11)**

---

## Single-File Quick Start

Everything (Graphical Control Center, `duplexd` Daemon, `duplex-guard` 250ms Dead-Man's Switch Watchdog, CLI Controller, and 1-Click Network Setup Scripts) is compiled into **one standalone file per OS**:

Mode 1+ also requires a separately installed Windows virtual-display driver;
Linux video needs the desktop-session packages listed below.

| Operating System | Single-File Executable | How to Run |
| :--- | :--- | :--- |
| **Windows 11 Desktop (`192.168.137.1`)** | **[`releases/duplex-kvm-windows-x86_64.exe`](releases/duplex-kvm-windows-x86_64.exe)** | Download the `.exe` and **double-click it** to open the Graphical Control Center. |
| **Debian Linux Laptop (`192.168.137.2`)** | **[`releases/duplex-kvm-linux-x86_64`](releases/duplex-kvm-linux-x86_64)** | Run `./duplex-kvm-linux-x86_64` to open the Graphical Control Center. |

### Inside the Graphical Control Center:
1. **1-Click Setup Network:** Click **"1-Click Setup Network"** on first run to prepare the `192.168.137.1/24` <-> `192.168.137.2/24` link, Windows NAT/firewall rules, and Linux input permissions. On Linux this creates a **manual** `duplex-kvm-1gbe` profile, with no internet gateway or DNS override. Select this profile in NetworkManager only when plugged into the PC; ordinary router Ethernet continues to use DHCP. Setup leaves active connections unchanged. See the Linux network instructions below for optional PC internet passthrough.
2. **Start Local Daemon:** Click **"Start Local Daemon"** to launch the built-in background daemon and 250ms Dead-Man's Switch watchdog directly from the same executable.
3. **Switch Operational Modes with 1 Click or Global Keyboard Shortcuts:**
   - **`Ctrl + Alt + P`** *(or `Ctrl + Win/Super + P`)*: **Cycle to the Next Mode** (`Mode 0 -> Mode 1 -> Mode 1+ -> Mode 2 -> Mode 0`) — works globally on both Windows 11 and Debian Linux even when input is grabbed!
   - **`Ctrl + Alt + 0`**: **Mode 0 — Idle** *(Local Only + 1GbE Internet Passthrough)*
   - **`Ctrl + Alt + 1`**: **Mode 1 — PC Primary** *(Laptop Controls Windows PC, KVM Input + Two-Finger Trackpad Scroll)*
   - **`Ctrl + Alt + 2`**: **Mode 1+ — PC Primary + Extended Display** *(Laptop Controls PC + Streams Extended Windows Display onto Laptop)*
   - **`Ctrl + Alt + 3`**: **Mode 2 — Laptop Primary** *(Dock Windows Dual Monitors & Peripherals to Laptop)*
4. **Emergency Panic Release:** Click **"EMERGENCY PANIC / UNGRAB ALL"** or press **`Ctrl + Alt + Shift + Backspace`** (or **`Escape` 5 times in 1.5s**) on your physical keyboard to immediately release all input grabs and virtual displays.

### Pair the computers before starting the daemon

The daemon requires one private pairing key shared by your two computers.
From the native app, open the **Web control center**, then **Pair computers**:

1. On one computer, click **Generate a key on this computer**, then **Copy key**.
2. Transfer it privately to the other computer, paste it into **Pairing key**, and click **Save key**.
3. Confirm the fingerprints match, then start the daemon on both computers.

The key is saved in your application configuration as `link.key`, so subsequent
launches work by double-clicking the app. Linux saves the file with permissions
0600; Windows stores it under the user's `%APPDATA%\Duplex-KVM` directory. Stop
the daemon before changing its key. No public/default key is accepted.

Existing `DUPLEX_PSK_HEX` environment overrides remain supported and take
precedence over saved pairing. To use saved pairing, clear the override and
reopen the app. If using an older executable without the pairing form, generate
a key once with `openssl rand -hex 32` and launch both apps with that same value:

```bash
export DUPLEX_PSK_HEX='<your 64 hexadecimal digits>'
./duplex-kvm-linux-x86_64
```

```powershell
$env:DUPLEX_PSK_HEX = '<your 64 hexadecimal digits>'
.\duplex-kvm-windows-x86_64.exe
```

Opening the control center does not start the daemon. Click **Start Local Daemon**
when ready, or explicitly pass `--autostart`. Pairing protects HID/control
messages; the current UDP video transport is unencrypted and intended for the
private wired link. Internet sharing does not require pairing or the daemon.

### Windows Defender detection

The original bundled Windows EXE was reported as `Trojan:Win32/Wacatac.C!ml`
while downloading. Source inspection found plausible detection contributors
(KVM keyboard hooks, input injection, screen capture, and an embedded elevated
PowerShell launcher), but does **not** establish Microsoft's reason or certify
the EXE as safe. See the [investigation and validation limits](docs/windows-defender-investigation.md).

Windows setup now requests UAC through the Windows Shell API and runs the
embedded network commands from the elevated helper. It no longer writes a
temporary Windows script or requests `ExecutionPolicy Bypass`. Release CI scans
the final EXE with updated Defender intelligence and refuses to publish if that
scan fails or cannot run. Releases include the scan report and SHA256 checksums.
Local cross-builds have not passed that Windows scan until a report says so.

If Defender still flags an exact build, leave it quarantined and submit that
file as a software developer to [Microsoft Security Intelligence](https://www.microsoft.com/en-us/wdsi/filesubmission).
Code signing with a trusted publisher certificate is still needed for publisher
identity; this project currently has no signing certificate configured.

---

## Linux networking: normal internet and the PC link

The static KVM profile must not autoconnect: the same Ethernet port may instead be
plugged into a router, which needs DHCP. Route metrics alone cannot distinguish
these two networks. Setup preserves existing wired and Wi-Fi profiles.

To repair the old high-priority KVM profile without interrupting your current
connection or changing input permissions:

```bash
sudo bash scripts/debian-setup-link.sh enp2s0 --network-only
```

Replace `enp2s0` with your Ethernet device. When connected directly to a router,
select your ordinary wired connection in the network menu (on this laptop,
`Wired connection 1`). This obtains the router's address, gateway, and DNS via
DHCP. Wi-Fi uses its saved connection as usual; the KVM profile does not enable,
disable, or repair Wi-Fi hardware.

When plugged directly into the PC, explicitly activate the KVM link:

```bash
sudo bash scripts/debian-setup-link.sh enp2s0 --network-only --connect
```

To also use the PC's internet connection, explicitly opt into passthrough:

```bash
sudo bash scripts/debian-setup-link.sh enp2s0 --network-only --connect --passthrough
```

Windows setup uses static addressing and NAT; it does **not** provide DHCP.
When the cable connects to the PC, select **duplex-kvm-1gbe** in the laptop's
network menu. Selecting an ordinary DHCP profile such as **Wired connection 1**
will wait for an address the PC does not supply. For the PC link, the laptop needs
`192.168.137.2/24`, gateway `192.168.137.1`, and DNS when passthrough is enabled.
The Web control center's **Connect PC link** button activates the saved profile
without changing its gateway/DNS settings. Use it again after reconnecting the
PC cable if NetworkManager selects the ordinary DHCP profile.

If the peer is unavailable, the Web control center reports missing addressing,
physical carrier loss on Linux, or missing authenticated heartbeats. On Windows,
check `Get-NetAdapter -Physical`: the USB Ethernet adapter must appear separately
from the built-in internet adapter. If it disappears, reconnect it to another
USB port and check Device Manager/its driver. Saved IP/firewall settings cannot
establish a physical link when Windows has not recognized the adapter.

Active modes require a recent authenticated peer before input is grabbed.
**Restart daemon** applies an updated executable while Idle; returning to Idle
first is required. The previous daemon log is retained as `daemon.previous.log`.

Passthrough uses route metric and DNS priority `2000`, below typical normal
connections in preference (custom connection priorities still apply). The profile
remains manual even with passthrough enabled. Select the normal wired profile
when returning to a router. Running setup without `--passthrough` clears the
saved PC gateway and DNS; without `--connect`, it leaves the running connection
unchanged until reactivation. Without NetworkManager, setup refuses to change
networking rather than replacing system routes.

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
| `crates/duplexd` | Symmetric peer-to-peer daemon coordinating input capture/injection, virtual display attachment, and screen streaming over dedicated paced video threads. |
| `crates/duplex-gui` | Unified single-file executable (`duplex-kvm`) combining the native GTK+ 3 (Linux) and Win32 (Windows 11) GUI, embedded localhost Web UI (`127.0.0.1:48180`), daemon, watchdog, CLI, and embedded OS setup scripts. |
| `crates/duplex-ctl` | Standalone lightweight CLI controller (`status`, `watch`, `mode`, `panic`, `gui`). |

## Video runtime and project validation

Desktop video on Linux requires Python GI, GTK 4 (GTK 3 works as a fallback),
Cairo, GStreamer, and PipeWire:

```bash
sudo apt-get install python3-gi python3-cairo python3-gi-cairo gir1.2-gtk-4.0 gir1.2-gtk-3.0 gir1.2-gstreamer-1.0 gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-pipewire
```

Run the application in the logged-in desktop session. Mode 2 creates two virtual
outputs through Mutter or a desktop portal offering virtual sources; portal
consent may be required. X11 needs usable virtual connectors.

**How streaming works.** Each display is sent at its native resolution (up to
7680×4320) and up to 60 frames per second (`DUPLEX_VIDEO_MAX_FPS` changes the
cap). The *DPXT* codec splits the screen into 64-pixel tiles and sends only
those that changed: flat areas as a single colour, text and interface graphics
losslessly (LZ4), and photos or video as JPEG while they move. When content
stops changing, JPEG tiles are refined to the exact pixels within a fraction of
a second, so still text is always pixel-perfect. Lost datagrams are detected and
repaired with a refresh, and every tile is also re-sent slowly in the background.
Coding runs on a below-normal-priority pool and the network side runs on its own
paced threads, so input and heartbeats never wait for video.

- **Linux capture:** Mutter `RecordVirtual` (or the desktop portal / X11) through
  a GStreamer helper that hands frames over in shared memory.
- **Windows capture:** DXGI Desktop Duplication, with an automatic GDI fallback
  (for example while a UAC prompt is shown); the real mouse cursor is drawn in.
- **Linux viewer:** a fullscreen GTK 4 window that maps the decoded frame from
  shared memory and draws it at exact device pixels, including fractional
  scaling such as 125%.
- **Windows viewer:** one borderless window per monitor, repainting only changed
  regions; 1:1 when sizes match, otherwise high-quality aspect-preserving scaling.

Measured on the development laptop (sender side, real Mutter capture in a
headless GNOME session): a 4K desktop with typing or scrolling text typically
streams at 52–56 frames per second (about 1 and 115–125 Mbit/s respectively);
full-screen 4K photo motion at about 34 frames per second (~250 Mbit/s); 1080p
content close to the 60 fps cap. Video is paced to at most
700 Mbit/s (`DUPLEX_VIDEO_MBPS`), leaving room on the gigabit link. Optional
settings: `DUPLEX_VIDEO_LOSSLESS=1` (never lossy), `DUPLEX_VIDEO_JPEG_QUALITY`
(30–100, default 90) and `DUPLEX_VIDEO_CHROMA=444` (full-colour JPEG).

**Update both computers.** The codec is negotiated: a peer that predates DPXT
receives the older native-resolution bands instead. On Linux, run the setup
script once more so the kernel allows the 4 MiB video socket buffers. The GNOME
sender temporarily aligns only its newly created virtual outputs and chooses
supported scales that preserve a return path to the laptop.

For Mode 1+, install the [VirtualDrivers Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver/releases)
on Windows, using its upstream installer/control application. Ensure its
supported resolutions include the laptop panel's mode (on this laptop,
**1920×1200 at 60 Hz**). Duplex activates that real virtual adapter to the right
of the physical monitors and captures it by adapter identity. It does not ship
an IddCx driver, and it no longer substitutes the PC's second monitor when the
driver is missing. Setup errors appear in the Web control center and daemon
log. Windows driver activation/detachment still needs native hardware validation.

GUI-launched daemon and watchdog diagnostics are saved to `logs/daemon.log` in
`~/.config/duplex-kvm` (or `$XDG_CONFIG_HOME/duplex-kvm`) on Linux and
`%APPDATA%\Duplex-KVM` on Windows. Starting a new daemon preserves the previous
log as `daemon.previous.log`.

Private-release updates can be configured under **Software & updates** in the
web control center. Supply a GitHub token with read access to repository contents,
then check/install there or use:

```bash
./duplex-kvm-linux-x86_64 config set-repo owner/repository
./duplex-kvm-linux-x86_64 update
./duplex-kvm-linux-x86_64 update install
```

Tokens can also be supplied through `DUPLEX_GITHUB_TOKEN` or `GITHUB_TOKEN`.
Automatic checks run after startup and every four hours while the control center
is running; automatic installation is opt-in. Release assets must include
`SHA256SUMS.txt`. Restart the application after installation.

See [implementation status, test commands, and hardware acceptance checks](docs/project-status.md).
