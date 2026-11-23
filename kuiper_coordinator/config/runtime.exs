import Config

# config/runtime.exs is executed for all environments, including
# during releases. It is executed after compilation and before the
# system starts, so it is typically used to load production configuration
# and secrets from environment variables or elsewhere. Do not define
# any compile-time configuration in here, as it won't be applied.
# The block below contains prod specific runtime configuration.

# ## Using releases
#
# If you use `mix release`, you need to explicitly enable the server
# by passing the PHX_SERVER=true when you start it:
#
#     PHX_SERVER=true bin/kuiper_coordinator start
#
# Alternatively, you can use `mix phx.gen.release` to generate a `bin/server`
# script that automatically sets the env var above.
# The Rust coordinator's env names (figment's KUIPER_<SECTION>__<KEY>) work as aliases for
# Phoenix's own, so a Rust deployment's env block (Docker, k8s) carries over unchanged (D73).
# Phoenix's own name wins when both are set. The aliases used are logged at boot, never values.
rust_env_aliases = [
  {"KUIPER_CA_CERT", "KUIPER_TLS__CA_CERT"},
  {"KUIPER_CA_KEY", "KUIPER_TLS__CA_KEY"},
  {"KUIPER_SERVER_CERT", "KUIPER_TLS__SERVER_CERT"},
  {"KUIPER_SERVER_KEY", "KUIPER_TLS__SERVER_KEY"},
  {"KUIPER_SERVER_CA_CERT", "KUIPER_TLS__SERVER_CA_CERT"},
  {"KUIPER_SERVER_TRUST_MODE", "KUIPER_TLS__SERVER_TRUST_MODE"},
  {"KUIPER_SERVER_USE_NATIVE_ROOTS", "KUIPER_TLS__SERVER_USE_NATIVE_ROOTS"},
  {"KUIPER_AGENT_PROXY_PROTOCOL", "KUIPER_GRPC__PROXY_PROTOCOL"},
  {"KUIPER_COORDINATOR_URL", "KUIPER_ADMIN__COORDINATOR_URL"},
  {"KUIPER_ADMIN_SESSION_TIMEOUT", "KUIPER_ADMIN__SESSION_TIMEOUT_SECS"},
  {"KUIPER_UPDATE_CHECK", "KUIPER_ADMIN__UPDATE_CHECK"},
  {"KUIPER_METRICS_RETENTION_DAYS", "KUIPER_METRICS__RETENTION_DAYS"}
]

env = fn name, default ->
  case List.keyfind(rust_env_aliases, name, 0) do
    {^name, rust} -> System.get_env(name) || System.get_env(rust) || default
    nil -> System.get_env(name) || default
  end
end

# Rust's Postgres settings (KUIPER_DATABASE__HOST/PORT/USER/PASSWORD/DATABASE) make a
# DATABASE_URL when it's unset, for a new database on the same server (D75): Phoenix's own
# tables must not land in the Rust coordinator's, so the name is KUIPER_PHOENIX_DATABASE, else
# the Rust database's name with _phoenix after it. Rust's SQLite (KUIPER_DATABASE__PATH) has
# no server to share, so it isn't an alias.
rust_database_url =
  if (host = System.get_env("KUIPER_DATABASE__HOST")) && !System.get_env("DATABASE_URL") do
    userinfo =
      [
        System.get_env("KUIPER_DATABASE__USER", "kuiper"),
        System.get_env("KUIPER_DATABASE__PASSWORD")
      ]
      |> Enum.reject(&is_nil/1)
      |> Enum.map_join(":", &URI.encode(&1, fn c -> URI.char_unreserved?(c) end))

    database =
      System.get_env("KUIPER_PHOENIX_DATABASE") ||
        System.get_env("KUIPER_DATABASE__DATABASE", "kuiper_forge") <> "_phoenix"

    port = System.get_env("KUIPER_DATABASE__PORT", "5432")
    "ecto://#{userinfo}@#{host}:#{port}/#{URI.encode(database, &URI.char_unreserved?/1)}"
  end

