defmodule KuiperCoordinator.AgentDispatch.Capacity do
  @moduledoc """
  Pure port of the capacity logic of `ConnectedAgent` and the capacity-related parts of
  `AgentRegistry` from the Rust coordinator's `agent_registry.rs`: limits, label sets, reserved
  slots, agent selection and pool definitions. No processes and no DB; time is passed in as
  milliseconds. Label comparison is case-insensitive like in Rust.
  """

  alias Cirunner.Agent.AgentStatus
  alias KuiperCoordinator.Fleet.PoolDefinition

  defstruct agent_id: nil,
            max_vms: 0,
            active_vms: 0,
            labels: [],
            label_sets: [],
            limits: [],
            set_limits: [],
            set_pools: [],
            set_ids: [],
            default_set: nil,
            runner_kinds: [],
            reservations: %{},
            unavailable: false

  @type limit :: %{
          name: String.t(),
          max: non_neg_integer(),
          external: non_neg_integer(),
          active: non_neg_integer()
        }
  @type t :: %__MODULE__{}

  @doc "Builds the capacity of an agent from its status (`AgentCapacity::from` plus counts and labels)."
  @spec from_status(String.t(), AgentStatus.t()) :: t()
  def from_status(agent_id, %AgentStatus{} = status) do
    sets = status.label_sets

    %__MODULE__{
      agent_id: agent_id,
      max_vms: status.max_vms,
      active_vms: status.active_vms,
      labels: status.labels,
      label_sets: Enum.map(sets, & &1.labels),
      limits:
        Enum.map(status.limits, fn l ->
          %{name: l.name, max: l.max, external: l.external, active: l.active}
        end),
      set_limits: Enum.map(sets, & &1.limits),
      set_pools: Enum.map(sets, & &1.pool_size),
      set_ids: Enum.map(sets, & &1.id),
      default_set: Enum.find_index(sets, & &1.is_default),
      runner_kinds: status.runner_kinds
    }
  end

  @doc "Refreshes everything from a new status but keeps reservations and the unavailable flag."
  @spec update_status(t(), AgentStatus.t()) :: t()
  def update_status(%__MODULE__{} = cap, %AgentStatus{} = status) do
    %{
      from_status(cap.agent_id, status)
      | reservations: cap.reservations,
        unavailable: cap.unavailable
    }
  end

  @doc "Lowercased and sorted, so the same labels in any order make the same pool."
  @spec normalize_labels([String.t()]) :: [String.t()]
  defdelegate normalize_labels(labels), to: PoolDefinition

  # --- label helpers ---

  defp ieq?(a, b), do: String.downcase(a) == String.downcase(b)

  defp set_covers?(set, required),
    do: Enum.all?(required, fn r -> Enum.any?(set, &ieq?(&1, r)) end)

  @doc "Label sets to show people: the default set is hidden when there are other sets."
  @spec display_label_sets(t()) :: [[String.t()]]
  def display_label_sets(%__MODULE__{label_sets: sets}) when length(sets) <= 1, do: sets

  def display_label_sets(%__MODULE__{label_sets: sets, default_set: default}) do
    sets
    |> Enum.with_index()
    |> Enum.reject(fn {_, i} -> i == default end)
    |> Enum.map(&elem(&1, 0))
  end

  @doc "True if the job labels are covered by any label set (or the flat labels without sets)."
  @spec matches_labels?(t(), [String.t()]) :: boolean()
  def matches_labels?(%__MODULE__{label_sets: [], labels: labels}, required),
    do: set_covers?(labels, required)

  def matches_labels?(%__MODULE__{label_sets: sets}, required),
    do: Enum.any?(sets, &set_covers?(&1, required))

  # --- limits ---

  defp limits_per_set?(cap), do: Enum.any?(cap.set_limits, &(&1 != []))

  defp limit_active(cap, limit),
    do: if(limits_per_set?(cap), do: limit.active, else: cap.active_vms)

  defp selected_set(cap, labels, set_id) do
    by_id = if set_id != "", do: Enum.find_index(cap.set_ids, &(&1 == set_id))

    cond do
      by_id != nil -> by_id
      cap.default_set == nil -> nil
      true -> pick_set(cap, labels, cap.default_set)
    end
  end

  defp pick_set(cap, labels, default) do
    base? = fn l -> Enum.any?(cap.labels, &ieq?(&1, l)) end

    asks_for_extra? = fn set ->
      set |> Enum.reject(base?) |> Enum.any?(fn l -> Enum.any?(labels, &ieq?(&1, l)) end)
    end

    found =
      cap.label_sets
      |> Enum.with_index()
      |> Enum.find_value(fn {set, i} ->
        if i != default and asks_for_extra?.(set) and set_covers?(set, labels), do: i
      end)

    cond do
      found != nil -> found
      (set = Enum.at(cap.label_sets, default)) != nil and set_covers?(set, labels) -> default
      true -> nil
    end
  end

  defp limits_for(cap, labels, set_id) do
    if limits_per_set?(cap) do
      names =
        case selected_set(cap, labels, set_id) do
          nil ->
            cap.label_sets
            |> Enum.zip(cap.set_limits)
            |> Enum.filter(fn {set, _} -> set_covers?(set, labels) end)
            |> Enum.flat_map(&elem(&1, 1))

          i ->
            Enum.at(cap.set_limits, i, [])
        end

      Enum.filter(cap.limits, &(&1.name in names))
    else
      cap.limits
    end
  end

  defp reserved_slots(cap), do: map_size(cap.reservations)

  defp limit_reserved(cap, limit) do
    if limits_per_set?(cap) do
      Enum.count(cap.reservations, fn {_, r} -> limit.name in r.limits end)
    else
      reserved_slots(cap)
    end
  end

  defp headroom(l), do: max(l.max - l.external, 0)

  @doc "How many VMs of any kind the agent can run now: max_vms lowered by limits every VM counts against."
  @spec effective_max(t()) :: non_neg_integer()
  def effective_max(%__MODULE__{} = cap) do
    per_set = limits_per_set?(cap)

    cap.limits
    |> Enum.filter(fn l -> not per_set or Enum.all?(cap.set_limits, &(l.name in &1)) end)
    |> Enum.map(&headroom/1)
    |> Enum.reduce(cap.max_vms, &min/2)
  end

  @doc "Most VMs for a job with these labels the agent can run at once with nothing else running."
  @spec ceiling_for(t(), [String.t()]) :: non_neg_integer()
  def ceiling_for(%__MODULE__{} = cap, labels) do
    cap |> limits_for(labels, "") |> Enum.map(&headroom/1) |> Enum.reduce(cap.max_vms, &min/2)
  end

  @doc "True if the agent sets its own fixed-capacity pools per label set."
  @spec explicit_pools?(t()) :: boolean()
  def explicit_pools?(%__MODULE__{set_pools: pools}), do: Enum.any?(pools, &(&1 != nil))

  @doc "Whether the agent can start `kind` (\"github\" or \"forgejo\") runners. GitHub always works."
  @spec runs?(t(), String.t()) :: boolean()
  def runs?(_cap, "github"), do: true
  def runs?(%__MODULE__{runner_kinds: kinds}, kind), do: Enum.any?(kinds, &ieq?(&1, kind))

  @doc "True if the agent is in the legacy pool with these (normalized) labels."
  @spec in_legacy_pool?(t(), [String.t()]) :: boolean()
  def in_legacy_pool?(%__MODULE__{} = cap, pool_labels),
    do: not explicit_pools?(cap) and normalize_labels(cap.labels) == pool_labels

  @doc "Free slots for a job with these labels, created from label set `set_id` if given."
  @spec available_capacity(t(), [String.t()], String.t()) :: non_neg_integer()
  def available_capacity(cap, labels, set_id \\ "")
  def available_capacity(%__MODULE__{unavailable: true}, _labels, _set_id), do: 0

  def available_capacity(%__MODULE__{} = cap, labels, set_id) do
    cap
    |> limits_for(labels, set_id)
    |> Enum.map(fn l ->
      max(l.max - (l.external + limit_active(cap, l) + limit_reserved(cap, l)), 0)
    end)
    |> Enum.reduce(max(cap.max_vms - (cap.active_vms + reserved_slots(cap)), 0), &min/2)
  end

  @doc "True if at least one slot is free for the job."
  @spec has_capacity?(t(), [String.t()], String.t()) :: boolean()
  def has_capacity?(cap, labels, set_id \\ ""), do: available_capacity(cap, labels, set_id) > 0

  # --- reservations ---

  @doc "Reserves a slot for `runner_name`. `:error` when there is no capacity."
  @spec reserve(t(), [String.t()], String.t(), String.t(), integer()) :: {:ok, t()} | :error
  def reserve(%__MODULE__{} = cap, labels, set_id, runner_name, now_ms) do
    if has_capacity?(cap, labels, set_id) do
      limits =
        if limits_per_set?(cap),
          do: cap |> limits_for(labels, set_id) |> Enum.map(& &1.name),
          else: []

      {:ok,
       %{
         cap
         | reservations: Map.put(cap.reservations, runner_name, %{limits: limits, at: now_ms})
       }}
    else
      :error
    end
  end

  @doc "Releases the reserved slot of `runner_name`; nothing happens if its VM already showed up."
  @spec release(t(), String.t()) :: t()
  def release(%__MODULE__{} = cap, runner_name),
    do: %{cap | reservations: Map.delete(cap.reservations, runner_name)}

  @doc "Drops reservations of runners whose VMs are in the latest status."
  @spec settle(t(), [String.t()]) :: t()
  def settle(%__MODULE__{} = cap, vm_names),
    do: %{cap | reservations: Map.reject(cap.reservations, fn {name, _} -> name in vm_names end)}

  @doc "Drops reservations at least `min_age_ms` old whose runner is not in `db_runner_names`."
  @spec reconcile(t(), [String.t()], integer(), integer()) :: t()
  def reconcile(%__MODULE__{} = cap, db_runner_names, min_age_ms, now_ms) do
    kept =
      Map.reject(cap.reservations, fn {name, r} ->
        now_ms - r.at >= min_age_ms and name not in db_runner_names
      end)

    %{cap | reservations: kept}
  end

  # --- over a list of agents ---

  @doc """
  Picks the least loaded agent that matches the labels and has a free slot: most free slots, then
  fewest in use (active + reserved), then id. Agents in `:avoid` only win when nobody else fits.
  `:include` filters the candidates.
  """
  @spec select([t()], [String.t()], keyword()) :: {:ok, String.t()} | :none
  def select(caps, labels, opts \\ []) do
    avoid = Keyword.get(opts, :avoid, [])
    include = Keyword.get(opts, :include, fn _ -> true end)

    caps
    |> Enum.filter(
      &(include.(&1) and has_capacity?(&1, labels, "") and matches_labels?(&1, labels))
    )
    |> Enum.map(fn c ->
      {{c.agent_id in avoid, -available_capacity(c, labels, ""), c.active_vms + reserved_slots(c),
        c.agent_id}, c.agent_id}
    end)
    |> case do
      [] -> :none
      list -> {:ok, list |> Enum.min_by(&elem(&1, 0)) |> elem(1)}
    end
  end

  @doc "Total free slots across agents matching the labels."
  @spec total_capacity([t()], [String.t()]) :: non_neg_integer()
  def total_capacity(caps, labels) do
    caps
    |> Enum.filter(&matches_labels?(&1, labels))
    |> Enum.map(&available_capacity(&1, labels, ""))
    |> Enum.sum()
  end

  @doc "Free slots across the members of the legacy pool with these (normalized) labels."
  @spec legacy_pool_capacity([t()], [String.t()]) :: non_neg_integer()
  def legacy_pool_capacity(caps, pool_labels) do
    caps
    |> Enum.filter(&in_legacy_pool?(&1, pool_labels))
    |> Enum.map(&available_capacity(&1, pool_labels, ""))
    |> Enum.sum()
  end

  @doc """
  Pool definitions derived from the agents. Unavailable agents are skipped. Agents with explicit
  pools get one pool per set with a size above 0; the rest share a legacy pool per normalized base
  label set whose target is the sum of their ceilings for the base labels.
  """
  @spec pool_definitions([t()]) :: [PoolDefinition.t()]
  def pool_definitions(caps) do
    caps = caps |> Enum.reject(& &1.unavailable) |> Enum.sort_by(& &1.agent_id)
    {explicit, legacy} = Enum.split_with(caps, &explicit_pools?/1)

    legacy_pools =
      legacy
      |> Enum.group_by(&normalize_labels(&1.labels))
      |> Enum.sort_by(&elem(&1, 0))
      |> Enum.map(fn {labels, members} ->
        %PoolDefinition{
          labels: labels,
          target_count: members |> Enum.map(&ceiling_for(&1, &1.labels)) |> Enum.sum(),
          agent_id: nil,
          label_set_id: "",
          members: members |> Enum.map(& &1.agent_id) |> Enum.sort()
        }
      end)

    explicit_pools =
      Enum.flat_map(explicit, fn cap ->
        cap.label_sets
        |> Enum.with_index()
        |> Enum.flat_map(fn {set, i} ->
          target = Enum.at(cap.set_pools, i) || 0

          if target > 0 do
            [
              %PoolDefinition{
                labels: normalize_labels(set),
                target_count: target,
                agent_id: cap.agent_id,
                label_set_id: Enum.at(cap.set_ids, i) || "",
                members: [cap.agent_id]
              }
            ]
          else
            []
          end
        end)
      end)

    legacy_pools ++ explicit_pools
  end
end
