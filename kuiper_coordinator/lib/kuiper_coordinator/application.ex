defmodule KuiperCoordinator.Application do
  # See https://elixir.hexdocs.pm/Application.html
  # for more information on OTP Applications
  @moduledoc false

  use Application

  @impl true
  def start(_type, _args) do
    children =
      [
        KuiperCoordinatorWeb.Telemetry,
        KuiperCoordinator.Repo,
        KuiperCoordinator.Vault,
        {DNSCluster,
         query: Application.get_env(:kuiper_coordinator, :dns_cluster_query) || :ignore},
        {Phoenix.PubSub, name: KuiperCoordinator.PubSub},
        {Task.Supervisor, name: KuiperCoordinator.TaskSupervisor},
        KuiperCoordinator.AgentRegistry,
        {Registry, keys: :unique, name: KuiperCoordinator.AgentGrpc.UploadSlots},
        KuiperCoordinator.Agents.Janitor
      ] ++
        KuiperCoordinator.AgentGrpc.children() ++
        [
          fleet_child(),
          # Start to serve requests, typically the last entry
          KuiperCoordinatorWeb.Endpoint
        ]

    children = Enum.reject(children, &is_nil/1)

    # See https://elixir.hexdocs.pm/Supervisor.html
    # for other strategies and supported options
    opts = [strategy: :one_for_one, name: KuiperCoordinator.Supervisor]
    Supervisor.start_link(children, opts)
  end

  # Tell Phoenix to update the endpoint configuration
  # whenever the application is updated.
  @impl true
  def config_change(changed, _new, removed) do
    KuiperCoordinatorWeb.Endpoint.config_change(changed, removed)
    :ok
  end

  # Provisioning runs from the forges stored in Postgres (D39); the loader starts and reloads
  # the fleet manager. Off in tests, which start their own.
  defp fleet_child do
    if Application.get_env(:kuiper_coordinator, :fleet_loader, true),
      do: KuiperCoordinator.Fleet.Loader
  end
end
