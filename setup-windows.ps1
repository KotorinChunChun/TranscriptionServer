# Downloads only the pinned official speech.cpp release into this package.
# Does not elevate, change PATH, install system packages, or download model weights.
param([switch]$PullModels)
$ErrorActionPreference = 'Stop'
$PackageRoot = $PSScriptRoot
$Version = '0.10.1'
$ArchiveName = "speech-$Version-windows-x64-vulkan.zip"
$DownloadUrl = "https://github.com/nyosegawa/speech.cpp/releases/download/v$Version/$ArchiveName"
$ExpectedSha256 = 'a8fe0e4b4f463c460e7ace9d38c363bec08b254cac5a7f764b7967af35d4fa1a'
$RuntimeDirectory = Join-Path $PackageRoot 'runtime\speech'
$DownloadDirectory = Join-Path $PackageRoot 'runtime\downloads'
New-Item -ItemType Directory -Force $DownloadDirectory | Out-Null
$ArchivePath = Join-Path $DownloadDirectory $ArchiveName
$StageDirectory = Join-Path $DownloadDirectory 'speech-extracted'

if (-not (Test-Path (Join-Path $RuntimeDirectory 'speech.exe'))) {
    Write-Host "Downloading official speech.cpp $Version (about 30 MB)..."
    Invoke-WebRequest -Uri $DownloadUrl -OutFile $ArchivePath -UseBasicParsing
    $ActualSha256 = (Get-FileHash -Path $ArchivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($ActualSha256 -ne $ExpectedSha256) {
        throw 'SHA-256 verification failed. The downloaded archive was not executed.'
    }
    Expand-Archive -Path $ArchivePath -DestinationPath $StageDirectory -Force
    $SpeechExecutable = Get-ChildItem -Path $StageDirectory -Filter 'speech.exe' -Recurse | Select-Object -First 1
    if (-not $SpeechExecutable) { throw 'The official archive did not contain speech.exe.' }
    New-Item -ItemType Directory -Force $RuntimeDirectory | Out-Null
    Copy-Item -Path (Join-Path $SpeechExecutable.Directory.FullName '*') -Destination $RuntimeDirectory -Recurse -Force
    Write-Host 'speech.cpp installed in runtime\speech. SHA-256 verified.'
} else {
    Write-Host 'Using the existing runtime\speech installation.'
}

& (Join-Path $RuntimeDirectory 'speech.exe') --version
if ($LASTEXITCODE -ne 0) {
    throw 'speech.exe could not start. Check the official Windows runtime prerequisites in docs\local-runtime.md.'
}

Write-Host 'HTTP/API and job management run in the Rust TranscriptionServer executable.'
Write-Host 'Python is bundled only with the Whisper recognition worker in the installer.'

$FFmpegFound = (Get-Command ffmpeg -ErrorAction SilentlyContinue) -or
    (Test-Path (Join-Path $PackageRoot 'runtime\ffmpeg\bin\ffmpeg.exe')) -or
    (Test-Path (Join-Path $PackageRoot 'runtime\ffmpeg\ffmpeg.exe'))
$FFprobeFound = (Get-Command ffprobe -ErrorAction SilentlyContinue) -or
    (Test-Path (Join-Path $PackageRoot 'runtime\ffmpeg\bin\ffprobe.exe')) -or
    (Test-Path (Join-Path $PackageRoot 'runtime\ffmpeg\ffprobe.exe'))
if (-not $FFmpegFound -or -not $FFprobeFound) {
    Write-Host 'FFmpeg and FFprobe are required. Install them yourself with:'
    Write-Host '  winget install --id Gyan.FFmpeg --exact'
    Write-Host 'Or place ffmpeg.exe and ffprobe.exe in runtime\ffmpeg\bin.'
}

if ($PullModels) {
    Write-Host 'Fetching Haas, Silero VAD and Nemotron model weights (about 764 MB total)...'
    foreach ($ModelName in @('haas-ja-v1.0', 'silero-vad', 'nemotron-3-diarization')) {
        & (Join-Path $RuntimeDirectory 'speech.exe') pull $ModelName
        if ($LASTEXITCODE -ne 0) { throw "Model download failed: $ModelName" }
    }
}
Write-Host ''
Write-Host 'After prerequisites are installed, close this console and run launch-windows.cmd.'
Write-Host 'Model weights are fetched when transcription first runs, unless -PullModels was selected.'
