param(
  [string]$RepoRoot = "C:\Users\Administrator\.zcode\workspace\default\llm-router"
)

$ErrorActionPreference = "Stop"
$Relay = Join-Path $RepoRoot "relay"

# 1) 精简版 → public/m/
Push-Location (Join-Path $Relay "client")
try {
  cmd /c "npm run build" | Out-Host
} finally {
  Pop-Location
}

# 2) 完整版 → public/index.html（内嵌 token 注入脚本，静态页也能带鉴权）
$utf8 = New-Object System.Text.UTF8Encoding($false)
$wrapper = @'
<script>
(function () {
  var q = new URLSearchParams(location.search);
  var t = q.get("t") || q.get("token") || localStorage.getItem("llm-relay-token") || "";
  if (!t) return;
  try { localStorage.setItem("llm-relay-token", t); } catch (e) {}
  var of = window.fetch;
  window.fetch = function (u, o) {
    try {
      var x = new URL(u, location.origin);
      if (x.origin === location.origin && !x.searchParams.has("token")) x.searchParams.set("token", t);
      return of(x.toString(), o);
    } catch (e) { return of(u, o); }
  };
  var OW = window.WebSocket;
  window.WebSocket = function (u, p) {
    try {
      var x = new URL(u);
      if (x.origin === location.origin) x.searchParams.set("token", t);
      return new OW(x.toString(), p);
    } catch (e) { return new OW(u, p); }
  };
})();
</script>
'@
$src = Join-Path $RepoRoot "webui.html"
$html = [System.IO.File]::ReadAllText($src)
if ($html -match "<head[^>]*>") {
  $html = [regex]::Replace($html, "<head[^>]*>", { param($m) $m.Value + $wrapper }, 1)
} else {
  $html = $wrapper + $html
}
[System.IO.File]::WriteAllText((Join-Path $Relay "public\index.html"), $html, $utf8)

# 3) 缓存头
$headers = @"
/assets/*
  Cache-Control: public, max-age=31536000, immutable

/m/assets/*
  Cache-Control: public, max-age=31536000, immutable

/
  Cache-Control: public, max-age=60
"@
[System.IO.File]::WriteAllText((Join-Path $Relay "public\_headers"), $headers, $utf8)

Write-Output "built: public/index.html (full) + public/m/ (thin)"
