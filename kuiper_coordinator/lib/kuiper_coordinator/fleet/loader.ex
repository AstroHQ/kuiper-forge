defmodule KuiperCoordinator.Fleet.Loader do
  @moduledoc """
  Runs the fleet from the stored forges and settings (D39). At start, and on every
  `{:forges_changed}` (`KuiperCoordinator.Forges.topic/0`), it reads `Forges.fleet_config/0`;
  when that differs from what's running it stops the old `Fleet.Manager` and starts a new one,
  and sets `config :kuiper_coordinator, :webhook` for the webhook plugs. No forges, or forges
  and settings that don't fit together, mean no fleet (the reason is logged and kept in
  `status/1`). Forges were checked live when saved, so they aren't checked again here: a forge
  that's down at boot doesn't stop the app (unlike Rust's fatal validate).

  Options: `:name`, `:manager` (extra options for `Fleet.Manager`, for tests).
  """
  use GenServer
  require Logger

  alias KuiperCoordinator.{Fleet, Forges}

  def start_link(opts) do
    GenServer.start_link(__MODULE__, opts, name: Keyword.get(opts, :name, __MODULE__))
  end

  @doc "`:off`, `{:error, reason}` or `{:running, %{forge_name => mode}, manager_pid}`."
  def status(server \\ __MODULE__), do: GenServer.call(server, :status)

  @impl true
  def init(opts) do
    Process.flag(:trap_exit, true)
    Forges.subscribe()
    state = %{config: nil, manager: nil, status: :off, extra: Keyword.get(opts, :manager, [])}
    {:ok, state, {:continue, :load}}
  end

  @impl true
  def handle_continue(:load, state), do: {:noreply, load(state)}

  @impl true
  def handle_info({:forges_changed}, state), do: {:noreply, load(state)}

  # the manager died on its own: start it again on the same config after a pause
  def handle_info({:EXIT, pid, reason}, %{manager: pid} = state) do
    Logger.error("Fleet manager exited (#{inspect(reason)}); starting it again in 5 s")
    Process.send_after(self(), :restart, 5_000)
    {:noreply, %{state | manager: nil, config: nil, status: {:error, "restarting"}}}
  end

  def handle_info(:restart, %{manager: nil} = state), do: {:noreply, load(state)}

  def handle_info(_msg, state), do: {:noreply, state}

  @impl true
  def handle_call(:status, _from, state), do: {:reply, state.status, state}

  @impl true
  def terminate(_reason, state), do: stop(state)

  defp load(state) do
    case Forges.fleet_config() do
      {:ok, config} when config == state.config ->
        state

      {:ok, config} ->
        state |> stop() |> start(config)

      {:error, msg} ->
        Logger.error("Provisioning is off: #{msg}")
        state = stop(state)
        Application.put_env(:kuiper_coordinator, :webhook, [])
        %{state | config: nil, status: {:error, msg}}
    end
  end

  defp start(state, %{fleet: nil} = config) do
    Application.put_env(:kuiper_coordinator, :webhook, [])
    %{state | config: config, status: :off}
  end

  defp start(state, %{fleet: fleet, webhook: webhook} = config) do
    Application.put_env(:kuiper_coordinator, :webhook, webhook)
    opts = fleet |> Keyword.merge(validate_forges: false) |> Keyword.merge(state.extra)

    case Fleet.Manager.start_link(opts) do
      {:ok, pid} ->
        modes = Map.new(fleet[:forges], &{&1.name, &1.mode})

        Logger.info(
          "Fleet running: " <>
            Enum.map_join(modes, ", ", fn {name, mode} -> "#{name} #{mode}" end)
        )

        %{state | config: config, manager: pid, status: {:running, modes, pid}}

      {:error, reason} ->
        msg = "the fleet didn't start: #{inspect(reason)}"
        Logger.error(msg)
        Application.put_env(:kuiper_coordinator, :webhook, [])
        %{state | config: config, status: {:error, msg}}
    end
  end

  defp stop(%{manager: nil} = state), do: state

  defp stop(%{manager: pid} = state) do
    ref = Process.monitor(pid)
    Process.exit(pid, :shutdown)

    receive do
      {:DOWN, ^ref, _, _, _} -> :ok
    after
      10_000 -> Process.exit(pid, :kill)
    end

    receive do
      {:EXIT, ^pid, _} -> :ok
    after
      0 -> :ok
    end

    %{state | manager: nil}
  end
end
