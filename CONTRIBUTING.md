# Contributing

Thanks for helping! The most useful contribution is **confirming another card works** and
**adding its subsystem ID** to the database.

## Add your card

1. Run `astral-watch` once. It prints a line like:

   ```
   # GPU 0000:0b:00.0  subsystem 1043:8a2e  -> unknown — not in card DB ...
   ```

2. If you then get a live per-pin reading (`p1 ~12.0V …`), it works on your card. Add it to
   [`src/cards.rs`](src/cards.rs):

   ```rust
   Card { subsystem: 0x8a2e, model: "ROG Astral RTX 5090 (your exact model)" },
   ```

3. Open a PR titled `cards: add <model> (1043:xxxx)`. Please include the `astral-watch` startup
   line and one decoded sample row in the description so we can verify the decode is correct.

If the chip answers but the numbers look wrong (not ~12 V, or implausible amps), open an issue
with the raw 24 bytes — the register layout may differ on that SKU.

## Dev

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs all three on every PR. No GPU is required — the decode/alert/logger logic is tested
against captured byte fixtures, and the i2c layer is only exercised at runtime.

## Scope & safety

Keep i2c access **read-only** (register-pointer reads only — never `i2cset`/write-byte). Any PR
that writes to the device will be rejected unless it's an explicit, documented, opt-in feature
(e.g. the planned NVML power-cap, which acts via the NVIDIA driver, not raw i2c writes).

## Releasing (maintainer checklist)

1. Bump `version` in `Cargo.toml`, update `CHANGELOG.md` (move changes under the new
   version with today's date, add the compare link), commit.
2. `git tag -a vX.Y.Z -m "vX.Y.Z"` and `git push origin main vX.Y.Z` — the release
   workflow builds gnu + musl tarballs and publishes them atomically once both succeed.
3. `cargo publish --locked` — CI's `publish-dry-run` job has already validated packaging.
4. Update the AUR package (see [`packaging/aur/README.md`](packaging/aur/README.md)).
