# TLS test fixture

`cert.pem` and `key-pkcs8.pem` are a throwaway self-signed certificate for `127.0.0.1`, issued with
`O=localplay test fixture`. They exist so `crates/events/tests/lol_mock.rs` can stand up a TLS mock
server without a real certificate.

They are safe to have in a public repository, and this note exists because a file named
`key-pkcs8.pem` containing `BEGIN PRIVATE KEY` reasonably attracts attention from anyone scanning a
repository: the key is not used by anything but that test, is trusted by nothing, was never used to
protect anything, and expires 2027-09-23. `key-pkcs8.pem` is mode 600 because a private key should
be, not because it guards a secret.
