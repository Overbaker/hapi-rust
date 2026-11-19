#!/usr/bin/env bash
set -euo pipefail
: "${HAPI_WRAPPER_BENCH_DIR:?Choose an output directory for the samples}"
repo=$(cd "$(dirname "$0")/../.." && pwd)
mkdir -p "$HAPI_WRAPPER_BENCH_DIR"
out=$(realpath "$HAPI_WRAPPER_BENCH_DIR")
cp "${HAPI_COMPILED_CLI:-$repo/cli/dist-exe/bun-linux-x64/hapi}.build.json" "$out/compiled.build.json"
scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj '/CN=HAPI benchmark CA' \
    -addext 'basicConstraints=critical,CA:TRUE' -addext 'keyUsage=critical,keyCertSign,cRLSign' -keyout "$scratch/ca.key" -out "$scratch/ca.pem" 2>/dev/null
openssl req -new -newkey rsa:2048 -nodes -subj '/CN=127.0.0.1' -keyout "$scratch/tls.key" -out "$scratch/tls.csr" 2>/dev/null
printf 'subjectAltName=IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid:always\n' > "$scratch/extensions"
openssl x509 -req -in "$scratch/tls.csr" -CA "$scratch/ca.pem" -CAkey "$scratch/ca.key" -CAcreateserial \
    -days 2 -extfile "$scratch/extensions" -out "$scratch/tls.pem" 2>/dev/null
openssl verify -x509_strict -CAfile "$scratch/ca.pem" "$scratch/tls.pem"
cd "$repo/cli"
if (( $# == 0 )); then set -- http-ts http-rust https-light https-loaded; fi
for profile in "$@"; do
    case "$profile" in
        http-ts|http-rust) unset HAPI_BENCH_TLS_CERT HAPI_BENCH_TLS_KEY NODE_EXTRA_CA_CERTS ;;
        https-light|https-loaded)
            export HAPI_BENCH_TLS_CERT="$scratch/tls.pem" HAPI_BENCH_TLS_KEY="$scratch/tls.key" NODE_EXTRA_CA_CERTS="$scratch/ca.pem" ;;
        *) echo "Unknown profile: $profile" >&2; exit 2 ;;
    esac
    export TMPDIR="$scratch/$profile" HAPI_WRAPPER_BENCH_PROFILE="$profile" HAPI_WRAPPER_BENCH_OUT="$out/$profile.json"
    mkdir "$TMPDIR"
    echo "Starting $profile"
    bun x --no-install vitest run --config vitest.integration.config.ts src/runner/rustWrapper.integration.test.ts -t 'measures wrapper cohorts' --reporter=verbose
done
