# Dependency license review

This review records the dependencies locked on **2026-10-20**, during beta
preparation. It supports preparing a distribution; it does not certify an
unbuilt release archive or change the licenses of dependency code.

See [Third-party notices](../THIRD_PARTY_NOTICES.md) for the package list and
retained texts, and the [inventory](../third-party/inventory.json) for exact
versions, provenance, source paths and checksums.

## Scope and verification

| Dependency graph | Reviewed packages | Scope |
| --- | ---: | --- |
| Rust `Cargo.lock` | 70 | All third-party entries, including build, test and target-specific dependencies; excludes the seven FailForge workspace packages. |
| Dart adapter `pubspec.lock` | 49 | 18 runtime dependencies and 31 development-only dependencies. |

Every one of the **119 package archives** was checked against its lockfile's
SHA-256. The retained package license files, notice headers and provenance
excerpts were compared with those verified archives. Package metadata was
checked against the locked package identities; Dart runtime/development scope
was checked against the resolved dependency graph.

The inventory retains 135 Rust license files, 104 Rust source notice fragments,
two Rust provenance excerpts, 50 Dart license files and 180 Dart source notice
fragments. Three additional upstream license texts have separately recorded
sources and hashes. Identical texts share a file, producing **115 exact-text
files**. A source path or notice mapping is not another package.

The Rust inventory conservatively includes all target-specific packages. It
does not assert that all 70 dependencies are linked into every executable. The
Dart classification describes the adapter's graph, not the target project's
dependencies.

The reviewed package set had no missing license texts or archive checksum
mismatches. No dependency replacement was identified as necessary by this
review. The distribution must still preserve the applicable notices and be
checked against its actual contents.

## Reading license information

Cargo metadata can contain an SPDX expression or a `license-file`. In an SPDX
expression, `OR` offers alternatives, `AND` combines requirements, and `WITH`
identifies an exception. The retained texts do not silently select an
alternative. See the [Cargo license metadata reference](https://doc.rust-lang.org/cargo/reference/manifest.html#the-license-and-license-file-fields).

For example, `unicode-ident` declares
`(MIT OR Apache-2.0) AND Unicode-3.0`: choosing MIT does not remove the Unicode
requirement. `version_check` retains its original legacy `MIT/Apache-2.0`
notation and both texts. `wasi` retains its alternatives and exception text
even though it is platform-specific.

MIT requires preserving its copyright and permission notice; BSD-3-Clause
includes notice, disclaimer and non-endorsement conditions. Apache-2.0 includes
license and applicable attribution requirements. These descriptions do not
replace the complete texts:
[MIT](https://spdx.org/licenses/MIT.html),
[BSD-3-Clause](https://spdx.org/licenses/BSD-3-Clause.html),
[Apache-2.0](https://www.apache.org/licenses/LICENSE-2.0.html).

## Additional notices

Package-level license metadata does not always describe every embedded file.
The inventory therefore retains the following additional material.

| Package/component | Review result |
| --- | --- |
| `unicode-ident`, `regex-syntax` | Unicode data notices are retained, including the additional Unicode-3.0 condition on `unicode-ident`. |
| `tracing-core` | Embedded spin-lock code includes a separate MIT copyright attribution. |
| `tracing-subscriber` | The datetime implementation includes a musl-derived MIT notice. |
| `sha2-asm` | Assembly copyright/license comments and the Apple public-domain provenance notice are retained. |
| `libyaml-safer` | Its manifest uses `license-file`, and its packaged license and README identify MIT. The exact published text is retained without inventing a missing copyright line. |
| Dart `node_preamble` | Its combined license text includes both MIT and BSD-3-Clause material; it is not treated as a choice between them. |
| Dart `mime` | The BSD package also contains `third_party/httpd/LICENSE` and a public-domain header on `mime.types`. The HTTPD file includes historical bundled notices; retaining it does not imply that every component named in it is shipped by FailForge. |
| Dart `web` — development-only | 177 generated-source documentation headers credit MDN/Mozilla and identify CC-BY-SA-2.5. These documentation terms are separate from the package's BSD code license. |
| Dart `vm_service` — development-only | Generated documentation contains Highlight.js 11.11.1 assets. Asset-specific attributions and the full upstream BSD license are retained. |

The `libyaml-safer` README links an exact original libyaml revision,
`2c891fc7a770e8ba2fec34fc6b545c672beb37e6`. Its
[upstream license](https://github.com/yaml/libyaml/blob/2c891fc7a770e8ba2fec34fc6b545c672beb37e6/License)
supplies the published Ingy döt Net and Kirill Simonov attributions. A second
supplement preserves `unsafe-libyaml` 0.2.11's license, from the version in
`libyaml-safer`'s own development lockfile. The README establishes ancestry but
does not pin that fork to this version; this supplement is **not evidence of an
exact fork ancestor**. Neither supplement is counted as a direct dependency in
FailForge's lockfile.

The Highlight.js supplement comes from the
[official 11.11.1 license](https://github.com/highlightjs/highlight.js/blob/11.11.1/LICENSE),
matching the version identified by the embedded asset. Its full license does
not replace the additional asset-specific attribution comments.

## Distribution boundaries

Keep these distinctions when preparing a release:

- Include `LICENSE`, `NOTICE`, `THIRD_PARTY_NOTICES.md`, this review document and
  the complete `third-party/` directory with the distribution.
- Review the files actually bundled. Package caches and generated dependency
  documentation can introduce additional licensed material, including the
  documentation/assets noted above. Their inclusion must be deliberate.
- This snapshot covers Cargo and the adapter's Pub lockfile. Compiler-provided
  Rust runtime libraries, system libraries and other toolchain components need
  review for the particular binary being distributed.
- Dart and Flutter SDKs are separate prerequisites in the current installation
  instructions; this review does not authorize bundling them without their
  own license/notice review.
- The separate `examples/flutter_fixture` dependency graph and dependencies of
  projects tested by users are outside this inventory. Reassess the scope if
  their dependency contents are added to a distribution.
- Original FailForge files remain Apache-2.0. Preserving a dependency notice does
  not relicense that dependency under Apache-2.0.

## Checking and refreshing the snapshot

From the repository root, verify the retained files with:

```sh
shasum -a 256 -c third-party/SHA256SUMS
```

This checks the package inventory, notice index and retained texts. It does not
fetch packages, check a release binary, or prove that the dependency lockfiles
still match the snapshot. The inventory's `inputs` entries record the hashes of
`Cargo.lock`, `adapters/dart/pubspec.yaml` and `adapters/dart/pubspec.lock`.

After changing dependencies or distribution contents:

1. Resolve and lock the new graph; compare all locked package identities and
   checksums with the inventory, including target-specific and development
   dependencies.
2. Verify package archive hashes. Inspect root/nested licenses, notice files,
   embedded source attributions and generated documentation/assets.
3. Preserve complete license texts and exact notice excerpts. Record original
   paths, package versions, archive hashes and separately sourced upstream
   provenance; distinguish license alternatives from combined obligations.
4. Refresh the package table, inventory, texts and checksums together. Check
   that every mapping resolves and every retained file has a mapping.
5. Review the final archive's runtime/toolchain components and included files,
   then verify that all required notices accompany that archive.

The current `ci.yml` does not automate this license review. A green build alone
does not establish that this snapshot is current or that a release's licensing
requirements have been met.
