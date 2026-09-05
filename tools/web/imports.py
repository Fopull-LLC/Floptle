#!/usr/bin/env python3
"""List a wasm module's imports by module, and fail if any come from WASI.

    tools/web/imports.py path/to/floptle_web.wasm

The browser build compiles Luau's C++ for wasm32-wasip1, so its libc asks for
a handful of `wasi_snapshot_preview1` functions; `floptle-web/src/wasi.rs`
defines them under the symbol names libc imports by, and a defined symbol wins
at link time. This checks that it did: the finished module must import from
the wasm-bindgen glue only. A WASI import that survives means a new libc
function is in use and needs a shim — the link cannot notice, the page would
fail to instantiate, and this says which one, by name.
"""
import sys
from collections import defaultdict


def leb(d, i):
    r = s = 0
    while True:
        b = d[i]
        i += 1
        r |= (b & 0x7F) << s
        s += 7
        if not b & 0x80:
            return r, i


def imports(path):
    d = open(path, "rb").read()
    assert d[:4] == b"\0asm", f"{path} is not a wasm module"
    i, by_module = 8, defaultdict(list)
    while i < len(d):
        sid = d[i]
        i += 1
        n, i = leb(d, i)
        if sid != 2:
            i += n
            continue
        count, j = leb(d, i)
        for _ in range(count):
            l, j = leb(d, j)
            mod = d[j : j + l].decode()
            j += l
            l, j = leb(d, j)
            name = d[j : j + l].decode()
            j += l
            kind = d[j]
            j += 1
            if kind == 0:
                _, j = leb(d, j)
            elif kind == 1:
                j += 1
                flags, j = leb(d, j)
                _, j = leb(d, j)
                if flags & 1:
                    _, j = leb(d, j)
            elif kind == 2:
                flags, j = leb(d, j)
                _, j = leb(d, j)
                if flags & 1:
                    _, j = leb(d, j)
            elif kind == 3:
                j += 2
            elif kind == 4:
                j += 1
                _, j = leb(d, j)
            by_module[mod].append(name)
        break
    return by_module


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    by_module = imports(sys.argv[1])
    for mod, names in by_module.items():
        print(f"{mod}: {len(names)} import(s)")
    wasi = {m: n for m, n in by_module.items() if m.startswith("wasi_")}
    if wasi:
        for mod, names in wasi.items():
            print(f"  {mod} still imported: {', '.join(names)} — add a shim in crates/floptle-web/src/wasi.rs")
        return 1
    print("no WASI imports: every libc import was satisfied at link time")
    return 0


if __name__ == "__main__":
    sys.exit(main())
