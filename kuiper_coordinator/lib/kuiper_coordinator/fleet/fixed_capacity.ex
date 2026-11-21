defmodule KuiperCoordinator.Fleet.FixedCapacity do
  @moduledoc """
  Fixed-capacity provisioning (Rust fleet.rs reconcile_pool, parity P2-P5): keep each pool the
  agents define topped up to its target with registration-token runners.

  The context is `%{scope: RunnerScope.t() | nil, forge_name: String.t(), forge: Forge.t()}`,
  the default (first) forge; `async: false` handles agent replies inline (tests).
  """
  require Logger

  alias KuiperCoordinator.{AgentDispatch, AgentFailures, Forge, Runners, RunHistory}
  alias KuiperCoordinator.Fleet.PoolDefinition
  alias KuiperCoordinator.Runners.Naming
  alias Cirunner.Agent.{CommandResult, CreateRunnerCommand}

  @doc "Reconciles every pool. With no `[runner_scope]` nothing is created (P2)."
  @spec reconcile(map()) :: {:ok, non_neg_integer()} | {:error, :no_runner_scope}
  def reconcile(%{scope: nil}) do
    Logger.error("fixed capacity: no runner_scope configured, creating no runners")
    {:error, :no_runner_scope}
  end

  def reconcile(ctx) do
    created =
      AgentDispatch.pool_definitions()
      |> Enum.map(fn pool ->
        {:ok, n} = reconcile_pool(pool, ctx)
        n
      end)
      |> Enum.sum()

    {:ok, created}
  end

  @doc """
  Runners a pool has, from the runner rows (P3): an explicit pool counts its agent's runners
  tagged with its key, a legacy pool every runner of its member agents.
  """
  @spec current_count(PoolDefinition.t()) :: non_neg_integer()
  def current_count(%PoolDefinition{agent_id: nil, members: members}),
    do: Runners.count_for_agents(members)

  def current_count(%PoolDefinition{agent_id: agent_id} = pool),
    do: Runners.count_for_pool(agent_id, PoolDefinition.key(pool))

  @doc "Creates up to `min(target - current, capacity)` runners; returns how many were sent."
  @spec reconcile_pool(PoolDefinition.t(), map()) :: {:ok, non_neg_integer()}
  def reconcile_pool(%PoolDefinition{} = pool, ctx) do
    current = current_count(pool)

    if current >= pool.target_count do
      {:ok, 0}
    else
      needed = pool.target_count - current

      case AgentDispatch.pool_capacity(pool) do
        0 ->
          Logger.warning(
            "pool #{inspect(pool.labels)}: need #{needed} more runners but no agent capacity"
          )

          {:ok, 0}

        capacity ->
          {:ok, create(pool, min(needed, capacity), ctx)}
      end
    end
  end

  defp create(pool, to_create, ctx) do
    Enum.reduce_while(1..to_create//1, 0, fn _, sent ->
      case create_one(pool, ctx) do
        :sent -> {:cont, sent + 1}
        :next -> {:cont, sent}
        :stop -> {:halt, sent}
      end
    end)
  end

  defp create_one(pool, ctx) do
    with {:agent, {:ok, agent_id}} <- {:agent, pick_agent(pool)},
         name = Naming.new(AgentDispatch.hostname(agent_id)),
         {:reserve, {:ok, agent_id}} <-
           {:reserve,
            AgentDispatch.reserve_slot(pool.labels, name,
              agent_id: agent_id,
              set_id: pool.label_set_id
            )},
         {:token, {:ok, token}} <- {:token, token(ctx, agent_id, name)},
         {:row, {:ok, _}, _, _} <-
           {:row,
            Runners.add_runner(%{
              runner_name: name,
              agent_id: agent_id,
              vm_name: name,
              runner_scope: ctx.scope,
              forge: ctx.forge_name,
              labels: pool.labels,
              pool: if(PoolDefinition.explicit?(pool), do: PoolDefinition.key(pool))
            }), agent_id, name} do
      cmd = %CreateRunnerCommand{
        vm_name: name,
        registration_token: token,
        labels: pool.labels,
        runner_scope_url: Forge.scope_url(ctx.forge, ctx.scope),
        jit_config: "",
        label_set_id: pool.label_set_id
      }

      RunHistory.runner_requested_on(agent_id, %{
        runner_name: name,
        forge: ctx.forge_name,
        labels: pool.labels,
        pool: PoolDefinition.key(pool)
      })

      send_create(agent_id, cmd, ctx)
      :sent
    else
      {:agent, _} ->
        Logger.warning("pool #{inspect(pool.labels)}: no available agent")
        :stop

      {:reserve, _} ->
        Logger.warning("pool #{inspect(pool.labels)}: could not reserve a slot")
        :next

      {:token, _} ->
        :stop

      # F8: no row, no CreateRunner; the slot goes back so the agent keeps its capacity
      {:row, {:error, reason}, agent_id, name} ->
        Logger.error(
          "pool #{inspect(pool.labels)}: could not record runner #{name}: #{inspect(reason)}"
        )

        AgentDispatch.release_slot(agent_id, name)
        :stop
    end
  end

  defp pick_agent(%PoolDefinition{agent_id: nil} = pool),
    do: AgentDispatch.select_pool_agent(pool)

  defp pick_agent(%PoolDefinition{agent_id: id}), do: {:ok, id}

  defp token(ctx, agent_id, name) do
    case Forge.registration_token(ctx.forge, ctx.scope) do
      {:ok, token} ->
        {:ok, token}

      {:error, reason} ->
        Logger.error("fixed capacity: registration token failed: #{inspect(reason)}")
        AgentDispatch.release_slot(agent_id, name)
        :error
    end
  end

  defp send_create(agent_id, cmd, ctx) do
    run = fn -> handle_reply(AgentDispatch.dispatch_create(agent_id, cmd), agent_id, cmd, ctx) end

    if Map.get(ctx, :async, true) do
      Task.Supervisor.start_child(KuiperCoordinator.TaskSupervisor, run)
    else
      run.()
    end
  end

  defp handle_reply(:ok, agent_id, cmd, _ctx) do
    Logger.info("CreateRunner accepted by agent #{agent_id} for #{cmd.vm_name}")
    RunHistory.runner_acked(cmd.vm_name)
  end

  # A legacy agent answers with the finished runner: it's gone, clean up after it.
  defp handle_reply({:ok, %CommandResult{} = result}, agent_id, cmd, ctx) do
    outcome = if result.success, do: :completed, else: :failed
    RunHistory.runner_finished(cmd.vm_name, outcome, result.error)

    unless result.success do
      Logger.warning("runner #{cmd.vm_name} failed: #{result.error}")
      AgentFailures.record(agent_id, :runner_failed, cmd.vm_name, nil, result.error)
    end

    cleanup(agent_id, cmd.vm_name, ctx)
  end

  # Rejected, timed out or disconnected: no reprocess in fixed capacity (P5); the next
  # reconcile tops the pool up again.
  defp handle_reply({:error, {:rejected, reason}}, agent_id, cmd, ctx) do
    Logger.warning("CreateRunner rejected by #{agent_id} for #{cmd.vm_name}: #{reason}")
    AgentFailures.record(agent_id, :command_rejected, cmd.vm_name, nil, reason)
    RunHistory.runner_finished(cmd.vm_name, :rejected, reason)
    cleanup(agent_id, cmd.vm_name, ctx)
  end

  defp handle_reply({:error, reason}, agent_id, cmd, ctx) do
    Logger.warning("CreateRunner on #{agent_id} for #{cmd.vm_name}: #{inspect(reason)}")

    AgentFailures.record(
      agent_id,
      :command_failed,
      cmd.vm_name,
      nil,
      "CreateRunner: #{inspect(reason)}"
    )

    RunHistory.runner_finished(cmd.vm_name, :command_failed, "CreateRunner: #{inspect(reason)}")
    cleanup(agent_id, cmd.vm_name, ctx)
  end

  defp cleanup(agent_id, name, ctx) do
    AgentDispatch.release_slot(agent_id, name)

    case Forge.remove_runner(ctx.forge, ctx.scope, name) do
      :ok ->
        :ok

      {:error, e} ->
        Logger.warning("could not remove runner #{name} from its forge: #{inspect(e)}")
    end

    Runners.remove_runner(name)
  end
end
