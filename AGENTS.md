# Developer & AI Agent Guide (`AGENTS.md`)

This file provides essential build, verification, and architectural rules for humans and AI coding agents working on **Duplex-KVM**. For deep protocol and threading details, read [`docs/architecture.md`](docs/architecture.md).

---

## 1. Quick Orientation

Duplex-KVM is a symmetric, single-file cross-platform KVM and native-resolution (up to 4K/8K) virtual display streamer between a **Windows 11 Desktop (`192.168.137.1`)** and a **Debian Linux Laptop (`192.168.137.2`)**.

| Crate | Path | Responsibility |
| :--- | :--- | :--- |
| `duplex-proto` | `crates/duplex-proto/` | Pure Rust wire framing, `ChaCha20-Poly1305` crypto + replay window, USB HID keymaps, mode state machine, EDID generator, `DPXT` tile codec (`tile_codec.rs`), legacy `DPXC`/`DPXB` codecs, `ShmWatchdogBlock` ABI. |
| `duplex-pal` | `crates/duplex-pal/` | OS backends (`linux/`, `windows/`) for input capture/injection, virtual displays, capture/viewer pipelines, DSCP QoS sockets (`net.rs`), shared memory (`shm.rs`), and shared parallel encode/decode stream machinery (`stream.rs`). |
| `duplexd` | `crates/duplexd/` | Background daemon (`lib.rs`), QoS socket multiplexer + paced video TX/RX threads (`net.rs`), and pairing key storage (`pairing.rs`). |
| `duplex-guard` | `crates/duplex-guard/` | Independent 250 ms dead-man's-switch watchdog process that releases stuck keys, kills viewers, and resets to Idle if `duplexd` or peer heartbeats stall. |
| `duplex-gui` | `crates/duplex-gui/` | Single-file binary (`duplex-kvm`) bundling the native GTK 3 / Win32 GUI, localhost Web UI (`127.0.0.1:48180`), daemon, guard, CLI, updater, and OS setup scripts. |
| `duplex-ctl` | `crates/duplex-ctl/` | Standalone lightweight CLI (`status`, `watch`, `mode`, `panic`, `gui`). |

---

## 2. Building

### Native Linux Build (Debian / Ubuntu)
```bash
# One-time build dependencies:
sudo apt-get install build-essential pkg-config libgtk-3-dev

# Build the unified single-file executable:
cargo build --release --bin duplex-kvm
```

### Native Windows 11 Build
With standard Rust (`rustup` default `x86_64-pc-windows-msvc` toolchain):
```powershell
cargo build --release --bin duplex-kvm
```

### Cross-Compiling Windows from Linux
On a standard Debian/Ubuntu machine with `gcc-mingw-w64-x86-64-posix` installed:
```bash
sudo apt-get install gcc-mingw-w64-x86-64-posix
rustup target add x86_64-pc-windows-gnu
cargo build --release --bin duplex-kvm --target x86_64-pc-windows-gnu
```
*(Note: If `gcc-mingw-w64-x86-64-posix` is not installed system-wide and a local sysroot exists at `target/mingw-sysroot`, pass `CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=$PWD/target/mingw-sysroot/usr/bin/x86_64-w64-mingw32-gcc-posix`.)*

### Updating Tracked Release Artifacts
When rebuilding the prebuilt binaries in `releases/`:
```bash
install -m 755 target/release/duplex-kvm releases/duplex-kvm-linux-x86_64
install -m 755 target/x86_64-pc-windows-gnu/release/duplex-kvm.exe releases/duplex-kvm-windows-x86_64.exe
(cd releases && sha256sum duplex-kvm-linux-x86_64 duplex-kvm-windows-x86_64.exe > SHA256SUMS.txt)
```
Use `install` (not `cp`) so an already-running local binary is unlinked cleanly rather than failing with `ETXTBSY`.

---

## 3. Verification & Test Suite

