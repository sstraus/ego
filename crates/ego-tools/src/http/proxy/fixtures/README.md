# Controlled proxy TLS certificates

Generated locally with OpenSSL for these tests on 2026-10-26. The CA signs a server certificate with SAN `provider.test`, CA:FALSE and serverAuth. `server-key.der` is a public test-only PKCS#8 key. These certificates have no production authority. The test uses a real rustls server over a local socket, not a provider fixture.

The leaf has a one-year validity ending 2027-10-26. Longer validity fails macOS native trust (`OtherTrustValidityPeriod`), even with an explicit private root. Regenerate the CA, leaf, and test key together before expiry; retain SAN `provider.test`, CA:FALSE, digitalSignature/keyEncipherment, and serverAuth. The initial chain was verified with `security verify-cert -c server.der -r ca.pem -p ssl -n provider.test -L -v`.
