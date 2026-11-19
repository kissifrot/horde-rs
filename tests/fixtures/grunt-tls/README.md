These DER certificates and intentionally public PKCS#8 keys belong to the
loopback transport tests. The fixture CA signs the localhost server and three
client identities. Two clients are authorized by the test endpoint and the
third is trusted by the CA but absent from the endpoint's authorizations.

The `identity-*` files exercise Grunt server authentication. The test root
`identity-ca.der` signs the DNS, IP and expired server certificates. The
foreign server has the expected DNS name and is signed by a separate CA
that the controller does not trust. The private keys are test data.