config :kuiper_coordinator,
       :env_aliases,
       for(
         {phoenix, rust} <- rust_env_aliases,
         System.get_env(rust) && !System.get_env(phoenix),
         do: {rust, phoenix}
       ) ++
         if(rust_database_url, do: [{"KUIPER_DATABASE__*", "DATABASE_URL"}], else: [])

if System.get_env("PHX_SERVER") do
  config :kuiper_coordinator, KuiperCoordinatorWeb.Endpoint, server: true
end

# HTTP port (admin UI, webhooks). Its own port, so it runs beside the Rust
# coordinator (which listens on 9443) without clashing: KUIPER_HTTP_PORT,
# else PORT, else 9480.
http_port = System.get_env("KUIPER_HTTP_PORT") || System.get_env("PORT") || "9480"

config :kuiper_coordinator, KuiperCoordinatorWeb.Endpoint,
  http: [port: String.to_integer(http_port)]

# Agent gRPC (RegistrationService + AgentService over TLS, optional client cert) on its own
# TLS listener (D21-D23): KUIPER_AGENT_PORT, default 9543, beside the Rust coordinator on 9443.
# Point the CA at the Rust coordinator's ca.crt/ca.key so existing agent certs stay valid.
# The server cert may come from another CA (KUIPER_SERVER_TRUST_MODE=chain, e.g. a public
# cert with its chain); agent client certs are always verified against KUIPER_CA_CERT.
# A fresh install makes its own CA and server cert in the data dir on first launch (D57); files
# that are already there (like the Rust coordinator's) are used as-is. Dev keeps them in
# kuiper_coordinator/.kuiper-data (gitignored), so it runs without sudo.
if config_env() != :test do
  default_data_dir =
    if config_env() == :dev,
      do: Path.expand("../.kuiper-data", __DIR__),
      else: "/var/lib/kuiper-forge"

  data_dir = System.get_env("KUIPER_DATA_DIR", default_data_dir)
  # on unless KUIPER_AGENT_GRPC is false or 0 (D58)
  agent_grpc? = System.get_env("KUIPER_AGENT_GRPC", "true") not in ~w(false 0)

  config :kuiper_coordinator, :agent_grpc,
    enabled: agent_grpc?,
    data_dir: data_dir,
    ca_org: System.get_env("KUIPER_CA_ORG", "Kuiper Forge"),
    port: String.to_integer(System.get_env("KUIPER_AGENT_PORT", "9543")),
    ca_cert: env.("KUIPER_CA_CERT", Path.join(data_dir, "ca.crt")),
    ca_key: env.("KUIPER_CA_KEY", Path.join(data_dir, "ca.key")),
    server_cert: env.("KUIPER_SERVER_CERT", Path.join(data_dir, "server.crt")),
    server_key: env.("KUIPER_SERVER_KEY", Path.join(data_dir, "server.key")),
    server_ca_cert: env.("KUIPER_SERVER_CA_CERT", nil),
    server_use_native_roots: env.("KUIPER_SERVER_USE_NATIVE_ROOTS", "false") in ~w(true 1),
    server_trust_mode: env.("KUIPER_SERVER_TRUST_MODE", "ca"),
    # optional PROXY v1/v2 header before TLS, for a TLS-passthrough balancer like Traefik's
    # TLSRoute (Rust grpc.proxy_protocol, A3); direct connections still work with it on
    proxy_protocol: env.("KUIPER_AGENT_PROXY_PROTOCOL", "false") in ~w(true 1),
    # the name agents and browsers use; the server cert is checked against it at startup
    server_hostname: System.get_env("KUIPER_SERVER_HOSTNAME") || System.get_env("PHX_HOST")

  # Management gRPC (management.proto) for the CLI's token/agent commands, on a Unix socket.
  # Its own path beside the Rust coordinator's <data_dir>/coordinator.sock (parity M6). On
  # with agent gRPC unless KUIPER_MANAGEMENT says otherwise (tokens are for registration).
  config :kuiper_coordinator, :management,
    enabled: System.get_env("KUIPER_MANAGEMENT", to_string(agent_grpc?)) in ~w(true 1),
    # dev's data dir sits deep in the checkout, too long for a Unix socket path (macOS allows
    # 103 bytes), so dev's socket goes in the temp dir, named from a hash of the data dir so two
    # checkouts (or two users on a shared /tmp) don't collide
    socket:
      System.get_env("KUIPER_MANAGEMENT_SOCKET") ||
        if(config_env() == :dev,
          do:
            Path.join(
              System.tmp_dir!(),
              "kuiper-phoenix-#{:crypto.hash(:sha256, data_dir) |> Base.encode16(case: :lower) |> binary_part(0, 12)}.sock"
            ),
          else: Path.join(data_dir, "phoenix-coordinator.sock")
        )
