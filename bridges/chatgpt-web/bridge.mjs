// Local ChatGPT web transport. One browser tab per Swarm session. The HTTP
// listener is loopback-only and does not expose browser data.
//
// Replies are read from ChatGPT's own conversation API (same-origin, using the
// signed-in cookies already in the tab), which is far more stable than scraping
// the chat DOM. The DOM read is the fallback for sites and fixtures that do not
// serve that API; force it with --no-api.
import http from 'node:http';
import { randomUUID } from 'node:crypto';
import { resolve } from 'node:path';
import { chromium } from 'playwright-core';

const args = process.argv.slice(2);
function option(name, fallback) {
  const index = args.indexOf(name);
  return index < 0 ? fallback : args[index + 1];
}
function options(name) {
  return args.flatMap((arg, index) => arg === name ? [args[index + 1]] : []);
}

const port = Number(option('--port', '8766'));
const cdpEndpoints = options('--cdp');
const profiles = (options('--profile').length ? options('--profile') :
  cdpEndpoints.length ? [] : ['.chatgpt-browser-profile']).map((path) => resolve(path));
const chatUrl = option('--chat-url', 'https://chatgpt.com/');
const headless = args.includes('--headless');
const useApi = !args.includes('--no-api');
// Deleting the chat on close is the polite default; --keep-chats leaves the
// conversation in history (useful when debugging a failed run).
const keepChats = args.includes('--keep-chats');
const responseTimeoutMs = Number(option('--response-timeout-ms', '600000'));
if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('invalid --port');
if (!Number.isFinite(responseTimeoutMs) || responseTimeoutMs < 1000) throw new Error('invalid --response-timeout-ms');
if (new Set(profiles).size !== profiles.length) throw new Error('duplicate --profile path');
if (new Set(cdpEndpoints).size !== cdpEndpoints.length) throw new Error('duplicate --cdp endpoint');
if (cdpEndpoints.length && profiles.length) throw new Error('choose --cdp or --profile');
for (const endpoint of cdpEndpoints) {
  if (!/^http:\/\/(localhost|127\.0\.0\.1):\d+$/.test(endpoint)) {
    throw new Error('--cdp must be a local Chrome debugging endpoint');
  }
}

// Selectors that ChatGPT has shipped across its 2025-2026 composer revisions,
// tried in order. All of them are role/id based, not layout based.
const COMPOSER = [
  '#prompt-textarea',
  // 2026 ChatGPT: a div with role=textbox whose contenteditable comes from an
  // ancestor, so the exact-attribute form below misses it.
  '[role="textbox"][contenteditable]',
  'div[contenteditable="true"][aria-label]',
  'div[role="textbox"]',
  'textarea[aria-label*="Chat" i]',
  'textarea[aria-label*="prompt" i]',
];
const SEND = [
  'button[aria-label="Send prompt"]',
  'button[aria-label*="Send" i]',
  'button[data-testid="send-button"]',
];
// Assistant turns, in the order they appear on the page, newest markup first.
// 2026 ChatGPT has no `article`/`.markdown`: each turn is a unit keyed by role
// (`fallback-turn-0:2:assistant`) whose wrapper carries a completion state.
const TURN = [
  '[data-content-search-unit-key$=":assistant"]',
  '[data-message-author-role="assistant"] .markdown',
  '[data-message-id] .markdown',
  'article .markdown',
];
// The user's own turn, newest markup first. This is the proof that the site
// POSTED the message: a send the app swallows creates no user unit, and the
// reply has not started yet, so nothing else on the page can tell you.
const USER_TURN = [
  '[data-content-search-unit-key$=":user"]',
  '[data-message-author-role="user"]',
  'article[data-message-author-role="user"]',
];
// Per-turn state attribute the 2026 site uses while streaming.
const TURN_STATE = '[data-talvt-turn-state]';
// Visually hidden "ChatGPT said:" heading that ships inside the unit, so it
// lands in innerText and must not be read back as model output.
const TURN_LABEL = '[data-conversation-role]';
const STOPPING = [
  'button[aria-label="Stop generating"]',
  'button[aria-label*="Stop" i]',
  '[data-testid="stop-button"]',
];

const contexts = [];
const attachedBrowsers = [];
for (const profilePath of profiles) {
  contexts.push(await chromium.launchPersistentContext(profilePath, {
    channel: 'chrome',
    headless,
    viewport: { width: 1280, height: 900 },
    // Google's sign-in page refuses browsers that advertise automation
    // ("This browser or app may not be secure"), and ChatGPT's CDN probes the
    // same signal, so drop Playwright's automation flag. Accounts that are
    // already signed in do not care; first-time sign-in should use --cdp with
    // start-chrome.ps1, which starts Chrome without any automation at all.
    args: ['--no-first-run', '--disable-blink-features=AutomationControlled'],
    ignoreDefaultArgs: ['--enable-automation'],
  }));
}
for (const endpoint of cdpEndpoints) {
  const browser = await chromium.connectOverCDP(endpoint);
  const context = browser.contexts()[0];
  if (!context) throw new Error(`Chrome at ${endpoint} has no default browser context`);
  attachedBrowsers.push(browser);
  contexts.push(context);
}
const sessions = new Map();
let nextAccessPoint = 0;
// Tabs one access point (one Playwright browser) will hold at once. Past that the
// site slows every chat in the browser down, so another access point is used, and
// when all are full a new session is refused instead of piling on.
const maxTabs = Number(option('--max-tabs', '8'));
if (!Number.isInteger(maxTabs) || maxTabs < 1) throw new Error('invalid --max-tabs');
// Sessions being created count as load, so two sessions asked for at the same
// moment do not both pick the same idle browser.
const reserved = contexts.map(() => 0);
// An access point whose browser was closed or crashed is skipped for a while.
const deadUntil = new Map();
const DEAD_FOR_MS = 30_000;

function accessPointLoad() {
  const load = reserved.slice();
  for (const session of sessions.values()) load[session.accessPoint] += 1;
  return load;
}

