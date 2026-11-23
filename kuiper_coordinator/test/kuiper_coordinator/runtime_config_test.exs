defmodule KuiperCoordinator.RuntimeConfigTest do
  # Mutates OS env vars read by config/runtime.exs.
  use ExUnit.Case, async: false
  require Logger

  @runtime Path.expand("../../config/runtime.exs", __DIR__)
  @vars ~w(KUIPER_HTTP_PORT PORT KUIPER_AGENT_GRPC KUIPER_AGENT_PORT KUIPER_GITHUB_APP_ID
           KUIPER_GITHUB_PRIVATE_KEY KUIPER_FORGEJO_URL KUIPER_FORGEJO_TOKEN KUIPER_FORGEJO_SCOPES
           KUIPER_PROVISIONING_MODE KUIPER_POLL_INTERVAL_SECS KUIPER_CONFIG
           KUIPER_ENCRYPTION_KEY KUIPER_DATA_DIR KUIPER_CA_CERT KUIPER_CA_ORG
           KUIPER_MANAGEMENT KUIPER_MANAGEMENT_SOCKET KUIPER_CA_KEY KUIPER_SERVER_CERT
           KUIPER_SERVER_KEY KUIPER_SERVER_CA_CERT KUIPER_SERVER_TRUST_MODE
           KUIPER_SERVER_USE_NATIVE_ROOTS KUIPER_AGENT_PROXY_PROTOCOL KUIPER_COORDINATOR_URL
           KUIPER_ADMIN_SESSION_TIMEOUT KUIPER_UPDATE_CHECK KUIPER_METRICS_RETENTION_DAYS
           KUIPER_TLS__CA_CERT KUIPER_TLS__CA_KEY KUIPER_TLS__SERVER_CERT KUIPER_TLS__SERVER_KEY
           KUIPER_TLS__SERVER_CA_CERT KUIPER_TLS__SERVER_TRUST_MODE
           KUIPER_TLS__SERVER_USE_NATIVE_ROOTS KUIPER_GRPC__PROXY_PROTOCOL
           KUIPER_GRPC__LISTEN_ADDR KUIPER_ADMIN__COORDINATOR_URL
           KUIPER_ADMIN__SESSION_TIMEOUT_SECS KUIPER_ADMIN__UPDATE_CHECK
           KUIPER_METRICS__RETENTION_DAYS KUIPER_DATABASE__PATH KUIPER_DATABASE__HOST
           KUIPER_DATABASE__PORT KUIPER_DATABASE__USER KUIPER_DATABASE__PASSWORD
           KUIPER_DATABASE__DATABASE KUIPER_PHOENIX_DATABASE KUIPER_NO_LOG_FILE
           DATABASE_URL SECRET_KEY_BASE PHX_HOST PHX_SERVER)

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

  defp config(env \\ :dev), do: Config.Reader.read!(@runtime, env: env, target: :host)
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

    # two checkouts get two dev sockets (Codex P2)
    System.put_env("KUIPER_DATA_DIR", "/srv/other-checkout/.kuiper-data")
    other = get_in(config(), [:kuiper_coordinator, :management, :socket])
    assert other != socket and byte_size(other) <= 103

    System.put_env("KUIPER_DATA_DIR", "/srv/kf")
    System.put_env("KUIPER_CA_ORG", "Acme")
    assert agent_grpc()[:ca_key] == "/srv/kf/ca.key"
    assert agent_grpc()[:ca_org] == "Acme"
  end

  # The Rust coordinator's env block (its Docker image's ENV plus the prod StatefulSet's settings:
  # TLS files on /data, PROXY protocol behind Traefik's TLSRoute, SQLite) with only what Phoenix
  # needs on top of it (DATABASE_URL, SECRET_KEY_BASE, KUIPER_ENCRYPTION_KEY) (D72, D73).
  @rust_prod_env %{
    "KUIPER_TLS__CA_CERT" => "/data/ca.crt",
    "KUIPER_TLS__CA_KEY" => "/data/ca.key",
    "KUIPER_TLS__SERVER_CERT" => "/data/server.crt",
    "KUIPER_TLS__SERVER_KEY" => "/data/server.key",
    "KUIPER_TLS__SERVER_TRUST_MODE" => "chain",
    "KUIPER_TLS__SERVER_USE_NATIVE_ROOTS" => "true",
    "KUIPER_GRPC__LISTEN_ADDR" => "0.0.0.0:9443",
    "KUIPER_GRPC__PROXY_PROTOCOL" => "true",
    "KUIPER_ADMIN__COORDINATOR_URL" => "https://kuiper.example.com",
    "KUIPER_ADMIN__SESSION_TIMEOUT_SECS" => "7200",
    "KUIPER_ADMIN__UPDATE_CHECK" => "false",
    "KUIPER_METRICS__RETENTION_DAYS" => "30",
    "KUIPER_DATABASE__PATH" => "/data/kuiper.db",
    "KUIPER_NO_LOG_FILE" => "true"
  }

  defp put_prod_env(extra \\ %{}) do
    Map.merge(
      %{
        "DATABASE_URL" => "ecto://kuiper:pw@pg/kuiper_phoenix",
        "SECRET_KEY_BASE" => String.duplicate("s", 64),
        "KUIPER_ENCRYPTION_KEY" => Base.encode64(:crypto.strong_rand_bytes(32))
      },
      Map.merge(@rust_prod_env, extra)
    )
    |> Enum.each(fn
      {k, nil} -> System.delete_env(k)
      {k, v} -> System.put_env(k, v)
    end)
  end

  test "the Rust coordinator's prod env block configures Phoenix (D73)" do
    put_prod_env()
    app = config(:prod)[:kuiper_coordinator]
    grpc = app[:agent_grpc]

    assert grpc[:ca_cert] == "/data/ca.crt"
    assert grpc[:ca_key] == "/data/ca.key"
    assert grpc[:server_cert] == "/data/server.crt"
    assert grpc[:server_key] == "/data/server.key"
    assert grpc[:server_trust_mode] == "chain"
    assert grpc[:server_use_native_roots] == true
    assert grpc[:proxy_protocol] == true
    # Rust's listen address keeps the agent port the deployment publishes (Codex P1)
    assert grpc[:port] == 9443
    assert grpc[:ip] == {0, 0, 0, 0}
    # the data dir isn't derived from the TLS paths, which may be read-only mounts (Codex P1)
    assert grpc[:data_dir] == "/var/lib/kuiper-forge"
    assert app[:admin][:coordinator_url] == "https://kuiper.example.com"
    assert app[:admin][:session_timeout_secs] == 7200
    assert app[:admin][:update_check] == false
    assert app[:metrics][:retention_days] == 30
    assert app[KuiperCoordinator.Repo][:url] == "ecto://kuiper:pw@pg/kuiper_phoenix"

    assert {"KUIPER_TLS__SERVER_TRUST_MODE", "KUIPER_SERVER_TRUST_MODE"} in app[:env_aliases]
    assert {"KUIPER_GRPC__PROXY_PROTOCOL", "KUIPER_AGENT_PROXY_PROTOCOL"} in app[:env_aliases]
    # every name in the block that has a Phoenix name (no server CA cert in it), and the listen
    # address
    assert length(app[:env_aliases]) == 12
  end

  test "Phoenix's own env names win over the Rust aliases" do
    put_prod_env(%{
      "KUIPER_CA_CERT" => "/phx/ca.crt",
      "KUIPER_SERVER_TRUST_MODE" => "ca",
      "KUIPER_AGENT_PROXY_PROTOCOL" => "false",
      "KUIPER_COORDINATOR_URL" => "https://phx.example.com"
    })

    app = config(:prod)[:kuiper_coordinator]
    assert app[:agent_grpc][:ca_cert] == "/phx/ca.crt"
    assert app[:agent_grpc][:ca_key] == "/data/ca.key"
    assert app[:agent_grpc][:server_trust_mode] == "ca"
    assert app[:agent_grpc][:proxy_protocol] == false
    assert app[:admin][:coordinator_url] == "https://phx.example.com"
    refute List.keymember?(app[:env_aliases], "KUIPER_TLS__CA_CERT", 0)
    assert List.keymember?(app[:env_aliases], "KUIPER_TLS__CA_KEY", 0)
  end

  test "KUIPER_AGENT_PORT wins over Rust's listen address" do
    put_prod_env(%{
      "KUIPER_AGENT_PORT" => "9544",
      "KUIPER_DATA_DIR" => "/srv/kc",
      "KUIPER_GRPC__LISTEN_ADDR" => "127.0.0.1:9443"
    })

    app = config(:prod)[:kuiper_coordinator]
    assert app[:agent_grpc][:port] == 9544
    assert app[:agent_grpc][:ip] == {0, 0, 0, 0}
    assert app[:agent_grpc][:data_dir] == "/srv/kc"
    refute List.keymember?(app[:env_aliases], "KUIPER_GRPC__LISTEN_ADDR", 0)
  end

  test "Rust's listen address takes IPv6, and a bad one stops boot" do
    put_prod_env(%{"KUIPER_GRPC__LISTEN_ADDR" => "[::]:9443"})
    grpc = config(:prod)[:kuiper_coordinator][:agent_grpc]
    assert grpc[:port] == 9443
    assert grpc[:ip] == {0, 0, 0, 0, 0, 0, 0, 0}

    for bad <- ["9443", "localhost:9443", "0.0.0.0:http"] do
      System.put_env("KUIPER_GRPC__LISTEN_ADDR", bad)
      assert_raise RuntimeError, ~r/KUIPER_GRPC__LISTEN_ADDR/, fn -> config(:prod) end
      # KUIPER_AGENT_PORT overrides a stale one, which isn't read then (Codex P2)
      System.put_env("KUIPER_AGENT_PORT", "9544")
      assert config(:prod)[:kuiper_coordinator][:agent_grpc][:port] == 9544
      System.delete_env("KUIPER_AGENT_PORT")
    end
  end

  test "the default coordinator URL uses the agent port, Rust's listen address's too (Codex P1)" do
    assert get_in(config(), [:kuiper_coordinator, :admin, :coordinator_url]) ==
             "https://localhost:9543"

    put_prod_env(%{"KUIPER_ADMIN__COORDINATOR_URL" => nil})

    assert get_in(config(:prod), [:kuiper_coordinator, :admin, :coordinator_url]) ==
             "https://localhost:9443"

    System.put_env("KUIPER_AGENT_PORT", "9601")

    assert get_in(config(:prod), [:kuiper_coordinator, :admin, :coordinator_url]) ==
             "https://localhost:9601"
  end

  test "PROXY protocol is off unless asked for" do
    refute agent_grpc()[:proxy_protocol]
    System.put_env("KUIPER_AGENT_PROXY_PROTOCOL", "1")
    assert agent_grpc()[:proxy_protocol]
    assert config()[:kuiper_coordinator][:env_aliases] == []
  end

  test "Rust env values are trimmed as figment trims them: ' true\\n' turns PROXY on (F22)" do
    put_prod_env(%{
      "KUIPER_GRPC__PROXY_PROTOCOL" => " true\n",
      "KUIPER_TLS__SERVER_USE_NATIVE_ROOTS" => "true ",
      "KUIPER_ADMIN__SESSION_TIMEOUT_SECS" => " 900\n",
      "KUIPER_METRICS__RETENTION_DAYS" => "30 ",
      "KUIPER_TLS__CA_CERT" => "/data/ca.crt\n"
    })

    app = config(:prod)[:kuiper_coordinator]
    assert app[:agent_grpc][:proxy_protocol] == true
    assert app[:agent_grpc][:server_use_native_roots] == true
    assert app[:agent_grpc][:ca_cert] == "/data/ca.crt"
    assert app[:admin][:session_timeout_secs] == 900
    assert app[:metrics][:retention_days] == 30

    System.put_env("KUIPER_GRPC__PROXY_PROTOCOL", "false\n")
    refute config(:prod)[:kuiper_coordinator][:agent_grpc][:proxy_protocol]
  end

  test "Rust's Postgres settings make a DATABASE_URL for a new database beside Rust's (D75)" do
    put_prod_env(%{
      "DATABASE_URL" => nil,
      "KUIPER_DATABASE__PATH" => nil,
      "KUIPER_DATABASE__HOST" => "postgres",
      "KUIPER_DATABASE__PORT" => "5433",
      "KUIPER_DATABASE__USER" => "kuiper",
      "KUIPER_DATABASE__PASSWORD" => "p@ss:w/rd+1",
      "KUIPER_DATABASE__DATABASE" => "kuiper_forge"
    })

    app = config(:prod)[:kuiper_coordinator]
    url = app[KuiperCoordinator.Repo][:url]
    assert url == "ecto://kuiper:p%40ss%3Aw%2Frd%2B1@postgres:5433/kuiper_forge_phoenix"
    # the URL Ecto parses back gives the password as it was
    assert Ecto.Repo.Supervisor.parse_url(url)[:password] == "p@ss:w/rd+1"
    assert {"KUIPER_DATABASE__*", "DATABASE_URL"} in app[:env_aliases]

    System.put_env("KUIPER_PHOENIX_DATABASE", "kc")
    assert config(:prod)[:kuiper_coordinator][KuiperCoordinator.Repo][:url] =~ ~r{:5433/kc$}

    System.put_env("DATABASE_URL", "ecto://u@h/own")
    app = config(:prod)[:kuiper_coordinator]
    assert app[KuiperCoordinator.Repo][:url] == "ecto://u@h/own"
    refute List.keymember?(app[:env_aliases], "KUIPER_DATABASE__*", 0)
  end

  test "an IPv6 KUIPER_DATABASE__HOST goes in brackets and gets an inet6 socket (Codex)" do
    put_prod_env(%{
      "DATABASE_URL" => nil,
      "KUIPER_DATABASE__PATH" => nil,
      "KUIPER_DATABASE__HOST" => "2001:db8::10",
      "KUIPER_DATABASE__PASSWORD" => "pw"
    })

    repo = config(:prod)[:kuiper_coordinator][KuiperCoordinator.Repo]
    assert repo[:url] == "ecto://kuiper:pw@[2001:db8::10]:5432/kuiper_forge_phoenix"
    parsed = Ecto.Repo.Supervisor.parse_url(repo[:url])
    assert parsed[:hostname] == "2001:db8::10"
    assert parsed[:port] == 5432
    assert repo[:socket_options] == [:inet6]

    # a name or an IPv4 address keeps the default socket
    System.put_env("KUIPER_DATABASE__HOST", "10.0.0.5")
    repo = config(:prod)[:kuiper_coordinator][KuiperCoordinator.Repo]
    assert repo[:url] =~ "@10.0.0.5:5432/"
    assert repo[:socket_options] == []
  end

  test "Rust's SQLite path isn't a database for Phoenix: without DATABASE_URL boot stops" do
    put_prod_env(%{"DATABASE_URL" => nil})
    assert_raise RuntimeError, ~r/DATABASE_URL is missing.*SQLite/s, fn -> config(:prod) end
  end

  test "the aliases used are logged at boot by name, never by value" do
    put_prod_env()
    aliases = config(:prod)[:kuiper_coordinator][:env_aliases]
    old = Application.get_env(:kuiper_coordinator, :env_aliases)
    Application.put_env(:kuiper_coordinator, :env_aliases, aliases)
    on_exit(fn -> Application.put_env(:kuiper_coordinator, :env_aliases, old || []) end)

    # tests log at :warning; the boot log is :info
    level = Logger.level()
    Logger.configure(level: :info)
    on_exit(fn -> Logger.configure(level: level) end)

    log =
      ExUnit.CaptureLog.capture_log([level: :info], fn ->
        KuiperCoordinator.Application.log_env_aliases()
      end)

    assert log =~ "KUIPER_TLS__CA_CERT for KUIPER_CA_CERT"
    assert log =~ "KUIPER_GRPC__PROXY_PROTOCOL for KUIPER_AGENT_PROXY_PROTOCOL"
    refute log =~ "/data/"
    refute log =~ "kuiper.example.com"
  end
end
