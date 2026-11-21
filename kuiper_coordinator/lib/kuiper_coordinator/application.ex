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
        {DNSCluster,
         query: Application.get_env(:kuiper_coordinator, :dns_cluster_query) || :ignore},
        {Phoenix.PubSub, name: KuiperCoordinator.PubSub},
        KuiperCoordinator.AgentRegistry,
        {Registry, keys: :unique, name: KuiperCoordinator.AgentGrpc.UploadSlots},
        KuiperCoordinator.Agents.Janitor
      ] ++
        KuiperCoordinator.AgentGrpc.children() ++
        [
          # Start to serve requests, typically the last entry
          KuiperCoordinatorWeb.Endpoint
        ]

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
end
