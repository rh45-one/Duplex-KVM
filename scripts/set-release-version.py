"""Stamp the stable release tag into the workspace version before building assets."""
import os
from pathlib import Path
import re

tag = os.environ['RELEASE_TAG']
if not re.fullmatch(r'v?(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)', tag):
    raise SystemExit('Release tag must be a stable version such as v0.1.1')
manifest = Path('Cargo.toml')
text, count = re.subn(r'(?m)^version = "[^"]+"$', f'version = "{tag.removeprefix("v")}"',
                      manifest.read_text(), count=1)
if count != 1:
    raise SystemExit('Workspace version not found')
manifest.write_text(text)
