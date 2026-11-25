#!/usr/bin/env node
// A local stand-in for the Fakturownia API, for development and the e2e
// scenario. It never contacts Fakturownia.
//
// HTTPS on 9543 answers `<account>.fakturownia.pl` with the certificate from
// `just mock-certs`; point Attricat's broker at it with
// EXTENSION_DEV_EGRESS_RESOLVE=*.fakturownia.pl=127.0.0.1:9543 and
// EXTENSION_DEV_EGRESS_CA_FILE=<repo>/mock/tls/ca.pem.
//
// The documented parts follow https://github.com/fakturownia/api: Bearer
// tokens, `/clients.json` CRUD with a `client` root, `page`/`per_page` (at most
// 100), `order=updated_at.desc`, `external_id` lookup, 422 validation errors
// and `/webhooks.json` CRUD. ASSUMED, to verify on a real account: the
// webhook registration body (`{webhook: {kind, url}}`), the delivery body
// (`{kind, api_token, client: {...}}`), that changes made through the API fire
// webhooks too, the 201 status of a create, and that clients store
// `external_id`.
//
// Inspection API on http://127.0.0.1:9580 (no auth):
//   GET  /state                       accounts, clients, webhooks, deliveries
//   GET  /requests                    API requests received (no tokens)
//   POST /reset
//   POST /ui/:account/clients         create a client as a user in the web app
//   PATCH /ui/:account/clients/:id    edit a client as a user
//   DELETE /ui/:account/clients/:id
//   POST /webhooks/pause {paused}     stop delivering webhooks (lost deliveries)
//   POST /fail {method, path, status, count}  fail the next matching requests

import { createServer as createHttpsServer } from 'node:https';
import { createServer as createHttpServer } from 'node:http';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const API_PORT = Number(process.env.MOCK_API_PORT ?? 9543);
const INSPECT_PORT = Number(process.env.MOCK_INSPECT_PORT ?? 9580);
const TOKEN = process.env.MOCK_API_TOKEN ?? 'mock-api-token';
const MAX_PER_PAGE = 100;
const DEFAULT_PER_PAGE = 25;
const CLIENT_FIELDS = ['name', 'tax_no', 'email', 'phone', 'street', 'city', 'post_code', 'country', 'external_id'];
const STATUS = { ok: 200, created: 201, noContent: 204, badRequest: 400, unauthorized: 401, notFound: 404, unprocessable: 422 };

const tls = fileURLToPath(new URL('./tls/', import.meta.url));

let state;
const reset = () => {
  // IDs stay unique across resets: Attricat keeps export mappings after an
  // extension is removed, so a reused ID would match an earlier run's record.
  state = { accounts: {}, nextId: Math.floor(Date.now() / 1000) * 10, requests: [], deliveries: [], paused: false, failures: [], clock: 0 };
};
reset();

const account = (name) => {
  state.accounts[name] ??= { clients: {}, webhooks: {} };
  return state.accounts[name];
};

/** Fakturownia-style local timestamps with an offset, strictly increasing. */
const timestamp = () => {
  const now = Math.max(Date.now(), state.clock + 1);
  state.clock = now;
  const local = new Date(now + 60 * 60 * 1000).toISOString().replace('Z', '+01:00');
  return local;
};

const json = (response, status, body) => {
  response.writeHead(status, { 'content-type': 'application/json; charset=utf-8' });
  response.end(body === undefined ? '' : JSON.stringify(body));
};

const readBody = (request) =>
  new Promise((resolve) => {
    const chunks = [];
    request.on('data', (chunk) => chunks.push(chunk));
    request.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
  });

const validationError = (fields) => ({
  code: 'error',
  message: Object.fromEntries(fields.map(([field, text]) => [field, [`- ${text}`]])),
});

const pick = (input = {}) =>
  Object.fromEntries(CLIENT_FIELDS.filter((field) => field in input).map((field) => [field, input[field] ?? '']));

