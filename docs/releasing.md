# Releasing srcmv

srcmv publishes native archives as GitHub Release assets. A personal
Homebrew tap can use those immutable, versioned URLs and the matching values in
`SHA256SUMS` without maintaining another binary host.

The release archives do not broaden the qualification contract. srcmv is
qualified only for Linux x86_64 on ext4 and macOS arm64 on APFS. A binary may
download or start on another filesystem, but that is not a supported row.

## One-time repository setup

1. Merge `.github/workflows/release.yml` into the default branch before creating
   or pushing a version tag. The tagged commit must contain the workflow.
2. Keep the workflow's default `contents: read` permission. Its publish job is
   the only job granted `contents: write`.
3. Protect tags matching `v*` with a GitHub ruleset. If the repository offers
   immutable releases, enable that setting as defense in depth after testing the
   first release.
4. Do not move or reuse a published version tag. Correct a bad release with a
   new patch version.

The existing `v0.1.0` tag predates this workflow. Do not move that tag merely to
trigger automation. If it has not been published, either upload its previously
qualified Phase 10 archives manually or publish the next patch release after
the workflow is present in that release's tagged commit.

## Release procedure

1. Update the workspace version in `Cargo.toml`, regenerate `Cargo.lock` if it
   changes, and update version-specific release notes and contracts.
2. Run the normal qualification and release acceptance checks.
   Semantic-selection changes additionally require the bounded fake-server test
   suite on both supported OS rows and the best-effort real-server smoke test on
   a development host:

   ```console
   scripts/qualify-lsp.sh
   ```

   The script exercises installed `clangd` and `rust-analyzer`. If neither is
   installed it prints a notice and exits successfully; fake-server CI remains
   authoritative for transport, lifecycle, ranges, limits, and failures. Record
   the server names and versions used for any real-server qualification in the
   release evidence. A successful smoke test demonstrates compatibility with
   those installed versions only; it does not bundle or certify every language
   server version.
3. Commit and merge the release candidate to the default branch.
4. Tagging is automatic. The `Release train` workflow (`.github/workflows/release-train.yml`)
   runs on every push to `main`; when it sees a workspace version that does not
   yet have a `vMAJOR.MINOR.PATCH` tag, it verifies that version-specific
   release notes exist, waits for a successful CI run on the exact HEAD,
   creates the annotated tag from those notes, pushes it, dispatches the
   `Release` workflow, and watches it to completion. A merge without a version
   bump is a no-op, so ordinary feature merges never release anything. If it
   fails before tagging (red CI, missing notes), nothing was published: fix the
   cause and re-run the workflow with
   `gh workflow run release-train.yml --ref main`. If it fails after tagging,
   the tag is immutable; recover with `gh workflow run Release --ref main -f tag=vX.Y.Z`.
5. Alternatively, from a clean, synchronized `main`, use the guarded release helper locally. It
   re-runs the local release acceptance checks, requires a successful CI push
   run for the exact commit, creates an annotated tag from the version-specific
   notes, pushes it, waits for the Release workflow, verifies the published
   asset set, and waits for the post-publication Homebrew notification:

   ```console
   scripts/release.sh publish 0.2.0
   ```

   A release candidate can run the same checks without creating a tag:

   ```console
   scripts/release.sh verify 0.2.0
   ```

   The local helper enforces the same gates as the automated train and is kept
   for recovery and for releases made before the train existed. The publish
   command is intentionally not a version-bump tool. Version and
   release-note changes remain ordinary reviewed source changes, and publishing
   happens only after they are merged.

Only stable tags of the form `vMAJOR.MINOR.PATCH` pass the workflow gate. The
workflow also requires the tag to be annotated and resolves it back to the exact
GitHub event commit before building and immediately before publishing.

For each tag, GitHub-hosted native runners:

