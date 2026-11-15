# クライアント向けモデル情報（v0.1.10）

本APIが返す利用可否・導入状態・参考速度はサーバー側の情報です。ブラウザー内での推論への対応を示すものではありません。

`GET /api/health` の `capabilities.modelInformation=true` と `modelInformationUrl=/api/models` で対応を確認する。未対応の旧版ではこのURLを前提にしない。

`GET /api/models` は管理対象のローカルモデルをすべて返す読み取り専用API。未導入・未対応・ポリシー対象外も含む。サーバーの既存の信頼LAN/Origin/Bearer規則を使い、local/lan構成では接続キーを手入力させない。Bearer構成では従来どおり認証が必要。読み取りによる導入・モデルロード・推論は発生しない。

| 項目 | 意味 |
|---|---|
| schemaVersion | 情報APIのスキーマ版。現在1。認識APIのprotocolVersion=3とは別 |
| serverVersion / generatedAt | 応答元の版 / 応答生成時刻（モデルの最終点検時刻ではない） |
| models | 保有・導入可能な管理対象。補助モデルと対象外モデルも含む |
| groups | 用途id・見出し・仕組み/用途/入出力などの説明ペア |
| referenceEnvironment | 保存済み参考測定のCPU・算出方法・注意事項。接続先PCの現在のハードウェア情報ではない |

モデル行の `id` は `engine/model`。`modelInstalled` は重みの保有、`runtimeReady` はランタイムの準備、`available` は現在の点検に基づく利用可否。`supported`、`supportStatus`、`availability`、`managementState` を併せて使い、不足・未確認・再起動待ち・対象外を区別する。未確認の値をtrueとして扱わない。状態は起動時や導入/削除/再点検で更新し、GETごとにディスク再走査しない。

`usage` は `transcription` / `diarization` / `vad` / `integrated`。`guidance` は `time`（比較条件付き参考時間）、`quality`（精度の傾向）、`bytes`（保存容量の参考バイト数）、`ram` / `vram`（概算・公式参考などの区別を含む説明）、`source`（資料）。保存容量は現在のディスク使用量の実測ではない。

`referenceSpeed` は未測定の場合 `null`。既測定の場合は以下を保持する。

- `charactersPerSecond`: 各回の正規化出力文字数 ÷ 処理秒数を求めた後の3回の中央値。`unit=characters/s`。文字数はNFKC正規化後、Unicode分類P/Z/S（句読点・区切り・記号）を除く。この集計では改行を含む。
- `characters` / `runSeconds`: 出力文字数・処理所要秒数それぞれの中央値。**この2値を割って参考速度を再計算しない。** 待機時間を含めない。
- `cpu` / `device` / `serverVersion` / `condition`: 測定CPU・処理方法・当時の版・音声/ロード/分離などの条件。
- `referenceDate` / `repetitions`: 測定日 / 回数。`processingMinSeconds` / `processingMaxSeconds` は3回の処理時間の範囲。
- `loadReadySeconds`: 事前の常駐準備時間（ワーカー起動と0.2秒間隔の完了確認を含む）。OSキャッシュは消去していない。`loadIncludedInProcessing=true` のNemotron ASRは区間ごとのロードを処理に含み、事前ロード秒数は `null`。
- `kind=historical_measurement` / `definition` / `caution`: 保存済み測定であり、現在の接続先の速度保証ではない。GPUは未測定。

未リリース追加分では、2026-11-13の共通5分音声（300.032秒）・i5-12500T・CPU 4スレッド・日本語・話者分離なし・12 ASRモデル各3回の結果へ更新した。VADの発話推定は51.752秒。素材と基本条件を共通にした比較で、認識器固有のVAD・デコーダー等は異なる。旧4件の異なる条件での数値を混在させない。

`referenceFiller`（追加）は未測定・文字出力なしでは `null`。測定済みの0件は数値 `0` を保持する。

- `candidatesMedian` / `candidatesMin` / `candidatesMax`: フィラー候補回数の中央値 / 最小 / 最大。
- `candidatesPer1000Characters`: 各回の1,000出力文字あたりの候補回数の中央値。中央値同士から再計算しない。
- `kind=output_tendency` / `audioVerified=false`: 辞書＋Janome/IPADICの語境界・品詞条件による出力傾向。音声との一致、認識精度、原音忠実性の評価値ではない。
- `referenceDate` / `repetitions` / `condition` / `serverVersion` / `definition` / `caution`: 評価条件と限界。

`referenceEnvironment` にCPUのコア/論理スレッド数・RAM、測定日・版・回数、素材と実行条件、ロード/処理の区別、文字数とフィラーの定義、評価辞書のSHA256・解析器を保持する。管理画面はこの情報を設定ポップアップに表示する。未リリースのソース変更であり、導入済みサーバーは自動で更新されない。

応答には本文・音声・job ID・絶対パス・認証値を含めない。UIとAPIは同じ `dist/model-metadata.json` を使用する。

従来の `GET /v1/models` は選択候補を返す契約を維持する。ローカル管理対象と一致する候補には `x_meeting.modelInfo` として上記モデル行を追加する。クライアントは新しい全件一覧をそのまま実行選択肢にせず、従来の候補と `available` 等を参照する。未導入でも初回取得を許す従来エンジンの候補を、保有済みだと誤表示しない。外部APIモデルは保有ローカルモデル一覧の対象外。

