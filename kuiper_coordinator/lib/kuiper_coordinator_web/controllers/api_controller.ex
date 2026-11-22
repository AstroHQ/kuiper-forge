defmodule KuiperCoordinatorWeb.ApiController do
  @moduledoc """
  The read-only JSON API (parity API2-API4, admin/api.rs), behind `Plugs.ApiAuth`:

  - `GET /api/v1/status`: fleet-wide counts. `total_capacity` is the sum of `max_vms` across
    connected agents, so it can differ from the dashboard's limit-aware capacity (as in Rust).
  - `GET /api/v1/agents`: every registered agent with its live status (revoked > online >
    offline); label sets, limits and last_seen_secs are only known while it's connected.
  - `GET /api/v1/agents/:agent_id/logs`: its uploaded log lines, newest first; `level`,
    `before` (a previous `next_before`), `limit` 1-1000 (default 500). An unknown agent is an
    empty 200, as in Rust.
  """
  use KuiperCoordinatorWeb, :controller

  alias KuiperCoordinator.{AgentRegistry, Agents, Jobs, Runners}

  def status(conn, _params) do
    registered = Agents.list_agents()
    connected = AgentRegistry.list()

    json(conn, %{
      registered_agents: length(registered),
      connected_agents: length(connected),
      revoked_agents: Enum.count(registered, & &1.revoked_at),
      unavailable_agents: Enum.count(registered, &(is_nil(&1.revoked_at) and &1.unavailable_at)),
      total_capacity: connected |> Enum.map(&live_max_vms/1) |> Enum.sum(),
      active_runners: length(Runners.list_runners()),
      pending_jobs: length(Jobs.list_pending())
    })
  end

  def agents(conn, _params) do
    connected = Map.new(AgentRegistry.list(), &{&1.agent_id, &1})
    runners = Enum.frequencies_by(Runners.list_runners(), & &1.agent_id)
    now = System.monotonic_time(:millisecond)

    json(
      conn,
      Enum.map(Agents.list_agents(), fn a ->
        live = Map.get(connected, a.agent_id)
        status = live && live[:status]

        %{
          agent_id: a.agent_id,
          hostname: a.hostname,
          agent_type: a.agent_type,
          status:
            cond do
              a.revoked_at -> "revoked"
              live -> "online"
              true -> "offline"
            end,
          unavailable: a.unavailable_at != nil,
          unavailable_since: a.unavailable_at,
          labels: a.labels || [],
          label_sets: if(status, do: Enum.map(status.label_sets, & &1.labels), else: []),
          max_vms: a.max_vms || 0,
          limits:
            if(status,
              do: Enum.map(status.limits, &Map.take(&1, [:name, :max, :external, :active])),
              else: []
            ),
          version: a.agent_version,
          active_runners: Map.get(runners, a.agent_id, 0),
          last_seen_secs: live && div(max(now - live.last_seen, 0), 1000),
          created_at: a.inserted_at,
          cert_expires_at: a.expires_at
        }
      end)
    )
  end

  def agent_logs(conn, %{"agent_id" => agent_id} = params) do
    limit = params |> Map.get("limit") |> parse_limit()
    min_level = Agents.level_filter(params["level"])
    lines = Agents.recent_logs(agent_id, min_level, params["before"], limit)

    next_before =
      if length(lines) == limit and lines != [], do: Agents.log_cursor(List.last(lines))

    json(conn, %{
      lines:
        Enum.map(lines, fn l ->
          %{
            ts: DateTime.from_unix!(l.ts, :microsecond),
            level: Agents.level_name(l.level),
            target: l.target,
            message: l.message
          }
        end),
      next_before: next_before
    })
  end

  defp parse_limit(raw) when is_binary(raw) do
    case Integer.parse(raw) do
      {n, ""} -> n |> max(1) |> min(1000)
      _ -> 500
    end
  end

  defp parse_limit(_), do: 500

  defp live_max_vms(%{status: %{max_vms: n}}) when is_integer(n), do: n
  defp live_max_vms(_), do: 0
end