/// Access points to try, best first: live before dead, then least loaded, then
/// rotating so equal loads share the work.
function accessPointOrder() {
  const load = accessPointLoad();
  const now = Date.now();
  const count = contexts.length;
  const turn = nextAccessPoint++ % count;
  const dead = (index) => ((deadUntil.get(index) ?? 0) > now ? 1 : 0);
  return contexts.map((_, index) => index).sort((a, b) =>
    dead(a) - dead(b) || load[a] - load[b] || ((a - turn + count) % count) - ((b - turn + count) % count));
}

const browserGone = (error) =>
  /has been closed|Target closed|Browser closed|browser has disconnected|Connection closed|Protocol error/i
    .test(error instanceof Error ? error.message : String(error));

// Launched profiles get an onboarding tab. An attached Chrome may already have
// user tabs; never navigate one of those away from its current page.
for (let index = 0; index < contexts.length; index++) {
  const context = contexts[index];
  const welcome = profiles.length
    ? (context.pages()[0] ?? await context.newPage())
    : (context.pages().find((page) => page.url().startsWith(chatUrl)) ?? await context.newPage());
  if (!welcome.url().startsWith(chatUrl)) {
    await welcome.goto(chatUrl, { waitUntil: 'domcontentloaded' });
  }
}

class BridgeError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

async function firstVisible(page, selectors, timeout = 30000) {
  const deadline = Date.now() + timeout;
  let last = null;
  while (Date.now() < deadline) {
    for (const selector of selectors) {
      const locator = page.locator(selector).first();
      if (await locator.isVisible().catch(() => false)) return locator;
    }
    last ??= selectors[0];
    await page.waitForTimeout(200);
  }
  return null;
}

async function composer(page) {
  const found = await firstVisible(page, COMPOSER);
  if (!found) {
    throw new BridgeError(401, 'ChatGPT composer unavailable; sign in in the bridge Chrome window');
  }
  return found;
}

function normalize(text) {
  return (text ?? '').toLowerCase().replace(/[^a-z0-9]+/g, ' ').trim();
}

async function findMenuItem(page, label) {
  const wanted = normalize(label);
  for (const role of ['menuitem', 'menuitemradio', 'option']) {
    const items = page.getByRole(role);
    const count = await items.count().catch(() => 0);
    for (let index = 0; index < count; index++) {
      const item = items.nth(index);
      const text = await item.innerText().catch(() => '');
      const name = await item.getAttribute('aria-label').catch(() => null);
      if (normalize(`${text} ${name ?? ''}`).includes(wanted)) return item;
    }
  }
  // Some ChatGPT builds render the picker as plain buttons inside a listbox.
  const loose = page.locator('[role="menu"] button, [role="listbox"] button, [role="menu"] [role="button"], [role="listbox"] [role="button"]');
  const count = await loose.count().catch(() => 0);
  for (let index = 0; index < count; index++) {
    const item = loose.nth(index);
    if (normalize(await item.innerText().catch(() => '')).includes(wanted)) return item;
  }
  return null;
}

async function selectModel(page, model) {
  if (!model || /^(auto|default|site default)$/i.test(model.trim())) return;
  const label = model.trim();
  const switcher = page.locator('header button, button[aria-haspopup="menu"], button[aria-haspopup="listbox"]')
    .filter({ hasText: /GPT|ChatGPT|Auto|Explore|Sonnet|claude|o[134]\b/i })
    .first();
  try {
    await switcher.waitFor({ state: 'visible', timeout: 30000 });
  } catch {
    throw new BridgeError(422, `ChatGPT model picker not found; cannot select '${label}'`);
  }
  await switcher.click();
  const option = await findMenuItem(page, label);
  if (!option) {
    await page.keyboard.press('Escape').catch(() => {});
    throw new BridgeError(422, `ChatGPT model '${label}' is not in the account's model picker`);
  }
  await option.click();
  const wall = page.getByText(/upgrade|sign up|not available on your plan|paid plan/i).first();
  if (await wall.waitFor({ state: 'visible', timeout: 1200 }).then(() => true).catch(() => false)) {
    await page.keyboard.press('Escape').catch(() => {});
    throw new BridgeError(403, `ChatGPT model '${label}' requires a plan this account does not have`);
  }
  // The switcher label is the only reliable proof the model took effect.
  const chosen = await switcher.innerText().catch(() => '');
  if (!normalize(chosen).includes(normalize(label))) {
    throw new BridgeError(403, `ChatGPT did not select model '${label}' (picker shows '${chosen.trim()}')`);
  }
}

async function createSession(model) {
  let lastError;
  let full = 0;
  for (const accessPoint of accessPointOrder()) {
    if (accessPointLoad()[accessPoint] >= maxTabs) { full += 1; continue; }
    reserved[accessPoint] += 1;
    try {
      return await createSessionOn(accessPoint, model);
    } catch (error) {
      // A closed browser is that access point's problem, not the caller's: mark it
      // and use the next one. Sign-in walls, bad models and the like are the
      // caller's to hear about.
      if (!browserGone(error)) throw error;
      deadUntil.set(accessPoint, Date.now() + DEAD_FOR_MS);
      lastError = error;
      console.log(`access point ${accessPoint} is unavailable for ${DEAD_FOR_MS / 1000}s: ${error.message.split('\n')[0]}`);
    } finally {
      reserved[accessPoint] -= 1;
    }
  }
  if (lastError) {
    throw new BridgeError(503, `no access point is usable (last error: ${lastError.message.split('\n')[0]})`);
  }
  throw new BridgeError(503,
    `all ${full} access point(s) are at the ${maxTabs}-tab limit; close a session or add an access point (--cdp / --profile)`);
}

async function createSessionOn(accessPoint, model) {
  const context = contexts[accessPoint];
  const page = await context.newPage();
  try {
    await page.goto(chatUrl, { waitUntil: 'domcontentloaded', timeout: 30000 });
    await composer(page);
    // Root URL resumes the last chat; a session must own a fresh conversation,
    // and the model has to be picked after the reset, not before it.
    await startNewChat(page);
    await composer(page);
    await selectModel(page, model);
    const id = randomUUID();
    sessions.set(id, { page, model, accessPoint, replies: new Map(), activeAt: Date.now(), pending: false, log: [] });
    return { id, access_point: accessPoint };
  } catch (error) {
    await page.close();
    throw error;
  }
}

