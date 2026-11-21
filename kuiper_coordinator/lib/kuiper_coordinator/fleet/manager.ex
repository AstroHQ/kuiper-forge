defmodule KuiperCoordinator.Fleet.Manager do
  @moduledoc """
  Runs provisioning (Rust FleetManager::run, parity P1, P6): `:fixed_capacity` reconciles the
  agents' pools each tick, `:webhook` processes pending jobs each tick and whenever a job is
  queued (`{:new_job, key}`), a job completes (`{:job_completed, key}`: it leaves the pending
  queue, Rust fleet.rs JobCompleted) or a pass is asked for (`:reprocess`) on PubSub "fleet".
  In webhook mode it also runs the forge scan (`Fleet.Recovery`, Rust scan_forges) at startup,
  after a first pass over jobs left from a previous run, and then on the tick once `scan_ms`
  has passed (Rust run_on_demand_loop). `:polling` is webhook mode without the webhook: the
  scan (`Fleet.Recovery.poll/1`) is the only source of jobs and runs at startup and every
  poll interval (`:poll_interval_s`, default 120, at least 10), with the tick never slower.

  Options: `:mode`, `:forges` (a list of forge specs, built and validated at startup by
  `Fleet.Forges`, fatal like Rust; or ready-made `%{name => Forge.t()}`), `:default_forge` (the
  forge for fixed capacity; default the first spec), `:validate_forges` (default true), `:runner_scope`, `:tick_ms` (default 30 s, Rust's tick), `:scan_ms`
  (default 5 min, Rust RECOVERY_SCAN_INTERVAL; the poll interval in polling mode), `:webhook` (label filters for the scan;
  default `config :kuiper_coordinator, :webhook`), `:name`.
  Started by the application only when `config :kuiper_coordinator, :fleet` is set.
  """
  use GenServer
  require Logger

  alias Cirunner.Agent.RunnerEvent
  alias KuiperCoordinator.{AgentRegistry, Runners}
  alias KuiperCoordinator.Fleet.{FixedCapacity, Forges, OnDemand, Recovery, RunnerEvents}

  @modes [:fixed_capacity, :webhook, :polling]
  @on_demand [:webhook, :polling]
  @default_tick_ms 30_000
  # Rust server.rs AGENT_DISCONNECT_GRACE (S9)
  @disconnect_grace_ms 30_000

  def start_link(opts) do
    case Keyword.get(opts, :name, __MODULE__) do
      nil -> GenServer.start_link(__MODULE__, opts)
      name -> GenServer.start_link(__MODULE__, opts, name: name)
    end
  end

  @impl true
  def init(opts) do
    mode = Keyword.get(opts, :mode, :fixed_capacity)

    with {:mode, true} <- {:mode, mode in @modes},
         {:ok, forges, opts} <- build_forges(Keyword.fetch!(opts, :forges), mode, opts) do
      {default_name, default_forge} = default_forge(forges, opts)

      scan_ms =
        Keyword.get_lazy(opts, :scan_ms, fn ->
          if mode == :polling,
            do: Recovery.poll_interval_ms(opts[:poll_interval_s]),
            else: Recovery.default_interval_ms()
        end)

      # never slower than the poll, so a short poll interval is honoured (Rust check_interval)
      default_tick =
        if mode == :polling, do: min(@default_tick_ms, scan_ms), else: @default_tick_ms

      state = %{
        mode: mode,
        tick_ms: Keyword.get(opts, :tick_ms, default_tick),
        scan_ms: scan_ms,
        last_scan: nil,
        grace_ms: Keyword.get(opts, :disconnect_grace_ms, @disconnect_grace_ms),
        # agent_id => the stamp of its latest disconnect, until it reconnects or fails over
        disconnects: %{},
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

      if mode in @on_demand,
        do: Phoenix.PubSub.subscribe(KuiperCoordinator.PubSub, "fleet")

      # runner events, connects and disconnects (F5, F7, P2, S4, S9)
      Phoenix.PubSub.subscribe(KuiperCoordinator.PubSub, AgentRegistry.topic())

      schedule(state)
      {:ok, state, {:continue, :startup}}
    else
      {:mode, false} -> {:error, {:unknown_mode, mode}}
      {:error, msg} -> {:stop, {:forges, msg}}
    end
  end

  # Ready-made `%{name => {module, client}}` (tests) pass through. A list of specs is built and
  # validated here (fatal, as in Rust), so the clients' caches belong to this process, and the
  # webhook learns its source forge and, for Forgejo, the scopes (`Fleet.Forges`).
  defp build_forges(forges, _mode, opts) when is_map(forges), do: {:ok, forges, opts}

  defp build_forges(specs, mode, opts) when is_list(specs) do
    with {:ok, built} <-
           Forges.build(specs,
             validate: Keyword.get(opts, :validate_forges, true),
             req_options: Keyword.get(opts, :req_options, [])
           ),
         :ok <- check_mode(mode, built),
         :ok <- configure_webhook(mode, built) do
      {name, _} = hd(built)
      {:ok, Map.new(built), Keyword.put_new(opts, :default_forge, name)}
    end
  end

  # C3: fixed capacity needs registration tokens, which Forgejo one-job runners don't have.
  defp check_mode(:fixed_capacity, built) do
    if Enum.any?(built, fn {_, {mod, _}} -> mod == KuiperCoordinator.Forgejo.Client end),
      do: {:error, "fixed_capacity mode doesn't support Forgejo forges (one-job runners only)"},
      else: :ok
  end

  defp check_mode(_mode, _built), do: :ok

  defp configure_webhook(:webhook, built) do
    webhook = Application.get_env(:kuiper_coordinator, :webhook, [])

    with {:ok, webhook} <- Forges.webhook_config(built, webhook) do
      Application.put_env(:kuiper_coordinator, :webhook, webhook)
    end
  end

  defp configure_webhook(_mode, _built), do: :ok

  # Webhook mode: jobs left from a previous run first, then the forge scan (Rust order).
  @impl true
  def handle_continue(:startup, %{mode: mode} = state) when mode in @on_demand do
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

  def handle_info({:new_job, _key}, %{mode: mode} = state) when mode in @on_demand,
    do: pass(state)

  def handle_info(:reprocess, %{mode: mode} = state) when mode in @on_demand, do: pass(state)

  # Rust fleet.rs JobCompleted: the job leaves the pending queue (a job cancelled or finished
  # before a runner took it), and an agent may be free, so pending jobs get a pass.
  def handle_info({:job_completed, key}, %{mode: mode} = state) when mode in @on_demand do
    KuiperCoordinator.Jobs.remove_pending(key)
    pass(state)
  rescue
    e ->
      Logger.error(
        "fleet job_completed #{key} failed: " <> Exception.format(:error, e, __STACKTRACE__)
      )

      {:noreply, state}
  end

  # Rust only records job_started in metrics; the fleet has nothing to do.

  def handle_info({:agent_event, agent_id, {:runner_event, %RunnerEvent{} = ev}}, state) do
    case RunnerEvents.handle(state.ctx, agent_id, ev) do
      :pass -> run_pass(state)
      :ok -> :ok
    end

    {:noreply, state}
  rescue
    e ->
      Logger.error(
        "runner event from #{agent_id} failed: " <> Exception.format(:error, e, __STACKTRACE__)
      )

      {:noreply, state}
  end

  # A (re)connect cancels a pending failover and may bring capacity: a pass (Rust notify, S4, P2).
  def handle_info({:agent_connected, agent_id, _info}, state) do
    state = %{state | disconnects: Map.delete(state.disconnects, agent_id)}
    pass(state)
  end

  # S9: if the agent is still gone after the grace, its runners with jobs are failed over
  # (Destroyed "agent disconnected"), so their jobs can move to other agents.
  def handle_info({:agent_disconnected, agent_id, _reason}, state) do
    stamp = make_ref()
    Process.send_after(self(), {:failover, agent_id, stamp}, state.grace_ms)
    {:noreply, put_in(state, [:disconnects, agent_id], stamp)}
  end

  def handle_info({:failover, agent_id, stamp}, state) do
    # snapshot first, claim second (Rust): runners created after a reconnect aren't in it
    orphaned = Runners.job_runners_for_agent(agent_id)

    if Map.get(state.disconnects, agent_id) == stamp and not AgentRegistry.connected?(agent_id) do
      state = %{state | disconnects: Map.delete(state.disconnects, agent_id)}
      failover(state, agent_id, orphaned)
      {:noreply, state}
    else
      # reconnected, or dropped again and that timer owns the grace
      {:noreply, state}
    end
  end

  def handle_info(_other, state), do: {:noreply, state}

  defp failover(_state, _agent_id, []), do: :ok

  defp failover(state, agent_id, names) do
    Logger.warning(
      "Agent #{agent_id} still offline after #{state.grace_ms} ms - failing its " <>
        "#{length(names)} runner(s) so their jobs can move"
    )

    for name <- names do
      ev = %RunnerEvent{
        runner_name: name,
        vm_id: name,
        event_type: :DESTROYED,
        error: "agent disconnected"
      }

      RunnerEvents.handle(state.ctx, agent_id, ev)
    end

    run_pass(state)
  rescue
    e ->
      Logger.error(
        "failover of #{agent_id} failed: " <> Exception.format(:error, e, __STACKTRACE__)
      )
  end

  defp pass(state) do
    run_pass(state)
    {:noreply, state}
  end

  # A pass that raises is logged; the next tick tries again.
  defp run_pass(%{mode: mode, ctx: ctx}) do
    case mode do
      :fixed_capacity -> FixedCapacity.reconcile(ctx)
      m when m in @on_demand -> OnDemand.process_pending(ctx)
    end
  rescue
    e ->
      Logger.error("fleet #{mode} pass failed: " <> Exception.format(:error, e, __STACKTRACE__))
  end

  defp scan_due?(%{mode: mode, last_scan: nil}) when mode in @on_demand, do: true

  defp scan_due?(%{mode: mode, last_scan: at, scan_ms: ms}) when mode in @on_demand,
    do: System.monotonic_time(:millisecond) - at >= ms

  defp scan_due?(_state), do: false

  # Recovered jobs get a pass straight away (Rust recover_queued_jobs).
  defp scan(state) do
    state = %{state | last_scan: System.monotonic_time(:millisecond)}

    try do
      result =
        if state.mode == :polling, do: Recovery.poll(state.ctx), else: Recovery.scan(state.ctx)

      case result do
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
