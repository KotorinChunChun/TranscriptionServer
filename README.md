# TranscriptionServer v0.1.17

動画・音声の文字起こしと話者分離を、複数のアプリから共通APIで利用するWindows向けサーバーです。RustがHTTP・ジョブ・SQLite履歴・管理画面を提供し、認識エンジンやモデルは必要時に専用領域へ導入します。

このリポジトリは公開用のソースです。実行ファイル・インストーラーは別途ビルドします。

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

`scripts/build-release.py` はWindows用プログラムとInno Setupのインストーラーを作成します。[署名設定](docs/installer-signing.md)が必要です。`scripts/publish-release.py` は非公開リポジトリ向けの配布補助であり、この公開リポジトリへのソース送信には使用しません。

確認内容と未検証事項は[開発記録](dev/v0.1.17-imp.md)を参照してください。
