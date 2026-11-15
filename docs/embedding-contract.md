# アプリ組込み用サーバー契約（v0.1.10）

v0.1.10はprotocolVersion=3の認識契約を維持し、サーバー管理専用UI、統一したモデル操作、参考速度とモデル情報APIを追加した配布版です。旧v0.1.7（695137d）や開発候補9cd17cbcとは別の成果物であり、BUILD.gitCommit/releaseStatusとSHA256で識別します。公開済み資産・タグは上書きしません。

## 同梱範囲と起動

program-only ZIPはRust EXE/Web資産/資料/依存ライセンスのみ。認識ランタイム・モデル・FFmpeg/FFprobe/Python・資格情報・設定・録音は同梱しない。Whisper用Pythonだけを管理UIから専用領域へオンライン導入する。他のエンジンはC++を使う。

```
TranscriptionServer.exe --instance-id transcript-cutter --local --host 127.0.0.1 --port <専用ポート> --data-dir <絶対パス> --settings-dir <絶対パス> --process-record <絶対パス> --no-browser
```

8769は共有用なので専用ポートに使わない。`--installed`は指定しない。プロセス記録/healthのinstanceId・PID・開始時刻を照合し、所有プロセスだけを管理する。モデルキャッシュも専用settings配下に分離する。親アプリの終了だけで無関係な共有サーバーを停止しない。

v0.1.12は起動時に不足実行環境を自動準備します（モデル本体は対象外）。準備中も管理HTTPとprocess-recordを提供し、health.startupState=preparing/acceptingJobs=falseで新規認識を止めます。管理HTTPの起動成功と認識準備完了を区別してください。開発/合成/オフライン検証や親アプリ側で準備を管理する場合は`--no-auto-runtimes`を指定します。正常な既存環境は再取得せず、初回準備と管理画面での再導入後の手動再起動は不要です。v0.1.11以前へこの動作があるとは扱いません。詳しくは[実行環境管理](runtime-management.md)を参照してください。

## 接続・モデル選択

v0.1.14の並列処理は [並列実行](concurrent-execution.md) を参照してください。v0.1.13以前には含まれません。既存の認識APIを維持し、能力フラグ・資源待ちphase・設定/状態APIを追加しています。

v0.1.10のモデル情報拡張は [model-information.md](model-information.md) を参照する。healthで対応を確認した場合だけ `/api/models` と `x_meeting.modelInfo` を使用する。公開済みv0.1.9や稼働v0.1.8に実装済みとは扱わない。

- protocolVersion=3。`GET /api/health`のauthModeがlocal/lanなら返された一時tokenをメモリ内でX-Local-Tokenへ使い、キー手入力を必須にしない。Bearer構成だけ接続キーを使う。認証値をログ/台帳/BUILDへ記録しない。
- `/v1/models`とhealth.enginesの実行可能な選択肢を使う。CPU/GPU/autoはprocessingMethodsから取得し、GPU候補を実推論の成功と扱わない。管理一覧には不足/破損/未対応/対象外も表示するので、管理一覧全件を実行候補にしない。
- 話者分離専用: engine=`diarization`、model=`nemotron-3-diarization`、diarize=true。本文は空、話者と時刻を返す。
- ASRとの組合せ: 対応ASRを選びdiarize=true。補助のNemotron 3 Diarization重みが必要。Whisper/各C++ ASRと一体型30Bを混同しない。
- 一体型: engine=`nemotron-transcribe`、model=`nemotron-3.5-transcribe`。既定enable_integrated_model=falseで拒否。明示有効化（--enable-integrated-modelまたは導入版設定）した環境だけで選択可能とする。

## ジョブ・媒体

`POST /api/jobs?engine=...&model=...&clientId=transcript-cutter&workload=file&device=auto&diarize=...`でバイナリを送り、返されたjob.idを使う。範囲はstart/end秒。認識結果と字幕・時間・測定の既存形式を維持する。

sourceSha256/sourceBytesは**アップロードしたバイナリ全体**の指紋。詳細は[運用契約](operations.md)。クライアントが元動画から音声抽出して送る場合は元動画の指紋と異なる。旧ジョブの未記録値を補完しない。同一媒体は対応FSでハードリンク共用し、対象の参照だけを削除する。

個別削除はcapabilities.jobDeletion=trueを確認し、`POST /api/jobs/{id}/delete`へ`{"confirm":"delete-job","clientId":"transcript-cutter"}`を送る。削除前に対象を個別確認する。認証/clientId保護/処理中拒否を維持し、元ファイル・他ジョブ・モデルを対象にしない。

## 配布物の識別と受入範囲

組込み時はBUILD.sourceDirty=false/gitCommit/distribution=program-only、ZIP/EXEのSHA256、依存ライセンスを照合する。未リリース候補を公開済みReleaseと呼ばない。本体の再配布ライセンスはリポジトリで明示されていないため、Rust依存ライセンスと本体の権利を混同しない。各Rust依存の条件と原文は同梱rust-dependencies.json/licenses/rustへ記録する。

合成loopback/保存回復の成功は、物理GPU/LAN/長時間実音声/各アプリの組込みインストーラー受入を意味しない。19GBモデルの隔離取得・実行確認は最新の明示指示により別途実施する。
