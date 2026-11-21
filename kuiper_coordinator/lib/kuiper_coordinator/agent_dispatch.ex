defmodule KuiperCoordinator.AgentDispatch do
  @moduledoc """
  How provisioning reaches agents: reserve a runner slot on an agent that matches the labels,
  send it a CreateRunner/DestroyRunner command over its AgentStream, release the slot. The real
  implementation is backed by the agent registry (D13, D19); provisioning code calls this module
  and tests swap in a double (`config :kuiper_coordinator, :agent_dispatch, MyDouble`).

  Slots are keyed by runner name, like the Rust coordinator's reserve_slot/release_slot (D18).
  Commands are the generated `Cirunner.Agent.*` structs, since the proto is the contract.
  """

  alias Cirunner.Agent.{CommandResult, CreateRunnerCommand}
  alias KuiperCoordinator.Fleet.PoolDefinition

  @type agent_id :: String.t()
  @type runner_name :: String.t()

  @doc """
  Picks a connected agent whose labels cover `labels` and reserves a slot on it for
  `runner_name`, in one atomic call. Options: `:avoid` (agent ids that already failed this job,
  parity R4/J4), `:agent_id` (a specific agent, for explicit pools), `:set_id` (the pool or
  runner set). `{:error, :no_available_agent}` means wait for capacity, not a failure (parity J3).
  """
  @callback reserve_slot(labels :: [String.t()], runner_name(), opts :: keyword()) ::
              {:ok, agent_id()} | {:error, :no_available_agent}

  @doc "Gives back the slot reserved for `runner_name` on `agent_id`."
  @callback release_slot(agent_id(), runner_name()) :: :ok

  @doc "The agent's hostname, for runner names (parity F1)."
  @callback hostname(agent_id()) :: String.t() | nil

  @doc """
  Sends CreateRunner (the registry fills in `command_id`) and waits for the agent's answer,
  like Rust's 30 s ack wait. `:ok` is an accepted Ack; the runner finishing arrives later as
  runner events on PubSub "agents" (D13). Legacy agents answer with a full
  `CommandResult` (success, error, and a CreateRunnerResult) instead (`{:ok, result}`, fleet.rs). `{:error, {:rejected, reason}}` is
  an Ack with accepted=false (parity F4: not a retry).
  """
  @callback dispatch_create(agent_id(), CreateRunnerCommand.t()) ::
              :ok
              | {:ok, CommandResult.t()}
              | {:error, {:rejected, String.t()} | :timeout | :not_connected | :disconnected}

  @doc """
  Pushes DestroyRunner (fire and forget) with `vm_id` set to the VM name exactly as created
  (parity F8: the proxmox agent only destroys a VMID that still belongs to that name).
  """
  @callback dispatch_destroy(agent_id(), vm_name :: String.t()) :: :ok | {:error, term()}

  @doc """
  Fixed-capacity pools from the connected agents (Rust get_pool_definitions, parity R6):
  unavailable agents skipped; agents with explicit pool sizes get one pool per label set,
  the rest share a legacy pool per normalized base-label set (`members` = those agents).
  """
  @callback pool_definitions() :: [PoolDefinition.t()]

  @doc "Free slots for the pool (Rust agent_capacity / legacy_pool_capacity)."
  @callback pool_capacity(PoolDefinition.t()) :: non_neg_integer()

  @doc "A member agent with room for a legacy pool's runner (Rust select_legacy_pool_agent)."
  @callback select_pool_agent(PoolDefinition.t()) :: {:ok, agent_id()} | :none

  def pool_definitions, do: impl().pool_definitions()
  def pool_capacity(%PoolDefinition{} = pool), do: impl().pool_capacity(pool)
  def select_pool_agent(%PoolDefinition{} = pool), do: impl().select_pool_agent(pool)

  def reserve_slot(labels, runner_name, opts \\ []) when is_binary(runner_name),
    do: impl().reserve_slot(labels, runner_name, opts)

  def release_slot(agent_id, runner_name), do: impl().release_slot(agent_id, runner_name)
  def hostname(agent_id), do: impl().hostname(agent_id)

  def dispatch_create(agent_id, %CreateRunnerCommand{} = cmd),
    do: impl().dispatch_create(agent_id, cmd)

  def dispatch_destroy(agent_id, vm_name) when is_binary(vm_name),
    do: impl().dispatch_destroy(agent_id, vm_name)

  defp impl,
    do:
      Application.get_env(
        :kuiper_coordinator,
        :agent_dispatch,
        KuiperCoordinator.AgentDispatch.None
      )
end

defmodule KuiperCoordinator.AgentDispatch.None do
  @moduledoc "Placeholder until the registry-backed dispatch lands: no agents, nothing sent."
  @behaviour KuiperCoordinator.AgentDispatch

  @impl true
  def reserve_slot(_labels, _runner_name, _opts), do: {:error, :no_available_agent}
  @impl true
  def release_slot(_agent_id, _runner_name), do: :ok
  @impl true
  def hostname(_agent_id), do: nil
  @impl true
  def pool_definitions, do: []
  @impl true
  def pool_capacity(_pool), do: 0
  @impl true
  def select_pool_agent(_pool), do: :none
  @impl true
  def dispatch_create(_agent_id, _cmd), do: {:error, :not_connected}
  @impl true
  def dispatch_destroy(_agent_id, _vm_name), do: {:error, :not_connected}
end
