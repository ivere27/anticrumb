# Releasing

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
