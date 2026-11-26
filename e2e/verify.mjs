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
import { createHash } from 'node:crypto';
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
// Configuration names the blueprint and the context by code; the extension
// looks their IDs and the current blueprint revision up itself.
const configuration = (blueprintCodeInConfig) => ({
  configuration: {
    account,
    clients: {
      blueprint: blueprintCodeInConfig,
      context: 'default',
      fields: [
        { field: 'name', attribute: 'name', owner: 'both' },
        { field: 'tax_no', attribute: 'nip', owner: 'attricat' },
        { field: 'email', attribute: 'email', owner: 'fakturownia' },
        { field: 'city', attribute: 'city', owner: 'both' },
      ],
    },
  },
});
await api('PUT', `/extensions/${extensionId}/configure`, configuration(blueprintCode));
await api('POST', `/extensions/${extensionId}/enable`);

step('create a webhook endpoint and register it in Fakturownia');
const endpoint = await api('POST', `/extensions/${extensionId}/webhook-endpoints`, { webhook: 'fakturownia', endpoint: 'main' });
assert(endpoint.url, 'set EXTENSION_WEBHOOK_BASE_URL so endpoints have a full URL');
for (const kind of ['client:create', 'client:update', 'client:delete', 'invoice:create', 'invoice:update', 'invoice:delete']) {
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
/** The agreed-state hash of a projection, as sync-core computes it. */
const projectionHash = (projection) => {
  const sorted = Object.fromEntries(Object.entries(projection).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)));
  return createHash('sha256').update(JSON.stringify(sorted)).digest('hex');
};

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
await waitFor('the cleared email', async () => !('email' in (await values(newcomer))));
// Removed through remove_values, not overwritten with an empty string.
assert.deepEqual(await values(newcomer), { name: 'Nowy Klient SA', nip: '1130000000', city: 'Łódź' });

step('a lost webhook is caught up by the pull operation');
await mock('POST', '/webhooks/pause', { paused: true });
const putsBeforePull = (await requests()).filter((request) => request.method === 'PUT').length;
await mock('PATCH', `/ui/${account}/clients/${remote.id}`, { name: 'Acme Polska sp. z o.o.', city: '', tax_no: '1111111111' });
await sleep(3000);
assert.equal((await values(acme)).name, 'Acme sp. z o.o.', 'nothing arrived while webhooks are paused');
const caughtUp = await pull('catch-up');
assert.equal(caughtUp.status, 'completed', JSON.stringify(caughtUp));
// The run reads the record and merges: it clears the emptied city, keeps the
// Attricat-owned NIP, and records the merged state as agreed.
const merged = await values(acme);
assert.deepEqual(merged, { name: 'Acme Polska sp. z o.o.', nip: '5250001009', email: 'biuro@acme.test' });
assert.equal(
  (await mapping(acme)).content_hash,
  projectionHash({ name: merged.name, tax_no: merged.nip, email: merged.email }),
  'the pull stored the merged projection as the agreed state',
);
await sleep(4000);
assert.equal((await requests()).filter((request) => request.method === 'PUT').length, putsBeforePull, 'the pull wrote nothing back to Fakturownia');
await mock('POST', '/webhooks/pause', { paused: false });

step('a Fakturownia validation error rejects the record');
const nameless = (await api('POST', '/v1/records', { blueprint: { code: blueprintCode }, values: [scalar('nip', '9999999999')] })).id;
const rejected = await waitFor('the rejection', async () => {
  const current = await mapping(nameless);
  return current?.status === 'rejected' ? current : undefined;
});
console.log(`  rejected: ${JSON.stringify(rejected.error ?? rejected.last_error)}`);

// ---- invoices -------------------------------------------------------------------

