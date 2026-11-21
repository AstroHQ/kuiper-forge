defmodule KuiperCoordinator.AgentRegistry do
  @moduledoc """
  Live agent connections (Rust `agent_registry.rs`, connection half).

  Each entry is keyed by agent_id but owned by the AgentStream process that registered it (D5):
  a reconnect replaces the entry and the old stream is told to close, and only the owning process
  can remove it, by unregistering or exiting. So an old stream cleaning up never evicts a fresh
  reconnect (parity S10). Agents silent for `stale_after_ms` are dropped by a sweep (S5).

  Events go out on `KuiperCoordinator.PubSub` topic `"agents"`:
  `{:agent_connected, agent_id, info}`, `{:agent_disconnected, agent_id, reason}`,
  `{:agent_status, agent_id, status}` and `{:agent_event, agent_id, payload}` for command
  results and runner events.

  It also listens there for `{:agent_availability, agent_id, available?}` (the dashboard's
  availability switch, #40): a switched-off agent keeps its stream but gets no new runners,
  including across reconnects and status updates.
  """
  use GenServer
  require Logger

  alias Cirunner.Agent.AgentStatus
  alias KuiperCoordinator.AgentDispatch.Capacity

  @topic "agents"

  def topic, do: @topic

  def start_link(opts), do: GenServer.start_link(__MODULE__, opts, name: __MODULE__)

  @doc "Registers the calling stream process as `agent_id`'s connection."
  def register(agent_id, info),
    do: GenServer.call(__MODULE__, {:register, agent_id, self(), info})

  @doc "Removes `agent_id` only if the calling process owns the entry."
  def unregister(agent_id), do: GenServer.call(__MODULE__, {:unregister, agent_id, self()})

  @doc "Refreshes last_seen (Pong or Status) for the calling stream."
  def touch(agent_id, status \\ nil),
    do: GenServer.cast(__MODULE__, {:touch, agent_id, self(), status})

  @doc "Asks `agent_id`'s live stream to close, e.g. after a revoke."
  def disconnect(agent_id, reason),
    do: GenServer.call(__MODULE__, {:disconnect, agent_id, reason})

  @doc """
  Reserves a slot for `runner_name` atomically (Rust reserve_slot). With `:agent_id` on that
  agent, else on the least-loaded match (`:avoid` agents last). `:set_id` picks a label set.
  """
  def reserve(labels, runner_name, opts \\ []),
    do: GenServer.call(__MODULE__, {:reserve, labels, runner_name, opts})

  @doc "Gives back `runner_name`'s reserved slot on `agent_id`."
  def release(agent_id, runner_name),
    do: GenServer.call(__MODULE__, {:release, agent_id, runner_name})

  @doc """
  Drops `agent_id`'s reservations older than `min_age_ms` whose runner isn't in
  `db_runner_names` (Rust reconcile_reservations, 60 s grace).
  """
  def reconcile(agent_id, db_runner_names, min_age_ms \\ 60_000),
    do: GenServer.cast(__MODULE__, {:reconcile, agent_id, db_runner_names, min_age_ms})

  @doc "Free slots for `labels` across connected, available agents (reservations counted)."
  def available_capacity(labels), do: Capacity.total_capacity(capacities(), labels)

  @doc "Capacity of every connected agent that has reported a status."
  def capacities, do: for(%{capacity: %Capacity{} = c} <- list(), do: c)

  def lookup(agent_id) do
    case :ets.lookup(__MODULE__, agent_id) do
      [{^agent_id, entry}] -> entry
      [] -> nil
    end
  end

  def connected?(agent_id), do: lookup(agent_id) != nil

  def list, do: :ets.tab2list(__MODULE__) |> Enum.map(fn {_id, entry} -> entry end)

  @doc """
  Sends a CoordinatorMessage payload to the agent's stream and waits for the CommandAck (or,
  from legacy agents, the CommandResult) whose command_id matches: `{:ok, ack_or_result}`.
  The timeout starts now, not after queueing (parity S11).
  """
  def send_command(agent_id, command_id, payload, timeout \\ 30_000) do
    case lookup(agent_id) do
      nil ->
        {:error, :not_connected}

      %{pid: pid} ->
        ref = Process.monitor(pid)
        send(pid, {:send_command, self(), ref, command_id, payload})

        receive do
          {^ref, {kind, reply}} when kind in [:ack, :result] ->
            Process.demonitor(ref, [:flush])
            {:ok, reply}

          {:DOWN, ^ref, :process, _, _} ->
            {:error, :disconnected}
        after
          timeout ->
            Process.demonitor(ref, [:flush])
            send(pid, {:forget_command, command_id})
            {:error, :timeout}
        end
    end
  end

  @doc "Fire-and-forget payload (Ping, DestroyRunner)."
  def push(agent_id, payload) do
    case lookup(agent_id) do
      nil ->
        {:error, :not_connected}

      %{pid: pid} ->
        send(pid, {:push, payload})
        :ok
    end
  end

  def broadcast(msg), do: Phoenix.PubSub.broadcast(KuiperCoordinator.PubSub, @topic, msg)

  ## Server

  @impl true
  def init(opts) do
    :ets.new(__MODULE__, [:named_table, :set, :protected, read_concurrency: true])
    :ok = Phoenix.PubSub.subscribe(KuiperCoordinator.PubSub, @topic)
    state = %{monitors: %{}, opts: opts, unavailable: MapSet.new()}
    schedule_sweep(state)
    {:ok, state}
  end

  @impl true
  def handle_call({:register, agent_id, pid, info}, _from, state) do
    state =
      case lookup(agent_id) do
        %{pid: ^pid} ->
          state

        %{pid: old} ->
          send(old, {:disconnect, :superseded})
          demonitor(state, old)

        nil ->
          state
      end

    ref = Process.monitor(pid)
    now = System.monotonic_time(:millisecond)

    entry =
      Map.merge(info, %{
        agent_id: agent_id,
        pid: pid,
        connected_at: DateTime.utc_now(),
        last_seen: now,
        capacity: capacity_for(state, agent_id, nil, info[:status])
      })

    :ets.insert(__MODULE__, {agent_id, entry})
    broadcast({:agent_connected, agent_id, info})
    {:reply, :ok, put_in(state.monitors[pid], {ref, agent_id})}
  end

  def handle_call({:unregister, agent_id, pid}, _from, state) do
    {:reply, :ok, drop_if_owner(state, agent_id, pid, :closed)}
  end

  def handle_call({:disconnect, agent_id, reason}, _from, state) do
    case lookup(agent_id) do
      %{pid: pid} ->
        send(pid, {:disconnect, reason})
        {:reply, :ok, drop_if_owner(state, agent_id, pid, reason)}

      nil ->
        {:reply, {:error, :not_connected}, state}
    end
  end

  def handle_call({:reserve, labels, runner_name, opts}, _from, state) do
    now = System.monotonic_time(:millisecond)
    set_id = Keyword.get(opts, :set_id, "")

    target =
      case Keyword.fetch(opts, :agent_id) do
        {:ok, id} ->
          with %{capacity: %Capacity{} = cap} <- lookup(id),
               true <- Capacity.matches_labels?(cap, labels),
               true <- Capacity.kind_filter(Keyword.get(opts, :kind)).(cap) do
            {:ok, id}
          else
            _ -> :none
          end

        :error ->
          Capacity.select(capacities(), labels,
            avoid: Keyword.get(opts, :avoid, []),
            include: Capacity.kind_filter(Keyword.get(opts, :kind))
          )
      end

    with {:ok, id} <- target,
         %{capacity: cap} = entry <- lookup(id),
         {:ok, cap} <- Capacity.reserve(cap, labels, set_id, runner_name, now) do
      :ets.insert(__MODULE__, {id, %{entry | capacity: cap}})
      {:reply, {:ok, id}, state}
    else
      _ -> {:reply, {:error, :no_available_agent}, state}
    end
  end

  def handle_call({:release, agent_id, runner_name}, _from, state) do
    update_capacity(agent_id, &Capacity.release(&1, runner_name))
    {:reply, :ok, state}
  end

  @impl true
  def handle_cast({:reconcile, agent_id, names, min_age}, state) do
    now = System.monotonic_time(:millisecond)
    update_capacity(agent_id, &Capacity.reconcile(&1, names, min_age, now))
    {:noreply, state}
  end

  def handle_cast({:touch, agent_id, pid, status}, state) do
    case lookup(agent_id) do
      %{pid: ^pid} = entry ->
        entry = %{entry | last_seen: System.monotonic_time(:millisecond)}

        entry =
          if status,
            do:
              Map.merge(entry, %{
                status: status,
                vm_names: Enum.map(status.vms, & &1.name),
                capacity: capacity_for(state, agent_id, entry[:capacity], status)
              }),
            else: entry

        :ets.insert(__MODULE__, {agent_id, entry})
        if status, do: broadcast({:agent_status, agent_id, status})

      _ ->
        :ok
    end

    {:noreply, state}
  end

  @impl true
  def handle_info({:DOWN, _ref, :process, pid, _reason}, state) do
    case Map.pop(state.monitors, pid) do
      {{_ref, agent_id}, monitors} ->
        {:noreply, drop_if_owner(%{state | monitors: monitors}, agent_id, pid, :disconnected)}

      {nil, _} ->
        {:noreply, state}
    end
  end

  def handle_info({:agent_availability, agent_id, available?}, state) do
    update_capacity(agent_id, &%{&1 | unavailable: not available?})

    unavailable =
      if available?,
        do: MapSet.delete(state.unavailable, agent_id),
        else: MapSet.put(state.unavailable, agent_id)

    {:noreply, %{state | unavailable: unavailable}}
  end

  def handle_info(:sweep, state) do
    state = sweep(state, System.monotonic_time(:millisecond))
    schedule_sweep(state)
    {:noreply, state}
  end

  # A sweep as of `now` (monotonic ms); tests send it to sweep at a set time without sleeping.
  def handle_info({:sweep, now}, state), do: {:noreply, sweep(state, now)}

  # Our own broadcasts on the "agents" topic come back to us too.
  def handle_info(_msg, state), do: {:noreply, state}

  defp sweep(state, now) do
    stale_after = setting(:stale_after_ms, 120_000)

    Enum.reduce(list(), state, fn entry, acc ->
      if now - entry.last_seen > stale_after do
        Logger.warning("agent #{entry.agent_id} silent for #{stale_after} ms, dropping it")
        send(entry.pid, {:disconnect, :stale})
        drop_if_owner(acc, entry.agent_id, entry.pid, :stale)
      else
        acc
      end
    end)
  end

  # Capacity from a status: fresh on connect, refreshed after (reservations kept), and
  # reservations whose VM the agent now reports are settled (Rust settle_reservations).
  defp capacity_for(_state, _agent_id, _old, nil), do: nil

  defp capacity_for(state, agent_id, old, %AgentStatus{} = status) do
    cap =
      case old do
        %Capacity{} ->
          Capacity.update_status(old, status)

        nil ->
          %{
            Capacity.from_status(agent_id, status)
            | unavailable: MapSet.member?(state.unavailable, agent_id)
          }
      end

    Capacity.settle(cap, Enum.map(status.vms, & &1.name))
  end

  defp update_capacity(agent_id, fun) do
    case lookup(agent_id) do
      %{capacity: %Capacity{} = cap} = entry ->
        :ets.insert(__MODULE__, {agent_id, %{entry | capacity: fun.(cap)}})

      _ ->
        :ok
    end
  end

  defp drop_if_owner(state, agent_id, pid, reason) do
    case lookup(agent_id) do
      %{pid: ^pid} ->
        :ets.delete(__MODULE__, agent_id)
        broadcast({:agent_disconnected, agent_id, reason})
        demonitor(state, pid)

      _ ->
        state
    end
  end

  defp demonitor(state, pid) do
    case Map.pop(state.monitors, pid) do
      {{ref, _}, monitors} ->
        Process.demonitor(ref, [:flush])
        %{state | monitors: monitors}

      {nil, _} ->
        state
    end
  end

  defp schedule_sweep(_state),
    do: Process.send_after(self(), :sweep, setting(:sweep_interval_ms, 60_000))

  defp setting(key, default) do
    :kuiper_coordinator |> Application.get_env(:agent_grpc, []) |> Keyword.get(key, default)
  end
end
