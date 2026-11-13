param(
  [string]$Token = "",
  [string]$BaseUrl = "https://relay.zhoush.dpdns.org"
)

$ErrorActionPreference = "Stop"
$BT = "D:\android-sdk\build-tools\android-14"
$AJ = "D:\android-sdk\platform\android-34\android.jar"
$Work = "D:\android-build"
$Root = Split-Path -Parent $MyInvocation.MyCommand.Path

if (-not $Token) { $Token = (Get-Content "D:\llm-router-target\relay-token.txt" -Raw).Trim() }
$StartUrl = "$BaseUrl/?t=$Token"

Remove-Item $Work -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$Work\src\com\llmrouter\mobile", "$Work\classes", "$Work\dex" | Out-Null

# 注入启动地址
$java = Get-Content "$Root\src\com\llmrouter\mobile\MainActivity.java" -Raw
$java = $java.Replace("__START_URL__", $StartUrl)
$utf8 = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllText("$Work\src\com\llmrouter\mobile\MainActivity.java", $java, $utf8)
Copy-Item "$Root\AndroidManifest.xml" "$Work\AndroidManifest.xml"

Push-Location $Work
try {
  # 1) 只打包 manifest，不带资源
  & "$BT\aapt2.exe" link -o app-unsigned.apk -I $AJ --manifest AndroidManifest.xml `
      --min-sdk-version 26 --target-sdk-version 34
  if ($LASTEXITCODE -ne 0) { throw "aapt2 link failed" }

  # 2) 编译 Java
  & javac -source 8 -target 8 -bootclasspath $AJ -d classes src\com\llmrouter\mobile\MainActivity.java
  if ($LASTEXITCODE -ne 0) { throw "javac failed" }

  # 3) dex（先打 jar，d8 在 Windows 下不递归目录）
  & jar cf app-classes.jar -C classes .
  if ($LASTEXITCODE -ne 0) { throw "jar cf failed" }
  & "$BT\d8.bat" --lib $AJ --min-api 26 --output dex app-classes.jar
  if ($LASTEXITCODE -ne 0) { throw "d8 failed" }

  # 4) 把 classes.dex 塞进 APK
  & jar uf app-unsigned.apk -C dex classes.dex
  if ($LASTEXITCODE -ne 0) { throw "jar uf failed" }

  # 5) 对齐 + 签名
  & "$BT\zipalign.exe" -f 4 app-unsigned.apk app-aligned.apk
  if ($LASTEXITCODE -ne 0) { throw "zipalign failed" }

  $ks = "D:\android-build\debug.keystore"
  if (-not (Test-Path $ks)) {
    & keytool -genkeypair -keystore $ks -alias androiddebugkey -storepass android -keypass android `
        -dname "CN=Android Debug,O=Android,C=US" -keyalg RSA -keysize 2048 -validity 10000
    if ($LASTEXITCODE -ne 0) { throw "keytool failed" }
  }

  & "$BT\apksigner.bat" sign --ks $ks --ks-pass pass:android --key-pass pass:android `
      --out llm-router-mobile.apk app-aligned.apk
  if ($LASTEXITCODE -ne 0) { throw "apksigner failed" }

  & "$BT\apksigner.bat" verify --print-certs llm-router-mobile.apk
  if ($LASTEXITCODE -ne 0) { throw "apksigner verify failed" }
} finally {
  Pop-Location
}

$out = "C:\Users\Administrator\.zcode\workspace\default\llm-router\desktop\dist\llm-router-mobile.apk"
Copy-Item "$Work\llm-router-mobile.apk" $out -Force
Get-Item $out | Select-Object FullName, @{n = "MB"; e = { [math]::Round($_.Length / 1MB, 2) } }
