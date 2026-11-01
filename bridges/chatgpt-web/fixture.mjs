// Stands in for chatgpt.com so the bridge can be tested offline: the composer
// and turn DOM, the /backend-api/conversation/{id} JSON the bridge prefers to
// read, and a model picker with one plan-gated model.
//
// Run directly (`node fixture.mjs --port 8790`) to drive the bridge by hand:
//   node fixture.mjs --port 8790
//   node bridge.mjs --profile /tmp/p --port 8766 --chat-url http://127.0.0.1:8790/app
import { createServer } from 'node:http';
import { randomUUID } from 'node:crypto';

export const TOOL_JSON = '{"tool":{"name":"write_file","arguments":{"path":"x.txt","content":"ok"}}}';

const html = `<!doctype html><title>ChatGPT fixture</title>
<header><button id="switcher" aria-haspopup="menu">GPT-5.1</button>
<div id="menu" hidden role="menu">
  <button role="menuitem" data-model="GPT-5.1">GPT-5.1</button>
  <button role="menuitem" data-model="GPT-5.1 Thinking">GPT-5.1 Thinking<br><small>For complex tasks</small></button>
  <button role="menuitem" data-model="o3 Pro">o3 Pro</button>
</div></header>
<main id="turns"></main>
<div id="prompt-textarea" role="textbox" contenteditable="true" aria-label="Chat input"></div>
<button id="send" aria-label="Send prompt">Send</button>
<div id="wall" hidden role="alert">Upgrade to Pro to use o3 Pro</div>
<nav id="side"></nav>
<div id="chatmenu" hidden role="menu"><button role="menuitem">Rename</button><button role="menuitem">Archive</button><button role="menuitem">Delete</button></div>
<div id="chatsheet" hidden role="dialog"><p>Delete this chat?</p><button>Delete</button></div>
<script>
  const conv = 'CONV_PLACEHOLDER';
  // /app?shape=2026 renders the current (2026) ChatGPT markup: role-keyed turn
  // units, a hidden "ChatGPT said:" label, and completion on the turn wrapper.
  const shape = new URL(location.href).searchParams.get('shape');
  // /app?signedout=1: a logged-out visitor still gets a composer, plus Log in /
  // Sign up buttons in the header, as on the live site.
  if (new URL(location.href).searchParams.get('signedout')) {
    for (const label of ['Log in', 'Sign up']) {
      const button = document.createElement('button');
      button.textContent = label;
      document.querySelector('header').append(button);
    }
  }
  // Sidebar row + actions menu, mirroring the live site's pairing (chat link and
  // "Chat actions" button 1:1 in DOM order) so the menu delete path is testable.
  const side = document.getElementById('side');
  const chatmenu = document.getElementById('chatmenu');
  const chatsheet = document.getElementById('chatsheet');
  for (const item of chatmenu.querySelectorAll('[role=menuitem]')) {
    item.onclick = () => {
      chatmenu.hidden = true;
      if (item.textContent.trim() === 'Delete') chatsheet.hidden = false;
    };
  }
  chatsheet.querySelector('button').onclick = async () => {
    chatsheet.hidden = true;
    await fetch('/fixture/conversation-delete', { method: 'POST',
      headers: { 'content-type': 'application/json' }, body: JSON.stringify({ id: conv }) });
    var rowLink = document.querySelector('a[href="/c/' + conv + '"]');
    var row = rowLink && rowLink.closest('div');
    if (row) row.remove();
  };
  switcher.onclick = () => { menu.hidden = false; };
  for (const item of menu.querySelectorAll('[role=menuitem]')) {
    item.onclick = () => {
      menu.hidden = true;
      if (item.dataset.model === 'o3 Pro') { wall.hidden = false; return; }
      switcher.textContent = item.dataset.model;
    };
  }
  window.addEventListener('keydown', (event) => {
    if (event.key === 'Escape') { menu.hidden = true; wall.hidden = true; }
  });
  send.onclick = async () => {
    const editor = document.getElementById('prompt-textarea');
    const prompt = editor.innerText;
    editor.innerText = '';
    history.replaceState(null, '', '/c/' + conv);
    if (!document.querySelector('a[href="/c/' + conv + '"]')) {
      var chatRow = document.createElement('div');
      var chatLink = document.createElement('a');
      chatLink.href = '/c/' + conv;
      chatLink.textContent = 'chat';
      var chatTrigger = document.createElement('button');
      chatTrigger.setAttribute('aria-label', 'Chat actions');
      chatTrigger.onclick = () => { chatmenu.hidden = false; };
      chatRow.append(chatLink, chatTrigger);
      side.append(chatRow);
    }
    if (prompt.startsWith('sign in')) {
      const modal = document.createElement('div');
      modal.setAttribute('role', 'dialog');
      modal.textContent = 'Continue to ChatGPT — log in to keep this conversation';
      document.body.append(modal);
      return;
    }
    if (prompt.startsWith('rate limit')) {
      const alert = document.createElement('div');
      alert.setAttribute('role', 'alert');
      alert.textContent = 'You have hit the limit for GPT-5.1. Try again later.';
      document.body.append(alert);
      return;
    }
    // The 2026 turn shape is chosen at load: the page rewrites its own URL to
    // /c/<id> on the first send, so the query is gone by then.
    let bubble;
    let turn = null;
    if (shape === '2026') {
      turn = document.createElement('div');
      turn.setAttribute('data-turn-key', 't-' + Date.now());
      turn.setAttribute('data-talvt-turn-state', 'generating');
      const unit = document.createElement('div');
      unit.setAttribute('data-content-search-unit-key', 'fallback-turn-0:2:assistant');
      const label = document.createElement('h4');
      label.className = 'sr-only';
      label.setAttribute('data-conversation-role', 'assistant');
      label.textContent = 'ChatGPT said:';
      bubble = document.createElement('div');
      unit.append(label, bubble);
      turn.append(unit);
      turns.append(turn);
    } else {
      const wrap = document.createElement('article');
      wrap.innerHTML = '<div data-message-author-role="assistant"><div class="markdown" aria-busy="true"></div></div>';
      turns.append(wrap);
      bubble = wrap.querySelector('.markdown');
    }
    bubble.setAttribute('aria-busy', 'true');
    const stop = document.createElement('button');
    stop.setAttribute('aria-label', 'Stop generating');
    document.body.append(stop);
    await fetch('/fixture/turn', { method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ id: conv, prompt }) });
    if (prompt.startsWith('connector ')) {
      // A connector asks before it acts: the answer waits for a button, as the live
      // site does. The reply says which button was pressed.
      const asks = prompt.includes('write') ? 'ChatGPT wants to use GitHub to post a comment on pull request 12 in coalign/platform.'
        : prompt.includes('other') ? 'ChatGPT wants to use Google Drive to read the file plan.md.'
        : 'ChatGPT wants to use GitHub to read the open pull requests in coalign/platform.';
      const card = document.createElement('div');
      card.setAttribute('role', 'dialog');
      const words = document.createElement('p');
      words.textContent = asks;
      card.append(words);
      for (const name of ['Allow', 'Always allow', 'Decline']) {
        const choice = document.createElement('button');
        choice.textContent = name;
        choice.onclick = () => {
          card.remove();
          bubble.innerText = 'CONNECTOR:' + name;
          bubble.setAttribute('aria-busy', 'false');
          turn?.setAttribute('data-talvt-turn-state', 'complete');
          stop.remove();
        };
        card.append(choice);
      }
      document.body.append(card);
      return;
    }
    setTimeout(() => {
      if (/TOOL RESULT/.test(prompt)) {
        bubble.innerText = 'The file is verified. Done.';
      } else if (/write hello\\.txt/.test(prompt)) {
        const header = document.createElement('div'); header.textContent = 'swarm';
        const copy = document.createElement('button'); copy.textContent = 'Copy';
        const pre = document.createElement('pre');
        const code = document.createElement('code');
        code.textContent = '{"tool":{"name":"write_file","arguments":{"path":"hello.txt","content":"hello from chatgpt"}}}';
        pre.append(code);
        bubble.append(header, copy, pre);
      } else if (prompt.startsWith('code block')) {
        const header = document.createElement('div'); header.textContent = 'swarm';
        const copy = document.createElement('button'); copy.textContent = 'Copy';
        const pre = document.createElement('pre');
        const code = document.createElement('code');
        code.textContent = 'TOOL_PLACEHOLDER';
        pre.append(code);
        bubble.append(header, copy, pre);
      } else if (prompt.startsWith('json file')) {
        // The model prefers to hand over a file: the bridge has to read it.
        bubble.innerText = 'I put the request in a file.';
        const link = document.createElement('a');
        link.setAttribute('download', 'swarm.json');
        link.href = '/fixture/swarm.json';
        link.textContent = 'Download swarm.json';
        bubble.append(link);
      } else if (prompt.startsWith('make document')) {
        // The model "creates a file" in its own sandbox: only a card is in the reply, and the text
        // lives in a side panel that opens when the preview button is pressed.
        bubble.innerText = 'Created REPORT.md';
        const name = document.createElement('span');
        name.setAttribute('role', 'link');
        name.setAttribute('aria-label', 'Open REPORT.md');
        name.textContent = 'REPORT.md';
        const preview = document.createElement('button');
        preview.setAttribute('aria-label', 'Open preview of REPORT.md');
        preview.onclick = () => {
          const panel = document.createElement('aside');
          panel.innerText = 'REPORT.md\\n\\nDOC-LINE-1 the finding\\nDOC-LINE-2 the detail';
          document.body.append(panel);
        };
        const download = document.createElement('button');
        download.setAttribute('aria-label', 'Download file');
        bubble.append(name, preview, download);
      } else {
        bubble.innerText = 'REPLY:' + prompt.slice(0, 400);
      }
      if (turn) {
        // 2026 attaches canvas chips inside the turn unit; the bridge must not
        // report them as model output.
        const chip = document.createElement('a');
        chip.setAttribute('download', 'hello.txt');
        chip.href = '#';
        chip.textContent = 'Download hello.txt';
        const open = document.createElement('button');
        open.textContent = 'Open file';
        bubble.append(chip, open);
      }
      bubble.setAttribute('aria-busy', 'false');
      turn?.setAttribute('data-talvt-turn-state', 'complete');
      stop.remove();
    }, 60);
  };
</script>`;

