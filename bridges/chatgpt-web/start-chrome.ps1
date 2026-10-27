<#
.SYNOPSIS
  Start a Chrome you control, with a DevTools endpoint the bridge can attach to.

.DESCRIPTION
  Sign-in is the one thing the bridge cannot do for you: Google refuses to sign
  you in inside a Chrome that Playwright launched ("This browser or app may not
  be secure"), and ChatGPT's CDN probes the same automation signal.

  Chrome started this way is a plain browser process with no automation flags,
  so Google sign-in works normally. Sign into ChatGPT in the window that opens,
  keep it open, then attach the bridge:

      node bridge.mjs --cdp http://127.0.0.1:9222

  Chrome 136+ only exposes the DevTools endpoint for a non-default profile, so
  this uses a dedicated profile directory. Sign in once; it persists there.

.PARAMETER Port
  DevTools endpoint port (default 9222).

.PARAMETER Profile
  Profile directory to use (default .chrome-cdp next to this script).

.EXAMPLE
  .\start-chrome.ps1
  .\start-chrome.ps1 -Port 9333 -Profile D:\chrome-swarm
#>
param(
  [int]$Port = 9222,
  [string]$Profile = (Join-Path $PSScriptRoot '.chrome-cdp')
)

$chrome = @(
  (Join-Path $env:PROGRAMFILES 'Google\Chrome\Application\chrome.exe'),
  (Join-Path ${env:PROGRAMFILES(x86)} 'Google\Chrome\Application\chrome.exe'),
  (Join-Path $env:LOCALAPPDATA 'Google\Chrome\Application\chrome.exe')
) | Where-Object { Test-Path $_ } | Select-Object -First 1

if (-not $chrome) {
  Write-Error 'Chrome was not found in the usual locations. Pass its path yourself:
  & "C:\path\to\chrome.exe" --remote-debugging-port=9222 --user-data-dir="..." https://chatgpt.com/'
  exit 1
}

if (-not (Get-Process chrome -ErrorAction SilentlyContinue)) {
  Write-Warning 'A DevTools endpoint needs the profile to be free: close any Chrome ' +
    'windows using this profile first (the default profile is a different one, so ' +
    'your normal Chrome is untouched).'
}

Write-Host "Chrome profile: $Profile"
Write-Host 'Sign into ChatGPT in the window that opens, keep it open, then run:'
Write-Host "  node bridge.mjs --cdp http://127.0.0.1:$Port"

Start-Process -FilePath $chrome -ArgumentList @(
  "--remote-debugging-port=$Port",
  "--user-data-dir=$Profile",
  '--no-first-run',
  '--no-default-browser-check',
  'https://chatgpt.com/'
)
