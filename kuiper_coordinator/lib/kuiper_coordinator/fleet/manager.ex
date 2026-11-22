defmodule KuiperCoordinator.Fleet.Manager do
  @moduledoc """
  Runs provisioning (Rust FleetManager::run, parity P1, P6). Each forge has its own mode, and
  the manager runs them side by side:

    * `:fixed_capacity` - the agents' pools are reconciled each tick, with runners registered
      on that forge. The pools name no forge, so one forge at most is in this mode.
    * `:webhook` - pending jobs are processed each tick and whenever a job is queued
      (`{:new_job, key}`), a job completes (`{:job_completed, key}`: it leaves the pending
      queue, Rust fleet.rs JobCompleted) or a pass is asked for (`:reprocess`) on PubSub
      "fleet". The forge scan (`Fleet.Recovery.scan/2`, Rust scan_forges) runs at startup,
      after a first pass over jobs left from a previous run, and then on the tick once the
      scan interval has passed (Rust run_on_demand_loop).
    * `:polling` - webhook mode without the webhook: the scan (`Fleet.Recovery.poll/2`) is the
      forge's only source of jobs and runs at startup and every poll interval, with the tick
      never slower.

  Options: `:forges` (a list of forge specs, built and validated at startup by `Fleet.Forges`,
  fatal like Rust; or ready-made `%{name => Forge.t()}`). A spec carries its forge's `:mode`,
  `:runner_scope` (fixed capacity) and `:poll_interval_s` (polling: default 120, at least 10).
  `:mode`, `:runner_scope` and `:poll_interval_s` as options are for the forges that don't say
  (every ready-made forge, say), and `:modes` (`%{name => mode}`) sets ready-made forges apart.
  `:default_forge` (which fixed-capacity forge fills the pools, when several are; default the
  first), `:validate_forges` (default true), `:tick_ms` (default 30 s, Rust's tick), `:scan_ms`
  (every forge's scan interval; default 5 min for webhook forges, Rust RECOVERY_SCAN_INTERVAL,
  and the poll interval for polling ones), `:webhook` (label filters for the scan; default
  `config :kuiper_coordinator, :webhook`), `:idle_runner_timeout_s` (webhook and polling
  forges: Forgejo one-job runners idle past it are torn down, `Fleet.Idle`; default 600, 0 off,
  at least 120), `:name`.
  Started (and restarted on every forge or settings change) by `Fleet.Loader`, from the forges
  stored in Postgres (D39).
  """
  use GenServer
  require Logger

  alias Cirunner.Agent.RunnerEvent
  alias KuiperCoordinator.{AgentRegistry, Runners}

  alias KuiperCoordinator.Fleet.{
    Drain,
    FixedCapacity,
    Forges,
    Idle,
    OnDemand,
    Recovery,
    RunnerEvents
  }

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
    with {:ok, forges, order, settings, opts} <- build_forges(Keyword.fetch!(opts, :forges), opts),
         nil <-
           Enum.find_value(order, &if(settings[&1].mode not in @modes, do: settings[&1].mode)) do
      on_demand? = Enum.any?(settings, fn {_, s} -> s.mode in @on_demand end)
      fixed = fixed_capacity_forge(order, settings, opts)
      scans = scans(settings, opts)

      # never slower than the shortest poll, so it is honoured (Rust check_interval)
      default_tick =
        Enum.min([@default_tick_ms | for({_, %{kind: :polling, every_ms: ms}} <- scans, do: ms)])

      state = %{
        modes: Map.new(settings, fn {name, s} -> {name, s.mode} end),
        on_demand?: on_demand?,
        tick_ms: Keyword.get(opts, :tick_ms, default_tick),
        # name => %{kind: :webhook | :polling, every_ms: ms, last: monotonic ms | nil}
        scans: scans,
        grace_ms: Keyword.get(opts, :disconnect_grace_ms, @disconnect_grace_ms),
        # agent_id => the stamp of its latest disconnect, until it reconnects or fails over
        disconnects: %{},
        # fixed capacity: runner_name => when DestroyRunner was sent to drain it (F16)
        draining: %{},
        # idle teardown of one-job runners (F13-F15); nil when off or with no on-demand forge
        idle: if(on_demand?, do: Idle.new(Keyword.get(opts, :idle_runner_timeout_s)), else: nil),
        # `forge_name`, `forge` and `scope` are the fixed-capacity forge's; nil with none
        ctx: %{
          forges: forges,
          forge_name: fixed,
          forge: fixed && Map.get(forges, fixed),
          scope: fixed && settings[fixed].runner_scope,
          async: Keyword.get(opts, :async, true)
        }
      }

      state =
        case Keyword.fetch(opts, :webhook) do
          {:ok, webhook} -> put_in(state, [:ctx, :webhook], webhook)
          :error -> state
        end

      if on_demand?, do: Phoenix.PubSub.subscribe(KuiperCoordinator.PubSub, "fleet")

      # runner events, connects and disconnects (F5, F7, P2, S4, S9)
      Phoenix.PubSub.subscribe(KuiperCoordinator.PubSub, AgentRegistry.topic())

      schedule(state)
      {:ok, state, {:continue, :startup}}
    else
      {:error, msg} -> {:stop, {:forges, msg}}
      mode -> {:error, {:unknown_mode, mode}}
    end
  end

  # Ready-made `%{name => {module, client}}` (tests) pass through. A list of specs is built and
  # validated here (fatal, as in Rust), so the clients' caches belong to this process, and the
  # webhook learns each source's kind and, for Forgejo, the scopes (`Fleet.Forges`).
  # Returns the forges, their names in config order, and each one's provisioning settings.
  defp build_forges(forges, opts) when is_map(forges) do
    modes = Keyword.get(opts, :modes, %{})
    order = forges |> Map.keys() |> Enum.sort()
    settings = Map.new(order, &{&1, provisioning(%{mode: modes[&1]}, opts)})
    {:ok, forges, order, settings, opts}
  end

  defp build_forges(specs, opts) when is_list(specs) do
    specs = Enum.map(specs, &Map.new/1)
    settings = Map.new(specs, &{&1[:name], provisioning(&1, opts)})

    with {:ok, built} <-
           Forges.build(specs,
             validate: Keyword.get(opts, :validate_forges, true),
             req_options: Keyword.get(opts, :req_options, [])
           ),
         :ok <- check_fixed_capacity(built, settings),
         :ok <- configure_webhook(built, settings) do
      {:ok, Map.new(built), Enum.map(built, &elem(&1, 0)), settings, opts}
    end
  end

  defp provisioning(spec, opts) do
    %{
      mode: spec[:mode] || Keyword.get(opts, :mode, :fixed_capacity),
      runner_scope: spec[:runner_scope] || Keyword.get(opts, :runner_scope),
      poll_interval_s: spec[:poll_interval_s] || Keyword.get(opts, :poll_interval_s)
    }
  end

  # C3: fixed capacity needs registration tokens, which Forgejo one-job runners don't have.
  defp check_fixed_capacity(built, settings) do
    forgejo? = fn {name, {mod, _}} ->
      mod == KuiperCoordinator.Forgejo.Client and settings[name].mode == :fixed_capacity
    end

    if Enum.any?(built, forgejo?),
      do: {:error, "fixed_capacity mode doesn't support Forgejo forges (one-job runners only)"},
      else: :ok
  end

  defp configure_webhook(built, settings) do
    if Enum.any?(settings, fn {_, s} -> s.mode == :webhook end) do
      webhook = Application.get_env(:kuiper_coordinator, :webhook, [])

      with {:ok, webhook} <- Forges.webhook_config(built, webhook) do
        Application.put_env(:kuiper_coordinator, :webhook, webhook)
      end
    else
      :ok
    end
  end

  defp fixed_capacity_forge(order, settings, opts) do
    fixed = Enum.filter(order, &(settings[&1].mode == :fixed_capacity))
    default = Keyword.get(opts, :default_forge)
    if default in fixed, do: default, else: List.first(fixed)
  end

  defp scans(settings, opts) do
    for {name, %{mode: mode} = s} <- settings, mode in @on_demand, into: %{} do
      every =
        Keyword.get_lazy(opts, :scan_ms, fn ->
          if mode == :polling,
            do: Recovery.poll_interval_ms(s.poll_interval_s),
            else: Recovery.default_interval_ms()
        end)

      {name, %{kind: mode, every_ms: every, last: nil}}
    end
  end

  # On-demand forges: jobs left from a previous run first, then the forge scan (Rust order).
  @impl true
  def handle_continue(:startup, %{on_demand?: true} = state) do
    run_pass(state)
    {:noreply, scan(state, Map.keys(state.scans))}
  end

  def handle_continue(:startup, state), do: {:noreply, state}

  @impl true
  def handle_info(:tick, state) do
    state = drain(state)
    run_pass(state)
    state = check_idle(state)
    state = scan(state, due_scans(state))
    schedule(state)
    {:noreply, state}
  end

  def handle_info({:new_job, _key}, %{on_demand?: true} = state), do: pass(state)

  def handle_info(:reprocess, %{on_demand?: true} = state), do: pass(state)

  # Rust fleet.rs JobCompleted: the job leaves the pending queue (a job cancelled or finished
  # before a runner took it), and an agent may be free, so pending jobs get a pass.
  def handle_info({:job_completed, key}, %{on_demand?: true} = state) do
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
    name = if ev.runner_name in [nil, ""], do: ev.vm_id, else: ev.runner_name
    now = System.monotonic_time(:millisecond)

    {result, idle} =
      cond do
        name in [nil, ""] ->
          {RunnerEvents.handle(state.ctx, agent_id, ev), state.idle}

        ev.event_type == :STARTED ->
          {RunnerEvents.handle(state.ctx, agent_id, ev),
           Idle.runner_started(state.idle, name, now)}

        ev.event_type in [:COMPLETED, :FAILED, :DESTROYED] ->
          case Idle.on_terminal(state.idle, name, now) do
            {:idled, idle} ->
              {RunnerEvents.idled(state.ctx, agent_id, name), idle}

            {:late, idle} ->
              Logger.debug("Late #{ev.event_type} for torn-down idle runner #{name}, ignoring")
              {:ok, idle}

            {:normal, idle} ->
              {RunnerEvents.handle(state.ctx, agent_id, ev), idle}
          end

        true ->
          {RunnerEvents.handle(state.ctx, agent_id, ev), state.idle}
      end

    state = %{state | idle: idle}
    if result == :pass, do: run_pass(state)
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

  # A check that raises is logged and keeps the old state; the next tick tries again.
  defp check_idle(%{idle: nil} = state), do: state

  defp check_idle(state) do
    %{state | idle: Idle.check(state.idle, state.ctx, System.monotonic_time(:millisecond))}
  rescue
    e ->
      Logger.error("fleet idle check failed: " <> Exception.format(:error, e, __STACKTRACE__))
      state
  end

  # Fixed capacity drains switched-off agents before the reconcile (Rust reconcile order, F16).
  # A drain that raises is logged and keeps the old state; the next tick tries again.
  defp drain(%{ctx: %{forge: nil}} = state), do: state

  defp drain(state) do
    %{state | draining: Drain.run(state.draining, state.ctx, System.monotonic_time(:millisecond))}
  rescue
    e ->
      Logger.error("fleet drain failed: " <> Exception.format(:error, e, __STACKTRACE__))
      state
  end

  # The pools first, then pending jobs. A pass that raises is logged; the next tick tries again.
  defp run_pass(%{ctx: ctx} = state) do
    ctx =
      Map.put(
        ctx,
        :backing_off,
        Idle.backing_off(Map.get(state, :idle), System.monotonic_time(:millisecond))
      )

    if ctx.forge, do: guarded("fixed_capacity", fn -> FixedCapacity.reconcile(ctx) end)
    if state.on_demand?, do: guarded("on-demand", fn -> OnDemand.process_pending(ctx) end)
  end

  defp guarded(what, fun) do
    fun.()
  rescue
    e ->
      Logger.error("fleet #{what} pass failed: " <> Exception.format(:error, e, __STACKTRACE__))
  end

  defp due_scans(%{scans: scans}) do
    now = System.monotonic_time(:millisecond)
    for {name, %{last: last, every_ms: ms}} <- scans, last == nil or now - last >= ms, do: name
  end

  # Polling forges are polled, webhook forges get the recovery scan. Recovered jobs get a pass
  # straight away (Rust recover_queued_jobs).
  defp scan(state, []), do: state

  defp scan(state, names) do
    now = System.monotonic_time(:millisecond)

    scans =
      Map.new(state.scans, fn {n, s} -> {n, if(n in names, do: %{s | last: now}, else: s)} end)

    state = %{state | scans: scans}
    {polled, scanned} = Enum.split_with(names, &(scans[&1].kind == :polling))

    try do
      results =
        if(polled == [], do: [], else: [Recovery.poll(state.ctx, polled)]) ++
          if(scanned == [], do: [], else: [Recovery.scan(state.ctx, scanned)])

      if Enum.any?(results, &(&1.recovered > 0)), do: run_pass(state)
    rescue
      e ->
        Logger.error("fleet forge scan failed: " <> Exception.format(:error, e, __STACKTRACE__))
    end

    state
  end

  defp schedule(%{tick_ms: ms}), do: Process.send_after(self(), :tick, ms)
end