step('create order, line, product, unit and single-line document blueprints');
const publish = async (definition) => {
  const { blueprint: created } = await api('POST', '/blueprints', { definition });
  await api('POST', `/blueprints/${created.id}/versions/${created.version}/publish`);
  for (const attribute of (await api('GET', `/blueprints/${created.id}/versions/${created.version}`)).attributes) {
    attributes[attribute.id] = attribute.code;
  }
  return created;
};
const codes = {
  unit: `fk_unit_${suffix}`,
  product: `fk_product_${suffix}`,
  order: `fk_order_${suffix}`,
  line: `fk_order_line_${suffix}`,
  issue: `fk_issue_${suffix}`,
};
const writeBackAttributes = `  - {code: invoice_number, value_type: string}
  - {code: invoice_status, value_type: string}
  - {code: invoice_url, value_type: string}
  - {code: ksef_number, value_type: string}
  - {code: ksef_status, value_type: string}
  - {code: invoice_pdf, value_type: file}
`;
const recordBlueprint = (code, name, attributes) => `format_version: 1
code: ${code}
name: ${name} ${suffix}
kind: record
views:
  dropdown_option: {type: dropdown_option, fields: [${attributes.match(/code: (\w+)/)[1]}]}
  detail: {type: stack, extensions: [${extensionId}/issue-invoice]}
  table: {type: table, extensions: [${extensionId}/issue-invoices]}
attributes:
${attributes}`;
await publish(recordBlueprint(codes.unit, 'Unit', `  - {code: code, value_type: string}\n`));
await publish(recordBlueprint(codes.product, 'Product', `  - {code: name, value_type: string}
  - {code: code, value_type: string}
  - {code: base_unit, value_type: relationship, target_blueprint: ${codes.unit}}
`));
const orderBlueprint = await publish(recordBlueprint(codes.order, 'Sales order', `  - {code: number, value_type: string}
  - {code: customer, value_type: relationship, target_blueprint: ${blueprintCode}}
  - {code: document_date, value_type: date}
  - {code: note, value_type: string}
${writeBackAttributes}`));
await publish(recordBlueprint(codes.line, 'Sales order line', `  - {code: title, value_type: string}
  - {code: sales_order, value_type: relationship, target_blueprint: ${codes.order}}
  - {code: item, value_type: relationship, target_blueprint: ${codes.product}}
  - {code: quantity, value_type: number}
  - {code: unit_price_net, value_type: number}
`));
const issueBlueprint = await publish(recordBlueprint(codes.issue, 'Goods issue', `  - {code: number, value_type: string}
  - {code: customer, value_type: relationship, target_blueprint: ${blueprintCode}}
  - {code: item_name, value_type: string}
  - {code: quantity, value_type: number}
  - {code: price, value_type: number}
${writeBackAttributes}`));

const orderInvoices = (extra = {}) => ({
  source: { blueprint: codes.order, context: 'default' },
  buyer: 'customer',
  header: { kind: 'vat', issue_date: null, sell_date: 'document_date', oid: 'number', description: 'note',
    payment_to_days: 14, payment_type: 'transfer', currency: 'PLN' },
  positions: {
    lines: { blueprint: codes.line, parent: 'sales_order' },
    name: 'item.name', code: 'item.code', quantity: 'quantity', unit: 'item.base_unit.code',
    price_net: 'unit_price_net', tax: { default: '23' },
  },
  write_back: { number: 'invoice_number', status: 'invoice_status', url: 'invoice_url',
    ksef_number: 'ksef_number', ksef_status: 'ksef_status', file: 'invoice_pdf' },
  link_existing_by_oid: false,
  ...extra,
});
const configureInvoices = async (invoices) => {
  const body = configuration(blueprintCode);
  body.configuration.invoices = invoices;
  await api('PUT', `/extensions/${extensionId}/configure`, body);
};
await configureInvoices(orderInvoices());

step('a pull records the invoice watermark');
assert.equal((await pull('invoice-baseline')).status, 'completed');

