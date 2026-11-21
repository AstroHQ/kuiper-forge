defmodule KuiperCoordinator.Fleet.OnDemand do
  @moduledoc """
  Webhook-mode provisioning (Rust fleet.rs process_pending_jobs / create_runner_on_demand,
  parity J2-J4, F2-F4, F7): one single-use runner per pending job, oldest job first.

  The context is `%{forges: %{name => Forge.t()}}`; `async: false` handles agent replies inline
  (tests). A job leaves the queue when its runner ran it, its forge says it's gone, or it failed
  `Jobs.max_retries/0` times. `:reprocess` on PubSub "fleet" asks for another pass.

  `waiting` (J8): one rule everywhere. A waiting job is pending: it keeps its place and gets no
  runner until it's queued, and a runner finishing while its job waits keeps the job.
  """
  require Logger

  alias KuiperCoordinator.{AgentDispatch, Forge, Jobs, Runners}
  alias KuiperCoordinator.Jobs.PendingJob
  alias KuiperCoordinator.Runners.Naming
  alias Cirunner.Agent.{CommandResult, CreateRunnerCommand}

  @pending_statuses ["queued", "waiting"]

  @doc "One pass over the pending jobs; returns how many runners were sent."
  @spec process_pending(map()) :: %{created: non_neg_integer()}
  def process_pending(ctx) do
    created =
      Jobs.list_pending()
      |> Enum.count(fn job -> process_job(job, ctx) == :created end)

    %{created: created}
  end

  defp process_job(%PendingJob{} = job, ctx) do
    max = Jobs.max_retries()

    cond do
      job.retry_count >= max ->
        Logger.warning("Pending job #{job.job_key} exceeded max retries (#{max}), abandoning")
        Jobs.remove_pending(job.job_key)
        :abandoned

      (runners = Runners.for_job(job.job_key)) != [] ->
        if length(runners) > 1,
          do: Logger.error("Multiple runners for job #{job.job_key}: #{inspect(runners)}")

        :skipped

      true ->
        case create_for_job(job, ctx) do
          :created ->
            :created

          {:error, :no_available_agent} ->
            Logger.info("No available agent for job #{job.job_key} #{inspect(job.agent_labels)}")
            :skipped

          :skipped ->
            :skipped

          {:error, reason} ->
            Logger.warning("Job #{job.job_key}: could not create a runner: #{inspect(reason)}")
            bump(job.job_key, nil)
            :failed
        end
    end
  end

  defp create_for_job(job, ctx) do
    with {:ok, forge} <- forge_for(job, ctx),
         {:ok, agent_id} <- AgentDispatch.select_agent(job.agent_labels, avoid: job.failed_agents),
         name = Naming.new(AgentDispatch.hostname(agent_id)),
         {:ok, ^agent_id} <-
           AgentDispatch.reserve_slot(job.agent_labels, name, agent_id: agent_id) do
      case still_wanted(forge, job) do
        :yes ->
          launch(forge, job, agent_id, name, ctx)

        :not_yet ->
          AgentDispatch.release_slot(agent_id, name)
          :skipped

        :gone ->
          AgentDispatch.release_slot(agent_id, name)
          Jobs.remove_pending(job.job_key)
          :skipped
      end
    end
  end

  # Re-check the forge before committing a runner (F2); a check error proceeds.
  defp still_wanted(_forge, %PendingJob{repository: nil}), do: :yes

  defp still_wanted(forge, job) do
    case Forge.job_status(forge, job.repository, job_id(job)) do
      {:ok, "queued"} ->
        :yes

      {:ok, "waiting"} ->
        Logger.info("Job #{job.job_key} is waiting, not creating a runner yet")
        :not_yet

      {:ok, status} ->
        Logger.info("Job #{job.job_key} is no longer queued (#{inspect(status)}), dropping it")
        :gone

      {:error, e} ->
        Logger.warning("Could not check job #{job.job_key}: #{inspect(e)}, proceeding")
        :yes
    end
  end

  defp launch(forge, job, agent_id, name, ctx) do
    case Forge.create_job_runner(
           forge,
           job.runner_scope,
           name,
           job.agent_labels,
           job.runner_group,
           job_id(job)
         ) do
      {:ok, {:github_jit, jit}} ->
        {:ok, _} =
          Runners.add_runner(%{
            runner_name: name,
            agent_id: agent_id,
            vm_name: name,
            runner_scope: job.runner_scope,
            forge: job.forge,
            job_key: job.job_key,
            job_name: job.job_name,
            repository: job.repository,
            workflow_name: job.workflow_name,
            labels: job.agent_labels
          })

        cmd = %CreateRunnerCommand{
          vm_name: name,
          registration_token: "",
          labels: job.agent_labels,
          runner_scope_url: Forge.scope_url(forge, job.runner_scope),
          jit_config: jit,
          # the agent picks by job labels
          label_set_id: ""
        }

        Logger.info("Sending CreateRunner to #{agent_id} for #{name} (job #{job.job_key})")
        send_create(forge, job, agent_id, cmd, ctx)
        :created

      {:error, _} = err ->
        AgentDispatch.release_slot(agent_id, name)
        err
    end
  end

  defp send_create(forge, job, agent_id, cmd, ctx) do
    run = fn ->
      handle_reply(
        AgentDispatch.dispatch_create(agent_id, cmd),
        forge,
        job,
        agent_id,
        cmd.vm_name
      )
    end

    if Map.get(ctx, :async, true),
      do: Task.Supervisor.start_child(KuiperCoordinator.TaskSupervisor, run),
      else: run.()
  end

  defp handle_reply(:ok, _forge, job, agent_id, name),
    do: Logger.info("CreateRunner accepted by #{agent_id} for #{name} (job #{job.job_key})")

  # Rejected (e.g. at capacity): the job is fine, another agent may take it (F4, no retry).
  defp handle_reply({:error, {:rejected, reason}}, forge, job, agent_id, name) do
    Logger.warning("CreateRunner rejected by #{agent_id} for #{name}: #{reason}")
    cleanup(forge, job, agent_id, name)
    reprocess()
  end

  # A legacy agent answers with the finished runner (F7).
  defp handle_reply({:ok, %CommandResult{success: true}}, forge, job, agent_id, name) do
    case check_after_runner(forge, job) do
      :done ->
        Logger.info("Runner #{name} completed job #{job.job_key}")
        Jobs.remove_pending(job.job_key)
        cleanup(forge, job, agent_id, name)

      :keep ->
        # JIT runners aren't bound to a job: it may have run another one
        cleanup(forge, job, agent_id, name)
        # only once the row is gone, or the pass skips the job for having a runner
        reprocess()
    end
  end

  defp handle_reply({:ok, %CommandResult{error: error}}, forge, job, agent_id, name) do
    Logger.warning("Runner #{name} failed: #{error}")
    bump(job.job_key, agent_id)
    cleanup(forge, job, agent_id, name)
  end

  # Timeout, disconnect, not connected: a command failure, retried avoiding this agent.
  defp handle_reply({:error, reason}, forge, job, agent_id, name) do
    Logger.error("CreateRunner on #{agent_id} for #{name} failed: #{inspect(reason)}")
    cleanup(forge, job, agent_id, name)
    bump(job.job_key, agent_id)
  end

  defp check_after_runner(_forge, %PendingJob{repository: nil}), do: :done

  defp check_after_runner(forge, job) do
    case Forge.job_status(forge, job.repository, job_id(job)) do
      {:ok, status} when status in @pending_statuses -> :keep
      {:ok, _} -> :done
      {:error, _} -> :keep
    end
  end

  defp cleanup(forge, job, agent_id, name) do
    AgentDispatch.release_slot(agent_id, name)

    case Forge.remove_runner(forge, job.runner_scope, name) do
      :ok ->
        :ok

      {:error, e} ->
        Logger.warning("Could not remove runner #{name} from its forge: #{inspect(e)}")
    end

    Runners.remove_runner(name)
  end

  defp bump(job_key, agent_id) do
    case Jobs.record_retry(job_key, agent_id) do
      {:ok, n} ->
        if n >= Jobs.max_retries() do
          Logger.warning("Job #{job_key} failed #{n} times, abandoning")
          Jobs.remove_pending(job_key)
        end

      {:error, :not_found} ->
        :ok
    end
  end

  defp forge_for(%PendingJob{forge: name}, %{forges: forges}) do
    case Map.fetch(forges, name) do
      {:ok, forge} -> {:ok, forge}
      :error -> {:error, {:unknown_forge, name}}
    end
  end

  defp job_id(%PendingJob{job_key: key, forge: forge}),
    do: String.replace_prefix(key, forge <> ":", "")

  defp reprocess, do: Phoenix.PubSub.broadcast(KuiperCoordinator.PubSub, "fleet", :reprocess)
end
