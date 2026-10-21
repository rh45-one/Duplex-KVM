# Project to-do implementation and validation

Updated: 2026-10-21.

> [!NOTE]
> Project status: Alpha / Beta. Duplex-KVM is currently in active development. Virtual display management, KVM input capture, and hardware acceleration are undergoing active testing and stabilization across hardware setups.

## Screen mirroring fixes implemented

- Mode transitions are idempotent. The three redundant control datagrams no longer
  tear down and recreate capture/viewer processes three times. Older epochs cannot
  undo a newer mode. The control/heartbeat port is **48101**; 48100 carries HID and
  cursor updates, and 48102/48103 carry video with AF41 QoS.
- A bounded transition worker keeps authenticated control heartbeats flowing during
  synchronous setup, sharing the control crypto sequence/replay window. It stops
  after the existing three-second progress cap so a hung setup still expires.
- Encoder teardown disconnects the frame receiver before joining producers. This
  releases a producer blocked on a full queue instead of deadlocking the daemon
  and triggering watchdog reclamation.
- Each network poll has a packet budget so continuous video yields to heartbeats.
  PLI requests are coalesced and limited to one per monitor per 500 ms. Encoders
  retain pending refresh requests during that interval. Receiver assemblers reset
  when entering a receiving mode, because sender sequences restart at zero.
- Linux GTK windows preserve their last frame during a stream pause. Frame
  presentation runs on the GTK thread and coalesces pending draws. EOF, helper
  exit, broken pipes, GStreamer errors, and closed capture sessions are logged.
- Mode 2 now requests **two virtual outputs**, including stream zero, instead of
  mirroring the laptop panel for one stream. PipeWire source caps specify the
  native output size. Updated peers negotiate native-resolution transport;
  older peers retain the 960×540 compatibility canvas.
- Mutter uses RecordVirtual. Other Wayland desktops use the ScreenCast portal
  only when it advertises VIRTUAL sources; permission dialogs may appear. An
  unsupported compositor reports an error. Capture failure no longer substitutes
  synthetic video. X11 requires actual provisionable virtual connectors and
  reports xrandr activation failures.
- Windows receiver creation reports CreateWindowExW failures or startup timeouts.
  GDI capture flushes drawing before reading DIB pixels and logs capture failures.
- GUI-launched daemons and watchdogs write diagnostics to `logs/daemon.log` under
  the application configuration directory. Foreground daemons retain console logs.

Coordination lives in `crates/duplexd/src/lib.rs` and `net.rs`; this checkout
has no `crates/duplexd/src/peer.rs`.

The video path is **Mutter/portal/X11 (Linux) or DXGI with GDI fallback
(Windows) capture → DPXT tiles → paced UDP threads → decoder threads → GTK 4
shared-memory viewer (Linux) or Win32 front-buffer windows (Windows)**. See
"Native-resolution streaming (DPXT)" below. DPXC/DPXB bands remain decodable
and are still sent to peers that predate DPXT. There is no inter-frame decoder
dependency: every tile is independently decodable and periodically refreshed.
These changes address concrete lifecycle and capture defects; they do not prove
which defect caused the reported seven-second shutdown on the physical setup.

## Polish items implemented

- Nine device-independent Linux gesture tests cover continuous-press tap-to-drag,
  the 280 ms boundary, sensor jitter, physical movement/hold rejection, a single
  tap's timeout release, ordinary double taps, two-finger right click, and
  releasing a drag when another finger arrives. Cancellation suppresses an
  unintended extra tap.
- A real GStreamer `videotestsrc` → Rust encoder → UDP → assembler → decoder →
  GTK integration test passed with **12 seconds of two-stream video followed by
  8 seconds without frames**, then bounded teardown with full encoder queues.
- A second integration test passed for **12 seconds**, exercising both mode
  directions, simulated packet loss, bounded PLI, and watchdog liveness alongside
  authenticated control heartbeats on isolated loopback addresses.
- A third integration test stalls local control polling for four seconds and
  checks authenticated liveness during setup, expiry at the three-second cap,
  delivery of deferred mode packets, and crypto handoff back to the main loop.
- Private GitHub release updates support a PAT, background checking, optional
  automatic installation, and manual installation from the control UI/CLI.
  HTTPS asset API downloads require matching sizes and valid SHA256SUMS.txt
  entries before replacement. Errors clear busy state; concurrent operations and
  repeated installs before restart are rejected. Linux token configuration files
  use mode 0600. The UI posts tokens in JSON bodies and rejects cross-site writes.
- Release builds stamp their tag into the compiled version so the updater does
  not repeatedly offer an already-installed release. The release workflow runs
  unit tests on both platforms and the synthetic video and control integration tests on Linux.