- test the locked workspace;
- execute every checked-in user-facing example;
- confirm the OS, CPU, Rust host target, and qualified filesystem;
- build with the repository's pinned Rust toolchain and `Cargo.lock`;
- verify the executable format and reported version; and
- package only `x86_64-unknown-linux-gnu` and `aarch64-apple-darwin`.

When semantic selection is included, release review also confirms that
`srcmv select --help`, the selection-v1 schemas and golden vectors, the
documented built-in descriptor table, and configuration defaults agree with the
tagged source. Selection protocol v1 remains independent of the frozen edit
protocol: do not alter `capabilities --json`, the protocol-v1 request/response
schemas, plan-hash encoding, or edit error/warning registries to advertise it.

The publish job waits for both builders, downloads their short-lived workflow
artifacts, creates a stable `SHA256SUMS`, and uploads all three files to a draft
GitHub Release. It verifies the exact asset set and tag again before publishing
the draft. A failed upload therefore does not expose a partial public release.
A rerun replaces an abandoned draft but refuses to replace an already published
release.

After the stable release is public, the final job sends a
`srcmv_release_published` repository-dispatch event to
`atacan/homebrew-tap`. The tap independently verifies the release and opens a
formula update PR. This notification is deliberately downstream of publication:
if it fails, the GitHub Release remains public and its tag remains immutable.

### One-time Homebrew automation setup

1. Create a fine-grained personal access token owned by `atacan`. Give it
   access to **only** the `atacan/homebrew-tap` repository and grant the
   repository permission **Contents: Read and write**. No Actions,
   pull-request, administration, or organization permissions are required for
   the repository-dispatch endpoint. Choose an expiration and rotate the token
   before it expires.
2. In `atacan/srcmv`, create the Actions repository secret
   `HOMEBREW_TAP_DISPATCH_TOKEN` with that token as its value. Do not add the
   token to repository variables, environments, source files, or the tap.
3. In `atacan/homebrew-tap`, open **Settings > Actions > General > Workflow
   permissions**. Keep the default token permission at read-only, and enable
   **Allow GitHub Actions to create and approve pull requests**. The tap
   workflow explicitly grants its job `contents: write` and
   `pull-requests: write`; it creates PRs but never approves or merges them.

No custom Actions variables are required. Both repositories receive their own
automatic, repository-scoped `GITHUB_TOKEN`; the srcmv token cannot notify
another repository and is not used for that purpose.

## Homebrew consumption

GitHub Release assets are the right first distribution source for the personal
tap. They provide stable versioned URLs, checksums, access logs, and one place to
retain release notes. Each tap update should copy the target archive digest from
the release's `SHA256SUMS`; never point a formula at a branch archive or `latest`
URL.

For example, the tap can choose the native URL and digest inside `on_macos` and
`on_linux` blocks and install `srcmv` from the archive's `bin.install`
equivalent. Keep the same two-row OS/architecture restrictions in the formula.

The more Homebrew-native refinement is to build bottles from a source formula
and publish the bottle metadata and archives from the tap's own workflow. That
adds reproducible formula testing and `brew`-managed bottle selection, but also
adds a second release pipeline. Start with the upstream GitHub Release assets;
adopt bottles once the formula and release cadence are stable.

## Failure recovery

- If either native build fails, no GitHub Release is created.
- If release creation or verification fails, the release remains a private
  draft. After correcting the workflow on `main`, recover an existing immutable
  tag with `gh workflow run Release --ref main -f tag=v0.1.1`. The manual run
  verifies the remote annotation and builds the exact tagged commit; it does not
  move or recreate the tag. It replaces only an abandoned draft.
- If the release is already public, the workflow stops. Do not delete or mutate
  it to make a rerun pass.
- If the stable release is public but the Homebrew notification fails, the
  release was not rolled back. Rerun only the failed notification job, or invoke
  the tap's recovery path directly with
  `gh workflow run update-srcmv.yml --repo atacan/homebrew-tap --ref main -f version=0.2.1`.
- If the tag or packaged content is wrong, increment the version, qualify a new
  commit, and publish a new tag.
