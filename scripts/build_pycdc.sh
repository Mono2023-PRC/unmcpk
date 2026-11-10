#!/usr/bin/env bash
# Build the vendored CPython 2.7 decompiler (`pycdc/`) next to our own binary.
#
#   bash scripts/build_pycdc.sh [--output PATH]
#
# `pycdc/` is kept exactly as vendored (no local patches). The only thing a
# mingw toolchain needs is a two-argument `mkdir`, which this script supplies
# through a generated shim header inside `target/pycdc-build/`, so `git status`
# never shows build noise.
#
# Compiler selection order: $CXX, ziglang from a python that has the `ziglang`
# wheel (`python -m ziglang c++`), then g++ / clang++.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
src="$root/pycdc"
work="$root/target/pycdc-build"

case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) exe=".exe"; windows=1 ;;
    *) exe=""; windows=0 ;;
esac
out="$root/bin/pycdc$exe"

while [ $# -gt 0 ]; do
    case "$1" in
        --output) out="$2"; shift 2 ;;
        -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

[ -d "$src" ] || { echo "missing $src (the vendored decompiler)" >&2; exit 1; }

cxx=()
if [ -n "${CXX:-}" ]; then
    cxx=("$CXX")
else
    for candidate in python python3 py; do
        if command -v "$candidate" >/dev/null 2>&1 &&
            "$candidate" -c "import ziglang" >/dev/null 2>&1; then
            cxx=("$candidate" -m ziglang c++)
            break
        fi
    done
    if [ ${#cxx[@]} -eq 0 ]; then
        for candidate in g++ clang++; do
            if command -v "$candidate" >/dev/null 2>&1; then
                cxx=("$candidate")
                break
            fi
        done
    fi
fi
[ ${#cxx[@]} -gt 0 ] || {
    echo "no C++ compiler found: set CXX, or install ziglang (pip install ziglang) / g++ / clang++" >&2
    exit 1
}

echo "pycdc: ${cxx[*]}"

rm -rf "$work"
mkdir -p "$work" "$(dirname "$out")"
cp -r "$src"/. "$work"/
rm -rf "$work/.git"

flags=(-std=c++11 -O2 -w -I"$work")
if [ "$windows" = 1 ]; then
    # Mingw declares `mkdir` with a single argument; pycdc's POSIX branch calls
    # the two-argument form. Overload it here instead of editing the sources.
    cat > "$work/mkdir_shim.h" <<'EOF'
#include <direct.h>
static inline int mkdir(const char *path, int) { return _mkdir(path); }
EOF
    flags+=(-include "$work/mkdir_shim.h")
fi

sources=(
    "$work/pycdc.cpp"
    "$work/ASTree.cpp"
    "$work/ASTNode.cpp"
    "$work/bytecode.cpp"
    "$work/data.cpp"
    "$work/pyc_code.cpp"
    "$work/pyc_module.cpp"
    "$work/pyc_numeric.cpp"
    "$work/pyc_object.cpp"
    "$work/pyc_sequence.cpp"
    "$work/pyc_string.cpp"
)
# One translation unit per bytecode map; `bytecode.cpp` references all of them.
for f in "$work"/bytes/python_*.cpp; do sources+=("$f"); done

"${cxx[@]}" "${flags[@]}" -o "$out" "${sources[@]}"
# zig/lld also drops a multi-megabyte .pdb next to the binary; nobody needs it.
rm -f "${out%.exe}.pdb" "${out%.exe}.lib" "${out%.exe}.exp" 2>/dev/null
echo "pycdc: wrote $out ($(du -h "$out" | cut -f1))"
