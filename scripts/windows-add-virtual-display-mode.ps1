#Requires -RunAsAdministrator
<#
.SYNOPSIS
    Add the laptop's native mode to an installed VirtualDrivers display driver.
.DESCRIPTION
    Run while the Duplex control center is open and in Idle. The original XML
    is backed up next to the configuration. Only Virtual Display Driver is
    restarted; Meta and Virtual Desktop adapters are not selected.
#>
param(
    [string]$ConfigPath = 'C:\VirtualDisplayDriver\vdd_settings.xml',
    [ValidateRange(320, 7680)][int]$Width = 1920,
    [ValidateRange(200, 4320)][int]$Height = 1200,
    [ValidateRange(1, 240)][int]$Refresh = 60
)

$ErrorActionPreference = 'Stop'
$status = Invoke-RestMethod -Uri 'http://127.0.0.1:48180/api/status' -TimeoutSec 3
if ($status.mode_code -ne 0 -or $status.input_grab_active -or $status.virtual_displays_active -ne 0) {
    throw 'Return Duplex to Idle before changing virtual-display modes.'
}
$devices = @(Get-PnpDevice -Class Display -FriendlyName 'Virtual Display Driver' -PresentOnly)
if ($devices.Count -ne 1 -or $devices[0].Status -ne 'OK') {
    throw 'Expected exactly one enabled Virtual Display Driver reporting OK in Device Manager.'
}
$config = Get-Item -LiteralPath $ConfigPath
$document = New-Object System.Xml.XmlDocument
$document.PreserveWhitespace = $true
$document.XmlResolver = $null
$document.Load($config.FullName)
$resolutions = $document.SelectSingleNode('/vdd_settings/resolutions')
if ($null -eq $resolutions) {
    throw 'Unsupported driver configuration format; configure the mode in Virtual Driver Control instead.'
}
$mode = $document.SelectSingleNode("/vdd_settings/resolutions/resolution[width='$Width' and height='$Height' and refresh_rate='$Refresh']")
if ($null -eq $mode) {
    $mode = $document.CreateElement('resolution')
    foreach ($setting in @(@('width', $Width), @('height', $Height), @('refresh_rate', $Refresh))) {
        $element = $document.CreateElement([string]$setting[0])
        $element.InnerText = [string]$setting[1]
        [void]$mode.AppendChild($element)
    }
    [void]$resolutions.AppendChild($mode)
    $temporary = Join-Path $config.DirectoryName ([System.IO.Path]::GetRandomFileName())
    $backup = $config.FullName + '.duplex-' + (Get-Date -Format 'yyyyMMdd-HHmmss-fff') + '.bak'
    try {
        $document.Save($temporary)
        [System.IO.File]::Replace($temporary, $config.FullName, $backup)
        Write-Host "Added ${Width}x${Height}@${Refresh}. Backup: $backup"
    } finally {
        if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary }
    }
} else {
    Write-Host "${Width}x${Height}@${Refresh} is already configured."
}

& "$env:SystemRoot\System32\pnputil.exe" /restart-device $devices[0].InstanceId
if ($LASTEXITCODE -eq 3010) {
    throw 'The mode is saved, but Windows requires a restart before it becomes available.'
}
if ($LASTEXITCODE -ne 0) {
    throw 'The mode is saved, but the virtual driver did not restart. Restart it in Device Manager before testing Mode 1+.'
}
for ($attempt = 0; $attempt -lt 30; $attempt++) {
    $device = Get-PnpDevice -InstanceId $devices[0].InstanceId
    if ($device.Status -eq 'OK') {
        Write-Host 'Virtual Display Driver reports OK. Mode 1+ can now be retried.'
        return
    }
    Start-Sleep -Milliseconds 200
}
throw 'The virtual driver has not returned to OK. Check its Device Manager problem code.'
