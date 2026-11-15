# 単独話者分離API（現行v0.1.6）

protocolVersion=3を維持した追加機能です。文字起こしを行わず、Nemotron 3 Diarizationだけで元メディア全体（または指定範囲）の話者区間を返します。既存のNemotron＋Whisper/native ASRは維持します。

## 受付

```http
POST /api/jobs?engine=diarization&model=nemotron-3-diarization&device=cpu&clientId=Example-ai-room&workload=file&name=recording.wav
Content-Type: application/octet-stream
Content-Length: <バイナリのバイト数>
X-Local-Token: <healthからメモリー内で取得した操作トークン>

<音声または動画のバイナリ>
```

認証・Host/Origin/信頼LAN許可・容量制限は既存ジョブと共通です。Bearerモードでは既存Authorizationを使用します。トークンやキーをURLやログへ保存しません。成功は201と`{"job":{...}}`です。

- engine=`diarization`、model=`nemotron-3-diarization`。別のASRや分離器に代替しません。
- device=`auto|cpu|gpu`。**autoはCPUへ確定**します。明示gpuはspeech.cppがGPUを広告する場合だけ受け付けます。`actualDevice`はこの経路では`cpu|gpu`です（Whisperの`cuda`表記とは別）。GPU検出と実機での推論成功は区別してください。
- workload=`file`推奨。既存realtime枠も使用できますが範囲は30秒以下です。既存キュー・GPU排他・中止を再利用します。
- start/endは省略時に全体、指定時は元メディア秒。出力へstartを一度だけ加算し、範囲末尾で切ります。重複して発話する話者の区間は重複したまま保持します。
- diarizeは省略またはtrue。falseは400です。
- speech.cpp、FFmpeg/FFprobe、Nemotronの選択された型の重みが導入済みの場合だけ利用できます。受付時と実行時に重みを再点検し、取得済みファイルの絶対パスで呼び出すため初回取得はしません。未導入時は503です。管理画面の補助モデルから利用者が明示的にインストールし、再点検してください。

`/v1/audio/transcriptions`はASR専用なので、このモデルの指定は400で拒否します。

## 状態・結果・中止

既存の`GET /api/jobs/{id}`、`GET /api/jobs/{id}/segments?after=0&limit=5000`、`POST /api/jobs/{id}/cancel`を使います。ページングのnext/total/revisionも維持します。

```json
{
  "segments": [
    {"id":1,"start":1.2,"end":3.4,"text":"","speaker":"話者1","speakers":["話者1"],"speakerSource":"automatic","speakerNeedsReview":false,"timestampType":"diarization_segment"}
  ],
  "next":1,
  "total":1
}
```

1行は1話者の連続区間です。同時発話は時刻が重なる別行で表します。speakerは既存クライアント用の互換値で、speakersはその行の話者配列です。話者IDはこの録音内の自動番号で、実在人物の識別や別録音との同一性を保証しません。単語・ASR本文は生成しません。**空のtextを理由に区間を捨てないでください。**

ジョブには`task=diarization`、metadataにも同じtaskを付けます。正常終了時は次を返します。

```json
{"asrPerformed":false,"diarizationStatus":"complete","diarizationModel":"nemotron-3-diarization","diarizationSpanCount":2,"actualDevice":"cpu","timestampType":"diarization_segment"}
```

無発話ならjob.statusはcomplete、diarizationStatusはno_speakers、segmentsは空です。待機/実行中はpending/running、失敗はerror、中止時はpartialです。cancelledはジョブの終端状態として維持し、後着結果でcompleteへ変更しません。全区間の分離完了後に区間を保存するため、分離中の中止では保存区間が0件になることがあります。

## 能力広告とASRの区別

`/api/health`の`capabilities.standaloneDiarization=true`はAPIの実装を示します。実際の利用可否はengines内の`id=diarization`のready/reason/modelsを確認します。

利用可能時だけ`GET /v1/models`に独立行を返します。

```json
{"id":"diarization/nemotron-3-diarization","object":"model","x_meeting":{"engineId":"diarization","modelId":"nemotron-3-diarization","task":"diarization","availability":"installed","timestampType":"diarization_segment"}}
```

実応答にはラベル・processingMethods・defaultProcessingMethod・diarization関連フィールドも含まれます。通常ASR行のtaskはtranscriptionです。ASR選択欄はtaskがdiarizationの行を除外し、話者分離選択欄へ表示してください。旧サーバーのtask省略はtranscriptionとして扱えます。

## Nemotron 3.5 Transcribe

これは分離専用Nemotron 3 Diarization、0.6BのNemotron ASRとは別の一体型です。v0.1.8の実装はspeech.cpp 0.10.1/CPU、engine=`nemotron-transcribe`、model=`nemotron-3.5-transcribe`です。**19GBモデルは現PCでは取得・実行しません。新PC購入後に再検討します。** `settings.json`の`enable_integrated_model`は既定false。管理画面に対象外と表示し、導入/ロード/認識をAPIでも拒否します。実行候補`/v1/models`には含めません。

将来のPCで再検討・明示有効化した場合、環境点検後にCPU/自動・内蔵話者分離・manualInstallRequired・downloadBytesを通知します。IQ4_NLの18,997,225,856 bytesは事前導入だけとし、認識時には自動取得しません。RAM約20～24GiBは概算で、速度・精度・メモリは未実測。追加Python/CUDAは不要ですが、一体型のGPU/M4000実行は未対応・未受入です。

認識は最大1時間を一括処理し、内蔵timestamps/speakersを使います。別VAD/別話者分離モデルを必須にせず、手動常駐モデルを再利用し、常駐がなければその処理だけロードして解放します。短い音声を順に投入する場合、話者番号の継続は保証しません。時刻と話者を検証して字幕へ保存し、重なり発話も保持。詳細出力は各ジョブの`integrated-result.json`へ保存します。時刻を確定できない場合も本文/理由を残しますが、字幕時刻を推測せずジョブをエラーにします。追加Pythonを使うNIM/vLLMのアダプターは実装しません。

