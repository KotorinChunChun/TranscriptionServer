# 運用と共通API（v0.1.6）

protocolVersionは3を維持します。従来の認証とAPIはそのまま使用できます。

単独話者分離は [単独話者分離API](standalone-diarization.md) を参照してください。ASR行と分離専用行をtaskで区別します。

## 処理枠

`POST /api/jobs`へ任意のクエリ`clientId`と`workload`を追加しました。

- clientId：英数字・ピリオド・ハイフン・アンダースコアの1〜64文字。未指定はunknown。分類用であり利用者認証・アクセス制御ではありません。
- workload：file（既定）またはrealtime。realtimeは30秒以下の処理範囲だけを受け付けます。全体長はメディア読取後に検証し、超過はerrorで確定します。
- fileとrealtimeは各1実行枠。CPUは両枠を独立に実行します。autoでGPU非対応の場合も受付時の能力情報からCPUへ確定して分離します。GPU候補のauto/GPUはメモリ過剰確保を防ぐため共通排他です。実行中GPU処理への割り込みはしません。
- Whisperのモデルは枠ごとに最大1プロセスを再利用。モデル/デバイス/スレッド変更、中止、失敗、終了、180秒のアイドルで破棄します。auto/GPU枠の開始前は他枠の未使用auto/GPU候補キャッシュを解放します。
- WhisperのCPUは未指定時、各枠で最大4スレッド。speech.cppへは指定時だけ`--threads`を渡します。speech.cppは管理画面で手動ロードしたworker protocol4モデルを同じ枠/モデル/デバイスで再利用し、通常のCLI認識は都度起動します。NeMo-Speech.cpp 0.3.0にはこのスレッド設定を渡さず、そのランタイムの既定値で動き、300秒以下の分割ごとに都度起動・ロードします。外部APIもモデル常駐の対象外です。
- v0.1.5から、Whisper・Nemotron ASR・手動ロードしたspeech.cppモデルで話者分離を有効にし、認識がGPU・分離がCPUの場合だけ同時に実行します。両方CPUは競合を避けて直列です。話者分離と認識の両方がGPUになる指定（`device=gpu`のWhisper/常駐モデル）は従来どおり話者分離→認識の直列です。並行時も、話者区間が確定するまで字幕とprocessedUntilを保存せずに保持し、確定後に従来と同じ方法で話者を付けて保存するため、保存される字幕・話者・字幕IDは直列時と同じです。未割当の字幕が一時的に見えたり、後から書き換わったりはしません。並行中のphaseは認識側（transcribing）で、認識が先に終わるとdiarizingへ切り替わります。片方の失敗・中止で他方も停止します。
- 手動ロードしたspeech.cppモデルの発話切り出しは、v0.1.5から発話ごとのFFmpeg起動をやめ、VAD用WAVの該当範囲を直接読みます（FFmpegの`-ss/-t`と同じサンプル範囲）。SQLiteはWALの`synchronous=NORMAL`で、アプリの異常終了ではコミット済みの字幕を失わず、OS停止・停電時は直前のコミットが失われ得ます。

## 処理時間の記録

v0.1.3以前の資料にはrunSeconds/finishedAtを返すと記載していましたが、Rust実装では保存されていませんでした。v0.1.4で実装し、待機との境界も修正しました。protocolVersion=3を維持し、metadataへ追加します。

| metadata | 意味 |
| --- | --- |
| timingVersion | 新計測は1 |
| queuedAt / dequeuedAt | アップロード後のキュー投入 / ワーカー枠の取得 |
| startedAt / finishedAt | GPU排他取得後の処理開始 / 完了・中止・エラー・中断の確定 |
| queueSeconds | キュー内の待機秒数 |
| resourceWaitSeconds | GPU排他の待機秒数。CPUは0 |
| waitSeconds | queueSeconds + resourceWaitSeconds |
| runSeconds | startedAtから終了までの処理秒数 |
| timingMeasuredAt | 応答の計測時刻。終端ではfinishedAtに固定 |
| completedAt | 正常完了時刻（従来互換） |
| timingEstimated | trueの場合、旧記録からの推定または異常終了後の最終更新時点までの下限 |

