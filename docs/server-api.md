# PC・LANサーバーの共通API

処理枠、アプリ分類、履歴ページング、稼働・容量確認と録音整理は[運用と共通API](operations.md)を参照してください。Rust版v0.1.0も既存protocolVersion 3を維持しています。

Rust本体の `TranscriptionServer.exe` は、OpenAIのモデル一覧と音声文字起こしに合わせたHTTP APIを提供します。PythonはWhisper認識ワーカーだけで使用します。
通常のOpenAIクライアントからベースURLを変更して音声文字起こしを呼び出せます。
画面で必要な処理方法・話者分離・保存済み処理などは、拡張情報と非同期APIで扱います。

## エンドポイントと互換性の範囲

| エンドポイント | 用途 |
| --- | --- |
| `GET /v1/models` | OpenAI形式のモデル一覧。`x_meeting` にこのアプリ向けの能力情報を追加 |
| `POST /v1/audio/transcriptions` | multipartで録音を渡し、処理完了後に文字起こしを返す |
| `GET /api/health` | プログラムの準備状況・認証方式・プロトコルバージョン |
| `POST /api/jobs` | 録音を保存して処理を開始する非同期API |
| `GET /api/jobs` | 保存済み処理の一覧 |
| `GET /api/jobs/{id}` | 処理状況 |
| `GET /api/jobs/{id}/segments` | 確定した発言の取得 |
| `POST /api/jobs/{id}/cancel` | 処理の停止 |
| `PATCH /api/jobs/{id}/segments` | 発言の一括編集 |

OpenAIの全APIを実装したものではありません。Chat Completions、Responses、音声合成、Realtime、
単語単位の時刻は対象外です。モデル一覧がOpenAI形式でも、会話専用サーバーが音声文字起こしを受け付けるとは限りません。
この画面の接続先には、処理の継続・履歴・編集を扱う `/api/` の拡張にも対応する本プログラムを使います。

## 認証

v4.3.0では認証方式をhealthの `authMode` で区別します。信頼LANの `--lan` は `authMode=lan`、同PC限定は `authMode=local`。どちらもhealthの `token` を自動取得して変更APIのX-Local-Tokenへ設定します。LANモードではキーの手入力は不要です。`/v1/audio/transcriptions` はこのtokenのBearer指定も受け付けます。LANモードでも接続元サブネット・Host・Originの制限を適用します。

外部公開用の `--server` は `authMode=bearer` で、全ての `/v1/` と `/api/` の操作に次のヘッダーを付けます（healthも認証が必要）。
キーは起動時に設定した `MEETING_SERVER_API_KEY` です。

```http
Authorization: Bearer <サーバー接続キー>
```

同じPC専用の通常モードでは、`GET /api/health` が返す `token` をBearerトークンとして音声文字起こしへ渡せます。
既存の `X-Local-Token` も利用できます。トークンは起動ごとに変わります。
通常モードの待受はループバックに限定し、信頼LANの別PCからは `--lan`、Bearer認証の構成では `--server` を使います。

ブラウザーからのリクエストには、既存の接続元オリジン制限も適用します。
必要なオリジンを `--allow-origin` に指定する方法は[起動手順](server-runtime.md)を参照してください。

## モデルと処理方法の取得

```sh
curl "$MEETING_BASE_URL/v1/models" \
  -H "Authorization: Bearer $MEETING_SERVER_API_KEY"
```

`MEETING_BASE_URL` は、自分で設置したサーバーのオリジンです。例: `https://speech.example.com`。
音声やAPIキーをクエリー文字列へ含めません。

応答例（説明用）:

```json
{
  "object": "list",
  "data": [
    {
      "id": "whisper/base",
      "object": "model",
      "created": 0,
      "owned_by": "meeting-transcriber",
      "x_meeting": {
        "engineId": "whisper",
        "engineLabel": "Whisper",
        "modelId": "base",
        "label": "Whisper Base",
        "processingMethods": [
          {"id": "auto", "label": "自動"},
          {"id": "cpu", "label": "CPU"}
        ],
        "defaultProcessingMethod": "auto",
        "diarization": false,
        "timestampType": "whisper_segment",
        "availability": "download_on_first_use"
      }
    }
  ]
}
```

通常のクライアントは `data[].id` を文字起こしの `model` にそのまま渡します。
モデルIDは `エンジンID/モデルID` で、別の実行エンジンに同じモデル名があっても区別できます。
例えば `whisper/base`、`haas/haas-ja-v1.0`、`openai/gpt-transcribe` です。
実際に選べるIDは、必ずそのサーバーの一覧で確認してください。

`x_meeting` はOpenAIの標準項目ではなく、本プログラムの拡張です。

| 項目 | 意味 |
| --- | --- |
| `engineId` / `engineLabel` | 画面でまとめて表示する実行エンジン |
| `modelId` / `label` | エンジン内部のモデルIDと表示名 |
| `processingMethods` | サーバーが受け付ける処理方法。各要素の `id` を `device` に渡す |
| `defaultProcessingMethod` | 未指定時の処理方法 |
| `diarization` | 自動話者分離の対応 |
| `timestampType` | 保存される区間時刻の種類 |
| `availability` | モデルの利用準備・取得方法 |