## Reproducing checks

```bash
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked
xvfb-run -a -s "-screen 0 3840x2160x24" cargo test --release -p duplex-pal --locked synthetic_capture_udp_viewer -- --ignored --nocapture
cargo test --release -p duplexd --locked -- --ignored --nocapture
cargo test --release -p duplex-proto --locked uhd_encode_decode_throughput -- --ignored --nocapture
cargo clippy --workspace --all-targets --locked --target x86_64-pc-windows-gnu
```

The integration tests are explicitly selected because they need local sockets,
Python GI/GStreamer plugins, and an X11 display (Xvfb is sufficient). They isolate
PID registries and watchdog state from a running application.

Windows code can also be exercised under Wine with a test runner wrapper that
starts `xvfb-run -a -s "-screen 0 1920x1080x24" wine "$@"`
(`CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER`). With `--include-ignored` this runs
the Windows viewer/capture test, which streams a pattern into a fullscreen
viewer window and requires the captured screen to match pixel for pixel (Wine
has no Desktop Duplication, so this covers the GDI fallback).

The real Mutter capture path and the GTK 4 viewer can be checked without
touching the live desktop inside a private headless GNOME Shell with separate
`HOME`/`XDG_*` directories:

```bash
dbus-run-session -- scripts/testing/headless-gnome.sh python3 scripts/testing/viewer_exact.py 1.25
cargo test --release -p duplex-pal --no-run   # then run the test binary inside the shell:
DUPLEX_TEST_SIZE=3840x2160 DUPLEX_TEST_ANIMATE="python3 scripts/testing/animate.py 1 scroll" \
  dbus-run-session -- scripts/testing/headless-gnome.sh \
  target/release/deps/duplex_pal-<hash> virtual_monitor_capture_throughput --ignored --nocapture
```

`scripts/testing/wine-runner.sh` is the Wine runner mentioned above.

## Remaining hardware acceptance checks

- Run Mode 1+ for at least one minute on the Windows/Linux pair, including a
  static desktop and animated content. Check daemon logs if a viewer exits;
  distinguish a helper failure from an explicit watchdog reset.
- Enter Mode 2 from **each** peer. Confirm two actual Linux virtual outputs appear
  in desktop settings and both physical Windows monitors display their streams.
  Exercise portal consent on KWin and verify cancellation reports an error.
- Repeatedly cycle Mode 0 → Mode 1 → Mode 1+ → Mode 2 → Mode 0 and verify the
  emergency input reclaim still works. The 250 ms watchdog threshold is retained.
- Confirm Windows IddCx driver/service availability for a genuine extra Windows
  output. This repository contains client plumbing, not an installable IddCx
  driver. Missing or unusable adapters now fail Mode 1+ explicitly; capture does
  not substitute an existing physical monitor.
- With a real private release and authorized token, test downloading and replacing
  an older executable on both operating systems. No private release was fetched,
  executable replaced, or GitHub release published during local validation.

