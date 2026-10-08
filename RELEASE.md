# Release v0.6.0

Release is driven by this repository's tag-triggered `.github/workflows/release.yml`, which
calls `dekopon-agents/provider-workflows/.github/workflows/release.yml@main`. Do not manually
upload assets or publish an image. Before tagging, confirm that the remote tag, GitHub release
and `ghcr.io/dekopon-agents/provider-openobserve:0.6.0` do not exist; any collision stops.
Merge the reviewed, green release-prep PR into `main` first, then create and push one annotated
`v0.6.0` tag pointing at the merged main commit. The shared workflow requires an annotated tag,
a matching package version and main ancestry. Wait for its release run to finish successfully.

The effective workflow builds and publishes **three** release assets:
`openobserve-provider.wasm`, `openobserve-provider.wasm.sha256`, and
`openobserve-provider.cdx.json` (CycloneDX SBOM). It verifies draft assets byte-for-byte,
attests the wasm and SBOM, publishes the OCI artifact with one `application/wasm` layer, then
publishes the release with `make_latest=true` for this non-prerelease tag. A tag or CI build
alone is not proof of a published release or image.

After the tag run, download all three assets into a scratch directory outside the repository.
Check the three exact asset names, validate the SBOM as CycloneDX, and verify the sidecar against
the downloaded wasm (`shasum -a 256 -c openobserve-provider.wasm.sha256` on macOS). Verify the
wasm's GitHub provenance with the merged-main SHA as `<main-merge-SHA>`:

```sh
gh attestation verify openobserve-provider.wasm \
  -R dekopon-agents/dekopon-provider-openobserve --format json \
  --signer-repo dekopon-agents/provider-workflows \
  --source-ref refs/tags/v0.6.0 --source-digest <main-merge-SHA>
```

Inspect the verified subject digest, tag and source commit. If required, constrain the signer
workflow to `dekopon-agents/provider-workflows/.github/workflows/release.yml`; never relax the
source or digest constraints. Verify the SBOM attestation too. Compare the wasm's SHA256 with
`crane manifest ghcr.io/dekopon-agents/provider-openobserve:0.6.0`: there must be exactly one
layer, with media type `application/wasm` and digest `sha256:<verified-wasm-SHA256>`. Record
`crane digest ghcr.io/dekopon-agents/provider-openobserve:0.6.0` as the **distinct OCI manifest
digest** to pin in the provider set, not the layer digest or a floating tag. Save the successful
release run URL and proofs. Missing assets, attestations, image job, unequal bytes or unexpected
state stop the release; never retag, rebuild, substitute or publish manually.
