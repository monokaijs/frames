param([string]$Action, [string]$BusId, [int]$Port, [string]$Token)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$utf8 = New-Object System.Text.UTF8Encoding($false)
[Console]::OutputEncoding = $utf8
function Exporter {
    $path = Join-Path $env:ProgramFiles 'usbipd-win\usbipd.exe'
    if (-not (Test-Path -LiteralPath $path)) { throw 'Install USB/IP export support using Frames devices setup usb-export.' }
    return $path
}
function Importer {
    $path = Join-Path $env:ProgramFiles 'USBip\usbip.exe'
    if (-not (Test-Path -LiteralPath $path)) { throw 'Install USB/IP receiving support on the host using Frames devices setup usb-host.' }
    return $path
}
function State {
    $value = & (Exporter) state | ConvertFrom-Json
    if ($LASTEXITCODE) { throw 'USB/IP device enumeration failed.' }
    return $value.Devices
}
function Isolated {
    if (@(Get-NetFirewallProfile | Where-Object { -not $_.Enabled }).Count) { throw 'Enable Windows Firewall before using USB forwarding.' }
    $rule = Get-NetFirewallRule -PolicyStore ActiveStore -Name 'Frames-USBIP-LocalOnly' -ErrorAction SilentlyContinue
    if (-not $rule -or $rule.Enabled -ne 'True' -or $rule.Action -ne 'Block' -or $rule.Direction -ne 'Inbound' -or $rule.Profile -ne 'Any') {
        throw 'Run Frames devices setup usb-export to restrict USB/IP to local connections.'
    }
    if ($rule.PrimaryStatus -ne 'OK' -or ($rule.EnforcementStatus -and $rule.EnforcementStatus -ne 'Full')) { throw 'Windows policy is not enforcing the Frames USB/IP isolation rule.' }
    $program = ($rule | Get-NetFirewallApplicationFilter).Program
    $addresses = ($rule | Get-NetFirewallAddressFilter).RemoteAddress
    $expected = @('0.0.0.0-126.255.255.255','128.0.0.0-255.255.255.255','::2-ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff')
    if ($program -ne (Exporter) -or @(Compare-Object $expected @($addresses)).Count) { throw 'USB/IP firewall configuration conflicts with Frames setup.' }
}
function Ancestors([string]$Instance) {
    $result = @($Instance)
    for ($i=0; $i -lt 16; $i++) {
        $parent = (Get-PnpDeviceProperty -InstanceId $Instance -KeyName 'DEVPKEY_Device_Parent' -ErrorAction SilentlyContinue).Data
        if (-not $parent -or $result -contains $parent) { break }
        $result += $parent; $Instance = $parent
    }
    return $result
}
function Native {
    if ('FramesUsbNative' -as [type]) { return }
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
public static class FramesUsbNative {
    [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)]
    public static extern SafeFileHandle CreateFile(string p,uint a,uint s,IntPtr sec,uint c,uint f,IntPtr t);
    [DllImport("kernel32.dll",SetLastError=true)]
    public static extern bool DeviceIoControl(SafeFileHandle h,uint c,IntPtr i,uint il,IntPtr o,uint ol,out uint n,IntPtr v);
    [DllImport("cfgmgr32.dll",CharSet=CharSet.Unicode)]
    public static extern uint CM_Locate_DevNodeW(out uint node,string id,uint flags);
    [DllImport("cfgmgr32.dll",CharSet=CharSet.Unicode)]
    public static extern uint CM_Request_Device_EjectW(uint node,out uint veto,System.Text.StringBuilder name,uint len,uint flags);
}
'@
}
if ($Action -eq 'installed') {
    if ($BusId -eq 'gamepad') { [bool](@(Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.InstanceId -like 'ROOT\VIGEMBUS*' }).Count) }
    elseif ($BusId -eq 'usb-export') { Test-Path -LiteralPath (Join-Path $env:ProgramFiles 'usbipd-win\usbipd.exe') }
    elseif ($BusId -eq 'usb-host') { Test-Path -LiteralPath (Join-Path $env:ProgramFiles 'USBip\usbip.exe') }
    else { throw 'Unknown device driver package.' }
} elseif ($Action -eq 'list') {
    $items = @(State | Where-Object { $_.BusId } | ForEach-Object {
        @{id=$_.BusId;name=$_.Description;physical_id=$_.InstanceId;forwarded=$false;error=$(if ($_.ClientIPAddress) {'Device is already attached to a USB/IP client.'} elseif ($_.PersistedGuid) {'Device is already shared outside this Frames attachment.'} else {$null})}
    })
    ConvertTo-Json -InputObject $items -Compress -Depth 5
} elseif ($Action -eq 'host-check') {
    $null = Importer
    $device = Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue | Where-Object { $_.InstanceId -like 'ROOT\USBIP_WIN2*' -and $_.Status -eq 'OK' }
    if (-not $device) { throw 'USB/IP receiving driver is unavailable. Run Frames devices setup usb-host on the host.' }
    Write-Output 'ok'
} elseif ($Action -eq 'isolate') {
    $path = Exporter
    if (@(State | Where-Object { $_.ClientIPAddress -and $_.ClientIPAddress -notin @('127.0.0.1','::1','::ffff:127.0.0.1') }).Count) {
        throw 'USB/IP has an active external client. Disconnect it before setting up Frames USB forwarding.'
    }
    # An explicit setup action owns this rule; other applications' rules stay intact.
    Get-NetFirewallRule -Name 'Frames-USBIP-LocalOnly' -ErrorAction SilentlyContinue | Remove-NetFirewallRule
    $null = New-NetFirewallRule -Name 'Frames-USBIP-LocalOnly' -DisplayName 'Frames USB/IP local access only' -Direction Inbound -Action Block -Enabled True -Profile Any -Program $path -RemoteAddress @('0.0.0.0-126.255.255.255','128.0.0.0-255.255.255.255','::2-ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff')
    Isolated
} elseif ($Action -eq 'lease') {
    $socket = New-Object Net.Sockets.TcpClient
    $socket.Connect('127.0.0.1',$Port)
    $writer = New-Object IO.StreamWriter($socket.GetStream(),$utf8); $writer.AutoFlush=$true
    $locks = New-Object 'System.Collections.Generic.List[Microsoft.Win32.SafeHandles.SafeFileHandle]'
    $bound=$false
    try {
        Isolated
        $device = State | Where-Object { $_.BusId -eq $BusId } | Select-Object -First 1
        if (-not $device -or $device.ClientIPAddress -or $device.PersistedGuid) { throw 'USB device is absent, busy, or already shared. Unshare it outside Frames first.' }
        Native
        foreach ($disk in @(Get-CimInstance Win32_DiskDrive)) {
            if ((Ancestors $disk.PNPDeviceID) -notcontains $device.InstanceId) { continue }
            foreach ($partition in @(Get-Partition -DiskNumber $disk.Index -ErrorAction Stop)) {
                foreach ($volume in @($partition | Get-Volume -ErrorAction SilentlyContinue)) {
                    if (-not $volume.Path) { continue }
                    $handle=[FramesUsbNative]::CreateFile($volume.Path.TrimEnd('\'),[uint32]3221225472,3,[IntPtr]::Zero,3,0,[IntPtr]::Zero)
                    if ($handle.IsInvalid) { $handle.Dispose(); throw 'Cannot open the USB volume. Close applications using this drive.' }
                    $locks.Add($handle); [uint32]$count=0
                    if (-not [FramesUsbNative]::DeviceIoControl($handle,0x90018,[IntPtr]::Zero,0,[IntPtr]::Zero,0,[ref]$count,[IntPtr]::Zero)) { throw 'USB drive is busy. Close files and applications before forwarding it.' }
                    if (-not [FramesUsbNative]::DeviceIoControl($handle,0x90020,[IntPtr]::Zero,0,[IntPtr]::Zero,0,[ref]$count,[IntPtr]::Zero)) { throw 'Cannot safely unmount the USB volume.' }
                }
            }
        }
        $null = & (Exporter) bind --busid $BusId 2>&1
        if ($LASTEXITCODE) { throw 'USB/IP could not bind the selected device.' }
        $bound=$true
        $writer.WriteLine((@{token=$Token;ok=$true} | ConvertTo-Json -Compress))
        # The parent owns this connection. EOF revokes the device even after a crash.
        while ($socket.GetStream().ReadByte() -ne -1) {}
    } catch {
        try { $writer.WriteLine((@{token=$Token;ok=$false;error=$_.Exception.Message} | ConvertTo-Json -Compress)) } catch {}
    } finally {
        if ($bound) { $null = & (Exporter) unbind --busid $BusId 2>&1 }
        foreach ($handle in $locks) { $handle.Dispose() }
        $socket.Dispose()
    }
} elseif ($Action -eq 'detach') {
    Native
    $matched=$false
    foreach ($device in @(Get-PnpDevice -PresentOnly | Where-Object { $_.InstanceId -like 'USB\VID_*' -and $_.InstanceId -notlike '*&MI_*' })) {
        $location = (Get-PnpDeviceProperty -InstanceId $device.InstanceId -KeyName 'DEVPKEY_Device_LocationInfo' -ErrorAction SilentlyContinue).Data
        if ($location -notmatch ('^Port_#'+$Port.ToString('0000')+'\.')) { continue }
        if (-not @((Ancestors $device.InstanceId) | Where-Object { $_ -like '*USBIP_WIN2*' -or $_ -like '*USBIP2*' }).Count) { continue }
        $matched=$true; [uint32]$node=0; [uint32]$veto=0; $name=New-Object Text.StringBuilder 256
        if ([FramesUsbNative]::CM_Locate_DevNodeW([ref]$node,$device.InstanceId,0) -ne 0) { throw 'Cannot locate the imported USB device.' }
        $status=[FramesUsbNative]::CM_Request_Device_EjectW($node,[ref]$veto,$name,256,0)
        if ($status -ne 0) { throw ('USB device is busy; safe eject was refused ('+$name.ToString()+'). Close its files and applications.') }
    }
    if (-not $matched) { throw 'Could not identify the imported device for safe eject. Eject it in Windows before disconnecting.' }
    $null = & (Importer) detach --port $Port 2>&1
    Write-Output 'ok'
} else { throw 'Unknown USB operation.' }
