# Rust版の導入・開発

通常はv0.1.0のWindowsインストーラーを使用する。CLI・HTTP/API・ジョブ/SQLite管理・更新ヘルパーはRust EXEであり、PythonはWhisper専用ワーカーだけに同梱される。Pythonをシステムへ導入する必要はない。

## 実行に必要なもの

- Windows x64。Rust本体はCRTを静的リンクし、Python/VC Runtimeの別途導入に依存させない。
- FFmpeg/FFprobeをPATH、bin、runtime/ffmpeg/binまたは明示的な--ffmpeg/--ffprobeで指定する。
- Whisperモデルは初回取得。ワーカーがオフライン点検した重みの状態を起動時に表示する。
- Haas/Qwen3-ASR/ReazonSpeech/ParakeetとNemotron話者分離には別途speech.cppが必要。setup-windows.ps1は既存のSHA256固定の導入を補助し、モデルの事前取得は-PullModelsを明示した場合だけ行う。
- GPUは対応するCUDAまたはspeech.cppランタイムが必要。候補検出と実推論/空きVRAM/精度の受入を区別する。

## 起動

```pwsh
./TranscriptionServer.exe --lan --host 0.0.0.0 --port 8769 --no-browser
```

LANはキーの手入力不要。起動時に表示する実IPのURLを他PCへ指定する。--localは同PC限定、--serverはBearer認証の構成。設定と録音はプログラムの外へ置く。既存--data-dirを維持し、新規の既定先は%LOCALAPPDATA%/TranscriptionServer/data。旧DBが旧保存先にしかない場合は引き継ぐ。

## ソースからの開発

RustツールチェーンとWindows C++ビルド環境を用意する。cargo test、cargo clippy --all-targets -- -D warnings、node --test tests/*.test.mjsで検証する。

Whisperの開発だけにPython 3.12の専用環境とrequirements-whisper.txtを使う。debug版はリポジトリの専用環境を検出できる。release版でソースワーカーを使う場合は--whisper-workerと--whisper-pythonを明示する。PythonのHTTP/DB/更新サーバーはない。

ビルドは専用Pythonへrequirements-build.txtを導入し、python scripts/build-release.py --iscc <ISCC.exe>で行う。Rust本体、PyInstallerによるWhisper専用ワーカー、Inno Setup、ソースZIP/ハッシュを生成する。ソースをコミットしてから実行する。

scripts/check-rust-runtime.pyは専用フォルダーと明示した合成音声を使う開発受入で、Rustの認識・再利用・中止・編集・Range・multipartを実HTTPで確認する。これは製品のPython依存ではなく検証ツールである。旧Python版の実装/試験はGit履歴に保存されている。

## 一次資料

- [speech.cpp](https://github.com/nyosegawa/speech.cpp)
- [faster-whisper](https://github.com/SYSTRAN/faster-whisper)
- [Axum](https://docs.rs/axum/0.8.9/axum/)
- [Tokio](https://docs.rs/tokio/1.54.1/tokio/)
