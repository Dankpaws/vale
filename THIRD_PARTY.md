# Third-party and upstream notices

Vale is a product fork of Redlib. The upstream Redlib source, its AGPL-3.0
license, and contributor credits remain in this checkout:

- [LICENSE](LICENSE) — repository license and upstream license text;
- [CREDITS](CREDITS) — Redlib contributors and upstream attribution; and
- [`static/hls.min.js`](static/hls.min.js) — the bundled HLS.js build, which
  retains its source comment and [Apache-2.0 notice](static/hls.LICENSE.txt);
- the bundled Source Sans 3 and Source Serif 4 fonts, distributed under the
  [SIL Open Font License 1.1](static/fonts/OFL.txt); and
- the locked Rust dependency graph and selected license texts in
  [THIRD_PARTY_LICENSES.html](THIRD_PARTY_LICENSES.html).

## Rust HTML processing dependencies

Vale directly uses `ammonia` 4.1.4 (MIT OR Apache-2.0) for sidebar HTML
sanitization and `lol_html` 3.0.1 (BSD-3-Clause) for archived-comment heading
rewriting. Their required transitive graph adds dependencies selected under
MIT, MIT OR Apache-2.0, and MPL-2.0. The generated
[`THIRD_PARTY_LICENSES.html`](THIRD_PARTY_LICENSES.html) inventory records each
locked direct and transitive package, its version, its selected license, and
the corresponding license text.

## Vale artwork

The Vale mark in `static/vale-mark.svg`, its app-icon and logo raster
derivatives (`logo.png`, `apple-touch-icon.png`, and `favicon.ico`), and the
four `static/scenes/vale-*` background files are Vale-specific project artwork
created for this fork. The Vale maintainers distribute those assets as part of
this repository under AGPL-3.0-only. They are not omitted third-party stock
assets; retained Redlib lineage remains covered by `CREDITS` and `LICENSE`.

Binary and container redistributions must preserve this file, `LICENSE`,
`CREDITS`, `THIRD_PARTY_LICENSES.html`, both bundled-asset license files, and
the applicable source or offer of source required by the AGPL-3.0 license.
Vale's tagged binary archives include `SOURCE_OFFER.txt`, and the same release
publishes the complete corresponding tagged source archive. A locally built
container includes `/usr/share/doc/vale/SOURCE_OFFER.txt` and an OCI source
label naming the canonical repository. Vale does not publish a registry image
in the current release workflow; anyone who redistributes one remains
responsible for making its exact corresponding source available.

After a Rust dependency change, install cargo-about 0.9.2 with its `cli`
feature and run `scripts/update_rust_licenses.sh`. After changing HLS.js, use
the version-and-checksum-pinned `scripts/update_hls_js.sh`. Review copyleft and
notice obligations before release; do not replace an upstream notice with a
Vale-only attribution.
