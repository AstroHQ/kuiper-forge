defmodule KuiperCoordinator.Agents.Janitor do
  @moduledoc "Hourly purge of expired tokens, long-revoked agents (A13, Rust main.rs:672-690) old or over-cap agent logs (agent_logs.rs prune) and expired admin sessions."
  use GenServer
  require Logger

  @interval :timer.hours(1)

  def start_link(opts), do: GenServer.start_link(__MODULE__, opts, name: __MODULE__)

  @impl true
  def init(opts) do
    interval = Keyword.get(opts, :interval, @interval)
    Process.send_after(self(), :purge, interval)
    {:ok, interval}
  end

  @impl true
  def handle_info(:purge, interval) do
    try do
      %{tokens: t, agents: a} = KuiperCoordinator.Agents.purge_expired()
      if t + a > 0, do: Logger.info("purged #{t} expired token(s), #{a} revoked agent(s)")

      %{old: old, capped: capped} = KuiperCoordinator.Agents.prune_logs()

      if old + capped > 0,
        do: Logger.info("pruned #{old} old and #{capped} over-cap agent log line(s)")

      s = KuiperCoordinator.Admin.purge_sessions()
      if s > 0, do: Logger.info("purged #{s} expired admin session(s)")
    rescue
      e -> Logger.warning("agent purge failed: #{Exception.message(e)}")
    end

    Process.send_after(self(), :purge, interval)
    {:noreply, interval}
  end
end
