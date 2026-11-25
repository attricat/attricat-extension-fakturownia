#!/usr/bin/env node
// End-to-end check of the Fakturownia client sync against a running Attricat
// worktree stack and the local Fakturownia mock (mock/server.mjs). It needs:
//
//   just pack && just mock                     # separately
//   ATTRICAT_WORKTREE=<attricat checkout> node e2e/verify.mjs
//
// The Attricat API must run with ATTRICAT_DEVTOOLS=true,
// EXTENSION_DEV_EGRESS_RESOLVE=*.fakturownia.pl=127.0.0.1:9543,
// EXTENSION_DEV_EGRESS_CA_FILE=<this repo>/mock/tls/ca.pem and
// EXTENSION_WEBHOOK_BASE_URL set.

import assert from 'node:assert/strict';
import { request as httpsRequest } from 'node:https';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const here = fileURLToPath(new URL('../', import.meta.url));
const worktree = process.env.ATTRICAT_WORKTREE;
assert(worktree, 'set ATTRICAT_WORKTREE to the Attricat checkout whose stack is running');
const env = Object.fromEntries(
  readFileSync(`${worktree}/.env`, 'utf8')
    .split('\n')
    .filter((line) => /^[A-Z_]+=/.test(line))
    .map((line) => {
      const index = line.indexOf('=');
      return [line.slice(0, index), line.slice(index + 1).replace(/^"|"$/g, '')];
    }),
);
const apiUrl = process.env.ATTRICAT_API_URL ?? env.ATTRICAT_API_URL;
const mockUrl = process.env.MOCK_INSPECT_URL ?? 'http://127.0.0.1:9580';
const mockApiPort = Number(process.env.MOCK_API_PORT ?? 9543);
const mockToken = process.env.MOCK_API_TOKEN ?? 'mock-api-token';
const manifest = JSON.parse(readFileSync(`${here}manifest.json`, 'utf8'));
const extensionId = manifest.attricat.id;
const archive = `${here}dist/${extensionId}-${manifest.version}.tar.zst`;
const account = 'acme';
const suffix = Date.now().toString(36);
const step = (message) => console.log(`\n▸ ${message}`);
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// ---- HTTP helpers -------------------------------------------------------------

const cookies = {};
const api = async (method, path, body, { expect = [200, 201, 202, 204], raw } = {}) => {
  const headers = { cookie: Object.entries(cookies).map(([name, value]) => `${name}=${value}`).join('; ') };
  if (cookies.attricat_csrf) headers['x-attricat-csrf'] = cookies.attricat_csrf;
  let payload;
  if (raw) {
    payload = raw.body;
    headers['content-type'] = raw.contentType;
  } else if (body !== undefined) {
    payload = JSON.stringify(body);
    headers['content-type'] = 'application/json';
  }
  const response = await fetch(`${apiUrl}${path}`, { method, headers, body: payload });
  for (const cookie of response.headers.getSetCookie()) {
    const [pair] = cookie.split(';');
    const [name, value] = pair.split('=');
    cookies[name] = value;
  }
  const text = await response.text();
  const parsed = text ? (() => { try { return JSON.parse(text); } catch { return text; } })() : null;
  if (!expect.includes(response.status)) throw new Error(`${method} ${path} -> ${response.status}: ${text.slice(0, 500)}`);
  return parsed;
};