function conversationId(url) {
  return url.match(/\/c\/([0-9a-f-]{16,})/i)?.[1] ?? null;
}

/// Reads the conversation over ChatGPT's own JSON API, from inside the page so
/// the signed-in cookies apply. Returns null on any non-OK/shape surprise.
async function readViaApi(page, baseline) {
  const id = conversationId(page.url());
  // A brand-new chat has no conversation id until the site creates one after
  // the first send; that is "not ready yet", not a failed read.
  if (!id) return { ok: true, ready: false, count: Math.max(baseline, 0), pending: true };
  return await page.evaluate(async ({ id, baseline }) => {
    const response = await fetch(`/backend-api/conversation/${id}`, {
      headers: { accept: 'application/json' },
      credentials: 'include',
    });
    if (!response.ok) return { ok: false, status: response.status };
    const data = await response.json().catch(() => null);
    const nodes = Object.values(data?.messages ?? {});
    const assistants = nodes
      .filter((message) => message?.author?.role === 'assistant' && message?.visible !== false)
      .map((message) => ({
        text: (message.content?.parts ?? []).filter((part) => typeof part === 'string').join('\n'),
        model: message.metadata?.model_slug ?? message.author?.metadata?.model_slug ?? null,
        at: message.create_time ?? 0,
        tokens: message.metadata?.token_count ?? message.metadata?.usage ?? message.usage ?? null,
      }))
      .filter((message) => message.text.trim().length > 0)
      .sort((left, right) => left.at - right.at);
    if (assistants.length <= baseline) return { ok: true, ready: false, count: assistants.length };
    const latest = assistants[assistants.length - 1];
    const tokens = latest.tokens ?? {};
    return {
      ok: true,
      ready: true,
      count: assistants.length,
      text: latest.text.trim(),
      model: latest.model,
      usage: {
        input_tokens: tokens.input_tokens ?? tokens.prompt_tokens ?? 0,
        output_tokens: tokens.output_tokens ?? tokens.completion_tokens ?? 0,
        cached_input_tokens: tokens.cached_tokens ?? tokens.cached_input_tokens ?? 0,
      },
    };
  }, { id, baseline }).catch(() => null);
}

/// How many assistant turns are on the page, using whichever markup shape this
/// site is currently using. A fresh chat has none, so 0 is a real answer.
async function assistantCount(page) {
  for (const selector of TURN) {
    const count = await page.locator(selector).count().catch(() => 0);
    if (count > 0) return count;
  }
  return 0;
}

async function isGenerating(page) {
  for (const selector of STOPPING) {
    if (await page.locator(selector).first().isVisible().catch(() => false)) return true;
  }
  // 2026 ChatGPT has no stop button, so the selector list above never fires and
  // an idle gate built on it is a no-op. The turn element carries the state: the
  // last one is `generating` until the site finishes, and submitting into that
  // window is what drops a mid-conversation send.
  const state = await page.evaluate(() => {
    const nodes = document.querySelectorAll('[data-talvt-turn-state]');
    const last = nodes[nodes.length - 1];
    return last ? last.getAttribute('data-talvt-turn-state') : null;
  }).catch(() => null);
  return state === 'generating';
}

async function userTurnCount(page) {
  for (const selector of USER_TURN) {
    const count = await page.locator(selector).count().catch(() => 0);
    if (count > 0) return count;
  }
  return 0;
}

/// The site drops a message submitted while it is still streaming the previous
/// turn: the text leaves the composer, no user unit is created, and the reply
/// poll then waits out the whole response window for a turn that never existed.
/// Swarm posts a tool result the moment a reply is read, so this gap is normal
/// and has to be closed before typing.
async function waitIdle(page, windowMs) {
  const until = Date.now() + windowMs;
  while (await isGenerating(page).catch(() => false)) {
    if (Date.now() > until) return false;
    await page.waitForTimeout(400);
  }
  return true;
}

async function fencedCode(element) {
  // The chat renders fenced code with a header row and a copy button, so
  // innerText of the bubble mangles the payload Swarm has to parse. Rebuild
  // every <pre> as a fence, tagging Swarm JSON so the language survives.
  // The 2026 markup also ships a visually hidden "ChatGPT said:" heading, which
  // innerText would return as model output, so work on a label-free clone.
  //
  // Everything below runs inside the page: Playwright serializes this function
  // alone, so it must not call module-scope helpers.
  //
  // A file the model wrote is a payload channel: a model that will not type a
  // fenced block will happily attach the same JSON as a download.
  const attachments = [];
  for (const link of [...element.querySelectorAll('a[download][href]')].slice(0, 3)) {
    const href = link.getAttribute('href') || '';
    if (!/\.json($|\?)/i.test(href) && !/\.json$/i.test(link.getAttribute('download') || '')) continue;
    try {
      const response = await fetch(link.href, { credentials: 'include' });
      if (!response.ok) continue;
      const text = (await response.text()).trim();
      JSON.parse(text);
      attachments.push(text);
    } catch {
      // A file that is not JSON, or is not reachable, is not a tool request.
    }
  }
  const clone = element.cloneNode(true);
  clone.querySelectorAll('[data-conversation-role]').forEach((node) => node.remove());
  // A canvas/attachment turn ships chips ("Download hello.txt", "Open file")
  // inside the unit; they are UI, not model output.
  clone.querySelectorAll('button, a[download], [role="button"]').forEach((node) => node.remove());
  let rendered = clone.innerText;
  for (const pre of clone.querySelectorAll('pre')) {
    const code = pre.textContent?.trimEnd();
    if (!code) continue;
    let language = '';
    try {
      const value = JSON.parse(code);
      if (value && typeof value === 'object' &&
          ('tool' in value || 'tool_calls' in value || 'plan' in value || 'steps' in value)) {
        language = 'swarm';
      }
    } catch {}
    const fence = `\`\`\`${language}\n${code}\n\`\`\``;
    const label = pre.parentElement?.innerText ?? '';
    rendered = rendered.replace(label.trim(), fence);
    if (!rendered.includes(fence)) rendered = rendered.replace(pre.innerText, fence);
  }
  // A file the model wrote is a payload channel too: the model that cannot be
  // talked into a fenced block will happily attach the same JSON as a file.
  for (const body of attachments) rendered += `\n\`\`\`swarm\n${body}\n\`\`\``;
  return rendered.trim();
}

