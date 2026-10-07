# Verifying a release

Every release publishes `SHA256SUMS` and `SHA256SUMS.sig`, an OpenSSH Ed25519 signature made with `ssh-keygen -Y sign`.

- **Namespace:** `kraskus-common-foundry-solo-release`
- **Signer identity:** `kraskus-common-foundry-solo-release`
- **Public key:** [`kraskus-solo-release.pub`](kraskus-solo-release.pub)

```bash
# 1. Allowed-signers entry for the committed public key
printf 'kraskus-common-foundry-solo-release namespaces="kraskus-common-foundry-solo-release" %s\n' \
  "$(cat KRASKUS/kraskus-solo-release.pub)" > allowed_signers

# 2. Check the signature on SHA256SUMS
ssh-keygen -Y verify -f allowed_signers -I kraskus-common-foundry-solo-release \
  -n kraskus-common-foundry-solo-release -s SHA256SUMS.sig < SHA256SUMS

# 3. Check the files against SHA256SUMS
sha256sum -c SHA256SUMS
```

## Key custody

- **Private key:** the Kraskus owner generates and holds it. Only the release workflow uses it, through the protected GitHub environment `release` (secret `KRASKUS_SOLO_RELEASE_SIGNING_KEY`). It is never committed, never printed and never stored on a developer machine.
- **Public key:** only the public key is committed here.
- **Key format:** an unencrypted OpenSSH Ed25519 private key (`ssh-keygen -t ed25519 -N ''`). It is protected by the environment's required reviewers and by GitHub's secret storage.
- **Fail-closed checks:** the release fails if the secret is absent, if the private key does not derive exactly the committed public key, if the two independent builds differ or fail their own `SHA256SUMS`, or if the packaged files do not match the verified build.
- **Mismatch guard:** after signing, the release job checks the signature against the committed public key. A secret that does not match the committed key fails the release.
- **Rotation:** replace `kraskus-solo-release.pub` in a reviewed commit, record it in [CHANGELOG.md](CHANGELOG.md), and keep the old key listed there with its validity period.
