# Changelog

## 1.0.8+kraskus-solo.1 (unreleased, Dev/experimental)

- **Base:** upstream v1.0.8 (`3aa5369`), with full history.
- **New:** solo endpoint for `cmfd-node run`. It reuses the upstream pool server and protocol, with:
  - the coinbase paid to the operator's address;
  - no payout ledger, no PPLNS, no fee;
  - a share target equal to the network target;
  - the official ProductionV4 replay and proof workers;
  - no listener until the workers are ready;
  - a status file.
- **Tests:**
  - the configuration has no payout accounting;
  - the share target resolves to the chain target;
  - the status file is written atomically;
  - end to end on DevNet: a block is mined through the solo endpoint by the upstream pool client, accepted, and paid to the node wallet, and no ledger is written.
- **Build:** reproducible build in a pinned `rust:1.94.1-slim-bookworm` image, with the upstream mainnet launch-identity assertions; CI compares two independent builds.
- **Release signing:** Ed25519 (`ssh-keygen -Y sign`) through a protected environment secret. The public key is pending from the owner.

### Signing keys

| Key | Valid from | Valid until |
|---|---|---|
| *(pending owner key)* | | |
