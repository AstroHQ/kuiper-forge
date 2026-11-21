defmodule KuiperCoordinator.AgentGrpc.Auth do
  @moduledoc """
  mTLS gate for AgentService (A15): identity is the first peer cert's CN, never a field in the
  message, and the agent must exist, not be revoked and not be expired.
  """
  alias GRPC.RPCError
  alias GRPC.Status

  def agent_id!(stream) do
    with der when is_binary(der) <- stream.adapter.get_cert(stream.payload),
         id when is_binary(id) <- KuiperCoordinator.CA.agent_id_from_der(der) do
      id
    else
      _ ->
        raise RPCError,
          status: Status.unauthenticated(),
          message: "No client certificate presented - mTLS required for agent service"
    end
  end

  def valid_agent!(agent_id) do
    unless KuiperCoordinator.Agents.valid_agent?(agent_id) do
      raise RPCError,
        status: Status.unauthenticated(),
        message: "Agent not registered or has been revoked"
    end

    agent_id
  end

  def authenticate!(stream), do: stream |> agent_id!() |> valid_agent!()
end
