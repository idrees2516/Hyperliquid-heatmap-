#!/usr/bin/env bash
# Build the wasm frontend and stage assets into frontend/dist/
# (equivalent of the original trunk build: wasm-bindgen --target web +
#  loader injection into dist/index.html)
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

echo "[web] building wasm…"
cargo build --release --target wasm32-unknown-unknown -p frontend

echo "[web] wasm-bindgen…"
rm -rf frontend/dist
mkdir -p frontend/dist
wasm-bindgen \
  --target web \
  --out-dir frontend/dist \
  --out-name frontend \
  target/wasm32-unknown-unknown/release/frontend.wasm

JS=$(ls frontend/dist | grep -E '^frontend(-[0-9a-f]+)?\.js$' | head -1)
WASM="${JS%.js}_bg.wasm"
V=$(date +%s)

echo "[web] injecting loader into index.html…"
python3 - "$JS" "$WASM" "$V" << 'PY'
import sys
js, wasm, ver = sys.argv[1], sys.argv[2], sys.argv[3]
src = open("frontend/index.html").read()
head_inject = (
    f'<link rel="modulepreload" href="/{js}?v={ver}">'
    f'<link rel="preload" href="/{wasm}?v={ver}" crossorigin="anonymous" as="fetch" type="application/wasm">'
)
body_inject = (
    '<script type="module">\n'
    f'import init from \'/{js}?v={ver}\';\n'
    f'init({{ module_or_path: \'/{wasm}?v={ver}\' }});\n'
    '</script>'
)
src = src.replace("</head>", head_inject + "</head>", 1)
src = src.replace("</body>", body_inject + "</body>", 1)
open("frontend/dist/index.html", "w").write(src)
print("wrote frontend/dist/index.html")
PY

echo "[web] done:"
ls -la frontend/dist/
