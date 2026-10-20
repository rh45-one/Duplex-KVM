# Windows Defender investigation — 18 October 2026

## Report and verdict

Defender removed `C:\Users\hugot\Desktop\duplex-kvm-windows-x86_64.exe`
while it was being downloaded. Protection history names
`Trojan:Win32/Wacatac.C!ml`. The generic description about attacker commands
does not identify a specific code path. This was a pre-execution detection;
runtime actions on the user's PC cannot explain the initial alert.

**Assessment: a false positive is plausible, but unconfirmed.** No deliberate
malware was found in the source paths reviewed. This is not proof that the
downloaded artifact matches the source or that it is safe. Microsoft's detector
features and cloud verdict are unavailable in this Linux workspace.

The existing repository EXE had SHA256:

```text
bc6ccb149761ac3fa35fd00e7bbd56e426dc0b10563342c5d726c363baef964e
```

Compare that with the detected file if it is available to an analyst. A filename
alone cannot establish that the user's download was this artifact. Do not restore
the quarantined file just to run it.

## Evidence reviewed

- Windows input hooks, hotkeys and `SendInput` in `duplex-pal/src/windows/input.rs`;
  screen capture in `windows/video.rs`; daemon/watchdog lifecycle and peer networking.
  These implement the advertised KVM functionality and overlap with capabilities
  used by malicious remote-control software.
- Embedded Windows setup: PowerShell extraction to a predictable temporary file,
  nested `Start-Process -Verb RunAs`, and `ExecutionPolicy Bypass`. This is a
  plausible static detection contributor and exposed a script substitution risk
  between extraction and elevated execution.
- Executable PE headers/imports: unsigned, no resource directory, ordinary PE
  sections, no visible packer. Source/build workflow contains no packer or
  obfuscator. Lack of publisher identity is a trust limitation, not evidence of
  a Trojan and not an explanation of this specific detection.
- Updater: GitHub release API, HTTPS downloads and required release checksums;
  installation is opt-in by default. SHA256SUMS from the same release detects
  corruption; it is not an independent publisher signature or protection against
  a compromised repository/token/build environment.
- No hidden command execution endpoint, credential theft, antivirus exclusion,
  startup registry persistence, or scheduled task was found in the source reviewed.
- ClamAV 1.4.3 scanned the original EXE without a detection (exit 0), using database
  28144 dated 5 October 2026. Those definitions were 13 days old. An independent
  engine's negative result does not overrule Defender.

The exact cause remains unknown. Do not treat these plausible contributors as a
reverse-engineered explanation of Microsoft's classification.

## Changes

1. Windows setup uses `ShellExecuteW` with explicit UAC consent to launch only
   `--setup-elevated`. The helper checks administrator rights and resolves the
   system PowerShell executable by absolute path. It supplies the compile-time,
   readable network commands directly, without a temporary script, encoded
   command, nested PowerShell launcher, or execution-policy setting. Browser
   opening also uses the Shell API.
2. Opening the GUI requires an explicit daemon start (`Start Local Daemon`, mode
   selection, or `--autostart`) before background input/network operation.
3. A real security weakness was fixed independently of the alert: the public,
   hardcoded link encryption key allowed peer impersonation. Production startup
   now requires a saved private pairing key or `DUPLEX_PSK_HEX`, rejects invalid values and the old public/all-zero
   keys, and fails before initializing hardware or sockets. Both machines need
   the same private random key. Regression tests exercise this validation.
4. Release CI updates Defender intelligence and scans the staged EXE before
   upload. The custom scan uses `-DisableRemediation`, which ignores file
   exclusions and leaves detections in scan output; this avoids accepting a
   successful remediation as a clean scan. Missing Defender, stale definitions,
   scan errors or detections block publication. The report records SHA256,
   source commit, engine and intelligence versions and is attached to releases.

These changes improve consent, privilege handling, peer authentication and release
verification. They do not guarantee that Defender's machine-learning verdict will
change. They do not add a signing identity or encrypt the current video transport.

## Local validation

The `releases/` binaries were rebuilt from the current working tree, including its
pre-existing uncommitted changes, with Rust/Cargo 1.99.1. Windows uses the GNU
`x86_64-pc-windows-gnu` target and the locally extracted MinGW toolchain; Linux uses
the native target. Both builds used `--release --locked --offline`.
`releases/SHA256SUMS.txt` identifies these local artifacts.

- `cargo test --workspace --locked --offline`: 32 passed; three existing hardware/
  socket integration tests remain ignored.
- `cargo check --workspace --target x86_64-pc-windows-gnu --locked --offline`: passed.
- Both Windows and Linux release builds: passed.
- ClamAV scan of the rebuilt Windows EXE: zero detections, with the same dated
  definitions described above. This is not a Defender verdict.
- Python network-setup tests: six passed.
- Separate daemon process checks: absent, malformed, non-ASCII, all-zero and old
  public keys all exit with status 1 before hardware/network initialization.
- Workflow YAML parses and the Defender scan step precedes executable upload.
- `git diff --check`: passed.

The Windows setup helper, Defender PowerShell scan script and UAC interaction
cannot be exercised natively on this Linux host. The rebuilt EXE remains unsigned
and has **not** been validated by Microsoft Defender here. The CI scan gate has
been added but has not been run remotely. No release has been published.

## Windows validation and Microsoft's review

On a Windows validation machine, keep Defender enabled, build the intended source
and run from an administrator PowerShell:

```powershell
cargo test --workspace --locked
cargo build --release --locked --bin duplex-kvm
.\scripts\scan-windows-release.ps1 -BinaryPath .\target\release\duplex-kvm.exe
```

The scan does not execute the application or configure networking. Exercise UAC
accept/cancel, explicit daemon startup and actual KVM functionality separately on
the intended Windows/Linux pair. A clean local or CI scan cannot guarantee a
clean download-time cloud verdict on another machine.

If detection persists, submit the exact detected binary to Microsoft as a
**software developer**, include the SHA256, detection name, source/build provenance
and this behavior explanation, and await their determination. Do not use
repacking, antivirus exclusions or disabling protection to hide the detection.
No binary has been submitted to an external scanning service by this investigation.

For distributed builds, sign the final EXE with a trusted Authenticode certificate
and timestamp before scanning and checksumming. No certificate is available here;
self-signing would not establish a trusted publisher.

References:

- [Microsoft's software developer FAQ](https://learn.microsoft.com/en-us/defender-xdr/developer-faq)
- [Microsoft file submission](https://www.microsoft.com/en-us/wdsi/filesubmission)
- [MpCmdRun scan semantics and options](https://learn.microsoft.com/en-us/defender-endpoint/command-line-arguments-microsoft-defender-antivirus)

## Pairing usability follow-up — 18 October 2026

The GUI now supports **Web control center → Pair computers**, generating and
saving one random key on the first computer and importing it on the second.
`DUPLEX_PSK_HEX` remains an explicit override. Missing keys still stop the daemon
before hardware/network initialization. Saved-key generation, validation,
replacement, restart persistence, private Linux file permissions, and rejected
cross-origin/rebinding HTTP requests were checked. Both release binaries were
rebuilt and `SHA256SUMS.txt` refreshed. The earlier ClamAV result above describes
the earlier artifacts; it is not a scan of this subsequent rebuild.
