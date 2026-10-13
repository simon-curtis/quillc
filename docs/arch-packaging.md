# Arch Linux package

Quill's package is `quillc`, currently `0.0.1-1`, for baseline x86-64 Linux.
The signed repository is served at
`https://quill.simoncurtis.dev/packages/x86_64`.
The existing website has its own Cloudflare Worker; only `/packages/*` is
handled by the `quill-packages` static-assets Worker.

## GitHub builds

The **Arch package** Actions workflow builds on compiler and packaging changes
to `main`, and can also be run manually on `main`. A fresh Arch Linux container
installs the build dependencies, imports the signing key from the encrypted
`ARCH_SIGNING_KEY` repository secret, and runs `scripts/build-arch.sh` as an
unprivileged user. Each run uploads a signed package, its source archive,
generated recipe, and repository databases as `quillc-arch-<commit>`.
Download that artifact from the workflow run's page; artifacts are retained
for 30 days. Builds have no deployment dependency on the website workflow.

The **Website** workflow independently builds and publishes the website on
website or documentation changes to `main`. It preserves the existing pages
and Markdown reader and forwards existing blog routes to `quill-devlog`.
`CLOUDFLARE_API_TOKEN` is an encrypted repository secret scoped to Worker
deployments in the Quill account and routes in the `simoncurtis.dev` zone.
Each deployment checks that the live site reports the exact Git commit.
Both workflows can be started from GitHub's Actions page.

Website staging is available locally with `python3 scripts/build-website.py`;
deployment uses `npx wrangler@4.150.0 deploy --config website/wrangler.jsonc`.
`target/website/` contains only selected public assets, documents, and results.
Compiler builds, Git metadata, and signing keys are excluded.

## Install with pacman

Download the public key and inspect its fingerprint:

```sh
curl --fail --location --output quillc.asc https://quill.simoncurtis.dev/packages/quillc.asc
gpg --show-keys --with-fingerprint quillc.asc
```

The fingerprint must be:

```text
DE2F 0B7A 28E6 45AE 83F8  7FB3 353C 9B63 F060 C5D8
```

Import and locally trust that key:

```sh
sudo pacman-key --add quillc.asc
sudo pacman-key --lsign-key DE2F0B7A28E645AE83F87FB3353C9B63F060C5D8
```

Add this section to `/etc/pacman.conf`:

```ini
[quill]
SigLevel = Required
Server = https://quill.simoncurtis.dev/packages/$arch
```

Then install:

```sh
sudo pacman -Syu quillc
quillc --version
```

The package installs `/usr/bin/quillc`, the license, implementation notes,
and source provenance under `/usr/share/doc/quillc`. It depends on the system
linker and Cargo because Quill's native and Cargo commands invoke them.
Compiler compatibility remains limited to the subset described in the README.

## Build from source

The published [PKGBUILD](https://quill.simoncurtis.dev/packages/PKGBUILD)
uses a SHA-256 checksum and verifies the source archive's detached signature.
Install `base-devel` and `rustup`; import `quillc.asc` into your own GPG
keyring with `gpg --import quillc.asc`. In an empty build directory:

```sh
curl --fail --location --output PKGBUILD https://quill.simoncurtis.dev/packages/PKGBUILD
makepkg --syncdeps
```

`prepare()` installs the pinned toolchain from `rust-toolchain.toml` when
missing and fetches locked dependencies. `build()` uses Cargo's frozen mode.
`check()` verifies the version, compiles and runs a native smoke program,
and builds the same fixture through Cargo integration.

## Build and publish a release

`Cargo.toml` holds the workspace version. Set a new numeric version there for
a new source release. `packaging/arch/PKGBUILD` holds the packaging revision
(`pkgrel`). The helper refreshes the generated recipe's version and checksum:

```sh
scripts/build-arch.sh
npx wrangler@4.150.0 deploy --config packaging/arch/wrangler.jsonc
```

The helper requires Python 3, Git, the Arch build tools, Rustup, and the signing
key. It snapshots tracked compiler files from the working tree, records the
Git revision and included modified paths in `BUILD-SOURCE.json`, then builds
in a fresh directory under `target/`. Untracked files are excluded. It signs
the source archive, package, and repository databases, embeds package
signatures with `repo-add`, and prepares only public files in
`target/arch-repository/`. It refuses to replace an existing package filename
with different contents. Keep the repository directory between releases so
previous package downloads remain available. Wrangler publishes this directory
as one deployment, so clients see matching database and package contents.

Inspect the generated `PKGBUILD` and `.SRCINFO` in
`target/arch-repository/packages/`; copy them back into `packaging/arch/` when
updating the checked-in recipe. The helper forwards extra arguments to
`makepkg`. On systems where Rustup was installed outside pacman, `--nodeps`
may be used after checking that all declared build and runtime tools exist.

The signing home defaults to `$XDG_DATA_HOME/quillc/gnupg`, or
`~/.local/share/quillc/gnupg`. The initial Ed25519 signing key expires on
2028-10-12 and is stored without a passphrase for unattended local signing.
Back up this private signing home and its revocation certificate securely;
only `quillc.asc` and the fingerprint belong in the repository or deployment.
`GNUPGHOME` and `GPGKEY` can override the signing home and fingerprint.

To remove the package hosting route, delete only the
`quill.simoncurtis.dev/packages/*` route in Cloudflare. The `quill-devlog`
custom domain continues to serve the existing site.