end

# Forges and provisioning settings live in Postgres and are edited in the admin UI (D39); the
# secrets among them (GitHub App keys, Forgejo tokens, webhook secrets) are encrypted with
# KUIPER_ENCRYPTION_KEY (32 bytes, base64: openssl rand -base64 32). Required in prod; dev
# falls back to a fixed key, tests set their own.
case {config_env(), System.get_env("KUIPER_ENCRYPTION_KEY")} do
  {:test, _} ->
    :ok

  {env, key} when key in [nil, ""] and env != :prod ->
    config :kuiper_coordinator, KuiperCoordinator.Vault,
      key: Base.decode64!("ZGV2LW9ubHkta2V5LWRvLW5vdC11c2UtaW4tcHJvZCE=")

  {:prod, key} when key in [nil, ""] ->
    raise """
    environment variable KUIPER_ENCRYPTION_KEY is missing.
    It encrypts stored forge secrets; make one with: openssl rand -base64 32
    """

  {_, key} ->
    case Base.decode64(String.trim(key)) do
      {:ok, bytes} when byte_size(bytes) == 32 ->
        config :kuiper_coordinator, KuiperCoordinator.Vault, key: bytes

      _ ->
        raise "KUIPER_ENCRYPTION_KEY must be 32 bytes, base64 (openssl rand -base64 32)"
    end
end

# Admin UI under /admin, always on (D49; Rust's [admin] enabled switch is dropped).
# Create the first user with `bin/kuiper_coordinator eval 'KuiperCoordinator.Release.create_admin("name")'`
# and KUIPER_ADMIN_PASSWORD set.
if config_env() != :test do
  config :kuiper_coordinator, :admin,
    session_timeout_secs: String.to_integer(env.("KUIPER_ADMIN_SESSION_TIMEOUT", "3600")),
    # agent-facing URL in registration bundles (Rust [admin] coordinator_url)
    coordinator_url: env.("KUIPER_COORDINATOR_URL", "https://localhost:9543"),
    # flag agents behind the newest GitHub release (Rust [admin] update_check, #75)
    # (KuiperCoordinator.AgentUpdates.enabled_from_env/1)
    update_check:
      String.downcase(String.trim(env.("KUIPER_UPDATE_CHECK", "true"))) not in ~w(0 false no off)

  # run history for the metrics page (parity C7/MX7: [metrics] retention_days 90)
  config :kuiper_coordinator, :metrics,
    retention_days: String.to_integer(env.("KUIPER_METRICS_RETENTION_DAYS", "90"))
end

if config_env() == :dev do
  # behind a tunnel (`mise run ngrok-server`) the public URL is the tunnel's, so the GitHub App
  # form and anything else built from `Endpoint.url/0` point there instead of at localhost
  if host = System.get_env("PHX_HOST") do
    config :kuiper_coordinator, KuiperCoordinatorWeb.Endpoint,
      url: [host: host, port: 443, scheme: "https"]
  end

  # Reload browser tabs when matching files change.
  config :kuiper_coordinator, KuiperCoordinatorWeb.Endpoint,
    live_reload: [
      web_console_logger: true,
      patterns: [
        # Static assets, except user uploads
        ~r"priv/static/(?!uploads/).*\.(js|css|png|jpeg|jpg|gif|svg)$"E,
        # Gettext translations
        ~r"priv/gettext/.*\.po$"E,
        # Router, Controllers, LiveViews and LiveComponents
        ~r"lib/kuiper_coordinator_web/router\.ex$"E,
        ~r"lib/kuiper_coordinator_web/(controllers|live|components)/.*\.(ex|heex)$"E
      ]
    ]
end

