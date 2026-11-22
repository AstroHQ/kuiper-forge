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
if config_env() != :test do
  data_dir = System.get_env("KUIPER_DATA_DIR", "/var/lib/kuiper-forge")

  config :kuiper_coordinator, :agent_grpc,
    enabled: System.get_env("KUIPER_AGENT_GRPC", "false") in ~w(true 1),
    port: String.to_integer(System.get_env("KUIPER_AGENT_PORT", "9543")),
    ca_cert: System.get_env("KUIPER_CA_CERT", Path.join(data_dir, "ca.crt")),
    ca_key: System.get_env("KUIPER_CA_KEY", Path.join(data_dir, "ca.key")),
    server_cert: System.get_env("KUIPER_SERVER_CERT", Path.join(data_dir, "server.crt")),
    server_key: System.get_env("KUIPER_SERVER_KEY", Path.join(data_dir, "server.key")),
    server_ca_cert: System.get_env("KUIPER_SERVER_CA_CERT"),
    server_use_native_roots:
      System.get_env("KUIPER_SERVER_USE_NATIVE_ROOTS", "false") in ~w(true 1),
    server_trust_mode: System.get_env("KUIPER_SERVER_TRUST_MODE", "ca"),
    # the name agents and browsers use; the server cert is checked against it at startup
    server_hostname: System.get_env("KUIPER_SERVER_HOSTNAME") || System.get_env("PHX_HOST")

  # Management gRPC (management.proto) for the CLI's token/agent commands, on a Unix socket.
  # Its own path beside the Rust coordinator's <data_dir>/coordinator.sock (parity M6). On
  # with agent gRPC unless KUIPER_MANAGEMENT says otherwise (tokens are for registration).
  config :kuiper_coordinator, :management,
    enabled:
      System.get_env("KUIPER_MANAGEMENT", System.get_env("KUIPER_AGENT_GRPC", "false")) in ~w(true 1),
    socket:
      System.get_env("KUIPER_MANAGEMENT_SOCKET") ||
        Path.join(data_dir, "phoenix-coordinator.sock")
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

# Admin UI under /admin (parity C7: [admin] enabled false, session_timeout_secs 3600).
# Create the first user with `bin/kuiper_coordinator eval 'KuiperCoordinator.Release.create_admin("name")'`
# and KUIPER_ADMIN_PASSWORD set.
if config_env() != :test do
  config :kuiper_coordinator, :admin,
    enabled: System.get_env("KUIPER_ADMIN", "false") in ~w(true 1),
    session_timeout_secs:
      String.to_integer(System.get_env("KUIPER_ADMIN_SESSION_TIMEOUT", "3600")),
    # agent-facing URL in registration bundles (Rust [admin] coordinator_url)
    coordinator_url: System.get_env("KUIPER_COORDINATOR_URL", "https://localhost:9543"),
    # flag agents behind the newest GitHub release (Rust [admin] update_check, #75)
    # (KuiperCoordinator.AgentUpdates.enabled_from_env/1)
    update_check:
      String.downcase(String.trim(System.get_env("KUIPER_UPDATE_CHECK", "true"))) not in ~w(0 false no off)

  # run history for the metrics page (parity C7/MX7: [metrics] retention_days 90)
  config :kuiper_coordinator, :metrics,
    retention_days: String.to_integer(System.get_env("KUIPER_METRICS_RETENTION_DAYS", "90"))
end

if config_env() == :dev do
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
    System.get_env("DATABASE_URL") ||
      raise """
      environment variable DATABASE_URL is missing.
      For example: ecto://USER:PASS@HOST/DATABASE
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
