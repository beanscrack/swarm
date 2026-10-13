// Local Gemini web transport. One browser tab per Swarm session. The HTTP
// listener is loopback-only and does not expose browser data.
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

const port = Number(option('--port', '8765'));
const cdpEndpoints = options('--cdp');
const profiles = (options('--profile').length ? options('--profile') :
  cdpEndpoints.length ? [] : ['.gemini-browser-profile']).map((path) => resolve(path));
const chatUrl = option('--chat-url', 'https://gemini.google.com/app');
const headless = args.includes('--headless');
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

const contexts = [];
const attachedBrowsers = [];
for (const profilePath of profiles) {
  contexts.push(await chromium.launchPersistentContext(profilePath, {
    channel: 'chrome',
    headless,
    viewport: { width: 1280, height: 900 },
    args: ['--no-first-run'],
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

async function selectModel(page, model) {
  if (!model || /^auto$/i.test(model)) return;
  const label = /argon/i.test(model) ? '4 Argon' : model;
  const picker = page.getByRole('button', { name: /Open mode picker/ });
  await picker.waitFor({ state: 'visible', timeout: 30000 });
  await picker.click();
  const choice = page.getByText(label, { exact: true });
  if (await choice.count() === 0) {
    throw new BridgeError(422, `Gemini model '${model}' is not in the account's model picker`);
  }
  await choice.click();
  const upgrade = page.getByRole('heading', { name: /Unlock Gemini 4/ });
  const locked = await upgrade.waitFor({ state: 'visible', timeout: 1200 })
    .then(() => true).catch(() => false);
  if (locked) {
    await page.getByRole('button', { name: 'Not now' }).click();
    throw new BridgeError(403, `Gemini model '${model}' requires an account upgrade`);
  }
  const selected = page.getByRole('button', { name: /Open mode picker/ });
  const aria = await selected.getAttribute('aria-label');
  if (!aria?.toLowerCase().includes(label.toLowerCase())) {
    throw new BridgeError(403, `Gemini did not select model '${model}'; sign in with an account that has access`);
  }
}

async function createSession(model) {
  const accessPoint = nextAccessPoint++ % contexts.length;
  const context = contexts[accessPoint];
  const page = await context.newPage();
  try {
    await page.goto(chatUrl, { waitUntil: 'domcontentloaded', timeout: 30000 });
    const editor = page.getByRole('textbox', { name: 'Enter a prompt for Gemini' });
    try {
      await editor.waitFor({ state: 'visible', timeout: 30000 });
    } catch {
      throw new BridgeError(401, 'Gemini composer unavailable; sign in in the bridge Chrome window');
    }
    await selectModel(page, model);
    const id = randomUUID();
    sessions.set(id, { page, model, accessPoint, replies: new Map(), activeAt: Date.now(), pending: false });
    return { id, access_point: accessPoint };
  } catch (error) {
    await page.close();
    throw error;
  }
}

async function sendMessage(session, prompt) {
  const { page } = session;
  const replies = page.locator('model-response message-content .markdown');
  const previous = await replies.count();
  const editor = page.getByRole('textbox', { name: 'Enter a prompt for Gemini' });
  await editor.fill(prompt);
  await page.getByRole('button', { name: 'Send message' }).click();
  try {
    await page.waitForFunction((oldCount) => {
      const answers = document.querySelectorAll('model-response message-content .markdown');
      if (answers.length <= oldCount) return false;
      const latest = answers[answers.length - 1];
      const stop = document.querySelector('button[aria-label="Stop response"]');
      return latest.getAttribute('aria-busy') === 'false'
        && latest.innerText.trim().length > 0 && !stop;
    }, previous, { timeout: responseTimeoutMs, polling: 250 });
  } catch {
    const alert = await page.locator('[role="alert"]').allTextContents().catch(() => []);
    const reason = alert.map((part) => part.trim()).filter(Boolean).join(' ');
    if (/limit|quota|too many/i.test(reason)) {
      throw new BridgeError(429, reason);
    }
    throw new BridgeError(504, `Gemini reply did not finish within ${responseTimeoutMs} ms${reason ? `: ${reason}` : ''}`);
  }
  const text = (await replies.last().evaluate((element) => {
    // Gemini renders fenced code as a custom <code-block>. innerText drops the
    // fence and adds "Code snippet", which would hide Swarm tool/plan JSON.
    let rendered = element.innerText;
    for (const block of element.querySelectorAll('code-block')) {
      const code = block.querySelector('pre')?.textContent?.trimEnd();
      if (!code) continue;
      let language = '';
      try {
        const value = JSON.parse(code);
        if (value && typeof value === 'object' &&
            ('tool' in value || 'tool_calls' in value || 'plan' in value || 'steps' in value)) {
          language = 'swarm';
        }
      } catch {}
      rendered = rendered.replace(block.innerText, `\`\`\`${language}\n${code}\n\`\`\``);
    }
    return rendered.trim();
  })).trim();
  if (!text) throw new BridgeError(502, 'Gemini returned an empty reply');
  return { text, chat_url: page.url() };
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
      return json(res, 200, { ready: true, sessions: sessions.size, access_points: contexts.length, chat_url: chatUrl });
    }
    if (req.method === 'POST' && path === '/sessions') {
      const body = await readJson(req);
      const created = await createSession(body.model);
      return json(res, 201, created);
    }
    const match = path.match(/^\/sessions\/([0-9a-f-]+)(\/messages)?$/);
    if (!match) throw new BridgeError(404, 'unknown bridge route');
    const session = sessions.get(match[1]);
    if (!session) throw new BridgeError(404, 'unknown Gemini session');
    session.activeAt = Date.now();
    if (req.method === 'DELETE' && !match[2]) {
      sessions.delete(match[1]);
      await session.page.close();
      return json(res, 200, { closed: true });
    }
    if (req.method === 'POST' && match[2]) {
      const body = await readJson(req);
      if (typeof body.request_id !== 'string' || typeof body.prompt !== 'string' || !body.prompt.trim()) {
        throw new BridgeError(400, 'request_id and nonempty prompt are required');
      }
      if (!session.replies.has(body.request_id)) {
        const previous = session.queue ?? Promise.resolve();
        const next = previous.catch(() => {}).then(() => sendMessage(session, body.prompt));
        session.pending = true;
        session.queue = next;
        next.finally(() => { session.pending = false; }).catch(() => {});
        session.replies.set(body.request_id, next);
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
      await session.page.close().catch(() => {});
    }
  }
}, 60_000);

server.listen(port, '127.0.0.1', () => {
  console.log(`Gemini bridge ready at http://127.0.0.1:${port}`);
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
