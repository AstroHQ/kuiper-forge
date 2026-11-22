defmodule KuiperCoordinator.RuntimeConfigTest do
  # Mutates OS env vars read by config/runtime.exs.
  use ExUnit.Case, async: false

  @runtime Path.expand("../../config/runtime.exs", __DIR__)
  @vars ~w(KUIPER_HTTP_PORT PORT KUIPER_AGENT_GRPC KUIPER_AGENT_PORT KUIPER_GITHUB_APP_ID
           KUIPER_GITHUB_PRIVATE_KEY KUIPER_FORGEJO_URL KUIPER_FORGEJO_TOKEN KUIPER_FORGEJO_SCOPES
           KUIPER_PROVISIONING_MODE KUIPER_POLL_INTERVAL_SECS KUIPER_CONFIG
           KUIPER_ENCRYPTION_KEY KUIPER_DATA_DIR KUIPER_CA_CERT KUIPER_CA_ORG
           KUIPER_MANAGEMENT KUIPER_MANAGEMENT_SOCKET)

  setup do
    saved = Map.new(@vars, &{&1, System.get_env(&1)})
    Enum.each(@vars, &System.delete_env/1)

    on_exit(fn ->
      Enum.each(saved, fn
        {k, nil} -> System.delete_env(k)
        {k, v} -> System.put_env(k, v)
      end)
    end)
  end

  defp config, do: Config.Reader.read!(@runtime, env: :dev, target: :host)
  defp http, do: get_in(config(), [:kuiper_coordinator, KuiperCoordinatorWeb.Endpoint, :http])
  defp agent_grpc, do: get_in(config(), [:kuiper_coordinator, :agent_grpc])

  test "HTTP defaults to its own port, not Phoenix's 4000 or the Rust coordinator's 9443" do
    assert http()[:port] == 9480
  end

  test "KUIPER_HTTP_PORT sets the port" do
    System.put_env("KUIPER_HTTP_PORT", "9555")
    assert http()[:port] == 9555
  end

  test "PORT works as a fallback, KUIPER_HTTP_PORT wins over it" do
    System.put_env("PORT", "9600")
    assert http()[:port] == 9600
    System.put_env("KUIPER_HTTP_PORT", "9601")
    assert http()[:port] == 9601
  end

  test "agent gRPC gets its own port, KUIPER_AGENT_PORT, default 9543, and HTTP keeps PORT (D21)" do
    System.put_env("KUIPER_AGENT_GRPC", "true")
    System.put_env("PORT", "9700")
    assert http()[:port] == 9700
    assert agent_grpc()[:enabled]
    assert agent_grpc()[:port] == 9543
    System.put_env("KUIPER_AGENT_PORT", "9701")
    assert agent_grpc()[:port] == 9701
    assert http()[:port] == 9700
  end

  test "provisioning isn't configured from env vars: forges live in the DB (D39)" do
    System.put_env("KUIPER_FORGEJO_URL", "https://git.example.com")
    System.put_env("KUIPER_FORGEJO_TOKEN", "tok")
    System.put_env("KUIPER_FORGEJO_SCOPES", "acme")
    System.put_env("KUIPER_PROVISIONING_MODE", "polling")
    System.put_env("KUIPER_GITHUB_APP_ID", "42")

    app = config()[:kuiper_coordinator]
    refute Keyword.has_key?(app, :fleet)
    refute Keyword.has_key?(app, :webhook)
  end

  test "KUIPER_ENCRYPTION_KEY (base64, 32 bytes) is the key for stored secrets" do
    key = :crypto.strong_rand_bytes(32)
    System.put_env("KUIPER_ENCRYPTION_KEY", Base.encode64(key))
    assert get_in(config(), [:kuiper_coordinator, KuiperCoordinator.Vault, :key]) == key
  end

  test "a KUIPER_ENCRYPTION_KEY that isn't 32 bytes of base64 stops startup" do
    System.put_env("KUIPER_ENCRYPTION_KEY", Base.encode64("too short"))
    assert_raise RuntimeError, ~r/KUIPER_ENCRYPTION_KEY/, fn -> config() end
  end

  test "dev without KUIPER_ENCRYPTION_KEY gets a fixed dev key" do
    assert byte_size(get_in(config(), [:kuiper_coordinator, KuiperCoordinator.Vault, :key])) == 32
  end

  test "agent gRPC and Management are on by default; false or 0 turns gRPC off (D58)" do
    assert agent_grpc()[:enabled]
    assert get_in(config(), [:kuiper_coordinator, :management, :enabled])

    for off <- ~w(false 0) do
      System.put_env("KUIPER_AGENT_GRPC", off)
      refute agent_grpc()[:enabled]
    end
  end

  test "dev keeps its data in a gitignored dir in the app, writable without sudo (D58)" do
    dir = Path.expand("../../.kuiper-data", __DIR__)
    assert agent_grpc()[:data_dir] == dir
    assert agent_grpc()[:ca_cert] == Path.join(dir, "ca.crt")
    assert agent_grpc()[:ca_org] == "Kuiper Forge"
    socket = get_in(config(), [:kuiper_coordinator, :management, :socket])
    assert byte_size(socket) <= 103

    {_, 0} =
      System.cmd("git", ["check-ignore", "-q", Path.join(dir, "ca.key")], cd: Path.dirname(dir))

    System.put_env("KUIPER_DATA_DIR", "/srv/kf")
    System.put_env("KUIPER_CA_ORG", "Acme")
    assert agent_grpc()[:ca_key] == "/srv/kf/ca.key"
    assert agent_grpc()[:ca_org] == "Acme"
  end
end
