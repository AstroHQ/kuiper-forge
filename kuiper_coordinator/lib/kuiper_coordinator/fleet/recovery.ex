defmodule KuiperCoordinator.Fleet.Recovery do
  @moduledoc """
  The webhook-mode forge scan (Rust fleet.rs scan_forges, parity G9): first verify every
  pending job is still queued on its forge, then add queued jobs the webhook missed (while
  the coordinator was down, or a lost delivery). Recovered jobs go through the same required
  labels and label mappings as the webhook (`Webhooks.Labels`), so a scan never takes a job
  a webhook would have ignored.

  `Fleet.Manager` runs it at startup and every `scan_ms` (Rust RECOVERY_SCAN_INTERVAL, 5 min).
  In polling mode `poll/1` is the only source of jobs instead, every poll interval.
  """
  require Logger

  alias KuiperCoordinator.{Forge, Jobs, Runners}
  alias KuiperCoordinator.Forge.QueuedJob
  alias KuiperCoordinator.Jobs.PendingJob
  alias KuiperCoordinator.Webhooks.Labels

  @doc "Rust RECOVERY_SCAN_INTERVAL."
  def default_interval_ms, do: 300_000

  # A job with no repository can't be checked on its forge; dropped once it's this old.
  @max_age_without_repo_s 24 * 3600

  @doc """
  Verify pending jobs, then recover missed ones. `ctx` is the fleet context (`:forges`, and
  optionally `:webhook`, the webhook config; defaults to `config :kuiper_coordinator,
  :webhook`). Returns how many pending jobs were dropped and how many were recovered.
  """
  @spec scan(map()) :: %{dropped: non_neg_integer(), recovered: non_neg_integer()}
  def scan(ctx) do
    dropped = verify_pending(ctx)
    recovered = recover_queued(ctx)
    %{dropped: dropped, recovered: recovered}
  end

  @doc "Drops pending jobs their forge no longer lists as queued or waiting (Rust verify_pending_jobs)."
  def verify_pending(ctx) do
    Jobs.list_pending() |> Enum.count(&(verify_job(&1, ctx) == :dropped))
  end

  defp verify_job(%PendingJob{repository: nil} = job, _ctx) do
    age = DateTime.diff(DateTime.utc_now(), job.inserted_at)

    if age > @max_age_without_repo_s do
      Logger.warning(
        "Pending job #{job.job_key} has no repository and is #{div(age, 3600)}h old - removing as stale"
      )

      drop(job)
    else
      :kept
    end
  end

  defp verify_job(%PendingJob{} = job, %{forges: forges}) do
    case Map.fetch(forges, job.forge) do
      :error ->
        Logger.warning(
          "Pending job #{job.job_key}: unknown forge #{inspect(job.forge)} - removing"
        )

        drop(job)

      {:ok, forge} ->
        case Forge.job_status(forge, job.repository, job_id(job)) do
          {:ok, status} when status in ["queued", "waiting"] ->
            :kept

          {:ok, nil} ->
            Logger.info("Pending job #{job.job_key} not found on its forge - removing")
            drop(job)

          {:ok, status} ->
            Logger.info(
              "Pending job #{job.job_key} is #{inspect(status)} on its forge - removing"
            )

            drop(job)

          {:error, e} ->
            # could be transient: keep it
            Logger.warning("Could not check pending job #{job.job_key}: #{inspect(e)} - keeping")
            :kept
        end
    end
  end

  defp drop(job) do
    Jobs.remove_pending(job.job_key, :gone)
    :dropped
  end

  @doc "Adds queued jobs found on the forges to the pending queue (Rust recover_queued_jobs)."
  def recover_queued(ctx) do
    {_scan, recovered} = list_and_recover(ctx)
    recovered
  end

  @doc """
  Polling mode's scan (Rust scan_forges with `ProvisioningMode::Polling`): the poll itself
  adds newly queued jobs, then only the pending jobs it no longer listed are checked with
  their forge. Jobs on a forge that couldn't be listed are left alone.
  """
  @spec poll(map()) :: %{dropped: non_neg_integer(), recovered: non_neg_integer()}
  def poll(ctx) do
    {scan, recovered} = list_and_recover(ctx)

    dropped =
      case dropped_pending(Jobs.list_pending(), scan) do
        [] ->
          0

        jobs ->
          Logger.info(
            "#{length(jobs)} pending job(s) no longer listed as queued - verifying with their forge"
          )

          Enum.count(jobs, &(verify_job(&1, ctx) == :dropped))
      end

    %{dropped: dropped, recovered: recovered}
  end

  @doc """
  Pending jobs a scan no longer listed as queued, in queue order (Rust dropped_pending_jobs).
  Only jobs on forges the scan listed count: an unreachable forge drops nothing.
  """
  def dropped_pending(pending, %{forges: forges, seen: seen}) do
    Enum.filter(pending, fn job ->
      MapSet.member?(forges, job.forge) and not MapSet.member?(seen, job.job_key)
    end)
  end

  @doc "Rust PollingConfig: 120 s by default, never under 10 s (MIN_POLL_INTERVAL_SECS)."
  def poll_interval_ms(nil), do: 120_000
  def poll_interval_ms(secs) when is_integer(secs), do: max(secs, 10) * 1000

  # Lists every forge's queue, recording which forges answered and every job they listed.
  defp list_and_recover(ctx) do
    config = Map.get_lazy(ctx, :webhook, fn -> webhook_config() end)
    required = Keyword.get(config, :required_labels, Labels.default_required())
    mappings = Keyword.get(config, :label_mappings, [])

    {scan, jobs} =
      ctx.forges
      |> Enum.sort_by(&elem(&1, 0))
      |> Enum.reduce({%{forges: MapSet.new(), seen: MapSet.new()}, []}, fn {name, forge},
                                                                           {scan, acc} ->
        case Forge.list_queued_jobs(forge) do
          {:ok, jobs} ->
            seen = Enum.reduce(jobs, scan.seen, &MapSet.put(&2, "#{name}:#{&1.id}"))

            {%{forges: MapSet.put(scan.forges, name), seen: seen},
             acc ++ Enum.map(jobs, &{name, &1})}

          {:error, e} ->
            Logger.warning("Failed to list queued jobs from forge #{name}: #{inspect(e)}")
            {scan, acc}
        end
      end)

    {scan,
     Enum.count(jobs, fn {name, job} -> recover(name, job, required, mappings) == :added end)}
  end

  defp recover(forge, %QueuedJob{} = job, required, mappings) do
    key = "#{forge}:#{job.id}"

    with {:required, true} <- {:required, Labels.has_required?(job.labels, required)},
         {:runner, []} <- {:runner, Runners.for_job(key)},
         {:ok, scope, group} <-
           Labels.resolve_scope(forge, job.labels, mappings, job.runner_scope),
         {:ok, :inserted} <-
           Jobs.add_pending(%{
             job_key: key,
             forge: forge,
             job_labels: job.labels,
             agent_labels: job.labels,
             runner_scope: scope,
             runner_group: group,
             repository: job.repository,
             queued_at: job.created_at
           }) do
      Logger.info("Recovered queued job #{key} (labels: #{inspect(job.labels)})")
      :added
    else
      {:runner, runners} ->
        Logger.info("Job #{key} already has runner(s) #{inspect(runners)} - skipping recovery")
        :skipped

      {:error, e} ->
        Logger.warning("Could not add recovered job #{key}: #{inspect(e)}")
        :skipped

      _ ->
        :skipped
    end
  end

  defp webhook_config, do: Application.get_env(:kuiper_coordinator, :webhook, [])

  defp job_id(%PendingJob{job_key: key, forge: forge}),
    do: String.replace_prefix(key, forge <> ":", "")
end
