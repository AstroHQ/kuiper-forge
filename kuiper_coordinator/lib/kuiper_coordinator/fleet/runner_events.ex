defmodule KuiperCoordinator.Fleet.RunnerEvents do
  @moduledoc """
  An agent's runner lifecycle events (Rust fleet.rs `handle_runner_event`, parity F5, F7).

  `Started` is logged. A terminal event (`Completed`, `Failed`, `Destroyed`) for a runner we
  track releases its slot, settles its job and removes the row, then the runner is removed
  from its forge in the background and pending jobs get a pass (only now, so the job is
  eligible again):

    * `Completed` on a job: asks the forge (`check_job_after_runner`): still queued means the
      runner ran a sibling job, so the job stays pending for a fresh runner; unknown keeps it
      (never delete on a guess); otherwise it ran and leaves the queue. No repository: trust
      the runner.
    * `Failed` / `Destroyed` on a job: a retry against that agent; abandoned at
      `Jobs.max_retries/0`.

  A terminal event for a runner no longer tracked only asks for a pass (cleanup already
  happened, so the slot isn't released twice). A runner torn down for idling (`Fleet.Idle`)
  goes through `idled/3` instead: cleaned up, its job untouched. Every event also feeds the
  run history (timings, started, outcome); drain outcomes come with F16.
  """
  require Logger

  alias Cirunner.Agent.RunnerEvent
  alias KuiperCoordinator.{AgentDispatch, AgentFailures, Forge, Jobs, Runners, RunHistory}
  alias KuiperCoordinator.Runners.Runner

  @terminal [:COMPLETED, :FAILED, :DESTROYED]
  @pending_statuses ["queued", "waiting"]

  @doc """
  Handles one event from `agent_id`. `ctx` has `:forges` (`%{name => Forge.t()}`) and
  optionally `:async` (default true: forge removal in a task). Returns `:pass` when pending
  jobs should get a pass, else `:ok`.
  """
  @spec handle(map(), String.t(), RunnerEvent.t()) :: :ok | :pass
  def handle(ctx, agent_id, %RunnerEvent{} = ev) do
    name = if ev.runner_name in [nil, ""], do: ev.vm_id, else: ev.runner_name
    # the event carrying the final timings can come after the one that finished the runner
    if ev.timings && name not in [nil, ""], do: RunHistory.runner_timings(name, ev.timings)

    cond do
      name in [nil, ""] ->
        Logger.warning("Runner event from #{agent_id} has no runner_name or vm_id")
        :ok

      ev.event_type == :STARTED ->
        Logger.info("Runner #{name} started on #{agent_id}")
        RunHistory.runner_started(name)
        :ok

      ev.event_type in @terminal ->
        terminal(ctx, agent_id, name, ev)

      true ->
        Logger.warning("Runner event for #{name} from #{agent_id} has no type")
        :ok
    end
  end

  @doc """
  A runner we destroyed for idling ended (Rust: RunOutcome::Idled): slot, row and forge
  registration go, but its job keeps its place and backoff, no retry counted. Returns `:pass`.
  """
  @spec idled(map(), String.t(), String.t()) :: :pass
  def idled(ctx, agent_id, name) do
    case Runners.get_runner(name) do
      nil ->
        :pass

      %Runner{} = row ->
        AgentDispatch.release_slot(agent_id, name)
        RunHistory.runner_finished(name, :idled, "")
        Runners.remove_runner(name)
        Logger.info("Idle runner #{name} on #{agent_id} torn down (job #{row.job_key})")
        remove_from_forge(ctx, Map.get(ctx.forges, row.forge), row)
        :pass
    end
  end

  defp terminal(ctx, agent_id, name, ev) do
    row = Runners.get_runner(name)
    record_failure(agent_id, name, row, ev)
    record_outcome(name, row, ev)

    case row do
      nil ->
        Logger.debug("#{ev.event_type} for #{name}, already cleaned up")
        :pass

      %Runner{} = row ->
        AgentDispatch.release_slot(agent_id, name)
        forge = Map.get(ctx.forges, row.forge)
        settle_job(forge, row, agent_id, ev)
        Runners.remove_runner(name)
        remove_from_forge(ctx, forge, row)
        :pass
    end
  end

  # Rust fleet.rs 1800-1888 (#33): Failed is recorded even for a runner already cleaned up (a
  # failed destroy lands after it); Destroyed is "lost" only for a tracked runner that had a job
  # or came with an error (a plain fixed-capacity destroy is normal).
  defp record_failure(agent_id, name, row, %{event_type: :FAILED} = ev),
    do: AgentFailures.record(agent_id, :runner_failed, name, row && row.job_key, ev.error)

  defp record_failure(agent_id, name, %Runner{} = row, %{event_type: :DESTROYED} = ev)
       when not is_nil(row.job_key) or ev.error not in [nil, ""] do
    message = if ev.error in [nil, ""], do: "runner destroyed before finishing", else: ev.error
    AgentFailures.record(agent_id, :runner_lost, name, row.job_key, message)
  end

  defp record_failure(_agent_id, _name, _row, _ev), do: :ok

  # Rust fleet.rs 1800-1888 run outcome: an untracked runner gets Completed/Failed only (first
  # finish wins anyway); a tracked one that was destroyed is lost if it had a job or an error,
  # else a normal pool teardown. Drained comes with F16.
  defp record_outcome(name, row, %{event_type: type} = ev) do
    outcome =
      case {type, row} do
        {:COMPLETED, _} -> :completed
        {:FAILED, _} -> :failed
        {:DESTROYED, nil} -> nil
        {:DESTROYED, %Runner{job_key: key}} when not is_nil(key) -> :lost
        {:DESTROYED, _} -> if ev.error in [nil, ""], do: :destroyed, else: :lost
      end

    if outcome, do: RunHistory.runner_finished(name, outcome, ev.error)
  end

  defp settle_job(_forge, %Runner{job_key: nil} = row, agent_id, ev) do
    case ev.event_type do
      :FAILED -> Logger.warning("Runner #{row.runner_name} on #{agent_id} failed: #{ev.error}")
      other -> Logger.info("Runner #{row.runner_name} on #{agent_id}: #{other}")
    end
  end

  defp settle_job(forge, %Runner{job_key: key} = row, agent_id, %{event_type: :COMPLETED}) do
    case check_job_after_runner(forge, row) do
      :queued ->
        Logger.warning(
          "Runner #{row.runner_name} completed but job #{key} is still queued on its forge - " <>
            "it ran another job, re-provisioning"
        )

      :unknown ->
        Logger.info(
          "Runner #{row.runner_name} completed but job #{key} is unverified - keeping it pending"
        )

      :done ->
        Logger.info("Runner #{row.runner_name} on #{agent_id} completed job #{key}")
        Jobs.remove_pending(key, :ran)
    end
  end

  defp settle_job(_forge, %Runner{job_key: key} = row, agent_id, ev) do
    case Jobs.record_retry(key, agent_id) do
      {:ok, n} ->
        if n >= Jobs.max_retries() do
          Logger.warning(
            "Runner #{row.runner_name} #{ev.event_type} (#{ev.error}); job #{key} failed #{n} times, abandoning"
          )

          Jobs.remove_pending(key, :abandoned)
        else
          Logger.warning(
            "Runner #{row.runner_name} #{ev.event_type} (#{ev.error}); job #{key} will be retried (#{n}/#{Jobs.max_retries()})"
          )
        end

      {:error, :not_found} ->
        Logger.warning(
          "Runner #{row.runner_name} #{ev.event_type} (#{ev.error}); job #{key} no longer pending"
        )
    end
  end

  # Rust check_job_after_runner: no repository to ask with means trust the runner; a forge
  # that's gone means nobody can run the job here.
  defp check_job_after_runner(_forge, %Runner{repository: nil}), do: :done
  defp check_job_after_runner(nil, _row), do: :done

  defp check_job_after_runner(forge, %Runner{job_key: key, forge: name, repository: repo}) do
    case Forge.job_status(forge, repo, String.replace_prefix(key, name <> ":", "")) do
      {:ok, status} when status in @pending_statuses ->
        :queued

      {:ok, _} ->
        :done

      {:error, e} ->
        Logger.warning("Could not verify job #{key} after its runner finished: #{inspect(e)}")
        :unknown
    end
  end

  defp remove_from_forge(_ctx, nil, row),
    do: Logger.warning("Runner #{row.runner_name}'s forge #{inspect(row.forge)} isn't configured")

  defp remove_from_forge(ctx, forge, row) do
    run = fn ->
      case Forge.remove_runner(forge, row.runner_scope, row.runner_name) do
        :ok ->
          :ok

        {:error, e} ->
          Logger.warning(
            "Could not remove runner #{row.runner_name} from its forge: #{inspect(e)}"
          )
      end
    end

    if Map.get(ctx, :async, true),
      do: Task.Supervisor.start_child(KuiperCoordinator.TaskSupervisor, run),
      else: run.()
  end
end
