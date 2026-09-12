#!/usr/bin/env bash
# The shared shipping gate: local, CI, and release all run this one script.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
cd "$root"

package=dekopon-openobserve-provider
core_package=dekopon-otel-query-core
component=openobserve-provider.wasm
checksum=openobserve-provider.wasm.sha256
core=target/wasm32-unknown-unknown/release/dekopon_openobserve_provider.wasm
wasm_tools_version=1.259.0
msrv=1.98.1

command -v jq >/dev/null 2>&1 || {
  echo "error: jq is required" >&2
  exit 1
}
command -v wasm-tools >/dev/null 2>&1 || {
  echo "error: wasm-tools $wasm_tools_version is required" >&2
  exit 1
}
if [[ "$(wasm-tools --version)" != "wasm-tools $wasm_tools_version" ]]; then
  echo "error: expected wasm-tools $wasm_tools_version" >&2
  exit 1
fi
if ! rustup run "$msrv" rustc --version >/dev/null 2>&1; then
  echo "error: Rust $msrv is required for the declared MSRV check" >&2
  exit 1
fi

cargo "+$msrv" check --locked --all-targets --workspace
cargo fmt --all -- --check
cargo test --locked --workspace
cargo clippy --all-targets --locked --workspace -- -D warnings
cargo check --locked --package "$package" --target wasm32-unknown-unknown
cargo clippy --locked --package "$package" --target wasm32-unknown-unknown --lib -- -D warnings
# The backend-neutral crate must stay buildable for the guest target on its own: a second backend
# component is the whole reason it is a separate crate, and a core that only compiles natively
# would make that repository's first build the place the mistake is found.
cargo check --locked --package "$core_package" --target wasm32-unknown-unknown

# The mirrored WIT, against the exact sources Cargo resolved. CI fetches them from the pinned
# release over the network; this compares the checkout Cargo already has, so the gate works offline
# too.
metadata=$(cargo metadata --locked --format-version 1)
manifest_for() {
  jq -er --arg name "$1" '.packages[] | select(.name == $name) | .manifest_path' <<<"$metadata"
}
sdk_manifest=$(manifest_for dekopon-provider-sdk)
http_manifest=$(manifest_for dekopon-provider-http)
clock_manifest=$(manifest_for dekopon-provider-clock)
cmp "$(dirname "$sdk_manifest")/wit/provider.wit" wit/deps/provider.wit
cmp "$(dirname "$http_manifest")/wit/deps/http.wit" wit/deps/http.wit
cmp "$(dirname "$clock_manifest")/wit/deps/clock.wit" wit/deps/clock.wit
# The world this component declares is the CLI world, so the component must export `run-command`.
grep -Fq 'include dekopon:provider/provider-cli@0.3.0;' wit/provider.wit

mkdir -p target/validation
cargo tree --locked --package "$package" --target wasm32-unknown-unknown --edges normal,build \
  --prefix none --format '{p}' | sort -u >target/validation/deps.tree
if grep -Eqi '^(wasi([^[:alnum:]]|$)|wasm-bindgen([^[:alnum:]]|$)|js-sys([^[:alnum:]]|$))' target/validation/deps.tree; then
  echo "error: forbidden ambient dependency" >&2
  grep -Ein '^(wasi([^[:alnum:]]|$)|wasm-bindgen([^[:alnum:]]|$)|js-sys([^[:alnum:]]|$))' target/validation/deps.tree >&2
  exit 1
fi

# Handwritten `unsafe` is forbidden everywhere the component can reach. There is no exemption in
# this repository: the core crate declares `#![forbid(unsafe_code)]`, and the component crate's only
# unsafe is inside the generated bindings.
# Comment lines are dropped before the match: a file is allowed to *discuss* unsafety, and the
# component's module documentation has to, since the generated bindings contain `unsafe` by
# construction.
found=$(grep -rn '\bunsafe\b' crates/*/src | awk '{
  line = $0
  sub(/^[^:]+:[0-9]+:/, "", line)
  if (line !~ /^[[:space:]]*(\/\/|\*)/) { print }
}')
if [[ -n "$found" ]]; then
  echo "error: handwritten unsafe source is forbidden" >&2
  echo "$found" >&2
  exit 1
fi
grep -Fq '#![forbid(unsafe_code)]' crates/dekopon-otel-query-core/src/lib.rs

./build.sh
test -s "$component"
test -s "$checksum"
test -s "$core"
wasm-tools validate "$core"
wasm-tools validate "$component"
wasm-tools metadata show "$core" >target/validation/core-metadata.txt
wasm-tools metadata show "$component" >target/validation/component-metadata.txt
grep -F 'wit-bindgen-rust' target/validation/component-metadata.txt >/dev/null

# Two host imports and no others. The broker's HTTP client is the only way out of this component;
# the wall clock is the only fact it reads that it did not compute. Any third import is a new
# authority nobody reviewed.
wasm-tools print "$core" | grep '(import ' >target/validation/core-imports.txt || true
test -s target/validation/core-imports.txt
if grep -Ev 'dekopon:http/client@1\.0\.0|dekopon:clock/wall@1\.0\.0' target/validation/core-imports.txt; then
  echo "error: unexpected core import" >&2
  exit 1
fi
if [[ $(wc -l <target/validation/core-imports.txt | tr -d ' ') != 2 ]]; then
  echo "error: expected exactly two core imports" >&2
  exit 1
fi

wasm-tools component wit "$component" >target/validation/component.wit
wasm-tools component wit -j "$component" >target/validation/component-wit.json
jq -e '
  (.worlds | length) == 1 and
  (.worlds[0].imports | length) == 2 and
  ((.worlds[0].exports | keys | sort) == ["describe", "invoke", "run-command"]) and
  (.interfaces | length) == 2 and
  ((.interfaces | map(.name) | sort) == ["client", "wall"]) and
  ([.interfaces[] | .packages[.package].name] | length) >= 0 and
  ((.interfaces | map(.package) | map(.) | unique | map(.) | length) == 2)
' target/validation/component-wit.json >/dev/null
jq -e '
  ([.interfaces[] | $packages[.package].name] | sort) ==
    ["dekopon:clock@1.0.0", "dekopon:http@1.0.0"]
' --argjson packages "$(jq '.packages' target/validation/component-wit.json)" \
  target/validation/component-wit.json >/dev/null
if grep -Eq 'wasi:|resolve-command' target/validation/component.wit; then
  echo "error: component exposes an ambient import or an unexpected export" >&2
  exit 1
fi

expected=$(awk '{print $1}' "$checksum")
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$component" | awk '{print $1}')
else
  actual=$(shasum -a 256 "$component" | awk '{print $1}')
fi
[[ "$actual" == "$expected" ]]
[[ $(awk 'NF {count++} END {print count+0}' "$checksum") == 1 ]]
[[ $(awk '{print $2}' "$checksum") == "$component" ]]

for forbidden in "$root" "${CARGO_HOME:-$HOME/.cargo}" "$(rustc --print sysroot)"; do
  if LC_ALL=C grep -aF -- "$forbidden" "$component" >/dev/null; then
    echo "error: component embeds local path $forbidden" >&2
    exit 1
  fi
done

printf 'all provider shipping gates passed; size=%s sha256=%s\n' \
  "$(wc -c <"$component" | tr -d ' ')" "$actual"
