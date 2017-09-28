# pairing [![Crates.io](https://img.shields.io/crates/v/pairing.svg)](https://crates.io/crates/pairing) #

This is a Rust crate for using pairing-friendly elliptic curves. Currently, only the [BLS12-381](https://z.cash/blog/new-snark-curve.html) construction is implemented.

## [Documentation](https://docs.rs/pairing/)

Bring the `pairing` crate into your project just as you normally would.

If you're using a supported platform and the nightly Rust compiler, you can enable the `u128-support` feature for faster arithmetic.

```toml
[dependencies.pairing]
version = "0.12"
features = ["u128-support"]
```

## Security Warnings

This library does not make any guarantees about constant-time operations, memory access patterns, or resistance to side-channel attacks.

## License

Licensed under either of

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