時刻はUTC、秒数は小数を含みます。runSecondsは音声前処理、モデル取得/ロード、ASR、話者分離、結果確定を含み、アップロードとキュー/GPU待機を含めません。モデルを事前ロードした場合との比較には注意してください。WhisperはmodelReused/modelLoadSecondsも返します。

ジョブ取得・一覧・ページング・稼働APIでは計測中の値を返し、終端でSQLiteへ保存します。字幕編集、履歴再読込、録音整理、再起動でも保存値を保持します。正常停止はその時点で固定し、強制終了後の復旧は最終DB更新時点までの下限です。停止していた時間を処理時間へ加算しません。UTC時計の逆行は0へ丸めます。

旧ジョブのstartedAt/completedAtがある場合だけ旧範囲の経過を推定します。旧startedAtはGPU待機前のため新runSecondsと直接比較できません。旧resourceWaitSecondsは前処理を含む誤った値だったため、新しい待機計算に流用しません。不明値はnull（画面は記録なし）、時刻自体がない旧履歴は時間表示なしです。

処理経過と所要時間、待機合計とキュー/GPU内訳はAPI値を使って利用アプリ側で表示します。未リリース修正で同梱の文字起こし画面とブラウザー内認識を廃止し、サーバーのWeb画面は管理専用になります。保存済みのmetadataと時間項目、APIでの取得は維持します。

「GPU候補」と「自動」の意味は[処理方法とGPU比較](gpu-execution.md)を参照してください。

## 稼働・保存容量

- `GET /api/status`：枠ごとのワーカー状態・待機数、実行中のジョブIDと分類・処理段階、モデルキャッシュの状態、最終ワーカー例外の種類。音声の本文や認証値は含めません。
- `GET /api/storage`：保存先全体のusedBytes、ディスクのfreeBytes/totalBytes、jobCount。明示更新時にファイルを集計します。
- 画面「保存済みの処理」の「サーバー稼働・保存容量」から確認できます。容量を常時全走査するポーリングはしません。

## サーバーPCで処理履歴を見る

サーバーPCでスタートメニューのTranscriptionServerを開き、管理画面上部の「処理履歴」を選びます。最新の受付から順に、ファイル名、受付日時、状態、認識結果の区間数、モデルとクライアントが表示されます。ジョブを選ぶと、保存されている入力音声を再生し、認識テキスト、話者、使用モデルと実行環境を確認できます。「更新」で処理中のジョブの状態を取り直せます。

既存の履歴も表示されます。サーバーに入力音声が残っているジョブを再生できます。別のPCからはこの管理機能にアクセスできません。接続中のクライアントは従来どおりのAPIを使います。

## 履歴ページング

`GET /api/jobs?limit=50&offset=0&clientId=meeting-visualizer&workload=realtime`

limitは1〜200、offsetは0以上。clientId/workloadは省略可能。応答はjobs、total、offset、nextOffset。末尾ではnextOffset=nullです。並びはcreatedAtとIDの降順。更新中の履歴に対するoffsetなので、新規ジョブ追加をまたぐ場合は先頭から再取得してください。

パラメーターなしの`GET /api/jobs`は従来どおり`{jobs:[...]}`です。既存データはjob_indexへ自動移行し、ID・元データ・編集を保持します。sourceVersionや新しい能力は`/api/health`のserverVersionとcapabilitiesから確認できます。

## 録音の整理

`POST /api/jobs/{id}/remove-media`、JSON `{"confirm":"remove-media"}`。変更操作の認証が必要です。

完了・中止・エラー・中断ジョブのsource.mediaだけを削除します。処理中は拒否し、字幕・編集・ジョブID・ログは保持します。画面では専用の確認画面を表示します。削除後はサーバーからの録音再生ができなくなります。TranscriptCutterの元動画はアプリPC側のファイルなので、この操作の対象ではありません。

自動削除はありません。全アプリが同じ履歴を参照するため、整理前に再生・再処理の必要性を確認してください。空き容量がアップロード長＋128MiB未満なら新規raw uploadを507で拒否します。

## 別PCへの移行

