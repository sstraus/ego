#!/usr/bin/env node
// A browser harness for `ego acp`: it owns the child process and the stdio
// JSON-RPC framing, and it forwards every line in both directions to the page
// untouched. It never answers the agent on the client's behalf — an
// agent-to-client request sits pending until the page replies, because a
// harness that auto-answers cannot show that the client side is wrong.
//
// Zero dependencies on purpose: this validates a protocol, so the fewer moving
// parts between the wire and the eye, the better.

'use strict';

const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const { spawn } = require('node:child_process');

const REPO = path.resolve(__dirname, '..', '..');

const options = parseArgs(process.argv.slice(2));
const PORT = Number(options.port ?? 7788);
const DEFAULT_BIN = options.bin ?? path.join(REPO, 'target', 'debug', 'ego');
const DEFAULT_WORKSPACE =
  options.workspace ?? path.join(os.homedir(), 'Gits', '.tmp', 'acp-ui-workspace');
/** The agent's `$EGO_HOME`: a user config layer that is not the real one. */
const EGO_HOME = options.egoHome ?? path.join(os.homedir(), 'Gits', '.tmp', 'acp-ui-home');

/** The live agent, or null when nothing is running. */
let agent = null;
/** SSE subscribers. */
const clients = new Set();
/** Next JSON-RPC id for page-originated requests. */
let nextId = 1;

function parseArgs(argv) {
  const parsed = {};
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (!arg.startsWith('--')) continue;
    const eq = arg.indexOf('=');
    if (eq > 0) parsed[arg.slice(2, eq)] = arg.slice(eq + 1);
    else parsed[arg.slice(2)] = argv[++i];
  }
  return parsed;
}

function broadcast(event) {
  const payload = `data: ${JSON.stringify({ at: Date.now(), ...event })}\n\n`;
  for (const res of clients) res.write(payload);
}

/**
 * Give the agent a private user config layer.
 *
 * Not the workspace's `.ego/config.local.toml`: `ego acp` is headless, headless
 * is always untrusted and bare, and an untrusted run drops the project and
 * local layers entirely — so a sandbox written there is silently ignored. The
 * user layer is the one an ACP host can actually reach, and `$EGO_HOME` moves
 * it off `~/.ego` so this harness never edits the real one.
 */
function prepareHome(model, sandbox, mode) {
  const notes = [];
  if (!fs.existsSync(EGO_HOME)) {
    fs.mkdirSync(EGO_HOME, { recursive: true });
    notes.push(`created config home ${EGO_HOME}`);
  }
  const lines = [];
  if (model) lines.push(`model = ${JSON.stringify(model)}`);
  if (sandbox) lines.push(`sandbox = ${JSON.stringify(sandbox)}`);
  if (mode) lines.push(`mode = ${JSON.stringify(mode)}`);
  const configFile = path.join(EGO_HOME, 'config.toml');
  fs.writeFileSync(configFile, `${lines.join('\n')}\n`, 'utf8');
  notes.push(`wrote ${configFile}: ${lines.join(', ') || 'empty'}`);
  return notes;
}

function startAgent({ bin, workspace, model, sandbox, mode, sessionDir }) {
  if (agent) throw new Error('an agent is already running: stop it first');
  const notes = [];
  if (!fs.existsSync(workspace)) {
    fs.mkdirSync(workspace, { recursive: true });
    notes.push(`created workspace ${workspace}`);
  }
  notes.push(...prepareHome(model, sandbox, mode));
  const args = ['acp', '-C', workspace];
  if (sessionDir) args.push('--session-dir', sessionDir);
  const env = { ...process.env, EGO_HOME };
  const child = spawn(bin, args, { cwd: workspace, env, stdio: ['pipe', 'pipe', 'pipe'] });
  agent = { child, workspace, bin, stdout: '', stderr: '' };

  child.stdout.on('data', (chunk) => {
    agent.stdout += chunk;
    let cut;
    while ((cut = agent.stdout.indexOf('\n')) >= 0) {
      const line = agent.stdout.slice(0, cut);
      agent.stdout = agent.stdout.slice(cut + 1);
      if (line.trim()) receive(line);
    }
  });
  child.stderr.on('data', (chunk) => {
    agent.stderr += chunk;
    let cut;
    while ((cut = agent.stderr.indexOf('\n')) >= 0) {
      const line = agent.stderr.slice(0, cut);
      agent.stderr = agent.stderr.slice(cut + 1);
      if (line.trim()) broadcast({ kind: 'stderr', line });
    }
  });
  child.on('exit', (code, signal) => {
    agent = null;
    broadcast({ kind: 'exit', code, signal });
  });
  child.on('error', (error) => {
    agent = null;
    broadcast({ kind: 'error', message: error.message });
  });

  broadcast({ kind: 'started', bin, args, workspace, notes, pid: child.pid });
  return { pid: child.pid, notes };
}

