defmodule KuiperCoordinator.Management.Server do
  @moduledoc "ManagementService RPCs (Rust `management.rs`), over the registration token and agent tables."
  use GRPC.Server, service: Cirunner.Management.ManagementService.Service

  alias Cirunner.Management, as: M
  alias GRPC.{RPCError, Status}
  alias KuiperCoordinator.{AgentGrpc, Agents}
  require Logger

  # Rust takes any u64; 0 makes a dead token and a huge one overflows. Cap at a year (M1).
  @max_ttl_secs 365 * 86_400

  def create_token(%M.CreateTokenRequest{expires_secs: secs}, _stream)
      when secs < 1 or secs > @max_ttl_secs do
    raise RPCError,
      status: Status.invalid_argument(),
      message: "expires_secs must be between 1 and #{@max_ttl_secs}"
  end

  def create_token(%M.CreateTokenRequest{expires_secs: secs, created_by: by}, _stream) do
    by = if by == "", do: "cli", else: by

    case Agents.create_registration_token(secs, by) do
      {:ok, token} ->
        settings = AgentGrpc.settings()
        Logger.info("management: registration token created created_by=#{inspect(by)}")

        %M.CreateTokenResponse{
          token: token.token,
          expires_at: rfc3339(token.expires_at),
          created_at: rfc3339(token.inserted_at),
          server_ca_pem: server_ca_pem(settings),
          server_trust_mode: Keyword.get(settings, :server_trust_mode, "ca")
        }

      {:error, reason} ->
        raise RPCError,
          status: Status.internal(),
          message: "Failed to create token: #{inspect(reason)}"
    end
  end

  def list_tokens(%M.ListTokensRequest{}, _stream) do
    tokens =
      for t <- Agents.list_registration_tokens() do
        %M.TokenInfo{
          token: t.token,
          expires_at: rfc3339(t.expires_at),
          created_by: t.created_by || "",
          created_at: rfc3339(t.inserted_at)
        }
      end

    %M.ListTokensResponse{tokens: tokens}
  end

  def delete_token(%M.DeleteTokenRequest{token: token}, _stream) do
    %M.DeleteTokenResponse{deleted: Agents.delete_registration_token(token)}
  end

  def list_agents(%M.ListAgentsRequest{}, _stream) do
    agents =
      for a <- Agents.list_agents() do
        %M.AgentInfo{
          agent_id: a.agent_id,
          hostname: a.hostname,
          agent_type: a.agent_type,
          labels: a.labels || [],
          max_vms: a.max_vms || 0,
          created_at: rfc3339(a.inserted_at),
          expires_at: rfc3339(a.expires_at),
          revoked: not is_nil(a.revoked_at),
          agent_version: a.agent_version || ""
        }
      end

    %M.ListAgentsResponse{agents: agents}
  end

  def revoke_agent(%M.RevokeAgentRequest{agent_id: id}, _stream) do
    revoked = Agents.revoke_agent(id) == :ok
    if revoked, do: Logger.info("management: agent revoked agent_id=#{inspect(id)}")
    %M.RevokeAgentResponse{revoked: revoked}
  end

  # The CA agents verify the coordinator with, as RegisterResponse gives it; "" when there's
  # no CA configured (agent gRPC off) rather than failing the token.
  defp server_ca_pem(settings) do
    if settings[:ca_cert] || settings[:server_ca_cert],
      do: AgentGrpc.TLS.server_ca_pem(settings),
      else: ""
  end

  defp rfc3339(nil), do: ""
  defp rfc3339(%DateTime{} = dt), do: DateTime.to_iso8601(dt)
end
