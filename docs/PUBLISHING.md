# Publishing a public Vale repository

Vale's canonical public repository is
[`Dankpaws/vale`](https://github.com/Dankpaws/vale). Its public history begins
with a reviewed clean export. The separate private maintenance repository has
older deployment history, including retired hostnames and network topology,
even when its current working tree has been sanitized. Treat every commit,
branch, tag, reflog, and alternate object from that private history as private.

Never push the private maintenance history or all of its refs wholesale. Create
public changes from a reviewed clean tree and a deliberately public history. A
clean current working tree in the private repository is not proof that its old
objects are safe to publish.

The reviewed first binary release is `v0.36.1`, dated 2026-08-30. The signed,
verified `v0.36.0` source tag remains attached to the parentless public root,
but its tagged workflow failed before publishing a GitHub Release or any
assets. Never move, delete, or reuse that failed source tag. Keep GitHub private vulnerability reporting enabled so `SECURITY.md` has a
working non-public channel.

The initial public commit and every public follow-up must use the verified
maintainer identity `Dankpaws <322286514+Dankpaws@users.noreply.github.com>`.
Each binary release must carry an annotated `v<package-version>` tag after every
check below passes. An unsigned tag may exist only in an isolated local review
candidate. Before public announcement, create the public tag with the
maintainer's configured SSH/GPG signing identity or a GitHub tag/release flow
that reports a verified signature, then verify that status independently.
Never publish the unsigned review tag while describing the release reference
as signed. Before creating or updating the public repository:

1. Confirm every copy-paste URL still names `Dankpaws/vale`, the release tag is
   `v0.36.1` for the first binary release, and the dated changelog entry
   matches it.
   Later releases must use the `v<package-version>` tag shape enforced by the
   release workflow.
2. Export only the reviewed source tree into a fresh directory without its
   private `.git` directory, local metadata, build output, credentials, or
   deployment state.
3. Review the complete tree for private hostnames, addresses, topology,
   account names, tokens, passwords, private keys, certificate paths, backup
   paths, and operational logs. Keep examples synthetic and use an explicit
   placeholder when a repository URL is not known yet.
4. Preserve `LICENSE`, `CREDITS`, `THIRD_PARTY.md`,
   `THIRD_PARTY_LICENSES.html`, both bundled-asset license files, and the
   applicable Redlib AGPL-3.0 source and notice obligations.
5. For an initial publication, initialize a new repository from that tree. For
   later releases, apply the reviewed tree to a clone of the public repository.
   Inspect every new commit and published ref; verify that no private remote,
   history, tag, or generated artifact was copied.
6. Run the approved secret scanner against the new repository's complete
   clean history. For gitleaks, the minimum release check is equivalent to:

   ```sh
   gitleaks detect --source . --redact --no-banner
   ```

   Resolve or explicitly review every finding; do not treat `--redact` as
   proof that a finding is safe to publish.
7. Clone the resulting public repository into a separate empty directory and
   repeat the private-literal, link, license, and smoke checks from
   `CONTRIBUTING.md` and `docs/INSTALL.md` before announcing a release.
8. Resolve every GitHub Action to its reviewed full commit SHA, verify the
   version comment matches that commit, and let the checked-in Dependabot
   policy propose future updates for review.

For each public tag, require the tagged-source quality gate to pass before
publishing. The release must include deterministic architecture archives,
individual checksums, `SHA256SUMS`, a complete tagged source archive, the
binary archive's `SOURCE_OFFER.txt`, and GitHub/Sigstore provenance for every
tarball. Verify one downloaded binary archive with both its checksum and
`gh attestation verify --repo Dankpaws/vale` before announcing it.

The checked-in workflow does not publish a registry image. If that policy ever
changes, publish only an immutable tag/digest built from the exact public tag,
attest it, and verify that its OCI source label and bundled `SOURCE_OFFER.txt`
identify the exact corresponding source before announcement.

The Dockerfile pins its Debian and Rust base-image digests, but Debian packages
installed from the distribution repositories and the optional host FFmpeg
package are resolved at build/install time. Record the resolved package/SBOM
evidence and rely on the release's provenance for those inputs; do not claim
that the unpublished Docker image or host package set is byte-for-byte
reproducible. Freezing a distro snapshot is a separate reviewed release-policy
change, not something the installer should improvise on an end user's machine.

After publication, keep private deployment operations in a private project.
Public issues, screenshots, logs, and support bundles must follow the same
redaction rules. If a private object is published accidentally, treat it as a
security incident: remove the public ref, rotate affected credentials, and
follow the repository's vulnerability-response process.
