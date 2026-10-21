# Duplex-KVM Architecture & Protocol Reference

This document is the technical reference for maintaining and extending Duplex-KVM. For user quick-start instructions, see [`README.md`](../README.md). For AI/developer build and verification commands, see [`AGENTS.md`](../AGENTS.md).

---

## 1. System Topology & Network Addressing

Duplex-KVM connects two computers over a dedicated 1 GbE wired link (never `0.0.0.0`):

| Peer Role | Enum Variant | Static IPv4 | Typical Hardware |
| :--- | :--- | :--- | :--- |
| **Windows 11 Desktop** | `NodeRole::WindowsDesktop` | `192.168.137.1/24` | Primary PC with 1–2 physical monitors |
| **Debian Linux Laptop** | `NodeRole::DebianLaptop` | `192.168.137.2/24` | Laptop with internal panel + trackpad/keyboard |

### Ports & DSCP QoS Tiers

All sockets are strictly bound to the local link IP (`validate_strict_interface_ip` in `crates/duplex-pal/src/net.rs`) and tagged with DSCP/TOS and Linux `SO_PRIORITY`:

| Port | Protocol | Traffic | DSCP | `IP_TOS` | Linux `SO_PRIORITY` | Socket Buffers |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| `48100` | UDP | Encrypted HID events (`ChaCha20-Poly1305`) | `EF` (`0x2E`) | `0xB8` | `6` | 256 KiB |
| `48101` | TCP + UDP | Encrypted 50 Hz heartbeats & mode control | `CS6` (`0x30`) | `0xC0` | `7` | 256 KiB |
| `48102` | UDP | Video stream 0 (primary virtual display) | `AF41` (`0x22`) | `0x88` | `4` | 4 MiB |
| `48103` | UDP | Video stream 1 (secondary virtual display) | `AF41` (`0x22`) | `0x88` | `4` | 4 MiB |
| `127.0.0.1:48180` | HTTP | Local-only Web Control Center | — | — | — | OS default |

---

## 2. Workspace Crate & Module Map

```text
crates/
├── duplex-proto/          # Pure Rust wire protocols, crypto, state machine, codecs, SHM ABI
│   ├── crypto.rs          # ChaCha20-Poly1305 AEAD framing, 64-bit anti-replay window, WirePayload
│   ├── hid.rs             # HidEvent, DepressedKeyBitmap, panic chord/escape sequence detector
│   ├── keymap.rs          # Linux evdev <-> USB HID Usage (0x07) <-> Windows VK/ScanCode
│   ├── state.rs           # OperationalMode, ModeStateMachine, epoch ordering, VESA EDID 1.4
│   ├── shm.rs             # ShmWatchdogBlock (#[repr(C, align(64))]) shared memory layout
│   ├── tile_codec.rs      # DPXT 64×64 motion-adaptive tile encoder/decoder (solid/LZ4/JPEG)
│   ├── screen_codec.rs    # Legacy DPXC zlib horizontal band codec (peer compatibility)
│   └── video.rs           # UDP fragment header (1380B payload), VideoFrameAssembler, legacy DPXB
│
├── duplex-pal/            # Platform Abstraction Layer (Linux & Windows)
│   ├── lib.rs             # Traits: InputBackend, VirtualDisplayBackend, VideoPipelineBackend
│   ├── net.rs             # Strict interface IP validation, DSCP/TOS/SO_PRIORITY & buffer sizing
│   ├── ring.rs            # Wait-free lockless SpscRingBuffer<T, N> for input hook callbacks
│   ├── shm.rs             # Cross-OS shared memory (/dev/shm vs CreateFileMappingW) + owner lock
│   ├── stream.rs          # Shared FrameSlot, EncoderControl, StreamDecoder, ViewerSet, codec pool
│   ├── linux/
│   │   ├── input.rs       # Raw evdev EVIOCGRAB capture, uinput injection, trackpad gestures
│   │   ├── display.rs     # Sysfs EDID enumeration, X11 xrandr virtual output provisioning
│   │   ├── video.rs       # FrameRing + SharedCanvas memfd IPC, GStreamer & GTK helper management
│   │   ├── capture.py     # Mutter RecordVirtual / portal / X11 GStreamer capture into FrameRing
│   │   ├── viewer.py      # Fullscreen GTK 4 pixel-exact viewer (with GTK 3 fallback)
│   │   └── layout.py      # GNOME Mutter DisplayConfig helper to align virtual monitors safely
│   └── windows/
│       ├── input.rs       # WH_KEYBOARD_LL / WH_MOUSE_LL hooks + Raw Input capture, SendInput
│       ├── display.rs     # EnumDisplayDevicesW / CCD virtual display activation & window rescue
│       ├── capture.rs     # DXGI Desktop Duplication with automatic GDI fallback + cursor overlay
│       └── video.rs       # Sender orchestration + borderless topmost Win32 viewer windows
│
├── duplexd/               # Symmetric background KVM & video daemon
│   ├── lib.rs             # 1000 Hz main loop, mode transitions, capability negotiation, health
│   ├── net.rs             # DuplexTransport, transition keepalive worker, paced video TX/RX threads
│   └── pairing.rs         # Private 32-byte link.key load/save/fingerprint & DUPLEX_PSK_HEX
│
├── duplex-guard/          # Independent 250 ms Dead-Man's Switch watchdog process
│   └── lib.rs             # Polls ShmWatchdogBlock every 10 ms; ungrabs input & kills viewers on stall
│
├── duplex-gui/            # Unified single-file executable (`duplex-kvm`)
│   ├── main.rs            # Subcommand router (GUI, --daemon, --guard, --setup, CLI, update)
│   ├── linux_gtk.rs       # Native GTK 3 desktop control window (dlopen/FFI, no Cargo GTK build dep)
│   ├── win32_gui.rs       # Native Win32 GDI desktop control window
│   ├── web.rs             # Localhost HTTP server (127.0.0.1:48180) with CSRF/origin protection
│   ├── diagnostics.rs     # Link/carrier/daemon-binary mismatch diagnosis & log tailing
│   ├── updater.rs         # Private GitHub release checker/installer with SHA256SUMS verification
│   ├── windows_setup.rs   # Elevated ShellExecuteW network/firewall setup without temp scripts
│   └── design.rs          # Colors, typography, and embedded font assets
│
└── duplex-ctl/            # Standalone lightweight CLI binary (also embedded in duplex-kvm)
```

