<#
.SYNOPSIS
    Scan a release with current Microsoft Defender intelligence and fail closed.
    Run on Windows as administrator. Does not add exclusions or disable protection.
#>
param(
    [Parameter(Mandatory = $true)]
    [string]$BinaryPath,
    [string]$ReportPath = "dist/defender-scan.txt"
)

$ErrorActionPreference = "Stop"
$PSNativeCommandUseErrorActionPreference = $false
$binary = (Resolve-Path -LiteralPath $BinaryPath).Path
$report = [System.IO.Path]::GetFullPath($ReportPath)
[System.IO.Directory]::CreateDirectory([System.IO.Path]::GetDirectoryName($report)) | Out-Null
$before = (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash

Update-MpSignature
$status = Get-MpComputerStatus
if (-not $status.AMServiceEnabled -or -not $status.AntivirusEnabled) {
    throw "Microsoft Defender Antivirus must be running; refusing to publish an unscanned executable."
}
if (-not $status.AntivirusSignatureLastUpdated -or
    $status.AntivirusSignatureLastUpdated -lt (Get-Date).AddDays(-2)) {
    throw "Defender signatures are stale; refusing to publish."
}

$platform = Join-Path $env:ProgramData "Microsoft/Windows Defender/Platform"
$scanner = Get-ChildItem -LiteralPath $platform -Directory -ErrorAction SilentlyContinue |
    Sort-Object Name -Descending |
    ForEach-Object { Join-Path $_.FullName "MpCmdRun.exe" } |
    Where-Object { Test-Path -LiteralPath $_ } |
    Select-Object -First 1
if (-not $scanner) {
    $scanner = Join-Path $env:ProgramFiles "Windows Defender/MpCmdRun.exe"
}
if (-not (Test-Path -LiteralPath $scanner)) {
    throw "Cannot find Microsoft Defender's MpCmdRun.exe."
}

@(
    "UTC: $([DateTime]::UtcNow.ToString('o'))"
    "Commit: $env:GITHUB_SHA"
    "File: $([System.IO.Path]::GetFileName($binary))"
    "SHA256: $before"
    "Engine: $($status.AMEngineVersion)"
    "Intelligence: $($status.AntivirusSignatureVersion)"
    "Intelligence updated: $($status.AntivirusSignatureLastUpdated.ToUniversalTime().ToString('o'))"
) | Set-Content -LiteralPath $report

# This custom-scan option ignores file exclusions and reports detections without
# remediating them. A normal remediating scan can return 0 even after finding malware.
& $scanner -Scan -ScanType 3 -File $binary -DisableRemediation 2>&1 |
    Tee-Object -FilePath $report -Append
$scanExit = $LASTEXITCODE
"Exit code: $scanExit" | Add-Content -LiteralPath $report
if ($scanExit -ne 0) {
    throw "Defender rejected the executable or scanning failed (exit $scanExit). See $report."
}
if (-not (Test-Path -LiteralPath $binary) -or
    (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash -ne $before) {
    throw "Executable disappeared or changed during scanning."
}
Write-Host "Defender scan passed for SHA256 $before. Report: $report"
