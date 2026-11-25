# Agent instructions

## Host ABI

The extension targets the additive `attricat:host@1.2.0` ABI (`host_api`
`>=1.2.0, <2.0.0`; WIT vendored in `server/component/wit`) and the
`webhook-extension` world. `server/component/wit/attricat-extension.wit` is the
[attricat-extension-sdk](https://github.com/attricat/attricat-extension-sdk)
`wit/attricat-host-1.2.0.wit` verbatim; never copy the core repository's
unreleased `wit-host/`, and never edit the vendored file by hand.
`just build-server` fails if the component imports anything other than
`attricat:host@1.2.0`.

## Layout

- `server/sync-core`: host-independent logic (configuration, field ownership,
  projections and hashes, webhook parsing). Unit-test new rules here.
- `server/component`: the WASM component; host calls are wrapped in `host.rs`.
- `mock/server.mjs`: the Fakturownia mock. Mark anything not taken from the
  official API documentation as ASSUMED in its header and in the README.

## Verification

Unit tests are not enough. Run `just check` and `just pack`, start `just mock`,
and run `ATTRICAT_WORKTREE=<attricat checkout> just e2e` against an Attricat
worktree stack configured as the README describes; it must print
`All checks passed`. Extend `e2e/verify.mjs` with every behaviour you add.

## No customer specifics

Never store anything specific to a customer here: names, data, account
subdomains, tokens or business rules. Test data stays fictional.