/** Sends `kind` to every matching webhook of the account, fire and forget. */
const fire = (accountName, kind, client) => {
  const { webhooks } = account(accountName);
  for (const hook of Object.values(webhooks)) {
    if (hook.kind !== kind || hook.active === false) continue;
    const delivery = { webhook_id: hook.id, kind, client_id: client.id, url_path: new URL(hook.url).pathname };
    state.deliveries.push(delivery);
    if (state.paused) {
      delivery.status = 'paused';
      continue;
    }
    fetch(hook.url, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ kind, api_token: TOKEN, client }),
    })
      .then(async (answer) => {
        delivery.status = answer.status;
        delivery.reply = (await answer.text()).slice(0, 200);
      })
      .catch((error) => {
        delivery.status = 'failed';
        delivery.reply = String(error.message);
      });
  }
};

const createClient = (accountName, input) => {
  const fields = pick(input);
  if (!fields.name?.trim()) return { error: validationError([['name', 'nie może być puste']]) };
  const id = state.nextId++;
  const now = timestamp();
  const client = { id, ...Object.fromEntries(CLIENT_FIELDS.map((f) => [f, ''])), ...fields, created_at: now, updated_at: now };
  account(accountName).clients[id] = client;
  fire(accountName, 'client:create', client);
  return { client };
};

const updateClient = (accountName, id, input) => {
  const client = account(accountName).clients[id];
  if (!client) return { missing: true };
  const fields = pick(input);
  if ('name' in fields && !String(fields.name).trim()) return { error: validationError([['name', 'nie może być puste']]) };
  Object.assign(client, fields, { updated_at: timestamp() });
  fire(accountName, 'client:update', client);
  return { client };
};

const deleteClient = (accountName, id) => {
  const client = account(accountName).clients[id];
  if (!client) return false;
  delete account(accountName).clients[id];
  fire(accountName, 'client:delete', client);
  return true;
};

const takeFailure = (method, path) => {
  const failure = state.failures.find((f) => f.count > 0 && f.method === method && path.startsWith(f.path));
  if (!failure) return null;
  failure.count -= 1;
  return failure.status;
};

const api = async (request, response) => {
  const url = new URL(request.url, 'https://mock');
  const host = (request.headers.host ?? '').split(':')[0];
  const accountName = host.endsWith('.fakturownia.pl') ? host.slice(0, -'.fakturownia.pl'.length) : null;
  const body = await readBody(request);
  const authorization = request.headers.authorization ?? '';
  state.requests.push({
    method: request.method,
    host,
    path: url.pathname,
    query: Object.fromEntries([...url.searchParams].filter(([key]) => key !== 'api_token')),
    bearer: authorization.startsWith('Bearer '),
    body: body ? JSON.parse(body) : null,
  });
  if (!accountName) return json(response, STATUS.notFound, { code: 'error', message: 'unknown account' });
  if (authorization !== `Bearer ${TOKEN}`) {
    return json(response, STATUS.unauthorized, { code: 'error', message: 'You must be logged in to gain access to the site' });
  }
  const failed = takeFailure(request.method, url.pathname);
  if (failed) return json(response, failed, { code: 'error', message: 'injected failure' });
  const input = body ? JSON.parse(body) : {};
  const { clients, webhooks } = account(accountName);
  let match;

  if (url.pathname === '/clients.json' && request.method === 'GET') {
    let list = Object.values(clients);
    for (const filter of ['external_id', 'tax_no', 'email', 'name']) {
      const value = url.searchParams.get(filter);
      if (value !== null) list = list.filter((client) => String(client[filter]) === value);
    }
    if (url.searchParams.get('order') === 'updated_at.desc') {
      list.sort((a, b) => Date.parse(b.updated_at) - Date.parse(a.updated_at));
    }
    const perPage = Math.min(Number(url.searchParams.get('per_page') ?? DEFAULT_PER_PAGE), MAX_PER_PAGE);
    const page = Number(url.searchParams.get('page') ?? 1);
    return json(response, STATUS.ok, list.slice((page - 1) * perPage, page * perPage));
  }
  if (url.pathname === '/clients.json' && request.method === 'POST') {
    const { client, error } = createClient(accountName, input.client);
    return error ? json(response, STATUS.unprocessable, error) : json(response, STATUS.created, client);
  }
  if ((match = url.pathname.match(/^\/clients\/(\d+)\.json$/))) {
    const id = Number(match[1]);
    if (request.method === 'GET') {
      return clients[id] ? json(response, STATUS.ok, clients[id]) : json(response, STATUS.notFound, { code: 'error', message: 'not found' });
    }
    if (request.method === 'PUT') {
      const { client, error, missing } = updateClient(accountName, id, input.client);
      if (missing) return json(response, STATUS.notFound, { code: 'error', message: 'not found' });
      return error ? json(response, STATUS.unprocessable, error) : json(response, STATUS.ok, client);
    }
    if (request.method === 'DELETE') {
      return deleteClient(accountName, id) ? json(response, STATUS.ok, {}) : json(response, STATUS.notFound, { code: 'error', message: 'not found' });
    }
  }
  if (url.pathname === '/webhooks.json' && request.method === 'GET') {
    return json(response, STATUS.ok, Object.values(webhooks));
  }
  if (url.pathname === '/webhooks.json' && request.method === 'POST') {
    const hook = input.webhook ?? {};
    if (!hook.kind || !hook.url) return json(response, STATUS.unprocessable, validationError([['url', 'nie może być puste']]));
    const id = state.nextId++;
    webhooks[id] = { id, kind: hook.kind, url: hook.url, active: hook.active ?? true };
    return json(response, STATUS.created, webhooks[id]);
  }
  if ((match = url.pathname.match(/^\/webhooks\/(\d+)\.json$/))) {
    const id = Number(match[1]);
    if (!webhooks[id]) return json(response, STATUS.notFound, { code: 'error', message: 'not found' });
    if (request.method === 'GET') return json(response, STATUS.ok, webhooks[id]);
    if (request.method === 'PUT') {
      Object.assign(webhooks[id], input.webhook ?? {});
      return json(response, STATUS.ok, webhooks[id]);
    }
    if (request.method === 'DELETE') {
      delete webhooks[id];
      return json(response, STATUS.ok, {});
    }
  }
  return json(response, STATUS.notFound, { code: 'error', message: 'not found' });
};

