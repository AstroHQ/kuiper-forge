defmodule KuiperCoordinator.AgentGrpc.RegistrationTest do
  use KuiperCoordinator.AgentGrpcCase
  @moduletag :capture_log

  alias Cirunner.Agent.RegisterRequest
  alias Cirunner.Agent.RegistrationService.Stub
  alias KuiperCoordinator.Agents

  # A1: Register works over server-only TLS (no client cert)
  test "Register without a client cert succeeds and returns a cert signed by the Rust-made CA",
       ctx do
    {resp, _files} = register_agent(ctx, "Mac Mini.local", "tart")

    assert resp.agent_id =~ ~r/\Aagent_tart_mac-mini-local_[A-Za-z0-9]{8}\z/
    assert resp.server_trust_mode == "ca"
    {ca_file, _} = fixture_ca()
    assert resp.server_ca_pem == File.read!(ca_file)
    assert {:ok, _, 0} = DateTime.from_iso8601(resp.expires_at)

    # A8: fields of the issued cert
    cert = X509.Certificate.from_pem!(resp.client_cert_pem)
    assert X509.Certificate.subject(cert, "CN") == [resp.agent_id]
    assert X509.Certificate.subject(cert, "O") == ["CI Runner Agent"]

    assert {:Extension, _, _, [{:dNSName, dns}]} =
             X509.Certificate.extension(cert, :subject_alt_name)

    assert to_string(dns) == resp.agent_id

    assert {:Extension, _, _, [{1, 3, 6, 1, 5, 5, 7, 3, 2}]} =
             X509.Certificate.extension(cert, :ext_key_usage)

    # P-256
    assert {{:ECPoint, _}, {:namedCurve, {1, 2, 840, 10045, 3, 1, 7}}} =
             X509.Certificate.public_key(cert)

    {:Validity, _, not_after} = X509.Certificate.validity(cert)
    days = DateTime.diff(X509.DateTime.to_datetime(not_after), DateTime.utc_now(), :day)
    assert days in 364..365

    # D2/A6/A7: chains to the CA file made by `kuiper-forge ca init`, issuer is its real subject
    ca = X509.Certificate.from_pem!(File.read!(ca_file))
    assert X509.Certificate.issuer(cert) == X509.Certificate.subject(ca)

    assert {:ok, _} =
             :public_key.pkix_path_validation(
               X509.Certificate.to_der(ca),
               [X509.Certificate.to_der(cert)],
               []
             )

    assert X509.PrivateKey.from_pem!(resp.client_key_pem) |> X509.PublicKey.derive() ==
             X509.Certificate.public_key(cert)

    agent = Agents.get_agent(resp.agent_id)
    assert agent.hostname == "Mac Mini.local"
    assert agent.agent_type == "tart"
    assert agent.labels == []
    assert agent.max_vms == 0
  end

  test "an unknown token is refused with Unauthenticated", ctx do
    {:ok, ch} = connect(ctx)

    assert {:error, %GRPC.RPCError{status: 16, message: "Registration failed: " <> _}} =
             Stub.register(ch, %RegisterRequest{
               registration_token: "reg_nope",
               hostname: "h",
               agent_type: "tart"
             })
  end

  # A10: a bad type is rejected before the token is consumed
  test "a bad agent type gets InvalidArgument and leaves the token usable", ctx do
    {:ok, token} = Agents.create_registration_token()
    {:ok, ch} = connect(ctx)

    req = %RegisterRequest{
      registration_token: token.token,
      hostname: "h",
      agent_type: "bad type!"
    }

    assert {:error, %GRPC.RPCError{status: 3}} = Stub.register(ch, req)
    assert {:ok, resp} = Stub.register(ch, %{req | agent_type: "Libvirt"})
    assert resp.agent_id =~ ~r/\Aagent_libvirt_h_/
  end

  test "agent types: any 1-32 of [a-z0-9-] after lowercasing" do
    assert {:ok, "proxmox"} = Agents.parse_agent_type("Proxmox")
    assert {:ok, "my-agent-2"} = Agents.parse_agent_type("my-agent-2")
    assert {:ok, _} = Agents.parse_agent_type(String.duplicate("a", 32))
    assert {:error, _} = Agents.parse_agent_type(String.duplicate("a", 33))
    assert {:error, _} = Agents.parse_agent_type("")
    assert {:error, _} = Agents.parse_agent_type("tart_vm")
  end

  # A11: single use
  test "a token works once: a second Register with it is refused", ctx do
    {:ok, token} = Agents.create_registration_token()
    {:ok, ch} = connect(ctx)
    req = %RegisterRequest{registration_token: token.token, hostname: "h", agent_type: "tart"}

    assert {:ok, _} = Stub.register(ch, req)
    assert {:error, %GRPC.RPCError{status: 16}} = Stub.register(ch, req)
  end

  # A11: atomic consume under concurrency
  test "concurrent Registers with one token: exactly one wins", ctx do
    {:ok, token} = Agents.create_registration_token()
    req = %RegisterRequest{registration_token: token.token, hostname: "h", agent_type: "tart"}

    results =
      1..8
      |> Enum.map(fn _ ->
        Task.async(fn ->
          {:ok, ch} = connect(ctx)
          Stub.register(ch, req)
        end)
      end)
      |> Task.await_many(10_000)

    assert Enum.count(results, &match?({:ok, _}, &1)) == 1
    assert Enum.count(results, &match?({:error, %GRPC.RPCError{status: 16}}, &1)) == 7
  end

  # A11: consumed before the expiry check, so an expired token is burned
  test "an expired token is refused and burned", ctx do
    {:ok, token} = Agents.create_registration_token(-1)
    {:ok, ch} = connect(ctx)
    req = %RegisterRequest{registration_token: token.token, hostname: "h", agent_type: "tart"}

    assert {:error, %GRPC.RPCError{status: 16, message: msg}} = Stub.register(ch, req)
    assert msg =~ "expired"
    refute KuiperCoordinator.Repo.get(Agents.RegistrationToken, token.token)
  end

  test "agent ids sanitize the hostname like auth.rs" do
    assert KuiperCoordinator.CA.sanitize_hostname("Mac_Mini.local") == "mac-mini-local"
    assert KuiperCoordinator.CA.sanitize_hostname("ci-01") == "ci-01"
    id = KuiperCoordinator.CA.new_agent_id("proxmox", "pve 1")
    assert id =~ ~r/\Aagent_proxmox_pve-1_[A-Za-z0-9]{8}\z/
  end
end
