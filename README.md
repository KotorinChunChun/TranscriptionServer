# TranscriptionServer v0.1.20

動画・音声の文字起こしと話者分離を、複数のアプリから共通APIで利用するWindows向けサーバーです。RustがHTTP・ジョブ・SQLite履歴・管理画面を提供し、認識エンジンやモデルは必要時に専用領域へ導入します。

Windows 11 x64向けのインストーラーと展開型ZIPを配布しています。

## 入手と初回起動

1. [GitHub Releases](https://github.com/KotorinChunChun/TranscriptionServer/releases)から `setup-x64.exe` を取得し、実行します。発行元は開発者の `KotorinChunChun` です。[署名の確認方法](docs/installer-signing.md)
2. このPCだけで使う場合は、導入先をPowerShell 7で開き、次のコマンドで起動します。展開型ZIPを使う場合は、展開先で同じコマンドを実行します。

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

音声と字幕はローカルに保存されます。外部の認識APIは利用者が選択・設定した場合に使用します。「設定・更新」から公開リリースを確認できます。

## Windows用の配布物

`scripts/build-release.py --portable-release` はWindows x64の展開型ZIPとSHA-256一覧を作成します。実行環境とモデルは起動後に準備します。Inno Setupインストーラーを作る場合は[署名設定](docs/installer-signing.md)が必要です。

サーバーPCの管理画面で「処理履歴」を開くと、保存済みの入力音声、認識結果と使用モデルを確認できます。[処理履歴と運用](docs/operations.md)