const releaseId = (await api('GET', '/extensions')).find((item) => item.extension_id === extensionId).installed_release_id;
const issueInvoices = async (blueprint, recordIds, key) => {
  const started = await api('POST', `/extensions/${extensionId}/issue-invoices/operations`, {
    release_id: releaseId,
    operation_id: 'issue-invoices',
    input: {},
    idempotency_key: `${key}-${suffix}`,
    selection: { blueprint_id: blueprint.id, blueprint_version: blueprint.version, context_id: null, record_ids: recordIds },
  });
  return waitFor(`issue ${key}`, async () => {
    const current = await api('GET', `/extension-operation-runs/${started.run_id}`);
    return ['completed', 'dead_letter', 'cancelled', 'failed'].includes(current.status) ? current : undefined;
  });
};
const results = (run) => run.progress?.results ?? [];
const invoicesIn = async () => Object.values((await mock('GET', '/state')).accounts[account]?.invoices ?? {});
const invoiceCreates = async () => (await mock('GET', '/requests')).filter((request) => request.method === 'POST' && request.path === '/invoices.json');
const order = async (number, lines, extra = []) => {
  const id = (await api('POST', '/v1/records', {
    blueprint: { code: codes.order },
    values: [scalar('number', number), scalar('document_date', '2026-11-20'), scalar('note', `Order ${number}`), ...extra,
      { kind: 'relationship', attribute_code: 'customer', context_id: context.id, target_record_id: acme }],
  })).id;
  for (const [index, line] of lines.entries()) {
    const values = [scalar('title', `${number} line ${index + 1}`), scalar('quantity', line.quantity),
      { kind: 'relationship', attribute_code: 'sales_order', context_id: context.id, target_record_id: id },
      { kind: 'relationship', attribute_code: 'item', context_id: context.id, target_record_id: line.item }];
    if (line.price !== undefined) values.push(scalar('unit_price_net', line.price));
    await api('POST', '/v1/records', { blueprint: { code: codes.line }, values });
  }
  return id;
};

const piece = (await api('POST', '/v1/records', { blueprint: { code: codes.unit }, values: [scalar('code', 'szt')] })).id;
const widget = (await api('POST', '/v1/records', { blueprint: { code: codes.product }, values: [scalar('name', 'Widget'), scalar('code', 'W-1'),
  { kind: 'relationship', attribute_code: 'base_unit', context_id: context.id, target_record_id: piece }] })).id;
const gadget = (await api('POST', '/v1/records', { blueprint: { code: codes.product }, values: [scalar('name', 'Gadget'), scalar('code', 'G-2'),
  { kind: 'relationship', attribute_code: 'base_unit', context_id: context.id, target_record_id: piece }] })).id;

step('an order with two lines is invoiced from the record action');
const number1 = `ZS/1/${suffix}`;
const order1 = await order(number1, [{ item: widget, quantity: 2, price: 10.5 }, { item: gadget, quantity: 1, price: 99 }]);
const issued = await issueInvoices(orderBlueprint, [order1], 'issue-1');
assert.equal(issued.status, 'completed', JSON.stringify(issued));
assert.equal(results(issued)[0]?.outcome, 'succeeded', JSON.stringify(issued.progress));
const [created1] = (await invoiceCreates()).map((request) => request.body.invoice);
assert.equal(created1.client_id, remote.id, 'the buyer is the mapped Fakturownia client');
assert.equal(created1.oid, number1);
assert.equal(created1.sell_date, '2026-11-20');
assert.equal(created1.description, `Order ${number1}`);
assert.equal(created1.payment_to_kind, '14');
assert.equal(created1.exclude_from_stock_level, true);
assert.deepEqual(created1.positions, [
  { name: 'Widget', code: 'W-1', quantity: '2', quantity_unit: 'szt', price_net: '10.5', tax: '23' },
  { name: 'Gadget', code: 'G-2', quantity: '1', quantity_unit: 'szt', price_net: '99', tax: '23' },
]);
assert(created1.positions.every((position) => !('product_id' in position)), 'no Fakturownia product, so no stock moves');
const invoice1 = (await invoicesIn()).find((invoice) => invoice.oid === number1);
let written = await values(order1);
assert.equal(written.invoice_number, invoice1.number);
assert.equal(written.invoice_status, 'issued');
assert.equal(written.invoice_url, `https://${account}.fakturownia.pl/invoice/${invoice1.token}`);
assert.equal((await mapping(order1)).external_id, `invoice:${invoice1.id}`);
const pdf = (await api('GET', `/records/${order1}/resolved-preview?context_id=${context.id}`)).values.invoice_pdf?.value?.[0];
assert.equal(pdf?.filename, `${invoice1.number.replaceAll('/', '-')}.pdf`, JSON.stringify(pdf));
assert.equal(pdf.mime_type, 'application/pdf');
assert((await mock('GET', '/requests')).some((request) => request.path === `/invoices/${invoice1.id}.pdf` && request.bearer));
console.log(`  order ${order1} -> invoice ${invoice1.number}`);

