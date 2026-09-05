# The browser build's toolchain. Source this, then build with tools/web/build.sh.
#
#   . tools/web/env.sh
#
# Luau is C++, and Rust's wasm32-unknown-unknown target ships no C/C++ toolchain
# or runtime, so the C++ is compiled with the WASI SDK's clang FOR wasm32-wasip1
# (its headers and libc are the only ones that exist for wasm) and linked into
# the wasm32-unknown-unknown module, which shares the C ABI. Three things make
# that work, and each is an environment variable below:
#
#   * `--target=wasm32-wasip1` after cc-rs's own `--target`: the last one wins.
#   * `-fwasm-exceptions` with the NEW encoding (`-wasm-use-legacy-eh=false`):
#     the SDK's exception-enabled C++ runtime (`lib/wasm32-wasip1/eh/`) is built
#     with it, and a module that mixes the two encodings is refused by the
#     browser at instantiation ("mix of legacy and new exception handling").
#   * `-include cstdlib`: Luau's parser calls strtod/strtoull through an include
#     it gets transitively elsewhere and not from wasi-libc's headers.
#
# The SDK is fetched once into ~/.cache/floptle-web (about 130 MB) unless
# WASI_SDK_PATH already points at one. wasi-sdk 33 or newer is required: that
# is the release whose sysroot carries a C++ runtime that can throw.

WASI_SDK_VERSION="${WASI_SDK_VERSION:-34}"
if [ -z "${WASI_SDK_PATH:-}" ]; then
    case "$(uname -s)-$(uname -m)" in
        Linux-x86_64) _fl_sdk_os=x86_64-linux ;;
        Linux-aarch64) _fl_sdk_os=arm64-linux ;;
        Darwin-arm64) _fl_sdk_os=arm64-macos ;;
        Darwin-x86_64) _fl_sdk_os=x86_64-macos ;;
        *) echo "tools/web/env.sh: no wasi-sdk build for $(uname -s)-$(uname -m); set WASI_SDK_PATH" >&2 ;;
    esac
    WASI_SDK_PATH="$HOME/.cache/floptle-web/wasi-sdk-${WASI_SDK_VERSION}.0-${_fl_sdk_os}"
    if [ ! -x "$WASI_SDK_PATH/bin/clang" ]; then
        _fl_tar="wasi-sdk-${WASI_SDK_VERSION}.0-${_fl_sdk_os}.tar.gz"
        echo "fetching wasi-sdk ${WASI_SDK_VERSION} into $HOME/.cache/floptle-web …" >&2
        mkdir -p "$HOME/.cache/floptle-web" \
            && curl -sSL -o "$HOME/.cache/floptle-web/$_fl_tar" \
                "https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-${WASI_SDK_VERSION}/$_fl_tar" \
            && tar -xzf "$HOME/.cache/floptle-web/$_fl_tar" -C "$HOME/.cache/floptle-web" \
            && rm -f "$HOME/.cache/floptle-web/$_fl_tar"
    fi
fi
export WASI_SDK_PATH

export CC_wasm32_unknown_unknown="$WASI_SDK_PATH/bin/clang"
export CXX_wasm32_unknown_unknown="$WASI_SDK_PATH/bin/clang++"
export AR_wasm32_unknown_unknown="$WASI_SDK_PATH/bin/llvm-ar"
export CFLAGS_wasm32_unknown_unknown="--target=wasm32-wasip1"
export CXXFLAGS_wasm32_unknown_unknown="--target=wasm32-wasip1 -fwasm-exceptions -mllvm -wasm-use-legacy-eh=false -include cstdlib"
# luau0-src guesses `stdc++` for anything that is not macOS; the SDK ships libc++.
export CXXSTDLIB_wasm32_unknown_unknown="c++"
