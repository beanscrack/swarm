import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';

import { fixtureServer } from './fixture.mjs';

async function listen(server) {
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return server.address().port;
}

async function freePort() {
  const server = createServer();
  const port = await listen(server);
  await new Promise((resolve) => server.close(resolve));
  return port;
}

async function eventually(callback, timeoutMs = 15000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try { return await callback(); } catch { await new Promise((resolve) => setTimeout(resolve, 100)); }
  }
  return callback();
}

async function withBridge(t, { fixture, extra = [], chatPath = '/app' }, body) {
  const fixturePort = await listen(fixture.server);
  const bridgePort = await freePort();
  const profiles = await Promise.all([
    mkdtemp(join(tmpdir(), 'swarm-chatgpt-test-')),
    mkdtemp(join(tmpdir(), 'swarm-chatgpt-test-')),
  ]);
  // Chrome keeps a lock on files inside the profile while it shuts down, and
  // Windows refuses unlink() on them (EBUSY), which used to fail passing tests.
  const removeProfile = async (path) => {
    for (let attempt = 0; attempt < 12; attempt += 1) {
      try { await rm(path, { recursive: true, force: true }); return; }
      catch { await new Promise((resolve) => setTimeout(resolve, 400)); }
    }
    await rm(path, { recursive: true, force: true }).catch(() => {});
  };
  const bridge = spawn(process.execPath, ['bridge.mjs', '--headless',
    '--profile', profiles[0], '--profile', profiles[1],
    '--port', String(bridgePort), '--response-timeout-ms', '6000',
    '--chat-url', `http://127.0.0.1:${fixturePort}${chatPath}`, ...extra],
  { cwd: import.meta.dirname, stdio: ['ignore', 'pipe', 'pipe'] });
  let logs = '';
  bridge.stdout.on('data', (data) => { logs += data; });
  bridge.stderr.on('data', (data) => { logs += data; });
  const base = `http://127.0.0.1:${bridgePort}`;
  async function post(path, payload) {
    const response = await fetch(`${base}${path}`, {
      method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(payload),
    });
    const json = await response.json();
    assert.ok(response.ok, `${response.status}: ${JSON.stringify(json)}\n${logs}`);
    return json;
  }
  async function status(path, payload, method = 'POST') {
    const response = await fetch(`${base}${path}`, {
      method, headers: { 'content-type': 'application/json' },
      body: method === 'GET' || method === 'DELETE' ? undefined : JSON.stringify(payload),
    });
    return { status: response.status, body: await response.json() };
  }
  try {
    await eventually(async () => {
      const response = await fetch(`${base}/health`);
      assert.equal(response.status, 200);
    });
    await body({ base, post, status, logs: () => logs, fixturePort });
  } catch (error) {
    throw new Error(`${error.message}\nBridge output:\n${logs}`, { cause: error });
  } finally {
    if (bridge.exitCode === null) {
      const exited = new Promise((resolve) => bridge.once('exit', resolve));
      bridge.kill();
      await exited;
    }
    await new Promise((resolve) => fixture.server.close(resolve));
    await Promise.all(profiles.map(removeProfile));
  }
}

