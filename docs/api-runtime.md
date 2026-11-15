# OpenAI・OpenAI互換APIで文字起こしする

画面の「01」で「LAN内サーバー」か「APIサーバー」へ接続し、「02」に取得したOpenAI APIまたはOpenAI互換APIを選ぶと、その補助アプリを経由して使えます。
ブラウザーは録音を補助アプリへ渡し、補助アプリがAPI提供先へ送信します。
APIキーは補助アプリを起動するプロセスの環境変数に設定します。
画面のサーバー接続キーは補助アプリへの認証用で、OpenAIなどのAPIキーとは別です。

画面の処理サーバーURLは実行環境ごとに記憶します。接続後にモデルを選んでもURLは変わりません。
自分で設置する処理サーバーは初回にURLを指定してください。OpenAI公式APIのURLをこの欄へ入力しません。
接続先が変わると接続キーを再入力します。

公開SiteだけではAPIのキーや推論用サーバーを提供しません。
APIを使うためのアカウント・利用枠と、Rust版TranscriptionServer・FFmpeg・FFprobeが必要です。
APIだけを使う場合はPythonやspeech.cppを起動しません。Windows配布物のPythonはWhisper専用です。

## OpenAI API

OpenAIへの送信先は `https://api.openai.com/v1/audio/transcriptions` です。
起動する端末で `OPENAI_API_KEY` を設定してから、補助アプリを起動します。

Windows PowerShell:

```powershell
$OpenAISecret = Read-Host 'OpenAI API key' -AsSecureString
$env:OPENAI_API_KEY = [System.Net.NetworkCredential]::new('', $OpenAISecret).Password
./launch-windows.cmd
```

Linux / Bash:

```sh
read -r -s OPENAI_API_KEY
export OPENAI_API_KEY
./TranscriptionServer --local --port 8769 --no-browser
```

Linuxの `read` はキーを画面に表示せず読み取ります。入力してEnterを押してください。
サービスとして実行する場合は、そのサービスの環境変数にキーを設定します。
環境変数を変えた後は補助アプリを起動し直してください。

画面の「01 実行環境を選ぶ」で補助アプリのURL・接続キーを設定して接続し、「02 文字起こしエンジンを選ぶ」で取得したOpenAI API、モデル種別を選びます。
録音と範囲を指定し、「文字起こしを開始」を押します。
APIの利用料は設定したキーのアカウントに従います。画面で接続するだけでは音声認識APIを呼びません。

### 選べるOpenAIモデル

