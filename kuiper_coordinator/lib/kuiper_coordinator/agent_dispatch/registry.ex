defmodule KuiperCoordinator.AgentDispatch.Registry do
  @moduledoc """
  The real AgentDispatch (D13, D19): capacity and reservations live in `AgentRegistry`
  (one Capacity per connected agent, from its status), commands go over the agent's stream.
  """
  @behaviour KuiperCoordinator.AgentDispatch

  alias Cirunner.Agent.{CommandAck, CommandResult, CreateRunnerCommand, DestroyRunnerCommand}
  alias KuiperCoordinator.AgentDispatch.Capacity
  alias KuiperCoordinator.AgentRegistry
  alias KuiperCoordinator.Fleet.PoolDefinition

  @impl true
  def reserve_slot(labels, runner_name, opts),
    do: AgentRegistry.reserve(labels, runner_name, opts)

  @impl true
  def select_agent(labels, opts) do
    case Capacity.select(AgentRegistry.capacities(), labels,
           avoid: Keyword.get(opts, :avoid, []),
           include: Capacity.kind_filter(Keyword.get(opts, :kind))
         ) do
      {:ok, id} -> {:ok, id}
      :none -> {:error, :no_available_agent}
    end
  end

  @impl true
  def release_slot(agent_id, runner_name), do: AgentRegistry.release(agent_id, runner_name)

  @impl true
  def hostname(agent_id) do
    case AgentRegistry.lookup(agent_id) do
      %{hostname: h} -> h
      _ -> nil
    end
  end

  @doc "Free slots for `labels` across available agents, reservations counted (for the dashboard)."
  def available_capacity(labels), do: AgentRegistry.available_capacity(labels)

  @impl true
  def pool_definitions, do: Capacity.pool_definitions(AgentRegistry.capacities())

  @impl true
  def unavailable_agents,
    do: for(%Capacity{unavailable: true, agent_id: id} <- AgentRegistry.capacities(), do: id)

  @impl true
  def pool_capacity(%PoolDefinition{agent_id: nil, labels: labels}),
    do: Capacity.legacy_pool_capacity(AgentRegistry.capacities(), labels)

  def pool_capacity(%PoolDefinition{agent_id: id, labels: labels, label_set_id: set_id}) do
    case AgentRegistry.lookup(id) do
      %{capacity: %Capacity{} = cap} -> Capacity.available_capacity(cap, labels, set_id)
      _ -> 0
    end
  end

  @impl true
  def select_pool_agent(%PoolDefinition{agent_id: nil, labels: labels}) do
    caps = AgentRegistry.capacities()

    case Capacity.select(caps, labels, include: &Capacity.in_legacy_pool?(&1, labels)) do
      {:ok, id} -> {:ok, id}
      :none -> :none
    end
  end

  def select_pool_agent(%PoolDefinition{agent_id: id} = pool),
    do: if(pool_capacity(pool) > 0, do: {:ok, id}, else: :none)

  @impl true
  def dispatch_create(agent_id, %CreateRunnerCommand{} = cmd) do
    cid = command_id()
    timeout = Application.get_env(:kuiper_coordinator, :dispatch_ack_timeout_ms, 30_000)

    case AgentRegistry.send_command(
           agent_id,
           cid,
           {:create_runner, %{cmd | command_id: cid}},
           timeout
         ) do
      {:ok, %CommandAck{accepted: true}} -> :ok
      {:ok, %CommandAck{error: error}} -> {:error, {:rejected, error}}
      {:ok, %CommandResult{} = result} -> {:ok, result}
      {:error, reason} -> {:error, reason}
    end
  end

  @impl true
  def dispatch_destroy(agent_id, vm_name),
    do:
      AgentRegistry.push(
        agent_id,
        {:destroy_runner, %DestroyRunnerCommand{command_id: command_id(), vm_id: vm_name}}
      )

  defp command_id, do: Ecto.UUID.generate()
end
