Test-only CA made by the Rust coordinator, so tests prove the Phoenix app loads a CA from
`init_ca` (decision D2):

    kuiper-forge --data-dir test/fixtures/rust_init_ca ca init --org "Kuiper Test"

It signs nothing outside the test suite. Never use it for a real coordinator.
