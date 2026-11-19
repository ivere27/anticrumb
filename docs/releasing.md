# Releasing

## GitHub Actions Releases (Recommended)

[Build, test, and release](../.github/workflows/build.yml) builds, tests, and packages Linux (`x86_64`, `aarch64` on Ubuntu 22.04 for glibc 2.35+ compatibility, plus native `ubuntu-24.04-arm` verification), Windows (`x86_64` MSVC), and macOS (`aarch64`, `x86_64`):

- **Pull requests & non-master branches**: run checks, tests, and package validation only.
- **`master` push**: updates the rolling `nightly` pre-release (`tag: nightly`, `prerelease: true`), replacing platform archives with the latest verified build without changing `latest`.
- **Version tag push (`vX.Y.Z`)**: set `[package].version` in `Cargo.toml` to `X.Y.Z`, commit to `master`, then push only that tag:

```sh
git tag v1.0.0 && git push origin v1.0.0
```

Push the specific tag rather than `git push --tags` so local copies of the moving `nightly` tag never conflict or get pushed.

The tag workflow verifies that `vX.Y.Z` matches `Cargo.toml`, validates all platform archives, creates a draft release, verifies uploaded sizes and GitHub SHA-256 digests, and publishes it as the latest stable release.

Users can update to stable releases with `anticrumb update`, or switch to/update the rolling `master` pre-release with `anticrumb update --nightly` (add `--check` to inspect or `--force` to reinstall).

## Manual Docker Release

Build fresh [packages](building.md#development-and-packaging) from the commit being released, inspect the dry run, then publish:

```sh
make package
make publish DRY_RUN=1
make publish
```

Without Make, use `cargo run --locked -q -p xtask -- publish`; add `--dry-run` to preview. A dry run validates archives and prints commands offline without changing files or GitHub.

## Requirements

- Set a stable version in `Cargo.toml` newer than previous stable releases.
- Commit changes and push `master`. Publication requires a clean checkout matching GitHub's `master`. An existing `vVERSION` tag must point to that commit; an existing release is rejected.
- Keep one archive per target under `dist/`, including Docker subdirectories. Remove old copies: filenames do not encode a version/commit, so the publisher cannot verify build provenance.
- Authenticate [GitHub CLI](https://cli.github.com/) with repository write access and permission to manage Pages. Fine-grained tokens need **Pages: write** and **Administration: write**.

## Packages and Publication

Unix archives use `anticrumb-TARGET.tar.gz`; Windows uses `.zip`. Each contains an `anticrumb-TARGET/` root with the executable and `LICENSE`, optionally `README.md` and `docs/`. Preserve Unix executable permissions. Supported targets and validation rules are in [archive.rs](../xtask/src/archive.rs) and [publish.rs](../xtask/src/publish.rs).

The publisher configures GitHub Pages from **master / (root)**, replacing any existing source configuration. `README.md`, `install.sh`, and `install.ps1` are served from the repository root.

It snapshots validated archives, uploads a draft, and checks uploaded names, sizes, and GitHub SHA-256 digests before publishing as the latest stable release. Missing or mismatched digests stop publication. The release contains only platform archives; installers and updates use GitHub's release API and asset digests, with no separate metadata files.

Make the repository public before public installation. [pages.yml](../.github/workflows/pages.yml) runs offline checks; it does not deploy a separate site.

## Failure Recovery

Inspect GitHub before retrying: a failed request may have reached the final publication step. Failed uploads or verification normally leave an unpublished draft. Existing drafts/releases are never overwritten; repair or remove the draft yourself, or use a new version. The script does not push branches or delete tags/releases.

Verification: [publisher contracts](../xtask/tests/publish_contract.rs), included in [automated checks](testing.md#automated-checks).
