# 処理方法とGPU比較（v0.1.5）

## 確認した現実装

- 「GPU候補」はデバイス/バックエンドの列挙が成功した状態。選択モデルの実ロード・実推論成功や速度・精度まで保証した意味ではない。WhisperはCUDA、speech.cppはそのGPU経路、同梱NeMo-Speech.cppのNemotron ASRはVulkan。起動時のWhisper computeType候補をNemotron ASRへ適用しない。
- v0.1.5のautoは、各エンジンがGPU候補を返せばGPUを先に実行し、推論中に失敗した場合は同じジョブをCPUで1回だけ再処理する。GPU候補がなければ最初からCPU。外部APIはAPI側で処理する。速さを比較するベンチマーク自動選択ではない。v0.1.4以前のNemotron ASR/単独話者分離のCPU固定から変更した。Nemotron ASRはautomaticDevice=gpu/cpuとautomaticFallback=cpuを広告する。
- 自動GPUでは、途中失敗時の字幕重複・音声文脈/話者番号の継ぎはぎを防ぐため、字幕を専用のSQLite暫定表へ保持する。成功時に一括確定し、失敗時は暫定結果を破棄してCPUで全範囲を処理する。進捗は表示するが、GPU試行中の字幕本文は完了まで未確定。中止/異常終了ではその時点の有効な暫定字幕を部分結果として保持する。
- 明示GPU/CPUは指定を維持し、GPU指定の失敗を勝手にCPUへ変更しない。中止/停止、GPU推論前のメディア検証失敗では再試行しない。CPUも失敗すると両方の理由でerror。モデル管理のロードautoもGPU優先/CPU切替で、元のモデル/デコード設定は変更しない。
- health.capabilities.automaticGpuFallback=true。metadata.executionDeviceは選択先（切替後cpu）、gpuFallback=true、gpuFallbackReason/gpuFallbackAt/gpuAttemptSecondsを記録し、actualDeviceは最後の実行先。gpuAttemptProvisional/gpuAttemptSegmentCountは暫定状態の診断値。runSecondsは最初のstartedAtから、失敗GPU試行とCPU再処理の両方を含み、GPU待機はresourceWaitSecondsへ分離する。同梱画面は「GPUからCPUへ切替」を表示する。
- Nemotron ASRの明示GPUは`--device vulkan:0`、CPUは`--device cpu`。モデルは同じ固定Q8_0。最大300秒の音声ごとにCLIを新規起動し、モデルを都度ロードする。`--no-batching --no-warmup`を付ける。realtime枠は最大30秒、内部`--stream`を使い、file枠と処理条件が異なる。

## M4000とXeonの速度・精度差

別PCのジョブID・結果JSON・ログは所在不明との回答のため、実測の原因は未確定。同一音声/範囲/言語/分割/ランタイム/モデル/話者分離/同時ジョブだったかも未照合。

M4000はMaxwell世代・8GB GDDR5・帯域192GB/s、Xeon E5-1680 v4は8コア16スレッド。GPUという分類だけでCPUより速いとは判断できない。Vulkanの初期化・転送・カーネル実装と、分割ごとのモデルロード費用でCPUが速くなる可能性がある。これは可能性であり、この実機の測定結果ではない。v0.1.4のrunSecondsはロード等を含む時間なので、待機時間や純粋な推論時間と分けて比較する。

精度は「CPUだから良い」「GPUが古いから悪い」とは説明できない。同じ重みと条件でもCPU/Vulkanの数値演算差がデコード結果に影響する可能性や、Vulkanバックエンドの不具合の可能性は残る。今回そのどちらも確認していない。言語指定、処理枠、分割、モデル等が違う可能性を先に除外し、正解文と文字誤り率等で比較する必要がある。単一の表示結果だけからCPU優位と一般化しない。

CUDAの再導入だけでは、このNemotron ASRのVulkan経路の問題は解決しない。Whisper/CTranslate2のCUDA互換性と混同しない。比較記録にはdevice（要求）、metadata.executionDevice（受付時選択）、actualDevice（起動先）、sourceModel/モデル、language、workload、範囲、runSeconds/waitSeconds、サーバー/ランタイム版と結果を残す。actualDeviceは要求したランタイム引数の記録でありGPUの稼働率を測定した証明ではない。

## 一次資料

- [NVIDIA NeMo-Speech.cpp v0.3.0 CLI](https://github.com/NVIDIA/NeMo-Speech.cpp/blob/v0.3.0/docs/cli.md): CPU/Vulkan/CUDAの指定、doctor、benchのモデルロード/ウォームアップと処理時間の区別。
- [NVIDIA ベンチマーク](https://github.com/NVIDIA/NeMo-Speech.cpp/blob/v0.3.0/BENCHMARK.md): 主な掲載GPUはRTX4090/GB10、M4000/日本語Nemotron3.5の比較証拠ではない。
- [NVIDIA M4000仕様](https://images.nvidia.com/content/pdf/quadro/data-sheets/75509_DS_NV_Quadro_M4000_US_NV_HR.pdf)
- [Intel Xeon E5-1680v4仕様](https://www.intel.com/content/www/us/en/products/sku/92992/intel-xeon-processor-e51680-v4-20m-cache-3-40-ghz/specifications.html)
