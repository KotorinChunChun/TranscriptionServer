# インストーラーの署名

インストーラーのビルドには利用者が管理するコード署名証明書が必要です。

```powershell
$env:TRANSCRIPTION_SIGNING_THUMBPRINT = '<証明書の40桁の拇印>'
$env:TRANSCRIPTION_SIGNING_SUBJECT = '<証明書のSubject全文>'
pwsh -NoProfile -File scripts/sign-installer.ps1 -Mode Check
```

証明書は `Cert:\CurrentUser\My` に秘密鍵付きで保存し、コード署名用途・有効期限・Windows上の信頼を確認してください。秘密鍵やPFXをソース・配布物へ保存しないでください。

`scripts/build-release.py` は署名の準備確認、Inno Setupによる生成、SHA256署名、RFC3161タイムスタンプ、署名検証の順に処理します。証明書を自動選択・新規発行したり、信頼ストアへ自動登録したりしません。

```powershell
pwsh -NoProfile -File scripts/sign-installer.ps1 -Mode Sign -Path 'C:\output\TranscriptionServer-setup.exe'
pwsh -NoProfile -File scripts/sign-installer.ps1 -Mode Verify -Path 'C:\output\TranscriptionServer-setup.exe'
```

未設定・不一致・期限切れ・署名やタイムスタンプの失敗は、終了コード非0で停止します。Windows SDKのSignToolとInno Setupは別途必要です。