/** One line off the agent's stdout, classified for the page. */
function receive(line) {
  let message;
  try {
    message = JSON.parse(line);
  } catch (error) {
    broadcast({ kind: 'unparsable', line, message: String(error) });
    return;
  }
  const isRequest = message.method !== undefined && message.id !== undefined;
  const isNotification = message.method !== undefined && message.id === undefined;
  broadcast({
    kind: isRequest ? 'agent_request' : isNotification ? 'agent_notification' : 'agent_response',
    message,
  });
}

function send(message) {
  if (!agent) throw new Error('no agent is running');
  const line = `${JSON.stringify(message)}\n`;
  agent.child.stdin.write(line);
  broadcast({ kind: 'client_out', message });
}

function body(req) {
  return new Promise((resolve, reject) => {
    let raw = '';
    req.on('data', (chunk) => {
      raw += chunk;
      if (raw.length > 4_000_000) reject(new Error('request body too large'));
    });
    req.on('end', () => {
      try {
        resolve(raw ? JSON.parse(raw) : {});
      } catch (error) {
        reject(error);
      }
    });
    req.on('error', reject);
  });
}

function json(res, status, payload) {
  const text = JSON.stringify(payload);
  res.writeHead(status, {
    'content-type': 'application/json',
    'content-length': Buffer.byteLength(text),
  });
  res.end(text);
}

const ROUTES = {
  'POST /start': async (req, res) => {
    const request = await body(req);
    const started = startAgent({
      bin: request.bin || DEFAULT_BIN,
      workspace: request.workspace || DEFAULT_WORKSPACE,
      model: request.model || '',
      sandbox: request.sandbox || '',
      mode: request.mode || '',
      sessionDir: request.sessionDir || '',
    });
    json(res, 200, started);
  },
  'POST /stop': async (_req, res) => {
    if (!agent) return json(res, 200, { stopped: false });
    agent.child.stdin.end();
    agent.child.kill('SIGTERM');
    json(res, 200, { stopped: true });
  },
  'POST /request': async (req, res) => {
    const { method, params } = await body(req);
    const id = nextId++;
    send({ jsonrpc: '2.0', id, method, params: params ?? {} });
    json(res, 200, { id });
  },
  'POST /notify': async (req, res) => {
    const { method, params } = await body(req);
    send({ jsonrpc: '2.0', method, params: params ?? {} });
    json(res, 200, { sent: true });
  },
  'POST /respond': async (req, res) => {
    const { id, result, error } = await body(req);
    const message = { jsonrpc: '2.0', id };
    if (error) message.error = error;
    else message.result = result ?? {};
    send(message);
    json(res, 200, { sent: true });
  },
  'GET /state': async (_req, res) => {
    json(res, 200, {
      running: Boolean(agent),
      pid: agent?.child.pid ?? null,
      defaults: { bin: DEFAULT_BIN, workspace: DEFAULT_WORKSPACE, egoHome: EGO_HOME, repo: REPO },
    });
  },
};

const server = http.createServer((req, res) => {
  const url = new URL(req.url, 'http://127.0.0.1');
  const route = `${req.method} ${url.pathname}`;

  if (route === 'GET /' || route === 'GET /index.html') {
    const page = fs.readFileSync(path.join(__dirname, 'index.html'));
    res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
    res.end(page);
    return;
  }

  if (route === 'GET /events') {
    res.writeHead(200, {
      'content-type': 'text/event-stream',
      'cache-control': 'no-cache',
      connection: 'keep-alive',
    });
    res.write(`retry: 1000\n\n`);
    clients.add(res);
    req.on('close', () => clients.delete(res));
    return;
  }

  const handler = ROUTES[route];
  if (!handler) return json(res, 404, { error: `no route ${route}` });
  handler(req, res).catch((error) => json(res, 400, { error: error.message }));
});

server.listen(PORT, '127.0.0.1', () => {
  process.stdout.write(`acp-test-ui on http://127.0.0.1:${PORT}\n`);
  process.stdout.write(`  binary    ${DEFAULT_BIN}\n`);
  process.stdout.write(`  workspace ${DEFAULT_WORKSPACE}\n`);
});

for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => {
    if (agent) agent.child.kill('SIGTERM');
    process.exit(0);
  });
}