/// The reply the fake site gives for a prompt, as ChatGPT stores it: markdown.
export function fixtureReply(prompt) {
  if (/TOOL RESULT/.test(prompt)) return 'The file is verified. Done.';
  if (/write hello\.txt/.test(prompt)) {
    return '```swarm\n{"tool":{"name":"write_file","arguments":{"path":"hello.txt","content":"hello from chatgpt"}}}\n```';
  }
  if (prompt.startsWith('code block')) return '```swarm\n' + TOOL_JSON + '\n```';
  return 'REPLY:' + prompt.slice(0, 400);
}

export function fixtureServer() {
  const conversations = new Map();
  const ids = new Set();
  // Chats whose conversation id was minted under ?nodelete=1 refuse the API
  // delete, so a test can drive the sidebar-menu fallback path.
  const apiDeleteRefused = new Set();
  const server = createServer(async (req, res) => {
    const url = new URL(req.url, 'http://127.0.0.1');
    const path = url.pathname;
    if (path === '/fixture/conversation-delete' && req.method === 'POST') {
      let body = '';
      for await (const chunk of req) body += chunk;
      let id = '';
      try { id = JSON.parse(body).id; } catch { id = ''; }
      ids.delete(id);
      conversations.delete(id);
      res.writeHead(204);
      res.end();
      return;
    }
    if (path === '/fixture/swarm.json') {
      // The file the model attaches instead of typing a fenced block.
      res.writeHead(200, { 'content-type': 'application/json' });
      return res.end(TOOL_JSON);
    }
    if (path === '/app') {
      const id = randomUUID();
      ids.add(id);
      conversations.set(id, []);
      if (url.searchParams.get('nodelete')) apiDeleteRefused.add(id);
      res.writeHead(200, { 'content-type': 'text/html' });
      return res.end(html.replace('CONV_PLACEHOLDER', id).replace('TOOL_PLACEHOLDER', TOOL_JSON));
    }
    if (path === '/fixture/turn' && req.method === 'POST') {
      let body = '';
      for await (const chunk of req) body += chunk;
      const { id, prompt } = JSON.parse(body);
      const turns = conversations.get(id) ?? [];
      const stamp = Date.now() / 1000;
      turns.push(
        { author: { role: 'user' }, content: { content_type: 'text', parts: [prompt] }, create_time: stamp },
        { author: { role: 'assistant' },
          content: { content_type: 'text', parts: [fixtureReply(prompt)] },
          metadata: { model_slug: 'gpt-5.1', token_count: { prompt_tokens: 120, completion_tokens: 30 } },
          create_time: stamp + 0.1 });
      conversations.set(id, turns);
      res.writeHead(204);
      return res.end();
    }
    if (path === '/fixture/conversations') {
      res.writeHead(200, { 'content-type': 'application/json' });
      return res.end(JSON.stringify({ ids: [...ids] }));
    }
    const match = path.match(/^\/backend-api\/conversation\/([0-9a-f-]+)$/);
    if (match && req.method === 'DELETE' && apiDeleteRefused.has(match[1])) {
      // Same answer the live site gives this account: the API path is closed.
      res.writeHead(404, { 'content-type': 'application/json' });
      return res.end(JSON.stringify({ error: { code: 'conversation_inaccessible' } }));
    }
    if (match && ids.has(match[1]) && req.method === 'DELETE') {
      // The bridge deletes the chat when the Swarm session closes.
      ids.delete(match[1]);
      conversations.delete(match[1]);
      res.writeHead(200, { 'content-type': 'application/json' });
      return res.end(JSON.stringify({ success: true }));
    }
    if (match && ids.has(match[1])) {
      const turns = conversations.get(match[1]) ?? [];
      const messages = {};
      turns.forEach((turn, index) => { turn.id = `m${index}`; messages[turn.id] = turn; });
      res.writeHead(200, { 'content-type': 'application/json' });
      return res.end(JSON.stringify({
        id: match[1], title: 'fixture', current_node: turns.at(-1)?.id ?? null, messages,
      }));
    }
    res.writeHead(404, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ error: 'not found' }));
  });
  return { server, conversations };
}

if (process.argv[1] && import.meta.url === `file://${process.argv[1].replace(/\\/g, '/')}`) {
  const index = process.argv.indexOf('--port');
  const port = index < 0 ? 8790 : Number(process.argv[index + 1]);
  const { server } = fixtureServer();
  server.listen(port, '127.0.0.1', () => {
    console.log(`ChatGPT fixture at http://127.0.0.1:${port}/app`);
  });
}
