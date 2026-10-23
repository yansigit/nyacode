#!/usr/bin/env bash
set -euo pipefail

: "${TARGET:?TARGET is required}"
: "${ZIG_BIN:?ZIG_BIN is required}"
: "${MUSL_LINKER:?MUSL_LINKER is required}"
: "${TOOL_ROOT:?TOOL_ROOT is required}"
: "${GITHUB_ENV:?GITHUB_ENV is required}"

# Rust keeps its own musl CRT and uses musl-gcc to link. Native C++ dependencies
# use Zig's headers, so they also need the matching target libc++ and ABI runtime.
# Neither the host's glibc libstdc++ nor V8's private __Cr libc++ can provide it.
runtime_root="${TOOL_ROOT}/cxx-runtime"
mkdir -p "${runtime_root}/lib"
zig_target="${TARGET/-unknown-linux-musl/-linux-musl}"

# Exercise the out-of-line uint16 sort specialization used by BoringSSL, plus
# strings and exception handling. This catches runtime mismatches before Cargo's
# expensive release build, and creates Zig's target runtime archives in isolation.
cat > "${runtime_root}/probe.cpp" <<'EOF'
#include <algorithm>
#include <array>
#include <stdexcept>
#include <string>
extern "C" int nyacode_cxx_runtime_probe() {
  std::array<unsigned short, 4> values{7, 2, 9, 1};
  std::sort(values.begin(), values.end());
  try { throw std::runtime_error(std::to_string(values[0])); }
  catch (const std::exception& error) {
    return std::string(error.what()) == "1" ? 0 : 1;
  }
}
#ifndef NYACODE_PROBE_LIBRARY
int main() { return nyacode_cxx_runtime_probe(); }
#endif
EOF
ZIG_GLOBAL_CACHE_DIR="${runtime_root}/cache" "${ZIG_BIN}" c++ \
  -target "${zig_target}" -O2 -static -fno-sanitize=undefined \
  "${runtime_root}/probe.cpp" -o "${runtime_root}/zig-probe"

python3 - "${runtime_root}" <<'PY'
from pathlib import Path
import shutil
import sys

root = Path(sys.argv[1]).resolve()
libraries = []
for name in ("libc++", "libc++abi"):
    matches = list((root / "cache").rglob(f"{name}.a"))
    if len(matches) != 1:
        raise SystemExit(f"expected one Zig target {name}.a, found {len(matches)}")
    target = root / "lib" / f"{name}-zig.a"
    shutil.copyfile(matches[0], target)
    libraries.append('"' + str(target).replace('\\', '\\\\').replace('"', '\\"') + '"')
# Rust supplies libunwind; adding Zig's copy would create duplicate symbols.
# A linker group handles dependencies between the two C++ runtime archives.
(root / "lib" / "libc++.a").write_text("GROUP ( " + " ".join(libraries) + " )\n")
PY

cat > "${runtime_root}/linker" <<EOF
#!/usr/bin/env bash
set -euo pipefail
exec "${MUSL_LINKER}" -L"${runtime_root}/lib" -Wl,--eh-frame-hdr "\$@"
EOF
chmod +x "${runtime_root}/linker"

"${ZIG_BIN}" c++ -target "${zig_target}" -O2 -fPIC -fno-sanitize=undefined \
  -DNYACODE_PROBE_LIBRARY \
  -c "${runtime_root}/probe.cpp" -o "${runtime_root}/probe.o"
ar rcs "${runtime_root}/lib/libnyacode_cxx_probe.a" "${runtime_root}/probe.o"
cat > "${runtime_root}/probe.rs" <<'EOF'
unsafe extern "C" { fn nyacode_cxx_runtime_probe() -> i32; }
fn main() {
    assert_eq!(unsafe { nyacode_cxx_runtime_probe() }, 0);
    std::panic::set_hook(Box::new(|_| {}));
    assert!(std::panic::catch_unwind(|| panic!("runtime probe")).is_err());
}
EOF
rustc --edition 2024 --target "${TARGET}" \
  -C "linker=${runtime_root}/linker" -L "native=${runtime_root}/lib" \
  -l static=nyacode_cxx_probe -l c++ \
  "${runtime_root}/probe.rs" -o "${runtime_root}/musl-probe"
if readelf -l "${runtime_root}/musl-probe" | grep -q INTERP; then
  echo 'The musl C++ probe unexpectedly requires a dynamic loader' >&2
  exit 1
fi
case "$(uname -m):${TARGET}" in
  x86_64:x86_64-unknown-linux-musl|aarch64:aarch64-unknown-linux-musl)
    "${runtime_root}/musl-probe"
    ;;
esac

target_suffix="${TARGET//-/_}"
{
  echo 'CXXSTDLIB=c++'
  echo "CXXSTDLIB_${target_suffix}=c++"
  echo 'BORING_BSSL_RUST_CPPLIB=c++'
  echo "BORING_BSSL_RUST_CPPLIB_${target_suffix}=c++"
} >> "$GITHUB_ENV"
