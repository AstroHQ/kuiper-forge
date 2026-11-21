defmodule KuiperCoordinator.Admin.Dashboard do
  @moduledoc """
  What the admin dashboard shows (parity UI2, Rust admin/routes.rs render_dashboard and
  admin/templates.rs): connected agents, active runners, pending jobs (still waiting for an agent
  first, then oldest), VM capacity across online non-revoked agents (an agent's "total" limit
  when it reports one, else max_vms; other limits summed by name), the agents table and the open
  registration tokens.

  `build/1` reads the DB and the live registry; the rest are pure, so they're tested without
  agents.
  """

  alias Cirunner.Agent.{AgentStatus, CapacityLimit, LabelSet}
  alias KuiperCoordinator.{AgentRegistry, Agents, Jobs, Runners}

  @total "total"

  @doc "Everything the dashboard renders, as of `now`."
  def build(now \\ DateTime.utc_now()) do
    connected = Map.new(AgentRegistry.list(), &{&1.agent_id, &1})
    runners = Runners.list_runners()
    agents = Enum.map(Agents.list_agents(), &agent_summary(&1, connected, runners))

    %{
      connected_agents: map_size(connected),
      active_runners: length(runners),
      pending_jobs: pending_jobs(Jobs.list_pending(), runners, connected, agents, now),
      capacity: capacity_summary(agents),
      agents: agents,
      tokens: Agents.list_registration_tokens()
    }
  end

  @doc "One row of the agents table: the DB record plus what its live connection reports."
  def agent_summary(agent, connected, runners) do
    live = Map.get(connected, agent.agent_id)
    status = live && live[:status]
    {base, mappings} = split_labels(status)

    %{
      agent_id: agent.agent_id,
      hostname: agent.hostname,
      agent_type: agent.agent_type,
      base_labels: base,
      mapping_labels: mappings,
      max_vms: agent.max_vms || 0,
      limits: limit_views(status),
      online?: live != nil,
      active_vms: Enum.count(runners, &(&1.agent_id == agent.agent_id)),
      created_at: agent.inserted_at,
      revoked?: agent.revoked_at != nil,
      unavailable_at: agent.unavailable_at,
      version: agent.agent_version
    }
  end

  @doc """
  An agent's base labels and what each label set adds to them, so the base isn't repeated per
  set. The default set is only the base, so it's left out when there are other sets.
  """
  def split_labels(nil), do: {[], []}

  def split_labels(%AgentStatus{labels: base, label_sets: sets}) do
    base_down = MapSet.new(base, &String.downcase/1)

    mappings =
      sets
      |> display_sets()
      |> Enum.map(fn labels -> Enum.reject(labels, &(String.downcase(&1) in base_down)) end)
      |> Enum.reject(&(&1 == []))

    {base, mappings}
  end

  defp display_sets([%LabelSet{labels: labels}]), do: [labels]

  defp display_sets(sets),
    do: for(%LabelSet{is_default: false, labels: labels} <- sets, do: labels)

  defp limit_views(nil), do: []

  defp limit_views(%AgentStatus{limits: limits}) do
    for %CapacityLimit{} = l <- limits,
        do: %{name: l.name, used: l.active + l.external, max: l.max, external: l.external}
  end

  @doc "VMs in use on the agent, external ones included when it reports a total limit."
  def capacity_used(a), do: if(l = total(a), do: l.used, else: a.active_vms)
  def capacity_max(a), do: if(l = total(a), do: l.max, else: a.max_vms)
  def capacity_external(a), do: if(l = total(a), do: l.external, else: 0)

  defp total(a), do: Enum.find(a.limits, &(&1.name == @total))

  @doc "Capacity across online, non-revoked agents; limits other than total summed by name."
  def capacity_summary(agents) do
    counted = Enum.filter(agents, &(&1.online? and not &1.revoked?))

    limits =
      counted
      |> Enum.flat_map(fn a -> Enum.reject(a.limits, &(&1.name == @total)) end)
      |> Enum.reduce([], fn l, acc ->
        case Enum.split_with(acc, &(&1.name == l.name)) do
          {[t], rest} ->
            rest ++
              [
                %{
                  t
                  | used: t.used + l.used,
                    max: t.max + l.max,
                    external: t.external + l.external
                }
              ]

          {[], _} ->
            acc ++ [l]
        end
      end)
      |> sort_like(counted)

    %{
      used: Enum.sum_by(counted, &capacity_used/1),
      total: Enum.sum_by(counted, &capacity_max/1),
      external: Enum.sum_by(counted, &capacity_external/1),
      limits: limits
    }
  end

  # keep first-seen order (the reduce appends updated entries at the end)
  defp sort_like(limits, agents) do
    order =
      agents
      |> Enum.flat_map(fn a -> Enum.map(a.limits, & &1.name) end)
      |> Enum.uniq()
      |> Enum.with_index()
      |> Map.new()

    Enum.sort_by(limits, &Map.get(order, &1.name))
  end

  @doc "How a limit is named on the page."
  def limit_label(name) do
    if String.downcase(name) == "macos", do: "macOS", else: name
  end

  @doc """
  Pending jobs, those still waiting for an agent first, then oldest. A job keeps its pending row
  until its runner finishes, so `assigned_agent` tells those apart.
  """
  def pending_jobs(pending, runners, connected, agents, now) do
    job_agents = for r <- runners, r.job_key, into: %{}, do: {r.job_key, r.agent_id}
    unavailable = for a <- agents, a.unavailable_at, into: MapSet.new(), do: a.agent_id

    pending
    |> Enum.sort_by(
      &{Map.has_key?(job_agents, &1.job_key), DateTime.to_unix(queued_at(&1), :microsecond)}
    )
    |> Enum.map(fn job ->
      matching = for {id, entry} <- connected, matches?(entry[:status], job.agent_labels), do: id

      free =
        matching
        |> Enum.reject(&(&1 in unavailable))
        |> Enum.map(&free_slots(connected[&1][:status], job.agent_labels))
        |> Enum.sum()

      %{
        job_key: job.job_key,
        assigned_agent: Map.get(job_agents, job.job_key),
        repository: job.repository,
        workflow_name: job.workflow_name,
        job_name: job.job_name,
        labels: job.job_labels,
        waiting: format_age(DateTime.diff(now, queued_at(job))),
        retry_count: job.retry_count,
        failed_agents: length(job.failed_agents),
        matching_agents: length(matching),
        unavailable_agents: Enum.count(matching, &(&1 in unavailable)),
        free_capacity: free
      }
    end)
  end

  defp queued_at(job), do: job.queued_at || job.inserted_at

  @doc "A job's labels are covered by one of the agent's label sets (or its flat labels)."
  def matches?(nil, _labels), do: false

  def matches?(%AgentStatus{label_sets: [], labels: base}, labels), do: covers?(base, labels)

  def matches?(%AgentStatus{label_sets: sets}, labels),
    do: Enum.any?(sets, &covers?(&1.labels, labels))

  defp covers?(set, labels) do
    set = MapSet.new(set, &String.downcase/1)
    Enum.all?(labels, &(String.downcase(&1) in set))
  end

  # Free slots the agent reports for a job like this: max_vms less active, and less what each
  # limit the matching label set counts against has left (every limit, for older agents that
  # don't say). Reservations live in the dispatcher, so this can run a little high while a
  # runner is being created.
  defp free_slots(%AgentStatus{} = s, labels) do
    base = max(s.max_vms - s.active_vms, 0)
    per_set? = Enum.any?(s.label_sets, &(&1.limits != []))

    names =
      if per_set? do
        s.label_sets |> Enum.filter(&covers?(&1.labels, labels)) |> Enum.flat_map(& &1.limits)
      else
        Enum.map(s.limits, & &1.name)
      end

    s.limits
    |> Enum.filter(&(&1.name in names))
    |> Enum.map(&max(&1.max - &1.external - &1.active, 0))
    |> Enum.reduce(base, &min/2)
  end

  @doc "Rust format_age: 42s, 3m 5s, 2h 10m, 1d 4h."
  def format_age(secs) when secs < 0, do: format_age(0)

  def format_age(secs) do
    {d, h, m, s} =
      {div(secs, 86_400), rem(div(secs, 3600), 24), rem(div(secs, 60), 60), rem(secs, 60)}

    cond do
      d > 0 -> "#{d}d #{h}h"
      h > 0 -> "#{h}h #{m}m"
      m > 0 -> "#{m}m #{s}s"
      true -> "#{s}s"
    end
  end
end
