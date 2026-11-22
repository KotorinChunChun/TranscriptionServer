#Requires -Version 7
param([Parameter(Mandatory)][string]$WorkDirectory)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
if (![IO.Path]::IsPathFullyQualified($WorkDirectory) -or (Test-Path -LiteralPath $WorkDirectory)) {
    throw '未作成の絶対パスを試験用の保存先として指定してください。'
}
New-Item -ItemType Directory -Path $WorkDirectory | Out-Null
$legacy = Join-Path $WorkDirectory 'legacy-tool'
New-Item -ItemType Directory -Path (Join-Path $legacy 'src') | Out-Null
$manifest = @'
[package]
name = "transcription-sqlite-legacy-fixture"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
rusqlite = { version = "=0.37.0", features = ["bundled", "backup"] }
serde_json = "1"
'@
[IO.File]::WriteAllText((Join-Path $legacy 'Cargo.toml'), $manifest)
Copy-Item -LiteralPath (Join-Path $root 'tests/fixtures/sqlite-legacy.rs') -Destination (Join-Path $legacy 'src/main.rs')
$previousTarget = $env:CARGO_TARGET_DIR
$previousFixture = $env:TRANSCRIPTION_SQLITE_FIXTURE
$process = $null
$passed = $false
try {
    $env:CARGO_TARGET_DIR = Join-Path $legacy 'target'
    & cargo build --offline --manifest-path (Join-Path $legacy 'Cargo.toml') -j 3
    if ($LASTEXITCODE) { throw '旧SQLiteの試験専用ツールをビルドできません。' }
    $executable = Join-Path $env:CARGO_TARGET_DIR 'debug/transcription-sqlite-legacy-fixture.exe'
    $fixture = Join-Path $WorkDirectory 'synthetic-database'
    $info = [Diagnostics.ProcessStartInfo]::new($executable)
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.ArgumentList.Add($fixture)
    $process = [Diagnostics.Process]::Start($info)
    $deadline = (Get-Date).AddSeconds(20)
    while (!(Test-Path -LiteralPath (Join-Path $fixture 'fixture-only.json'))) {
        if ($process.HasExited -or (Get-Date) -gt $deadline) { throw '旧SQLiteの合成DBを作成できません。' }
        Start-Sleep -Milliseconds 100
    }
    $env:CARGO_TARGET_DIR = $previousTarget
    $env:TRANSCRIPTION_SQLITE_FIXTURE = $fixture
    Push-Location $root
    try {
        & cargo test --locked --offline legacy_wal_store_backup_restore_roundtrip -- --ignored --nocapture
        if ($LASTEXITCODE) { throw '旧DBとWALの互換性試験に失敗しました。' }
    } finally {
        Pop-Location
    }
    [IO.File]::WriteAllText((Join-Path $fixture 'release-old-connection'), '')
    if (!$process.WaitForExit(15000) -or $process.ExitCode -ne 0) { throw '旧SQLiteで新版の保存結果を再読込できません。' }
    $passed = $true
    $process.StandardOutput.ReadToEnd()
    Get-Content -LiteralPath (Join-Path $fixture 'old-reopened.json')
} finally {
    $env:CARGO_TARGET_DIR = $previousTarget
    $env:TRANSCRIPTION_SQLITE_FIXTURE = $previousFixture
    if ($process -and !$process.HasExited) {
        [IO.File]::WriteAllText((Join-Path $fixture 'release-old-connection'), '')
        if (!$process.WaitForExit(10000)) { $process.Kill(); $process.WaitForExit() }
    }
    if ($process -and !$passed) {
        $process.StandardError.ReadToEnd()
        $process.StandardOutput.ReadToEnd()
    }
}
