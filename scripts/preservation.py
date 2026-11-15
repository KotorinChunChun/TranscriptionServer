"""Read-only deployment/cleanup guards. Never restore or delete protected files."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import sys


class PreservationError(RuntimeError):
    pass


def absolute(value):
    path = Path(value)
    if not path.is_absolute():
        raise PreservationError(f"絶対パスが必要です: {path}")
    return path


def file_hash(value):
    path = absolute(value)
    if not path.is_file():
        raise PreservationError(f"必須ファイルがありません: {path}")
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def object_json(path):
    try:
        value = json.loads(absolute(path).read_text(encoding="utf-8-sig"))
    except (OSError, ValueError) as error:
        raise PreservationError(f"必須JSONを読み取れません: {path}") from error
    if not isinstance(value, dict) or not value:
        raise PreservationError(f"必須JSONが空またはオブジェクトではありません: {path}")
    return value


def requirements(program, settings, data):
    program, settings, data = map(absolute, (program, settings, data))
    return {
        "settings": str(settings),
        "database": str(data / "workbench.sqlite3"),
        "executable": str(program / "TranscriptionServer.exe"),
        "build": str(program / "BUILD.json"),
        "installation": str(program / "INSTALLATION.json"),
    }


def check_required(required):
    roles = {"settings", "database", "executable", "build", "installation"}
    if not isinstance(required, dict) or set(required) != roles:
        raise PreservationError("設定/DB/EXE/BUILD/INSTALLATIONの必須指定が不完全です")
    hashes = {str(absolute(path)): file_hash(path) for path in required.values()}
    if len(hashes) != len(roles):
        raise PreservationError("必須ファイルの指定が重複しています")
    for role in ("settings", "build", "installation"):
        object_json(required[role])
    return hashes


def tree_files(root):
    root = absolute(root)
    if not root.is_dir():
        raise PreservationError(f"必須フォルダーがありません: {root}")

    def failed(error):
        raise error

    files = []
    for directory, dirs, names in os.walk(root, onerror=failed):
        # Do not silently omit linked subdirectories from a preservation inventory.
        for name in dirs:
            path = Path(directory) / name
            if path.is_symlink() or path.is_junction():
                raise PreservationError(f"保持対象のリンク先を明示してください: {path}")
        files.extend(Path(directory) / name for name in names if name != ".server.lock")
    if not files:
        raise PreservationError(f"保持対象フォルダーが空です: {root}")
    return files


def capture(required, mode, keep_files=(), keep_trees=()):
    if mode not in ("update", "cleanup"):
        raise PreservationError("用途はupdateまたはcleanupを指定してください")
    hashes = check_required(required)
    unchanged = dict(hashes) if mode == "cleanup" else {
        required[role]: hashes[required[role]] for role in ("settings", "database")
    }
    trees = [str(absolute(root)) for root in keep_trees]
    for path in [*keep_files, *(p for root in trees for p in tree_files(root))]:
        unchanged[str(absolute(path))] = file_hash(path)
    return {"schema": 1, "mode": mode, "required": required,
            "trees": trees, "files": unchanged, "beforeRequiredHashes": hashes}


def validate_manifest(manifest):
    if not isinstance(manifest, dict) or manifest.get("schema") != 1:
        raise PreservationError("必須ファイルを記録していない旧形式の検証計画は使用できません")
    mode = manifest.get("mode")
    if mode not in ("update", "cleanup"):
        raise PreservationError("検証用途がありません")
    required = manifest.get("required")
    check_required(required)
    files = manifest.get("files")
    before = manifest.get("beforeRequiredHashes")
    if not isinstance(files, dict) or not files or not isinstance(before, dict):
        raise PreservationError("保持基準が空または不完全です")
    if set(before) != set(required.values()):
        raise PreservationError("更新前の必須ファイルの記録が不完全です")
    mandatory = required.values() if mode == "cleanup" else [required["settings"], required["database"]]
    if not set(mandatory).issubset(files):
        raise PreservationError("設定/DBなど必須の保持基準が欠落しています")
    for path, digest in [*files.items(), *before.items()]:
        absolute(path)
        if not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest):
            raise PreservationError(f"保持基準のSHA256が不正です: {path}")
    if not isinstance(manifest.get("trees"), list):
        raise PreservationError("保持フォルダーの記録がありません")
    return files


def verify(manifest, before_operation=False):
    files = validate_manifest(manifest)
    expected = dict(files)
    if before_operation:
        expected.update(manifest["beforeRequiredHashes"])
    for path, digest in expected.items():
        if file_hash(path) != digest:
            raise PreservationError(f"保持対象の内容が変化しました: {path}")
    for root in manifest["trees"]:
        tree_files(root)
    return len(expected)


def check_targets(manifest, targets):
    verify(manifest, before_operation=True)
    if manifest["mode"] != "cleanup":
        raise PreservationError("削除候補の検査にはcleanupの基準が必要です")
    protected = [*manifest["files"], *manifest["required"].values(), *manifest["trees"]]
    # Check both lexical paths and resolved aliases (junctions/symlinks).
    for value in targets:
        target = absolute(value)
        for name in protected:
            keep = absolute(name)
            for t, k in ((target, keep), (target.resolve(), keep.resolve())):
                if t == k or k.is_relative_to(t) or t.is_relative_to(k):
                    raise PreservationError(f"削除候補が保持対象と重なっています: {target}")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    save = commands.add_parser("capture")
    for name in ("program-dir", "settings-file", "data-dir", "output"):
        save.add_argument("--" + name, required=True)
    save.add_argument("--mode", choices=("update", "cleanup"), required=True)
    save.add_argument("--keep-file", action="append", default=[])
    save.add_argument("--keep-tree", action="append", default=[])
    check = commands.add_parser("verify")
    check.add_argument("--manifest", required=True)
    check.add_argument("--before-operation", action="store_true")
    targets = commands.add_parser("check-targets")
    targets.add_argument("--manifest", required=True)
    targets.add_argument("--targets", required=True, help="削除候補の絶対パス配列JSON")
    args = parser.parse_args(argv)
    try:
        if args.command == "capture":
            required = requirements(args.program_dir, args.settings_file, args.data_dir)
            manifest = capture(required, args.mode, args.keep_file, args.keep_tree)
            # Missing inputs must never create or overwrite a successful baseline.
            with absolute(args.output).open("x", encoding="utf-8") as stream:
                json.dump(manifest, stream, ensure_ascii=False, indent=2)
        else:
            manifest = object_json(args.manifest)
            if args.command == "verify":
                verify(manifest, args.before_operation)
            else:
                values = json.loads(absolute(args.targets).read_text(encoding="utf-8-sig"))
                if not isinstance(values, list) or not values or not all(isinstance(v, str) for v in values):
                    raise PreservationError("削除候補は空でない絶対パス配列を指定してください")
                check_targets(manifest, values)
    except (OSError, ValueError, PreservationError) as error:
        print(f"保持検証失敗: {error}", file=sys.stderr)
        return 1
    print("必須ファイル・保持対象の検証が完了しました")
    return 0


if __name__ == "__main__":
    sys.exit(main())
