defmodule KuiperCoordinator.Fleet.Manager do
  @moduledoc """
  Runs provisioning (Rust FleetManager::run, parity P1, P6): `:fixed_capacity` reconciles the
  agents' pools each tick, `:webhook` processes pending jobs each tick and whenever a job is
  queued (`{:new_job, key}`) or a pass is asked for (`:reprocess`) on PubSub "fleet".
  In webhook mode it also runs the forge scan (`Fleet.Recovery`, Rust scan_forges) at startup,
  after a first pass over jobs left from a previous run, and then on the tick once `scan_ms`
  has passed (Rust run_on_demand_loop). Polling mode comes with the Forgejo work.

  Options: `:mode`, `:forges` (`%{name => Forge.t()}`, the first is the default forge for
  fixed capacity), `:runner_scope`, `:tick_ms` (default 30 s, Rust's tick), `:scan_ms`
  (default 5 min, Rust RECOVERY_SCAN_INTERVAL), `:webhook` (label filters for the scan;
  default `config :kuiper_coordinator, :webhook`), `:name`.
  Started by the application only when `config :kuiper_coordinator, :fleet` is set.
  """
  use GenServer
  require Logger

  alias KuiperCoordinator.Fleet.{FixedCapacity, OnDemand, Recovery}

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
        scan_ms: Keyword.get(opts, :scan_ms, Recovery.default_interval_ms()),
        last_scan: nil,
        ctx: %{
          forges: forges,
          forge_name: default_name,
          forge: default_forge,
          scope: Keyword.get(opts, :runner_scope),
          async: Keyword.get(opts, :async, true)
        }
      }

      state =
        case Keyword.fetch(opts, :webhook) do
          {:ok, webhook} -> put_in(state, [:ctx, :webhook], webhook)
          :error -> state
        end

      if mode == :webhook,
        do: Phoenix.PubSub.subscribe(KuiperCoordinator.PubSub, "fleet")

      schedule(state)
      {:ok, state, {:continue, :startup}}
    else
      {:error, {:unknown_mode, mode}}
    end
  end

  # Webhook mode: jobs left from a previous run first, then the forge scan (Rust order).
  @impl true
  def handle_continue(:startup, %{mode: :webhook} = state) do
    run_pass(state)
    {:noreply, scan(state)}
  end

  def handle_continue(:startup, state), do: {:noreply, state}

  @impl true
  def handle_info(:tick, state) do
    run_pass(state)
    state = if scan_due?(state), do: scan(state), else: state
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

  defp scan_due?(%{mode: :webhook, last_scan: nil}), do: true

  defp scan_due?(%{mode: :webhook, last_scan: at, scan_ms: ms}),
    do: System.monotonic_time(:millisecond) - at >= ms

  defp scan_due?(_state), do: false

  # Recovered jobs get a pass straight away (Rust recover_queued_jobs).
  defp scan(state) do
    state = %{state | last_scan: System.monotonic_time(:millisecond)}

    try do
      case Recovery.scan(state.ctx) do
        %{recovered: n} when n > 0 -> run_pass(state)
        _ -> :ok
      end
    rescue
      e ->
        Logger.error("fleet forge scan failed: " <> Exception.format(:error, e, __STACKTRACE__))
    end

    state
  end

  defp schedule(%{tick_ms: ms}), do: Process.send_after(self(), :tick, ms)

  defp default_forge(forges, opts) do
    name =
      Keyword.get(opts, :default_forge) || forges |> Map.keys() |> Enum.sort() |> List.first()

    {name, Map.get(forges, name)}
  end
end
