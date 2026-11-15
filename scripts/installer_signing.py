"""Mandatory installer signature gate shared by build and publication."""
from pathlib import Path
import subprocess

SCRIPT = Path(__file__).resolve().with_name('sign-installer.ps1')


def check_installer_signing():
    _run('Check')


def sign_installer(path):
    # Sign mode also verifies signer identity, trust and timestamp. A nonzero
    # result stops the build before archives/checksums are created.
    _run('Sign', path)


def verify_installer(path):
    # Verification needs public trust only, not access to the signing key.
    _run('Verify', path)


def _run(mode, path=None):
    command = ['pwsh', '-NoProfile', '-NonInteractive', '-File', str(SCRIPT), '-Mode', mode]
    if path is not None:
        command += ['-Path', str(Path(path).resolve(strict=True))]
    subprocess.run(command, check=True, timeout=180)