1. 進行中の認識・アップロードを完了または中止し、サーバーを正常終了します。
2. データフォルダー全体（workbench.sqlite3、関連するSQLiteファイル、各ジョブのフォルダー）をバックアップして移行します。起動中DBのファイル単純コピーは避けます。
3. Windows x64インストーラーを使用するか、ZIP全体を展開します。Rust EXEとworker・distを一緒に置きます。PythonはWhisper専用ワーカーへ同梱し、別途導入不要です。FFmpeg/FFprobeは別途配置し、speech.cppとGPU関連ライブラリーは必要時に導入します。モデルは同梱しません。
4. 同PCだけの初期確認には`--local`、信頼LANには既定の`--lan`を使います。LANはプライベートIPv4サブネット・Host・OriginとWindows FWで限定し、キーの手入力は不要です。Bearerが必要な構成では`--server`と環境変数`MEETING_SERVER_API_KEY`を明示します。移行先アドレスや秘密値は成果物へ保存しません。
5. 各アプリに移行先URLを設定します。TranscriptCutterは信頼LANに対応します。MeetingVisualizerとAI Roomの現在のloopback接続は維持し、将来の別PC移行は担当チャネルでURL対応を済ませてから切り替えます。

実移行先PCとGPUの動作、長時間実会議の精度は別の受入項目です。開発PCでの合成音声試験と区別します。

## v0.1.5 自動選択と再起動

自動はGPU候補があればGPU優先、失敗時にCPUへ1回切り替えます。自動GPUの字幕は成功まで暫定で、失敗時に重複なくCPUで再処理します。詳細は[gpu-execution.md](gpu-execution.md)。再起動ボタンとAPI・実行環境の反映は[runtime-management.md](runtime-management.md)を参照してください。

## 工程別計測（v0.1.7）

`job.metadata.measurements`は`schemaVersion:1`と`stages`配列。工程は`id`（試行ごとに固有）、`stage`/`label`、`status`（running/complete/error/cancelled/interrupted）、UTCの`startedAt`/`finishedAt`、サーバー単調時計の`elapsedSeconds`、`executionDevice`/`cpuFallbackAttempt`を保存し、履歴・結果JSONにも保持する。工程の並行実行や全体/内訳は重なるため、工程時間の合計を所要時間にはしない。待機は既存queueSeconds/resourceWaitSecondsで別計測する。

工程種別はmedia_probe/audio_preparation/diarization/vad/recognition、Whisper全体worker_processingと新ワーカーのmodel_load/audio_preparation/recognition、外部API全体provider_processing。ネイティブCLI内部のロード/VAD/認識を分解できない場合は結合した工程名にする。旧Whisper EXEは全体だけ記録し、細かい内訳を捏造しない。GPU失敗とCPU再試行は異なる工程IDとデバイスで保持する。

`memory`は`scope:owned_worker_process_tree`、`metric:private_committed_bytes`、`unit:bytes`。Windows Job Objectへ所有登録したワーカー/子プロセスのPrivateUsage合計を約200ms間隔で標本化する。`peakBytes`は標本最大、`lastBytes`は最終成功標本（終了時現在値ではない）、`sampleCount`/`samplingIntervalMs`/`peakKind:sampled_maximum`/`method`を記録する。ロード済みの常駐モデル分も含む。VRAM・物理メモリ常駐量・サーバー全体・PC全体・外部API提供元のメモリではない。瞬間的なピークを取り逃す可能性がある。不明/未計測はnullと`notMeasuredReason`、0で埋めない。OS異常終了で未確定の工程はinterrupted、単調時計の終了値がないためelapsedSeconds/finishedAt=nullとする。

新WhisperワーカーへのJSON Lines要求に`reportStages:true`を指定した場合だけ`type:stage`、`stage`、`event:start/complete/error`を出力する。要求しない旧サーバーには新しい種類のメッセージを送らない。

## 個別ジョブの認識結果・保存音声削除（v0.1.8）

