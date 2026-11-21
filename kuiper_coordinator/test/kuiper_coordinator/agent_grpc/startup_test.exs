defmodule KuiperCoordinator.AgentGrpc.StartupTest do
  # A4: bad cert files stop boot with a clear error instead of silently failing handshakes
  use KuiperCoordinator.AgentGrpcCase
  @moduletag :capture_log
  @moduletag :no_server

  alias KuiperCoordinator.AgentGrpc.TLS

  test "starts with the Rust-made CA and a matching server pair", ctx do
    assert %{ca: _, chain: [_]} = TLS.check!(ctx.settings)

    assert %KuiperCoordinator.CA{} =
             KuiperCoordinator.CA.install!(ctx.certs.ca_cert, ctx.certs.ca_key)
  end

  test "refuses a server key that doesn't match the server cert", ctx do
    other = Path.join(ctx.dir, "other.key")
    File.write!(other, X509.PrivateKey.new_ec(:secp256r1) |> X509.PrivateKey.to_pem())

    assert_raise ArgumentError, ~r/does not match server certificate/, fn ->
      TLS.check!(Keyword.put(ctx.settings, :server_key, other))
    end
  end

  test "refuses a server cert that doesn't chain to the CA", ctx do
    key = X509.PrivateKey.new_ec(:secp256r1)
    cert = X509.Certificate.self_signed(key, "/CN=localhost")

    {cert_file, key_file} =
      write_pair(
        ctx.dir,
        "selfsigned",
        X509.Certificate.to_pem(cert),
        X509.PrivateKey.to_pem(key)
      )

    assert_raise ArgumentError, ~r/does not verify against CA/, fn ->
      TLS.check!(Keyword.merge(ctx.settings, server_cert: cert_file, server_key: key_file))
    end
  end

  test "refuses a missing file", ctx do
    assert_raise ArgumentError, ~r/cannot read server certificate/, fn ->
      TLS.check!(Keyword.put(ctx.settings, :server_cert, Path.join(ctx.dir, "nope.crt")))
    end
  end

  test "refuses a CA key that doesn't match the CA cert", ctx do
    other = Path.join(ctx.dir, "other-ca.key")
    File.write!(other, X509.PrivateKey.new_ec(:secp256r1) |> X509.PrivateKey.to_pem())

    assert_raise ArgumentError, ~r/CA key .* does not match/, fn ->
      KuiperCoordinator.CA.load!(ctx.certs.ca_cert, other)
    end
  end

  test "the supervisor fails to start with bad files", ctx do
    Process.flag(:trap_exit, true)
    bad = Keyword.put(ctx.settings, :server_cert, Path.join(ctx.dir, "nope.crt"))
    assert {:error, _} = KuiperCoordinator.AgentGrpc.Supervisor.start_link(bad)
  end

  test "refuses an unknown server trust mode", ctx do
    assert_raise ArgumentError, ~r/server trust mode "chian" is not one of ca, chain/, fn ->
      TLS.check!(Keyword.put(ctx.settings, :server_trust_mode, "chian"))
    end
  end

  test "refuses a server cert that isn't valid for server_hostname", ctx do
    assert %{} = TLS.check!(ctx.settings)

    # an IP host matches the cert's iPAddress SAN
    assert %{} = TLS.check!(Keyword.put(ctx.settings, :server_hostname, "127.0.0.1"))

    assert_raise ArgumentError, ~r/not valid for hostname 10.0.0.9/, fn ->
      TLS.check!(Keyword.put(ctx.settings, :server_hostname, "10.0.0.9"))
    end

    assert_raise ArgumentError, ~r/not valid for hostname coordinator.example.com/, fn ->
      TLS.check!(Keyword.put(ctx.settings, :server_hostname, "coordinator.example.com"))
    end
  end
end
