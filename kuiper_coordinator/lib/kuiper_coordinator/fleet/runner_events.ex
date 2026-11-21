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
  happened, so the slot isn't released twice). Idle teardown (F6, F13), drain outcomes and
  run metrics come with their own features.
  """
  require Logger

  alias Cirunner.Agent.RunnerEvent
  alias KuiperCoordinator.{AgentDispatch, Forge, Jobs, Runners}
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

    cond do
      name in [nil, ""] ->
        Logger.warning("Runner event from #{agent_id} has no runner_name or vm_id")
        :ok

      ev.event_type == :STARTED ->
        Logger.info("Runner #{name} started on #{agent_id}")
        :ok

      ev.event_type in @terminal ->
        terminal(ctx, agent_id, name, ev)

      true ->
        Logger.warning("Runner event for #{name} from #{agent_id} has no type")
        :ok
    end
  end

  defp terminal(ctx, agent_id, name, ev) do
    case Runners.get_runner(name) do
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
        Jobs.remove_pending(key)
    end
  end

  defp settle_job(_forge, %Runner{job_key: key} = row, agent_id, ev) do
    case Jobs.record_retry(key, agent_id) do
      {:ok, n} ->
        if n >= Jobs.max_retries() do
          Logger.warning(
            "Runner #{row.runner_name} #{ev.event_type} (#{ev.error}); job #{key} failed #{n} times, abandoning"
          )

          Jobs.remove_pending(key)
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