---

## 3. Operational Modes & State Machine

`OperationalMode` (`crates/duplex-proto/src/state.rs`) defines four states synchronized across both peers with a monotonically increasing `epoch: u64`:

| Mode | Enum Variant | Input Direction | Video Streams | Virtual Displays |
| :--- | :--- | :--- | :--- | :--- |
| **Mode 0 — Idle** | `Mode0Idle` | Local only | None | 0 |
| **Mode 1 — PC Primary** | `Mode1PcPrimary { extend_display: false }` | Laptop → Windows PC | None | 0 |
| **Mode 1+ — Extended Display** | `Mode1PcPrimary { extend_display: true }` | Laptop → Windows PC | 1 (Windows → Laptop) | 1 on Windows |
| **Mode 2 — Laptop Primary** | `Mode2LaptopPrimary` | Windows PC → Laptop | 2 (Laptop → Windows) | 2 on Linux |

### Transition Invariants
1. **Epoch & Idempotence:** Every local mode change increments `epoch`. Incoming `ModeTransitionRequest` messages with `epoch < local_epoch` are ignored; duplicate requests for the already-active `(mode, epoch)` re-acknowledge without tearing down capture or viewer windows.
2. **Transition Keepalive (`TransitionKeepalive` in `crates/duplexd/src/net.rs`):** Display/capture setup can take several hundred milliseconds. Before synchronous setup begins, `duplexd` spawns a temporary keepalive worker that shares the control `CryptoSession` and ticks `ShmWatchdogBlock` + peer heartbeats for up to **3 seconds** (`TRANSITION_WATCHDOG_GRACE_NS`). If setup hangs beyond 3 seconds, the worker stops and `duplex-guard` reclaims the machine.
3. **Teardown Order:** Leaving an active mode always ungrabs input first, detaches the network video consumer, closes viewer/sender pipelines, and destroys virtual displays (rescuing off-screen windows back to the primary monitor).

---

## 4. Threading Model (`duplexd`)

Video encoding, decoding, and socket I/O are completely isolated from the daemon's real-time input and heartbeat loop:

```text
┌─────────────────────────────────────────────────────────────────────────┐
│ duplexd Main Thread (1 ms tick)                                         │
│  • Polls captured local HID events -> encrypts & sends on UDP 48100     │
│  • Polls incoming UDP 48100 HID & TCP/UDP 48101 control                 │
│  • Ticks 50 Hz CS6 heartbeat & updates ShmWatchdogBlock                 │
│  • Checks video_pal.poll_health() & rate-limits PLI refresh requests    │
└──────────────┬──────────────────────────────────────────┬───────────────┘
               │ VideoSink (bounded 64-unit channel)      │ VideoFrameConsumer
               ▼                                          ▼
┌──────────────────────────────────┐       ┌──────────────────────────────┐
│ Capture + Encode (per monitor)   │       │ Video RX + Decode (per mon.) │
│  • duplex-capture-{0,1} thread   │       │  • duplex-video-rx-{0,1}     │
│    writes newest BGRx frame into │       │    blocks on UDP 48102/48103 │
│    FrameSlot (drops stale raw)   │       │    reassembles units         │
│  • duplex-encode-{0,1} thread    │       │  • duplex-decode-{0,1}       │
│    runs DPXT on low-priority     │       │    decodes on codec pool &   │
│    rayon pool (duplex-codec-*)   │       │    presents complete frames  │
└──────────────┬───────────────────┘       └──────────────────────────────┘
               ▼
┌──────────────────────────────────┐
│ duplex-video-tx Thread           │
│  • Token-bucket pacer (700 Mbps) │
│  • Emits 1380B UDP fragments     │
└──────────────────────────────────┘
```

