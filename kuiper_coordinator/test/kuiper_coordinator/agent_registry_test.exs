defmodule KuiperCoordinator.AgentRegistryTest do
  use ExUnit.Case, async: false
  @moduletag :capture_log

  alias KuiperCoordinator.AgentRegistry

  # stands in for an AgentStream handler process; supervised, so it goes when the test ends
  defp fake_stream(agent_id) do
    parent = self()

    pid =
      start_supervised!(
        {Task,
         fn ->
           :ok = AgentRegistry.register(agent_id, %{hostname: "h"})
           send(parent, {:registered, self()})

           receive do
             {:disconnect, reason} ->
               send(parent, {:told_to_close, self(), reason})

               receive do
                 :exit -> :ok
               end

             :unregister_and_exit ->
               AgentRegistry.unregister(agent_id)
           end
         end},
        id: make_ref()
      )

    assert_receive {:registered, ^pid}
    pid
  end

  defp idle_process do
    start_supervised!({Task, fn -> receive do: (:never -> :ok) end}, id: make_ref())
  end

  # waits for `pid` to exit, then for the registry to handle its DOWN
  defp await_exit(pid) do
    ref = Process.monitor(pid)
    assert_receive {:DOWN, ^ref, :process, ^pid, _}
    _ = :sys.get_state(AgentRegistry)
  end

  defp stale_after(ms) do
    old = Application.get_env(:kuiper_coordinator, :agent_grpc, [])
    Application.put_env(:kuiper_coordinator, :agent_grpc, stale_after_ms: ms)
    on_exit(fn -> Application.put_env(:kuiper_coordinator, :agent_grpc, old) end)
  end

  defp sweep_at(now) do
    send(AgentRegistry, {:sweep, now})
    _ = :sys.get_state(AgentRegistry)
  end

  setup do
    id = "agent_tart_h_#{System.unique_integer([:positive])}"
    {:ok, id: id}
  end

  # S10 / D5: an old stream's cleanup must not evict a fresh reconnect
  test "a reconnect replaces the entry, and the old stream exiting leaves it alone", %{id: id} do
    old = fake_stream(id)
    new = fake_stream(id)

    assert_receive {:told_to_close, ^old, :superseded}
    assert AgentRegistry.lookup(id).pid == new

    send(old, :exit)
    await_exit(old)
    assert AgentRegistry.lookup(id).pid == new
  end

  test "unregister from a stream that no longer owns the entry is ignored", %{id: id} do
    old = idle_process()
    :ok = GenServer.call(AgentRegistry, {:register, id, old, %{hostname: "h"}})
    new = fake_stream(id)

    # old calls unregister for the same agent id (what Rust's cleanup does)
    :ok = GenServer.call(AgentRegistry, {:unregister, id, old})
    assert AgentRegistry.lookup(id).pid == new

    send(new, :unregister_and_exit)
    await_exit(new)
    assert AgentRegistry.lookup(id) == nil
  end

  test "the entry goes when its stream process dies", %{id: id} do
    pid = fake_stream(id)
    Process.exit(pid, :kill)
    await_exit(pid)
    assert AgentRegistry.lookup(id) == nil
  end

  # S5: silent agents are dropped by the sweep
  test "an agent silent past stale_after_ms is told to close and dropped", %{id: id} do
    stale_after(100)
    pid = fake_stream(id)
    seen = AgentRegistry.lookup(id).last_seen

    sweep_at(seen + 100)
    assert AgentRegistry.lookup(id).pid == pid

    sweep_at(seen + 101)
    assert_receive {:told_to_close, ^pid, :stale}
    assert AgentRegistry.lookup(id) == nil
  end

  test "a touch from another process doesn't refresh last_seen", %{id: id} do
    stale_after(200)
    _pid = fake_stream(id)
    seen = AgentRegistry.lookup(id).last_seen

    AgentRegistry.touch(id)
    _ = :sys.get_state(AgentRegistry)
    assert AgentRegistry.lookup(id).last_seen == seen
  end

  test "touch from the owner refreshes last_seen and keeps the agent past the sweep", %{id: id} do
    stale_after(200)
    pid = fake_stream(id)
    first = AgentRegistry.lookup(id).last_seen

    GenServer.cast(AgentRegistry, {:touch, id, pid, nil})
    _ = :sys.get_state(AgentRegistry)
    seen = AgentRegistry.lookup(id).last_seen
    assert seen >= first

    sweep_at(seen + 200)
    assert AgentRegistry.lookup(id).pid == pid
    sweep_at(seen + 201)
    assert_receive {:told_to_close, ^pid, :stale}
  end
end
