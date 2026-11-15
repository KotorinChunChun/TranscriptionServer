# Nemotron 3.5 ASR Streaming 0.6B（v0.1.2）

文字起こし専用の多言語モデル `nvidia/nemotron-3.5-asr-streaming-0.6b` を追加します。日本語は公式のtranscription-ready対象です。一体型のNemotron 3.5 Transcribe 30B-A4B、分離専用のNemotron 3 Diarizationとは別のモデルです。

## 実行環境と導入

- HTTP/CLI/ジョブ管理はRustのまま。NeMo-Speech.cpp 0.3.0のC++ CLIを認識子プロセスとして使い、Python環境は追加しません。C++のHTTPサーバーやマイク捕捉コマンドは起動しません。
- Windows配布物はCPU/Vulkanランタイムを同梱。ソース開発では`pwsh -File setup-nemo.ps1`で固定版・SHA256を検証して準備します。PATH/ドライバーは変更しません。
- 管理画面の「Nemotron ASR · 多言語」からモデルをインストールします。Q8_0重みは742,090,464バイト。Rustで固定revision/サイズ/SHA256を照合して保存し、取得中止・容量超過・不整合時には未完成品を導入済みにしません。
- 重みは設定ディレクトリー内の`models/nemotron-asr`へ保存します。起動や再点検で取得しません。未導入時は実行一覧から除外し、ジョブ受付は503。管理画面には未導入として表示します。
- ロード方式は**認識時のロード/解放**です。手動常駐ロード・要求間のモデル再利用を実装済みと表示しません。旧speech.cppのモデル・キャッシュ・常駐機能はそのままです。

## 接続契約

利用可能時の`GET /v1/models`のIDは次です。

```text
nemotron-asr/nemotron-3.5-asr-streaming-0.6b
```

`x_meeting.engineId=nemotron-asr`、`modelId=nemotron-3.5-asr-streaming-0.6b`、`task=transcription`、`integratedDiarization=false`。`supportedLanguages`に対象localeとautoを返します。モデルの導入後に管理画面で再点検すると一覧へ反映されます。

```http
POST /api/jobs?engine=nemotron-asr&model=nemotron-3.5-asr-streaming-0.6b&device=cpu&diarize=false&language=ja-JP&clientId=Example-ai-room&workload=file&name=audio.wav
```

既存のContent-Length付きraw音声/動画・操作トークン・状態/segments/中止/保存契約を維持します。`/v1/audio/transcriptions`でもモデルIDを指定でき、language=jaまたはja-JPを日本語として受け付けます。言語省略はja-JP、autoは言語自動判定。英語en-USなど公式のtranscription-ready/broad-coverage localeを指定できます。追加学習が必要なadaptation-ready localeをそのまま対応済みとして広告しません。

## リアルタイムと時刻

- v0.1.6から、録音ファイルは無音（silero-vad、余白0.2秒・最小無音0.3秒）と話者の切り替わりで区切り、1回最大30秒の塊で認識します。塊同士の間の無音は送らず、塊の中の短い無音はそのまま送ります。今の塊が4秒未満なら話者が替わっても切りません。無音のない1発話は文脈を保つため30秒で切らず、300秒までは1つの塊にします（300秒超だけ300秒ごと）。塊ごとにモデルを起動・解放し、出力時刻へ元音声の位置を一度だけ加算します。
- 話者の切り替わりを使うのは、話者分離を認識より先に実行する直列時（CPU認識）だけです。Vulkan認識と並行する場合は無音だけで区切ります。speech.cppまたは導入済みのsilero-vadがない場合は自動取得せず、v0.1.5までと同じ300秒固定分割で処理します。metadataの`audioChunking`は`vad_speaker`/`vad`/`fixed`/`realtime`、`audioChunkMaxSeconds`・`audioChunkCount`・`continuousSpeechMaxSeconds`で方式を確認できます。`audioChunkSeconds`は塊の上限秒数（vad系30、固定300）です。
- `workload=realtime`は既存契約どおり30秒以下の音声ジョブです。この場合C++ CLIの`--stream`を使い、音声内でキャッシュ付きストリーミング認識を行います。
- **WebSocketによる連続音声入力や、要求間のキャッシュ継続は提供しません。** 能力は`nativeStreaming=true`、`streamingTransport=false`、`realtimeMode=bounded_jobs`。短区間の再ロード時間を含む遅延は利用アプリ側でも検証してください。
- JSONのalignment単位は日本語では句・文になる場合があります。返されたstart/endを保持し、正確な単語境界を新たに作りません。本文とalignmentが一致しない場合やゼロ長のalignmentは本文を保持し、`nemo_chunk_estimated`と要確認フラグで概算範囲を明示します。

## 話者識別とGPU

- ASRモデル自体は話者識別をしません。`diarize=true`では既存の取得済みNemotron 3 DiarizationをCPUで別途実行し、重なり・被覆率に基づいて既存契約の話者候補/要確認情報を付けます。未導入の分離器へ自動変更しません。
- v0.1.5のautoはVulkan GPU候補があればGPUを優先し、実行失敗時にCPUへ1回切り替えます。候補なしはCPU、明示gpuの失敗はエラーです。ASRの`actualDevice`はcpuまたはvulkan:0、補助分離の`diarizationDevice`はcpuです。自動GPUの字幕確定と切替の記録は[処理方法](gpu-execution.md)を参照してください。
- このモデルのVulkan経路にCUDA/cuDNNは不要です。ただしM4000実機・ドライバー552.86とのVulkan動作/速度/メモリーは未確認です。WhisperのCUDA経路で不足しているSM5.2コードとは別の問題です。

公式資料: [モデルカード](https://huggingface.co/nvidia/nemotron-3.5-asr-streaming-0.6b)、[C++実行環境](https://github.com/NVIDIA/NeMo-Speech.cpp/tree/v0.3.0)、[CLI仕様](https://github.com/NVIDIA/NeMo-Speech.cpp/blob/v0.3.0/docs/cli.md)。
