<#
.SYNOPSIS
  Start several Chrome browsers, each with its own profile and DevTools port, as
  a pool of access points for the bridge.

.DESCRIPTION
  One signed-in browser holds a handful of chats at once before the site slows
  them all down. For more parallel agents, run more browsers: each one is a
  separate Playwright access point, and the bridge hands new sessions to the one
  with the fewest open chats and skips one that has died.

  Every browser needs ChatGPT signed in once. Sign in in each window that opens
  (the same account is fine); the login persists in that browser's profile. Then
  attach the bridge to all of them, which this script prints for you.

  Ports already answering are left alone, so re-running it only starts what is
  missing. Profiles live under -Root, apart from your normal Chrome.

.PARAMETER Count
  How many browsers (default 3).

.PARAMETER BasePort
  First DevTools port; the rest follow (default 9230, clear of start-chrome.ps1's 9222).

.PARAMETER Root
  Directory holding one profile per browser (default .chrome-pool next to this script).

.EXAMPLE
  .\start-pool.ps1 -Count 3
#>
param(
  [ValidateRange(1, 8)][int]$Count = 3,
  [int]$BasePort = 9230,
  [string]$Root = (Join-Path $PSScriptRoot '.chrome-pool')
)

$chrome = @(
  (Join-Path $env:PROGRAMFILES 'Google\Chrome\Application\chrome.exe'),
  (Join-Path ${env:PROGRAMFILES(x86)} 'Google\Chrome\Application\chrome.exe'),
  (Join-Path $env:LOCALAPPDATA 'Google\Chrome\Application\chrome.exe')
) | Where-Object { Test-Path $_ } | Select-Object -First 1

if (-not $chrome) {
  Write-Error 'Chrome was not found in the usual locations.'
  exit 1
}

function Test-Endpoint([int]$Port) {
  try {
    $null = Invoke-WebRequest -Uri "http://127.0.0.1:$Port/json/version" -UseBasicParsing -TimeoutSec 2
    return $true
  } catch { return $false }
}

$endpoints = @()
for ($i = 0; $i -lt $Count; $i++) {
  $port = $BasePort + $i
  $profile = Join-Path $Root ("browser-{0}" -f ($i + 1))
  $endpoints += "http://127.0.0.1:$port"
  if (Test-Endpoint $port) {
    Write-Host "port $port already answers; leaving it alone"
    continue
  }
  New-Item -ItemType Directory -Force -Path $profile | Out-Null
  Write-Host "starting browser $($i + 1) on port $port (profile $profile)"
  Start-Process -FilePath $chrome -ArgumentList @(
    "--remote-debugging-port=$port",
    "--user-data-dir=$profile",
    '--no-first-run',
    '--no-default-browser-check',
    'https://chatgpt.com/'
  )
}

$cdp = ($endpoints | ForEach-Object { "--cdp $_" }) -join ' '
Write-Host ''
Write-Host 'Sign into ChatGPT in every window that opened, keep them open, then run:'
Write-Host "  node bridge.mjs --no-api $cdp"
Write-Host 'and tell Swarm how many requests it may send at once (about 3 per browser):'
Write-Host "  `$env:SWARM_BROWSER_CONCURRENCY = $($Count * 3)"