`health.capabilities.jobDeletion=true`のサーバーだけで使用する。`POST /api/jobs/{32桁小文字hex ID}/delete`、既存の認証を使用し、JSONは`{"confirm":"delete-job","clientId":"transcript-cutter"}`。必ず個別の対象確認後に送る。一括/自動削除APIは提供しない。成功200は`{"deleted":true,"id":"...","jobId":"..."}`。対象の字幕（編集済みを含む）/GPU暫定字幕/索引/ジョブ記録と、その専用フォルダーの保存音声・中間ファイル・ログを削除する。元動画、モデル、他ジョブは対象外。

確認値/clientId欠落は400、アプリ識別の不一致は403、対象なし/削除済みは404、処理/待機/転送/更新/管理操作中または非終端状態は409、ファイル使用中・不正な保存パス・リンク・保存/DBエラーは500。既存認証不許可は従来どおり401/403。clientId一致はジョブの取り違え防止であり、別アプリの認証主体を新設するものではない。

受付排他とDBロックの下で対象4表を未確定削除し、専用フォルダーを`.deleted-{jobId}-{nonce}`へ退避してからDBをcommitする。COMMIT失敗は媒体を戻し、成功後だけ清掃する。プロセス異常終了時は起動時にDBのジョブ存否を照合し、未確定削除を復元・確定済み削除の清掃を再試行する。清掃時に使用中のファイルは退避領域に保持する。OS電源断等も含む完全なファイルシステム/DB一括トランザクションではなく、WAL NORMALの制限は残る。物理媒体の消去保証はしない。

## アップロード媒体の照合・共用（v0.1.8）

- `health.capabilities.sourceMediaDigest=true` / `mediaDeduplication=true`、`mediaDeduplicationMode=hardlink_when_supported`を通知する。
- 完全受信したバイナリ全体の`sourceSha256`（小文字64桁hex）と`sourceBytes`（バイト数）をジョブ直下へ保存する。raw `/api/jobs`とmultipart `/v1/audio/transcriptions`で同じ規則。旧ジョブの未知値は未記録で、推測や自動再計算をしない。
- これは送信したバイナリの指紋。クライアントが動画をWAVへ変換して送った場合は変換後の値であり、元動画の指紋ではない。元動画照合にはクライアント側で対応を保存する。
- 同一SHA256かつサイズの既存媒体があれば、ジョブ別`source.media`をハードリンクで共用する。保存後は書き換えない。1参照を削除しても他は残り、最後のリンクでOSが実体を解放する。共有元ジョブIDは開示しない。元動画・モデルは触らない。
- 非対応/リンク作成不可の環境では独立保存する。`metadata.mediaDeduplicationApplied`はその登録で共用を適用したかの履歴で、現在の参照数ではない。usedBytesはジョブ別の論理容量合計で物理使用量ではない。空き容量はファイルシステムの実値。
- remove-mediaも対象の参照だけを`source.removing`へ退避してDB確定後に清掃し、失敗/再起動時には復元・清掃を行う。照合用ハッシュは整理後も残す。処理・転送・更新・モデル操作中は拒否する。


## 旧アップロードのオフライン整理（v0.1.10）

共用機能の追加前に保存したジョブには指紋がなく、新規アップロード時の照合対象に入りません。サーバーを停止し、未完了ジョブがない状態で次を実行します。既存DBのある絶対パスだけを受け付け、HTTP/認識ワーカーは起動しません。

```
TranscriptionServer.exe --compact-media --data-dir C:\path\to\data
TranscriptionServer.exe --compact-media --data-dir C:\path\to\data --apply
```

最初は点検のみ、--applyでSHA256/サイズを補完し、同一保存先内の完全一致媒体をハードリンク共用します。全ジョブのパスと字幕を残し、他の保存先/バックアップ/元動画/モデルへは触れません。使用中の保存先・未完了ジョブ・記録済み指紋との不一致は拒否します。ファイル/DB失敗時は既存のsource.unshared退避形式で復元し、異常終了時は通常の起動回復も使用します。非対応ファイルシステムでは独立保存を維持します。

sharedPathsは今回リンクを作成した操作件数で、物理的な削減量ではありません。エクスプローラーのファイル名・件数・サイズ合計やAPIのusedBytesは、ハードリンク化しても同じです。物理使用量/空き容量と区別してください。整理後は既存のアップロード共用処理が補完済みの旧ジョブにも一致できます。
