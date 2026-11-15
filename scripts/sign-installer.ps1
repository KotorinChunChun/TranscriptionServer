[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidateSet('Check', 'Sign', 'Verify')][string]$Mode,
    [string]$Path,
    [string]$CertificateThumbprint = $env:TRANSCRIPTION_SIGNING_THUMBPRINT,
    [string]$CertificateSubject = $env:TRANSCRIPTION_SIGNING_SUBJECT
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# Use an explicitly configured identity; never auto-select or export private keys.
if ($CertificateThumbprint -notmatch '^[A-Fa-f0-9]{40}$' -or [string]::IsNullOrWhiteSpace($CertificateSubject)) {
    throw 'TRANSCRIPTION_SIGNING_THUMBPRINT（40桁）とTRANSCRIPTION_SIGNING_SUBJECTを設定してください。'
}
$thumbprint = $CertificateThumbprint.ToUpperInvariant()
$subject = $CertificateSubject
$timestampUrl = 'http://timestamp.digicert.com'

function Find-SignTool {
    $command = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($command) { return $command.Source }
    $sdkRoot = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    $versions = Get-ChildItem -LiteralPath $sdkRoot -Directory -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match '^\d+\.\d+\.\d+\.\d+$' } |
        Sort-Object { [version]$_.Name } -Descending
    foreach ($version in $versions) {
        $candidate = Join-Path $version.FullName 'x64\signtool.exe'
        if (Test-Path -LiteralPath $candidate -PathType Leaf) { return $candidate }
    }
    throw 'Windows SDKのsigntool.exeがありません。署名なしでは続行しません。'
}

$signTool = Find-SignTool
if ($Mode -ne 'Verify') {
    $certificate = Get-Item -LiteralPath "Cert:\CurrentUser\My\$thumbprint" -ErrorAction Stop
    $now = Get-Date
    if ($certificate.Subject -ne $subject -or -not $certificate.HasPrivateKey -or
        $now -lt $certificate.NotBefore -or $now -ge $certificate.NotAfter -or
        '1.3.6.1.5.5.7.3.3' -notin @($certificate.EnhancedKeyUsageList | ForEach-Object { $_.ObjectId.ToString() })) {
        throw '指定の開発者署名証明書が使用できません。有効期限・秘密鍵・コード署名用途を確認してください。'
    }
}
if ($Mode -eq 'Check') {
    Write-Output "署名準備OK: $subject / $thumbprint"
    return
}
if (-not $Path -or -not (Test-Path -LiteralPath $Path -PathType Leaf)) {
    throw '署名対象のインストーラー実ファイルが必要です。'
}
$installer = (Resolve-Path -LiteralPath $Path).ProviderPath
if ([IO.Path]::GetExtension($installer) -ine '.exe') { throw '対象はEXEインストーラーに限ります。' }
if ($Mode -eq 'Sign') {
    & $signTool sign /sha1 $thumbprint /s My /fd SHA256 /tr $timestampUrl /td SHA256 /d 'TranscriptionServer' $installer
    if ($LASTEXITCODE -ne 0) { throw "インストーラー署名に失敗しました（終了コード $LASTEXITCODE）。" }
}

$signature = Get-AuthenticodeSignature -LiteralPath $installer
if ($signature.Status -ne 'Valid' -or -not $signature.SignerCertificate -or
    $signature.SignerCertificate.Thumbprint -ne $thumbprint -or
    $signature.SignerCertificate.Subject -ne $subject -or -not $signature.TimeStamperCertificate) {
    throw "署名検証失敗: $($signature.Status)。指定の開発者署名と有効なタイムスタンプが必要です。"
}
& $signTool verify /pa /all /tw /v $installer
if ($LASTEXITCODE -ne 0) { throw "SignToolの署名検証に失敗しました（終了コード $LASTEXITCODE）。" }
Write-Output "署名検証OK: $subject / SHA256 $((Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash)"
