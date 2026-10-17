#Requires -RunAsAdministrator
<#
.SYNOPSIS
    Duplex-KVM Windows 11 Desktop 1GbE Passthrough, Anti-Dropouts & DSCP QoS Setup Script.

.DESCRIPTION
    Run from an elevated Administrator PowerShell prompt with ExecutionPolicy Bypass:
        powershell -ExecutionPolicy Bypass -File .\scripts\windows-setup-passthrough.ps1

    Configures Computer B (Windows 11 Desktop):
    1. Assigns static IP 192.168.137.1/24 to the USB 1GbE adapter connected to the Debian Laptop
       (updating both PersistentStore and ActiveStore so New-NetIPAddress never fails with
       "Inconsistent PolicyStore Persistence and DHCP enabled").
    2. Enables IPv4 Routing & configures Kernel NAT (`New-NetNat -InternalIPInterfaceAddressPrefix 192.168.137.0/24`)
       between the USB 1GbE adapter and the Motherboard WAN RJ45 port.
    3. Disables USB Selective Suspend, Energy Efficient Ethernet (EEE), and NIC power-saving states on the USB 1GbE adapter.
    4. Enables WinSock DSCP IP_TOS marking (`DisableUserTOSSetting = 0`) and registers hardware QoS policies.
    5. Opens Windows Firewall rules strictly bound to local IP 192.168.137.1 and peer 192.168.137.2 (ports 48100-48103).

.PARAMETER KvmAdapterName
    Name of the USB 1GbE Network Adapter connected to the Debian Laptop (auto-detected if omitted).
#>

param(
    [Parameter(Mandatory = $false)]
    [string]$KvmAdapterName = ""
)

$ErrorActionPreference = "Stop"

$DuplexHostIp   = "192.168.137.1"
$DuplexMask     = "255.255.255.0"
$DuplexPeerIp   = "192.168.137.2"
$DuplexSubnet   = "192.168.137.0/24"
$NatName        = "DuplexKvmNat"

Write-Host "=== Duplex-KVM: Windows 11 1GbE Passthrough & QoS Provisioning ===" -ForegroundColor Cyan

# 1. Locate the USB 1GbE adapter
if ([string]::IsNullOrWhiteSpace($KvmAdapterName)) {
    $usbCandidate = Get-NetAdapter | Where-Object {
        $_.InterfaceDescription -match "USB|Realtek USB|ASIX|AX88179|RTL8153" -and $_.Status -ne "Disabled"
    } | Select-Object -First 1

    if ($null -eq $usbCandidate) {
        Write-Error "Could not auto-detect USB 1GbE adapter. Re-run with -KvmAdapterName '<Adapter Name>'."
        exit 1
    }
    $KvmAdapterName = $usbCandidate.Name
}

$kvmAdapter = Get-NetAdapter -Name $KvmAdapterName
$ifIndex = $kvmAdapter.ifIndex
Write-Host "[1/6] Selected KVM 1GbE Adapter: '$($kvmAdapter.Name)' ($($kvmAdapter.InterfaceDescription), ifIndex=$ifIndex)" -ForegroundColor Green

# 2. Disable USB Selective Suspend, Energy Efficient Ethernet (EEE), and Power Management
Write-Host "[2/6] Disabling USB Selective Suspend, EEE, and Power Saving on '$KvmAdapterName'..." -ForegroundColor Yellow
$advProps = @(
    "*SelectiveSuspend",
    "*EEE",
    "Energy Efficient Ethernet",
    "Green Ethernet",
    "Power Saving Mode",
    "*IdlePowerDownRestriction",
    "*WakeOnMagicPacket",
    "*WakeOnPattern"
)
foreach ($prop in $advProps) {
    $existing = Get-NetAdapterAdvancedProperty -Name $KvmAdapterName -ErrorAction SilentlyContinue |
        Where-Object { $_.RegistryKeyword -eq $prop -or $_.DisplayName -eq $prop }
    if ($null -ne $existing) {
        Set-NetAdapterAdvancedProperty -Name $KvmAdapterName -RegistryKeyword $existing.RegistryKeyword -RegistryValue 0 -ErrorAction SilentlyContinue
    }
}
Disable-NetAdapterPowerManagement -Name $KvmAdapterName -ErrorAction SilentlyContinue

# 3. Assign Static IPv4 192.168.137.1/24 to the USB 1GbE Adapter
#    Explicitly disable DHCP in BOTH PersistentStore and ActiveStore first, and use netsh static
#    assignment so Windows never throws "Inconsistent PolicyStore Persistence and DHCP enabled".
Write-Host "[3/6] Configuring Static IP $DuplexHostIp/24 on '$KvmAdapterName'..." -ForegroundColor Yellow
Set-NetIPInterface -InterfaceIndex $ifIndex -AddressFamily IPv4 -Dhcp Disabled -PolicyStore PersistentStore -ErrorAction SilentlyContinue
Set-NetIPInterface -InterfaceIndex $ifIndex -AddressFamily IPv4 -Dhcp Disabled -PolicyStore ActiveStore -ErrorAction SilentlyContinue

