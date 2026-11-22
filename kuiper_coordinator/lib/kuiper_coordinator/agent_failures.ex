defmodule KuiperCoordinator.AgentFailures do
  @moduledoc """
  Recent failures per agent (Rust agent_failures.rs, #33/#74): runner failed or lost, a command
  the agent rejected (capacity) or that failed. Only the newest #{100} per agent are kept.
  Recording never raises: a DB error is logged and dropped, like Rust.
  """
  import Ecto.Query
  require Logger

  alias KuiperCoordinator.Repo
  alias KuiperCoordinator.AgentFailures.Failure

  @max_per_agent 100
  @kinds [
    {:runner_failed, "Runner failed"},
    {:runner_lost, "Runner lost"},
    {:command_rejected, "Rejected"},
    {:command_failed, "Command failed"}
  ]

  @doc "Known kinds, in display order, as `{string, label}`."
  def kinds, do: Enum.map(@kinds, fn {k, l} -> {Atom.to_string(k), l} end)

  @doc "A kind's label; unknown kinds are \"Other\"."
  def label(kind) when is_atom(kind), do: label(Atom.to_string(kind))
  def label(kind), do: Enum.find_value(kinds(), "Other", fn {k, l} -> k == kind && l end)

  @doc "A known kind string, else nil (an unknown filter shows everything, like Rust)."
  def parse_kind(kind), do: Enum.find_value(kinds(), fn {k, _} -> k == kind && k end)

  @doc "Records a failure and prunes the agent to its newest #{@max_per_agent}. Always returns :ok."
  def record(agent_id, kind, runner_name, job_key, message)
      when is_binary(agent_id) and is_atom(kind) do
    true = Keyword.has_key?(@kinds, kind)

    Repo.transaction(fn ->
      Repo.insert!(%Failure{
        agent_id: agent_id,
        occurred_at: DateTime.utc_now(),
        kind: Atom.to_string(kind),
        runner_name: runner_name,
        job_key: job_key && to_string(job_key),
        message: to_string(message)
      })

      keep =
        from(f in Failure,
          where: f.agent_id == ^agent_id,
          order_by: [desc: f.occurred_at],
          limit: @max_per_agent,
          select: f.id
        )

      Repo.delete_all(
        from(f in Failure, where: f.agent_id == ^agent_id and f.id not in subquery(keep))
      )
    end)

    :ok
  rescue
    e ->
      Logger.warning("Failed to record agent failure for #{agent_id}: #{Exception.message(e)}")
      :ok
  end

  @doc "An agent's newest failures, newest first."
  def recent(agent_id, limit) do
    Repo.all(
      from(f in Failure,
        where: f.agent_id == ^agent_id,
        order_by: [desc: f.occurred_at],
        limit: ^limit
      )
    )
  end

  @doc "Newest failures across agents; nil `kind` or `agent_id` matches everything."
  def recent_all(kind, agent_id, limit) do
    from(f in Failure, order_by: [desc: f.occurred_at], limit: ^limit)
    |> then(&if(kind, do: where(&1, [f], f.kind == ^kind), else: &1))
    |> then(&if(agent_id, do: where(&1, [f], f.agent_id == ^agent_id), else: &1))
    |> Repo.all()
  end

  @doc "`{agent_id, kind, count}` for failures since `since`."
  def counts_since(%DateTime{} = since) do
    Repo.all(
      from(f in Failure,
        where: f.occurred_at >= ^since,
        group_by: [f.agent_id, f.kind],
        select: {f.agent_id, f.kind, count(f.id)}
      )
    )
  end

  @doc """
  Per-agent counts in kind order (unknown kinds count toward the total only, 33b1c0f), sorted by
  total then hostname; `hostname` maps an agent id to its name, falling back to the id.
  """
  def count_rows(counts, hostname) do
    kinds = Enum.map(kinds(), &elem(&1, 0))

    counts
    |> Enum.group_by(&elem(&1, 0))
    |> Enum.map(fn {agent_id, rows} ->
      by_kind = Map.new(rows, fn {_, k, n} -> {k, n} end)

      %{
        agent_id: agent_id,
        hostname: hostname.(agent_id),
        total: rows |> Enum.map(&elem(&1, 2)) |> Enum.sum(),
        counts: Enum.map(kinds, &Map.get(by_kind, &1, 0))
      }
    end)
    |> Enum.sort_by(&{-&1.total, &1.hostname})
  end
end
