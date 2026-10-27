// End-to-end: real swarm CLI -> real bridge -> real Chrome -> fixture chat site.
// Not part of `npm test`; run with: node e2e.mjs
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { mkdtemp, rm, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fixtureServer } from './fixture.mjs';

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

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

const fixture = fixtureServer();
const fixturePort = await listen(fixture.server);
const bridgePort = await freePort();
const profile = await mkdtemp(join(tmpdir(), 'swarm-chatgpt-e2e-'));
const workspace = await mkdtemp(join(tmpdir(), 'swarm-chatgpt-ws-'));
const store = await mkdtemp(join(tmpdir(), 'swarm-chatgpt-store-'));

const bridge = spawn(process.execPath, ['bridge.mjs', '--headless', '--profile', profile,
  '--port', String(bridgePort), '--response-timeout-ms', '30000',
  '--chat-url', `http://127.0.0.1:${fixturePort}/app`],
  { cwd: import.meta.dirname, stdio: ['ignore', 'pipe', 'pipe'] });
let bridgeLog = '';
bridge.stdout.on('data', (d) => { bridgeLog += d; });
bridge.stderr.on('data', (d) => { bridgeLog += d; });

async function runCli(args) {
  return await new Promise((resolve) => {
    let out = '';
    const child = spawn('cargo', ['run', '-q', '-p', 'swarm-cli', '--', ...args],
      { cwd: join(import.meta.dirname, '..', '..'), stdio: ['ignore', 'pipe', 'pipe'] });
    child.stdout.on('data', (d) => { out += d; });
    child.stderr.on('data', (d) => { out += d; });
    child.on('exit', (code) => resolve({ code, out }));
  });
}

let failed = null;
try {
  await new Promise((resolve, reject) => {
    const started = Date.now();
    const tick = async () => {
      try {
        const response = await fetch(`http://127.0.0.1:${bridgePort}/health`);
        if (response.ok) return resolve();
      } catch {}
      if (Date.now() - started > 60000) return reject(new Error('bridge did not start\n' + bridgeLog));
      setTimeout(tick, 300);
    };
    tick();
  });

  const run = await runCli(['run', 'write hello.txt containing hello from chatgpt and verify it',
    '--provider', 'chatgpt-web', '--base-url', `http://127.0.0.1:${bridgePort}`,
    '--store', store, '--workspace', workspace, '--approve-all']);
  console.log('--- swarm run ---\n' + run.out.trim());
  if (run.code !== 0) throw new Error(`run exited ${run.code}\n${bridgeLog}`);

  const written = await readFile(join(workspace, 'hello.txt'), 'utf8');
  console.log('--- workspace/hello.txt ---\n' + written);
  if (written !== 'hello from chatgpt') throw new Error(`unexpected file content: ${written}`);

  const status = await runCli(['status', '--store', store]);
  console.log('--- swarm status ---\n' + status.out.trim());
  if (!/completed/i.test(status.out)) throw new Error('run did not complete\n' + status.out);
  // The fixture reports token counts, so the ledger must not be all zeros.
  if (!/tokens\s+300/.test(status.out)) {
    throw new Error('bridge-reported token usage did not reach the ledger:\n' + status.out);
  }
  const replay = await runCli(['logs', '--store', store, '--transcript']);
  console.log('--- swarm logs --transcript ---\n' + replay.out.trim());
  if (!/write_file\(.*path=hello\.txt/.test(replay.out)) {
    throw new Error('transcript replay did not render tool activity:\n' + replay.out);
  }
  console.log('\nE2E PASS: CLI -> bridge -> Chrome -> fixture, tool loop and usage verified.');
} catch (error) {
  failed = error;
  console.error('E2E FAIL:', error.message);
  console.error('bridge log:\n' + bridgeLog);
} finally {
  if (bridge.exitCode === null) bridge.kill();
  await new Promise((resolve) => fixture.server.close(resolve));
  await sleep(200);
  await Promise.all([profile, workspace, store].map((p) => rm(p, { recursive: true, force: true })));
}
process.exit(failed ? 1 : 0);
