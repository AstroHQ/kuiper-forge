defmodule KuiperCoordinator.Fleet.RunnerEventsTest do
  # Rust fleet.rs handle_runner_event (parity F5, F7) and the S9 disconnect failover
  # (server.rs:644-682, agent_registry.rs:519-597). Nye's Forgejo read (Nov 21), item 3.
  use KuiperCoordinator.DataCase, async: false
  import Mox

  alias Cirunner.Agent.RunnerEvent

  alias KuiperCoordinator.{
    AgentDispatchMock,
    AgentFailures,
    AgentRegistry,
    ForgeMock,
    Jobs,
    Runners
  }

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

  describe "run history (MX1-MX3)" do
    alias KuiperCoordinator.RunHistory

    defp requested(name, job_key) do
      RunHistory.runner_requested_on("a1", %{runner_name: name, job_key: job_key, labels: ["x"]})
    end

    test "Started, timings and Completed fill the runner's row; the job is recorded as ran" do
      key = job(31)
      runner("r31", "a1", key)
      requested("r31", key)
      expect_cleanup("a1", "r31")
      expect(ForgeMock, :job_status, fn :client, "astro/app", "31" -> {:ok, "completed"} end)

      started = %{
        event("r31", :STARTED)
        | timings: %Cirunner.Agent.RunnerTimings{steps_ms: %{"boot" => 900}, image: "macos-15"}
      }

      assert :ok = RunnerEvents.handle(@ctx, "a1", started)

      done = %{
        event("r31", :COMPLETED)
        | timings: %Cirunner.Agent.RunnerTimings{steps_ms: %{"job" => 5_000}}
      }

      assert :pass = RunnerEvents.handle(@ctx, "a1", done)

      [run] = RunHistory.runs_since(0)
      assert run.hostname == "a1" and run.agent_type == "unknown"
      assert run.started_at && run.finished_at
      assert run.outcome == "completed"
      assert run.image == "macos-15"
      assert run.steps == %{"boot" => 900, "job" => 5_000}
      assert [%{job_key: ^key, outcome: "ran"}] = RunHistory.jobs_since(0)
    end

    test "a tracked runner destroyed mid-job is lost; a plain pool teardown is destroyed" do
      key = job(32)
      runner("r32", "a1", key)
      runner("p1", "a1", nil)
      requested("r32", key)
      requested("p1", nil)
      expect_cleanup("a1", "r32")
      expect_cleanup("a1", "p1")
      stub(AgentDispatchMock, :hostname, fn _ -> "mac-1" end)

      RunnerEvents.handle(@ctx, "a1", event("r32", :DESTROYED))
      RunnerEvents.handle(@ctx, "a1", event("p1", :DESTROYED))

      outcomes = Map.new(RunHistory.runs_since(0), &{&1.runner_name, &1.outcome})
      assert outcomes == %{"r32" => "lost", "p1" => "destroyed"}
    end

    test "an idled runner is recorded as idled and its job stays pending" do
      key = job(33)
      runner("r33", "a1", key)
      requested("r33", key)
      expect_cleanup("a1", "r33")

      assert :pass = RunnerEvents.idled(@ctx, "a1", "r33")
      assert [%{outcome: "idled"}] = RunHistory.runs_since(0)
      assert [%{outcome: nil}] = RunHistory.jobs_since(0)
    end

    test "an abandoned job is recorded as abandoned" do
      key = job(34)
      runner("r34", "a1", key)
      expect_cleanup("a1", "r34")
      stub(AgentDispatchMock, :hostname, fn _ -> "mac-1" end)

      for _ <- 1..(Jobs.max_retries() - 1), do: Jobs.record_retry(key, "a0")
      RunnerEvents.handle(@ctx, "a1", event("r34", :FAILED, "boom"))

      assert Jobs.get_pending(key) == nil
      assert [%{outcome: "abandoned", attempts: 0}] = RunHistory.jobs_since(0)
    end
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

  describe "agent failures (Rust fleet.rs 1800-1888, #33)" do
    defp failures,
      do:
        AgentFailures.recent_all(nil, nil, 10)
        |> Enum.map(&{&1.agent_id, &1.kind, &1.runner_name, &1.job_key, &1.message})
        |> Enum.sort()

    test "Failed is runner_failed, with its job; Destroyed mid-job or with an error is runner_lost" do
      key = job(1)
      runner("r1", "a1", key)
      expect_cleanup("a1", "r1")
      RunnerEvents.handle(@ctx, "a1", event("r1", :FAILED, "vm crashed"))

      runner("r2", "a2", key)
      expect_cleanup("a2", "r2")
      RunnerEvents.handle(@ctx, "a2", event("r2", :DESTROYED))

      runner("p1", "a3", nil)
      expect_cleanup("a3", "p1")
      RunnerEvents.handle(@ctx, "a3", event("p1", :DESTROYED, "disk gone"))

      assert failures() == [
               {"a1", "runner_failed", "r1", key, "vm crashed"},
               {"a2", "runner_lost", "r2", key, "runner destroyed before finishing"},
               {"a3", "runner_lost", "p1", nil, "disk gone"}
             ]
    end

    test "a plain Destroyed of a jobless runner and a Completed record nothing" do
      runner("p1", "a1", nil)
      expect_cleanup("a1", "p1")
      RunnerEvents.handle(@ctx, "a1", event("p1", :DESTROYED))
      runner("p2", "a1", nil)
      expect_cleanup("a1", "p2")
      RunnerEvents.handle(@ctx, "a1", event("p2", :COMPLETED))
      assert failures() == []
    end

    test "Failed for a runner already cleaned up is still recorded; Destroyed is not" do
      RunnerEvents.handle(@ctx, "a1", event("gone", :FAILED, "destroy failed"))
      RunnerEvents.handle(@ctx, "a1", event("gone2", :DESTROYED, "late"))
      assert failures() == [{"a1", "runner_failed", "gone", nil, "destroy failed"}]
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
