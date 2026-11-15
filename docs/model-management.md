# サーバー管理

v0.1.10では既定ページ `/` をサーバー管理専用とし、同梱の文字起こし画面と`/transcribe`を廃止します。文字起こしはTranscriptCutterなどの利用アプリから同じ8769番のAPIを使います。更新設定・確認・適用は管理画面右上の「設定・更新」ポップアップに集約します。以下の旧v0.2.0でのGPU検査は履歴で、そのReleaseは番号訂正により撤回済みです。現配布のWhisperワーカーもM4000向け再ビルドはしていません。

モデルのインストール・ロード・アンロード・再点検・中止は、同PCと許可された信頼LANで利用できます。変更にはhealthから自動取得する一時操作トークンを使い、キーの手入力は不要です。明示Bearer構成では従来の接続キーを使います。アプリの更新管理・停止は引き続きサーバーPC限定です。

インストールはディスクへの取得、ロードは選択枠へのメモリ常駐です。v0.1.8ではアンロードも選択枠（lane=0ファイル/lane=1逐次音声）だけを解放し、他枠の同じモデルは保持します。重み・録音・字幕は削除しません。処理/転送/更新中には競合操作を拒否します。

v0.1.8の画面の処理方法は初期autoで、再描画後も選択を保持します。補助モデル/Nemotron ASRは処理時だけロード・解放するため、無効な常駐操作を省きます。CPU/GPU等の実行候補はクライアントへAPIで通知。19GB一体型は既定無効を維持し、明示有効化した環境のみ導入・実行します。

WhisperとネイティブASRは手動ロードしたモデルをその枠の認識へ再利用します。手動ロードは自動アイドル解放の対象外です。CPUの枠ごとに1モデル、GPU候補はメモリー競合を避けて未使用のキャッシュを解放します。Silero VAD/Nemotron話者分離は補助処理の都度ロードし、終了時に解放します。

v0.1.7の配布物には認識ランタイム・FFmpeg・モデルを含めません。Whisper専用環境、speech.cpp、NeMo-Speech.cpp、FFmpeg/FFprobeを管理画面の導入ボタンから専用領域へ準備できます。導入後は再起動待ちを表示し、既存処理を停止しません。Whisperワーカー欠落時も専用実行環境の導入へ案内します。[実行環境の管理](runtime-management.md)を参照してください。モデル重みは配布物へ含めません。

v0.1.2の [Nemotron ASR](nemotron-asr.md) は別のNeMo-Speech.cppランタイムを同梱し、重みは管理画面で導入します。認識時にロード/解放する方式で、手動常駐ロードの対象外です。旧speech.cppモデルとPython環境を置き換えません。

## 保存先の旧名移行

旧既定の `%LOCALAPPDATA%\HaasMeetingWorkbench` を使用している場合、次回起動時に `%LOCALAPPDATA%\TranscriptionServer\data` へ整合バックアップと媒体照合を行います。旧フォルダーはバックアップとして保持します。明示の別保存先は変更しません。新旧両方に別データがある場合は自動統合しません。旧サーバーを正常終了してから更新してください。

## Quadro M4000

古いGPUでは、CUDAの導入だけでなく、ワーカーとライブラリが対象GPUのCompute Capabilityに対応していることを確認してください。未対応の場合はCPU処理を選択します。

M4000はMaxwell/CC5.2で、FP16を前提にできません。v0.2.0はCTranslate2の対応形式を照会し、FP32だけのGPUも候補に含めます。CUDAが検出できたことと実モデルのロード/認識成功は別です。

- Maxwellを対象とするCUDA 12系と、その構成に対応するNVIDIAドライバー/ライブラリーが必要です。CUDA 13を一律に導入しません。
- cuDNNを必要とする構成では、Maxwellを含む対応表の版を選びます。最新版へ無条件に置換しません。今回ドライバー/CUDAの導入・変更は行っていません。
- FP32はメモリー使用量が増えます。M4000の8GBではまずTiny/Base/Smallを確認し、大型モデルを無条件にロードしないでください。
- 当方にM4000実機がないため、速度/VRAM/ドライバーとの動作は未受入です。Vulkan対応のspeech.cppも別のGPU経路として候補になりますが、同様に実機確認が必要です。

参照: [NVIDIA世代一覧](https://developer.nvidia.com/cuda/gpus/legacy)、[CT2計算形式](https://opennmt.net/CTranslate2/quantization.html)、[CUDA世代対応](https://docs.nvidia.com/datacenter/tesla/drivers/cuda-toolkit-driver-and-architecture-matrix.html)、[cuDNN 9.10.2対応表](https://docs.nvidia.com/deeplearning/cudnn/backend/v9.10.2/reference/support-matrix.html)。
