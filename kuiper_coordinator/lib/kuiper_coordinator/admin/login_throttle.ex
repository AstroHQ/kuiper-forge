defmodule KuiperCoordinator.Admin.LoginThrottle do
  @moduledoc """
  Failed admin logins per client address (UI10: the Rust admin has no rate limit). After
  `@max_failures` failures inside `@window_ms`, that address is refused until the window ends.
  A successful login clears its count. An ETS table owned by this process; per node.
  """
  use GenServer

  @table __MODULE__
  @max_failures 10
  @window_ms 5 * 60 * 1000

  def start_link(_), do: GenServer.start_link(__MODULE__, [], name: __MODULE__)

  @doc "true when `key` may try to log in now."
  def allowed?(key) do
    case :ets.lookup(@table, key) do
      [{^key, count, since}] -> count < @max_failures or now() - since > @window_ms
      [] -> true
    end
  end

  def failed(key) do
    t = now()

    case :ets.lookup(@table, key) do
      [{^key, _count, since}] when t - since > @window_ms -> :ets.insert(@table, {key, 1, t})
      [{^key, _, _}] -> :ets.update_counter(@table, key, {2, 1})
      [] -> :ets.insert(@table, {key, 1, t})
    end

    :ok
  end

  def succeeded(key), do: :ets.delete(@table, key) && :ok

  @doc false
  def reset, do: :ets.delete_all_objects(@table) && :ok

  defp now, do: System.monotonic_time(:millisecond)

  @impl true
  def init(_) do
    :ets.new(@table, [:named_table, :public, :set, write_concurrency: true])
    :timer.send_interval(@window_ms, :sweep)
    {:ok, nil}
  end

  @impl true
  def handle_info(:sweep, state) do
    cutoff = now() - @window_ms
    :ets.select_delete(@table, [{{:_, :_, :"$1"}, [{:<, :"$1", cutoff}], [true]}])
    {:noreply, state}
  end
end
