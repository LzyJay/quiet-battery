"""Regenerate bundled third-party notices from locked Cargo dependencies."""
from pathlib import Path
from html.parser import HTMLParser
import argparse, hashlib, json, os, subprocess

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--offline', action='store_true')
args = parser.parse_args()
env = os.environ.copy()
env['CARGO_HOME'] = str(ROOT / '.build/cargo-home')
env['CARGO_TARGET_DIR'] = str(ROOT / '.build/target')
command = ['cargo', 'metadata', '--manifest-path', str(ROOT / 'Cargo.toml'),
           '--format-version', '1', '--locked', '--filter-platform', 'x86_64-pc-windows-msvc']
if args.offline:
    command.append('--offline')
metadata = json.loads(subprocess.check_output(command, env=env, text=True, encoding='utf-8'))
groups = {}
packages = []
for package in sorted(metadata['packages'], key=lambda p: (p['name'], p['version'])):
    if package['name'] == 'quiet-battery':
        continue
    directory = Path(package['manifest_path']).parent
    files = {p.name.lower(): p for p in directory.iterdir() if p.is_file()}
    candidates = ['license-mit', 'license-mit.txt', 'license.txt', 'license']
    selected = next((files[n] for n in candidates if n in files and
                     'permission is hereby granted' in files[n].read_text(encoding='utf-8').lower()), None)
    if selected is None or 'MIT' not in (package['license'] or ''):
        raise RuntimeError(f"Review license selection manually: {package['name']} {package['license']}")
    notices = [selected]
    if 'Unicode-3.0' in package['license']:
        notices.append(files['license-unicode'])
    name = f"{package['name']} {package['version']}"
    packages.append(f"- {name}: {package['license']} (MIT selected where offered as an alternative)")
    for source in notices:
        body = source.read_text(encoding='utf-8').replace('\r\n', '\n').strip()
        digest = hashlib.sha256(body.encode()).hexdigest()
        group = groups.setdefault(digest, {'body': body, 'packages': []})
        group['packages'].append(name)

class PlainText(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.parts = []
    def handle_starttag(self, tag, attrs):
        if tag in {'p', 'br', 'li', 'h1', 'h2', 'h3', 'pre', 'section'}:
            self.parts.append('\n')
    def handle_endtag(self, tag):
        if tag in {'p', 'li', 'h1', 'h2', 'h3', 'pre', 'section'}:
            self.parts.append('\n')
    def handle_data(self, data):
        self.parts.append(data)

rust_root = Path(subprocess.check_output(['rustc', '--print', 'sysroot'], text=True).strip())
rust_version = subprocess.check_output(['rustc', '--version'], text=True).strip()
copyright_file = rust_root / 'share/doc/rust/COPYRIGHT-library.html'
if copyright_file.exists():
    html = PlainText()
    html.feed(copyright_file.read_text(encoding='utf-8'))
    std_text = '\n'.join(line.rstrip() for line in ''.join(html.parts).splitlines())
    for _ in range(4):
        std_text = std_text.replace('\n\n\n', '\n\n')
else:
    # Minimal toolchains may omit rust-docs. Preserve the checked-in library
    # notice section for exactly the same compiler; otherwise require refresh.
    previous = ROOT / 'THIRD_PARTY_NOTICES.txt'
    marker = f'RUST STANDARD LIBRARY — {rust_version}\n'
    text = previous.read_text(encoding='utf-8') if previous.exists() else ''
    if marker not in text:
        raise RuntimeError('Install rust-docs for this compiler and regenerate notices before release.')
    std_text = text.split(marker, 1)[1].strip()

parts = ['Quiet Battery — third-party notices\n',
         'These components retain their original licenses; the project noncommercial license does not replace them.\n',
         'Locked Cargo packages (including build-time tools):\n' + '\n'.join(packages)]
for group in groups.values():
    parts.append('\n' + '=' * 72 + '\n' + ', '.join(group['packages']) + '\n\n' + group['body'])
parts.append('\n' + '=' * 72 + f'\nRUST STANDARD LIBRARY — {rust_version}\n' + std_text.strip())
(ROOT / 'THIRD_PARTY_NOTICES.txt').write_text('\n\n'.join(parts) + '\n', encoding='utf-8', newline='\n')
print(f'Bundled notices: {len(packages)} Cargo packages, {len(groups)} distinct texts, Rust library notices.')