step('a second action on the same order is refused');
const again = await issueInvoices(orderBlueprint, [order1], 'issue-1-again');
assert.equal(results(again)[0]?.outcome, 'failed');
assert.match(results(again)[0].message, /already invoiced as/);
assert.equal((await invoicesIn()).filter((invoice) => invoice.oid === number1).length, 1);

step('a create whose answer is lost is retried without a second invoice');
const number2 = `ZS/2/${suffix}`;
const order2 = await order(number2, [{ item: widget, quantity: 3, price: 5 }]);
await mock('POST', '/fail', { method: 'POST', path: '/invoices.json', drop: true, count: 1 });
const lost = await issueInvoices(orderBlueprint, [order2], 'issue-2');
assert.equal(results(lost)[0]?.outcome, 'succeeded', JSON.stringify(lost.progress));
assert.equal((await invoicesIn()).filter((invoice) => invoice.oid === number2).length, 1, 'one invoice despite the lost answer');
const invoice2 = (await invoicesIn()).find((invoice) => invoice.oid === number2);
assert.equal((await values(order2)).invoice_number, invoice2.number);

step('a line without a price is rejected before anything is sent');
const creates = (await invoiceCreates()).length;
const number3 = `ZS/3/${suffix}`;
const order3 = await order(number3, [{ item: widget, quantity: 1 }]);
const incomplete = await issueInvoices(orderBlueprint, [order3], 'issue-3');
assert.equal(results(incomplete)[0]?.outcome, 'failed');
assert.match(results(incomplete)[0].message, /line 1 \(.*\): price_net is empty/);
assert.equal((await invoiceCreates()).length, creates, 'nothing was sent');
const rejected3 = await mapping(order3);
assert.equal(rejected3.status, 'rejected');
assert.match(JSON.stringify(rejected3.error), /price_net is empty/);

step('a Fakturownia validation error rejects the order');
const number4 = `ZS/4/${suffix}`;
const order4 = await order(number4, [{ item: gadget, quantity: 1, price: 1 }]);
await mock('POST', '/fail', { method: 'POST', path: '/invoices.json', status: 422, count: 1,
  body: { code: 'error', message: { buyer_tax_no: ['- jest nieprawidłowy'] } } });
const invalid = await issueInvoices(orderBlueprint, [order4], 'issue-4');
assert.equal(results(invalid)[0]?.outcome, 'failed');
assert.equal(results(invalid)[0].message, 'buyer_tax_no: jest nieprawidłowy');
assert.equal((await mapping(order4)).status, 'rejected');

step('payment and the KSeF number arrive through the webhook');
await mock('PATCH', `/ui/${account}/invoices/${invoice1.id}`, { status: 'paid', gov_id: '5250001009-20261126-0A1B2C3D4E5F-01', gov_status: 'ok' });
written = await waitFor('the paid invoice in Attricat', async () => {
  const current = await values(order1);
  return current.invoice_status === 'paid' ? current : undefined;
});
assert.equal(written.ksef_number, '5250001009-20261126-0A1B2C3D4E5F-01');
assert.equal(written.ksef_status, 'ok');

