defmodule KuiperCoordinator.AgentGrpc.Supervisor do
  @moduledoc """
  The agent gRPC listener (RegistrationService, AgentService) on its own TLS port
  (`:port`, default 9543), separate from the Phoenix endpoint's HTTP port (D21-D23).

  Makes the CA and server cert on first launch when they're missing
  (`KuiperCoordinator.AgentGrpc.Bootstrap`, D57), checks the cert files and loads the CA (startup fails loudly when they're unusable, A4), then
  starts the gRPC server. The client cert is optional at the TLS layer (RegistrationService runs
  before an agent has one) and enforced per RPC. With `:proxy_protocol` the port also takes an
  optional PROXY v1/v2 header first (`KuiperCoordinator.AgentGrpc.ProxyTransport`).
  """
  use Supervisor

  def start_link(opts), do: Supervisor.start_link(__MODULE__, opts, name: __MODULE__)

  @impl true
  def init(opts) do
    KuiperCoordinator.AgentGrpc.Bootstrap.ensure!(opts)
    ssl = KuiperCoordinator.AgentGrpc.TLS.ssl_opts!(opts)
    KuiperCoordinator.CA.install!(Keyword.fetch!(opts, :ca_cert), Keyword.fetch!(opts, :ca_key))

    grpc_opts = [
      endpoint: KuiperCoordinator.AgentGrpc.Endpoint,
      port: Keyword.get(opts, :port, 9543),
      start_server: true,
      exception_log_filter: {KuiperCoordinator.AgentGrpc, :log_exception?},
      adapter_opts: [
        cred: GRPC.Credential.new(ssl: ssl),
        ip: Keyword.get(opts, :ip, {0, 0, 0, 0})
      ]
    ]

    children = [
      if Keyword.get(opts, :proxy_protocol, false) do
        # optional PROXY v1/v2 header before TLS, for a TLS-passthrough balancer (A3, D74)
        GRPC.Server.Supervisor.child_spec(grpc_opts[:endpoint], grpc_opts[:port], grpc_opts)
        |> KuiperCoordinator.AgentGrpc.ProxyTransport.wrap_child_spec()
      else
        {GRPC.Server.Supervisor, grpc_opts}
      end
    ]

    Supervisor.init(children, strategy: :one_for_one)
  end
end
