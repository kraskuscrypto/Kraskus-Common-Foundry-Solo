# Kraskus Common Foundry Solo (Dev / experimental)

This repository is **not the official Common Foundry repository.** It contains the official source of [Common-Foundry-1/CommonFoundry](https://github.com/Common-Foundry-1/CommonFoundry) **v1.0.8** (MIT, commit `3aa5369`, full history) plus one small, separately reviewable Kraskus change. That change lets `cmfd-node run` serve the official pool protocol for **solo mining to the operator's own wallet**, while RPC, wallet and explorer keep running.

- What changed and why: [KRASKUS/README.md](../KRASKUS/README.md)
- The exact diff: [KRASKUS/PATCH.md](../KRASKUS/PATCH.md)
- Reproducible build: [KRASKUS/BUILD.md](../KRASKUS/BUILD.md)
- Verifying a signed release: [KRASKUS/VERIFY.md](../KRASKUS/VERIFY.md)

**No consensus, algorithm or protocol changes.** Not qualified for production use. If upstream ships an official solo mode ([#19](https://github.com/Common-Foundry-1/CommonFoundry/issues/19)), this repository is retired.

The upstream project README is unchanged at [README.md](../README.md).
