# Test fixtures (xorgate-agent integration tests)

`throwaway-selfsigned.{crt,key}` — a self-signed RSA-2048 pair generated once
with `openssl req -x509 -newkey rsa:2048 -nodes -days 36500 -subj
"/CN=xorgate-rs-test-throwaway"`. It is NOT a device credential and is trusted
by nothing; it only exists so `tests/watchdog.rs` can get the worker past
`tls::client_config` (rustls insists the cert and key match) and into its
online select loop against an endpoint that can never connect.
