#!/usr/bin/env bash
# Reproducible Linux x86_64 build of the Kraskus solo cmfd-node.
# Runs inside the pinned toolchain image (see KRASKUS/build/PINS). Inputs:
#   /src  read-only checkout of this repository at the commit being built
#   /out  empty output directory
# Every path that ends up in the binary is fixed (/src, /usr/local/cargo, /target),
# and remapped, so the result does not depend on the host.
set -euo pipefail

: "${CMFD_BUILD_SOURCE_COMMIT:?set to the full commit being built}"
UPSTREAM_COMMIT=3aa5369512f47d0b3c49a49a54a0395e71e0c000

cd /src
test "$(git rev-parse HEAD)" = "$CMFD_BUILD_SOURCE_COMMIT"
git merge-base --is-ancestor "$UPSTREAM_COMMIT" HEAD
test -z "$(git status --porcelain --untracked-files=no)"

export SOURCE_DATE_EPOCH="$(git show -s --format=%ct HEAD)"
export CARGO_TARGET_DIR=/target
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_RELEASE_DEBUG=0
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"
export RUSTFLAGS="--remap-path-prefix=/src=/build/commonfoundry --remap-path-prefix=/usr/local/cargo=/cargo"

cargo build --release --locked -p cmfd-node --features production-mainnet --bin cmfd-node
cargo build --release --locked -p kraskus-cmfd-prover --features production-mainnet --bin kraskus-cmfd-prover

# Upstream mainnet identity check (same assertions as upstream's
# mainnet-sync-recovery workflow), run against the binary just built.
mkdir -p /target/release/production-mainnet
cp packaging/mainnet/MAINNET-PLAN.json /target/release/production-mainnet/
/target/release/cmfd-node mainnet-launch-info > /out/MAINNET-LAUNCH-INFO.json
python3 - "$CMFD_BUILD_SOURCE_COMMIT" <<'PY'
import json, sys
value = json.load(open('/out/MAINNET-LAUNCH-INFO.json'))
assert value['source_commit'] == sys.argv[1], value['source_commit']
assert value['launch_plan']['network_id'] == '88296bc39c10e8bc1dd4818d4d42412fe5f08210651110377f495da299812f62'
assert value['launch_plan']['launch_plan_digest'] == '2133726558490606e89a8fe3499f32c9a35722ed0022e09b7cd1cd30239d04af'
assert value['beacon_round'] == 32747812
print('mainnet launch identity: OK')
PY
/target/release/cmfd-node --version | tee /out/VERSION.txt
/target/release/cmfd-node run --help | grep -q -- '--solo-pool-bind'
/target/release/kraskus-cmfd-prover --version | tee -a /out/VERSION.txt

cp /target/release/cmfd-node /out/cmfd-node
cp /target/release/kraskus-cmfd-prover /out/kraskus-cmfd-prover
chmod 0755 /out/cmfd-node /out/kraskus-cmfd-prover
git diff "$UPSTREAM_COMMIT" HEAD -- . ':(exclude)KRASKUS' ':(exclude).github/workflows/kraskus-*' > /out/KRASKUS-PATCH.diff
python3 - "$CMFD_BUILD_SOURCE_COMMIT" "$UPSTREAM_COMMIT" "$SOURCE_DATE_EPOCH" <<'PY'
import json, subprocess, sys
info = {
    "schema": "KRASKUS_CMFD_SOLO_BUILD_V1",
    "source_commit": sys.argv[1],
    "upstream_commit": sys.argv[2],
    "upstream_tag": "v1.0.8",
    "source_date_epoch": int(sys.argv[3]),
    "cargo_features": "production-mainnet",
    "rustc": subprocess.check_output(["rustc", "-Vv"], text=True),
    "toolchain_image": open('/src/KRASKUS/build/PINS').read().split('TOOLCHAIN_IMAGE=')[1].split()[0],
}
json.dump(info, open('/out/BUILD-INFO.json', 'w'), indent=2, sort_keys=True)
PY
cd /out
sha256sum cmfd-node kraskus-cmfd-prover KRASKUS-PATCH.diff > SHA256SUMS
cat SHA256SUMS