if config_env() == :prod do
  database_url =
    System.get_env("DATABASE_URL") || rust_database_url ||
      raise """
      environment variable DATABASE_URL is missing.
      For example: ecto://USER:PASS@HOST/DATABASE
      (or the Rust coordinator's KUIPER_DATABASE__HOST/PORT/USER/PASSWORD for a Postgres
      server; Phoenix can't use the Rust coordinator's SQLite file)
      """

  maybe_ipv6 = if System.get_env("ECTO_IPV6") in ~w(true 1), do: [:inet6], else: []

  config :kuiper_coordinator, KuiperCoordinator.Repo,
    # ssl: true,
    url: database_url,
    pool_size: String.to_integer(System.get_env("POOL_SIZE") || "10"),
    # For machines with several cores, consider starting multiple pools of `pool_size`
    # pool_count: 4,
    socket_options: maybe_ipv6

  # The secret key base is used to sign/encrypt cookies and other secrets.
  # A default value is used in config/dev.exs and config/test.exs but you
  # want to use a different value for prod and you most likely don't want
  # to check this value into version control, so we use an environment
  # variable instead.
  secret_key_base =
    System.get_env("SECRET_KEY_BASE") ||
      raise """
      environment variable SECRET_KEY_BASE is missing.
      You can generate one by calling: mix phx.gen.secret
      """

  host = System.get_env("PHX_HOST") || "example.com"

  config :kuiper_coordinator, :dns_cluster_query, System.get_env("DNS_CLUSTER_QUERY")

  config :kuiper_coordinator, KuiperCoordinatorWeb.Endpoint,
    url: [host: host, port: 443, scheme: "https"],
    http: [
      # Enable IPv6 and bind on all interfaces.
      # Set it to  {0, 0, 0, 0, 0, 0, 0, 1} for local network only access.
      # See the documentation on https://bandit.hexdocs.pm/Bandit.html#t:options/0
      # for details about using IPv6 vs IPv4 and loopback vs public addresses.
      ip: {0, 0, 0, 0, 0, 0, 0, 0}
    ],
    secret_key_base: secret_key_base

  # ## SSL Support
  #
  # To get SSL working, you will need to add the `https` key
  # to your endpoint configuration:
  #
  #     config :kuiper_coordinator, KuiperCoordinatorWeb.Endpoint,
  #       https: [
  #         ...,
  #         port: 443,
  #         cipher_suite: :strong,
  #         keyfile: System.get_env("SOME_APP_SSL_KEY_PATH"),
  #         certfile: System.get_env("SOME_APP_SSL_CERT_PATH")
  #       ]
  #
  # The `cipher_suite` is set to `:strong` to support only the
  # latest and more secure SSL ciphers. This means old browsers
  # and clients may not be supported. You can set it to
  # `:compatible` for wider support.
  #
  # `:keyfile` and `:certfile` expect an absolute path to the key
  # and cert in disk or a relative path inside priv, for example
  # "priv/ssl/server.key". For all supported SSL configuration
  # options, see https://plug.hexdocs.pm/Plug.SSL.html#configure/1
  #
  # We also recommend setting `force_ssl` in your config/prod.exs,
  # ensuring no data is ever sent via http, always redirecting to https:
  #
  #     config :kuiper_coordinator, KuiperCoordinatorWeb.Endpoint,
  #       force_ssl: [hsts: true]
  #
  # Check `Plug.SSL` for all available options in `force_ssl`.

  # ## Configuring the mailer
  #
  # In production you need to configure the mailer to use a different adapter.
  # Here is an example configuration for Mailgun:
  #
  #     config :kuiper_coordinator, KuiperCoordinator.Mailer,
  #       adapter: Swoosh.Adapters.Mailgun,
  #       api_key: System.get_env("MAILGUN_API_KEY"),
  #       domain: System.get_env("MAILGUN_DOMAIN")
  #
  # Most non-SMTP adapters require an API client. Swoosh supports Req, Hackney,
  # and Finch out-of-the-box. This configuration is typically done at
  # compile-time in your config/prod.exs:
  #
  #     config :swoosh, :api_client, Swoosh.ApiClient.Req
  #
  # See https://swoosh.hexdocs.pm/Swoosh.html#module-installation for details.
end