このアプリの初期選択は **GPT Transcribe (`gpt-transcribe`)** です。
2026年11月に確認した[OpenAI公式ガイド](https://developers.openai.com/api/docs/guides/speech-to-text)では、
従来の `gpt-4o-transcribe`、`gpt-4o-mini-transcribe`、`gpt-4o-transcribe-diarize`、`whisper-1` は
非推奨で、停止予定日は **2027年2月26日**です。画面では旧モデルと表示します。
利用できるモデルはAPIアカウントと提供時期によって変わります。

| モデル | このアプリが指定する形式 | 時刻と話者の扱い |
| --- | --- | --- |
| `gpt-transcribe` | `json` | 本文を取得。区間時刻が返らない場合は、約1分ずつ送った範囲を概略時刻として表示。自動話者分離なし |
| `gpt-4o-transcribe`（旧） | `json` | 同上 |
| `gpt-4o-mini-transcribe`（旧） | `json` | 同上 |
| `gpt-4o-transcribe-diarize`（旧） | `diarized_json` | APIの発言時刻と話者を利用。話者の反映は「話者を自動で分ける」を有効にした場合 |
| `whisper-1`（旧） | `verbose_json` | APIの発言区間タイムスタンプを利用。自動話者分離なし |

`gpt-transcribe` には日本語を `languages[]` で指定します。他のOpenAIモデルには `language=ja` を指定します。
時刻が得られない本文へ、単語・文ごとの正確な時刻を推測して付けることはしません。
概略時刻の行には「概算時刻」を表示します。API応答の仕様は
[文字起こしAPIリファレンス](https://developers.openai.com/api/reference/resources/audio/subresources/transcriptions/methods/create)を参照してください。

## OpenAI互換API

以下は、本プログラムが別の音声APIを呼ぶときの設定です。本プログラム自体が提供する共通APIについては、
[サーバーAPI仕様](server-api.md)を参照してください。

他の音声モデルを提供するサーバーが、OpenAI形式の `POST /audio/transcriptions` に対応している場合に使えます。
補助アプリを起動する側で、APIのベースURLとモデルIDを設定してください。
対応する音声モデルや料金は、そのAPI提供先によります。

| 環境変数 | 用途・初期値 |
| --- | --- |
| `TRANSCRIPTION_API_BASE_URL` | 必須。例 `https://api.example.com/v1`。末尾に `/audio/transcriptions` を付けて送信 |
| `TRANSCRIPTION_API_MODEL` | 必須。提供先が受け付けるモデルID。画面にもこのIDを表示 |
| `TRANSCRIPTION_API_KEY` | 認証が必要なAPIのBearerキー。認証不要の接続先では省略可能 |
| `TRANSCRIPTION_API_RESPONSE_FORMAT` | `json` が初期値。`verbose_json` または `diarized_json` も指定可能 |
| `TRANSCRIPTION_API_LANGUAGE` | `ja` が初期値。空文字なら言語のフィールドを送信しない |

Windows PowerShellの例:

```powershell
$env:TRANSCRIPTION_API_BASE_URL = 'https://api.example.com/v1'
$env:TRANSCRIPTION_API_MODEL = 'provider-model-id'
$env:TRANSCRIPTION_API_RESPONSE_FORMAT = 'json'
$env:TRANSCRIPTION_API_LANGUAGE = 'ja'
$ProviderSecret = Read-Host 'API key' -AsSecureString
$env:TRANSCRIPTION_API_KEY = [System.Net.NetworkCredential]::new('', $ProviderSecret).Password
./launch-windows.cmd
```

URLとモデルIDは実際の提供先に置き換えます。URLはHTTPSを使い、同じPC上の開発用サーバーだけは
`http://127.0.0.1:...` などを使用できます。URLにユーザー名・パスワード・クエリーを含めません。
APIの接続先やキーを録音アップロードのパラメーターから変更する機能はありません。

このアダプターはmultipartの `file`・`model`・`response_format` を送り、JSONの `text`、または
`segments` 内の `text`・`start`・`end` を読みます。`diarized_json` の場合は `speaker` も読み、
`chunking_strategy=auto` を指定します。`verbose_json` では `timestamp_granularities[]=segment` を指定します。
提供先がこれらの項目やMP3入力に対応することを確認してください。
すべての「互換」を名乗るAPIや、会話API・Realtime APIへ接続する汎用アダプターではありません。

## APIサーバーを経由する

[サーバー手順](server-runtime.md)の `MEETING_SERVER_API_KEY` とHTTPSを設定したうえで、
同じサーバープロセスに `OPENAI_API_KEY` または `TRANSCRIPTION_API_*` を設定します。
画面には処理用サーバーのURLと `MEETING_SERVER_API_KEY` を入力します。
API提供先のキーはサーバーの環境変数に残し、画面・録音のURL・書き出したJSONには含めません。

| キー | 設定する場所 | 認証する相手 |
| --- | --- | --- |
| `MEETING_SERVER_API_KEY` | サーバーの環境変数と、画面のサーバー接続欄 | 自分の文字起こしサーバー |
| `OPENAI_API_KEY` | 補助アプリまたはサーバーの環境変数 | OpenAI |
| `TRANSCRIPTION_API_KEY` | 補助アプリまたはサーバーの環境変数 | 設定した互換API |

## 長い録音・タイムスタンプ・話者

APIへ送る前に、FFmpegで指定範囲をモノラル16 kHz・64 kbpsのMP3へ変換します。
本文のみのJSON形式は最大60秒、区間時刻付きの形式は最大600秒ずつ送信します。
各送信ファイルの上限は24 MBで、OpenAIが公表する25 MBの上限に余裕を持たせています。
元の録音全体を一度にAPIへ送る方式ではありません。

APIが区間時刻を返した場合は、送信区間の開始位置を足して、元の録音の時刻として保存します。
時刻が欠けている、または不正な値の場合は、その送信区間の概略時刻を使って確認表示を付けます。
区間の境界で文が分かれることがあります。

話者番号は通常、そのAPIリクエストの中だけで対応します。複数区間に分割した場合は、
「区間1・話者A」「区間2・話者A」のように区間を含む名前を付け、同一人物とは自動で判断しません。
必要に応じて、話者名のポップアップから1件・範囲・全体置換で修正してください。

確定した結果と編集は、接続した補助アプリの保存先へ残ります。
結果JSONには実際に選んだエンジンとモデル、提供先への送信、時刻の種類を記録します。
API経由の処理を「PC内だけで処理した結果」として扱いません。

## エラーと停止

接続時の準備状況は、環境変数・FFmpeg・FFprobeがそろっているかを確認します。
キーの有効性、残高、モデルの利用権限、提供先の稼働状況を実際の有料リクエストで確認することはありません。

認証・利用枠・応答形式などのエラーは、処理画面へ表示します。
APIへの自動再送や別モデルへの自動切り替えはしません。必要に応じて設定を直し、利用者が再実行します。
停止操作は実行中の接続処理を止め、次の音声区間の送信を防ぎます。
既にAPI提供先が受け付けたリクエストの処理・課金を取り消すものではありません。

この実装の検証は、API応答の試験データと設定・時刻変換・停止の動作確認で行います。
利用者のAPIキーによる実送信や、APIアカウントの利用枠確認は含みません。
