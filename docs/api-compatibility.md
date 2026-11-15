# 認識APIの互換範囲（v0.1.16）

外部APIはOpenAIのファイル認識・Realtime文字起こしの形に寄せる。全仕様互換ではない。既存の非同期ジョブ、同ID復旧、媒体重複排除、RAM／VRAM制限、リアルタイム優先は継続する。

## モデル情報

- `GET /v1/models` と `GET /v1/models/{id}`。IDには一覧の `engine/model` を使用する。個別取得では `/` を `%2F` にURLエンコードできる。
- `x_meeting.capabilities` に言語、prompt、keywords、decoding、temperature、SSE、WebSocketの可否を返す。
- `GET /api/models` の各モデルにも `capabilities` を追加する。従来の概要・導入状態・容量・保存済み参考速度は維持する。
- 能力と導入済み／実行可能は別。古い独自Whisperワーカーが新引数の対応を通知しない場合は対応情報を縮小し、指定された新引数を無視せず拒否する。

## 共通の認識オプション

| 項目 | 対応と意味 |
|---|---|
| `language` | Whisper／Qwenは対応する言語コードまたはauto。NeMoは既存のlocaleまたはauto。Haas／Reazon／日本語Parakeet／一体型の現在の公開範囲は日本語。外部APIはサーバー側の提供先設定と一致する値だけ受け付ける |
| `prompt` | Qwen・Whisper・Nemotron一体型の文脈指定。UTF-8で8192バイト以下。認識が必ず指定語どおりになる保証ではない |
| `keywords[]` | ファイル認識multipartのHaas用専門用語。フィールドを繰り返す。例 `Haas=ハース,ハウス`、`AIモデル`。64件・合計8192バイト以下。各語句に改行は入れない |
| `decoding` | Reazonのbeamまたはgreedy |
| `temperature` | Whisperは0〜1。それ以外は0だけを互換入力として許可する |
| `device` | 既存のauto／cpu／gpuを維持 |

`POST /api/jobs` は同じオプションをクエリに指定できる。専門用語だけは `keywords` に文字列のJSON配列をURLエンコードする。秘密を含む文脈をURLに載せたくない場合はmultipartの認識入口を使用する。

指定はジョブの `metadata.recognitionOptions` に保存する。CLI・常駐ワーカー・CPUへのフォールバック・同ID復旧で同じ値を使う。要求ごとの文脈をモデル全体の既定値として保持せず、次の要求へ漏らさない。

`verbose_json.language` は検出された言語、なければ指定言語を返す。自動検出結果がない場合は省略し、日本語という固定値で補わない。`diarized_json` にはtask・duration・話者付きsegmentsを返す。単語時刻・任意VADパラメーターは今回追加していない。

`/v1/` のエラーは `{error:{message,type,param,code}}`。入力不正、未対応、認証、利用不可、実行失敗を区別する。`/api/` の既存文字列errorは変えない。

Whisperのverbose_jsonはバックエンドのavg_logprob・compression_ratio・no_speech_prob・seek・temperature・tokensを保持し、公式Python SDK 3.51.0の厳密検査を確認した。他エンジンのverbose_jsonは時刻中心の拡張形式で、取得できない統計は省略する。全エンジンでの厳密なWhisper統計スキーマ互換を保証するものではない。

機械可読仕様は[openapi.json](openapi.json)。候補サーバーの `GET /v1/openapi.json` でも取得できる。OpenAPI 3.1で30経路を記述し、WebSocketイベントは `x-websocket` 拡張のスキーマで定義する。通常のHTTPレスポンスと区別する。

## SSE：ファイル認識の結果を逐次取得

`POST /v1/audio/transcriptions` のmultipartに `stream=true`、`response_format=json` を指定する。音声アップロード完了後、Content-Type `text/event-stream` で通知する。

```text
event: transcript.text.delta
data: {"type":"transcript.text.delta","delta":"確定した字幕"}

event: transcript.text.done
data: {"type":"transcript.text.done","text":"全字幕","x_meeting":{"jobId":"...","resultGeneration":0}}
```