step('a lost invoice webhook is caught up by the pull operation');
await mock('POST', '/webhooks/pause', { paused: true });
await mock('PATCH', `/ui/${account}/invoices/${invoice2.id}`, { status: 'sent' });
await sleep(3000);
assert.equal((await values(order2)).invoice_status, 'issued');
assert.equal((await pull('invoice-catch-up')).status, 'completed');
assert.equal((await values(order2)).invoice_status, 'sent');
await mock('POST', '/webhooks/pause', { paused: false });

step('a deleted invoice clears the order, which can then be invoiced again');
await mock('DELETE', `/ui/${account}/invoices/${invoice2.id}`);
await waitFor('the cleared order', async () => !('invoice_number' in (await values(order2))));
assert.equal(await mapping(order2), undefined, 'the mapping is gone');
const reissued = await issueInvoices(orderBlueprint, [order2], 'issue-2-again');
assert.equal(results(reissued)[0]?.outcome, 'succeeded', JSON.stringify(reissued.progress));
const invoice2b = (await invoicesIn()).find((invoice) => invoice.oid === number2);
assert.notEqual(invoice2b.id, invoice2.id);
assert.equal((await values(order2)).invoice_number, invoice2b.number);

step('an invoice made in Fakturownia is linked to its order by number');
await configureInvoices(orderInvoices({ link_existing_by_oid: true }));
const number5 = `ZS/5/${suffix}`;
const order5 = await order(number5, [{ item: widget, quantity: 1, price: 1 }]);
const { id: manualId } = await mock('POST', `/ui/${account}/invoices`, { oid: number5, client_id: remote.id,
  positions: [{ name: 'Typed by hand', quantity: 1, price_net: 1, tax: '23' }] });
await waitFor('the linked order', async () => (await mapping(order5))?.external_id === `invoice:${manualId}`);
assert.equal((await values(order5)).invoice_status, 'issued');

step('a single-line goods issue is invoiced from the record itself');
await configureInvoices({
  source: { blueprint: codes.issue },
  buyer: 'customer',
  header: { oid: 'number' },
  positions: { name: 'item_name', quantity: 'quantity', price_net: 'price', tax: { default: '8' } },
  write_back: { number: 'invoice_number', status: 'invoice_status' },
});
const number6 = `WZ/6/${suffix}`;
const goodsIssue = (await api('POST', '/v1/records', { blueprint: { code: codes.issue }, values: [scalar('number', number6),
  scalar('item_name', 'Pallet'), scalar('quantity', 4), scalar('price', 12),
  { kind: 'relationship', attribute_code: 'customer', context_id: context.id, target_record_id: acme }] })).id;
const single = await issueInvoices(issueBlueprint, [goodsIssue], 'issue-6');
assert.equal(results(single)[0]?.outcome, 'succeeded', JSON.stringify(single.progress));
const created6 = (await invoiceCreates()).map((request) => request.body.invoice).find((invoice) => invoice.oid === number6);
assert.deepEqual(created6.positions, [{ name: 'Pallet', quantity: '4', price_net: '12', tax: '8' }]);
assert.equal((await values(goodsIssue)).invoice_number, (await invoicesIn()).find((invoice) => invoice.oid === number6).number);
await configureInvoices(orderInvoices());

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

step('a pull refuses to start when no blueprint has the configured code');
await api('PUT', `/extensions/${extensionId}/configure`, configuration(`no_such_${suffix}`));
const misconfigured = await pull('misconfigured');
assert.equal(misconfigured.status, 'dead_letter', JSON.stringify(misconfigured));
assert(JSON.stringify(misconfigured).includes('no published blueprint has the code'), JSON.stringify(misconfigured));
await api('PUT', `/extensions/${extensionId}/configure`, configuration(blueprintCode));

console.log('\nAll checks passed');
