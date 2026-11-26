# attricat-fakturownia

Two-way sync between Attricat and [Fakturownia](https://fakturownia.pl), on the
**`attricat:host@1.2.0`** ABI.

> **Spike.** This release syncs **clients** both ways and issues **sales
> invoices** from Attricat records. It has been tested against the local mock
> in `mock/`, not a real Fakturownia account. The webhook format in particular
> is assumed; see [Assumptions to verify](#assumptions-to-verify).
>
> 0.3.0 needs an Attricat Core that lets selection runs read linked records
> and attach files (`attricat-data.read` `record`/`incoming` in interactive
> runs, `files.attach.v1`), which is newer than v1.4.0. These are JSON-level
> additions that `host_api` cannot express, and older hosts refuse them.

## What it does

| Direction | How | Code |
| --- | --- | --- |
| Attricat → Fakturownia | `record.created.v1` / `record.updated.v1` on the configured blueprint (debounced 2 s) push the client with `POST /clients.json` or `PUT /clients/{id}.json`. The export mapping keeps the Fakturownia ID and the hash of the last agreed state. | `sync.rs` (`push_record`) |
| Fakturownia → Attricat | A Fakturownia webhook is a **hint**: `receive` checks it and keeps only `{resource, action, id}`; the `webhook.received.v1` handler fetches the client again with `GET /clients/{id}.json` and applies it. A forged request costs one GET at most. | `webhook.rs`, `sync.rs` (`apply_client`) |
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

Validation errors (HTTP 422) become the record's export status `rejected`, with Fakturownia's field messages.

### Invoices

An invoice is issued from a **source** record: a sales order whose lines are records pointing at it, or a single-line document such as a goods issue, which is its own position.

- **One invoice per source record.** The source record's export mapping holds the invoice as `invoice:<id>` (prefixed, so it never collides with a client's ID). A record that has one is refused, and the run's result names the invoice. A run that crashed after the create finishes the write-back on retry.
- **No duplicate after a lost answer.** Before sending, the mapping is committed `uncertain` with a `pending` marker (run, order number, client). A retry looks the invoice up by order number and buyer (`GET /invoices.json?oid=&client_id=`) before creating again; a pending create of another run is left alone for two minutes. The invoice is also sent with `oid_unique: "yes"`, so Fakturownia itself refuses a second invoice for the order; that 422 also triggers the lookup.
- **Checked before sending.** The buyer must already be a Fakturownia client (it has a client mapping), there must be a position, and every position needs a name, a positive quantity, a price and a tax rate. Otherwise nothing is sent, and the record's export status is `rejected` with every reason, each naming the record (`line 2 (<record ID>): price_net is empty`). An unmapped buyer is **rejected, not pushed**: the run may only write its selected records, so it cannot push the client. Save the client record and issue again.
- **No warehouse.** Positions carry name, code, quantity, unit (`quantity_unit`), net price and tax, never a Fakturownia `product_id`, and the invoice is sent with `exclude_from_stock_level: true`, so it moves no Fakturownia stock. The extension never recomputes totals. ASSUMED, to check on a warehouse-enabled account: that such an invoice creates no warehouse document (WZ). If it does, turn off the automatic creation of warehouse documents for invoices in the account's warehouse settings; the setting's exact name is not in the API documentation and must be checked there.
- **Write-back.** After a create, the run writes the number, status, view URL (`https://<account>.fakturownia.pl/invoice/<token>`, which needs no login) and, when configured, KSeF fields to the source, and attaches the PDF (`GET /invoices/{id}.pdf`, fetched with the Bearer header) to the `file` attribute. The writes come from the extension, so they trigger no push. Every write-back field is owned by Fakturownia: the mapping's content hash is the written-back state, so an unchanged webhook writes nothing.
- **Deleted invoices.** A deleted invoice clears the scalar write-back fields (`remove_values`) and removes the mapping. The PDF stays on the record. The extension never deletes an invoice itself, paid or not. `pull` cannot see deletions (Fakturownia does not list deleted invoices), so only the webhook clears them.
- **Invoices made in Fakturownia.** With `link_existing_by_oid`, an invoice whose `oid` equals exactly one source record's order-number attribute (an `attricat.read` `lookup`) is linked to it: mapping and write-back, as if it had been issued from Attricat. Several matches are logged and skipped, as is a record that already has an invoice.

## Configuration

```json
{
  "account": "acme",
  "token_secret": "fakturownia-api-token",
  "verify_webhook_token": true,
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

1. Store the API token as the workspace extension secret `fakturownia-api-token`.
2. Create a webhook endpoint (Webhooks section, or `acli extension webhook create attricat-fakturownia --webhook fakturownia --endpoint main`).
3. Register its URL in Fakturownia for `client:create`, `client:update` and `client:delete`, and with invoices also for `invoice:create`, `invoice:update` and `invoice:delete`.
4. Schedule `pull` (for example every 30 minutes). Its first run only sets the watermarks, the invoices one included.
5. With invoices: give the source blueprint the write-back attributes, push the buyers (save each client record once) and, on a warehouse-enabled Fakturownia account, check the warehouse setting described under [Invoices](#invoices).

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
- a 422 rejection
- a forged webhook refused
- that the token travelled only as a Bearer header
- a `pull` refused when the configured blueprint code does not exist

and, with a fresh order, line, product and unit blueprint set and a single-line goods issue:

- an invoice issued from an order with two lines (`client_id`, `oid`, positions with units and the default tax, no product IDs, `exclude_from_stock_level`), with number, status, view URL and the PDF written back
- a second action on the same order refused
- a create whose answer is lost retried without a second invoice
- a line without a price rejected before anything is sent, and a Fakturownia 422 rejection
- the paid status and KSeF number arriving by webhook, and a lost invoice webhook caught up by `pull`
- a deleted invoice clearing the order, which is then invoiced again
- an invoice made in the mock linked to its order with `link_existing_by_oid`
- a single-line goods issue invoiced from the record itself

## Spike findings

Found while building against the host. These need Core work or a decision.
Reading records from operation runs, looking up blueprints and contexts by
code, and clearing values of any type were added to Core after the spike
(`attricat.read` `record`, `blueprint` and `context`; intents' `remove_values`)
and are used since 0.2.0. For invoices, Core then let selection runs read the
records linked from their selection and attach a fetched file to a selected
record (`files.attach.v1`), used since 0.3.0. All are JSON-level, so the
vendored WIT is unchanged.

1. **Mappings outlive the installation.** Removing the extension keeps its export mappings, so a re-installation pointed at another Fakturownia account would match the old external IDs. With a static API token there is no connection to scope mappings by; a fixed-token connection kind in Core would fix this and allow several accounts.
2. **Attricat-owned fields edited in Fakturownia stay diverged.** The inbound path ignores them, and the extension's own writes never trigger a push, so nothing restores them until the next Attricat edit. A pull could push them back; that is a policy decision.

## Assumptions to verify

Fakturownia documents only its webhook management API (`/webhooks.json`). To check on a real account:

- **Registration body** (assumed `{webhook: {kind, url}}`) and the event kinds (assumed `client:create|update|delete`).
- **Delivery body** (assumed `{kind, api_token, client: {...}}`): is the object complete, and does `api_token` carry the account token? `verify_webhook_token: false` turns that check off.
- Whether changes made **through the API** fire webhooks, plus retries, timeouts, and whether deletes, payments and KSeF status changes fire.
- Whether clients store and filter by **`external_id`**, which is used to find a create whose answer was lost.
- The status of a create (assumed 201), the `updated_at` format, and the rate limits (the host permission allows 60 requests a minute).
- **Invoice webhooks**: the kinds (assumed `invoice:create|update|delete`) and body (assumed `{kind, api_token, invoice: {...}}`), and whether payments and KSeF numbers fire `invoice:update` (the KSeF documentation says a KSeF number does).
- **Invoice lookup**: that `GET /invoices.json` filters by `oid` and `client_id` together, used to find a create whose answer was lost.
- The 422 Fakturownia answers to a duplicate `oid` with `oid_unique: "yes"`, the number format (the mock uses `FV <n>/<MM>/<YYYY>`), and that an omitted issue or sell date means today.
- **Warehouse**: that an invoice whose positions have no `product_id`, sent with `exclude_from_stock_level`, creates no warehouse document on a warehouse-enabled account.

## Next

- Corrections (faktury korygujące).
- One invoice from several source records.
- Proformas and advance invoices.
- Product catalogue sync.
- Pushing changes of an issued invoice back to Fakturownia.
- First-time linking of clients by NIP, and a drift report.