test('sessions use independent chats, the API reply path, and idempotent requests',
  { timeout: 90000 }, async (t) => {
  const fixture = fixtureServer();
  await withBridge(t, { fixture }, async ({ base, post }) => {
    const [a, b] = await Promise.all([
      post('/sessions', { model: 'auto' }),
      post('/sessions', { model: 'auto' }),
    ]);
    assert.notEqual(a.id, b.id);
    assert.notEqual(a.access_point, b.access_point);

    const [one, two] = await Promise.all([
      post(`/sessions/${a.id}/messages`, { request_id: 'a-1', prompt: 'agent one' }),
      post(`/sessions/${b.id}/messages`, { request_id: 'b-1', prompt: 'agent two' }),
    ]);
    assert.equal(one.text, 'REPLY:agent one');
    assert.equal(two.text, 'REPLY:agent two');
    assert.equal(one.source, 'api');
    assert.match(one.chat_url, /\/c\/[0-9a-f-]{16,}/);
    assert.notEqual(one.chat_url, two.chat_url);
    assert.equal(one.model, 'gpt-5.1');
    assert.deepEqual(one.usage, { input_tokens: 120, output_tokens: 30, cached_input_tokens: 0 });

    const replay = await post(`/sessions/${a.id}/messages`, { request_id: 'a-1', prompt: 'wrong duplicate' });
    assert.deepEqual(replay, one);

    const followup = await post(`/sessions/${a.id}/messages`, { request_id: 'a-2', prompt: 'tool result' });
    assert.equal(followup.text, 'REPLY:tool result');

    const code = await post(`/sessions/${a.id}/messages`, { request_id: 'a-3', prompt: 'code block' });
    assert.match(code.text, /^```swarm\n\{"tool"/);
    assert.equal(JSON.parse(code.text.replace(/^```swarm\n|\n```$/g, '')).tool.name, 'write_file');

    await Promise.all([a, b].map(async ({ id }) => {
      const response = await fetch(`${base}/sessions/${id}`, { method: 'DELETE' });
      assert.equal(response.status, 200);
    }));
    const health = await (await fetch(`${base}/health`)).json();
    assert.equal(health.sessions, 0);
    assert.equal(health.access_points, 2);
  });
});

test('model picker selection, plan-gated model, unknown model', { timeout: 90000 }, async (t) => {
  const fixture = fixtureServer();
  await withBridge(t, { fixture }, async ({ post, status }) => {
    const picked = await post('/sessions', { model: 'GPT-5.1 Thinking' });
    assert.ok(picked.id);

    const locked = await status('/sessions', { model: 'o3 Pro' });
    assert.equal(locked.status, 403);
    assert.match(locked.body.error, /requires a plan/);

    const unknown = await status('/sessions', { model: 'GPT-99 Ultra' });
    assert.equal(unknown.status, 422);
    assert.match(unknown.body.error, /not in the account's model picker/);
  });
});

test('sessions are balanced over access points by load, and a freed slot is reused',
  { timeout: 120000 }, async (t) => {
  const fixture = fixtureServer();
  await withBridge(t, { fixture }, async ({ base, post }) => {
    const made = [];
    for (let i = 0; i < 4; i += 1) made.push(await post('/sessions', { model: 'auto' }));
    const count = (index) => made.filter((s) => s.access_point === index).length;
    assert.deepEqual([count(0), count(1)], [2, 2], 'two browsers share four sessions evenly');
    let health = await (await fetch(`${base}/health`)).json();
    assert.deepEqual(health.access_point_sessions, [2, 2]);
    assert.deepEqual(health.unavailable, []);

    // Free a slot on one browser: the next session goes there, not round-robin.
    const freed = made[0];
    assert.equal((await fetch(`${base}/sessions/${freed.id}`, { method: 'DELETE' })).status, 200);
    const next = await post('/sessions', { model: 'auto' });
    assert.equal(next.access_point, freed.access_point, 'the least loaded browser takes it');
    health = await (await fetch(`${base}/health`)).json();
    assert.deepEqual(health.access_point_sessions, [2, 2]);
  });
});

test('--max-tabs refuses a session when every access point is full',
  { timeout: 120000 }, async (t) => {
  const fixture = fixtureServer();
  await withBridge(t, { fixture, extra: ['--max-tabs', '1'] }, async ({ post, status }) => {
    await post('/sessions', { model: 'auto' });
    await post('/sessions', { model: 'auto' });
    const third = await status('/sessions', { model: 'auto' });
    assert.equal(third.status, 503);
    assert.match(third.body.error, /1-tab limit/);
  });
});

test('fresh-chat requests run in parallel across access points and each gets its own chat',
  { timeout: 120000 }, async (t) => {
  const fixture = fixtureServer();
  await withBridge(t, { fixture }, async ({ post }) => {
    const sessions = [];
    for (let i = 0; i < 4; i += 1) sessions.push(await post('/sessions', { model: 'auto' }));
    // Two rounds, so every session also shows that a second fresh request in the
    // same tab opens another chat instead of continuing the first.
    for (const round of [1, 2]) {
      const replies = await Promise.all(sessions.map((s, i) =>
        post(`/sessions/${s.id}/messages`,
          { request_id: `r${round}-${i}`, prompt: `agent ${i} round ${round}`, fresh: true })));
      replies.forEach((reply, i) => assert.equal(reply.text, `REPLY:agent ${i} round ${round}`));
      assert.equal(new Set(replies.map((r) => r.chat_url)).size, 4, 'four separate chats');
      if (round === 1) globalThis.__firstChats = replies.map((r) => r.chat_url);
      else replies.forEach((r, i) => assert.notEqual(r.chat_url, globalThis.__firstChats[i], 'a new chat per request'));
    }
  });
});

test('site rate limit surfaces as 429', { timeout: 90000 }, async (t) => {
  const fixture = fixtureServer();
  await withBridge(t, { fixture }, async ({ post, status }) => {
    const session = await post('/sessions', { model: 'auto' });
    const reply = await status(`/sessions/${session.id}/messages`,
      { request_id: 'r-1', prompt: 'rate limit please' });
    assert.equal(reply.status, 429);
    assert.match(reply.body.error, /limit/i);
  });
});

test('sign-in modal surfaces as 401 without waiting out the timeout',
  { timeout: 90000 }, async (t) => {
  const fixture = fixtureServer();
  await withBridge(t, { fixture, extra: ['--response-timeout-ms', '120000'] },
    async ({ post, status }) => {
    const session = await post('/sessions', { model: 'auto' });
    const started = Date.now();
    const reply = await status(`/sessions/${session.id}/messages`,
      { request_id: 'r-login', prompt: 'sign in wall please' });
    assert.equal(reply.status, 401);
    assert.match(reply.body.error, /sign in/i);
    // Detection is near-instant (2-3s alone); a budget this far under the
    // response timeout is the point of the test, and it stays meaningful when
    // the machine is busy launching other Chrome instances.
    assert.ok(Date.now() - started < 20000, 'must fail fast, not burn the response timeout');
  });
});

test('DOM fallback reads rendered turns without the conversation API',
  { timeout: 90000 }, async (t) => {
  const fixture = fixtureServer();
  await withBridge(t, { fixture, extra: ['--no-api'] }, async ({ base, post }) => {
    const health = await (await fetch(`${base}/health`)).json();
    assert.equal(health.reply_source, 'dom');
    const session = await post('/sessions', { model: 'auto' });
    const plain = await post(`/sessions/${session.id}/messages`, { request_id: 'd-1', prompt: 'hello' });
    assert.equal(plain.text, 'REPLY:hello');
    assert.equal(plain.source, 'dom');
    const code = await post(`/sessions/${session.id}/messages`, { request_id: 'd-2', prompt: 'code block' });
    assert.match(code.text, /^```swarm\n\{"tool"/, `DOM text was: ${code.text}`);
    assert.equal(JSON.parse(code.text.replace(/^```swarm\n|\n```$/g, '')).tool.name, 'write_file');
  });
});

test('the 2026 turn markup is read through its turn-state marker',
  { timeout: 90000 }, async (t) => {
  const fixture = fixtureServer();
  await withBridge(t, { fixture, extra: ['--no-api'], chatPath: '/app?shape=2026' },
    async ({ post }) => {
      const session = await post('/sessions', { model: 'auto' });
      const reply = await post(`/sessions/${session.id}/messages`,
        { request_id: 'shape-1', prompt: 'ping' });
      assert.equal(reply.text, 'REPLY:ping');
      // The hidden "ChatGPT said:" heading is inside the unit: it must not be
      // read back as model output.
      assert.ok(!reply.text.includes('ChatGPT said:'));
    });
});

test('closing a session deletes the chat from the site',
  { timeout: 90000 }, async (t) => {
  const fixture = fixtureServer();
  await withBridge(t, { fixture, extra: ['--no-api'] },
    async ({ post, status, fixturePort }) => {
      const list = async () => (await (await fetch(
        `http://127.0.0.1:${fixturePort}/fixture/conversations`)).json()).ids;
      const session = await post('/sessions', { model: 'auto' });
      const reply = await post(`/sessions/${session.id}/messages`,
        { request_id: 'del-1', prompt: 'ping' });
      const chatId = reply.chat_url.match(/\/c\/([0-9a-f-]{36})/)[1];
      assert.ok((await list()).includes(chatId), 'the run created a chat');
      const closed = await status(`/sessions/${session.id}`, null, 'DELETE');
      assert.equal(closed.status, 200);
      assert.equal(closed.body.chat_deleted, 'api');
      const after = await list();
      assert.ok(!after.includes(chatId),
        `chat ${chatId} survived close (open: ${after.join(',')}), chat_deleted=${closed.body.chat_deleted}`);
    });
});

test('a JSON file the model attaches is read back as the tool request',
  { timeout: 90000 }, async (t) => {
  const fixture = fixtureServer();
  await withBridge(t, { fixture, extra: ['--no-api'] }, async ({ post }) => {
    const session = await post('/sessions', { model: 'auto' });
    const reply = await post(`/sessions/${session.id}/messages`,
      { request_id: 'file-1', prompt: 'json file' });
    // The model wrote the request into a file instead of typing a fenced block.
    assert.match(reply.text, /```swarm/);
    assert.match(reply.text, /write_file/);
    assert.ok(!reply.text.includes('Download swarm.json'), 'chip label is not model output');
  });
});

test('the sidebar menu deletes when the site refuses the API delete',
  { timeout: 90000 }, async (t) => {
  // The live 2026 site answers 404 conversation_inaccessible for this account,
  // so closing a session has to fall back to the chat row's own menu.
  const fixture = fixtureServer();
  await withBridge(t, { fixture, extra: ['--no-api'], chatPath: '/app?shape=2026&nodelete=1' },
    async ({ post, status, fixturePort }) => {
      const list = async () => (await (await fetch(
        `http://127.0.0.1:${fixturePort}/fixture/conversations`)).json()).ids;
      const session = await post('/sessions', { model: 'auto' });
      const reply = await post(`/sessions/${session.id}/messages`,
        { request_id: 'menu-1', prompt: 'ping' });
      const chatId = reply.chat_url.match(/\/c\/([0-9a-f-]{36})/)[1];
      assert.ok((await list()).includes(chatId), 'the run created a chat');
      const closed = await status(`/sessions/${session.id}`, null, 'DELETE');
      assert.equal(closed.status, 200);
      assert.equal(closed.body.chat_deleted, 'menu',
        `expected the menu fallback, got ${closed.body.chat_deleted}`);
      const after = await list();
      assert.ok(!after.includes(chatId), `chat ${chatId} survived the menu delete`);
    });
});
