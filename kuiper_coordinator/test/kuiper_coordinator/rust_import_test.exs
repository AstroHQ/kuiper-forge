defmodule KuiperCoordinator.RustImportTest do
  # the import writes several tables and the mix task shares the sandbox
  use KuiperCoordinator.DataCase, async: false

  import ExUnit.CaptureIO
  import ExUnit.CaptureLog

  require Logger

  alias Exqlite.Sqlite3
  alias KuiperCoordinator.{Admin, Agents, Forges, RustImport}
  alias KuiperCoordinator.Admin.ApiTokens
  alias KuiperCoordinator.Agents.{Agent, RegistrationToken}

  @rust_migrations Path.expand("../../../kuiper-forge/migrations", __DIR__)

  # Argon2id with the argon2 crate's defaults (m=19456, t=2, p=1), which is what
  # kuiper-forge/src/admin/auth.rs hash_password stores
  @rust_admin_hash "$argon2id$v=19$m=19456,t=2,p=1$jABA6itsVbD8znhv6ggkfg$We4MClQmjXKkpZbz8I+rZCRTZxyVuEHRTE1jKZSnk8k"
  @rust_admin_pw "rust-admin-pw-1"

  @api_token "kfapi_" <> String.duplicate("A1b2C3d4", 5)
  @live_token "reg_" <> String.duplicate("q", 32)
  @expired_token "reg_" <> String.duplicate("x", 32)
  @forgejo_token "fj-secret-token-0123456789"
  @webhook_secret "wh-secret-9876543210"

  setup do
    dir = Path.join(System.tmp_dir!(), "rust_import_#{System.unique_integer([:positive])}")
    File.mkdir_p!(dir)
    on_exit(fn -> File.rm_rf!(dir) end)

    db = Path.join(dir, "kuiper.db")
    build_rust_sqlite!(db)

    key = Path.join(dir, "app.pem")
    File.write!(key, X509.PrivateKey.new_rsa(2048) |> X509.PrivateKey.to_pem())

    config = Path.join(dir, "config.toml")

    File.write!(config, """
    [[forges]]
    name = "gh"
    type = "github"
    app_id = 4242
    private_key_path = "#{key}"

    [[forges]]
    name = "knope"
    type = "forgejo"
    url = "https://git.example.test/"
    token = "#{@forgejo_token}"
    scopes = ["lodge", "jfro/kuiper-forge"]

    [tls]
    ca_cert = "ca.crt"
    ca_key = "ca.key"
    server_cert = "server.crt"
    server_key = "server.key"

    [provisioning]
    mode = "webhook"
    idle_runner_timeout_secs = 900

    [provisioning.webhook]
    secret = "#{@webhook_secret}"
    path = "/gh-hook"
    forge = "gh"
    required_labels = ["self-hosted", "kuiper"]
    queued_scan_lookback_hours = 24
    label_mappings = [
      { labels = ["self-hosted", "macOS"], runner_scope = { type = "organization", name = "astro" } },
      { labels = ["linux"], runner_group = "ci", forge = "knope" },
    ]
    """)

    %{db: db, url: "sqlite://" <> db, config: config, dir: dir}
  end

  test "a dry run counts what it would import and writes nothing", %{url: url, config: config} do
    assert {:ok, report} = RustImport.run(from: url, config: config, dry_run: true)
    out = report |> RustImport.format_report() |> IO.iodata_to_binary()

    assert out =~ "dry run"
    assert out =~ ~r/agents: 2 new/
    assert out =~ ~r/registration tokens: 1 new, 1 skipped/
    assert out =~ ~r/admin users: 1 new/
    assert out =~ ~r/api tokens: 1 new/
    assert out =~ ~r/forges: 2 new/
    refute_secrets(out)

    assert Repo.aggregate(Agent, :count) == 0
    assert Repo.aggregate(RegistrationToken, :count) == 0
    assert Admin.list_users() == []
    assert ApiTokens.list() == []
    assert Forges.list_forges() == []
  end

  test "imports agents, tokens, admins, API tokens and forges as they were", %{
    url: url,
    config: config
  } do
    assert {:ok, report} = RustImport.run(from: url, config: config)
    refute_secrets(report |> RustImport.format_report() |> IO.iodata_to_binary())

    live = Repo.get!(Agent, "agent-live")
    assert live.hostname == "mac-mini-1"
    assert live.agent_type == "tart"
    assert live.labels == ["macos", "arm64"]
    assert live.max_vms == 2
    assert live.serial_number == "0a1b2c"
    assert live.revoked_at == nil
    assert live.agent_version == "0.9.1"
    assert DateTime.compare(live.expires_at, ~U[2027-11-01 00:00:00Z]) == :eq
    # the shared CA's cert keeps authenticating
    assert Agents.valid_agent?("agent-live")

    gone = Repo.get!(Agent, "agent-revoked")
    assert DateTime.compare(gone.revoked_at, ~U[2026-06-01 12:00:00Z]) == :eq
    refute Agents.valid_agent?("agent-revoked")

    assert Repo.get(RegistrationToken, @live_token)
    refute Repo.get(RegistrationToken, @expired_token)

    # the Rust Argon2id hash logs in on Phoenix
    assert {:ok, _sid, _user} = Admin.login("jeremy", @rust_admin_pw, %{})
    assert ApiTokens.validate(@api_token).name == "grafana"

    gh = Forges.get_forge_by_name("gh")
    assert gh.type == :github and gh.app_id == 4242
    assert gh.private_key =~ "PRIVATE KEY"
    assert gh.mode == :webhook and gh.webhook_secret == @webhook_secret

    fj = Forges.get_forge_by_name("knope")
    assert fj.type == :forgejo and fj.url == "https://git.example.test"
    assert fj.token == @forgejo_token and fj.scopes == ["lodge", "jfro/kuiper-forge"]
    assert fj.mode == :polling

    s = Forges.get_settings()
    assert s.idle_runner_timeout_s == 900
    assert s.webhook_path == "/gh-hook"
    assert s.required_labels == ["self-hosted", "kuiper"]
    assert s.scan_lookback_hours == 24

    assert [m1, m2] = Forges.list_label_mappings()
    assert m1.labels == ["self-hosted", "macOS"] and m1.runner_scope == "astro"
    assert m2.labels == ["linux"] and m2.runner_group == "ci" and m2.forge == "knope"
  end

  test "a rerun adds nothing, keeps Phoenix's changes and brings revocations over", %{
    db: db,
    url: url,
    config: config
  } do
    {:ok, _} = RustImport.run(from: url, config: config)

    # Phoenix moves on: the token is used, a password changes, an API token is deleted
    # (registering consumes a token by deleting its row)
    Repo.delete_all(RegistrationToken)
    :ok = Admin.set_password("jeremy", "phoenix-pw-22", "phoenix-pw-22")
    [api] = ApiTokens.list()
    ApiTokens.delete(api.id)
    # ...and the Rust coordinator revokes an agent
    sqlite!(
      db,
      "UPDATE registered_agents SET revoked = 1, revoked_at = '2026-11-20T08:00:00+00:00' WHERE agent_id = 'agent-live'"
    )

    assert {:ok, report} = RustImport.run(from: url, config: config)
    out = report |> RustImport.format_report() |> IO.iodata_to_binary()

    assert out =~ ~r/agents: 1 revoked, 1 unchanged/
    assert out =~ ~r/admin users: 1 conflict/
    assert out =~ "jeremy"
    assert out =~ ~r/forges: 2 unchanged/
    refute_secrets(out)

    # the used single-use token isn't brought back, nor the deleted API token
    refute Repo.get(RegistrationToken, @live_token)
    assert ApiTokens.list() == []
    assert {:ok, _, _} = Admin.login("jeremy", "phoenix-pw-22", %{})
    refute Agents.valid_agent?("agent-live")
    assert Repo.aggregate(Agent, :count) == 2
  end

  test "polling settings the Rust coordinator takes import and rerun clean", %{
    url: url,
    dir: dir
  } do
    config = Path.join(dir, "polling.toml")
    System.put_env("RUST_IMPORT_TEST_FJ_TOKEN", @forgejo_token)
    on_exit(fn -> System.delete_env("RUST_IMPORT_TEST_FJ_TOKEN") end)

    # lookback 0 is "scan every repo"; an idle timeout under 120 s is raised to 120 by both
    File.write!(config, """
    [[forges]]
    name = "knope"
    type = "forgejo"
    url = "https://git.example.test"
    token_env = "RUST_IMPORT_TEST_FJ_TOKEN"
    scopes = ["@jfro"]

    [provisioning]
    mode = "polling"
    idle_runner_timeout_secs = 60

    [provisioning.polling]
    interval_secs = 30
    queued_scan_lookback_hours = 0
    """)

    assert {:ok, _} = RustImport.run(from: url, config: config)
    assert %{scan_lookback_hours: 0, idle_runner_timeout_s: 120} = Forges.get_settings()
    fj = Forges.get_forge_by_name("knope")
    assert fj.mode == :polling and fj.poll_interval_s == 30 and fj.token == @forgejo_token

    assert {:ok, report} = RustImport.run(from: url, config: config)
    refute RustImport.conflicts?(report)
    out = report |> RustImport.format_report() |> IO.iodata_to_binary()
    assert out =~ "provisioning settings: 1 unchanged"
  end

  test "bad inputs give clear errors", %{dir: dir, url: url} do
    assert {:error, msg} = RustImport.run(from: "sqlite://" <> Path.join(dir, "nope.db"))
    assert msg =~ "no such file"

    assert {:error, msg} = RustImport.run(from: "mysql://db/kuiper")
    assert msg =~ "sqlite://"

    other = Path.join(dir, "other.db")
    sqlite!(other, "CREATE TABLE t (x INTEGER)")
    assert {:error, msg} = RustImport.run(from: "sqlite://" <> other)
    assert msg =~ "registered_agents"

    bad = Path.join(dir, "bad.toml")
    File.write!(bad, "[[forges]\nname=")
    assert {:error, msg} = RustImport.run(from: url, config: bad)
    assert msg =~ "bad.toml"

    assert {:error, msg} = RustImport.run(from: url, config: Path.join(dir, "missing.toml"))
    assert msg =~ "missing.toml"

    capture_log(fn ->
      assert {:error, msg} =
               RustImport.run(from: "postgres://kuiper:hunter2@127.0.0.1:1/kuiper")

      send(self(), {:msg, msg})
    end)

    assert_received {:msg, msg}
    assert msg =~ "127.0.0.1"

    refute msg =~ "hunter2"
  end

  test "mix kuiper.import_rust runs the import", %{url: url, config: config} do
    out =
      capture_io(fn ->
        Mix.Tasks.Kuiper.ImportRust.run(["--from", url, "--config", config, "--dry-run"])
      end)

    assert out =~ ~r/agents: 2 new/
    refute_secrets(out)
    assert Repo.aggregate(Agent, :count) == 0

    assert_raise Mix.Error, ~r/--from/, fn -> Mix.Tasks.Kuiper.ImportRust.run([]) end
  end

  test "the import keeps secrets out of the debug log", %{url: url, config: config} do
    level = Logger.level()
    Logger.configure(level: :debug)
    on_exit(fn -> Logger.configure(level: level) end)

    log =
      capture_log([level: :debug], fn ->
        out =
          capture_io(fn ->
            Mix.Tasks.Kuiper.ImportRust.run(["--from", url, "--config", config])
          end)

        refute_secrets(out)
      end)

    refute_secrets(log)
    assert Forges.get_forge_by_name("knope").token == @forgejo_token
  end

  # astro-remote-infrastructure kuiper-forge/deployment.yaml: a SQLite StatefulSet with no
  # forge config in config.toml, everything from KUIPER_X__Y env (config.rs Config::load)
  @prod_env %{
    "KUIPER_GITHUB__APP_ID" => "4242",
    "KUIPER_RUNNER_SCOPE__TYPE" => "organization",
    "KUIPER_RUNNER_SCOPE__NAME" => "AstroHQ",
    "KUIPER_PROVISIONING__MODE" => "webhook",
    "KUIPER_PROVISIONING__WEBHOOK__SECRET" => @webhook_secret,
    "KUIPER_GRPC__PROXY_PROTOCOL" => "true"
  }

  defp put_env!(env) do
    old = Map.new(env, fn {k, _} -> {k, System.get_env(k)} end)
    System.put_env(env)

    on_exit(fn ->
      for {k, v} <- old, do: if(v, do: System.put_env(k, v), else: System.delete_env(k))
    end)
  end

  test "an env-only Rust config (the prod k8s shape) imports its GitHub forge", %{db: db} do
    pem = X509.PrivateKey.new_rsa(2048) |> X509.PrivateKey.to_pem()
    put_env!(Map.put(@prod_env, "KUIPER_GITHUB__PRIVATE_KEY", pem))

    dry =
      capture_io(fn -> Mix.Tasks.Kuiper.ImportRust.run(["--sqlite", db, "--dry-run"]) end)

    assert dry =~ ~r/agents: 2 new/
    assert dry =~ ~r/forges: 1 new/
    refute_secrets(dry)
    assert Forges.list_forges() == []

    out = capture_io(fn -> Mix.Tasks.Kuiper.ImportRust.run(["--sqlite", db]) end)
    refute_secrets(out)

    assert [gh] = Forges.list_forges()
    assert gh.name == "github" and gh.type == :github and gh.app_id == 4242
    # figment trims env values, so Rust reads the PEM without its trailing newlines
    assert gh.private_key == String.trim(pem)
    assert {:ok, _} = X509.PrivateKey.from_pem(gh.private_key)
    assert gh.mode == :webhook and gh.webhook_secret == @webhook_secret
    assert Repo.get!(Agent, "agent-live").hostname == "mac-mini-1"
    assert Agents.valid_agent?("agent-live")

    # a rerun is clean
    again = capture_io(fn -> Mix.Tasks.Kuiper.ImportRust.run(["--sqlite", db]) end)
    assert again =~ ~r/forges: 1 unchanged/
    refute again =~ "conflict"
  end

  test "Rust env overrides win over config.toml, nested and typed", %{dir: dir} do
    key = Path.join(dir, "app.pem")
    config = Path.join(dir, "partial.toml")

    File.write!(config, """
    [github]
    app_id = 1
    private_key_path = "#{key}"

    [provisioning]
    mode = "polling"
    """)

    put_env!(%{
      "KUIPER_GITHUB__APP_ID" => "4242",
      "KUIPER_PROVISIONING__POLLING__INTERVAL_SECS" => "300",
      "KUIPER_PROVISIONING__IDLE_RUNNER_TIMEOUT_SECS" => "900"
    })

    assert {:ok, cfg} = RustImport.Config.load(config)
    assert [%{name: "github", app_id: 4242, mode: :polling, poll_interval_s: 300}] = cfg.forges
    assert cfg.settings.idle_runner_timeout_s == 900

    # without --config the env alone is the config; with no Rust forge env there's none
    assert {:ok, %{forges: [%{app_id: 4242}]}} =
             RustImport.Config.load(nil, %{
               "KUIPER_GITHUB__APP_ID" => "4242",
               "KUIPER_GITHUB__PRIVATE_KEY" => File.read!(key),
               "KUIPER_PROVISIONING__MODE" => "polling"
             })

    assert {:ok, nil} = RustImport.Config.load(nil, %{"KUIPER_CA_ORG" => "KuiperOrg"})
  end

  test "the release command takes the same flags" do
    assert {:ok, opts} =
             RustImport.parse_args(["--sqlite", "/data/kuiper.db", "--config", "/data/c.toml"])

    assert opts[:from] == "sqlite:///data/kuiper.db" and opts[:config] == "/data/c.toml"

    assert {:ok, [from: "postgres://h/db", dry_run: true]} =
             RustImport.parse_args(["--from", "postgres://h/db", "--dry-run"])

    assert {:error, msg} = RustImport.parse_args(["--sqlite", "a.db", "--from", "sqlite://b"])
    assert msg =~ "either"
    assert {:error, _} = RustImport.parse_args(["--bogus"])
    assert {:error, msg} = RustImport.parse_args([])
    assert msg =~ "--sqlite"

    script = Path.expand("../../rel/overlays/bin/import_rust", __DIR__)
    assert File.read!(script) =~ "KuiperCoordinator.Release.import_rust_cli"
  end

  test "reads a Rust Postgres database too" do
    cfg = Repo.config()
    db = "kuiper_rust_src#{System.get_env("MIX_TEST_PARTITION")}"
    admin = Keyword.merge(cfg, database: "postgres", pool_size: 1) |> Keyword.delete(:pool)
    {:ok, conn} = Postgrex.start_link(admin)
    Postgrex.query!(conn, ~s(DROP DATABASE IF EXISTS "#{db}"), [])
    Postgrex.query!(conn, ~s(CREATE DATABASE "#{db}"), [])

    on_exit(fn ->
      {:ok, c} = Postgrex.start_link(admin)
      Postgrex.query!(c, ~s{DROP DATABASE IF EXISTS "#{db}" WITH (FORCE)}, [])
    end)

    {:ok, src} = Postgrex.start_link(Keyword.put(admin, :database, db))

    # the extended protocol takes one statement at a time (no migration here has a $$ body)
    for file <- rust_migration_sql("postgres") ++ fixture_rows(),
        sql <- String.split(file, ~r/;\s*$/m),
        sql |> String.replace(~r/^\s*--.*$/m, "") |> String.trim() != "" do
      Postgrex.query!(src, sql, [])
    end

    GenServer.stop(src)
    GenServer.stop(conn)

    url =
      "postgres://#{cfg[:username]}:#{cfg[:password]}@#{cfg[:hostname]}:#{cfg[:port] || 5432}/#{db}"

    assert {:ok, report} = RustImport.run(from: url)
    out = report |> RustImport.format_report() |> IO.iodata_to_binary()
    refute out =~ ":#{cfg[:password]}@"
    assert out =~ ~r/agents: 2 new/
    assert Repo.get!(Agent, "agent-revoked").revoked_at
    assert {:ok, _, _} = Admin.login("jeremy", @rust_admin_pw, %{})
  end

  defp refute_secrets(out) do
    for s <- [
          @api_token,
          @live_token,
          @expired_token,
          @forgejo_token,
          @webhook_secret,
          @rust_admin_hash
        ],
        do: refute(out =~ s)

    refute out =~ "PRIVATE KEY"
  end

  # the Rust coordinator's schema, made the way sqlx does it: shared migrations, then sqlite's
  defp build_rust_sqlite!(path) do
    for sql <- rust_migration_sql("sqlite") ++ fixture_rows(), do: sqlite!(path, sql)
  end

  defp rust_migration_sql(backend) do
    for dir <- ["shared", backend],
        file <- Path.wildcard(Path.join([@rust_migrations, dir, "*.sql"])) |> Enum.sort() do
      File.read!(file)
    end
  end

  defp fixture_rows do
    future = "2027-11-01T00:00:00+00:00"

    [
      """
      INSERT INTO registered_agents (agent_id, hostname, agent_type, labels, max_vms, serial_number,
        created_at, expires_at, revoked, revoked_at, agent_version)
      VALUES ('agent-live', 'mac-mini-1', 'tart', '["macos","arm64"]', 2, '0a1b2c',
        '2026-01-02T03:04:05.123456789+00:00', '#{future}', 0, NULL, '0.9.1'),
      ('agent-revoked', 'pve-1', 'proxmox', '["linux"]', 4, 'ffee01',
        '2026-01-01T00:00:00+00:00', '#{future}', 1, '2026-06-01T12:00:00+00:00', NULL)
      """,
      """
      INSERT INTO registration_tokens (token, expires_at, created_by, created_at)
      VALUES ('#{@live_token}', '#{future}', 'jeremy', '2026-11-01T00:00:00+00:00'),
      ('#{@expired_token}', '2026-01-01T00:00:00+00:00', 'jeremy', '2025-12-31T00:00:00+00:00')
      """,
      """
      INSERT INTO admin_users (username, password_hash, totp_secret, created_at, last_login)
      VALUES ('jeremy', '#{@rust_admin_hash}', NULL, '2026-01-01T00:00:00+00:00', '2026-11-20T10:00:00+00:00')
      """,
      """
      INSERT INTO api_tokens (id, name, token_hash, token_prefix, created_by, created_at, last_used_at)
      VALUES ('8c1f0b9e-2f7a-4d3c-9a51-0d6c1e2b3a4f', 'grafana',
        '#{:crypto.hash(:sha256, @api_token) |> Base.encode16(case: :lower)}',
        '#{binary_part(@api_token, 0, 12)}', 'jeremy', '2026-09-23T00:00:00+00:00', NULL)
      """,
      """
      INSERT INTO admin_sessions (session_id, username, created_at, expires_at)
      VALUES ('s1', 'jeremy', '2026-11-20T10:00:00+00:00', '#{future}')
      """
    ]
  end

  defp sqlite!(path, sql) do
    {:ok, db} = Sqlite3.open(path)
    :ok = Sqlite3.execute(db, sql)
    :ok = Sqlite3.close(db)
  end
end
