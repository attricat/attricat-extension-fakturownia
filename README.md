# attricat-fakturownia

Two-way sync between Attricat and [Fakturownia](https://fakturownia.pl), on the
**`attricat:host@1.2.0`** ABI.

> This release syncs **clients** both ways and issues **sales
> invoices** from Attricat records. It has been tested against the local mock
> in `mock/`, and its webhooks and invoice issuing against a real account:
> since 0.4.1 webhooks follow Fakturownia's documented delivery format, and
> since 0.4.2 positions carry the gross total a real account requires. See
> [Assumptions to verify](#assumptions-to-verify) for what is still unchecked.
>
> 0.3.0 needs an Attricat Core that lets selection runs read linked records
> and attach files (`attricat-data.read` `record`/`incoming` in interactive
> runs, `files.attach.v1`), which is newer than v1.4.0. These are JSON-level
> additions that `host_api` cannot express, and older hosts refuse them.

## What it does

| Direction | How | Code |
| --- | --- | --- |
| Attricat → Fakturownia | `record.created.v1` / `record.updated.v1` on the configured blueprint (debounced 2 s) push the client with `POST /clients.json` or `PUT /clients/{id}.json`. The export mapping keeps the Fakturownia ID and the hash of the last agreed state. | `sync.rs` (`push_record`) |
| Fakturownia → Attricat | A Fakturownia webhook is a **hint**: `receive` reads the object's key and ID and keeps only `{resource, action, id}`; the `webhook.received.v1` handler fetches the client again with `GET /clients/{id}.json` and applies it. A forged request costs one GET at most. | `webhook.rs`, `sync.rs` (`apply_client`) |
| Safety net | The `pull` operation pages `GET /clients.json?order=updated_at.desc` back to the watermark of the previous run (Fakturownia has no "changed since" filter) and applies what it finds, merging exactly as the webhook path does. With invoices configured it then pages `GET /invoices.json?period=all&order=updated_at.desc` back to a separate watermark. Run it on a schedule. | `pull.rs` |
| Issue an invoice | **Issue invoice in Fakturownia** (*Wystaw fakturę w Fakturowni*) on a record's header, or for up to 50 records from the Explorer, runs the `issue-invoices` operation: one invoice per source record, with one result per record in the run. | `invoices.rs` (`process_batch`, `issue`) |
| Invoice → Attricat | `invoice:*` webhooks are hints too: the handler fetches `GET /invoices/{id}.json` and writes the number, status, view URL and KSeF number and status to the source record, only where they differ. A deleted invoice (404) clears them and removes the mapping, so the record can be invoiced again. | `webhook.rs`, `invoices.rs` (`apply_invoice`, `deleted`) |

Each mapped field has an **owner**:

- `attricat`: pushed; Fakturownia edits are ignored.
- `fakturownia`: pulled; never pushed after the client exists.
- `both`: both directions; the latest change wins.

Creating a client seeds every mapped field.

**Echo suppression.** The mapping's content hash is the projection of mapped fields both sides last agreed on:

- A push whose projection is unchanged is skipped. This covers the events caused by the extension's own writes, which are also skipped by `source-name`.
- An inbound change that leaves the projection unchanged writes nothing. This covers the webhook announcing our own push.

Inbound changes, from a webhook or from `pull`, read the record, write only the fields that differ, and store the merged projection as the agreed state. A field emptied in Fakturownia is removed from the record (`remove_values`), not overwritten with an empty string.

**API token.** It lives in an extension secret and is sent only as a host-substituted `Authorization: Bearer` header. It never enters component memory, a URL or a log.

**Fakturownia errors.** Since 0.6.0 every unusable Fakturownia answer becomes a typed failure with a message a person can read, which Attricat shows on the run page, in the run's failure toast, on the extension's **Failures** tab and in a record's export status. The message carries Fakturownia's own error text, read from `{"code":"error","message":{"field":["- text"]}}`, `{"message":"text"}`, a bare field map, a JSON string or plain text, as `Fakturownia: <text>` (at most 500 bytes), or names the HTTP status when the body has none (`errors.rs` in sync-core):

| Answer | Failure | Code |
| --- | --- | --- |
| 401, 403 | `rejected`: *Fakturownia refused the API token (HTTP 401). Check the Fakturownia API token secret…* | `fakturownia_unauthorized` |
| 422 | `rejected` with Fakturownia's field messages | `fakturownia_invalid` |
| other 4xx | `rejected` with Fakturownia's text | `fakturownia_rejected` (`fakturownia_client_missing` for a push to a deleted client) |
| 429, 5xx | `retry` after `Retry-After` seconds (capped at an hour), else 5 s | |
| unreachable, the manifest's request budget spent, an unconfirmed write | `retry` | |
| 1xx, 3xx | `rejected` | `fakturownia_unexpected` |

Attricat Core keeps and shows these messages since the release after v1.13.0; an older host still retries and rejects alike but shows less. None is a `fault`, so a Fakturownia error never quarantines the installation. A push's rejection names the record, whose export status becomes `rejected`. A `pull` that is refused fails the run with the reason. In `issue-invoices` a refused invoice is that record's result (`failed`, with the message) and its export status, but a refused token ends the run with the reason, since every other record would fail alike. The token never appears in a code or message: the component never holds it.

### Invoices

An invoice is issued from a **source** record: a sales order whose lines are records pointing at it, or a single-line document such as a goods issue, which is its own position.

- **One invoice per source record.** The source record's export mapping holds the invoice as `invoice:<id>` (prefixed, so it never collides with a client's ID). A record that has one is refused, and the run's result names the invoice. A run that crashed after the create finishes the write-back on retry.
- **No duplicate after a lost answer.** Before sending, the mapping is committed `uncertain` with a `pending` marker (run, order number, client). A retry looks the invoice up by order number and buyer (`GET /invoices.json?oid=&client_id=`) before creating again; a pending create of another run is left alone for two minutes. The invoice is also sent with `oid_unique: "yes"`, so Fakturownia itself refuses a second invoice for the order; that 422 also triggers the lookup.
- **Checked before sending.** The buyer must already be a Fakturownia client (it has a client mapping), there must be a position, and every position needs a name, a positive quantity, a price and a tax rate. Otherwise nothing is sent, and the record's export status is `rejected` with every reason, each naming the record (`line 2 (<record ID>): price_net is empty`). An unmapped buyer is **rejected, not pushed**: the run may only write its selected records, so it cannot push the client. Save the client record and issue again.
- **Amounts.** Each position carries its net unit price and its gross total (`total_price_gross`), which Fakturownia requires for a position without a product. The gross total is computed exactly, as an invoice position is: net value (price × quantity) rounded to the grosz, plus the VAT on that net value rounded to the grosz; `zw` and `np` add no VAT. For 10 × 19.11 at 23% that is 191.10 + 43.95 = 235.05. Fakturownia computes every other amount and the invoice totals.
- **Seller.** No seller fields are sent, so Fakturownia fills in the account's default company details (Ustawienia → Dane firmy). An account without them refuses every invoice with `seller_name: pole nie może być puste`.
- **No warehouse.** Positions carry name, code, quantity, unit (`quantity_unit`), prices and tax, never a Fakturownia `product_id`, and the invoice is sent with `exclude_from_stock_level: true`, so it moves no Fakturownia stock. ASSUMED, to check on a warehouse-enabled account: that such an invoice creates no warehouse document (WZ). If it does, turn off the automatic creation of warehouse documents for invoices in the account's warehouse settings; the setting's exact name is not in the API documentation and must be checked there.
- **Write-back.** After a create, the run writes the number, status, view URL (`https://<account>.fakturownia.pl/invoice/<token>`, which needs no login) and, when configured, KSeF fields to the source, and attaches the PDF (`GET /invoices/{id}.pdf`, fetched with the Bearer header) to the `file` attribute. The writes come from the extension, so they trigger no push. Every write-back field is owned by Fakturownia: the mapping's content hash is the written-back state, so an unchanged webhook writes nothing.
- **Deleted invoices.** A deleted invoice clears the scalar write-back fields (`remove_values`) and removes the mapping. The PDF stays on the record. The extension never deletes an invoice itself, paid or not. `pull` cannot see deletions (Fakturownia does not list deleted invoices), so only a delete webhook clears them, and Fakturownia documents none for invoices yet.
- **Invoices made in Fakturownia.** With `link_existing_by_oid`, an invoice whose `oid` equals exactly one source record's order-number attribute (an `attricat.read` `lookup`) is linked to it: mapping and write-back, as if it had been issued from Attricat. Several matches are logged and skipped, as is a record that already has an invoice.

## Show the actions on a blueprint

**Issue invoice in Fakturownia** appears on records, and the bulk action in the
Explorer, only for blueprints whose views list them (Attricat 1.8 or later):

```yaml
views:
  detail:
    type: stack
    extensions: [attricat-fakturownia/issue-invoice]
  table:
    type: table
    extensions: [attricat-fakturownia/issue-invoices]
```

A `detail` stack without `children` keeps the default record layout, and a
`table` without `columns` the default Explorer columns.

## Configuration

```json
{
  "account": "acme",
  "clients": {
    "blueprint": "client",
    "context": "default",
    "fields": [
      {"field": "name", "attribute": "name", "owner": "both"},
      {"field": "tax_no", "attribute": "nip", "owner": "attricat"},
      {"field": "email", "attribute": "email", "owner": "fakturownia"},
      {"field": "city", "attribute": "city", "owner": "both"}
    ]
  },
  "invoices": {
    "source": {"blueprint": "sales_order", "context": "default"},
    "buyer": "customer",
    "header": {
      "kind": "vat",
      "issue_date": null,
      "sell_date": "document_date",
      "oid": "number",
      "description": "note",
      "payment_to_days": 14,
      "payment_type": "transfer",
      "currency": "PLN"
    },
    "positions": {
      "lines": {"blueprint": "sales_order_line", "parent": "sales_order"},
      "name": "item.name",
      "code": "item.code",
      "quantity": "quantity",
      "unit": "item.base_unit.code",
      "price_net": "unit_price_net",
      "tax": {"default": "23"}
    },
    "write_back": {
      "number": "invoice_number",
      "status": "invoice_status",
      "url": "invoice_url",
      "ksef_number": "ksef_number",
      "ksef_status": "ksef_status",
      "file": "invoice_pdf"
    },
    "link_existing_by_oid": false
  }
}
```

`blueprint` and `context` are codes. The extension looks up their IDs, and creates records on the blueprint's current published revision, so publishing a new revision needs no reconfiguration. `context` defaults to `default`. A `pull` whose blueprint or context does not exist fails before it fetches anything.

The supported client fields are `name`, `tax_no`, `email`, `phone`, `street`, `city`, `post_code` and `country`.

`invoices` is optional. Every key:

| Key | Meaning |
| --- | --- |
| `source.blueprint` | Code of the blueprint invoices are issued from. It must not be the `clients` blueprint. |
| `source.context` | Code of the context values are read and written in; default `default`. |
| `buyer` | A relationship on the source that points at the `clients` blueprint. The invoice's `client_id` is that client's Fakturownia ID. |
| `header.kind` | `vat` (default) or `bill`. |
| `header.issue_date`, `header.sell_date` | Date (or string) attributes on the source; `null` lets Fakturownia use today (ASSUMED). |
| `header.oid` | Attribute holding the source's number, sent as the order number (`oid`); `null` sends the source record's ID. |
| `header.description` | Attribute sent as the description; `null` sends none. |
| `header.payment_to_days`, `header.payment_type`, `header.currency` | Fixed values: days to pay (`payment_to_kind`), Fakturownia's payment type, an ISO currency code. Left out when `null`. |
| `positions.lines` | `{blueprint, parent}`: the line records whose `parent` relationship points at the source, read with `attricat.read` `incoming`. Without it the source is the single position. |
| `positions.name`, `code`, `quantity`, `unit`, `price_net` | Attribute paths from the line (or the source): up to three relationship hops, then the value, such as `item.base_unit.code`. A relationship is followed to its first target; a target without the leaf (on a multi-target relationship) yields nothing. `name`, `quantity` and `price_net` are required per position. Decimal commas become points. |
| `positions.tax` | `{"attribute": "<path>"}`, `{"default": "23"}` (one of `23`, `8`, `5`, `0`, `zw`, `np`), or both, the default filling empty values. |
| `write_back.number`, `status`, `url`, `ksef_number`, `ksef_status` | Optional string attributes on the source for the invoice number, its status (`issued`, `sent`, `paid`, `partial`, `rejected`), the view URL, the KSeF number (`gov_id`) and the KSeF status (`gov_status`). |
| `write_back.file` | Optional file attribute on the source that receives the invoice PDF. |
| `link_existing_by_oid` | Link invoices made in Fakturownia by their order number; needs `header.oid`. |

The section is validated against the published revisions before any Fakturownia call: every code exists, write-back fields are strings (`file` a file attribute), dates are dates or strings, `buyer` points at the clients blueprint, `lines.parent` at the source, and every path resolves. A run with a bad configuration fails in `prepare` with a message naming the code.

**Setup:**

1. Set the extension's `fakturownia-api-token` secret to the API token: the manifest declares it as required, so the extension's **Secrets** tab lists it (or `printf %s "$TOKEN" | acli extension secret set attricat-fakturownia fakturownia-api-token --value-stdin`). Secrets belong to this extension and need Attricat 1.12 or later. To use another name, set `token_secret` and add a secret by that name.
2. Create a webhook endpoint (Webhooks section, or `acli extension webhook create attricat-fakturownia --webhook fakturownia --endpoint main`).
3. In Fakturownia (Ustawienia → Ustawienia konta → Integracja → Webhooki, or `POST /webhooks.json` with `{"webhook": {"kind": "client:update", "url": "..."}}`), register the endpoint's URL for `client:create` and `client:update`, and with invoices also for `invoice:create` and `invoice:update`. Append `?kind=<kind>` to each URL, such as `…?kind=client:update`: Fakturownia's body does not say which event fired, and the kind is kept with each delivery, so the webhook receipts show it. `client:destroy` is accepted too, but a client deleted in Fakturownia is only logged: the extension never deletes Attricat records. Leave the webhook's **Api token** empty; see [Webhooks](#webhooks).
4. Schedule `pull` (for example every 30 minutes). Its first run only sets the watermarks, the invoices one included. Attricat 1.9.0 refuses to schedule an operation for an extension whose configuration has a key containing `token` or `secret`, so leave out `token_secret` while its default fits.
5. With invoices: fill in the account's company details in Fakturownia (Ustawienia → Dane firmy), give the source blueprint the write-back attributes, push the buyers (save each client record once) and, on a warehouse-enabled Fakturownia account, check the warehouse setting described under [Invoices](#invoices).

### Webhooks

Fakturownia documents its deliveries on its help page, [Integracja danych z
Fakturowni za pomocą
webhooków](https://pomoc.fakturownia.pl/1239014-integracja-danych-z-fakturowni-za-pomoca-webhookow):
a POST whose JSON body holds the changed object under `client`, `deal` (an
invoice) or `product`, with `app_name`, `locale` and `api_token`. The body
names no event. Any answer other than 2xx, a redirect included, is retried,
and after 25 failures Fakturownia turns the webhook off and emails the account
owner. `receive` therefore acknowledges with 200 every body it cannot read.
Deliveries are not deduplicated. The objects carry no `updated_at`, and an
object can return to an earlier state (an invoice paid, then partly paid, then
paid again), so any key built from a delivery would drop a real change. A
repeated delivery is harmless, since the handler fetches the current object
and writes only what differs, and rare, since Fakturownia retries only
answers other than 2xx.

The `api_token` is a value the receiver may set on the webhook, not the
account's API token, and the extension ignores it. The endpoint's URL, with
its unguessable path, is the credential, and a delivery is only a hint: the
handler fetches the object with the API token before it changes anything, so
a request with a leaked URL costs one GET at most. To retire a leaked URL,
rotate the endpoint and register the new URL. `verify_webhook_token` is
ignored since 0.4.1 and still accepted, so older configurations stay valid.

## Development

```sh
just check        # fmt, clippy, unit tests
just pack         # dist/attricat-fakturownia-<version>.tar.zst
just mock         # mock API on 127.0.0.1:9543, inspection on http://127.0.0.1:9580
```

The mock serves `*.fakturownia.pl` with a throwaway CA generated by `just mock-certs` (into the ignored `mock/tls/`). Start an Attricat worktree stack with:

```sh
ATTRICAT_DEVTOOLS=true
EXTENSION_DEV_EGRESS_RESOLVE=*.fakturownia.pl=127.0.0.1:9543
EXTENSION_DEV_EGRESS_CA_FILE=<this repo>/mock/tls/ca.pem
EXTENSION_WEBHOOK_BASE_URL=http://127.0.0.1:<API_PORT>/api/public/extension-webhooks/
```

The scenario resets the mock. If another mock already holds those ports, start a second one with `MOCK_API_PORT=9643 MOCK_INSPECT_PORT=9680 just mock`, point `EXTENSION_DEV_EGRESS_RESOLVE` at `127.0.0.1:9643`, and pass `MOCK_API_PORT=9643 MOCK_INSPECT_URL=http://127.0.0.1:9680` to `just e2e`.

Then run the scenario, which must print `All checks passed`:

```sh
ATTRICAT_WORKTREE=<attricat checkout> just e2e
```

It side-loads the archive, configures a fresh blueprint, registers the webhook endpoint in the mock and checks:

- create and update pushes
- that the create webhook's echo neither writes nor pushes again
- field ownership in both directions
- a client created in Fakturownia becoming a record
- clearing a field (the value is removed, not set to `""`)
- a lost webhook caught up by `pull`, which merges: it clears an emptied field, keeps Attricat-owned ones, stores the merged hash and pushes nothing back
- a 422 rejection carrying Fakturownia's field message
- a 503 on a push retried after `Retry-After`
- a 401 that fails `pull` with `fakturownia_unauthorized` and a message listed on the Failures tab, and a 429 retried after `Retry-After` without quarantine
- a forged delivery accepted as a hint only, its body data never written, and an unreadable body acknowledged
- that the token travelled only as a Bearer header
- a `pull` refused when the configured blueprint code does not exist

and, with a fresh order, line, product and unit blueprint set and a single-line goods issue:

- an invoice issued from an order with two lines (`client_id`, `oid`, positions with units and the default tax, no product IDs, `exclude_from_stock_level`), with number, status, view URL and the PDF written back
- a second action on the same order refused
- a create whose answer is lost retried without a second invoice
- a line without a price rejected before anything is sent, and a Fakturownia 422 rejection with its field message
- a 401 that ends the issuing run with `fakturownia_unauthorized`
- the paid status and KSeF number arriving by webhook, and a lost invoice webhook caught up by `pull`
- a deleted invoice clearing the order, which is then invoiced again
- an invoice made in the mock linked to its order with `link_existing_by_oid`
- a single-line goods issue invoiced from the record itself

## Open issues

Found while building against the host. These need Core work or a decision.
Reading records from operation runs, looking up blueprints and contexts by
code, and clearing values of any type were added to Core after the first version
(`attricat.read` `record`, `blueprint` and `context`; intents' `remove_values`)
and are used since 0.2.0. For invoices, Core then let selection runs read the
records linked from their selection and attach a fetched file to a selected
record (`files.attach.v1`), used since 0.3.0. All are JSON-level, so the
vendored WIT is unchanged.

1. **Mappings outlive the installation.** Removing the extension keeps its export mappings, so a re-installation pointed at another Fakturownia account would match the old external IDs. With a static API token there is no connection to scope mappings by; a fixed-token connection kind in Core would fix this and allow several accounts.
2. **Attricat-owned fields edited in Fakturownia stay diverged.** The inbound path ignores them, and the extension's own writes never trigger a push, so nothing restores them until the next Attricat edit. A pull could push them back; that is a policy decision.

## Assumptions to verify

Checked on a real account: the registration body `{webhook: {kind, url}}`
answers 201, the kinds `client:create`, `client:update`, `invoice:create` and
`invoice:update` are accepted and `client:delete` and `invoice:delete` are
refused with 422, and a client created or edited in the web app is delivered
with no event kind in its body. Creating an invoice (`POST /invoices.json`)
without a `product_id` needs `total_price_gross` on every position, and with
no `seller_name` or `department_id` it takes the seller from the account's
company details (Ustawienia → Dane firmy): an account without them answers
422 with `seller_name: pole nie może być puste`, and the same answer lists
every position's missing gross total. The view link, the PDF and the
`invoice:create` webhook work as described. Still to check:

- Whether changes made **through the API** fire webhooks, the delivery timeout, and whether payments and KSeF status changes fire.
- Whether invoices have a delete kind (the mock assumes `invoice:destroy`).
- Whether clients store and filter by **`external_id`**, which is used to find a create whose answer was lost.
- The status of a create (assumed 201), the `updated_at` format, and the rate limits (the host permission allows 60 requests a minute).
- **Invoice webhooks**: whether payments and KSeF numbers fire `invoice:update` (the KSeF documentation says a KSeF number does).
- **Error bodies**: that answers other than a 422 (a refused token, a 404, a 5xx) carry `{"code":"error","message":"<text>"}`, and that a throttled request is answered 429 with `Retry-After` in seconds. The mock answers so; an unexpected shape only loses the text, and the message names the HTTP status instead.
- **Invoice lookup**: that `GET /invoices.json` filters by `oid` and `client_id` together, used to find a create whose answer was lost.
- The 422 Fakturownia answers to a duplicate `oid` with `oid_unique: "yes"` (a real account numbered its first invoice `1/11/2026`: the format follows the account's numbering settings, and the mock uses `FV <n>/<MM>/<YYYY>`), and that an omitted issue or sell date means today.
- **Warehouse**: that an invoice whose positions have no `product_id`, sent with `exclude_from_stock_level`, creates no warehouse document on a warehouse-enabled account.

## Next

- Corrections (faktury korygujące).
- One invoice from several source records.
- Proformas and advance invoices.
- Product catalogue sync.
- Pushing changes of an issued invoice back to Fakturownia.
- First-time linking of clients by NIP, and a drift report.
