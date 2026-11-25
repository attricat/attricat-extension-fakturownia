# Development and release tasks for attricat-fakturownia.
#
# Attricat installs a zstd-compressed tar archive whose root contains the strict
# manifest and every artifact path declared by that manifest.

extension_id := "attricat-fakturownia"
version := `node -p "require('./manifest.json').version"`
dist_dir := "dist"
stage_dir := "dist/package"
archive := dist_dir / extension_id + "-" + version + ".tar.zst"

default: check

fmt:
  cargo fmt --check

test:
  cargo test --workspace

clippy:
  cargo clippy --workspace --all-targets -- -D warnings

check: fmt clippy test

# Build a component that imports only the attricat:host@1.2.0 ABI.
# wasm32-wasip2 would import ambient WASI interfaces, which Attricat never links.
build-server:
  #!/usr/bin/env bash
  set -euo pipefail
  command -v wasm-tools >/dev/null || { echo "Install wasm-tools: cargo install wasm-tools --locked" >&2; exit 1; }
  rustup target add wasm32-unknown-unknown
  cargo build --release --target wasm32-unknown-unknown -p attricat-fakturownia-server
  mkdir -p {{dist_dir}}
  wasm-tools component new target/wasm32-unknown-unknown/release/attricat_fakturownia_server.wasm -o {{dist_dir}}/server.wasm
  if wasm-tools component wit {{dist_dir}}/server.wasm | grep -E '^\s*import ' | grep -v 'attricat:host/.*@1\.2\.0'; then
    echo "server.wasm imports something other than attricat:host@1.2.0" >&2; exit 1
  fi

build: check build-server

# The archive Attricat's installer accepts: the manifest at the root plus
# exactly the files it declares, the README and the icon.
pack: build
  #!/usr/bin/env bash
  set -euo pipefail
  rm -rf {{stage_dir}}
  mkdir -p {{stage_dir}}/dist
  cp manifest.json README.md {{stage_dir}}/
  cp -R assets {{stage_dir}}/
  cp {{dist_dir}}/server.wasm {{stage_dir}}/dist/
  rm -f {{archive}}
  (cd {{stage_dir}} && tar -cf - manifest.json README.md assets dist/server.wasm) | zstd -q -o {{archive}}
  echo "Created {{archive}}"

# A throwaway CA and a *.fakturownia.pl certificate for the mock (mock/tls/,
# ignored by git). Point the Attricat API at mock/tls/ca.pem.
mock-certs:
  #!/usr/bin/env bash
  set -euo pipefail
  mkdir -p mock/tls && cd mock/tls
  [ -f server.pem ] && { echo "mock/tls already exists"; exit 0; }
  openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.pem -days 3650 \
    -subj "/CN=Fakturownia mock test CA" \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null
  openssl req -newkey rsa:2048 -nodes -keyout server.key -out server.csr -subj "/CN=*.fakturownia.pl" 2>/dev/null
  printf "subjectAltName=DNS:*.fakturownia.pl,DNS:fakturownia.pl\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n" > ext.cnf
  openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -out server.pem -days 3650 -extfile ext.cnf 2>/dev/null
  rm -f server.csr ext.cnf ca.srl
  echo "Created mock/tls"

# The Fakturownia mock: HTTPS API on 9543, inspection API on http://127.0.0.1:9580.
mock: mock-certs
  node mock/server.mjs

# End-to-end scenario against a running Attricat dev server and the mock
# (see e2e/verify.mjs). Requires ATTRICAT_API_URL and ATTRICAT_TOKEN.
e2e:
  node e2e/verify.mjs

clean:
  rm -rf {{dist_dir}} target