/// Text of every side panel and dialog on the page, used to tell which one a click opened.
async function panelTexts(page) {
  return page.evaluate(() => [...document.querySelectorAll('aside, [role="dialog"], [role="complementary"]')]
    .map((node) => node.innerText || '')).catch(() => []);
}

/// A model that cannot be talked out of it makes its "file" in ChatGPT's own sandbox: the reply is
/// a card ("Created FINDINGS.md", buttons "Open preview of FINDINGS.md" and "Download file") and the
/// text exists only in a side panel. Swarm sees a reply that claims a file and carries none of it, and
/// the chat is deleted when the session closes, so the work is lost (four research workers lost their
/// reports that way). Opening the preview puts the whole document into the page; read it and hand it
/// back with the reply. Downloads are not used: a CDP-attached Chrome does not surface them.
async function readAttachments(page, unitSelector) {
  const unit = page.locator(unitSelector).last();
  const buttons = unit.locator('button[aria-label^="Open preview of "]');
  const count = Math.min(await buttons.count().catch(() => 0), 3);
  const parts = [];
  let total = 0;
  for (let index = 0; index < count && total < 60000; index += 1) {
    const button = buttons.nth(index);
    const label = (await button.getAttribute('aria-label').catch(() => '')) || '';
    const name = label.replace(/^Open preview of /, '').trim() || `attachment-${index + 1}`;
    const before = await panelTexts(page);
    await button.click({ timeout: 4000 }).catch(() => {});
    // The panel opens with its title and fills in the document a moment later, so a first
    // look reads only a header. Keep the longest new text and stop once it has held still.
    let text = '';
    let stable = 0;
    for (let tick = 0; tick < 40 && (stable < 6 || text.length < 120); tick += 1) {
      await page.waitForTimeout(250);
      const fresh = (await panelTexts(page)).filter((value) => value.length > 20 && !before.includes(value));
      const best = fresh.sort((a, b) => b.length - a.length)[0] || '';
      if (best.length > text.length) { text = best; stable = 0; } else if (text) { stable += 1; }
      if (tick >= 12 && stable >= 6) break;
    }
    trace('attachment', name, text.length, 'chars');
    await page.keyboard.press('Escape').catch(() => {});
    const body = text.trim() || '(the preview panel did not open, so the file content could not be read)';
    parts.push(`[ATTACHMENT ${name}]\n${body}\n[/ATTACHMENT]`);
    total += body.length;
  }
  return parts.length ? `\n\n${parts.join('\n\n')}` : '';
}

async function readViaDom(page, previous, deadline) {
  // Which markup shape the site is using is decided by which candidate has a
  // finished new turn, so all shapes are probed together. The "still
  // streaming" check runs through Playwright: STOPPING uses Playwright's `i`
  // flag syntax, which raw document.querySelector rejects by throwing.
  const read = () => page.evaluate(({ selectors, previous, stateSelector }) => {
    for (const selector of selectors) {
      const answers = document.querySelectorAll(selector);
      if (answers.length <= previous) continue;
      const latest = answers[answers.length - 1];
      const turn = latest.closest(stateSelector);
      if (turn) {
        if (turn.getAttribute('data-talvt-turn-state') !== 'complete') continue;
      } else if (latest.getAttribute('aria-busy') === 'true') {
        continue;
      }
      return { selector, text: latest.innerText };
    }
    return null;
  }, { selectors: TURN, previous, stateSelector: TURN_STATE }).catch(() => null);

  let emptySince = 0;
  while (Date.now() < deadline) {
    const found = await read();
    if (found && !(await isGenerating(page))) {
      const rendered = await page.locator(found.selector).last()
        .evaluate(fencedCode).catch(() => found.text);
      const attachments = await readAttachments(page, found.selector).catch(() => '');
      const text = (String(rendered ?? '').trim() + attachments).trim();
      if (text) return { text, model: null, usage: null };
      // A turn the site marks complete that holds no text (measured: a prompt that
      // asks for a JSON object gets exactly this). Polling on would hold the request
      // for the whole response window; report it so the caller can retry or change.
      emptySince ||= Date.now();
      if (Date.now() - emptySince > 15000) return { text: '', empty: true };
    } else {
      emptySince = 0;
    }
    await page.waitForTimeout(400);
  }
  return null;
}

/// Delete the chat this tab is on, so a finished run does not litter the user's
/// ChatGPT history. Server-side first; the sidebar menu is the fallback.
async function deleteChat(page) {
  if (keepChats) return 'kept';
  const match = page.url().match(/\/c\/([0-9a-f]{8}-[0-9a-f-]{27,})/i);
  if (!match) return 'none';   // nothing was ever sent, so no chat exists
  const id = match[1];
  const deleted = await page.evaluate(async (chatId) => {
    try {
      const response = await fetch(`/backend-api/conversation/${chatId}`, {
        method: 'DELETE', credentials: 'include',
      });
      return response.ok;
    } catch {
      return false;
    }
  }, id).catch(() => false);
  if (deleted) return 'api';
  return await deleteChatFromMenu(page, id) ? 'menu' : 'failed';
}