- **Low-Priority Codec Pool (`video_pool()` in `crates/duplex-pal/src/stream.rs`):** Uses `clamp(num_cpus / 2, 2, 8)` worker threads set to `nice +5` (Linux) or `THREAD_PRIORITY_BELOW_NORMAL` (Windows) so 4K encoding/decoding never starves input or user applications.
- **Backpressure Without Deadlock:** `FrameSlot` keeps only the newest captured raw frame. `EncoderControl` uses a non-blocking `VideoSink::try_send` loop that checks `control.stop` every 500 µs when the TX queue is full, guaranteeing instant encoder shutdown during mode transitions.

---

## 5. Wire Protocols & Codecs

### 5.1 Control & HID AEAD Framing (`crates/duplex-proto/src/crypto.rs`)
- **Cipher:** `ChaCha20-Poly1305` using a 32-byte pre-shared key (`link.key` or `DUPLEX_PSK_HEX`).
- **Nonce (`12 bytes`):** `[channel_id: u32 BE, seq: u64 BE]`. Directional channel IDs prevent cross-channel and reflection attacks (`HidLaptopToDesktop = 1`, `HidDesktopToLaptop = 2`, `ControlLaptopToDesktop = 3`, `ControlDesktopToLaptop = 4`).
- **Sequence initialization:** Seeded from wall-clock nanoseconds at daemon start so a restarted daemon's sequence numbers are immediately accepted by a running peer without resetting its 64-bit `ReplayWindow`.

### 5.2 UDP Video Fragmentation (`crates/duplex-proto/src/video.rs`)
Every encoded slice (`EncodedVideoFrame`) is split by `emit_video_datagrams` into datagrams with a 24-byte `VideoFragmentHeader` (`magic = "DPXV"`, monitor index, codec ID, flags, `fragment_index`, `fragment_count`, `frame_seq`, `capture_timestamp_ns`, `payload_len`) followed by up to **1380 bytes** of payload (`<= 1404` bytes total, safe for 1500-byte Ethernet MTU).

`VideoFrameAssembler`:
- Allocates a single contiguous buffer per unit (`fragment_count * 1380` bytes).
- Rejects non-final fragments smaller than 1380 bytes or out-of-range fragment indices.
- Ignores late duplicate sequences within `STALE_SEQUENCE_WINDOW` (1024), while automatically resynchronizing if a sequence is more than 1024 behind (handling sender restarts cleanly).

### 5.3 DPXT Tile Codec (`crates/duplex-proto/src/tile_codec.rs`)
- **Canvas:** Up to `7680×4320`, partitioned into **64×64 pixel tiles** (`TILE_SIZE = 64`).
- **Slice Header (`22 bytes`):** `magic = "DPXT"`, `version = 1`, `flags` (`FLAG_END_OF_FRAME = 1`, `FLAG_REFRESH = 2`, `FLAG_SYNC = 4`), `width: u16`, `height: u16`, `session: u32`, `frame_no: u32`, `slice_idx: u16`, `record_count: u16`.
- **Tile Records (`8-byte header + payload`):** `tile_index: u16`, `run_len: u8`, `kind: u8`, `payload_len: u32`:
  - `KIND_SOLID (0)`: 3-byte `[B, G, R]` fill across `1..=255` horizontal tiles.
  - `KIND_LZ4 (1)`: Lossless LZ4-compressed BGR bytes for a single tile (used for text, UI, and lossless refinement).
  - `KIND_JPEG (2)`: Baseline JPEG strip across `1..=16` horizontal photographic tiles.
- **Classification & Refinement:**
  - `looks_photographic` samples every 4th row of a tile; if `< 1/8` of adjacent horizontal pixel pairs are identical, the tile is routed directly to JPEG without spending CPU on a trial LZ4 compression.
  - Tiles sent as lossy JPEG are tracked with `last_change_ns`. Once a lossy tile stays unchanged for **250 ms** (`refine_after_ns`), `encode_frame` / `encode_idle` re-sends it losslessly (`KIND_LZ4`) within a `1.5 MB/frame` refinement budget.
  - Background refresh (`refresh_period_ns = 10 s`) continuously sweeps all tiles so any undetected loss self-heals.