Mutter's API is private and version-dependent; virtual source availability is
compositor-dependent. See the [Mutter ScreenCast interface](https://github.com/GNOME/mutter/blob/main/data/dbus-interfaces/org.gnome.Mutter.ScreenCast.xml),
[ScreenCast portal specification](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html),
and [GitHub release asset API](https://docs.github.com/en/rest/releases/assets).

## Pairing and internet-sharing follow-up — 18 October 2026

The application now saves private pairing keys separately from updater settings.
Use **Web control center → Pair computers** on both machines. Generate a key on
one machine, transfer it privately, and save it on the other; matching
fingerprints confirm the configuration. The daemon also reads this saved key,
so ordinary GUI launches do not need environment variables. Explicit
`DUPLEX_PSK_HEX` overrides still take precedence, and public/default keys remain
rejected.

The laptop's reported passthrough problem was traced to the ordinary DHCP
profile being selected on a static PC link, plus missing gateway/DNS settings in
the manual KVM profile. The saved manual profile was corrected and activated on
`enp2s0`, with gateway `192.168.137.1` and fallback route/DNS priority `2000`.
An HTTPS request explicitly bound to Ethernet succeeded. Wi-Fi remained active.
This verification is independent of daemon pairing and does not require Windows
to answer ICMP pings.

## Peer standby caused by stale daemons — 18 October 2026

Three old Linux daemon processes were alive simultaneously. PID 64711 held the
five peer sockets with an undrained control queue and a blocked main thread;
PID 70549 was refreshing the shared status block but held no peer sockets. A
newer daemon could previously overwrite shared daemon identity before failing
its port binds, hiding the process that actually needed recovery.

The stale daemons and their guards were stopped while idle. After saving the
existing Windows pairing key on the laptop, the current daemon (PID 150079)
bound all five sockets and exchanged authenticated heartbeats/acknowledgements
with Windows. Observed RTT was approximately 0.65 ms.

Daemon ownership is now locked on the shared watchdog object (Linux) or a named
kernel object (Windows), before shared state is initialized. A live legacy
owner is also detected. Duplicate launches leave daemon identity and telemetry
untouched. GUI launch requests are serialized, immediate child failures are
reported, and exited children are reaped. Linux process checks distinguish
unreaped zombies from live daemons. Watchdogs stop when their supervised daemon
exits or ownership changes.

Regression tests cover duplicate ownership, unchanged telemetry, replacement
following owner release, live legacy owners, and zombie process detection. A
live duplicate-start attempt was rejected while preserving the connected
PID and its authenticated peer session.

## Input outage traced to USB adapter and reconnect profile — 18 October 2026

Live evidence distinguished the current incident from pairing and capture errors:

- Linux's `enp2s0` was administratively up but reported `NO-CARRIER`, carrier `0`,
  and no IPv4 address. The adapter's r8169 driver was loaded and runtime power
  state was active. Input-device permissions and group membership were correct.
- Windows `Get-NetAdapter -Physical` listed only the built-in Realtek internet
  adapter and Wi-Fi. The USB Ethernet adapter for the laptop was absent. The
  user confirmed no link lights. The specific cause of that USB disappearance
  (port, power, dongle, or driver) was not established.
- Reconnecting the USB adapter to another Windows port restored enumeration and
  physical carrier on Linux. Linux then selected `Wired connection 1`, waiting
  for DHCP. Activating the saved `duplex-kvm-1gbe` profile restored
  `192.168.137.2/24` and authenticated heartbeats, with RTT around 0.6–1.4 ms.
- The user tested Mode 1 in Windows Notepad and confirmed that both the laptop
  keyboard and trackpad worked. Before recovery, daemon logs showed successful
  Linux device grabs followed by peer-heartbeat expiry and watchdog return to
  Idle. The watchdog was responding to the lost connection.

Additional changes make these failures visible and prevent misleading status:

- Web status distinguishes missing local addressing and Linux physical carrier
  loss from missing authenticated heartbeats. It detects a different running
  Linux daemon executable. Native UI no longer labels missing heartbeats as
  standby. Idle mode no longer claims internet passthrough is necessarily active.
- Connect PC link activates the existing manual NetworkManager profile and
  preserves its saved passthrough configuration. The profile stays manual so
  normal router/DHCP use of the same laptop port remains available.
- Local mode requests require a recent authenticated peer. Transition watchdog
  grace timestamps and unauthenticated video no longer count as proof of a peer.
  Removed local addressing discards stale transport sockets and retries binding.
- UDP HID/control send errors propagate; daemon input capture, send, and
  injection errors are logged and request Idle. Linux rejects capture if it
  acquired no devices, and failed capture setup cleans up and notifies the peer.
- Windows capture waits for successful window, Raw Input, and hook setup before
  reporting success. Message-queue creation precedes publishing the hook thread
  ID, closing the lost-WM_QUIT startup race. Partial initialization cleans up.
  SendInput's return count is checked, and input errors identify possible Windows
  integrity-level blocking. These Windows changes cross-compile; they have not
  been exercised in a native Windows runtime in this session.
- Idle-only daemon restart and preservation of the previous log support applying
  a new executable without leaving an older background daemon unnoticed.
  Live restart testing exposed delayed lock release during Linux device close;
  restart now probes actual ownership release before launching the replacement.
  An isolated regression covers an exited identity with a still-held descriptor.

Validation: 38 Rust unit tests, six NetworkManager script tests, Windows
cross-compilation, and three explicitly selected socket regressions passed.
The 12-second synthetic test now verifies encrypted HID in both directions
alongside video/control and loss recovery, including input arriving after eleven
seconds. Mismatched keys never mark a peer authenticated; transition keepalive
expiry remains bounded. Both release artifacts and checksums were rebuilt.
The laptop GUI and daemon were restarted while Idle, their actual process image
hashes were checked against the new release, and peer authentication recovered.

Physical display follow-up in the same session: Mode 1+ remained active for about
30 seconds and received 1,819 encoded video bands while transmitting input, then
returned to Idle through the operator hotkey without a watchdog reset. A bounded
Mode 2 check created two Mutter virtual capture sessions and transmitted 1,009
video bands; the peer stayed authenticated and Idle cleanup removed both virtual
outputs. Those counters establish transport/capture activity, not visual
correctness of both Windows viewers; visual confirmation remains separate.
The final Idle-only restart succeeded and all 50 subsequent connection samples
were authenticated (maximum observed peer age 17.58 ms). Ethernet-bound HTTPS
also succeeded after recovery. The laptop was left in Idle with its link active.

## Display quality and gesture follow-up

The user confirmed that Mode 1+ stayed open but mirrored the second physical
Windows monitor. Mode 2 displayed two separate Linux outputs on both Windows
monitors, with poor clarity and a pointer return trap. The requested drag gesture
is tap, then touch again and hold while moving.

- Windows Mode 1+ now activates and captures an actual supported virtual adapter
  by identity. Capture is restricted to outputs claimed by Duplex. The Meta and
  Virtual Desktop adapters are excluded from physical destinations and never
  claimed for Mode 1+. Window rescue also touches only Duplex-owned outputs.
  Failed setup returns to Idle and reports the error to the peer and web status.
  Both reported PnP errors were CM_PROB_DISABLED (Code 22). The user enabled
  Virtual Display Driver and confirmed it now reports OK; Virtual Desktop
  Monitor stays disabled. Native Windows activation remains pending verification.
- Both peers advertise native-video support after authentication. The new codec
  preserves full-colour pixels at each advertised resolution with independent
  zlib bands and a bounded traffic budget. Deferred bands are retried, allowing
  static content to converge without a lossy 540p upscale. Rapidly changing 4K
  content can update more slowly within the budget. Existing peers remain
  compatible; native quality requires updating both executables.
- The Linux viewer starts fullscreen, preserves aspect ratio, and accepts actual
  decoded dimensions. RGB conversion runs in its worker so a 4K frame does not
  block daemon heartbeats. Late/replayed fragments and malformed compressed or
  legacy bands cannot overwrite a newer frame or partially corrupt the canvas.
- The Windows viewer and EDID advertisement now share one physical-monitor
  order. Per-monitor DPI awareness prevents scaled capture/viewer coordinates
  from confusing physical pixel dimensions.
- A measured Mutter layout had a 960-pixel laptop edge next to 2160- and
  1080-pixel remote edges. The temporary layout helper selects supported scales
  and aligns only newly created outputs. A live layout check produced 864-pixel
  remote edges beside the unchanged 960-pixel laptop edge, allowing return from
  any vertical position on either remote display. Automatic application in the
  updated daemon passed a 35-second live Mode 2 check; the user confirms the
  pointer no longer gets stuck. All 140 connection samples were authenticated
  (maximum peer age 17.94 ms), 2,444 bands were transmitted, and automatic return
  to Idle removed both virtual outputs while preserving the laptop layout.
  This deliberate test timeout explains the windows closing at its end.
- Trackpad tap recognition now uses evdev event timestamps and maximum physical
  displacement from touchdown instead of accumulated coordinate jitter. The
  hardware-calibrated tolerance is 3 mm. Live logs confirmed recognition, but the
  user still observed a double click. The original first-tap click plus second
  press could dispatch Windows' double-click behavior. Release is now deferred
  for the 280 ms drag decision window: a held second contact continues one press;
  two quick taps still generate two clicks. Timeout polling releases a single
  tap without requiring another device event, and Windows panic/mode cleanup now
  releases mouse buttons too. Nine gesture tests pass. Live user acceptance of
  this second correction is confirmed: the user reports that dragging now works.

Validation: 46 Rust unit tests (including nine gesture tests), eight Python
setup/layout tests, and four
explicitly selected video/control integration tests passed. The native synthetic
test sends 3840×2160 and 1920×1080 through GStreamer, UDP, decoding, and GTK for
12 seconds, preserves viewers through eight idle seconds, and tears down within
the deadline. Linux release and Windows cross-builds succeed. Windows driver
repair, native Windows runtime checks, and visual acceptance are separate from
these automated checks.

Both release artifacts and SHA256SUMS.txt were rebuilt after the gesture
correction. The laptop GUI and daemon were restarted in Idle, actual process
image hashes matched the release, and the authenticated peer reconnected. The
Windows operator has installed the updated executable, restarted its daemon, and
confirmed the authenticated peer reconnects. Visual display acceptance remains
separate from successful installation and the confirmed drag behavior.

The first live Mode 2 capture helpers still used 960×540 despite the Windows
update report, and the user confirmed quality remained poor. Hashes and process
paths identified the cause: the new control center ran from S:\\, but active
daemon PID 29896 still ran the older Desktop executable. An explicit Idle-only
restart launched PID 26608; the laptop then logged native_lossless=true.
The native control centers now offer Start / restart daemon instead of returning
"already running" when applying a new build, and Windows web/native diagnostics
detect a daemon running from a different executable. These UI changes pass both
Linux and Windows all-target checks. Native-resolution visual acceptance is
being checked separately from successful capability negotiation.

## Native-resolution streaming (DPXT)

The band codecs could not deliver sharp 4K at interactive rates: Linux capture
was still forced to 30 fps, every changed band was re-sent losslessly, and video
packets were sent and received on the daemon's input/heartbeat loop. The video
path was redesigned:

- **Codec (`duplex-proto/src/tile_codec.rs`).** 64-pixel tiles; only changed
  tiles are sent. Each tile is a solid colour, LZ4-lossless (text and interface
  graphics), or part of a JPEG strip (photos and video). A cheap flatness test
  routes photographic tiles straight to JPEG; change detection uses two threads
  because it is memory-bound. Lossy tiles are refined to exact pixels 250 ms
  after they stop changing, and every tile is re-sent over ten seconds.
  Slices are at most 60 KB, carry session/frame numbers, and are applied only
  when complete; decoders detect lost slices and request a refresh. Malformed,
  replayed, or old-session data is rejected without touching the canvas.
- **Transport (`duplexd/src/net.rs`).** One paced transmit thread (token bucket,
  700 Mbit/s default, `DUPLEX_VIDEO_MBPS`) and one blocking receive thread per
  monitor. Reassembly allocates once per unit, resynchronises when a sender
  restarts its numbering, and reports gaps for rate-limited refresh requests.
  Peers advertise `VideoCapabilitiesV2`; older peers get DPXC bands.
- **Pipeline (`duplex-pal/src/stream.rs`).** Capture threads publish to a
  newest-frame slot; an encoder thread codes on a below-normal-priority pool
  (half the CPUs, at most eight) and never blocks teardown when the link stops
  draining. Decoder threads present complete frames, or a partial frame after
  30 ms of silence. A failed capture helper or viewer returns both peers to Idle
  with an error instead of leaving a frozen image.
- **Linux.** The GStreamer helper copies frames into a shared-memory ring (one
  copy, no 4K pipe bottleneck) and requests twice the target rate from Mutter,
  whose frame limiter otherwise halves 60 fps content. The GTK 4 viewer maps the
  decoded canvas and draws a plain texture node aligned to the device-pixel
  grid. The setup script raises `net.core.rmem_max`/`wmem_max` for 4 MiB video
  socket buffers.
- **Windows.** DXGI Desktop Duplication (change-driven GPU copy) with automatic
  GDI fallback and periodic DXGI retry; the cursor is composited. Viewers keep a
  front buffer updated by the decoder and repaint only invalidated regions,
  1:1 or with aspect-preserving HALFTONE scaling, with the cursor hidden.

Validation (automated, this machine):

- Workspace tests on Linux and on the Windows target under Wine; clippy is clean
  for both targets. 26 codec tests cover exact text, JPEG-then-exact
  refinement, changed-tile-only updates, background refresh, slice limits, loss
  detection, session/size changes, replay protection and malformed input.
- Loopback integration: both mode directions through the real paced/threaded
  link with injected loss; refreshes stay bounded, decoded canvases converge to
  the exact final frames, and heartbeats/HID are unaffected.
- GStreamer → frame ring → DPXT → UDP → GTK 4 viewer processes for 3840×2160 and
  1920×1080 streams (Xvfb), surviving eight idle seconds with bounded teardown.
- Headless GNOME Shell: the GTK 4 viewer's output matched a one-pixel
  checkerboard exactly (0 of 2,304,000 pixels differing) at desktop scales 1.0,
  1.25, 1.5 and 2.0. The previous GTK 3 viewer blurs at fractional scales.
- Headless GNOME Shell, real `RecordVirtual` capture and encoding (several
  runs): 4K typing 52–56 fps (~1 Mbit/s), 4K scrolling text typically 52–56 fps
  with one run at 42 (115–124 Mbit/s), full-screen 4K photo motion 34 fps
  (~250 Mbit/s), 1080p photo motion 59 fps. Before these changes the same tests
  measured 30–31, 31 and 19 fps at 4K.
- Wine: a fullscreen Windows viewer presented a streamed pattern that the GDI
  capture path read back pixel-exactly.

Not verified here: DXGI Desktop Duplication on real Windows hardware (Wine
lacks it; the GDI fallback is what was exercised), Windows viewer appearance on
physical monitors, and the complete two-machine session. Run the hardware
acceptance checks above after installing the new builds on both computers.