const mock = async (method, path, body) => {
  const response = await fetch(`${mockUrl}${path}`, {
    method,
    headers: { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  return response.json();
};

/** Calls the mock's HTTPS Fakturownia API as an outside client would. */
const fakturownia = (method, path, body) =>
  new Promise((resolve, reject) => {
    const request = httpsRequest(
      {
        host: '127.0.0.1',
        port: mockApiPort,
        servername: `${account}.fakturownia.pl`,
        ca: readFileSync(`${here}mock/tls/ca.pem`),
        method,
        path,
        headers: { host: `${account}.fakturownia.pl`, authorization: `Bearer ${mockToken}`, 'content-type': 'application/json' },
      },
      (response) => {
        const chunks = [];
        response.on('data', (chunk) => chunks.push(chunk));
        response.on('end', () => resolve({ status: response.statusCode, body: JSON.parse(Buffer.concat(chunks).toString() || 'null') }));
      },
    );
    request.on('error', reject);
    request.end(body === undefined ? undefined : JSON.stringify(body));
  });

const waitFor = async (label, check, timeoutMs = 90_000) => {
  const started = Date.now();
  for (;;) {
    const result = await check();
    if (result) return result;
    if (Date.now() - started > timeoutMs) throw new Error(`timed out waiting for ${label}`);
    await sleep(500);
  }
};

const clients = async () => Object.values((await mock('GET', '/state')).accounts[account]?.clients ?? {});
const requests = async () => (await mock('GET', '/requests')).filter((request) => request.path.startsWith('/clients'));

// ---- scenario -------------------------------------------------------------------

step('reset the mock and sign in');
await mock('POST', '/reset', {});
const owner = env.ATTRICAT_BOOTSTRAP_OWNER_EMAIL ?? 'owner@example.test';
const password = readFileSync(`${worktree}/.secrets/bootstrap_owner_password`, 'utf8').trim();
await api('POST', '/auth/login', { login_identifier: 'default.local', email: owner, password });

step('create a client blueprint');
const contexts = await api('GET', '/contexts');
const context = contexts.find((item) => item.code === 'default');
const blueprintCode = `fk_client_${suffix}`;
const created = await api('POST', '/blueprints', {
  definition: `format_version: 1
code: ${blueprintCode}
name: Fakturownia client ${suffix}
kind: record
views:
  dropdown_option: {type: dropdown_option, fields: [name]}
attributes:
  - {code: name, value_type: string}
  - {code: nip, value_type: string}
  - {code: email, value_type: string}
  - {code: city, value_type: string}
`,
});
const blueprint = created.blueprint;
await api('POST', `/blueprints/${blueprint.id}/versions/${blueprint.version}/publish`);

step('side-load the extension, grant, configure and enable it');
const installed = await api('GET', '/extensions');
if (installed.some((item) => item.extension_id === extensionId)) {
  await api('POST', `/extensions/${extensionId}/disable`, undefined, { expect: [200, 409, 422] });
  await api('DELETE', `/extensions/${extensionId}`);
}
await api('POST', '/extensions/sideload', undefined, { raw: { body: readFileSync(archive), contentType: 'application/zstd' } });
for (const capability of manifest.permissions) {
  await api('POST', `/extensions/${extensionId}/grants`, { grant_kind: 'capability', grant_id: capability });
}
for (const rule of manifest.host_permissions) {
  await api('POST', `/extensions/${extensionId}/grants`, { grant_kind: 'host_permission', grant_id: rule.id });
}
await api('PUT', '/workspace/extension-secrets/fakturownia-api-token', { value: mockToken });
await api('PUT', `/extensions/${extensionId}/configure`, {
  configuration: {
    account,
    clients: {
      blueprint_id: blueprint.id,
      blueprint_version: blueprint.version,
      context_id: context.id,
      fields: [
        { field: 'name', attribute: 'name', owner: 'both' },
        { field: 'tax_no', attribute: 'nip', owner: 'attricat' },
        { field: 'email', attribute: 'email', owner: 'fakturownia' },
        { field: 'city', attribute: 'city', owner: 'both' },
      ],
    },
  },
});
await api('POST', `/extensions/${extensionId}/enable`);

step('create a webhook endpoint and register it in Fakturownia');
const endpoint = await api('POST', `/extensions/${extensionId}/webhook-endpoints`, { webhook: 'fakturownia', endpoint: 'main' });
assert(endpoint.url, 'set EXTENSION_WEBHOOK_BASE_URL so endpoints have a full URL');
for (const kind of ['client:create', 'client:update', 'client:delete']) {
  const registered = await fakturownia('POST', '/webhooks.json', { webhook: { kind, url: endpoint.url } });
  assert.equal(registered.status, 201);
}

step('a pull without a watermark only records one');
const pull = async (key) => {
  const run = await api('POST', `/extensions/${extensionId}/operations`, { operation_id: 'pull', input: {}, idempotency_key: `${key}-${suffix}` });
  const runId = run.run_id ?? run.id;
  return waitFor(`pull ${key}`, async () => {
    const current = await api('GET', `/extension-operation-runs/${runId}`);
    return ['completed', 'dead_letter', 'cancelled', 'failed'].includes(current.status) ? current : undefined;
  });
};
const baseline = await pull('baseline');
assert.equal(baseline.status, 'completed', JSON.stringify(baseline));

const scalar = (code, value) => ({ kind: 'scalar', attribute_code: code, context_id: context.id, value });
const update = (recordId, values) => api('PUT', `/v1/records/${recordId}`, { values, relationships: [], remove_values: [] });
const attributes = Object.fromEntries(
  (await api('GET', `/blueprints/${blueprint.id}/versions/${blueprint.version}`)).attributes.map((attribute) => [attribute.id, attribute.code]),
);
const values = async (recordId) => {
  const current = await api('GET', `/records/${recordId}/values/current`);
  return Object.fromEntries(
    current.filter((value) => value.context_id === context.id && value.active !== false).map((value) => [attributes[value.attribute_id], value.value]),
  );
};
const mapping = async (recordId) => (await api('GET', `/v1/records/${recordId}/export-status`)).statuses?.[0];

step('a new Attricat client is created in Fakturownia');
const acme = (await api('POST', '/v1/records', {
  blueprint: { code: blueprintCode },
  values: [scalar('name', 'Acme sp. z o.o.'), scalar('nip', '5250001009'), scalar('email', 'seeded@acme.test'), scalar('city', 'Warszawa')],
})).id;
const remote = await waitFor('the client in Fakturownia', async () => (await clients()).find((client) => client.external_id === acme));
assert.equal(remote.name, 'Acme sp. z o.o.');
assert.equal(remote.tax_no, '5250001009');
assert.equal(remote.city, 'Warszawa');
assert.equal(remote.email, 'seeded@acme.test', 'creating a client seeds the fields Fakturownia owns');
const synced = await waitFor('the synced mapping', async () => {
  const current = await mapping(acme);
  return current?.status === 'synced' && current.external_id ? current : undefined;
});
assert.equal(synced.external_id, String(remote.id));
console.log(`  record ${acme} -> client ${remote.id}`);

step('the create webhook echoes back without a write or a second push');
await waitFor('the create delivery', async () => (await mock('GET', '/state')).deliveries.some((d) => d.kind === 'client:create' && d.status === 200));
await sleep(4000);
let pushes = (await requests()).filter((request) => request.method !== 'GET');
assert.equal(pushes.length, 1, `expected one push, got ${JSON.stringify(pushes.map((p) => `${p.method} ${p.path}`))}`);
assert.equal((await values(acme)).email, 'seeded@acme.test', 'the echo did not overwrite the record');
const echoFailures = (await api('GET', `/extensions/${extensionId}/export-status`)).handler_failures ?? [];
assert.equal(echoFailures.length, 0, JSON.stringify(echoFailures));

step('an Attricat edit is pushed with PUT');
await update(acme, [scalar('city', 'Kraków')]);
await waitFor('the city in Fakturownia', async () => (await clients()).find((client) => client.id === remote.id)?.city === 'Kraków');
pushes = (await requests()).filter((request) => request.method === 'PUT');
assert.equal(pushes.length, 1);
assert.deepEqual(Object.keys(pushes[0].body.client).sort(), ['city', 'name', 'tax_no']);

step('a Fakturownia edit reaches Attricat through the webhook, for the fields it may change');
await mock('PATCH', `/ui/${account}/clients/${remote.id}`, { email: 'biuro@acme.test', city: 'Gdańsk', tax_no: '0000000000' });
const pulledIn = await waitFor('the webhook edit in Attricat', async () => {
  const current = await values(acme);
  return current.email === 'biuro@acme.test' && current.city === 'Gdańsk' ? current : undefined;
});
assert.equal(pulledIn.nip, '5250001009', 'tax_no is owned by Attricat and not pulled');
await sleep(4000);
assert.equal((await requests()).filter((request) => request.method === 'PUT').length, 1, 'the inbound write was not pushed back');

step('a client created in Fakturownia becomes a record');
const { id: newcomerId } = await mock('POST', `/ui/${account}/clients`, { name: 'Nowy Klient SA', tax_no: '1130000000', email: 'kontakt@nowy.test', city: 'Łódź' });
const newcomer = await waitFor('the new record', async () => {
  const status = await api('GET', `/extensions/${extensionId}/export-status?status=synced`);
  return status.mappings.find((item) => item.external_id === String(newcomerId))?.record_id;
});
assert.deepEqual(await values(newcomer), { name: 'Nowy Klient SA', nip: '1130000000', email: 'kontakt@nowy.test', city: 'Łódź' });

step('emptying a field in Fakturownia clears it in Attricat');
await mock('PATCH', `/ui/${account}/clients/${newcomerId}`, { email: '' });
await waitFor('the cleared email', async () => !(await values(newcomer)).email);

step('a lost webhook is caught up by the pull operation');
await mock('POST', '/webhooks/pause', { paused: true });
await mock('PATCH', `/ui/${account}/clients/${remote.id}`, { name: 'Acme Polska sp. z o.o.' });
await sleep(3000);
assert.equal((await values(acme)).name, 'Acme sp. z o.o.', 'nothing arrived while webhooks are paused');
const caughtUp = await pull('catch-up');
assert.equal(caughtUp.status, 'completed', JSON.stringify(caughtUp));
assert.equal((await values(acme)).name, 'Acme Polska sp. z o.o.');
await mock('POST', '/webhooks/pause', { paused: false });

step('a Fakturownia validation error rejects the record');
const nameless = (await api('POST', '/v1/records', { blueprint: { code: blueprintCode }, values: [scalar('nip', '9999999999')] })).id;
const rejected = await waitFor('the rejection', async () => {
  const current = await mapping(nameless);
  return current?.status === 'rejected' ? current : undefined;
});
console.log(`  rejected: ${JSON.stringify(rejected.error ?? rejected.last_error)}`);

step('a webhook with a wrong token is refused and kept nowhere');
const forged = await fetch(endpoint.url, {
  method: 'POST',
  headers: { 'content-type': 'application/json' },
  body: JSON.stringify({ kind: 'client:update', api_token: 'guess', client: { id: remote.id } }),
});
assert.equal(forged.status, 401);

step('the token only ever travelled as a Bearer header');
const all = await mock('GET', '/requests');
assert(all.every((request) => request.bearer), 'every API request used Authorization: Bearer');
assert(!JSON.stringify(all).includes(mockToken), 'the token never appears in a query or body');

console.log('\nAll checks passed');
