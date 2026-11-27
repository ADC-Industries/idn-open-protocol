# Paths for the development scripts (sourced): the workspace (ROOT), its release
# programs (B; IDN_BIN overrides), the build kit beside it (K; IDN_BUILD_KIT overrides:
# SDKs, test media and every script's logs and results, none of it in the repository),
# and `workdir NAME`, a script's folder of logs, states and results in the kit.
ROOT=$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/../.." && pwd)
B=${IDN_BIN:-$ROOT/target/release}
K=${IDN_BUILD_KIT:-$(cd "$ROOT/.." && pwd)/build-kit}
workdir() {
  mkdir -p "$K/$1"
  echo "$K/$1"
}
