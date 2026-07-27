#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 SOURCE_DIRECTORY OUTPUT_ZIP" >&2
  exit 2
fi

SOURCE_DIRECTORY="$1"
OUTPUT_ZIP="$2"

if [[ ! -d "$SOURCE_DIRECTORY" ]]; then
  echo "plugin source directory does not exist: $SOURCE_DIRECTORY" >&2
  exit 1
fi
if [[ ! -f "$SOURCE_DIRECTORY/index.html" ]]; then
  echo "plugin source directory must contain index.html" >&2
  exit 1
fi

python3 - "$SOURCE_DIRECTORY" "$OUTPUT_ZIP" <<'PY'
import os
from pathlib import Path
import stat
import sys
import zipfile

source = Path(sys.argv[1]).resolve()
output = Path(sys.argv[2]).resolve()
output.parent.mkdir(parents=True, exist_ok=True)

with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED) as archive:
    for path in sorted(source.rglob("*")):
        mode = path.lstat().st_mode
        if stat.S_ISLNK(mode):
            raise SystemExit(f"plugin bundle may not contain symbolic links: {path}")
        if not path.is_file():
            continue
        archive.write(path, path.relative_to(source).as_posix())
PY
