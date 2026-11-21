defmodule KuiperCoordinator.AgentGrpc do
  @moduledoc """
  Agent gRPC: RegistrationService and AgentService from `kuiper-agent-proto`, using the Rust
  coordinator's CA (D2), on their own TLS port, separate from the Phoenix endpoint's HTTP port
  (D21-D23; see `KuiperCoordinator.AgentGrpc.Supervisor`). The client cert is optional at TLS
  and enforced per RPC.

  Settings live under `config :kuiper_coordinator, :agent_grpc` (see `config/runtime.exs`):
  `:enabled`, `:port` (KUIPER_AGENT_PORT, default 9543), `:ip`, `:ca_cert`, `:ca_key`,
  `:server_cert` (may hold intermediates after the leaf), `:server_key`, `:server_ca_cert`,
  `:server_use_native_roots`, `:server_trust_mode` ("ca" or "chain"), `:ping_interval_ms`,
  `:first_status_timeout_ms`, `:stale_after_ms`, `:sweep_interval_ms`.
  """

  def settings, do: Application.get_env(:kuiper_coordinator, :agent_grpc, [])
  def setting(key, default \\ nil), do: Keyword.get(settings(), key, default)

  @doc """
  gRPC server exception log filter: an RPCError we raise on purpose (bad token, no cert,
  revoked) is an answer to the agent, not a crash, so only other exceptions are logged.
  """
  def log_exception?(%GRPC.Server.Adapters.ReportException{reason: %GRPC.RPCError{}}), do: false
  def log_exception?(_), do: true

  @doc "Child spec for the application: the gRPC server when enabled, nothing otherwise."
  def children do
    if setting(:enabled, false), do: [{__MODULE__.Supervisor, settings()}], else: []
  end
end
