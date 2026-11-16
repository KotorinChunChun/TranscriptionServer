"""Release gates must fail before packaging/publication, not merely log errors."""
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from zipfile import ZipFile

SCRIPTS = Path(__file__).resolve().parents[1] / 'scripts'
sys.path.insert(0, str(SCRIPTS))
import installer_signing


def load(name):
    spec = importlib.util.spec_from_file_location(name.replace('-', '_'), SCRIPTS / (name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class InstallerSigningTests(unittest.TestCase):
    def test_wrapper_propagates_verification_failure(self):
        with tempfile.TemporaryDirectory() as tmp:
            setup = Path(tmp) / 'setup.exe'
            setup.write_bytes(b'unsigned')
            with patch.object(installer_signing.subprocess, 'run', side_effect=subprocess.CalledProcessError(1, 'pwsh')) as run:
                with self.assertRaises(subprocess.CalledProcessError):
                    installer_signing.verify_installer(setup)
                args, options = run.call_args
                self.assertEqual(args[0][-4:], ['-Mode', 'Verify', '-Path', str(setup.resolve())])
                self.assertTrue(options['check'])

    def run_build(self, mode):
        module = load('build-release')
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for folder in ('dist/downloads', 'manifests', 'scripts', 'docs', 'installer', 'target/x86_64-pc-windows-msvc/release'):
                (root / folder).mkdir(parents=True, exist_ok=True)
            (root / 'package.json').write_text('{"version":"0.1.14"}')
            (root / 'manifests/management-assets.json').write_text('[]')
            for file in ('README.md', 'launch-windows.cmd', 'scripts/start-server.ps1', 'scripts/Configure-LanFirewall.ps1', 'scripts/preservation.py', 'docs/openapi.json'):
                (root / file).write_text('fixture')
            (root / 'target/x86_64-pc-windows-msvc/release/TranscriptionServer.exe').write_bytes(b'core')
            out = root / 'release/v0.1.14'
            setup = out / 'TranscriptionServer-v0.1.14-setup-x64.exe'
            events = []

            def preflight():
                events.append('preflight')
                if mode == 'preflight-failure': raise RuntimeError('certificate absent')

            def command(args, **kwargs):
                if args[:2] == ['cargo', 'metadata']:
                    return SimpleNamespace(stdout='{"packages":[]}')
                if args[0] == 'cargo': events.append('compile')
                elif args[0].endswith('ISCC.exe'):
                    events.append('iscc'); setup.write_bytes(b'unsigned')
                else:
                    events.append('source'); (root / 'dist/downloads/transcription-server.zip').write_bytes(b'source')
                return SimpleNamespace(returncode=0)

            def sign(path):
                events.append('sign')
                self.assertEqual(path.read_bytes(), b'unsigned')
                if mode == 'signature-failure': raise RuntimeError('timestamp failed')
                path.write_bytes(b'signed-and-verified')

            options = ['--portable-release'] if mode == 'portable' else ['--iscc', str(root / 'ISCC.exe')]
            with patch.object(module, 'ROOT', root), patch.object(module, 'git', side_effect=lambda *args: '' if args[0] == 'status' else 'fixture-commit'), \
                 patch.object(module, 'check_installer_signing', side_effect=preflight), patch.object(module, 'sign_installer', side_effect=sign), \
                 patch.object(module.subprocess, 'run', side_effect=command), patch.dict(sys.modules, {'pefile':SimpleNamespace(PE=lambda _:SimpleNamespace(DIRECTORY_ENTRY_IMPORT=[]))}), \
                 patch.object(sys, 'argv', ['build-release.py', *options]):
                if mode in ('success', 'portable'): module.main()
                else:
                    with self.assertRaises(RuntimeError): module.main()
            if mode == 'preflight-failure':
                self.assertEqual(events, ['preflight'])
                self.assertFalse(out.exists())
            elif mode == 'signature-failure':
                self.assertEqual(events, ['preflight', 'compile', 'iscc', 'sign'])
                self.assertFalse((out / 'SHA256SUMS.txt').exists())
                self.assertFalse(list(out.glob('*.zip')))
            elif mode == 'portable':
                self.assertEqual(events, ['compile'])
                archive = out / 'TranscriptionServer-v0.1.14-windows-x64.zip'
                self.assertEqual(set(f.name for f in out.iterdir()), {'TranscriptionServer', archive.name, 'SHA256SUMS.txt'})
                with ZipFile(archive) as payload:
                    build = json.loads(payload.read('TranscriptionServer/BUILD.json'))
                    self.assertEqual(build['releaseStatus'], 'release')
                    self.assertEqual(build['gitCommit'], 'fixture-commit')
                self.assertEqual((out / 'SHA256SUMS.txt').read_text().strip(),
                                 hashlib.sha256(archive.read_bytes()).hexdigest() + '  ' + archive.name)
            else:
                self.assertEqual(events, ['preflight', 'compile', 'iscc', 'sign', 'source'])
                self.assertIn(hashlib.sha256(b'signed-and-verified').hexdigest() + '  ' + setup.name, (out / 'SHA256SUMS.txt').read_text())

    def test_missing_certificate_stops_before_build(self): self.run_build('preflight-failure')
    def test_signing_failure_stops_before_archives_and_checksums(self): self.run_build('signature-failure')
    def test_checksums_use_verified_signed_installer(self): self.run_build('success')
    def test_portable_release_ships_only_checked_zip(self): self.run_build('portable')

    def test_unsigned_installer_cannot_reach_credentials_or_network(self):
        module = load('publish-release')
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); out = root / 'release/v0.1.14'; out.mkdir(parents=True)
            (root / 'package.json').write_text('{"version":"0.1.14"}')
            for name in ('windows-x64.zip', 'source.zip', 'setup-x64.exe'):
                (out / ('TranscriptionServer-v0.1.14-' + name)).write_bytes(b'fixture')
            (out / 'SHA256SUMS.txt').write_text('')
            for flags in ([], ['--publish'], ['--publish', '--draft-only']):
                with self.subTest(flags=flags), patch.object(module, 'ROOT', root), \
                     patch.object(module, 'verify_installer', side_effect=RuntimeError('invalid signature')), \
                     patch.object(module.subprocess, 'run') as process, patch.object(module, 'urlopen') as network, \
                     patch.object(sys, 'argv', ['publish-release.py', '--repository', 'KotorinChunChun/TranscriptionServer', '--notes', str(root/'notes.md'), *flags]):
                    with self.assertRaises(RuntimeError): module.main()
                    process.assert_not_called(); network.assert_not_called()


if __name__ == '__main__': unittest.main()
