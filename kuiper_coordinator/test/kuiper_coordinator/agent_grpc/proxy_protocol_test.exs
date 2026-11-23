defmodule KuiperCoordinator.AgentGrpc.ProxyProtocolTest do
  # A3, D74: behind a TLS-passthrough balancer (prod: Traefik TLSRoute with
  # KUIPER_GRPC__PROXY_PROTOCOL=true) every connection starts with a PROXY v1/v2 header; direct
  # connections must keep working with it on, like the Rust coordinator's parse_proxy_protocol.
  use KuiperCoordinator.AgentGrpcCase
  @moduletag :capture_log
  @moduletag grpc_settings: [proxy_protocol: true, proxy_header_timeout: 300]

  alias Cirunner.Agent.CoordinatorMessage
  alias KuiperCoordinator.AgentRegistry

  @v2_sig <<"\r\n\r\n", 0, "\r\nQUIT\n">>

  def v1_tcp4, do: "PROXY TCP4 203.0.113.7 10.0.0.1 51234 9543\r\n"
  def v1_tcp6, do: "PROXY TCP6 2001:db8::7 2001:db8::1 51234 9543\r\n"
  def v1_unknown, do: "PROXY UNKNOWN\r\n"

  # PROXY v2, PROXY command, TCP over IPv4, with an ALPN TLV after the addresses
  def v2_tcp4 do
    addrs = <<203, 0, 113, 7, 10, 0, 0, 1, 51234::16, 9543::16>>
    tlv = <<0x01, 2::16, "h2">>
    <<@v2_sig, 0x21, 0x11, byte_size(addrs <> tlv)::16>> <> addrs <> tlv
  end

  # PROXY v2 LOCAL (a balancer's own health check): no addresses
  def v2_local, do: <<@v2_sig, 0x20, 0x00, 0::16>>

  @doc """
  A balancer in front of the agent port: each connection gets `header` and then the client's
  bytes, the header and the client's first bytes (the TLS ClientHello) in one write, so the
  server has to stop reading exactly at the header's end.
  """
  def start_balancer(ctx, header) do
    {:ok, lsock} = :gen_tcp.listen(0, [:binary, active: false, ip: {127, 0, 0, 1}])
    {:ok, port} = :inet.port(lsock)
    pid = spawn_link(fn -> accept_loop(lsock, ctx.port, header) end)
    :ok = :gen_tcp.controlling_process(lsock, pid)
    on_exit(fn -> Process.exit(pid, :kill) end)
    %{ctx | port: port}
  end

  defp accept_loop(lsock, upstream, header) do
    {:ok, client} = :gen_tcp.accept(lsock)
    pid = spawn(fn -> relay(client, upstream, header) end)
    :ok = :gen_tcp.controlling_process(client, pid)
    send(pid, :go)
    accept_loop(lsock, upstream, header)
  end

  defp relay(client, upstream, header) do
    receive do: (:go -> :ok)
    {:ok, server} = :gen_tcp.connect(~c"127.0.0.1", upstream, [:binary, active: false])
    {:ok, first} = :gen_tcp.recv(client, 0, 5_000)
    :ok = :gen_tcp.send(server, header <> first)
    :ok = :inet.setopts(client, active: true)
    :ok = :inet.setopts(server, active: true)
    pipe(client, server)
  end

  defp pipe(client, server) do
    receive do
      {:tcp, ^client, data} -> :gen_tcp.send(server, data) && pipe(client, server)
      {:tcp, ^server, data} -> :gen_tcp.send(client, data) && pipe(client, server)
      {:tcp_closed, _} -> :gen_tcp.close(client) && :gen_tcp.close(server)
    end
  end

  defp raw_connect(ctx) do
    {:ok, sock} = :gen_tcp.connect(~c"127.0.0.1", ctx.port, [:binary, active: false])
    sock
  end

  for {name, header} <- [
        {"v1 TCP4", :v1_tcp4},
        {"v1 TCP6", :v1_tcp6},
        {"v1 UNKNOWN", :v1_unknown},
        {"v2 TCP4 with a TLV", :v2_tcp4},
        {"v2 LOCAL", :v2_local}
      ] do
    test "an agent registers through a PROXY #{name} header", ctx do
      via = start_balancer(ctx, unquote(header)())
      {resp, _files} = register_agent(via)
      assert "agent_tart_" <> _ = resp.agent_id
    end
  end

  test "an agent registers on a direct connection with PROXY protocol on", ctx do
    {resp, _files} = register_agent(ctx)
    assert "agent_tart_" <> _ = resp.agent_id
  end

  test "mTLS AgentStream works through a PROXY v2 header: identity still from the client cert",
       ctx do
    via = start_balancer(ctx, v2_tcp4())
    {resp, files} = register_agent(via)
    {:ok, ch} = connect(via, client: files)
    {_stream, {:ok, replies}} = open_stream(ch, [status_msg(agent_id: "agent_someone_else")])
    assert [{:ok, %CoordinatorMessage{payload: {:ping, _}}}] = Enum.take(replies, 1)
    assert AgentRegistry.connected?(resp.agent_id)
    refute AgentRegistry.connected?("agent_someone_else")
  end

  test "a malformed PROXY header closes the connection", ctx do
    for bad <- [
          "PROXY BOGUS\r\n",
          "PROXY TCP4 not-an-ip 10.0.0.1 1 2\r\n",
          "P" <> String.duplicate("X", 200),
          <<@v2_sig, 0x99, 0x11, 0::16>>,
          <<"\r\n\r\nnot-a-v2-signature">>
        ] do
      sock = raw_connect(ctx)
      :ok = :gen_tcp.send(sock, bad)
      assert {:error, :closed} = :gen_tcp.recv(sock, 0, 2_000), inspect(bad)
    end
  end

  test "dropped connections don't leave processes behind", ctx do
    # warm up, then count
    sock = raw_connect(ctx)
    :ok = :gen_tcp.send(sock, "PROXY BOGUS\r\n")
    {:error, :closed} = :gen_tcp.recv(sock, 0, 2_000)
    Process.sleep(100)
    before = length(Process.list())

    for _ <- 1..50 do
      sock = raw_connect(ctx)
      :ok = :gen_tcp.send(sock, "PROXY BOGUS\r\n")
      {:error, :closed} = :gen_tcp.recv(sock, 0, 2_000)
    end

    Process.sleep(300)
    assert length(Process.list()) <= before + 5
  end

  test "a PROXY header that stalls is dropped after the header timeout", ctx do
    sock = raw_connect(ctx)
    :ok = :gen_tcp.send(sock, "PROXY TCP4 203.0.113.7")
    started = System.monotonic_time(:millisecond)
    assert {:error, :closed} = :gen_tcp.recv(sock, 0, 3_000)
    assert System.monotonic_time(:millisecond) - started < 2_000
  end

  @tag grpc_settings: [proxy_protocol: false]
  test "with PROXY protocol off (the default) a PROXY header fails the handshake", ctx do
    via = start_balancer(ctx, v1_tcp4())
    assert {:error, _} = connect(via)
  end

  # D72/D73: the Rust coordinator's env block, unchanged, boots Phoenix's agent port behind the
  # balancer: TLS files from KUIPER_TLS__*, trust mode chain, PROXY on from KUIPER_GRPC__*.
  @tag :no_server
  test "the Rust coordinator's prod env block boots the agent port behind a PROXY balancer",
       ctx do
    # the Rust image's /data volume with its CA; KUIPER_DATA_DIR=/data comes from the Phoenix
    # image's own ENV (milestone 9), not the Rust env block
    data = Path.join(ctx.dir, "data")
    File.mkdir_p!(data)
    File.cp!(ctx.certs.ca_cert, Path.join(data, "ca.crt"))
    File.cp!(ctx.certs.ca_key, Path.join(data, "ca.key"))

    rust_env = %{
      "KUIPER_TLS__CA_CERT" => Path.join(data, "ca.crt"),
      "KUIPER_TLS__CA_KEY" => Path.join(data, "ca.key"),
      "KUIPER_TLS__SERVER_CERT" => ctx.certs.server_cert,
      "KUIPER_TLS__SERVER_KEY" => ctx.certs.server_key,
      "KUIPER_TLS__SERVER_TRUST_MODE" => "chain",
      "KUIPER_TLS__SERVER_USE_NATIVE_ROOTS" => "true",
      "KUIPER_GRPC__LISTEN_ADDR" => "127.0.0.1:#{ctx.port}",
      "KUIPER_GRPC__PROXY_PROTOCOL" => "true",
      "KUIPER_ADMIN__COORDINATOR_URL" => "https://localhost:#{ctx.port}",
      "KUIPER_DATABASE__PATH" => "/data/kuiper.db",
      "KUIPER_DATA_DIR" => data,
      "PHX_HOST" => "localhost",
      "DATABASE_URL" => "ecto://kuiper:pw@pg/kuiper_phoenix",
      "SECRET_KEY_BASE" => String.duplicate("s", 64),
      "KUIPER_ENCRYPTION_KEY" => Base.encode64(:crypto.strong_rand_bytes(32))
    }

    # nothing of Phoenix's own set
    for name <- ~w(KUIPER_AGENT_PORT KUIPER_CA_CERT),
        do: assert(System.get_env(name) == nil)

    saved = Map.new(rust_env, fn {k, _} -> {k, System.get_env(k)} end)
    Enum.each(rust_env, fn {k, v} -> System.put_env(k, v) end)

    on_exit(fn ->
      Enum.each(saved, fn
        {k, nil} -> System.delete_env(k)
        {k, v} -> System.put_env(k, v)
      end)
    end)

    runtime = Path.expand("../../../config/runtime.exs", __DIR__)
    settings = Config.Reader.read!(runtime, env: :prod)[:kuiper_coordinator][:agent_grpc]
    assert settings[:proxy_protocol]
    assert settings[:server_cert] == ctx.certs.server_cert
    assert settings[:server_trust_mode] == "chain"
    assert settings[:data_dir] == data
    assert {settings[:ip], settings[:port]} == {{127, 0, 0, 1}, ctx.port}

    settings = Keyword.put(settings, :proxy_header_timeout, 300)

    Application.put_env(:kuiper_coordinator, :agent_grpc, settings)
    start_supervised!({KuiperCoordinator.AgentGrpc.Supervisor, settings})

    {resp, _} = register_agent(start_balancer(ctx, v2_tcp4()))
    assert "agent_tart_" <> _ = resp.agent_id
    # chain mode with native roots: agents verify the server with their system roots, like Rust
    assert resp.server_ca_pem == ""
    {resp, _} = register_agent(ctx)
    assert "agent_tart_" <> _ = resp.agent_id
  end
end