const inspect = async (request, response) => {
  const url = new URL(request.url, 'http://mock');
  const body = await readBody(request);
  const input = body ? JSON.parse(body) : {};
  let match;
  if (url.pathname === '/state') return json(response, STATUS.ok, state);
  if (url.pathname === '/requests') return json(response, STATUS.ok, state.requests);
  if (url.pathname === '/reset' && request.method === 'POST') {
    reset();
    return json(response, STATUS.ok, { reset: true });
  }
  if (url.pathname === '/webhooks/pause' && request.method === 'POST') {
    state.paused = Boolean(input.paused);
    return json(response, STATUS.ok, { paused: state.paused });
  }
  if (url.pathname === '/fail' && request.method === 'POST') {
    state.failures.push({ method: input.method, path: input.path, status: input.status, count: input.count ?? 1 });
    return json(response, STATUS.ok, { failures: state.failures });
  }
  if ((match = url.pathname.match(/^\/ui\/([a-z0-9-]+)\/clients$/)) && request.method === 'POST') {
    const { client, error } = createClient(match[1], input);
    return error ? json(response, STATUS.unprocessable, error) : json(response, STATUS.created, client);
  }
  if ((match = url.pathname.match(/^\/ui\/([a-z0-9-]+)\/clients\/(\d+)$/))) {
    if (request.method === 'PATCH') {
      const { client, error, missing } = updateClient(match[1], Number(match[2]), input);
      if (missing) return json(response, STATUS.notFound, {});
      return error ? json(response, STATUS.unprocessable, error) : json(response, STATUS.ok, client);
    }
    if (request.method === 'DELETE') {
      return json(response, deleteClient(match[1], Number(match[2])) ? STATUS.ok : STATUS.notFound, {});
    }
  }
  return json(response, STATUS.notFound, {});
};

createHttpsServer({ key: readFileSync(`${tls}server.key`), cert: readFileSync(`${tls}server.pem`) }, (request, response) => {
  api(request, response).catch((error) => json(response, STATUS.badRequest, { code: 'error', message: String(error.message) }));
}).listen(API_PORT, '127.0.0.1', () => console.log(`Fakturownia mock API on https://*.fakturownia.pl -> 127.0.0.1:${API_PORT}`));

createHttpServer((request, response) => {
  inspect(request, response).catch((error) => json(response, STATUS.badRequest, { error: String(error.message) }));
}).listen(INSPECT_PORT, '127.0.0.1', () => console.log(`Mock inspection API on http://127.0.0.1:${INSPECT_PORT}`));