### 5.4 Linux Shared-Memory IPC (`crates/duplex-pal/src/linux/video.rs`)
To avoid piping 33 MB raw 4K frames through `stdout`:
1. **`FrameRing` (`magic = "DPXF"`):** A `memfd` passed as `fd 3` to `capture.py` with a 4 KiB header and 3 page-aligned frame slots. `capture.py` maps each `GstBuffer` via `libgstreamer-1.0.so.0` (`gst_buffer_map`), copies it into a slot under an odd/even sequence counter, and writes the 4-byte slot index to `stdout`.
2. **`SharedCanvas` (`magic = "DPXS"`):** A `memfd` passed as `fd 3` to `viewer.py` with a 4 KiB header and an 8K-capable BGRx pixel buffer. The daemon writes decoded dirty rectangles under a seqlock at offset `20` and writes `[rect_count, (x, y, w, h)...]` to the viewer's non-blocking `stdin`.
3. **Pixel-Exact GTK 4 Presentation (`viewer.py`):** Uses `Gdk.MemoryTextureBuilder` with `set_update_texture` / `set_update_region` so only dirty tiles are uploaded to the GPU. When `fit == 1.0`, it appends a plain `snap.append_texture` node aligned to the device-pixel grid (`surface.get_scale()`) rather than `append_scaled_texture`, avoiding GTK 4's fractional-scale offscreen blur.

---

## 6. Watchdog & Emergency Reclamation (`duplex-guard`)

`ShmWatchdogBlock` (`crates/duplex-proto/src/shm.rs`) lives in `/dev/shm/duplex_kvm_watchdog_v1` (Linux) or `Local\DuplexKvmWatchdogV1` (Windows):

- **Single-Instance Lock:** `ShmWatchdog::create_or_open` acquires `flock(LOCK_EX | LOCK_NB)` on `/dev/shm/duplex_kvm_watchdog_v1.lock` (Linux) or a named Win32 mutex `Local\DuplexKvmDaemonOwnerV1` (Windows) before initializing the block, preventing duplicate daemons from overwriting telemetry or stealing supervision.
- **Liveness Evaluation (`evaluate_liveness`):** `duplex-guard` polls every 10 ms. If `active_mode != 0` and either `daemon_heartbeat_ns` or `peer_heartbeat_ns` is older than **250 ms** (`WATCHDOG_TIMEOUT_NS`)—and not within a bounded `transition_grace_until_ns` window—the guard immediately:
  1. Releases all depressed HID keys (`emergency_release_all`).
  2. Kills all viewer/capture helper processes (`/tmp/duplex-kvm-linux-video.pids` on Linux) and destroys virtual displays.
  3. Resets `active_mode` to `0` (`Mode0Idle`).

---

## 7. Environment Variables & Configuration Files

### Configuration Paths
| File | Linux Path | Windows Path | Purpose |
| :--- | :--- | :--- | :--- |
| `link.key` | `~/.config/duplex-kvm/link.key` (`0600`) | `%APPDATA%\Duplex-KVM\link.key` | 64-hex-digit (32-byte) private pairing key |
| `config.json` | `~/.config/duplex-kvm/config.json` (`0600`) | `%APPDATA%\Duplex-KVM\config.json` | Updater repo/token and UI appearance settings |
| `logs/daemon.log` | `~/.config/duplex-kvm/logs/daemon.log` | `%APPDATA%\Duplex-KVM\logs\daemon.log` | Background daemon & watchdog log |

### Environment Overrides
| Variable | Default | Description |
| :--- | :--- | :--- |
| `DUPLEX_PSK_HEX` | *(none)* | Overrides `link.key` with an explicit 64-hex-character pre-shared key |
| `DUPLEX_ROLE` | Auto by OS | Override node role (`desktop` / `windows` or `laptop` / `linux`) |
| `DUPLEX_LOCAL_IP` | Role default | Override strict local bind IPv4 address |
| `DUPLEX_PEER_IP` | Role default | Override peer target IPv4 address |
| `DUPLEX_VIDEO_MAX_FPS` | `60` | Capture frame-rate ceiling (`5..=144`) |
| `DUPLEX_VIDEO_MBPS` | `700` | Outgoing video token-bucket pacing limit in Mbit/s (`20..=10000`) |
| `DUPLEX_VIDEO_LOSSLESS` | `0` | Set to `1` / `true` to disable JPEG and send all tiles losslessly |
| `DUPLEX_VIDEO_JPEG_QUALITY` | `90` | JPEG quality for moving photographic tiles (`30..=100`) |
| `DUPLEX_VIDEO_CHROMA` | `420` | Set to `444` for full-chroma JPEG encoding |
| `DUPLEX_VIEWER_GTK` | `4` | Set to `3` on Linux to force the GTK 3 fallback viewer |
| `DUPLEX_GITHUB_TOKEN` | *(none)* | GitHub PAT for private release updates (falls back to `GITHUB_TOKEN`) |
