# Agent notes for Duplex-KVM

## Build

- Linux: `cargo build --release --bin duplex-kvm`
- Windows (cross, from Linux): the MinGW linker lives in the repo's target dir:
  `CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=$PWD/target/mingw-sysroot/usr/bin/x86_64-w64-mingw32-gcc-posix cargo build --release --bin duplex-kvm --target x86_64-pc-windows-gnu`
- Release artifacts: copy both binaries into `releases/` (use `install`, not
  `cp`, in case one is running) and regenerate `releases/SHA256SUMS.txt` with
  `sha256sum duplex-kvm-linux-x86_64 duplex-kvm-windows-x86_64.exe`.
- The app binary has no `--help`; unknown arguments open the GUI. Use
  `duplex-kvm status` for a non-interactive smoke test.

## Verify

- `cargo test --workspace` and `cargo clippy --workspace --all-targets`; repeat
  clippy with `--target x86_64-pc-windows-gnu` (same linker variable). Both
  targets are expected to be warning-free.
- `python3 -m unittest discover -s tests` (setup script and layout helper).
- Integration (optimised builds; need sockets, Python GI/GStreamer, X11):
  - `xvfb-run -a -s "-screen 0 3840x2160x24" cargo test --release -p duplex-pal synthetic_capture_udp_viewer -- --ignored --nocapture`
  - `cargo test --release -p duplexd -- --ignored --nocapture`
- Windows code under Wine:
  `CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=scripts/testing/wine-runner.sh` plus the
  linker variable, then `cargo test -p duplex-proto -p duplex-pal --target x86_64-pc-windows-gnu -- --include-ignored`.
- Real Mutter capture and viewer exactness without touching the live desktop:
  `dbus-run-session -- scripts/testing/headless-gnome.sh ...` (see
  `docs/project-status.md`, "Reproducing checks").
- Never run video tests on the live desktop without asking: they create
  virtual monitors and fullscreen windows.

## Pitfalls

- Tools that rewrite files on disk (`cargo clippy --fix`, `rustfmt`, scripts)
  can leave the editor's buffer of that file stale; edits made through the
  editor afterwards may silently not reach disk. Check with `grep`/`sed` after
  such tools run, and re-sync a file by writing its full current content.
- `cargo clippy --fix` can outlive the command that started it; wait for it to
  finish before editing.
- Rust lint `chunks_exact_to_as_chunks` is active: prefer `as_chunks::<N>()`.
