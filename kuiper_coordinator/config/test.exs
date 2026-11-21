import Config

# Configure your database
#
# The MIX_TEST_PARTITION environment variable can be used
# to provide built-in test partitioning in CI environment.
# Run `mix help test` for more information.
config :kuiper_coordinator, KuiperCoordinator.Repo,
  username: "postgres",
  password: "postgres",
  hostname: "localhost",
  port: String.to_integer(System.get_env("PGPORT", "5432")),
  database: "kuiper_coordinator_test#{System.get_env("MIX_TEST_PARTITION")}",
  pool: Ecto.Adapters.SQL.Sandbox,
  pool_size: System.schedulers_online() * 2

# We don't run a server during test. If one is required,
# you can enable the server option below.
config :kuiper_coordinator, KuiperCoordinatorWeb.Endpoint,
  http: [ip: {127, 0, 0, 1}, port: 4002],
  secret_key_base: "zVHymrk9c1yPvStZ/xYIIFbiYwro0+clRRUXVrLb034fpGyTzGq8ekoNd6dpBhbF",
  server: false

# In test we don't send emails
config :kuiper_coordinator, KuiperCoordinator.Mailer, adapter: Swoosh.Adapters.Test

# Disable swoosh api client as it is only required for production adapters
config :swoosh, :api_client, false

# Print only warnings and errors during test
config :logger, level: :warning

# Initialize plugs at runtime for faster test compilation
config :phoenix, :plug_init_mode, :runtime

# Enable helpful, but potentially expensive runtime checks
config :phoenix_live_view,
  enable_expensive_runtime_checks: true

# Sort query params output of verified routes for robust url comparisons
config :phoenix,
  sort_verified_routes_query_params: true

config :kuiper_coordinator, :agent_dispatch, KuiperCoordinator.AgentDispatchMock

# Stored secrets (D39): a fixed test key; forges aren't checked live on save, and the fleet
# loader is off (tests start their own manager and loader)
config :kuiper_coordinator, KuiperCoordinator.Vault,
  key: Base.decode64!("3Jnb0hZiX0QaVE8EmUhx4q0mqYbnbYp9G6u4C2AbmP0=")

config :kuiper_coordinator, validate_forges_on_save: false, fleet_loader: false

# Cheap Argon2 in tests (production uses the library defaults, Argon2id)
config :argon2_elixir, t_cost: 1, m_cost: 8