- 確定した区間を順に通知する。モデルの仮説をトークンごとに推論し直す機能ではない。deltaを順に結合するとdone.textに一致する。
- ヘッダー `X-Meeting-Job-Id` から履歴を参照できる。10秒ごとにkeepaliveを送る。
- エラー後はdoneを送らない。結果世代が変わる場合もエラーで閉じる。旧世代のdeltaと新しい結果を結合しない。
- 切断・送信詰まりでは、その接続の未完了ジョブだけを中止する。保存済みの履歴・媒体は残る。接続を自動で再開する機能やLast-Event-IDによる再送は今回提供しない。必要なら既存ジョブAPIで全結果を再取得する。

## WebSocket：クライアントが発言確定を送る認識

`/v1/realtime?intent=transcription` に接続する。Bearer認証、またはブラウザー用のサブプロトコル `realtime` と `openai-insecure-api-key.<接続トークン>` を使用する。トークンをURLへ入れない。既存のHost・接続元・Origin制限を適用し、GETでも操作認証を必須とする。HTTPS運用ではwssを使用する。

最初に `session.created` を受け取り、次を送る。

```json
{
  "type": "session.update",
  "session": {
    "type": "transcription",
    "audio": {
      "input": {
        "format": {"type": "audio/pcm", "rate": 24000},
        "transcription": {"model": "haas/haas-ja-v1.0", "language": "ja"},
        "turn_detection": null
      }
    },
    "x_meeting": {"device": "cpu"}
  }
}
```

`session.updated` の後、24kHz・mono・PCM16 little-endian音声をbase64にして追加し、発言末尾で確定する。

```json
{"type":"input_audio_buffer.append","audio":"<base64 PCM>"}
{"type":"input_audio_buffer.commit"}
```

受信イベントは `input_audio_buffer.committed`（item_id・previous_item_id・x_meeting.jobId）、`conversation.item.input_audio_transcription.delta`、`conversation.item.input_audio_transcription.completed`（全文）。同じitem_idのdeltaを結合するとcompleted.transcriptに一致する。失敗時は`.failed`を送る。複数発言の完了順序は固定せず、item_idで対応を取る。

- `input_audio_buffer.clear` は未確定音声だけを消す。`transcription.cancel` とitem_idは、この接続の未完了発言だけを中止する独自拡張。
- 1発言0.1〜30秒、1接続で未完了4発言まで。32接続、1メッセージ256KiB、送信待ち10秒、無通信120秒の制限がある。音声の未確定バッファは1.44MB以下。
- Whisper／Haas／Qwen／Reazon／Parakeet／NeMoの短時間認識を利用する。一体型・外部API・単独話者分離はWebSocketの対象外。
- `x_meeting.device` はauto／cpu／gpu。既定auto。GPU容量を検出できずVRAM上限も未設定なら、自動は実行前にCPUへ切り替える。明示gpuは容量確認待ち、容量確認済みで予算不足なら従来どおり待機する。CPUへ切り替えた後もRAM上限を守る。
- 処理は通常のrealtimeジョブなので、資源上限・公平化・受付停止・待機・履歴保存が共通。切断時は当該接続の未完了ジョブを中止する。
- モデルと言語の変更は未確定音声・未完了発言がないときだけ可能。文脈promptは対応モデルで指定する。
- サーバー側の無音自動確定、話者分離、トークン単位の暫定推論、切断後のイベント再開、WebRTC、音声合成・会話生成は今回の対象外。

## 参照

- [OpenAI ファイル認識](https://developers.openai.com/api/reference/resources/audio/subresources/transcriptions/methods/create)
- [OpenAI Realtime transcription](https://developers.openai.com/api/docs/guides/realtime-transcription)
- [speech.cpp 0.10.1 worker](https://github.com/nyosegawa/speech.cpp/blob/v0.10.1/docs/worker.md)
- [speech.cpp 0.10.1 認識オプション](https://github.com/nyosegawa/speech.cpp/blob/v0.10.1/docs/cli.md)
