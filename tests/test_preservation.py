import contextlib
import copy
import importlib.util
import io
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import preservation as guard


class PreservationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.program = self.root / "program"
        self.data = self.root / "data"
        self.program.mkdir()
        self.data.mkdir()
        self.settings = self.root / "settings.json"
        self.required = guard.requirements(self.program, self.settings, self.data)
        for role, name in self.required.items():
            Path(name).write_bytes(b'{"fixture":true}' if role in ("settings", "build", "installation") else b"fixture")

    def capture(self, mode="update", **kwargs):
        return guard.capture(self.required, mode, **kwargs)

    def test_unchanged_passes_both_modes(self):
        for mode in ("update", "cleanup"):
            manifest = self.capture(mode)
            guard.verify(manifest, before_operation=True)
            guard.verify(manifest)

    def test_every_required_file_is_mandatory_before_capture(self):
        for name in self.required.values():
            path = Path(name)
            content = path.read_bytes()
            path.unlink()
            with self.subTest(path=path), self.assertRaises(guard.PreservationError):
                self.capture()
            path.write_bytes(content)

    def test_every_required_file_is_mandatory_after_operation(self):
        manifest = self.capture()
        for name in self.required.values():
            path = Path(name)
            content = path.read_bytes()
            path.unlink()
            with self.subTest(path=path), self.assertRaises(guard.PreservationError):
                guard.verify(manifest)
            path.write_bytes(content)

    def test_corrupt_and_empty_settings_fail(self):
        for value in ("", "{", "{}", "null", "[]"):
            self.settings.write_text(value)
            with self.subTest(value=value), self.assertRaises(guard.PreservationError):
                self.capture()

    def test_unreadable_file_fails(self):
        with patch.object(Path, "open", side_effect=PermissionError("fixture")):
            with self.assertRaises(PermissionError):
                self.capture()

    def test_missing_and_empty_retained_model_directory_fail(self):
        directory = self.root / "retained-models"
        with self.assertRaises(guard.PreservationError):
            self.capture(keep_trees=[directory])
        directory.mkdir()
        with self.assertRaises(guard.PreservationError):
            self.capture(keep_trees=[directory])

    def test_retained_model_removed_after_capture_fails(self):
        model = self.root / "retained-model.bin"
        model.write_bytes(b"model")
        manifest = self.capture(keep_files=[model])
        model.unlink()
        with self.assertRaises(guard.PreservationError):
            guard.verify(manifest)

    def test_changed_settings_or_database_fail(self):
        for role in ("settings", "database"):
            manifest = self.capture()
            Path(self.required[role]).write_bytes(b'{"changed":true}')
            with self.subTest(role=role), self.assertRaises(guard.PreservationError):
                guard.verify(manifest)

    def test_program_change_allowed_only_after_update(self):
        update, cleanup = self.capture(), self.capture("cleanup")
        Path(self.required["executable"]).write_bytes(b"new program")
        guard.verify(update)
        with self.assertRaises(guard.PreservationError):
            guard.verify(update, before_operation=True)
        with self.assertRaises(guard.PreservationError):
            guard.verify(cleanup)

    def test_incomplete_or_old_manifest_rejected(self):
        manifest = self.capture()
        invalid = [{}, {"protected": {}, "normalSettingsExisted": False}]
        for field in ("required", "files", "beforeRequiredHashes"):
            modified = copy.deepcopy(manifest)
            modified[field].pop(str(self.settings) if field != "required" else "settings")
            invalid.append(modified)
        for value in invalid:
            with self.subTest(value=value), self.assertRaises(guard.PreservationError):
                guard.verify(value)

    def test_cleanup_cannot_target_required_file_or_parent(self):
        manifest = self.capture("cleanup")
        for target in (self.settings, self.root, self.program):
            with self.subTest(target=target), self.assertRaises(guard.PreservationError):
                guard.check_targets(manifest, [target])
        guard.check_targets(manifest, [self.root / "disposable"])

    def test_cleanup_rechecks_content_before_targets(self):
        manifest = self.capture("cleanup")
        self.settings.unlink()
        with self.assertRaises(guard.PreservationError):
            guard.check_targets(manifest, [self.root / "disposable"])

    def test_cli_missing_settings_never_writes_baseline(self):
        self.settings.unlink()
        output = self.root / "manifest.json"
        with contextlib.redirect_stderr(io.StringIO()):
            result = guard.main(["capture", "--program-dir", str(self.program), "--settings-file", str(self.settings), "--data-dir", str(self.data), "--mode", "update", "--output", str(output)])
        self.assertEqual(result, 1)
        self.assertFalse(output.exists())

    def test_cli_does_not_overwrite_existing_evidence(self):
        output = self.root / "manifest.json"
        output.write_text("historical evidence")
        with contextlib.redirect_stderr(io.StringIO()):
            result = guard.main(["capture", "--program-dir", str(self.program), "--settings-file", str(self.settings), "--data-dir", str(self.data), "--mode", "cleanup", "--output", str(output)])
        self.assertEqual(result, 1)
        self.assertEqual(output.read_text(), "historical evidence")

    def test_local_acceptance_aborts_before_installer_or_success_record(self):
        harness_path = Path(__file__).resolve().parents[2] / "runtime/transcription-0.1.12-acceptance/check-installers.py"
        if sys.platform != "win32" or not harness_path.is_file():
            self.skipTest("このPCの過去の受入スクリプトがある環境のみ")
        spec = importlib.util.spec_from_file_location("acceptance_guard_test", harness_path)
        harness = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(harness)
        with patch.object(harness, "requirements", return_value=self.required), \
             patch.object(harness, "registry", side_effect=lambda name: None if name == harness.CANDIDATE else {"fixture": True}), \
             patch.object(harness, "install") as installer, \
             patch.object(harness, "save") as save:
            self.assertEqual(len(harness.official()["files"]), 5)
            self.settings.unlink()
            with self.assertRaises(guard.PreservationError):
                harness.main()
            installer.assert_not_called()
            save.assert_not_called()
        self.settings.write_text('{"fixture":true}')
        with patch.object(harness, "requirements", return_value=self.required), \
             patch.object(harness, "registry", return_value=None), \
             patch.object(harness, "install") as installer, \
             patch.object(harness, "save") as save:
            with self.assertRaises(guard.PreservationError):
                harness.main()
            installer.assert_not_called()
            save.assert_not_called()

    def test_local_cleanup_scripts_reject_old_plan_without_writes(self):
        folder = Path(__file__).resolve().parents[2] / "runtime/transcription-v0.1.12-post-release-cleanup-20261114"
        if not folder.is_dir():
            self.skipTest("このPCの過去の整理スクリプトがある環境のみ")
        import runpy
        for filename in ("make-plan.py", "verify.py"):
            with self.subTest(filename=filename), patch.object(Path, "write_text") as write:
                with self.assertRaises(RuntimeError):
                    runpy.run_path(str(folder / filename), run_name="__main__")
                write.assert_not_called()


if __name__ == "__main__":
    unittest.main()