async function deleteChatFromMenu(page, id) {
  const say = (...parts) => { if (process.env.SWARM_BRIDGE_DEBUG) console.log('[menu-delete]', ...parts); };
  // 2026 sidebar: chat links and their "Chat actions" buttons are 1:1 in DOM
  // order and share a row (measured 1px offset). The menu has no test ids, so
  // items are matched by accessible name.
  const index = await page.evaluate((chatId) => [...document.querySelectorAll('a[href^="/c/"]')]
    .findIndex((link) => link.href.includes(chatId)), id).catch(() => -1);
  say('index', index);
  if (index < 0) return false;   // row is not rendered: the list is virtualized
  const link = page.locator('a[href^="/c/"]').nth(index);
  const trigger = page.locator('button[aria-label="Chat actions"]').nth(index);
  const linkBox = await link.boundingBox().catch(() => null);
  const triggerBox = await trigger.boundingBox().catch(() => null);
  say('link box', JSON.stringify(linkBox), 'trigger box', JSON.stringify(triggerBox));
  if (!linkBox || !triggerBox) return false;
  // A wrong click here destroys a chat the user cares about, so the trigger is
  // only used when it sits on the same row as this chat's own link.
  const offset = Math.abs((linkBox.y + linkBox.height / 2) - (triggerBox.y + triggerBox.height / 2));
  say('row offset', offset.toFixed(1), 'tolerance', Math.max(linkBox.height, 24));
  if (offset > Math.max(linkBox.height, 24)) return false;
  await link.hover().catch(() => {});
  await trigger.click({ timeout: 3000 }).catch(() => {});
  const item = page.getByRole('menuitem', { name: 'Delete', exact: true }).first();
  say('menu items', await item.count().catch(() => 0));
  if (await item.count().catch(() => 0) === 0) {
    await page.keyboard.press('Escape').catch(() => {});
    return false;
  }
  await item.click({ timeout: 3000 }).catch(() => {});
  const confirm = page.locator('[data-testid="confirm-delete-chat"], [role="dialog"] button:has-text("Delete"), [role="alertdialog"] button:has-text("Delete"), [role="dialog"] button:has-text("Confirm")').first();
  say('confirm buttons', await confirm.count().catch(() => 0));
  if (await confirm.count().catch(() => 0) > 0) await confirm.click({ timeout: 3000 }).catch(() => {});
  // Confirm the row is really gone; a menu that opened and closed is not a
  // delete. The site deletes asynchronously, so this polls rather than peeking.
  const gone = await page.waitForFunction((chatId) => ![...document.querySelectorAll('a[href^="/c/"]')]
    .some((link) => link.href.includes(chatId)), id, { timeout: 8000 })
    .then(() => true).catch(() => false);
  say('row gone:', gone);
  return gone;
}

/// Did the site take the message? Proved only by new turn units: a user unit
/// means the app POSTED the prompt, an assistant unit means the reply is there.
/// "is generating" is deliberately NOT a start signal — it is inherited from the
/// previous turn when a submission is swallowed, which is what turned a dropped
/// send into a 10-minute 504.
async function turnStarted(page, previousTurns, previousUserTurns, windowMs) {
  const until = Date.now() + windowMs;
  while (Date.now() < until) {
    if (await assistantCount(page) > previousTurns) return true;
    if (await userTurnCount(page) > previousUserTurns) return true;
    if (await loginWall(page)) return true;
    await page.waitForTimeout(300);
  }
  return false;
}

async function pageComplaint(page) {
  const alerts = await page.locator('[role="alert"], [data-testid*="limit"], .banner').allTextContents()
    .catch(() => []);
  return alerts.map((part) => part.trim()).filter(Boolean).join(' ');
}

/// True when a sign-in modal is open. A logged-out ChatGPT tab accepts composer
/// input and then asks you to sign in, so this has to be watched mid-flight.
async function loginWall(page) {
  const dialogs = await page.locator('[role="dialog"], [role="alertdialog"]').allTextContents()
    .catch(() => []);
  return dialogs.some((text) =>
    /sign (in|up)|log ?in to|continue to chatgpt|create a free account|authenticat/i.test(text));
}

/// Type the prompt into the composer and prove the DOM took it, retrying when a
/// late hydration resets the editor.
///
/// Long prompts go in as chunked CDP insertText. Writing innerHTML in one shot
/// renders the text and the composer *looks* full while the editor's own
/// document stays empty: Enter then submits nothing and the reply poll waits out
/// the whole response window for a turn that was never requested (the 600s 504).
/// A single insertText of ~6k chars was measured dropping one character, hence
/// the chunks.
///
/// The proof is a LENGTH check, never a string comparison. ChatGPT's editor
/// parses markdown as you type: "- " becomes a bullet, and paired underscores in
/// identifiers like generation_config.go become emphasis, which re-nests nodes
/// and reorders innerText, so a tail comparison rejects faithful prompts (that
/// was the 503 storm). Truncation is still caught, by length.
async function typePrompt(page, prompt) {
  for (let attempt = 0; attempt < 3; attempt += 1) {
    const editor = await composer(page);
    try {
      await editor.waitFor({ state: 'visible', timeout: 10000 });
      await editor.click({ timeout: 5000 });
      if (prompt.split('\n').length > 40 && await editor.evaluate((node) => node.isContentEditable)) {
        await editor.evaluate((node) => node.focus());
        for (let at = 0; at < prompt.length; at += 1800) {
          await page.keyboard.insertText(prompt.slice(at, at + 1800));
          await page.waitForTimeout(25);
        }
      } else {
        await editor.fill(prompt, { timeout: 10000 });
      }
      await page.waitForTimeout(500);
      const verdict = await editor.evaluate((node, expected) => {
        const squash = (text) => text.replace(/\s+/g, ' ').trim();
        const norm = (text) => squash(text).replace(/•/g, ' ');
        const core = (text) => text.replace(/[^a-z0-9]/gi, '');
        const read = ('value' in node ? node.value : node.innerText) || '';
        return {
          want: core(norm(expected)).length,
          got: core(norm(read)).length,
          wantTail: core(norm(expected)).slice(-40),
          gotTail: core(norm(read)).slice(-40),
        };
      }, prompt);
      if (verdict.got >= Math.floor(verdict.want * 0.98)) return true;
      console.error(`[composer] attempt ${attempt}: want=${verdict.want} got=${verdict.got}`);
      console.error(`[composer]   want tail ${JSON.stringify(verdict.wantTail)}`);
      console.error(`[composer]   got  tail ${JSON.stringify(verdict.gotTail)}`);
    } catch (error) {
      // A navigation or a late hydration can replace the composer.
      console.error(`[composer] attempt ${attempt} (${prompt.length} chars): ${String(error.message).split('\n')[0]}`);
    }
    await page.waitForTimeout(1000);
  }
  return false;
}

