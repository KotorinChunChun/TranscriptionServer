#Requires -Version 7.4
[CmdletBinding(SupportsShouldProcess)]
param(
    [ValidateSet('Plan','Enable','Disable')][string]$Action='Plan',
    [string]$Program=(Join-Path $env:LOCALAPPDATA 'Programs/TranscriptionServer/TranscriptionServer.exe'),
    [ValidateRange(1,65535)][int]$Port=8769,
    [Parameter(Mandatory)][string[]]$AllowedNetwork,
    [Parameter(Mandatory)][string[]]$LocalAddress,
    [string]$ReportPath
)
$ErrorActionPreference='Stop'
function Normalize-Network([string]$Value) {
    $parts=$Value.Split('/')
    if($parts.Count -ne 2){throw '許可ネットワークはIPv4/CIDRで指定してください。'}
    $address=[Net.IPAddress]::Parse($parts[0])
    if($address.AddressFamily -ne [Net.Sockets.AddressFamily]::InterNetwork){throw 'IPv4だけを許可します。'}
    if($parts[1].Contains('.')) {
        $bits=([Net.IPAddress]::Parse($parts[1]).GetAddressBytes() | ForEach-Object {[Convert]::ToString($_,2).PadLeft(8,'0')}) -join ''
        if($bits -notmatch '^1*0*$'){throw 'ネットマスクが不正です。'}
        $prefix=$bits.TrimEnd('0').Length
    } else { $prefix=[int]$parts[1] }
    $network=[Net.IPNetwork]::new($address,$prefix)
    $private=$false
    foreach($range in @('10.0.0.0/8','172.16.0.0/12','192.168.0.0/16')){
        $parent=[Net.IPNetwork]::Parse($range)
        if($prefix -ge $parent.PrefixLength -and $parent.Contains($address)){$private=$true}
    }
    if(!$private){throw '許可ネットワークはプライベートIPv4内に限定してください。'}
    return $network.ToString()
}
try {
    $programPath=(Resolve-Path -LiteralPath $Program).Path
    $marker=Join-Path (Split-Path $programPath -Parent) 'INSTALLATION.json'
    $product=Get-Content -LiteralPath $marker -Raw | ConvertFrom-Json
    if($product.product -ne 'TranscriptionServer' -or $product.appId -ne '{6C0A7E36-24C5-4D67-8D18-0677E953741B}') {throw '導入済みTranscriptionServerのEXEを指定してください。'}
    $networks=@($AllowedNetwork | ForEach-Object {Normalize-Network $_} | Sort-Object -Unique)
    $addresses=@($LocalAddress | Sort-Object -Unique)
    $actualAddresses=@(Get-NetIPAddress -AddressFamily IPv4 | Where-Object AddressState -eq 'Preferred' | Select-Object -ExpandProperty IPAddress)
    foreach($address in $addresses) {
        if($address -notin $actualAddresses){throw "現在のLANアドレスではありません: $address"}
        if(!@($networks | Where-Object {([Net.IPNetwork]::Parse($_)).Contains([Net.IPAddress]::Parse($address))}).Count){throw 'LANアドレスが許可サブネットに含まれていません。'}
    }
    $name="TranscriptionServer-LAN-$Port"
    $report=@{action=$Action;rule=$name;program=$programPath;port=$Port;localAddress=$addresses;remoteNetwork=$networks;profile='Private';success=$false}
    if($Action -eq 'Plan'){$report.success=$true}
    else {
        $principal=[Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())
        if(!$principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)){throw 'このFW変更だけはWindowsの管理者実行が必要です。'}
        $existing=Get-NetFirewallRule -Name $name -ErrorAction SilentlyContinue
        if($existing) {
            $filter=$existing | Get-NetFirewallPortFilter
            $addressFilter=$existing | Get-NetFirewallAddressFilter
            $appFilter=$existing | Get-NetFirewallApplicationFilter
            $remote=@($addressFilter.RemoteAddress | ForEach-Object {Normalize-Network $_} | Sort-Object -Unique)
            if($filter.LocalPort -ne "$Port" -or $filter.Protocol -ne 'TCP' -or $filter.RemotePort -ne 'Any' -or
               ($remote -join ',') -ne ($networks -join ',') -or
               (@($addressFilter.LocalAddress | Sort-Object -Unique) -join ',') -ne ($addresses -join ',') -or
               $appFilter.Program -ne $programPath -or $existing.Profile -ne 'Private' -or
               $existing.Direction -ne 'Inbound' -or $existing.Action -ne 'Allow' -or $existing.EdgeTraversalPolicy -ne 'Block') {
                throw '同名FW規則の条件が異なります。既存規則は変更しません。'
            }
        }
        if($PSCmdlet.ShouldProcess($name,"LAN限定受信規則 $Action")) {
            if($Action -eq 'Disable'){if($existing){$existing | Disable-NetFirewallRule | Out-Null}}
            elseif($existing){$existing | Enable-NetFirewallRule | Out-Null}
            else {New-NetFirewallRule -Name $name -DisplayName 'TranscriptionServer LAN' -Direction Inbound -Action Allow -Enabled True -Protocol TCP -LocalPort $Port -LocalAddress $addresses -RemoteAddress $networks -Program $programPath -Profile Private -EdgeTraversalPolicy Block | Out-Null}
            $report.success=$true
        }
    }
    if($ReportPath){$report | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $ReportPath -Encoding utf8}
    $report | ConvertTo-Json -Depth 4
} catch {
    if($ReportPath){@{success=$false;error=$_.Exception.Message} | ConvertTo-Json | Set-Content -LiteralPath $ReportPath -Encoding utf8}
    throw
}
