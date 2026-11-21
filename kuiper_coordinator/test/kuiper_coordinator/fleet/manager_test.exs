defmodule KuiperCoordinator.Fleet.ManagerTest do
  use KuiperCoordinator.DataCase, async: false
  import Mox

  alias KuiperCoordinator.{AgentDispatchMock, ForgeMock, Jobs}
  alias KuiperCoordinator.Fleet.Manager
  alias KuiperCoordinator.Forge.QueuedJob

  setup :set_mox_global
  setup :verify_on_exit!

  setup do
    stub(ForgeMock, :runner_kind, fn _ -> "github" end)
    :ok
  end

  @scope {:org, "astro"}
  @forges %{"github" => {ForgeMock, :client}}

  defp start(opts) do
    opts = Keyword.merge([name: nil, tick_ms: 60_000, forges: @forges, async: false], opts)
    pid = start_supervised!({Manager, opts})
    Ecto.Adapters.SQL.Sandbox.allow(KuiperCoordinator.Repo, self(), pid)
    pid
  end

  test "fixed capacity reconciles the pools on each tick (P1)" do
    test = self()

    expect(AgentDispatchMock, :pool_definitions, 2, fn ->
      send(test, :reconciled)
      []
    end)

    pid = start(mode: :fixed_capacity, runner_scope: @scope)
    send(pid, :tick)
    assert_receive :reconciled
    send(pid, :tick)
    assert_receive :reconciled
  end

  test "fixed capacity with no runner scope creates nothing" do
    pid = start(mode: :fixed_capacity, runner_scope: nil)
    send(pid, :tick)
    # no pool_definitions expectation: Mox fails the test if it's called
    assert %{mode: :fixed_capacity} = :sys.get_state(pid)
  end

  test "webhook mode processes pending jobs on a new job, a reprocess and a tick (P6, W7)" do
    {:ok, _} =
      Jobs.add_pending(%{job_key: "github:1", forge: "github", runner_scope: @scope})

    test = self()
    stub(ForgeMock, :list_queued_jobs, fn :client -> {:ok, []} end)

    # startup pass, then one each for the new job, the reprocess and the tick
    expect(AgentDispatchMock, :select_agent, 4, fn _, _ ->
      send(test, :processed)
      {:error, :no_available_agent}
    end)

    pid = start(mode: :webhook, scan_ms: 60_000)
    assert_receive :processed
    Phoenix.PubSub.broadcast(KuiperCoordinator.PubSub, "fleet", {:new_job, "github:1"})
    assert_receive :processed
    Phoenix.PubSub.broadcast(KuiperCoordinator.PubSub, "fleet", :reprocess)
    assert_receive :processed
    send(pid, :tick)
    assert_receive :processed
    # unrelated fleet events don't trigger a pass
    Phoenix.PubSub.broadcast(KuiperCoordinator.PubSub, "fleet", {:job_completed, "github:1"})
    :sys.get_state(pid)
    refute_received :processed
  end

  test "a crashing pass doesn't take the manager down" do
    expect(AgentDispatchMock, :pool_definitions, fn -> raise "boom" end)
    pid = start(mode: :fixed_capacity, runner_scope: @scope)
    send(pid, :tick)
    assert %{mode: :fixed_capacity} = :sys.get_state(pid)
    assert Process.alive?(pid)
  end

  test "an unknown mode is refused at start" do
    assert {:error, {:unknown_mode, :polling_v2}} =
             Manager.init(mode: :polling_v2, forges: @forges, tick_ms: 1)
  end

  describe "forge scan (Rust scan_forges in run_on_demand_loop)" do
    test "at startup: jobs left from a previous run first, then the scan, then recovered jobs" do
      {:ok, _} =
        Jobs.add_pending(%{
          job_key: "github:1",
          forge: "github",
          runner_scope: @scope,
          repository: "astro/app"
        })

      test = self()

      expect(AgentDispatchMock, :select_agent, 2, fn labels, _ ->
        send(test, {:pass, labels})
        {:error, :no_available_agent}
      end)

      expect(ForgeMock, :job_status, fn :client, "astro/app", "1" ->
        send(test, :verified)
        {:ok, "completed"}
      end)

      expect(ForgeMock, :list_queued_jobs, fn :client ->
        send(test, :scanned)

        {:ok, [%QueuedJob{id: "2", labels: ["self-hosted", "macos"], runner_scope: @scope}]}
      end)

      start(mode: :webhook, webhook: [])
      assert_receive {:pass, []}
      assert_receive :verified
      assert_receive :scanned
      assert_receive {:pass, ["self-hosted", "macos"]}
      assert [%{job_key: "github:2"}] = Jobs.list_pending()
    end

    test "a tick scans again once the scan interval has passed" do
      test = self()
      stub(AgentDispatchMock, :select_agent, fn _, _ -> {:error, :no_available_agent} end)

      expect(ForgeMock, :list_queued_jobs, 3, fn :client ->
        send(test, :scanned)
        {:ok, []}
      end)

      pid = start(mode: :webhook, scan_ms: 0)
      assert_receive :scanned
      send(pid, :tick)
      assert_receive :scanned
      send(pid, :tick)
      assert_receive :scanned
    end

    test "a tick before the scan interval doesn't scan" do
      test = self()
      stub(AgentDispatchMock, :select_agent, fn _, _ -> {:error, :no_available_agent} end)

      expect(ForgeMock, :list_queued_jobs, 1, fn :client ->
        send(test, :scanned)
        {:ok, []}
      end)

      pid = start(mode: :webhook, scan_ms: 60_000)
      assert_receive :scanned
      send(pid, :tick)
      :sys.get_state(pid)
      refute_received :scanned
    end

    test "a scan that crashes is logged and the manager carries on" do
      expect(ForgeMock, :list_queued_jobs, fn :client -> raise "boom" end)
      pid = start(mode: :webhook, scan_ms: 60_000)
      assert %{mode: :webhook, last_scan: at} = :sys.get_state(pid)
      assert is_integer(at)
      assert Process.alive?(pid)
    end

    test "fixed capacity never scans" do
      stub(AgentDispatchMock, :pool_definitions, fn -> [] end)
      pid = start(mode: :fixed_capacity, runner_scope: @scope, scan_ms: 0)
      send(pid, :tick)
      # no list_queued_jobs expectation: Mox fails the test if it's called
      assert %{last_scan: nil} = :sys.get_state(pid)
    end
  end
end