/// Keep the bridge's own copy of the conversation. The site stops accepting new
/// messages once a chat is long, and the chat that replaces it needs the history.
// Measured: the site drops the 6th message of a chat, so rotate after the 5th.
const MAX_TURNS_PER_CHAT = 5;

/// One request, one chat: delete the previous chat, open an empty one, and let the
/// normal send path run against it. The caller sends the whole context every time,
/// as an API would, so nothing depends on the site remembering earlier turns and
/// the site's long-chat limits never come into play.
async function resetToFreshChat(page) {
  await deleteChat(page).catch(() => 'failed');
  await startNewChat(page);
  for (let tick = 0; tick < 40; tick += 1) {
    if (await assistantCount(page) === 0 && await userTurnCount(page) === 0) break;
    await page.waitForTimeout(250);
  }
  trace('fresh chat ready', page.url());
}

async function sendLogged(session, prompt, fresh = false) {
  if (fresh) {
    await resetToFreshChat(session.page);
    session.log = [];
    session.rotate = false;
  }
  session.log.push({ role: 'user', text: prompt });
  const reply = await sendMessage(session, prompt);
  session.log.push({ role: 'assistant', text: reply.text });
  if (fresh) {
    session.log = [];
    return reply;
  }
  // The site stops accepting new messages in a chat after about six exchanges,
  // and it fails by dropping the send with no wall at all. Flag the session so
  // the next prompt opens a fresh chat carrying the transcript.
  if (await userTurnCount(session.page) >= MAX_TURNS_PER_CHAT) session.rotate = true;
  return reply;
}

/// Newest turns first, newest-first accumulation, capped so the carry-over stays
/// inside the size the site accepts on a fresh chat.
function carriedTranscript(log, budget = Number(process.env.SWARM_CARRY_CHARS) || 16000) {
  const parts = [];
  let size = 0;
  // The first prompt holds the goal and the reply protocol. Newest-first
  // accumulation drops it, and the agent then starts the task over after every
  // rotation. Pin its head and tail, and spend the rest of the budget on recent turns.
  let floor = 0;
  let pinned = '';
  if (log.length > 1 && log[0].role === 'user') {
    const first = log[0].text;
    const text = first.length > 5000 ? `${first.slice(0, 2500)}\n[...]\n${first.slice(-2500)}` : first;
    pinned = `Swarm (original request): ${text}`;
    size += pinned.length;
    floor = 1;
  }
  // The newest log entry is the prompt being sent now; the caller appends it
  // itself, so carrying it too would send the same text twice.
  for (let index = log.length - 2; index >= floor; index -= 1) {
    const turn = log[index];
    const part = `${turn.role === 'user' ? 'Swarm' : 'ChatGPT'}: ${turn.text}`;
    if (size + part.length > budget) break;
    size += part.length;
    parts.unshift(part);
  }
  if (pinned) parts.unshift(pinned);
  if (parts.length === 0) return '';
  return '[CONVERSATION SO FAR — the browser chat was restarted because the site'
    + ' stopped accepting messages. Continue from here, do not greet again.]\n\n'
    + `${parts.join('\n\n')}\n\n[CONTINUE]\n`;
}

/// The site silently stops accepting messages in a long chat: Enter clears the
/// composer, posts nothing, and no wall is shown. Nothing recovers that chat, so
/// delete it, open a fresh one in the same tab, and carry the transcript.
const trace = (...parts) => {
  if (process.env.SWARM_BRIDGE_TRACE) console.log(new Date().toISOString().slice(11, 19), ...parts);
};

async function restartChat(session, prompt) {
  const { page } = session;
  trace('restartChat: start, prompt chars', prompt.length, 'log turns', session.log.length);
  await deleteChat(page).catch(() => 'failed');
  await startNewChat(page);
  trace('restartChat: new chat opened', page.url());
  await page.waitForTimeout(1500);
  // The old chat's turns can stay in the DOM for a moment after the URL leaves
  // /c/<id>. A baseline taken then is the old count, the new chat never exceeds
  // it, and a prompt that was posted reads as "not posted". Wait for an empty
  // view; a fresh chat has no turns, so the baseline is then 0.
  for (let tick = 0; tick < 40; tick += 1) {
    if (await assistantCount(page) === 0 && await userTurnCount(page) === 0) break;
    await page.waitForTimeout(250);
  }
  const previousTurns = await assistantCount(page);
  const previousUserTurns = await userTurnCount(page);
  trace('restartChat: baselines', previousTurns, previousUserTurns);
  if (!(await waitIdle(page, 20000))) { trace('restartChat: never idle'); return false; }
  const carried = carriedTranscript(session.log) + prompt;
  if (!(await typePrompt(page, carried))) { trace('restartChat: typePrompt failed, chars', carried.length); return false; }
  await page.keyboard.press('Enter');
  const started = await turnStarted(page, previousTurns, previousUserTurns, 30000);
  trace('restartChat: turnStarted', started, 'chars', carried.length);
  return started;
}

