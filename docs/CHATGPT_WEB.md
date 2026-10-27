# ChatGPT web bridge

Swarm can use `https://chatgpt.com/` as a reasoning provider. A local Node
bridge controls Chrome; each Swarm provider session gets its own ChatGPT chat
tab. Swarm remains responsible for planning, tool execution, approvals,
budgets, checkpoints, and the shared workspace. ChatGPT's own agent features
are not used: it only supplies text.

Why the bridge reads ChatGPT's conversation API: the reply comes back as clean
markdown with the model slug and token counts, so Swarm's budget ledger records
real usage instead of zeros. The DOM reader is the fallback (and `--no-api`
forces it) for the case where that endpoint is unavailable to the tab.

## Current verification

Verified offline against `fixture.mjs`, a stand-in for chatgpt.com: sessions get
independent tabs, replies arrive over the API path with `model` and token usage,
repeated `request_id` values replay the same reply, fenced Swarm tool JSON
survives both the API path and the DOM path (ChatGPT wraps code blocks in a
header row and a copy button), a plan-gated model returns 403, an unknown model
returns 422, a site limit banner returns 429, a sign-in modal returns 401 within
seconds instead of waiting out the response timeout, and `--no-api` completes
the same loop from rendered DOM. Run them with `npm test` (5 tests).

The end-to-end path `swarm run --provider chatgpt-web` -> bridge -> headless
Chrome -> fixture (tool call, sandbox write, tool result, completion, usage in
the ledger, transcript replay through `swarm logs --transcript`) is exercised by
`npm run e2e`.

Not verified here: a live chatgpt.com run. Two gates block an unattended first
run, both observed from this machine: chatgpt.com answers an automated headless
browser with a Cloudflare challenge, and Google refuses to sign in a Chrome that
Playwright launched. Sign in once through `npm run chrome` (plain Chrome, no
automation), then attach the bridge with `--cdp`. Treat that first real run as a
smoke test: one agent, one task, browser visible, and watch the Chrome tab.

## Start the browser

Install the bridge dependency once:

```powershell
cd bridges/chatgpt-web
npm ci
```

Launch Chrome with an isolated profile and a local debugging port, sign in to
chatgpt.com in that window, and confirm the plan can select the model you want:

```powershell
$chatgptProfilePath = Join-Path (Resolve-Path ../..).Path '.swarm/chatgpt-cdp'
Start-Process -FilePath 'C:\Program Files\Google\Chrome\Application\chrome.exe' -ArgumentList @('--remote-debugging-address=127.0.0.1', '--remote-debugging-port=9222', "--user-data-dir=$chatgptProfilePath", 'https://chatgpt.com/')
```

Start the bridge from `bridges/chatgpt-web`:

```powershell
npm start -- --cdp http://127.0.0.1:9222
```

Sign in in that window, then keep Chrome open. `npm run chrome` is the same
launch with the profile path chosen for you:

```powershell
npm run chrome        # opens Chrome at http://127.0.0.1:9222, profile .chrome-cdp
npm start -- --cdp http://127.0.0.1:9222
```

Use this route for the **first** sign-in. Google refuses to authenticate a
Chrome that Playwright launched ("This browser or app may not be secure"), so
`npm start` on its own profile cannot create a new session — it only works once
that profile is already signed in:

```powershell
npm start             # http://127.0.0.1:8766, profile .chatgpt-browser-profile
```

`--headless` is supported for accounts that are already signed in, but
chatgpt.com answers an automated headless browser with a Cloudflare challenge,
so keep the browser visible.

## Run Swarm against it

```sh
cargo run -p swarm-cli -- run "create hello.txt and verify it" --provider chatgpt-web
```

