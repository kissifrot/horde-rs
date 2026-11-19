These DER certificates and intentionally public PKCS#8 keys belong to the
loopback transport tests. The fixture CA signs the localhost server and three
client identities. Two clients are authorized by the test endpoint and the
third is trusted by the CA but absent from the endpoint's authorizations.