/// Force a genuinely new conversation.
///
/// Opening the site's root URL RESUMES the most recent chat, so every "new"
/// bridge session piles the run into the same conversation. That chat grows past
/// the point where the site accepts messages at all (Enter clears the composer,
/// posts nothing, no wall), which is what killed long runs at turn 6-10. The
/// sidebar's New chat control is the only reliable reset.
async function startNewChat(page) {
  const before = page.url();
  const control = page.locator('button[aria-label="New chat"], a[aria-label="New chat"]').first();
  if (await control.count().catch(() => 0)) {
    await control.click({ timeout: 5000 }).catch(() => {});
  } else {
    await page.goto(chatUrl, { waitUntil: 'domcontentloaded', timeout: 30000 }).catch(() => {});
  }
  // The chat view is /c/<id>; a fresh composer stays on the root URL.
  for (let tick = 0; tick < 20; tick += 1) {
    if (!/\/c\//.test(page.url())) return true;
    await page.waitForTimeout(250);
  }
  return page.url() !== before && !/\/c\//.test(page.url());
}

async function sendMessage(session, prompt) {
  const { page } = session;
  // The site stops accepting messages in a chat after roughly six exchanges and
  // fails SILENTLY (no wall, dropped send), so rotate proactively: sendLogged
  // flags the session when the chat gets long, and the next prompt opens a fresh
  // chat with the transcript carried over. Rotating after a drop, as this used to
  // do, spends a full watchdog cycle on every turn.
  let sent = false;
  if (session.rotate) {
    session.rotate = false;
    sent = await restartChat(session, prompt);
    trace('sendMessage: rotated, sent =', sent);
  }
  // Which markup shape the site uses is decided per read, not cached per
  // session: a fresh chat has no turns to inspect yet.
  // After a rotation the prompt is already posted in a fresh chat, so counting
  // turns now would include the reply that is being generated and the reader
  // would wait for one more. A fresh chat has no earlier turns: baseline 0.
  let previousTurns = sent ? 0 : await assistantCount(page);
  const probe = useApi && !sent ? await readViaApi(page, -1) : null;
  let apiBaseline = probe && probe.count >= 0 ? probe.count : 0;

  // Never type into a site that is still answering: the send is swallowed.
  if (!sent && !(await waitIdle(page, 90000))) {
    throw new BridgeError(503,
      'ChatGPT is still generating the previous turn after 90s; the browser is busy'
      + ' (a second swarm/agent session on this Chrome does this) — let it finish and retry');
  }
  const previousUserTurns = await userTurnCount(page);

  // An overlay over the composer (banners, tooltips) makes Playwright's click
  // time out on a visible element; focusing is enough for insertText to land.
  if (!sent && !(await typePrompt(page, prompt))) {
    throw new BridgeError(503, 'ChatGPT composer did not retain the prompt; wait for the page to load and retry');
  }
  if (!sent) {
    const send = await firstVisible(page, SEND, 5000);
    if (send) {
      await send.click();
    } else {
      await page.keyboard.press('Enter');
    }
  }

  // Did the site start a turn at all? A submission the app ignored (composer
  // rendered the text, its document did not take it) used to burn the whole
  // response window before anyone noticed. Walls are classified first, and Enter
  // is pressed again ONLY while the composer still holds the prompt: re-pressing
  // on a cleared composer submits a second, empty turn.
  if (!sent && !(await turnStarted(page, previousTurns, previousUserTurns, 20000))) {
    const complaint = await pageComplaint(page);
    if (/limit|quota|too many|try again later|usage cap|out of free/i.test(complaint)) {
      throw new BridgeError(429, complaint);
    }
    if (await loginWall(page)) {
      throw new BridgeError(401,
        'ChatGPT asked you to sign in; sign in in the bridge Chrome window and retry');
    }
    const box = await firstVisible(page, COMPOSER, 2000).catch(() => null);
    const holds = box ? await box.evaluate((node, expected) => {
      const squash = (text) => text.replace(/\s+/g, ' ').trim();
      const read = squash('value' in node ? node.value : node.innerText);
      return squash(expected).slice(0, 40).length > 0 && read.startsWith(squash(expected).slice(0, 40));
    }, prompt).catch(() => false) : false;
    if (holds) {
      // A late hydration can swallow the first keypress while the text sits there.
      await page.keyboard.press('Enter');
      if (await turnStarted(page, previousTurns, previousUserTurns, 20000)) {
        // Recovered; fall through to the reply poll.
      } else {
        throw new BridgeError(503,
          'ChatGPT ignored the send: the prompt is in the composer and no generation'
          + ' started; the editor accepted the text without registering it — re-run the turn');
      }
    } else {
      // The site swallowed the submission — the normal outcome of submitting
      // into the window where it is still finishing the previous turn. The
      // composer is empty, so typing again cannot post a second copy.
      let recovered = false;
      if (await waitIdle(page, 30000) && await typePrompt(page, prompt)) {
        await page.keyboard.press('Enter');
        recovered = await turnStarted(page, previousTurns, previousUserTurns, 20000);
      }
      // Dropped twice means this chat is finished with: the site stops accepting
      // new messages in a long chat and shows no wall at all. Carry the
      // transcript into a fresh chat and try there.
      if (!recovered) {
        recovered = await restartChat(session, prompt);
        // The prompt now sits in a fresh chat, so the old chat's turn count is no
        // longer the thing to wait past: the reply there is turn number one.
        if (recovered) {
          previousTurns = 0;
          apiBaseline = 0;
        }
      }
      if (!recovered) {
        throw new BridgeError(503,
          'ChatGPT would not post the prompt: it accepted the text, posted nothing and'
          + ' stayed idle, in a fresh chat too; re-run the turn');
      }
    }
  }

  trace('sendMessage: waiting for reply, sent', sent, 'previousTurns', previousTurns, 'useApi', useApi);
  const deadline = Date.now() + responseTimeoutMs;
  let apiFailures = 0;
  while (Date.now() < deadline) {
    if (useApi && apiFailures < 3) {
      const reply = await readViaApi(page, apiBaseline);
      if (reply?.ready) {
        return { text: reply.text, chat_url: page.url(), model: reply.model, usage: reply.usage,
          source: 'api' };
      }
      if (reply === null || reply.ok === false) apiFailures += 1;
      else if (reply.ready === false) apiFailures = 0;
    }
    if (!useApi || apiFailures >= 3) {
      const remaining = deadline - Date.now();
      if (remaining < 1000) break;
      const reply = await readViaDom(page, previousTurns, deadline);
      if (reply?.text) return { ...reply, chat_url: page.url(), source: 'dom' };
      if (reply?.empty) {
        throw new BridgeError(502,
          'ChatGPT finished its turn with an empty reply; the prompt got no text back'
          + ' (JSON-object requests do this: ask for a plain line instead)');
      }
      break;
    }
    // A stop button is the site's own "still streaming" signal.
    if (await isGenerating(page)) {
      await page.waitForTimeout(500);
      continue;
    }
    // Nothing is generating: fail fast on the walls the site shows inline
    // (sign-in modal, quota banner) instead of burning the whole timeout.
    if (await loginWall(page)) {
      throw new BridgeError(401,
        'ChatGPT asked you to sign in; sign in in the bridge Chrome window and retry');
    }
    const complaint = await pageComplaint(page);
    if (/limit|quota|too many|try again later|usage cap|out of free/i.test(complaint)) {
      throw new BridgeError(429, complaint);
    }
    await page.waitForTimeout(400);
  }

  const reason = await pageComplaint(page);
  if (/limit|quota|too many|try again later|usage cap|out of free/i.test(reason)) {
    throw new BridgeError(429, reason);
  }
  if (/sign (in|up)|log in|authenticat/i.test(reason)) {
    throw new BridgeError(401, reason);
  }
  if (await isGenerating(page)) {
    throw new BridgeError(504,
      `ChatGPT was still generating after ${responseTimeoutMs} ms; raise --response-timeout-ms`);
  }
  throw new BridgeError(504,
    `ChatGPT reply did not arrive within ${responseTimeoutMs} ms${reason ? `: ${reason}` : ''}`);
}

async function readJson(req) {
  let source = '';
  for await (const chunk of req) {
    source += chunk;
    if (source.length > 2_000_000) throw new BridgeError(413, 'request is too large');
  }
  try { return JSON.parse(source); }
  catch { throw new BridgeError(400, 'invalid JSON request'); }
}

function json(res, status, body) {
  res.writeHead(status, { 'content-type': 'application/json; charset=utf-8', 'cache-control': 'no-store' });
  res.end(JSON.stringify(body));
}

const server = http.createServer(async (req, res) => {
  try {
    const path = new URL(req.url, 'http://127.0.0.1').pathname;
    if (req.method === 'GET' && path === '/health') {
      return json(res, 200, {
        ready: true, sessions: sessions.size, access_points: contexts.length,
        chat_url: chatUrl, reply_source: useApi ? 'api' : 'dom',
        // Per access point: open sessions, and which are skipped as unavailable.
        access_point_sessions: accessPointLoad(), max_tabs: maxTabs,
        unavailable: contexts.map((_, index) => index).filter((index) => (deadUntil.get(index) ?? 0) > Date.now()),
      });
    }
    if (req.method === 'POST' && path === '/sessions') {
      const body = await readJson(req);
      const created = await createSession(body.model);
      return json(res, 201, created);
    }
    const match = path.match(/^\/sessions\/([0-9a-f-]+)(\/messages)?$/);
    if (!match) throw new BridgeError(404, 'unknown bridge route');
    const session = sessions.get(match[1]);
    if (!session) throw new BridgeError(404, 'unknown ChatGPT session');
    session.activeAt = Date.now();
    if (req.method === 'DELETE' && !match[2]) {
      sessions.delete(match[1]);
      const chat_deleted = await deleteChat(session.page).catch(() => 'failed');
      await session.page.close();
      return json(res, 200, { closed: true, chat_deleted });
    }
    if (req.method === 'POST' && match[2]) {
      const body = await readJson(req);
      if (typeof body.request_id !== 'string' || typeof body.prompt !== 'string' || !body.prompt.trim()) {
        throw new BridgeError(400, 'request_id and nonempty prompt are required');
      }
      if (!session.replies.has(body.request_id)) {
        const previous = session.queue ?? Promise.resolve();
        const next = previous.catch(() => {}).then(() => sendLogged(session, body.prompt, body.fresh === true));
        session.pending = true;
        session.queue = next;
        next.finally(() => { session.pending = false; }).catch(() => {});
        session.replies.set(body.request_id, next);
        // A failed request must be retryable: keeping its rejected promise would
        // answer every retry of the same request_id with the same stored error.
        next.catch(() => { session.replies.delete(body.request_id); });
      }
      const reply = await session.replies.get(body.request_id);
      return json(res, 200, reply);
    }
    throw new BridgeError(405, 'method not allowed');
  } catch (error) {
    const status = error instanceof BridgeError ? error.status : 500;
    const message = error instanceof Error ? error.message : String(error);
    json(res, status, { error: message });
  }
});

// Tabs abandoned by a crashed Swarm process do not accumulate indefinitely.
const cleanup = setInterval(async () => {
  const cutoff = Date.now() - 30 * 60 * 1000;
  for (const [id, session] of sessions) {
    if (session.activeAt < cutoff && !session.pending) {
      sessions.delete(id);
      // A Swarm crash must not leave the conversation behind either.
      const chat_deleted = await deleteChat(session.page).catch(() => 'failed');
      if (chat_deleted !== 'none') console.log(`idle session ${id}: chat ${chat_deleted}`);
      await session.page.close().catch(() => {});
    }
  }
}, 60_000);

server.listen(port, '127.0.0.1', () => {
  console.log(`ChatGPT bridge ready at http://127.0.0.1:${port} (reply source: ${useApi ? 'api' : 'dom'})`);
  profiles.forEach((profilePath, index) => console.log(`Chrome profile ${index}: ${profilePath}`));
  cdpEndpoints.forEach((endpoint, index) => console.log(`Attached Chrome ${index}: ${endpoint}`));
});

async function shutdown() {
  clearInterval(cleanup);
  server.close();
  for (const session of sessions.values()) await session.page.close().catch(() => {});
  for (const context of profiles.length ? contexts : []) await context.close();
  for (const browser of attachedBrowsers) await browser.close();
  process.exit(0);
}
process.on('SIGINT', shutdown);
process.on('SIGTERM', shutdown);