Always run these checks after modifying code:

1. **Fast Unit Tests & Clippy (Both Targets):**
   ```bash
   cargo test --workspace
   cargo clippy --workspace --all-targets
   cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu
   ```
   Both targets must compile with **zero warnings**.

2. **Python Setup & Layout Tests:**
   ```bash
   python3 -m unittest discover -s tests -v
   ```

3. **End-to-End Integration Tests (Linux):**
   - **Loopback Network + Crypto + Paced Video + Loss Recovery (`~12s`):**
     ```bash
     cargo test --release -p duplexd -- --ignored --nocapture
     ```
   - **GStreamer Capture → Shared-Memory FrameRing → DPXT → UDP → GTK Viewer (`~20s`, isolated in Xvfb):**
     ```bash
     xvfb-run -a -s "-screen 0 3840x2160x24" cargo test --release -p duplex-pal synthetic_capture_udp_viewer -- --ignored --nocapture
     ```
   - **4K DPXT Codec Throughput Benchmark:**
     ```bash
     cargo test --release -p duplex-proto uhd_encode_decode_throughput -- --ignored --nocapture
     ```

4. **Optional Headless GNOME / Mutter & Wine Harnesses:**
   - **GTK 4 Viewer Pixel-Exactness at Fractional Scaling (125%):**
     ```bash
     dbus-run-session -- scripts/testing/headless-gnome.sh python3 scripts/testing/viewer_exact.py 1.25
     ```
   - **Windows Unit + Viewer/GDI Capture Tests Under Wine:**
     ```bash
     CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=scripts/testing/wine-runner.sh \
       cargo test -p duplex-proto -p duplex-pal --target x86_64-pc-windows-gnu -- --include-ignored
     ```

---

## 4. Strict Safety & Architectural Invariants

1. **Never Run Unisolated Display/Input Tests on the Host Desktop:**
   - Video and viewer tests create virtual monitors and fullscreen windows. Always wrap them in `xvfb-run` or `scripts/testing/headless-gnome.sh`.
   - Note that `duplex-kvm` has no `--help` flag; unknown arguments launch the desktop GUI. Use `./target/release/duplex-kvm status` for non-interactive CLI smoke checks.

2. **Never Block the Daemon Main Loop on Video or GUI Work:**
   - `duplexd`'s main loop (`crates/duplexd/src/lib.rs`) must tick heartbeats at 50 Hz and forward HID events within `< 1 ms`. All video capture, DPXT encoding/decoding (`crates/duplex-pal/src/stream.rs`), and UDP video transmission/reception (`crates/duplexd/src/net.rs`) run on dedicated threads.

3. **Strict Interface Binding & Authentication:**
   - Never bind sockets to `0.0.0.0` (`Ipv4Addr::UNSPECIFIED`). `validate_strict_interface_ip` enforces binding to the dedicated link IP (`192.168.137.1` / `192.168.137.2`, or `127.0.0.x` in tests).
   - Never reintroduce a default, hardcoded, or all-zero pre-shared key. Production startup requires `~/.config/duplex-kvm/link.key` (`%APPDATA%\Duplex-KVM\link.key` on Windows) or `DUPLEX_PSK_HEX`.

4. **Preserve Emergency Reclamation (`duplex-guard`):**
   - Any new child process or window spawned by the video/display backends on Linux must record its PID via `persist_pids()` (`/tmp/duplex-kvm-linux-video.pids`) so `duplex-guard` can kill it if the daemon or link dies.
   - Encoder threads must never block indefinitely when the network TX queue is full; `deliver()` in `crates/duplex-pal/src/stream.rs` polls `control.stop` so mode teardown always finishes within milliseconds.

5. **Windows Mode 1+ Virtual Display Integrity:**
   - Never substitute a physical Windows monitor when no attached virtual display adapter is available for Mode 1+. Fail explicitly so the user knows to install/enable the Virtual Display Driver.
