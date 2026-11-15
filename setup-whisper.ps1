# Optional Whisper runtime: create a private Python environment inside this package.
# Does not elevate, change system PATH, install CUDA, or fetch model weights.
param([string]$PythonPath = '')
$ErrorActionPreference = 'Stop'
$PackageRoot = $PSScriptRoot
$WhisperEnvironment = Join-Path $PackageRoot 'runtime\whisper-python'
$WhisperPython = Join-Path $WhisperEnvironment 'Scripts\python.exe'
$RequirementsPath = Join-Path $PackageRoot 'requirements-whisper.txt'
$PythonCheck = 'import sys, struct; assert sys.version_info >= (3,10), "Python 3.10+ is required"; assert struct.calcsize("P") == 8, "64-bit Python is required"'

if (-not (Test-Path $RequirementsPath)) { throw 'requirements-whisper.txt was not found beside this script.' }

if (-not (Test-Path $WhisperPython)) {
    $SelectedPython = $null
    $SelectedArguments = @()
    if ($PythonPath) {
        $PythonCommand = Get-Command $PythonPath -CommandType Application -ErrorAction Stop
        & $PythonCommand.Source -c $PythonCheck
        if ($LASTEXITCODE -ne 0) { throw 'The selected Python must be 64-bit Python 3.10 or newer.' }
        $SelectedPython = $PythonCommand.Source
    } else {
        # Prefer Python 3.13, then other maintained versions with binary packages.
        $Candidates = @(
            @{ Command = 'py'; Arguments = @('-3.13') },
            @{ Command = 'py'; Arguments = @('-3.12') },
            @{ Command = 'py'; Arguments = @('-3.11') },
            @{ Command = 'py'; Arguments = @('-3.10') },
            @{ Command = 'python'; Arguments = @() }
        )
        foreach ($Candidate in $Candidates) {
            $PythonCommand = Get-Command $Candidate.Command -CommandType Application -ErrorAction SilentlyContinue
            if (-not $PythonCommand) { continue }
            $CandidateArguments = @($Candidate.Arguments)
            try { & $PythonCommand.Source @CandidateArguments -c $PythonCheck 2>$null }
            catch { continue }
            if ($LASTEXITCODE -eq 0) {
                $SelectedPython = $PythonCommand.Source
                $SelectedArguments = $CandidateArguments
                break
            }
        }
    }
    if (-not $SelectedPython) {
        throw 'Install 64-bit Python 3.10+ (recommended: Python 3.13), then run this script again. You can also pass -PythonPath C:\path\to\python.exe.'
    }
    Write-Host 'Creating runtime\whisper-python...'
    & $SelectedPython @SelectedArguments -m venv $WhisperEnvironment
    if ($LASTEXITCODE -ne 0) { throw 'The dedicated Whisper environment could not be created.' }
}

& $WhisperPython -c $PythonCheck
if ($LASTEXITCODE -ne 0) { throw 'The existing Whisper environment cannot start. Recreate runtime\whisper-python with a working 64-bit Python.' }

Write-Host 'Installing the pinned faster-whisper runtime in the dedicated environment...'
& $WhisperPython -m pip install --disable-pip-version-check --only-binary=:all: --requirement $RequirementsPath
if ($LASTEXITCODE -ne 0) { throw 'Whisper installation failed. Check the Python version and network connection, then rerun this script.' }

& $WhisperPython -c 'import faster_whisper, ctranslate2; print("faster-whisper", faster_whisper.__version__); print("CTranslate2", ctranslate2.__version__)'
if ($LASTEXITCODE -ne 0) { throw 'Whisper was installed but could not be imported. See docs\local-runtime.md for runtime prerequisites.' }

# Check real audio decoding too: import alone misses incompatible PyAV versions.
$DecodeCheck = @'
import tempfile, wave
from pathlib import Path
from faster_whisper.audio import decode_audio
with tempfile.TemporaryDirectory() as directory:
    path = Path(directory) / 'check.wav'
    with wave.open(str(path), 'wb') as output:
        output.setparams((1, 2, 16000, 0, 'NONE', 'not compressed'))
        output.writeframes(b'\0\0' * 1600)
    assert len(decode_audio(str(path))) == 1600
print('Audio decoder verified without model download.')
'@
& $WhisperPython -c $DecodeCheck
if ($LASTEXITCODE -ne 0) { throw 'The Whisper audio decoder failed. Check the pinned PyAV requirement.' }

Write-Host ''
Write-Host 'Whisper is ready for CPU execution. No model weights have been downloaded.'
Write-Host 'GPU execution additionally requires the NVIDIA libraries described in docs\local-runtime.md.'
Write-Host 'Close any running meeting app, then open launch-windows.cmd.'
Write-Host 'Choose Whisper and This PC. The selected model downloads on first use.'