`--provider chatgpt-web` selects the preset: bridge at
`http://127.0.0.1:8766`, model `auto` (the account's own pick), four concurrent
sessions. Override with `--model`, `--base-url`, and `--agents N` (each agent
gets its own tab). Use `--approve-all` only in a workspace where the tools are
safe; by default every risky tool prompts in the terminal.

Because a ChatGPT subscription is not zero-cost, the run's zero-cost policy is
turned off by this preset. In a TOML config, name the provider and set
`kind = "chatgpt_web"`, `base_url`, `model`, and `zero_cost = false`.

## Models

`--model` is matched against the account's model picker, not against an API
model slug. Matching ignores case, punctuation, and whitespace, so
`--model "GPT-5.1 Thinking"` matches a picker label of `GPT-5.1 Thinking`.
`auto` (the default) does not touch the picker.

The bridge verifies the switch by reading the picker's own label afterwards, so
a plan-gated model fails fast:

* `403 ... requires a plan this account does not have` — the account cannot use
  that model.
* `422 ... is not in the account's model picker` — the name matched nothing;
  open the picker in the Chrome window and copy the exact label.

## Bridge options

| Flag | Meaning |
| --- | --- |
| `--port` | Loopback port, default `8766` |
| `--profile` | Chrome profile dir to launch; repeat for more access points |
| `--cdp` | Attach to an existing Chrome debug endpoint; repeat for more |
| `--chat-url` | Chat page, default `https://chatgpt.com/` |
| `--headless` | Launch Chrome headless (only works for the fixture) |
| `--no-api` | Read replies from the DOM, never `/backend-api/conversation` |
| `--keep-chats` | Leave the conversation in ChatGPT history when a session closes |
| `--response-timeout-ms` | Per-reply budget, default `600000` |

`GET /health` reports `sessions`, `access_points`, and `reply_source`
(`api` or `dom`). The listener binds to `127.0.0.1` only.

## Chat history

Each Swarm session gets its own ChatGPT conversation, and closing the session
deletes it, so a run leaves nothing behind in the sidebar. `DELETE
/sessions/{id}` returns `chat_deleted`: `api` (deleted server-side), `menu`
(deleted through the sidebar menu, used only for a row that links to this exact
conversation id), `none` (nothing was ever sent, so no chat exists), `kept`
(`--keep-chats`), or `failed`. Tabs abandoned by a crashed Swarm process are
reaped the same way once they go idle. Pass `--keep-chats` while debugging so
the failing conversation stays open for inspection.

The 2026 sidebar is a virtualized list of bare `a[href^="/c/"]` links; each
row's `Chat actions` button is a sibling, not a child, so the bridge pairs the
Nth link with the Nth button and refuses to click unless the two boxes share a
row. The menu carries no test ids (Rename, Pin, Share, Archive, Delete, Move to
project), so items are matched by accessible name, and the site deletes
asynchronously — the bridge polls until the row is gone rather than assuming
one click did it.

The site's `/backend-api/conversation/{id}` read path is not available to every
account (a signed-in owner can get `conversation_inaccessible`), which is why
`--no-api` is the recommended setting for live use; the bridge reads the DOM
turns instead and needs no site API at all.

## Concurrency and rate limits

Every `--profile` (or `--cdp`) is an access point, and sessions are handed out
round-robin, so N signed-in profiles give N parallel chats. ChatGPT enforces
per-account message caps: when a tab shows a limit banner, the bridge answers
`429` and Swarm's quota gate marks that provider degraded and backs off. Add
access points (`--profile ... --profile ...`) before raising `--agents`.

Replies stay in the bridge until Swarm reads them, and a retried request with
the same `request_id` returns the stored reply, so a Swarm restart cannot
double-spend a chat turn. Tabs idle for 30 minutes are closed.

## Troubleshooting

* `401 ChatGPT composer unavailable` / `ChatGPT asked you to sign in` — that tab
  is signed out. Sign in in the bridge's Chrome window and retry. If Google
  answers "This browser or app may not be secure", sign in through Chrome
  started by `npm run chrome` and attach with `--cdp` instead.
* Google refuses to sign you in — expected in a Playwright-launched Chrome. Use
  the `--cdp` route (`npm run chrome`), sign in there, then attach the bridge.
* `504 ... was still generating` — a reasoning model exceeded the timeout;
  raise `--response-timeout-ms` or pick a faster model.
* Model picker never opens — ChatGPT changed the header markup; the bridge
  looks for a header button whose label mentions a model family.
* Replies arrive empty — check the tab for a "sign in", "high traffic", or
  rate-limit banner; the bridge surfaces those as 401/504/429 with the page text.

Run the fake site and the bridge side by side to reproduce failures without
touching a real account:

```sh
node fixture.mjs --port 8790
node bridge.mjs --headless --profile /tmp/swarm-fixture --port 8766 \
  --chat-url http://127.0.0.1:8790/app
```
