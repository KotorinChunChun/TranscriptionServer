# LAN接続とBearer認証

## 信頼LANでキーを入力せずに使う

```pwsh
./TranscriptionServer.exe --lan --host 0.0.0.0 --port 8769 --no-browser
```

有効なプライベートIPv4インターフェースとサブネットを点検し、他PC用URLを表示する。0.0.0.0は待受用、127.0.0.1は操作しているPC自身なので、別PCには実IPを指定する。範囲を限定する場合は--allow-network <IPv4/CIDR>を繰り返す。

クライアントはキー空欄でGET /api/healthを呼び、appId=meeting-transcriber、protocolVersion=3、authMode=lan、tokenを確認する。変更操作はこのtokenをX-Local-Tokenへ自動設定する。ユーザー認証ではなく信頼LANでの共有利用であり、同じ履歴を参照する。

接続元サブネット、Host、Originを検証する。別の場所で配信したブラウザー画面には--allow-originで正確なoriginを追加する。更新管理は実ソケットの接続元がloopbackの場合だけ許可する。Forwardedヘッダーを信頼して更新操作を開放しない。

Windows FWはConfigure-LanFirewall.ps1 -Action Plan -LocalAddress <実IP> -AllowedNetwork <サブネット>で変更内容を確認できる。対象プログラム・Private・TCPポート・サブネットを確認して、Windows管理者で-Action Enableを実行する。全プロファイル/全プログラム/全送信元へ広げない。

TranscriptCutter Web版v0.2611.3以降は工程1の接続先URLを指定してキー空欄で接続する。古い保管版EXEは別PC対応済みとは扱わない。

## 同PC限定

--local --host 127.0.0.1を指定する。authMode=localとhealthのtokenを使う。機械外への待受に--localは使えない。

## Bearer認証の構成

外部公開用の--serverはLANとは別の明示モード。32文字以上の印字可能ASCII/空白なしのMEETING_SERVER_API_KEYを起動環境に設定する。値を引数・設定JSON・Git・URLへ入れない。

```pwsh
./TranscriptionServer.exe --server --host 0.0.0.0 --port 8769 --no-browser --allow-origin https://ui.example.com
```

このモードはhealth/履歴/モデル/変更操作を含むAPIへAuthorization: Bearer <接続キー>を要求し、healthはtokenを返さない。HTTPSは承認済みのリバースプロキシで終端する。プロキシはOrigin/Authorization/Rangeと必要なHTTPメソッドを転送する。任意の認証値付きリダイレクトを追わない。

メディア再生は認証済みmedia-link APIで発行する録音限定の期限付きURLを使い、サーバー接続キーをURLへ含めない。OpenAI等の提供先キーはサーバー接続キーと別の環境変数で扱う。

## モデル点検の意味

起動時は実行ファイル・ランタイム・対応モデル・ローカル重みの状態・CPU/GPU候補を点検する。重みのダウンロードと実認識は行わない。導入済みでも精度・GPU初期化・VRAM容量・API認証/利用枠が確認済みという意味ではない。