Get-NetIPAddress -InterfaceIndex $ifIndex -AddressFamily IPv4 -ErrorAction SilentlyContinue |
    Where-Object { $_.IPAddress -ne $DuplexHostIp } |
    Remove-NetIPAddress -Confirm:$false -ErrorAction SilentlyContinue

# Use netsh to atomically commit both PersistentStore and ActiveStore static IPv4 configuration
& netsh interface ipv4 set address name="$KvmAdapterName" static $DuplexHostIp $DuplexMask none | Out-Null

# Ensure ActiveStore has the IP immediately even if media sense is transitioning
$existingIp = Get-NetIPAddress -InterfaceIndex $ifIndex -IPAddress $DuplexHostIp -ErrorAction SilentlyContinue
if ($null -eq $existingIp) {
    New-NetIPAddress -InterfaceIndex $ifIndex -IPAddress $DuplexHostIp -PrefixLength 24 -PolicyStore ActiveStore -ErrorAction SilentlyContinue | Out-Null
}
Set-NetConnectionProfile -InterfaceIndex $ifIndex -NetworkCategory Private -ErrorAction SilentlyContinue

# 4. Enable IPv4 Forwarding & Configure Kernel NAT Passthrough (192.168.137.0/24 -> WAN)
Write-Host "[4/6] Enabling IPv4 Forwarding & Configuring NetNat '$NatName' ($DuplexSubnet)..." -ForegroundColor Yellow
Set-NetIPInterface -AddressFamily IPv4 -Forwarding Enabled -ErrorAction SilentlyContinue
Set-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters" -Name "IPEnableRouter" -Value 1 -Type DWord

$existingNat = Get-NetNat -Name $NatName -ErrorAction SilentlyContinue
if ($null -eq $existingNat) {
    # Remove any conflicting NetNat on the system (Windows Client supports 1 NetNat instance)
    Get-NetNat -ErrorAction SilentlyContinue | Remove-NetNat -Confirm:$false
    New-NetNat -Name $NatName -InternalIPInterfaceAddressPrefix $DuplexSubnet | Out-Null
}

# 5. Enable WinSock IP_TOS DSCP Tagging & Register QoS Policies
Write-Host "[5/6] Enabling WinSock DSCP IP_TOS support (EF=46, CS6=48, AF41=34)..." -ForegroundColor Yellow
Set-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters" -Name "DisableUserTOSSetting" -Value 0 -Type DWord

Get-NetQosPolicy -ErrorAction SilentlyContinue | Where-Object { $_.Name -like "DuplexKVM-*" } | Remove-NetQosPolicy -Confirm:$false
New-NetQosPolicy -Name "DuplexKVM-HID-EF"      -IPProtocol UDP -IPSrcPortStart 48100 -IPSrcPortEnd 48100 -DSCPAction 46 -NetworkProfile All | Out-Null
New-NetQosPolicy -Name "DuplexKVM-Control-CS6" -IPProtocol Both -IPSrcPortStart 48101 -IPSrcPortEnd 48101 -DSCPAction 48 -NetworkProfile All | Out-Null
New-NetQosPolicy -Name "DuplexKVM-Video-AF41"  -IPProtocol UDP -IPSrcPortStart 48102 -IPSrcPortEnd 48103 -DSCPAction 34 -NetworkProfile All | Out-Null

# 6. Configure Strict Interface-Scoped Windows Defender Firewall Rules
Write-Host "[6/6] Creating strict Windows Firewall rules scoped to $DuplexHostIp <-> $DuplexPeerIp..." -ForegroundColor Yellow
Get-NetFirewallRule -DisplayName "Duplex-KVM*" -ErrorAction SilentlyContinue | Remove-NetFirewallRule

New-NetFirewallRule -DisplayName "Duplex-KVM UDP (HID, Heartbeat, Video)" `
    -Direction Inbound -Action Allow -Protocol UDP `
    -LocalAddress $DuplexHostIp -RemoteAddress $DuplexPeerIp `
    -LocalPort 48100-48103 -Profile Any | Out-Null

New-NetFirewallRule -DisplayName "Duplex-KVM TCP (Control & Heartbeat)" `
    -Direction Inbound -Action Allow -Protocol TCP `
    -LocalAddress $DuplexHostIp -RemoteAddress $DuplexPeerIp `
    -LocalPort 48101 -Profile Any | Out-Null

Write-Host "=== Windows 11 Duplex-KVM Setup Complete! ($DuplexHostIp <-> $DuplexPeerIp) ===" -ForegroundColor Green
