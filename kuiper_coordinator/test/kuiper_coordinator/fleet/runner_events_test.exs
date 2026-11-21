defmodule KuiperCoordinator.Fleet.RunnerEventsTest do
  # Rust fleet.rs handle_runner_event (parity F5, F7) and the S9 disconnect failover
  # (server.rs:644-682, agent_registry.rs:519-597). Nye's Forgejo read (Nov 21), item 3.
  use KuiperCoordinator.DataCase, async: false
  import Mox

  alias Cirunner.Agent.RunnerEvent
  alias KuiperCoordinator.{AgentDispatchMock, AgentRegistry, ForgeMock, Jobs, Runners}
  alias KuiperCoordinator.Fleet.{Manager, RunnerEvents}

  setup :set_mox_global
  setup :verify_on_exit!

  @scope {:org, "astro"}
  @ctx %{forges: %{"github" => {ForgeMock, :client}}, async: false}

  setup do
    stub(ForgeMock, :runner_kind, fn _ -> "github" end)
    :ok
  end

  defp job(n) do
    {:ok, _} =
      Jobs.add_pending(%{
        job_key: "github:#{n}",
        forge: "github",
        runner_scope: @scope,
        repository: "astro/app"
      })

    "github:#{n}"
  end

  defp runner(name, agent, job_key) do
    {:ok, _} =
      Runners.add_runner(%{
        runner_name: name,
        agent_id: agent,
        vm_name: name,
        runner_scope: @scope,
        forge: "github",
        job_key: job_key,
        repository: job_key && "astro/app"
      })

    name
  end

  defp event(name, type, error \\ ""),
    do: %RunnerEvent{runner_name: name, vm_id: name, event_type: type, error: error}

  defp expect_cleanup(agent, name) do
    expect(AgentDispatchMock, :release_slot, fn ^agent, ^name -> :ok end)
    expect(ForgeMock, :remove_runner, fn :client, @scope, ^name -> :ok end)
  end

  describe "a terminal event for a tracked runner (F5, F7)" do
    test "Completed and the job is done on the forge: the job leaves the queue" do
      key = job(1)
      runner("r1", "a1", key)
      expect_cleanup("a1", "r1")
      expect(ForgeMock, :job_status, fn :client, "astro/app", "1" -> {:ok, "completed"} end)

      assert :pass = RunnerEvents.handle(@ctx, "a1", event("r1", :COMPLETED))
      assert Jobs.get_pending(key) == nil
      assert Runners.get_runner("r1") == nil
    end

    test "Completed but the job is still queued (it ran a sibling) or unverified: the job stays" do
      k1 = job(1)
      k2 = job(2)
      runner("r1", "a1", k1)
      runner("r2", "a1", k2)
      expect_cleanup("a1", "r1")
      expect_cleanup("a1", "r2")
      expect(ForgeMock, :job_status, fn :client, _, "1" -> {:ok, "queued"} end)
      expect(ForgeMock, :job_status, fn :client, _, "2" -> {:error, :timeout} end)

      assert :pass = RunnerEvents.handle(@ctx, "a1", event("r1", :COMPLETED))
      assert :pass = RunnerEvents.handle(@ctx, "a1", event("r2", :COMPLETED))
      assert %{retry_count: 0} = Jobs.get_pending(k1)
      assert %{retry_count: 0} = Jobs.get_pending(k2)
      assert Runners.get_runner("r1") == nil and Runners.get_runner("r2") == nil
    end

    test "Failed / Destroyed retry the job avoiding that agent, and abandon it at max retries" do
      key = job(1)

      for {n, type} <- [{1, :FAILED}, {2, :DESTROYED}] do
        runner("r#{n}", "a#{n}", key)
        expect_cleanup("a#{n}", "r#{n}")
        assert :pass = RunnerEvents.handle(@ctx, "a#{n}", event("r#{n}", type, "boom"))
      end

      assert %{retry_count: 2, failed_agents: ["a1", "a2"]} = Jobs.get_pending(key)

      runner("r3", "a3", key)
      expect_cleanup("a3", "r3")
      RunnerEvents.handle(@ctx, "a3", event("r3", :FAILED))
      assert Jobs.get_pending(key) == nil
    end

    test "a runner with no job (fixed capacity) is just cleaned up" do
      runner("p1", "a1", nil)
      expect_cleanup("a1", "p1")
      assert :pass = RunnerEvents.handle(@ctx, "a1", event("p1", :DESTROYED))
      assert Runners.get_runner("p1") == nil
    end
  end

  test "an event for a runner already cleaned up only asks for a pass (no double release)" do
    # no release_slot / remove_runner expectations: Mox fails the test if either is called
    assert :pass = RunnerEvents.handle(@ctx, "a1", event("gone", :DESTROYED))
  end

  test "Started is logged, the name falls back to vm_id, and a nameless event is ignored" do
    assert :ok = RunnerEvents.handle(@ctx, "a1", event("r1", :STARTED))
    runner("vm-only", "a1", nil)
    expect_cleanup("a1", "vm-only")

    assert :pass =
             RunnerEvents.handle(@ctx, "a1", %RunnerEvent{
               vm_id: "vm-only",
               event_type: :COMPLETED
             })

    assert :ok = RunnerEvents.handle(@ctx, "a1", %RunnerEvent{event_type: :COMPLETED})
  end

  describe "the fleet manager" do
    defp start(opts) do
      opts =
        Keyword.merge(
          [
            name: nil,
            mode: :webhook,
            tick_ms: 60_000,
            scan_ms: 60_000,
            forges: @ctx.forges,
            async: false,
            disconnect_grace_ms: 50
          ],
          opts
        )

      stub(ForgeMock, :list_queued_jobs, fn :client -> {:ok, []} end)
      # the startup scan verifies pending jobs: still queued
      stub(ForgeMock, :job_status, fn :client, _, _ -> {:ok, "queued"} end)
      stub(AgentDispatchMock, :select_agent, fn _, _ -> {:error, :no_available_agent} end)
      pid = start_supervised!({Manager, opts})
      Ecto.Adapters.SQL.Sandbox.allow(KuiperCoordinator.Repo, self(), pid)
      :sys.get_state(pid)
      pid
    end

    defp broadcast(msg),
      do: Phoenix.PubSub.broadcast(KuiperCoordinator.PubSub, AgentRegistry.topic(), msg)

    test "handles runner events from the agents topic" do
      key = job(1)
      runner("r1", "a1", key)
      pid = start([])
      expect_cleanup("a1", "r1")
      broadcast({:agent_event, "a1", {:runner_event, event("r1", :FAILED, "vm crashed")}})
      :sys.get_state(pid)
      assert Runners.get_runner("r1") == nil
      assert %{retry_count: 1, failed_agents: ["a1"]} = Jobs.get_pending(key)
    end

    test "S9: an agent still gone after the grace has its job runners failed over" do
      key = job(1)
      runner("r1", "a1", key)
      runner("pool1", "a1", nil)
      runner("other", "a2", job(2))
      pid = start([])
      expect_cleanup("a1", "r1")

      broadcast({:agent_disconnected, "a1", :closed})
      Process.sleep(120)
      :sys.get_state(pid)

      assert Runners.get_runner("r1") == nil
      assert %{retry_count: 1, failed_agents: ["a1"]} = Jobs.get_pending(key)
      # a runner with no job, and other agents' runners, are left alone
      assert Runners.get_runner("pool1")
      assert Runners.get_runner("other")
    end

    test "S9: a reconnect within the grace keeps the runners" do
      key = job(1)
      runner("r1", "a1", key)
      pid = start([])

      broadcast({:agent_disconnected, "a1", :closed})
      broadcast({:agent_connected, "a1", %{}})
      Process.sleep(120)
      :sys.get_state(pid)

      assert Runners.get_runner("r1")
      assert %{retry_count: 0} = Jobs.get_pending(key)
    end

    test "S9: a second disconnect owns the grace (the first timer does nothing)" do
      key = job(1)
      runner("r1", "a1", key)
      pid = start(disconnect_grace_ms: 80)

      broadcast({:agent_disconnected, "a1", :closed})
      broadcast({:agent_connected, "a1", %{}})
      Process.sleep(40)
      broadcast({:agent_disconnected, "a1", :closed})
      # the first timer fires here (80 ms after the first disconnect) and must not fail over
      Process.sleep(50)
      :sys.get_state(pid)
      assert Runners.get_runner("r1")

      expect_cleanup("a1", "r1")
      Process.sleep(60)
      :sys.get_state(pid)
      assert Runners.get_runner("r1") == nil
      assert %{retry_count: 1} = Jobs.get_pending(key)
    end
  end
end
