#!/usr/bin/env bash
# Build and sign an Arch package and a repository, without installing anything.
set -euo pipefail
task_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
export GNUPGHOME=${GNUPGHOME:-${XDG_DATA_HOME:-$HOME/.local/share}/quillc/gnupg}
export GPGKEY=${GPGKEY:-$(cat "$task_root/packaging/arch/signing-key.fingerprint")}
for command in python3 git tar gzip sha256sum makepkg repo-add gpg rustup cargo; do
    command -v "$command" >/dev/null || { printf 'Missing command: %s\n' "$command" >&2; exit 1; }
done
gpg --list-secret-keys "$GPGKEY" >/dev/null
mkdir -p "$task_root/target"
(
    cd "$task_root"
    cargo metadata --locked --format-version 1 >/dev/null
)
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git -C "$task_root" show -s --format=%ct HEAD)}
task_build=$(mktemp -d "$task_root/target/arch-build.XXXXXX")
task_public="$task_root/target/arch-repository"
mkdir -p "$task_public/packages/sources" "$task_public/packages/x86_64"

# Snapshot tracked compiler files from the working tree. Other local artifacts
# and private keys never enter the source archive. Record any included edits.
python3 - "$task_root" "$task_build" <<'PY'
import json, os, re, shutil, subprocess, sys, tomllib
from pathlib import Path
root, build = map(Path, sys.argv[1:])
version = tomllib.loads((root / 'Cargo.toml').read_text())['workspace']['package']['version']
if not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+', version):
    raise SystemExit('The Arch build requires a numeric major.minor.patch version.')
files = ['Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'LICENSE', 'README.md', 'docs/architecture.md', 'docs/arch-packaging.md']
files += subprocess.check_output(['git', 'ls-files', '-z', 'crates'], cwd=root).decode().strip('\0').split('\0')
source = build / f'quillc-{version}'
source.mkdir()
for name in files:
    destination = source / name
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(root / name, destination)
revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip()
modified = subprocess.check_output(['git', 'diff', 'HEAD', '--name-only', '--', *files], cwd=root, text=True).splitlines()
(source / 'BUILD-SOURCE.json').write_text(json.dumps({
    'version': version, 'revision': revision, 'modified_files': modified,
    'source_date_epoch': int(os.environ['SOURCE_DATE_EPOCH']),
}, indent=2) + '\n')
(build / 'version').write_text(version)
PY
task_version=$(cat "$task_build/version")
task_archive="quillc-${task_version}.tar.gz"
tar --sort=name --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 --numeric-owner \
    -C "$task_build" -cf - "quillc-${task_version}" | gzip -n > "$task_build/$task_archive"
if [[ -e "$task_public/packages/sources/$task_archive" ]] && \
    ! cmp -s "$task_build/$task_archive" "$task_public/packages/sources/$task_archive"; then
    printf 'Refusing to replace source archive %s; increment the workspace version.\n' "$task_archive" >&2
    exit 1
fi
gpg --batch --yes --local-user "$GPGKEY" --detach-sign "$task_build/$task_archive"
task_checksum=$(sha256sum "$task_build/$task_archive")
task_checksum=${task_checksum%% *}
python3 - "$task_root" "$task_build" "$task_version" "$task_checksum" "$GPGKEY" <<'PY'
import re, sys
from pathlib import Path
root, build = map(Path, sys.argv[1:3])
version, checksum, key = sys.argv[3:]
recipe = (root / 'packaging/arch/PKGBUILD').read_text()
recipe = re.sub(r'^pkgver=.*$', 'pkgver=' + version, recipe, flags=re.M)
recipe = re.sub(r'^sha256sums=.*$', f"sha256sums=('{checksum}' 'SKIP')", recipe, flags=re.M)
recipe = re.sub(r'^validpgpkeys=.*$', f"validpgpkeys=('{key}')", recipe, flags=re.M)
(build / 'PKGBUILD').write_text(recipe)
PY
(
    cd "$task_build"
    # A fresh directory prevents accidental reuse of a package with old contents.
    PKGEXT=.pkg.tar.zst makepkg --sign --key "$GPGKEY" "$@"
    makepkg --printsrcinfo > .SRCINFO
)
task_packages=("$task_build"/*.pkg.tar.zst)
for task_package in "${task_packages[@]}"; do
    gpg --verify "$task_package.sig" "$task_package"
    task_destination="$task_public/packages/x86_64/$(basename "$task_package")"
    if [[ -e "$task_destination" ]] && ! cmp -s "$task_package" "$task_destination"; then
        printf 'Refusing to replace %s; increment pkgver or pkgrel.\n' "$task_destination" >&2
        exit 1
    fi
    cp "$task_package" "$task_package.sig" "$task_public/packages/x86_64/"
done
cp "$task_build/$task_archive" "$task_build/$task_archive.sig" "$task_public/packages/sources/"
cp "$task_build/PKGBUILD" "$task_build/.SRCINFO" "$task_public/packages/"
gpg --armor --export "$GPGKEY" > "$task_public/packages/quillc.asc"
printf '%s\n' "$GPGKEY" > "$task_public/packages/signing-key.fingerprint"
(
    cd "$task_public/packages/x86_64"
    task_repo_packages=("${task_packages[@]##*/}")
    task_verify=()
    [[ ! -e quill.db.tar.gz ]] || task_verify=(--verify)
    repo-add --sign --key "$GPGKEY" --include-sigs "${task_verify[@]}" quill.db.tar.gz "${task_repo_packages[@]}"
    # Static assets need actual files for the aliases pacman requests.
    for task_alias in quill.db quill.db.sig quill.files quill.files.sig; do
        task_resolved=$(readlink "$task_alias")
        rm "$task_alias"
        cp "$task_resolved" "$task_alias"
    done
    rm -f -- *.old
    gpg --verify quill.db.sig quill.db
)
cp "$task_root/packaging/arch/_headers" "$task_public/_headers"
printf '\nSigned repository: %s\nBuild directory: %s\n' "$task_public" "$task_build"
