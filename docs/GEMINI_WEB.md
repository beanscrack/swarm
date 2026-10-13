# Gemini web bridge

Swarm can use `https://gemini.google.com/app` as a reasoning provider. A local
Node bridge controls Chrome; each Swarm provider session gets its own Gemini
chat tab. Swarm remains responsible for planning, tool execution, approvals,
budgets, checkpoints, and the shared workspace.

Current verification: the bridge and full agent loop completed live tasks with
Gemini Auto, including two concurrent worker chats and file write/read-back.
The signed-in account shown in this environment did not have 4 Argon access;
the model picker requested an upgrade. Google rejected sign-in to the Chrome
window launched by automation. The local desktop's automatic approval review
also blocked starting a normal Chrome window with a debugging port, so the
`--cdp` connection mode remains unverified here. A live 4 Argon run is still
required before treating this provider as complete.

## Start the browser

Install the bridge dependency once:

```powershell
cd bridges/gemini-web
npm ci
```

If your environment permits Chrome's local debugging endpoint, start a normal
Chrome window with an isolated profile and local debugging port:

```powershell
$geminiProfilePath = Join-Path (Resolve-Path ../..).Path '.swarm/gemini-cdp'
Start-Process -FilePath 'C:\Program Files\Google\Chrome\Application\chrome.exe' -ArgumentList @('--remote-debugging-address=127.0.0.1', '--remote-debugging-port=9222', "--user-data-dir=$geminiProfilePath", 'https://gemini.google.com/app')
```

Sign in there and confirm that the model picker lets the account select **4
Argon**. Start the bridge from `bridges/gemini-web`:

```powershell
npm start -- --cdp http://127.0.0.1:9222
```

Keep Chrome and the bridge terminal running. The bridge attaches to Chrome's
documented debugging endpoint and leaves existing tabs alone. Only use this
port on your own computer; Chrome's debugging endpoint gives local clients
control of that isolated profile. The bridge server binds only to
`127.0.0.1:8765`; `http://127.0.0.1:8765/health` reports readiness.

For an account already signed in to a separate automation profile, the bridge
can also launch Chrome itself with `--profile ../../.swarm/gemini-chrome`.
Google may refuse sign-in to a browser launched by automation. Do not use that
mode to sign in if Google displays a browser security warning.

From the repository root, start a Swarm run:

```powershell
cargo run -p swarm-cli -- run "create hello.txt and verify it" --provider gemini-web --agents 3
```

`gemini-web` selects `4 Argon`, uses the local bridge, allows up to four
simultaneous browser sessions, and treats access as a subscription. Use
`--model Auto` to test the transport with the account's default Gemini model.
Use `--base-url http://127.0.0.1:PORT` to point to another local bridge.

If the account cannot select Argon, the run fails with an account access
message. Selecting Argon on an account without access does not start a paid
upgrade; the bridge dismisses the upgrade prompt.

## Multiple chat access points

Within one Chrome profile, each agent owns a separate chat tab. To distribute
sessions across different Google accounts in one Swarm run, start a separate
normal Chrome window for each isolated profile and debugging port, then pass
`--cdp` more than once when starting the bridge:

```powershell
npm start -- --cdp http://127.0.0.1:9222 --cdp http://127.0.0.1:9223
```

Sign in to each Chrome window. The bridge assigns new sessions to profiles in
round-robin order; every account must be able to select the configured model.
The `/health` response reports the number of access points. You can also run
separate bridges on different ports and point separate Swarm runs at them with
`--base-url http://127.0.0.1:PORT`.

## Tool protocol and limits

Gemini web chat does not expose native function calls to this bridge. Swarm
sends its tool catalog in the chat prompt and asks for a fenced `swarm` JSON
block when a tool is needed. The provider parses the block and passes the
request through Swarm's existing sandbox and approval checks. Tool results go
back to that agent's Gemini chat.

The web interface does not report token usage to the bridge, so Gemini runs
show zero reported tokens and token budgets cannot bound them. Inference and
step budgets still apply. Google account limits also apply, and the web UI can
change its labels or markup; update the bridge selectors when that happens.
