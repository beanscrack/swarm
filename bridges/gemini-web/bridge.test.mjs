import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';

const html = `<!doctype html><title>Gemini fixture</title>
<button aria-label="Open mode picker, currently Gemini Auto" id="picker">Gemini Auto</button>
<div id="menu" hidden><button id="argon">4 Argon</button></div>
<div role="textbox" aria-label="Enter a prompt for Gemini" contenteditable="true"></div>
<button aria-label="Send message" id="send">Send</button>
<div id="turns"></div>
<script>
  const session = crypto.randomUUID();
  picker.onclick = () => menu.hidden = false;
  argon.onclick = () => { picker.setAttribute('aria-label', 'Open mode picker, currently Gemini 4 Argon'); menu.hidden = true; };
  send.onclick = () => {
    const prompt = document.querySelector('[role=textbox]').innerText;
    document.querySelector('[role=textbox]').innerText = '';
    const turn = document.createElement('model-response');
    turn.innerHTML = '<message-content><div class="markdown" aria-busy="true"></div></message-content>';
    turns.append(turn);
    const stop = document.createElement('button');
    stop.setAttribute('aria-label', 'Stop response');
    document.body.append(stop);
    setTimeout(() => {
      if (prompt === 'code block') {
        const block = document.createElement('code-block');
        const header = document.createElement('div');
        header.innerText = 'Code snippet';
        const pre = document.createElement('pre');
        pre.textContent = '{"tool":{"name":"write_file","arguments":{"path":"x.txt","content":"ok"}}}';
        block.append(header, pre);
        turn.querySelector('.markdown').append(block);
      } else {
        turn.querySelector('.markdown').innerText = 'REPLY:' + prompt;
      }
      turn.querySelector('.markdown').setAttribute('aria-busy','false');
      stop.remove();
      history.replaceState(null, '', '/app/' + session);
    }, 40);
  };
</script>`;

async function listen(server) {
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return server.address().port;
}

async function eventually(callback, timeoutMs = 15000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try { return await callback(); } catch { await new Promise((resolve) => setTimeout(resolve, 100)); }
  }
  return callback();
}

test('two sessions use independent chat pages and idempotent requests', { timeout: 60000 }, async () => {
  const fixture = createServer((_req, res) => {
    res.writeHead(200, { 'content-type': 'text/html' });
    res.end(html);
  });
  const fixturePort = await listen(fixture);
  const freePort = createServer();
  const bridgePort = await listen(freePort);
  await new Promise((resolve) => freePort.close(resolve));
  const profile = await mkdtemp(join(tmpdir(), 'swarm-gemini-test-'));
  const secondProfile = await mkdtemp(join(tmpdir(), 'swarm-gemini-test-'));
  const bridge = spawn(process.execPath, ['bridge.mjs', '--headless', '--profile', profile,
    '--profile', secondProfile,
    '--port', String(bridgePort), '--chat-url', `http://127.0.0.1:${fixturePort}/app`],
  { cwd: import.meta.dirname, stdio: ['ignore', 'pipe', 'pipe'] });
  let logs = '';
  bridge.stdout.on('data', (data) => { logs += data; });
  bridge.stderr.on('data', (data) => { logs += data; });
  const base = `http://127.0.0.1:${bridgePort}`;
  async function post(path, body) {
    const response = await fetch(`${base}${path}`, {
      method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body),
    });
    const json = await response.json();
    assert.ok(response.ok, `${response.status}: ${JSON.stringify(json)}\n${logs}`);
    return json;
  }
  try {
    await eventually(async () => {
      const response = await fetch(`${base}/health`);
      assert.equal(response.status, 200);
    });
    const [a, b] = await Promise.all([
      post('/sessions', { model: '4 Argon' }),
      post('/sessions', { model: '4 Argon' }),
    ]);
    assert.notEqual(a.id, b.id);
    assert.notEqual(a.access_point, b.access_point);
    const [one, two] = await Promise.all([
      post(`/sessions/${a.id}/messages`, { request_id: 'a-1', prompt: 'agent one' }),
      post(`/sessions/${b.id}/messages`, { request_id: 'b-1', prompt: 'agent two' }),
    ]);
    assert.equal(one.text, 'REPLY:agent one');
    assert.equal(two.text, 'REPLY:agent two');
    assert.notEqual(one.chat_url, two.chat_url);
    const replay = await post(`/sessions/${a.id}/messages`, { request_id: 'a-1', prompt: 'wrong duplicate' });
    assert.deepEqual(replay, one);
    const followup = await post(`/sessions/${a.id}/messages`, { request_id: 'a-2', prompt: 'tool result' });
    assert.equal(followup.text, 'REPLY:tool result');
    const code = await post(`/sessions/${a.id}/messages`, { request_id: 'a-3', prompt: 'code block' });
    assert.match(code.text, /^```swarm\n\{"tool"/);
    await Promise.all([a, b].map(async ({ id }) => {
      const response = await fetch(`${base}/sessions/${id}`, { method: 'DELETE' });
      assert.equal(response.status, 200);
    }));
    const health = await (await fetch(`${base}/health`)).json();
    assert.equal(health.sessions, 0);
    assert.equal(health.access_points, 2);
  } catch (error) {
    throw new Error(`${error.message}\nBridge output:\n${logs}`, { cause: error });
  } finally {
    if (bridge.exitCode === null) {
      const exited = new Promise((resolve) => bridge.once('exit', resolve));
      bridge.kill();
      await exited;
    }
    await new Promise((resolve) => fixture.close(resolve));
    await rm(profile, { recursive: true, force: true });
    await rm(secondProfile, { recursive: true, force: true });
  }
});
