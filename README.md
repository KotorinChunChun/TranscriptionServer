# TranscriptionServer v0.1.17

動画・音声の文字起こしと話者分離を、複数のアプリから共通APIで利用するWindows向けサーバーです。RustがHTTP・ジョブ・SQLite履歴・管理画面を提供し、認識エンジンやモデルは必要時に専用領域へ導入します。

Windows 11 x64向けの展開型ZIPを配布しています。

## 入手と初回起動

1. [GitHub Releases](https://github.com/KotorinChunChun/TranscriptionServer/releases)からWindows x64のZIPを取得し、展開します。
2. 展開したフォルダーをPowerShell 7で開き、次のコマンドで起動します。

```powershell
.\TranscriptionServer.exe --local --host 127.0.0.1 --port 8769 --no-browser
```

3. [管理画面](http://127.0.0.1:8769/)を開きます。初回は、認識に必要な実行環境とモデルの準備が完了するまで待ちます。

## 開発

RustのMSVCツールチェーン、WindowsのC++ビルドツール、Node.js、Pythonを使用します。

```powershell
cargo fmt --all -- --check
cargo test --locked
cargo clippy --all-targets -- -D warnings
node --test tests/*.test.mjs
python -m unittest discover -s tests -p "test_*.py"
cargo run -- --local --host 127.0.0.1 --port 8769
```

管理画面は `http://127.0.0.1:8769/` です。文字起こしにはAPIに対応するクライアントアプリを使用します。

## APIと運用

- [API互換範囲・SSE・WebSocket](docs/api-compatibility.md)
- [OpenAPI仕様](docs/openapi.json)
- [APIの利用方法](docs/server-api.md)
- [運用とジョブ管理](docs/operations.md)
- [実行環境とモデル](docs/runtime-management.md)
- [LANとBearer認証](docs/server-runtime.md)
- [HTTPSの設定](docs/https-and-client-certificates.md)

既定の同PC限定設定から開始してください。LANでは許可するサブネット、接続元、ファイアウォール、証明書を設定します。LANモードの履歴は接続利用者で共有されます。インターネットへの設置は認証・TLS・接続制限を確認してから行ってください。

音声と字幕はローカルに保存されます。外部APIは利用者が選択・設定した場合に使用します。APIキーやGitHubトークンは環境変数またはWindows資格情報に設定してください。

## Windows用の配布物

`scripts/build-release.py --portable-release` はWindows x64の展開型ZIPとSHA-256一覧を作成します。実行環境とモデルは起動後に準備します。Inno Setupインストーラーを作る場合は[署名設定](docs/installer-signing.md)が必要です。

確認内容と未検証事項は[開発記録](dev/v0.1.17-imp.md)を参照してください。