一覧には、必要なランタイム・FFmpeg・FFprobe・API設定などがそろったモデルを含めます。
モデルの重みを取得済みかどうかとは別で、`download_on_first_use` のモデルは実行時に必要なファイルを取得します。
`installed` は、speech.cppが返した対象音声認識モデルの既定の重みを取得済みという意味です。
補助のSilero VAD・Nemotronの取得は別なので、`installed` でも追加ダウンロードが発生する場合があります。
Whisperの `download_on_first_use` は取得方式を表し、既存のキャッシュがあれば再利用します。
`provider` は、サーバーに設定した外部APIを使います。外部APIの利用権限や残高は一覧取得時には確認しません。

speech.cppのモデルは `speech models --json` の応答と本アプリが扱えるモデルを照合し、
処理方法は `speech devices --json` の応答から確認します。
WhisperはCTranslate2のCUDA検出結果を使います。GPUを検出できない場合はCPUの選択肢となります。
検出はモデルの読み込みや認識精度の検証ではなく、モデルがGPUメモリに収まる保証でもありません。

画面は「01」で接続した後、この一覧から「02」の選択肢を作ります。
取得に失敗したときや、接続先を変更したときに、別の接続先で取得した一覧を使用し続けません。
サーバー側でも未掲載モデル・未対応処理方法を受け付けません。

## 音声文字起こし

```sh
curl "$MEETING_BASE_URL/v1/audio/transcriptions" \
  -H "Authorization: Bearer $MEETING_SERVER_API_KEY" \
  -F "file=@meeting.wav" \
  -F "model=whisper/base" \
  -F "language=ja" \
  -F "response_format=verbose_json" \
  -F "device=cpu"
```

ファイルはmultipartの `file` として送信します。`model` は一覧に含まれる名前付きIDです。
受け付ける項目は次のとおりです。未対応項目や同じ項目の重複はエラーにします。

| 項目 | 対応する値 |
| --- | --- |
| `file` | 必須。音声または動画1ファイル。FFmpegで音声を読み取れること |
| `model` | 必須。`GET /v1/models` で取得したID |
| `language` | `ja`。省略時も日本語 |
| `response_format` | `json`（既定）、`verbose_json`、`text`、`srt`、`vtt`、`diarized_json` |
| `timestamp_granularities[]` | `verbose_json` のときの `segment` のみ |
| `stream` | 省略または `false` |
| `temperature` | 省略または `0` / `0.0` |
| `prompt` | 省略または空文字 |
| `chunking_strategy` | 省略または `auto` |
| `device`（拡張） | 対象モデルの `processingMethods[].id`。省略時は `auto` |
| `diarize`（拡張） | `true` / `false`。既定は `false` |

`diarized_json` は自動話者分離を要求するため、対応モデルでのみ利用できます。
`json` では `text` と `x_meeting`、`text` では本文、`verbose_json` では本文と発言区間の時刻を返します。
JSONの `x_meeting` は処理ID・時刻の種類・選択した処理方法を含みます。
完了した応答の `X-Meeting-Job-Id` ヘッダーにも処理IDを返します。
`srt` と `vtt` は字幕、`diarized_json` は区間ごとの話者名も返します。
時刻は選んだエンジンに従い、提供先が時刻を返さない場合は音声分割区間の概略時刻です。

HTTPは `Content-Length` 付きmultipartを使用します。ファイル上限は16 GiB、フォームの追加領域は64 KiB、
各文字列項目は8 KiB、項目数は最大16件です。ファイル本体は少量ずつディスクへ書き込みます。
リバースプロキシやクライアントにさらに小さい容量制限がある場合は、その制限も適用されます。

エラーはHTTPステータスと `{"error":{"message":"…","type":"…","code":"…","param":"…"}}` を返します。
利用できないモデルや処理方法を、別の値に黙って変更しません。

このエンドポイントも録音をディスクへ保存し、既存の処理キューへ渡します。
HTTP応答は処理完了まで待つため、長い録音ではクライアントとリバースプロキシのタイムアウト設定が必要です。
アップロード完了後に接続が切れても、サーバーが動作していれば受け付けた処理は続きます。
処理を止める場合は保存済み処理から停止するか、非同期APIの停止エンドポイントを使います。

## 非同期処理との関係

長時間録音の画面は、`POST /api/jobs` のアップロード完了時に処理IDを受け取ります。
`GET /api/jobs/{id}` と `/segments` を使って、完了を待たずに状態や確定済みの発言を表示します。
ブラウザーを閉じた後もサーバーは処理を続け、再接続後は一覧から結果を開けます。

`GET /api/health` の `protocolVersion` は `3` で、`capabilities` に
`modelDiscovery`、`openaiAudio`、`asynchronousJobs` を返します。
モデル一覧・共通音声APIに対応しない以前のプログラムを使っている場合は、配布ZIPのコードを更新して再起動してください。

## 参照

- [OpenAI: モデル一覧](https://developers.openai.com/api/reference/resources/models/methods/list)
- [OpenAI: 音声文字起こし](https://developers.openai.com/api/reference/resources/audio/subresources/transcriptions/methods/create)
- [speech.cpp: CLI、デバイス・モデル一覧](https://github.com/nyosegawa/speech.cpp/blob/v0.10.1/docs/cli.md)
