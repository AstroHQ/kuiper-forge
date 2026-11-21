defmodule KuiperCoordinator.Fleet.Manager do
  @moduledoc """
  Runs provisioning (Rust FleetManager::run, parity P1, P6): `:fixed_capacity` reconciles the
  agents' pools each tick, `:webhook` processes pending jobs each tick and whenever a job is
  queued (`{:new_job, key}`) or a pass is asked for (`:reprocess`) on PubSub "fleet".
  Polling mode comes with the Forgejo work.

  Options: `:mode`, `:forges` (`%{name => Forge.t()}`, the first is the default forge for
  fixed capacity), `:runner_scope`, `:tick_ms` (default 30 s, Rust's tick), `:name`.
  Started by the application only when `config :kuiper_coordinator, :fleet` is set.
  """
  use GenServer
  require Logger

  alias KuiperCoordinator.Fleet.{FixedCapacity, OnDemand}

  @modes [:fixed_capacity, :webhook]
  @default_tick_ms 30_000

  def start_link(opts) do
    case Keyword.get(opts, :name, __MODULE__) do
      nil -> GenServer.start_link(__MODULE__, opts)
      name -> GenServer.start_link(__MODULE__, opts, name: name)
    end
  end

  @impl true
  def init(opts) do
    mode = Keyword.get(opts, :mode, :fixed_capacity)

    if mode in @modes do
      forges = Keyword.fetch!(opts, :forges)
      {default_name, default_forge} = default_forge(forges, opts)

      state = %{
        mode: mode,
        tick_ms: Keyword.get(opts, :tick_ms, @default_tick_ms),
        ctx: %{
          forges: forges,
          forge_name: default_name,
          forge: default_forge,
          scope: Keyword.get(opts, :runner_scope),
          async: Keyword.get(opts, :async, true)
        }
      }

      if mode == :webhook,
        do: Phoenix.PubSub.subscribe(KuiperCoordinator.PubSub, "fleet")

      schedule(state)
      {:ok, state}
    else
      {:error, {:unknown_mode, mode}}
    end
  end

  @impl true
  def handle_info(:tick, state) do
    run_pass(state)
    schedule(state)
    {:noreply, state}
  end

  def handle_info({:new_job, _key}, %{mode: :webhook} = state), do: pass(state)
  def handle_info(:reprocess, %{mode: :webhook} = state), do: pass(state)
  def handle_info(_other, state), do: {:noreply, state}

  defp pass(state) do
    run_pass(state)
    {:noreply, state}
  end

  # A pass that raises is logged; the next tick tries again.
  defp run_pass(%{mode: mode, ctx: ctx}) do
    case mode do
      :fixed_capacity -> FixedCapacity.reconcile(ctx)
      :webhook -> OnDemand.process_pending(ctx)
    end
  rescue
    e ->
      Logger.error("fleet #{mode} pass failed: " <> Exception.format(:error, e, __STACKTRACE__))
  end

  defp schedule(%{tick_ms: ms}), do: Process.send_after(self(), :tick, ms)

  defp default_forge(forges, opts) do
    name =
      Keyword.get(opts, :default_forge) || forges |> Map.keys() |> Enum.sort() |> List.first()

    {name, Map.get(forges, name)}
  end
end
