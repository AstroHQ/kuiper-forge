defmodule KuiperCoordinator.AgentGrpc.BootstrapTest do
  # D57: a fresh install makes its own CA and server cert on first launch; existing files are
  # never touched, and half a pair stops boot naming the file.
  use ExUnit.Case, async: true
  import Bitwise
  @moduletag :capture_log

  alias KuiperCoordinator.AgentGrpc.{Bootstrap, TLS}
  alias KuiperCoordinator.CA

  setup do
    root = Path.join(System.tmp_dir!(), "kc-boot-#{System.unique_integer([:positive])}")
    on_exit(fn -> File.rm_rf!(root) end)
    dir = Path.join(root, "data")

    settings = [
      data_dir: dir,
      ca_cert: Path.join(dir, "ca.crt"),
      ca_key: Path.join(dir, "ca.key"),
      server_cert: Path.join(dir, "server.crt"),
      server_key: Path.join(dir, "server.key"),
      server_trust_mode: "ca",
      server_hostname: "ci.example.com",
      ca_org: "Acme Builds"
    ]

    %{dir: dir, settings: settings}
  end

  defp mode(path), do: File.stat!(path).mode &&& 0o777

  test "an empty data dir gets a CA and a server cert, keys 0600, dir 0700", %{dir: dir} = ctx do
    assert :ok = Bootstrap.ensure!(ctx.settings)

    assert mode(dir) == 0o700
    assert mode(ctx.settings[:ca_key]) == 0o600
    assert mode(ctx.settings[:server_key]) == 0o600

    ca = CA.load!(ctx.settings[:ca_cert], ctx.settings[:ca_key])
    assert X509.Certificate.subject(ca.cert, "CN") == ["Acme Builds CA"]
    assert X509.Certificate.subject(ca.cert, "O") == ["Acme Builds"]

    assert {:Extension, _, true, {:BasicConstraints, true, :asn1_NOVALUE}} =
             X509.Certificate.extension(ca.cert, :basic_constraints)

    # the startup check accepts it, for the configured hostname and localhost / 127.0.0.1
    assert %{chain: [server]} = TLS.check!(ctx.settings)
    assert :public_key.pkix_verify_hostname(server, dns_id: ~c"localhost")
    assert :public_key.pkix_verify_hostname(server, ip: {127, 0, 0, 1})
    TLS.check!(Keyword.put(ctx.settings, :server_hostname, "127.0.0.1"))

    # an agent cert issued from the new CA verifies against it
    %{cert_pem: pem} = CA.issue_agent_cert("agent_tart_mac_abcdefgh", ca)
    {:ok, agent} = X509.Certificate.from_pem(pem)

    assert {:ok, _} =
             :public_key.pkix_path_validation(
               X509.Certificate.to_der(ca.cert),
               [
                 X509.Certificate.to_der(agent)
               ],
               []
             )
  end

  test "an org with / or , in it stays one O value (Codex P2)", ctx do
    settings = Keyword.put(ctx.settings, :ca_org, "Research/Development, Inc.")
    Bootstrap.ensure!(settings)
    ca = CA.load!(settings[:ca_cert], settings[:ca_key])
    assert X509.Certificate.subject(ca.cert, "O") == ["Research/Development, Inc."]
    assert X509.Certificate.subject(ca.cert, "CN") == ["Research/Development, Inc. CA"]
    assert %{chain: [_]} = TLS.check!(settings)
  end

  test "an IP hostname gets an IP SAN", ctx do
    settings = Keyword.put(ctx.settings, :server_hostname, "10.0.0.5")
    Bootstrap.ensure!(settings)
    assert %{chain: [_]} = TLS.check!(settings)
  end

  test "no hostname still makes a localhost server cert", ctx do
    settings = Keyword.delete(ctx.settings, :server_hostname)
    Bootstrap.ensure!(settings)
    assert %{chain: [_]} = TLS.check!(Keyword.put(settings, :server_hostname, "localhost"))
  end

  test "existing files (the Rust CA) are used as-is, byte for byte", %{dir: dir} = ctx do
    Bootstrap.ensure!(ctx.settings)
    files = for k <- ~w(ca_cert ca_key server_cert server_key)a, do: ctx.settings[k]
    before = Map.new(files, &{&1, {File.read!(&1), File.stat!(&1).mtime}})
    File.chmod!(dir, 0o755)

    assert :ok = Bootstrap.ensure!(ctx.settings)
    assert Map.new(files, &{&1, {File.read!(&1), File.stat!(&1).mtime}}) == before
    # an existing dir keeps its mode
    assert mode(dir) == 0o755
  end

  test "an existing CA without a server pair gets a server cert from that CA", ctx do
    {ca_cert, ca_key} = KuiperCoordinator.AgentGrpcCase.fixture_ca()
    File.mkdir_p!(ctx.dir)
    File.cp!(ca_cert, ctx.settings[:ca_cert])
    File.cp!(ca_key, ctx.settings[:ca_key])
    rust_ca = File.read!(ca_cert)

    Bootstrap.ensure!(ctx.settings)
    assert File.read!(ctx.settings[:ca_cert]) == rust_ca
    assert %{chain: [_]} = TLS.check!(ctx.settings)
  end

  test "half a CA pair stops boot naming the missing file", ctx do
    Bootstrap.ensure!(ctx.settings)
    File.rm!(ctx.settings[:ca_key])
    cert = File.read!(ctx.settings[:ca_cert])

    assert_raise ArgumentError, ~r/ca\.key.*missing/s, fn -> Bootstrap.ensure!(ctx.settings) end
    assert File.read!(ctx.settings[:ca_cert]) == cert
    refute File.exists?(ctx.settings[:ca_key])
  end

  test "half a server pair stops boot naming the missing file", ctx do
    Bootstrap.ensure!(ctx.settings)
    File.rm!(ctx.settings[:server_cert])
    key = File.read!(ctx.settings[:server_key])

    assert_raise ArgumentError, ~r/server\.crt.*missing/s, fn ->
      Bootstrap.ensure!(ctx.settings)
    end

    assert File.read!(ctx.settings[:server_key]) == key
  end

  test "a CA key that doesn't match fails, and nothing is written", ctx do
    Bootstrap.ensure!(ctx.settings)
    File.rm!(ctx.settings[:server_cert])
    File.rm!(ctx.settings[:server_key])

    File.write!(
      ctx.settings[:ca_key],
      X509.PrivateKey.new_ec(:secp256r1) |> X509.PrivateKey.to_pem()
    )

    assert_raise ArgumentError, ~r/does not match/, fn -> Bootstrap.ensure!(ctx.settings) end
    refute File.exists?(ctx.settings[:server_cert])
  end

  test "chain trust mode never makes a server cert", ctx do
    settings = Keyword.put(ctx.settings, :server_trust_mode, "chain")
    Bootstrap.ensure!(settings)
    assert File.exists?(settings[:ca_cert])
    refute File.exists?(settings[:server_cert])
    refute File.exists?(settings[:server_key])
  end
end
