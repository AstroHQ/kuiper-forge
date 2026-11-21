defmodule KuiperCoordinator.AgentGrpc.RegistrationServer do
  @moduledoc "RegistrationService.Register: server-TLS only, token in exchange for a client cert."
  use GRPC.Server, service: Cirunner.Agent.RegistrationService.Service

  alias Cirunner.Agent.{RegisterRequest, RegisterResponse}
  alias GRPC.{RPCError, Status}
  alias KuiperCoordinator.{AgentGrpc, Agents}
  require Logger

  def register(%RegisterRequest{} = req, _stream) do
    Logger.info(
      # agent-supplied text, inspected so it can't break or forge log lines (D25)
      "agent registration request hostname=#{inspect(req.hostname)} agent_type=#{inspect(req.agent_type)}"
    )

    case Agents.register(req.registration_token, req.hostname, req.agent_type) do
      {:ok, issued} ->
        Logger.info("agent registered agent_id=#{issued.agent_id}")
        trust_mode = AgentGrpc.setting(:server_trust_mode, "ca")

        %RegisterResponse{
          agent_id: issued.agent_id,
          client_cert_pem: issued.cert_pem,
          client_key_pem: issued.key_pem,
          expires_at: DateTime.to_iso8601(issued.expires_at),
          server_ca_pem: AgentGrpc.TLS.server_ca_pem(AgentGrpc.settings()),
          server_trust_mode: trust_mode
        }

      {:error, :invalid_argument, msg} ->
        raise RPCError, status: Status.invalid_argument(), message: msg

      {:error, :unauthenticated, msg} ->
        Logger.warning("agent registration failed: #{msg}")
        raise RPCError, status: Status.unauthenticated(), message: "Registration failed: #{msg}"
    end
  end
end
