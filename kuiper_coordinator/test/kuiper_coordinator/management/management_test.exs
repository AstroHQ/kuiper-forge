defmodule KuiperCoordinator.ManagementTest do
  @moduledoc """
  ManagementService (management.proto) on its own Unix socket (parity M1-M6): the Rust CLI's
  `token` and `agent` commands talk to it. Phoenix uses its own socket path so it never steals
  the Rust coordinator's `coordinator.sock`, and the socket is owner-only (0600).
  """
  use ExUnit.Case, async: false

  alias Cirunner.Management, as: M
  alias Cirunner.Management.ManagementService.Stub
  alias KuiperCoordinator.Agents.Agent
  alias KuiperCoordinator.Repo

  @fixtures Path.expand("../../fixtures/rust_init_ca", __DIR__)

  setup tags do
    KuiperCoordinator.DataCase.setup_sandbox(Map.put(tags, :async, false))
    # Unix socket paths are capped near 108 bytes, so keep the dir short
    dir = Path.join("/tmp", "kcm-#{System.unique_integer([:positive])}")
    on_exit(fn -> File.rm_rf!(dir) end)
    socket = Path.join([dir, "run", "management.sock"])

    old = Application.get_env(:kuiper_coordinator, :agent_grpc)

    Application.put_env(:kuiper_coordinator, :agent_grpc,
      ca_cert: Path.join(@fixtures, "ca.crt"),
      ca_key: Path.join(@fixtures, "ca.key"),
      server_trust_mode: "ca"
    )

    on_exit(fn -> Application.put_env(:kuiper_coordinator, :agent_grpc, old || []) end)

    unless tags[:no_server] do
      start_supervised!({KuiperCoordinator.Management.Supervisor, socket: socket})
    end

    {:ok, socket: socket, dir: dir}
  end

  defp channel(socket) do
    # Mint needs a :hostname for a non-TCP address (the :authority header)
    {:ok, ch} =
      GRPC.Stub.connect("unix:" <> socket,
        adapter: GRPC.Client.Adapters.Mint,
        adapter_opts: [config_options: [hostname: "localhost"]]
      )

    on_exit(fn -> GRPC.Stub.disconnect(ch) end)
    ch
  end

  defp insert_agent(attrs) do
    now = DateTime.utc_now()

    Repo.insert!(
      struct(
        %Agent{
          agent_id: "agent_" <> Integer.to_string(System.unique_integer([:positive])),
          hostname: "mac-mini-1",
          agent_type: "tart",
          labels: ["macos", "arm64"],
          max_vms: 2,
          serial_number: "01",
          expires_at: DateTime.add(now, 365 * 86_400, :second)
        },
        attrs
      )
    )
  end

  test "the socket is owner-only and its directory is created", %{socket: socket} do
    assert {:ok, %File.Stat{type: :other, mode: mode}} = File.stat(socket)
    assert Bitwise.band(mode, 0o777) == 0o600
  end

  test "CreateToken returns a registration token with the server CA and trust mode",
       %{socket: socket} do
    ch = channel(socket)
    before = DateTime.utc_now()

    assert {:ok, %M.CreateTokenResponse{} = resp} =
             Stub.create_token(ch, %M.CreateTokenRequest{expires_secs: 600, created_by: "cli"})

    assert "reg_" <> rest = resp.token
    assert String.length(rest) == 32
    assert resp.server_trust_mode == "ca"
    assert resp.server_ca_pem == File.read!(Path.join(@fixtures, "ca.crt"))

    {:ok, expires_at, 0} = DateTime.from_iso8601(resp.expires_at)
    {:ok, created_at, 0} = DateTime.from_iso8601(resp.created_at)
    assert DateTime.diff(expires_at, before, :second) in 599..601
    assert DateTime.diff(created_at, before, :second) in -1..1

    # the token works for registration (same single-use table as RegistrationService)
    KuiperCoordinator.CA.install!(Path.join(@fixtures, "ca.crt"), Path.join(@fixtures, "ca.key"))
    assert {:ok, _issued} = KuiperCoordinator.Agents.register(resp.token, "host-a", "tart")
  end

  test "CreateToken refuses a TTL of zero or over a year (M1)", %{socket: socket} do
    ch = channel(socket)

    for secs <- [0, 366 * 86_400, 18_446_744_073_709_551_615] do
      assert {:error, %GRPC.RPCError{status: status}} =
               Stub.create_token(ch, %M.CreateTokenRequest{expires_secs: secs, created_by: "cli"})

      assert status == GRPC.Status.invalid_argument()
    end

    assert {:ok, %M.ListTokensResponse{tokens: []}} =
             Stub.list_tokens(ch, %M.ListTokensRequest{})
  end

  test "ListTokens lists tokens in plaintext; DeleteToken deletes once", %{socket: socket} do
    ch = channel(socket)
    {:ok, a} = Stub.create_token(ch, %M.CreateTokenRequest{expires_secs: 60, created_by: "cli"})
    {:ok, b} = Stub.create_token(ch, %M.CreateTokenRequest{expires_secs: 120, created_by: "ops"})

    assert {:ok, %M.ListTokensResponse{tokens: tokens}} =
             Stub.list_tokens(ch, %M.ListTokensRequest{})

    assert Enum.sort(Enum.map(tokens, &{&1.token, &1.created_by})) ==
             Enum.sort([{a.token, "cli"}, {b.token, "ops"}])

    info = Enum.find(tokens, &(&1.token == a.token))
    assert info.expires_at == a.expires_at
    assert info.created_at == a.created_at

    assert {:ok, %M.DeleteTokenResponse{deleted: true}} =
             Stub.delete_token(ch, %M.DeleteTokenRequest{token: a.token})

    assert {:ok, %M.DeleteTokenResponse{deleted: false}} =
             Stub.delete_token(ch, %M.DeleteTokenRequest{token: a.token})

    assert {:ok, %M.ListTokensResponse{tokens: [%M.TokenInfo{token: tok}]}} =
             Stub.list_tokens(ch, %M.ListTokensRequest{})

    assert tok == b.token
  end

  test "ListAgents returns every agent, newest first, with revoked and version", %{socket: socket} do
    old =
      insert_agent(
        agent_version: "0.9.1",
        inserted_at: ~U[2026-11-01 10:00:00.000000Z],
        revoked_at: ~U[2026-11-02 10:00:00.000000Z]
      )

    new = insert_agent(hostname: "pve-1", agent_type: "proxmox", labels: ["linux"], max_vms: 8)

    ch = channel(socket)

    assert {:ok, %M.ListAgentsResponse{agents: [first, second]}} =
             Stub.list_agents(ch, %M.ListAgentsRequest{})

    assert first.agent_id == new.agent_id
    assert first.hostname == "pve-1"
    assert first.agent_type == "proxmox"
    assert first.labels == ["linux"]
    assert first.max_vms == 8
    assert first.revoked == false
    assert first.agent_version == ""
    assert {:ok, _, 0} = DateTime.from_iso8601(first.created_at)
    assert {:ok, _, 0} = DateTime.from_iso8601(first.expires_at)

    assert second.agent_id == old.agent_id
    assert second.revoked == true
    assert second.agent_version == "0.9.1"
    assert second.created_at == "2026-11-01T10:00:00.000000Z"
  end

  test "RevokeAgent revokes a known agent once; unknown is false with OK status (M5)",
       %{socket: socket} do
    agent = insert_agent([])
    ch = channel(socket)

    assert {:ok, %M.RevokeAgentResponse{revoked: true}} =
             Stub.revoke_agent(ch, %M.RevokeAgentRequest{agent_id: agent.agent_id})

    assert Repo.get!(Agent, agent.agent_id).revoked_at
    refute KuiperCoordinator.Agents.valid_agent?(agent.agent_id)

    assert {:ok, %M.RevokeAgentResponse{revoked: false}} =
             Stub.revoke_agent(ch, %M.RevokeAgentRequest{agent_id: agent.agent_id})

    assert {:ok, %M.RevokeAgentResponse{revoked: false}} =
             Stub.revoke_agent(ch, %M.RevokeAgentRequest{agent_id: "agent_nope"})
  end

  @tag :no_server
  test "a stale socket file is replaced, a live one is never stolen (M6)", %{socket: socket} do
    # stale: a socket left behind by a crash, nothing listening
    File.mkdir_p!(Path.dirname(socket))
    {:ok, stale} = :gen_tcp.listen(0, ifaddr: {:local, socket})
    :gen_tcp.close(stale)
    assert File.exists?(socket)

    start_supervised!({KuiperCoordinator.Management.Supervisor, socket: socket})
    ch = channel(socket)
    assert {:ok, _} = Stub.list_tokens(ch, %M.ListTokensRequest{})

    # live: another coordinator already answers on it
    assert {:error, reason} =
             start_supervised(
               Supervisor.child_spec(
                 {KuiperCoordinator.Management.Supervisor, socket: socket, name: :second},
                 id: :second
               )
             )

    assert inspect(reason) =~ "already in use"
    # and the first one still serves
    assert {:ok, _} = Stub.list_tokens(ch, %M.ListTokensRequest{})
  end

  @tag :no_server
  @tag :no_server
  test "starts with the logger at :info, as in prod (grpc's start log can't print a Unix socket)",
       %{socket: socket} do
    level = Logger.level()
    Logger.configure(level: :info)
    on_exit(fn -> Logger.configure(level: level) end)

    ExUnit.CaptureLog.capture_log(fn ->
      start_supervised!({KuiperCoordinator.Management.Supervisor, socket: socket})
    end)

    assert {:ok, %File.Stat{type: :other}} = File.lstat(socket)
  end

  test "a socket path too long for a Unix socket fails naming the setting" do
    long = Path.join(System.tmp_dir!(), String.duplicate("d", 90) <> "/phoenix-coordinator.sock")

    assert_raise ArgumentError, ~r/at most 103.*KUIPER_MANAGEMENT_SOCKET/, fn ->
      KuiperCoordinator.Management.Supervisor.prepare_socket!(long)
    end

    refute File.exists?(Path.dirname(long))
  end

  test "the default socket path is not the Rust coordinator's coordinator.sock" do
    path = KuiperCoordinator.Management.default_socket("/var/lib/kuiper-forge")
    assert Path.dirname(path) == "/var/lib/kuiper-forge"
    refute Path.basename(path) == "coordinator.sock"
  end
end
