defmodule KuiperCoordinator.AgentGrpcCase do
  @moduledoc """
  Starts the agent gRPC port for a test with the CA made by the Rust `kuiper-forge ca init`
  (test/fixtures/rust_init_ca) and a server cert for localhost signed by it, and gives a client
  that connects with or without an agent client cert.
  """
  use ExUnit.CaseTemplate
  alias X509.Certificate.Extension, as: Ext

  @fixtures Path.expand("../fixtures/rust_init_ca", __DIR__)

  using do
    quote do
      import KuiperCoordinator.AgentGrpcCase
      alias Cirunner.Agent
    end
  end

  setup tags do
    KuiperCoordinator.DataCase.setup_sandbox(Map.put(tags, :async, false))
    dir = Path.join(System.tmp_dir!(), "kc-grpc-#{System.unique_integer([:positive])}")
    File.mkdir_p!(dir)
    on_exit(fn -> File.rm_rf!(dir) end)

    certs = make_server_certs(dir)
    port = free_port()

    settings =
      [
        enabled: true,
        port: port,
        ip: {127, 0, 0, 1},
        ca_cert: certs.ca_cert,
        ca_key: certs.ca_key,
        server_cert: certs.server_cert,
        server_key: certs.server_key,
        server_trust_mode: "ca",
        server_hostname: "localhost"
      ] ++ Map.get(tags, :grpc_settings, [])

    old = Application.get_env(:kuiper_coordinator, :agent_grpc)
    Application.put_env(:kuiper_coordinator, :agent_grpc, settings)
    on_exit(fn -> Application.put_env(:kuiper_coordinator, :agent_grpc, old || []) end)

    unless tags[:no_server] do
      start_supervised!({KuiperCoordinator.AgentGrpc.Supervisor, settings})
    end

    {:ok, port: port, dir: dir, certs: certs, settings: settings}
  end

  def fixture_ca, do: {Path.join(@fixtures, "ca.crt"), Path.join(@fixtures, "ca.key")}

  def make_server_certs(dir) do
    {ca_cert_file, ca_key_file} = fixture_ca()
    ca = KuiperCoordinator.CA.load!(ca_cert_file, ca_key_file)
    key = X509.PrivateKey.new_ec(:secp256r1)

    cert =
      key
      |> X509.PublicKey.derive()
      |> X509.Certificate.new("/O=Kuiper Test/CN=localhost", ca.cert, ca.key,
        extensions: [
          subject_alt_name: Ext.subject_alt_name(["localhost", {:iPAddress, <<127, 0, 0, 1>>}]),
          ext_key_usage: Ext.ext_key_usage([:serverAuth])
        ]
      )

    server_cert = Path.join(dir, "server.crt")
    server_key = Path.join(dir, "server.key")
    File.write!(server_cert, X509.Certificate.to_pem(cert))
    File.write!(server_key, X509.PrivateKey.to_pem(key))

    %{
      ca_cert: ca_cert_file,
      ca_key: ca_key_file,
      server_cert: server_cert,
      server_key: server_key
    }
  end

  @doc "A client cert and key from a CA the coordinator doesn't trust."
  def foreign_client_cert(dir, cn) do
    ca_key = X509.PrivateKey.new_ec(:secp256r1)
    ca = X509.Certificate.self_signed(ca_key, "/O=Rogue/CN=Rogue CA", template: :root_ca)
    key = X509.PrivateKey.new_ec(:secp256r1)

    cert =
      key
      |> X509.PublicKey.derive()
      |> X509.Certificate.new("/O=CI Runner Agent/CN=#{cn}", ca, ca_key,
        extensions: [ext_key_usage: Ext.ext_key_usage([:clientAuth])]
      )

    write_pair(dir, "rogue", X509.Certificate.to_pem(cert), X509.PrivateKey.to_pem(key))
  end

  def write_pair(dir, name, cert_pem, key_pem) do
    cert = Path.join(dir, "#{name}.crt")
    key = Path.join(dir, "#{name}.key")
    File.write!(cert, cert_pem)
    File.write!(key, key_pem)
    {cert, key}
  end

  def free_port do
    {:ok, sock} = :gen_tcp.listen(0, [])
    {:ok, port} = :inet.port(sock)
    :gen_tcp.close(sock)
    port
  end

  @doc "Connects to the agent port; pass `client: {cert_file, key_file}` for mTLS."
  def connect(ctx, opts \\ []) do
    ssl =
      [
        cacertfile: Keyword.get(opts, :server_ca, ctx.certs.ca_cert),
        verify: :verify_peer,
        server_name_indication: ~c"localhost"
      ] ++
        case Keyword.get(opts, :client) do
          {cert, key} -> [certfile: cert, keyfile: key]
          nil -> []
        end

    GRPC.Stub.connect("localhost:#{ctx.port}",
      cred: GRPC.Credential.new(ssl: ssl),
      adapter: GRPC.Client.Adapters.Mint
    )
  end

  @doc "Registers with a fresh token and returns the response and the client cert files."
  def register_agent(ctx, hostname \\ "mac-mini-1", type \\ "tart") do
    {:ok, token} = KuiperCoordinator.Agents.create_registration_token()
    {:ok, ch} = connect(ctx)

    {:ok, resp} =
      Cirunner.Agent.RegistrationService.Stub.register(ch, %Cirunner.Agent.RegisterRequest{
        registration_token: token.token,
        hostname: hostname,
        agent_type: type
      })

    GRPC.Stub.disconnect(ch)
    files = write_pair(ctx.dir, resp.agent_id, resp.client_cert_pem, resp.client_key_pem)
    {resp, files}
  end

  def status_msg(opts \\ []) do
    %Cirunner.Agent.AgentMessage{
      payload:
        {:status,
         %Cirunner.Agent.AgentStatus{
           hostname: Keyword.get(opts, :hostname, "mac-mini-1"),
           agent_type: Keyword.get(opts, :agent_type, "tart"),
           labels: Keyword.get(opts, :labels, ["self-hosted", "macOS"]),
           max_vms: Keyword.get(opts, :max_vms, 2),
           agent_version: Keyword.get(opts, :agent_version, "0.9.1"),
           # the coordinator ignores this; identity comes from the cert
           agent_id: Keyword.get(opts, :agent_id, "agent_spoofed")
         }}
    }
  end

  @doc "Opens AgentStream on `ch`, sends `msgs`, returns {stream, replies enum or error}."
  def open_stream(ch, msgs, opts \\ []) do
    stream = Cirunner.Agent.AgentService.Stub.agent_stream(ch)
    Enum.each(msgs, &GRPC.Stub.send_request(stream, &1))
    if Keyword.get(opts, :end_stream, false), do: GRPC.Stub.end_stream(stream)
    {stream, GRPC.Stub.recv(stream, timeout: Keyword.get(opts, :timeout, 5_000))}
  end
end
