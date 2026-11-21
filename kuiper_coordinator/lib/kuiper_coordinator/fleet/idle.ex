defmodule KuiperCoordinator.Fleet.Idle do
  @moduledoc """
  Idle teardown of job-bound runners (Rust fleet.rs check_idle_runners, parity F6, F13), with
  the fixes for open issues #49 (F14) and #48 (F15).

  Only Forgejo one-job runners are checked: such a runner can only ever run its own job, so if
  that job is still waiting (held back by a concurrency group, say) or gone (cancelled), the
  runner and its VM are idle. GitHub JIT runners can take any queued job with matching labels,
  so they're left alone.

    * The idle clock starts when the runner reports `Started` (registered and about to take its
      job), not at creation (#49): a slow clone or boot isn't idle. A runner that never reports
      `Started` is measured from creation plus a 10 min boot grace, so a VM
      stuck booting still goes.
    * Past the timeout the runner's job is looked up: `in_progress` -> not idle, never checked
      again; `queued` or gone -> torn down (DestroyRunner); anything else or an error -> checked
      next tick. A runner without a repository (org, user and instance scopes) is looked up in
      its scope's job list (#48) instead of being skipped.
    * A torn-down runner's job stays pending but gets no new runner for the timeout, doubling
      each time up to 1 h. The teardown isn't a retry. A destroy that goes unanswered is sent
      again after 120 s.
    * Its terminal events aren't failures, and late ones are ignored for 10 min (F6).

  Timeout: default 600 s, 0 = off, at least 120 s (Rust config.rs idle_runner_timeout). State is
  in memory (lost on restart, which at worst means one more runner for a held-back job).
  """
  require Logger

  alias KuiperCoordinator.{AgentDispatch, Forge, Jobs, Runners}
  alias KuiperCoordinator.Runners.Runner

  @default_timeout_s 600
  @min_timeout_s 120
  @max_backoff_ms 3_600_000
  @destroy_retry_ms 120_000
  @torn_down_memory_ms 600_000
  @boot_grace_ms 600_000

  defstruct timeout_ms: @default_timeout_s * 1000,
            boot_grace_ms: @boot_grace_ms,
            # runner => monotonic ms of its first Started (#49)
            active_since: %{},
            # runners whose job was seen running: not idle, not checked again
            started: MapSet.new(),
            # runner => when its destroy was sent
            tearing_down: %{},
            # runner => when it was torn down, so late events aren't failures (F6)
            torn_down: %{},
            # job_key => {until_ms, delay_ms}
            backoff: %{}

  @type t :: %__MODULE__{}

  @doc "Idle tracking for a timeout in seconds (nil = default); nil when 0 (off)."
  @spec new(nil | non_neg_integer(), keyword()) :: t() | nil
  def new(timeout_s, opts \\ [])
  def new(0, _opts), do: nil

  def new(timeout_s, opts) do
    s = max(timeout_s || @default_timeout_s, @min_timeout_s)

    %__MODULE__{
      timeout_ms: s * 1000,
      boot_grace_ms: Keyword.get(opts, :boot_grace_ms, @boot_grace_ms)
    }
  end

  @doc "The delay before a job gets another runner after an idle teardown (Rust next_idle_backoff)."
  @spec next_backoff(nil | non_neg_integer(), non_neg_integer()) :: non_neg_integer()
  def next_backoff(nil, timeout_ms), do: min(timeout_ms, @max_backoff_ms)
  def next_backoff(previous_ms, _timeout_ms), do: min(previous_ms * 2, @max_backoff_ms)

  @doc """
  What to do with a runner past the timeout, from its job's status (Rust idle_action):
  `:started`, `:tear_down` or `:wait`.
  """
  @spec action({:ok, String.t() | nil} | {:error, term()}) :: :started | :tear_down | :wait
  def action({:ok, "in_progress"}), do: :started
  def action({:ok, "queued"}), do: :tear_down
  def action({:ok, nil}), do: :tear_down
  def action(_), do: :wait

  @doc "The runner reported `Started`: its idle clock starts now (first report wins)."
  @spec runner_started(t() | nil, String.t(), integer()) :: t() | nil
  def runner_started(nil, _name, _now), do: nil

  def runner_started(%__MODULE__{} = idle, name, now),
    do: %{idle | active_since: Map.put_new(idle.active_since, name, now)}

  @doc """
  A terminal event arrived for `name`: `:idled` (we tore it down: not a failure), `:late` (a
  late event for one torn down: ignore it) or `:normal`.
  """
  @spec on_terminal(t() | nil, String.t(), integer()) :: {:idled | :late | :normal, t() | nil}
  def on_terminal(nil, _name, _now), do: {:normal, nil}

  def on_terminal(%__MODULE__{} = idle, name, now) do
    idle = %{
      idle
      | started: MapSet.delete(idle.started, name),
        active_since: Map.delete(idle.active_since, name)
    }

    case Map.pop(idle.tearing_down, name) do
      {nil, _} ->
        if Map.has_key?(idle.torn_down, name), do: {:late, idle}, else: {:normal, idle}

      {_, tearing} ->
        {:idled, %{idle | tearing_down: tearing, torn_down: Map.put(idle.torn_down, name, now)}}
    end
  end

  @doc "Jobs still waiting out their delay after an idle runner (skipped by `OnDemand`)."
  @spec backing_off(t() | nil, integer()) :: MapSet.t()
  def backing_off(nil, _now), do: MapSet.new()

  def backing_off(%__MODULE__{backoff: b}, now),
    do: for({key, {until, _}} <- b, until > now, into: MapSet.new(), do: key)

  @doc """
  One idle check over the runners (each on-demand tick). `ctx` has `:forges`; `now` is
  monotonic ms, `utc_now` the wall clock runner rows are stamped with.
  """
  @spec check(t() | nil, map(), integer(), DateTime.t()) :: t() | nil
  def check(idle, ctx, now, utc_now \\ DateTime.utc_now())
  def check(nil, _ctx, _now, _utc_now), do: nil

  def check(%__MODULE__{} = idle, ctx, now, utc_now) do
    runners = Runners.list_runners()
    idle = forget(idle, MapSet.new(runners, & &1.runner_name), now)

    idle =
      Enum.reduce(runners, idle, fn row, idle -> check_runner(idle, ctx, row, now, utc_now) end)

    pending = MapSet.new(Jobs.list_pending(), & &1.job_key)
    %{idle | backoff: Map.filter(idle.backoff, fn {k, _} -> MapSet.member?(pending, k) end)}
  end

  defp forget(idle, names, now) do
    keep = fn {n, _} -> MapSet.member?(names, n) end

    %{
      idle
      | started: MapSet.filter(idle.started, &MapSet.member?(names, &1)),
        active_since: Map.filter(idle.active_since, keep),
        tearing_down: Map.filter(idle.tearing_down, keep),
        torn_down: Map.filter(idle.torn_down, fn {_, at} -> now - at < @torn_down_memory_ms end)
    }
  end

  defp check_runner(idle, _ctx, %Runner{job_key: nil}, _now, _utc), do: idle

  defp check_runner(idle, ctx, %Runner{} = row, now, utc_now) do
    name = row.runner_name
    idle_ms = idle_for(idle, row, now, utc_now)
    forge = Map.get(ctx.forges, row.forge)

    cond do
      idle_ms < idle.timeout_ms -> idle
      MapSet.member?(idle.started, name) -> idle
      destroy_in_flight?(idle, name, now) -> idle
      forge == nil -> idle
      Forge.runner_kind(forge) != "forgejo" -> idle
      true -> act(idle, forge, row, idle_ms, now)
    end
  end

  # #49: from Started; never started -> from creation, after the boot grace.
  defp idle_for(idle, row, now, utc_now) do
    case Map.fetch(idle.active_since, row.runner_name) do
      {:ok, since} -> now - since
      :error -> DateTime.diff(utc_now, row.inserted_at, :millisecond) - idle.boot_grace_ms
    end
  end

  defp destroy_in_flight?(idle, name, now) do
    case Map.fetch(idle.tearing_down, name) do
      {:ok, sent} -> now - sent < @destroy_retry_ms
      :error -> false
    end
  end

  defp act(idle, forge, row, idle_ms, now) do
    status = job_status(forge, row)

    case action(status) do
      :started ->
        %{idle | started: MapSet.put(idle.started, row.runner_name)}

      :wait ->
        with {:error, e} <- status,
             do:
               Logger.debug(
                 "Idle check: couldn't get job status for #{row.runner_name}: #{inspect(e)}"
               )

        idle

      :tear_down ->
        tear_down(idle, row, status, idle_ms, now)
    end
  end

  # #48: a runner without a repository is looked up in its scope's jobs.
  defp job_status(forge, %Runner{repository: nil} = row),
    do: Forge.scope_job_status(forge, row.runner_scope, job_id(row))

  defp job_status(forge, row), do: Forge.job_status(forge, row.repository, job_id(row))

  defp tear_down(idle, row, status, idle_ms, now) do
    previous =
      case Map.fetch(idle.backoff, row.job_key) do
        {:ok, {_, delay}} -> delay
        :error -> nil
      end

    delay = next_backoff(previous, idle.timeout_ms)
    state = if match?({:ok, s} when is_binary(s), status), do: "still waiting", else: "gone"

    Logger.warning(
      "Runner #{row.runner_name}'s job #{row.job_key} is #{state} after #{div(idle_ms, 1000)} s " <>
        "idle - destroying its VM on #{row.agent_id}; next runner in #{div(delay, 1000)} s"
    )

    case AgentDispatch.dispatch_destroy(row.agent_id, row.runner_name) do
      :ok ->
        :ok

      {:error, e} ->
        Logger.warning("DestroyRunner for idle #{row.runner_name} failed: #{inspect(e)}")
    end

    %{
      idle
      | tearing_down: Map.put(idle.tearing_down, row.runner_name, now),
        backoff: Map.put(idle.backoff, row.job_key, {now + delay, delay})
    }
  end

  defp job_id(%Runner{job_key: key, forge: forge}),
    do: String.replace_prefix(key, forge <> ":", "")
end
